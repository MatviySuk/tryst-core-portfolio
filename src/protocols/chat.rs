//! Chat connection state machine and glare resolution.
//!
//! In pure P2P networks, "glare" occurs when two nodes attempt to dial each other
//! simultaneously. Without intervention, this results in duplicate connections or
//! dropped streams.
//!
//! This module resolves glare by implementing a deterministic tie-breaker based on
//! lexicographical comparison of the nodes' `EndpointId`s. The node with the "higher"
//! ID always assumes the `Dial` role, while the other assumes the `Accept` role,
//! ensuring only a single, stable transport is maintained per peer pair.
//!
//! Connection lifecycle and multiplexing are managed by the `ChatConnection` state machine.

pub mod message;
pub mod protocol_handler;
mod utils;

use crate::protocols::chat::protocol_handler::ConnOrigin;

use iroh::EndpointId;
use message::ProtoMessage;
use protocol_handler::ConnectionRequest;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, warn};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ChatConnection {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    pub local_id: EndpointId,
    pub remote_id: EndpointId,
    protocol_tx: mpsc::Sender<ConnectionRequest>,
    state_notifier: watch::Sender<ChatConnectionState>,
    state_subscriber: watch::Receiver<ChatConnectionState>,
    state: RwLock<InternalConnState>,
}

#[derive(uniffi::Enum, Clone, Debug, PartialEq, Eq)]
pub enum UniffiChatConnectionState {
    Disconnected,
    Connected { id: String },
    Reconnecting,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChatConnectionState {
    Disconnected,
    Connected { id: Uuid },
    Reconnecting,
}

#[derive(Clone, Debug)]
pub enum InternalConnState {
    Disconnected,
    Connected(ChatConnectionHandle),
    Reconnecting,
}

#[derive(Clone, Debug)]
pub struct ChatConnectionHandle {
    pub id: Uuid,
    pub origin: ConnOrigin,
    pub connection: iroh::endpoint::Connection,
    pub tx: mpsc::Sender<ProtoMessage>,
    pub rx: async_channel::Receiver<ProtoMessage>,
    pub cancel_token: CancellationToken,
}

impl ChatConnection {
    #[instrument(skip_all, fields(remote_id = %remote_id))]
    pub fn new(
        local_id: EndpointId,
        remote_id: EndpointId,
        protocol_tx: mpsc::Sender<ConnectionRequest>,
    ) -> Self {
        let state = InternalConnState::Disconnected;
        let (tx, rx) = watch::channel(ChatConnectionState::from(&state));

        let chat_conn = Self {
            inner: Arc::new(Inner {
                local_id,
                remote_id,
                protocol_tx,
                state_notifier: tx,
                state_subscriber: rx,
                state: RwLock::new(state),
            }),
        };

        chat_conn.reconnect();

        chat_conn
    }

    #[instrument(skip_all, fields(remote_id = %handle.connection.remote_id()))]
    pub fn new_connected(
        local_id: EndpointId,
        handle: ChatConnectionHandle,
        protocol_tx: mpsc::Sender<ConnectionRequest>,
    ) -> Self {
        let remote_id = handle.connection.remote_id();
        let (tx, rx) =
            watch::channel(ChatConnectionState::Connected { id: handle.id });

        Self {
            inner: Arc::new(Inner {
                local_id,
                remote_id,
                protocol_tx,
                state: RwLock::new(InternalConnState::Connected(handle)),
                state_notifier: tx,
                state_subscriber: rx,
            }),
        }
    }

    pub fn remote_id(&self) -> EndpointId {
        self.inner.remote_id
    }

    #[instrument(skip_all)]
    pub fn subscribe_to_state(&self) -> watch::Receiver<ChatConnectionState> {
        self.inner.state_subscriber.clone()
    }

    #[instrument(skip_all)]
    pub async fn send_message(&self, msg: ProtoMessage) {
        let Some((tx, handle_id)) = ({
            let guard = self.inner.state.read().await;

            if let InternalConnState::Connected(handle) = &*guard {
                Some((handle.tx.clone(), handle.id))
            } else {
                None
            }
        }) else {
            warn!("ChatConnection: Send message called but not connected.");
            return;
        };

        if tx.send(msg).await.is_err() {
            error!(
                "ChatConnection: Unable to send message because channel is closed. Connection likely dropped."
            );
            let myself = self.clone();
            tokio::spawn(async move {
                myself.set_state_disconnected_if_id_matches(handle_id).await;
            });
        } else {
            debug!("ChatConnection: Message sent successfully.");
        }
    }

