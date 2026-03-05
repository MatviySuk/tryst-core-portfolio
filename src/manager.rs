//! Chat session coordination and CRDT-based message synchronization.
//!
//! This module defines the `ChatManager` and its underlying `Actor`, responsible
//! for managing a single P2P conversation. Each `ChatManager` operates as a 
//! dedicated actor, ensuring that:
//!
//! - **CRDT State Consistency:** All incoming and outgoing messages are reconciled
//!   using the `automerge` document.
//! - **P2P Transport Stability:** The actor manages the underlying `iroh` QUIC
//!   connection, handling automatic reconnection and glare resolution.
//! - **Persistence Lifecycle:** The actor coordinates periodic snapshotting of the 
//!   CRDT state to the `SQLite` storage layer.
//! - **UI Reactivity:** All chat state updates (new messages, connection changes, 
//!   sync progress) are pushed to the UI via the `ChatManagerReceiver` trait.

use crate::error::{GossipError, LibError};
use crate::persistence::{ChatId, ChatMessage, ChatMessageContent, ChatRepository};
use crate::protocols::chat::{
    ChatConnection, ChatConnectionHandle, ChatConnectionState,
    UniffiChatConnectionState, message::ProtoMessage,
};
use anyhow::anyhow;
use automerge::sync;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, trace, warn};

pub const CHANNEL_BUFFER: usize = 1024usize;
const SYNC_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

#[uniffi::export(with_foreign)]
pub trait ChatManagerReceiver: Send + Sync + std::fmt::Debug {
    fn process_messages(&self, msgs: Vec<ChatMessage>);
    fn process_connection_state(&self, state: UniffiChatConnectionState);
    fn process_sync_status(&self, is_synced: bool);
}

pub type ChatManagerMessageReceiverObject = Arc<dyn ChatManagerReceiver + 'static>;

#[derive(uniffi::Object, Clone, Debug)]
pub struct ChatManager {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    chat_id: ChatId,
    in_tx: mpsc::Sender<InMessage>,
    subscribe_tx: mpsc::Sender<SubscribeMessage>,
}

struct Actor {
    doc: ChatRepository,
    conn: ChatConnection,
    in_rx: mpsc::Receiver<InMessage>,
    subscribe_rx: mpsc::Receiver<SubscribeMessage>,
    msg_receiver: Option<ChatManagerMessageReceiverObject>,
    sync_status_rx: watch::Receiver<bool>,
    conn_state_rx: watch::Receiver<ChatConnectionState>,
    reset_doc_state: bool,
    cancel_token: CancellationToken,
}

#[derive(Debug)]
enum InMessage {
    Single {
        msg: ChatMessageContent,
        tx: oneshot::Sender<Result<ChatMessage, LibError>>,
    },
    All {
        tx: oneshot::Sender<Result<Vec<ChatMessage>, LibError>>,
    },
    PassAcceptedConnHandle(ChatConnectionHandle),
}

#[derive(Debug)]
struct SubscribeMessage {
    msg_receiver: ChatManagerMessageReceiverObject,
    tx: oneshot::Sender<Result<(), LibError>>,
}

impl ChatManager {
    #[instrument(skip(doc, conn, cancel_token))]
    pub fn new(
        doc: ChatRepository,
        conn: ChatConnection,
        cancel_token: CancellationToken,
    ) -> Self {
        let chat_id = doc.chat_id();
        let (msg_receiver_tx, msg_receiver_rx) = mpsc::channel(CHANNEL_BUFFER);
        let (in_tx, in_rx) = mpsc::channel(CHANNEL_BUFFER);

        let sync_status_rx = doc.subscribe_to_sync_status();
        let conn_state_rx = conn.subscribe_to_state();

        trace!(chat_id = %chat_id, "Subscribed ChatConnection to state changes.");

        let mut actor = Actor {
            doc,
            conn,
            in_rx,
            subscribe_rx: msg_receiver_rx,
            msg_receiver: None,
            sync_status_rx,
            conn_state_rx,
            reset_doc_state: false,
            cancel_token: cancel_token.clone(),
        };
        trace!(chat_id = %chat_id, "ChatManager Actor initialized.");

        tokio::task::spawn(async move {
            trace!(
                chat_id = %chat_id,
                "ChatManager Actor task spawned."
            );
            if let Err(err) = actor.run().await {
                error!(
                    chat_id = %chat_id,
                    "ChatManager Actor finished with error: {err}"
                );
            } else {
                debug!(chat_id = %chat_id, "ChatManager Actor finished.");
            }

            actor.conn.close().await;
        });

        debug!(chat_id = %chat_id, "ChatManager created.");
        ChatManager {
            inner: Arc::new(Inner {
                chat_id,
                in_tx,
                subscribe_tx: msg_receiver_tx,
            }),
        }
    }

