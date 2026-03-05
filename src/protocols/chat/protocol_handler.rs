use super::message::ProtoMessage;
use super::utils::{read_message, write_message};
use crate::error::LibError;
use crate::protocols::chat::ChatConnectionHandle;
use anyhow::{Context, anyhow};
use bytes::BytesMut;
use iroh::{
    Endpoint, EndpointAddr, EndpointId,
    endpoint::{Connection, ConnectionError},
    protocol::{AcceptError, ProtocolHandler},
};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, instrument, warn};
use tracing::{error, info};
use uuid::Uuid;

use iroh::endpoint::VarInt;
pub const ALPN: &[u8] = b"/gossip/chat/0";
pub const OPEN_MESSAGE: &[u8] = b"OPEN_MESSAGE";
pub const CHANNEL_BUFFER: usize = 1024usize;
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 1024 * 1024;

#[derive(uniffi::Enum, Copy, Clone, Debug, PartialEq, Eq)]
pub enum ConnOrigin {
    Accept,
    Dial,
}

#[derive(Debug, Clone)]
pub struct ChatProtocol {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    accept_rx: async_channel::Receiver<Result<ChatConnectionHandle, anyhow::Error>>,
    request_tx: mpsc::Sender<ConnectionRequest>,
    actor_join_handle: Mutex<Option<JoinHandle<()>>>,
    cancel_token: CancellationToken,
}

#[derive(Debug)]
struct Actor {
    endpoint: Endpoint,
    accept_tx: async_channel::Sender<Result<ChatConnectionHandle, anyhow::Error>>,
    request_rx: mpsc::Receiver<ConnectionRequest>,
    cancel_token: CancellationToken,
}