    #[instrument(skip_all)]
    pub async fn receive(&self) -> Option<ProtoMessage> {
        let Some((rx, handle_id)) = ({
            let guard = self.inner.state.read().await;

            if let InternalConnState::Connected(handle) = &*guard {
                Some((handle.rx.clone(), handle.id))
            } else {
                None
            }
        }) else {
            warn!("ChatConnection: Receive called but not connected.");
            return None;
        };

        match rx.recv().await {
            Ok(msg) => return Some(msg),
            Err(_) => {
                error!("ChatConnection: Channel closed. Connection likely dropped.");
                let myself = self.clone();

                tokio::spawn(async move {
                    myself.set_state_disconnected_if_id_matches(handle_id).await;
                });

                return None;
            }
        }
    }

    #[instrument(skip_all)]
    pub fn reconnect(&self) {
        let remote_id = self.inner.remote_id;
        let proto_tx = self.inner.protocol_tx.clone();
        let myself = self.clone();

        tokio::spawn(async move {
            if !myself
                .set_state_if_disconnected(InternalConnState::Reconnecting)
                .await
            {
                return;
            }

            let (tx, rx) = oneshot::channel();
            let req = ConnectionRequest::Dial {
                endpoint_id: remote_id,
                respond_to: tx,
            };

            if proto_tx.send(req).await.is_err() {
                myself
                    .set_state_if_reconnecting(InternalConnState::Disconnected)
                    .await;

                return;
            }

            match rx.await {
                Ok(Ok(handle)) => {
                    info!("ChatConnection: Background dial success.");

                    myself.update_handle(handle).await;
                }
                Ok(Err(e)) => {
                    warn!("ChatConnection: Dial failed: {}", e);

                    myself
                        .set_state_if_reconnecting(InternalConnState::Disconnected)
                        .await;
                }
                Err(_) => {
                    myself
                        .set_state_if_reconnecting(InternalConnState::Disconnected)
                        .await;
                }
            }
        });
    }

    #[instrument(skip(self, new_handle), fields(remote_id = %self.inner.remote_id, origin = ?new_handle.origin))]
    pub async fn update_handle(&self, new_handle: ChatConnectionHandle) {
        let local_id = self.inner.local_id;
        let remote_id = self.inner.remote_id;

        let mut guard = self.inner.state.write().await;

        let should_switch = if let InternalConnState::Connected(current_handle) =
            &*guard
        {
            // If origins are the same (Dial vs Dial, or Accept vs Accept),
            // we always "Refresh" (take the new one). This handles corruption recovery.
            if current_handle.origin == new_handle.origin {
                info!(
                    "ChatConnection: Refreshing connection ({:?} -> {:?}).",
                    current_handle.origin, new_handle.origin
                );
                true
            }
            // If origins differ, we have a GLARE (Simultaneous Open).
            // Use Node ID Tie-Breaker.
            else {
                match (local_id < remote_id, new_handle.origin) {
                    // Case 1: I am Master (Local < Remote). I prefer DIAL.
                    // If new is Dial, I take it. If new is Accept, I reject it.
                    (true, ConnOrigin::Dial) => {
                        info!(
                            "ChatConnection: Glare Resolution. I am Master. Preferring my new Dial."
                        );
                        true
                    }
                    (true, ConnOrigin::Accept) => {
                        info!(
                            "ChatConnection: Glare Resolution. I am Master. Rejecting Incoming Accept in favor of active Dial."
                        );
                        false
                    }

                    // Case 2: I am Slave (Local > Remote). I prefer ACCEPT.
                    // If new is Accept, I take it. If new is Dial, I reject it.
                    (false, ConnOrigin::Accept) => {
                        info!(
                            "ChatConnection: Glare Resolution. I am Slave. Preferring new Incoming Accept."
                        );
                        true
                    }
                    (false, ConnOrigin::Dial) => {
                        info!(
                            "ChatConnection: Glare Resolution. I am Slave. Rejecting my new Dial in favor of active Accept."
                        );
                        false
                    }
                }
            }
        } else {
            true
        };

        if should_switch {
            if let InternalConnState::Connected(old_handle) = &*guard {
                old_handle.shutdown();
            }

            let internal_state = InternalConnState::Connected(new_handle.to_owned());
            let public_state = ChatConnectionState::from(&internal_state);

            *guard = internal_state;

            self.inner.state_notifier.send_modify(|current| {
                *current = public_state;
            });
        } else {
            // We decided to keep the OLD connection.
            // However, we need to rotate handle ID in order to reset state
            // and resend everything that was lost by remote peer.
            new_handle.shutdown();

            if let InternalConnState::Connected(current_handle) = &mut *guard {
                let old_id = current_handle.id;
                let new_id = Uuid::new_v4();

                debug!(%old_id, %new_id, "ChatConnection: Glare resolution - Rotating UUID.");
                current_handle.id = new_id;

                let public_state = ChatConnectionState::from(
                    &InternalConnState::Connected(current_handle.to_owned()),
                );

                self.inner.state_notifier.send_modify(|current| {
                    *current = public_state;
                });
            } else {
                warn!(
                    "ChatConnection: State changed concurrently during rotation. Ignoring."
                );
            }
        }
    }