    pub async fn pass_accepted_conn_handle(
        &self,
        conn_handle: ChatConnectionHandle,
    ) {
        if self
            .inner
            .in_tx
            .send(InMessage::PassAcceptedConnHandle(conn_handle))
            .await
            .is_err()
        {
            warn!(chat_id = ?self.inner.chat_id, "Failed to pass accepted connection in chat manager.");
        }
    }
}

impl Actor {
    #[instrument(skip(self), fields(chat_id = %self.doc.chat_id()))]
    async fn run(&mut self) -> anyhow::Result<()> {
        let initial_conn_state = self.conn_state_rx.borrow_and_update().to_owned();
        self.handle_connection_state_change(initial_conn_state)
            .await;

        let mut sync_interval = tokio::time::interval(SYNC_INTERVAL);
        sync_interval
            .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;
                _ = self.cancel_token.cancelled() => {
                    debug!("ChatManagerActor: Cancellation received. Closing...");
                    self.conn.close().await;
                    return Ok(());
                }
                _ = sync_interval.tick() => {
                    let conn_state = self.conn_state_rx.borrow().to_owned();
                    match conn_state {
                        ChatConnectionState::Connected { ..} => {
                            debug!("ChatManagerActor: Triggering periodic sync.");
                            let sync_msg = self.doc.generate_sync_message();
                            self.handle_outgoing_sync_message(sync_msg).await;
                        },
                        ChatConnectionState::Disconnected => {
                            self.conn.reconnect();
                        },
                        _ => {},
                    };
                },
                res = self.sync_status_rx.changed() => {
                    if res.is_err() {
                       warn!("Sync status channel is closed and current values is seen.");
                    }

                    let is_synced = self.sync_status_rx.borrow_and_update().to_owned();

                    if let Some(msg_receiver) = &self.msg_receiver {
                        msg_receiver.process_sync_status(is_synced);
                    }
                },
                res = self.conn_state_rx.changed() => {
                    if res.is_err() {
                       warn!("Connection state channel is closed and current values is seen.");
                    }

                    let new_conn_state = self.conn_state_rx.borrow_and_update().to_owned();

                    self.handle_connection_state_change(new_conn_state).await;
                },
                subscribe_msg = self.subscribe_rx.recv() => {
                    self.handle_subscription(subscribe_msg).await?;
                }
                in_msg = self.in_rx.recv() => {
                    self.handle_outgoing_message(in_msg).await?;
                }
                msg_opt = self.conn.receive(), if matches!(*self.conn_state_rx.borrow(), ChatConnectionState::Connected { .. })  => {
                    let Some(msg) = msg_opt else {
                        warn!("Failed to receive msg from remote endpoint. Channel is closed.");
                        continue;
                    };

                    self.handle_incomming_message(msg).await;
                }
            }
        }
    }

    async fn handle_connection_state_change(
        &mut self,
        new_state: ChatConnectionState,
    ) {
        if let Some(msg_receiver) = &self.msg_receiver {
            msg_receiver.process_connection_state(new_state.into());
        }

        if let ChatConnectionState::Connected { .. } = new_state {
            self.doc.reset_sync_state();
            let sync_msg = self.doc.generate_sync_message();
            self.handle_outgoing_sync_message(sync_msg).await;
        }
    }

    async fn handle_subscription(
        &mut self,
        subscribe_msg: Option<SubscribeMessage>,
    ) -> anyhow::Result<()> {
        debug!("ChatManagerActor: Received a message on subscribe_rx.");
        let Some(subscribe_msg) = subscribe_msg else {
            return Err(anyhow!(
                "ChatManagerActor: Subscribe message channel is closed. Closing manager..."
            ));
        };

        let is_synced = self.sync_status_rx.borrow().to_owned();
        let new_conn_state = self.conn_state_rx.borrow().to_owned();

        subscribe_msg.msg_receiver.process_sync_status(is_synced);
        subscribe_msg
            .msg_receiver
            .process_connection_state(new_conn_state.into());

        self.msg_receiver = Some(subscribe_msg.msg_receiver);

        if subscribe_msg.tx.send(Ok(())).is_err() {
            warn!(
                "ChatManagerActor: Failed to return subscribe message result. Channel closed."
            );
        }

        Ok(())
    }

    async fn handle_incomming_message(&mut self, msg: ProtoMessage) {
        match msg {
            ProtoMessage::Sync(proto_sync_msg) => {
                debug!(
                    "ChatManagerActor: Received incoming sync message from peer. Is some: {}",
                    proto_sync_msg.is_some()
                );
                let in_sync_msg: Option<sync::Message> = match proto_sync_msg {
                    Some(bytes) => match bytes.try_into() {
                        Ok(msg) => Some(msg),
                        Err(err) => {
                            warn!(
                                ?err,
                                "ChatManagerActor: Failed to decode sync message."
                            );
                            return;
                        }
                    },
                    None => None,
                };

                match self.doc.receive_sync_message(in_sync_msg).await {
                    Ok((chunks, reply_action)) => {
                        self.reset_doc_state = false;

                        if !chunks.is_empty() {
                            if let Some(msg_receiver) = self.msg_receiver.as_ref() {
                                msg_receiver.process_messages(chunks);
                            } else {
                                warn!(
                                    "Failed to pass updated message chunks on sync. Message Receiver is not setup."
                                );
                            }
                        }

                        if let Some(out_sync_msg) = reply_action {
                            self.handle_outgoing_sync_message(out_sync_msg).await;
                        }
                    }
                    Err(rx_err) => {
                        if self.reset_doc_state {
                            error!(
                                ?rx_err,
                                "Failed to process received sync message multiple times."
                            );
                        } else {
                            warn!("Sync error. Attempting to reset state from DB.");
                            match self.doc.reset_doc_state_to_persisted().await {
                                Ok(()) => {
                                    self.reset_doc_state = true;
                                    let sync_message =
                                        self.doc.generate_sync_message();

                                    self.handle_outgoing_sync_message(sync_message)
                                        .await;
                                }
                                Err(reset_err) => {
                                    error!(
                                        ?rx_err,
                                        ?reset_err,
                                        "Failed to both persist and then reset chat state."
                                    );
                                }
                            }
                        }
                    }
                }
            }
        };
    }

    async fn handle_outgoing_message(
        &mut self,
        in_msg: Option<InMessage>,
    ) -> anyhow::Result<()> {
        debug!("ChatManagerActor: Received a message on in_rx (outgoing message).");
        let Some(in_msg) = in_msg else {
            return Err(anyhow!(
                "ChatManagerActor: Outgoing message channel is closed. Closing manager..."
            ));
        };

        match in_msg {
            InMessage::Single { msg, tx } => {
                debug!(
                    "ChatManagerActor: Handling InMessage::Single to add a new message."
                );
                let result = self.doc.add_message(msg.clone()).await;
                debug!(
                    "ChatManagerActor: Message {:?} added to document with result: {:?}",
                    msg,
                    result.is_ok()
                );
                let result = match result {
                    Ok((msgs, sync_msg)) => {
                        self.handle_outgoing_sync_message(sync_msg).await;

                        Ok(msgs)
                    }
                    Err(err) => Err(err),
                };

                if tx.send(result).is_err() {
                    return Err(anyhow!(
                        "ChatManagerActor: Failed to return add message result. Channel is closed."
                    ));
                }
            }
            InMessage::All { tx } => {
                debug!(
                    "ChatManagerActor: Handling InMessage::All to get all messages."
                );
                let result = self.doc.get_all_messages();
                debug!(
                    "ChatManagerActor: Retrieved all messages with result: {:?}",
                    result.is_ok()
                );

                if tx.send(result).is_err() {
                    return Err(anyhow!(
                        "ChatManagerActor: Failed to return all messages. Channel closed."
                    ));
                }
            }
            InMessage::PassAcceptedConnHandle(handle) => {
                debug!(chat_id = ?self.doc.chat_id(), "Received new Accepted conn handle.");
                self.conn.update_handle(handle).await;
            }
        }

        Ok(())
    }

    async fn handle_outgoing_sync_message(
        &mut self,
        sync_msg: Option<sync::Message>,
    ) {
        debug!(
            "ChatManagerActor: Sending sync message to connection: {:?}",
            sync_msg
        );

        self.conn
            .send_message(ProtoMessage::Sync(sync_msg.map(|m| m.into())))
            .await;
    }
}