#[derive(Debug)]
pub enum ConnectionRequest {
    Accept {
        conn: Connection,
    },
    Dial {
        endpoint_id: EndpointId,
        respond_to: oneshot::Sender<Result<ChatConnectionHandle, LibError>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionCloseReason {
    Normal,
    InvalidOpenMessage,
    StreamReadFailed,
    StreamWriteFailed,
}

#[derive(Debug, PartialEq, Eq)]
pub struct InvalidConnectionCloseReasonCode(u64);

impl Actor {
    #[instrument(skip_all)]
    async fn run(&mut self) -> anyhow::Result<()> {
        loop {
            tokio::select! {
                biased;
                _ = self.cancel_token.cancelled() => {
                    info!("ChatProtocolActor: Cancellation token received. Shutting down.");
                    return Ok(());
                }
               // msg = self.reconnect_rx.recv() => {
               //     let Some(msg) = msg else {
               //         return Err(anyhow!("ChatProtocolActor: Reconnection channel is closed. Shutting down."));
               //     };
               //     debug!("ChatProtocolActor: Received a reconnection message.");

               //     let result = self.establish_connection_with(msg.node_id).await.map(|_| ());

               //     if msg.respond_to.send(result).is_err() {
               //         warn!("ChatProtocolActor: Failed to return Chat reconnection result. Channel is closed.");
               //     }
               //}
                request_opt = self.request_rx.recv() => {
                    let Some(request) = request_opt else {
                        return Err(anyhow!("ChatProtocolActor: Connection request channel is closed. Shutting down."));
                    };

                    match request {
                        ConnectionRequest::Accept { conn } => {
                            debug!("ChatProtocolActor: Accepting...");
                            let result = self
                                .process_established_connection(conn, ConnOrigin::Accept)
                                .await;

                            if self.accept_tx.send(result).await.is_err() {
                                return Err(anyhow!("ChatProtocolActor: Failed to send Accepted connection. Channel is closed."));
                            }
                        }
                        ConnectionRequest::Dial {
                            endpoint_id,
                            respond_to,
                        } => {
                            debug!(?endpoint_id, "ChatProtocolActor: dialing...");
                            let result = self.dial_endpoint(endpoint_id).await;

                            if respond_to.send(result).is_err() {
                                error!(
                                    "ChatProtocolActor: Failed to return Dial result. Channel is closed."
                                );
                            }
                        }
                    }
                },
            }
        }
    }

    #[instrument(skip_all, fields(endpoint_id = %endpoint_id), err)]
    async fn dial_endpoint(
        &mut self,
        endpoint_id: EndpointId,
    ) -> Result<ChatConnectionHandle, LibError> {
        let node_addr = EndpointAddr::from(endpoint_id);

        match self.endpoint.connect(node_addr, ALPN).await {
            Ok(conn) => {
                debug!(
                    %endpoint_id,
                    "ChatProtocolActor: Established connection on Dial.",
                );

                self.process_established_connection(conn, ConnOrigin::Dial)
                    .await
                    .map_err(LibError::from)
            }
            Err(err) => Err(LibError::from(err)),
        }
    }

    #[instrument(skip_all, fields(remote_id = ?conn.remote_id(), conn_origin = ?conn_origin), err)]
    async fn process_established_connection(
        &mut self,
        conn: Connection,
        conn_origin: ConnOrigin,
    ) -> Result<ChatConnectionHandle, anyhow::Error> {
        let remote_node_id = conn.remote_id();
        let cancel_token = self.cancel_token.child_token();

        let (mut send, mut recv) = match conn_origin {
            ConnOrigin::Accept => {
                let (send, mut recv) = conn.accept_bi().await?;

                let mut buf = BytesMut::zeroed(OPEN_MESSAGE.len());
                if let Err(err) = recv.read_exact(&mut buf).await.context(
                    "Failed to read open message while ACCEPTING connection.",
                ) {
                    let (code, reason) =
                        ConnectionCloseReason::StreamReadFailed.to_code_reason();
                    conn.close(code, reason.as_bytes());
                    return Err(err);
                }

                if buf.ne(OPEN_MESSAGE) {
                    let (code, reason) =
                        ConnectionCloseReason::InvalidOpenMessage.to_code_reason();
                    conn.close(code, reason.as_bytes());
                    return Err(anyhow!(
                        "Received invalid OPEN_MESSAGE while ACCEPTING connection."
                    ));
                }
                (send, recv)
            }
            ConnOrigin::Dial => {
                let (mut send, recv) = conn.open_bi().await?;

                if let Err(err) = send
                    .write_all(OPEN_MESSAGE)
                    .await
                    .context("Failed to send OPEN_MESSAGE while DIAL other peer.")
                {
                    let (code, reason) =
                        ConnectionCloseReason::StreamWriteFailed.to_code_reason();
                    conn.close(code, reason.as_bytes());
                    return Err(err);
                }

                (send, recv)
            }
        };

        let (write_tx, mut write_rx) = mpsc::channel::<ProtoMessage>(CHANNEL_BUFFER);
        let (read_tx, read_rx) =
            async_channel::bounded::<ProtoMessage>(CHANNEL_BUFFER);

        let chat_conn_handle_id = Uuid::new_v4();
        let chat_conn_handle = ChatConnectionHandle {
            id: chat_conn_handle_id,
            origin: conn_origin,
            connection: conn.clone(),
            tx: write_tx,
            rx: read_rx,
            cancel_token: cancel_token.clone(),
        };

        let recv_join_handle: JoinHandle<anyhow::Result<()>> =
            tokio::task::spawn(async move {
                let mut recv_buf = BytesMut::new();
                loop {
                    let msg_opt = read_message(
                        &mut recv,
                        &mut recv_buf,
                        DEFAULT_MAX_MESSAGE_SIZE,
                    )
                    .await
                    .context("Failed to read message on iroh connection.")?;
                    let Some(msg) = msg_opt else {
                        break;
                    };

                    read_tx.send(msg).await?;
                }

                Ok(())
            });

        let send_join_handle = tokio::task::spawn(async move {
            let mut send_buf = BytesMut::new();
            while let Some(msg) = write_rx.recv().await {
                write_message(
                    &mut send,
                    &mut send_buf,
                    &msg,
                    DEFAULT_MAX_MESSAGE_SIZE,
                )
                .await?;
            }

            // notify the other node no more data will be sent
            send.finish()?;

            // wait for the other node to ack all the sent data
            let _ = send.stopped().await?;

            anyhow::Ok(())
        });

        tokio::task::spawn(async move {
            tokio::select! {
                _ = cancel_token.cancelled() => {
                    info!(
                        %remote_node_id,
                        "ChatConnection was cancelled and intentionally closed by local peer."
                    );
                }
                conn_err = conn.closed() => {
                    match conn_err {
                        ConnectionError::LocallyClosed => {
                            info!(
                                %remote_node_id,
                                "ChatConnection was intentionally closed by local peer."
                            );
                        },
                        ConnectionError::ApplicationClosed(close) => {
                            match ConnectionCloseReason::try_from(close.error_code) {
                                Ok(reason) => {
                                    let (code, reason_str) = reason.to_code_reason();
                                    warn!(
                                        %remote_node_id, %code, %reason_str,
                                        "Connection is closed by remote peer."
                                    );
                                },
                                Err(err) => {
                                    error!(
                                        %remote_node_id, err = %err.0,
                                        "Chat connection was closed with unsupported reason."
                                    );
                               }
                            }
                        },
                        error => {
                            error!(
                                %remote_node_id, %error,
                                "Chat connection was closed unexpectedly."
                            );
                        },
                    }
                }
                recv_res = recv_join_handle => {
                    match recv_res {
                        Ok(_) => info!(%remote_node_id, "Chat connection incoming stream is gracefully closed."),
                        Err(err) => error!(%remote_node_id, %err, "Chat connection incoming stream is closed with error."),
                    }
                }
                send_res = send_join_handle => {
                    match send_res {
                        Ok(_) => info!(%remote_node_id, "Chat connection outgoing stream is gracefully closed."),
                        Err(err) => error!(%remote_node_id, %err, "Chat connection outgoing stream is closed with error."),
                    }
                }
            }

            let (code, reason) = ConnectionCloseReason::Normal.to_code_reason();
            conn.close(code, reason.as_bytes());
            info!(
                %remote_node_id, %code, %reason,
                "Chat connection is closed.",
            );
        });

        Ok(chat_conn_handle)
    }
}

impl ProtocolHandler for ChatProtocol {
    #[instrument(skip_all, fields(alpn = %ALPN.escape_ascii()))]
    async fn accept(
        &self,
        conn: iroh::endpoint::Connection,
    ) -> Result<(), AcceptError> {
        // Otherwise, accept or reject connection.
        // It could be efficiently solved by EndpointHooks added in iroh 0.96.0.

        match self
            .inner
            .request_tx
            .send(ConnectionRequest::Accept { conn })
            .await
            .context("Connection channel is closed.")
        {
            Ok(()) => Ok(()),
            Err(err) => {
                error!(%err, "Failed to process ACCEPTED incoming chat connection.");
                Err(AcceptError::Connection {
                    source: ConnectionError::LocallyClosed,
                    meta: n0_error::meta(),
                })
            }
        }
    }

    #[instrument(skip_all)]
    async fn shutdown(&self) -> () {
        let inner = self.inner.clone();

        info!("ChatProtocol: Initiating shutdown.");
        inner.cancel_token.cancel();

        let mut guard = inner.actor_join_handle.lock().await;

        if let Some(handle) = guard.take() {
            if let Err(join_err) = handle.await {
                error!(error = %join_err, "ChatProtocolActor task panicked!");
            } else {
                info!("ChatProtocolActor task finished.");
            }
        } else {
            warn!("Shutdown called more than once.");
        }
    }
}

impl ChatProtocol {
    #[instrument(skip_all)]
    pub fn new(endpoint: Endpoint) -> Self {
        let cancel_token = CancellationToken::new();
        let (accept_tx, accept_rx) = async_channel::bounded(CHANNEL_BUFFER);
        let (request_tx, request_rx) = mpsc::channel(CHANNEL_BUFFER);

        let mut actor = Actor {
            endpoint,
            accept_tx,
            request_rx,
            cancel_token: cancel_token.clone(),
        };

        let acton_join_handle = tokio::task::spawn(async move {
            if let Err(err) = actor.run().await {
                error!(%err, "ChatProtocolActor exited with an error");
            }
        });

        ChatProtocol {
            inner: Arc::new(Inner {
                accept_rx,
                request_tx,
                actor_join_handle: Mutex::new(Some(acton_join_handle)),
                cancel_token,
            }),
        }
    }

    #[instrument(skip_all, err)]
    pub async fn accept_conn(&self) -> Result<ChatConnectionHandle, LibError> {
        tokio::select! {
            _ = self.inner.cancel_token.cancelled() => {
                Err(LibError::from(anyhow::anyhow!("Chat protocol accept connnection request was cancelled.")))
            }
            conn_result = self.inner.accept_rx.recv() => {
                conn_result
                    .context("Failed to receive chat connection. Channel is closed.")?
                    .map_err(LibError::from)
            }
        }
    }

    pub fn sender(&self) -> mpsc::Sender<ConnectionRequest> {
        self.inner.request_tx.to_owned()
    }
}

impl ConnectionCloseReason {
    pub fn to_code_reason(&self) -> (VarInt, &'static str) {
        match self {
            ConnectionCloseReason::Normal => (VarInt::from_u32(0), "Normal closure"),
            ConnectionCloseReason::InvalidOpenMessage => (
                VarInt::from_u32(1001),
                "Invalid or unexpected open message received",
            ),
            ConnectionCloseReason::StreamReadFailed => {
                (VarInt::from_u32(1002), "Stream read operation failed")
            }
            ConnectionCloseReason::StreamWriteFailed => {
                (VarInt::from_u32(1003), "Stream write operation failed")
            }
        }
    }
}

impl TryFrom<VarInt> for ConnectionCloseReason {
    type Error = InvalidConnectionCloseReasonCode;

    fn try_from(value: VarInt) -> Result<Self, Self::Error> {
        match value.into_inner() {
            0 => Ok(ConnectionCloseReason::Normal),
            1001 => Ok(ConnectionCloseReason::InvalidOpenMessage),
            1002 => Ok(ConnectionCloseReason::StreamReadFailed),
            1003 => Ok(ConnectionCloseReason::StreamWriteFailed),
            other => {
                warn!(
                    %other,
                    "Received unsupported connection close reason code.",
                );
                Err(InvalidConnectionCloseReasonCode(other))
            }
        }
    }
}