    #[instrument(skip_all)]
    pub async fn close(&self) {
        self.set_state(InternalConnState::Disconnected).await
    }
}

impl ChatConnection {
    async fn set_state(&self, new_state: InternalConnState) {
        let mut guard = self.inner.state.write().await;

        if let InternalConnState::Connected(old) = &*guard {
            old.shutdown();
        }

        let public_state = ChatConnectionState::from(&new_state);

        *guard = new_state;

        self.inner.state_notifier.send_if_modified(|current| {
            if *current != public_state {
                *current = public_state;
                true
            } else {
                false
            }
        });
    }

    async fn set_state_disconnected_if_id_matches(&self, expected_id: Uuid) {
        let mut guard = self.inner.state.write().await;

        if let InternalConnState::Connected(handle) = &*guard {
            if handle.id == expected_id {
                handle.shutdown();

                let new_state = InternalConnState::Disconnected;
                let public_state = ChatConnectionState::from(&new_state);

                *guard = new_state;

                self.inner.state_notifier.send_if_modified(|current| {
                    if *current != public_state {
                        *current = public_state;
                        true
                    } else {
                        false
                    }
                });
            } else {
                info!(
                    "ChatConnection: Ignoring disconnect from old handle {:?}, current is {:?}",
                    expected_id, handle.id
                );
            }
        }
    }

    async fn set_state_if_disconnected(&self, new_state: InternalConnState) -> bool {
        let mut guard = self.inner.state.write().await;

        if let InternalConnState::Disconnected = *guard {
            let public_state = ChatConnectionState::from(&new_state);
            *guard = new_state;

            self.inner.state_notifier.send_if_modified(|current| {
                if *current != public_state {
                    *current = public_state;
                    true
                } else {
                    false
                }
            });

            true
        } else {
            false
        }
    }

    async fn set_state_if_reconnecting(&self, new_state: InternalConnState) -> bool {
        let mut guard = self.inner.state.write().await;

        if let InternalConnState::Reconnecting = *guard {
            let public_state = ChatConnectionState::from(&new_state);
            *guard = new_state;

            self.inner.state_notifier.send_if_modified(|current| {
                if *current != public_state {
                    *current = public_state;
                    true
                } else {
                    false
                }
            });
            true
        } else {
            false
        }
    }
}

impl ChatConnectionHandle {
    pub fn remote_id(&self) -> EndpointId {
        self.connection.remote_id()
    }

    #[instrument(skip_all, fields(handle_id = %self.id))]
    fn shutdown(&self) {
        self.cancel_token.cancel();
    }
}

impl From<&InternalConnState> for ChatConnectionState {
    fn from(value: &InternalConnState) -> Self {
        match value {
            InternalConnState::Disconnected => ChatConnectionState::Disconnected,
            InternalConnState::Connected(handle) => {
                ChatConnectionState::Connected { id: handle.id }
            }
            InternalConnState::Reconnecting => ChatConnectionState::Reconnecting,
        }
    }
}

impl From<ChatConnectionState> for UniffiChatConnectionState {
    fn from(value: ChatConnectionState) -> Self {
        match value {
            ChatConnectionState::Disconnected => {
                UniffiChatConnectionState::Disconnected
            }
            ChatConnectionState::Connected { id } => {
                UniffiChatConnectionState::Connected { id: id.to_string() }
            }
            ChatConnectionState::Reconnecting => {
                UniffiChatConnectionState::Reconnecting
            }
        }
    }
}