#[uniffi::export]
impl ChatManager {
    #[uniffi::method(async_runtime = "tokio")]
    #[instrument(skip(self, msg_receiver), err)]
    pub async fn subscribe(
        &self,
        msg_receiver: Arc<dyn ChatManagerReceiver + 'static>,
    ) -> Result<(), GossipError> {
        let (tx, rx) = oneshot::channel();
        let msg = SubscribeMessage { msg_receiver, tx };

        self.inner.subscribe_tx.send(msg).await.map_err(|e| {
            let err = anyhow!(
                "ChatManager: Failed to send subscribe message. Channel closed. Err: {}",
                e
            );
            error!("{err}");
            GossipError::from(err)
        })?;

        rx.await
            .map_err(|e| {
                let err = anyhow!(
                    "ChatManager: Failed to receive subscribe result. Channel closed. Err: {}",
                    e
                );
                error!("{err}");
                GossipError::from(err)
            })?
            .map_err(|e| {
                error!(
                    "ChatManager: Subscribe operation failed with LibError: {:?}",
                    e
                );
                GossipError::from(e)
            })
    }

    #[uniffi::method(async_runtime = "tokio")]
    #[instrument(skip(self, msg), err)]
    pub async fn send_message(
        &self,
        msg: ChatMessageContent,
    ) -> Result<ChatMessage, GossipError> {
        info!(
            "ChatManager: Send message method called with content: {:?}",
            msg
        );
        let (tx, rx) = oneshot::channel();

        self.inner
            .in_tx
            .send(InMessage::Single { msg, tx })
            .await
            .map_err(|e| {
                let err = anyhow!(
                    "ChatManager: Failed to send InMessage::Single. Channel closed. Err: {}",
                    e
                );
                error!("{err}");
                GossipError::from(err)
            })?;

        rx.await
            .map_err(|e| {
                let err = anyhow!(
                    "ChatManager: Failed to receive send_message result. Channel closed. Err: {}",
                    e
                );
                error!("{err}");
                GossipError::from(err)
            })?
            .map_err(|e| {
                error!(
                    "ChatManager: Send message operation failed with LibError: {:?}",
                    e
                );
                GossipError::from(e)
            })
    }

    #[uniffi::method(async_runtime = "tokio")]
    #[instrument(skip(self), err)]
    pub async fn get_all_messages(&self) -> Result<Vec<ChatMessage>, GossipError> {
        info!("ChatManager: Get all messages method called.");
        let (tx, rx) = oneshot::channel();
        trace!("Oneshot channel created for get_all_messages response.");

        self.inner
            .in_tx
            .send(InMessage::All { tx })
            .await
            .map_err(|e| {
                let err = anyhow!(
                    "ChatManager: Failed to send InMessage::All. Channel closed. Err: {}",
                    e
                );
                error!("{err}");
                GossipError::from(err)
            })?;

        rx.await
            .map_err(|e| {
                let err = anyhow!(
                    "ChatManager: Failed to receive all messages result. Channel closed. Err: {}",
                    e
                );
                error!("{err}");
                GossipError::from(err)
            })?
            .map_err(|e| {
                error!(
                    "ChatManager: Get all messages operation failed with LibError: {:?}",
                    e
                );
                GossipError::from(e)
            })
    }
}

pub(crate) mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::sync::watch;

    #[derive(Debug, Clone)]
    #[allow(dead_code)]
    pub struct TestChatManagerStateReceiver {
        pub all_msgs: Arc<Mutex<Vec<ChatMessage>>>,
        pub _sync_status_watcher: Arc<tokio::sync::Mutex<watch::Receiver<bool>>>,
        sync_status_watcher_tx: watch::Sender<bool>,
        pub conn_state_watcher:
            Arc<tokio::sync::Mutex<watch::Receiver<UniffiChatConnectionState>>>,
        conn_state_watcher_tx: watch::Sender<UniffiChatConnectionState>,
        pub msg_update_rx: Arc<tokio::sync::Mutex<watch::Receiver<()>>>,
        msg_update_tx: watch::Sender<()>,
    }

    #[allow(dead_code)]
    impl TestChatManagerStateReceiver {
        pub fn new(all_messages: Vec<ChatMessage>) -> Self {
            let (tx_state, rx_state) = watch::channel(false);
            let (tx, rx) = watch::channel(UniffiChatConnectionState::Disconnected);
            let (tx_msg, rx_msg) = watch::channel(());

            Self {
                all_msgs: Arc::new(Mutex::new(all_messages)),
                _sync_status_watcher: Arc::new(tokio::sync::Mutex::new(rx_state)),
                sync_status_watcher_tx: tx_state,
                conn_state_watcher: Arc::new(tokio::sync::Mutex::new(rx)),
                conn_state_watcher_tx: tx,
                msg_update_rx: Arc::new(tokio::sync::Mutex::new(rx_msg)),
                msg_update_tx: tx_msg,
            }
        }
    }

    impl ChatManagerReceiver for TestChatManagerStateReceiver {
        fn process_messages(&self, msgs: Vec<ChatMessage>) {
            let mut current_msgs = self
                .all_msgs
                .lock()
                .expect("Expected to get chat history lock for tests.");

            for msg in msgs {
                if !current_msgs.iter().any(|m| m.id == msg.id) {
                    current_msgs.push(msg);
                }
            }

            current_msgs.sort_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.id.cmp(&b.id))
            });

            let _ = self.msg_update_tx.send(());
        }

        fn process_connection_state(&self, new_state: UniffiChatConnectionState) {
            self.conn_state_watcher_tx.send_modify(|state| {
                if *state != new_state {
                    *state = new_state;
                }
            });
        }

        fn process_sync_status(&self, is_synced: bool) {
            self.sync_status_watcher_tx.send_modify(|status| {
                if *status != is_synced {
                    *status = is_synced;
                }
            });
        }
    }
}
