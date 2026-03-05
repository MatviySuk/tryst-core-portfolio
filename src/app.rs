//! Main application lifecycle and actor orchestration.
//!
//! This module defines the `Application` handle and its underlying `AppActor`.
//! The `Application` struct serves as the primary FFI-compatible entry point,
//! while the `AppActor` manages the long-running background tasks, including:
//!
//! - **Network Endpoint Lifecycle:** Initializing and monitoring the `iroh` P2P stack.
//! - **Discovery Management:** Coordinating periodic location publishing and peer resolution.
//! - **Chat Session Coordination:** Spawning and managing individual `ChatManager` actors.
//!
//! All cross-thread state changes and network events are serialized through the
//! actor's message queue to ensure thread safety and consistent state across
//! platform boundaries.

use crate::{
    error::{GossipError, LibError},
    manager::ChatManager,
    network_endpoint::NetworkEndpoint,
    persistence::{ChatId, ChatRepository},
    protocols::{
        chat::{ChatConnection, ChatConnectionHandle},
        tryst::{
            invite::{self, InviteManager},
            persistence::TrystRepository,
        },
    },
};

use anyhow::{Context, anyhow};
use dashmap::{DashMap, Entry};
use iroh::{EndpointId, PublicKey, SecretKey, discovery::pkarr::DEFAULT_PKARR_TTL};
use sqlx::{Sqlite, SqlitePool, migrate::MigrateDatabase, sqlite::SqlitePoolOptions};
use std::fmt::Debug;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

#[derive(uniffi::Enum, Debug, Clone)]
pub enum CreateInviteType {
    // Open,
    Sealed { recipient_pk: String },
}

#[uniffi::export]
pub fn generate_private_key() -> String {
    let secret_key = iroh::SecretKey::generate(&mut rand::rng());
    data_encoding::HEXLOWER.encode(&secret_key.to_bytes())
}

pub fn secret_key_from_hex_lower(secret_key_str: &str) -> anyhow::Result<SecretKey> {
    let decoded_bytes = data_encoding::HEXLOWER
        .decode(secret_key_str.as_bytes())
        .context("Failed to decode Secret Key from HEX Lower str.")?;
    let decoded_key = TryInto::<[u8; 32]>::try_into(decoded_bytes)
        .map_err(|_| anyhow!("Provided Secret Key hex str is wrong length."))?;
    let secret_key = iroh::SecretKey::from_bytes(&decoded_key);

    Ok(secret_key)
}

pub fn generate_secret_key() -> iroh::SecretKey {
    iroh::SecretKey::generate(&mut rand::rng())
}

#[derive(uniffi::Object, Clone, Debug)]
pub struct Application {
    inner: Arc<Inner>,
}

#[derive(Debug)]
pub(crate) struct Inner {
    pub(crate) secret_key: SecretKey,
    pub(crate) db_pool: SqlitePool,
    #[allow(dead_code)]
    endpoint: iroh::Endpoint,
    invite_manager: InviteManager,
    app_actor_handle: AppActorHandle,
    app_actor_join_handle: Mutex<Option<JoinHandle<()>>>,
    cancel_token: CancellationToken,
}

#[derive(Debug)]
struct AppActor {
    network_endpoint: NetworkEndpoint,
    db_pool: SqlitePool,
    chat_managers: DashMap<ChatId, ChatManager>,
    secret_key: SecretKey,
    request_rx: mpsc::Receiver<AppActorRequest>,
    cancel_token: CancellationToken,
}

#[derive(Debug)]
struct AppActorHandle {
    tx: mpsc::Sender<AppActorRequest>,
}

#[derive(Debug)]
enum AppActorRequest {
    GetChatManager {
        remote_id: PublicKey,
        respond_to: oneshot::Sender<Result<ChatManager, LibError>>,
    },
}

impl Application {
    #[tracing::instrument(skip_all, err)]
    pub async fn new_with_config(
        secret_key: String,
        db_directory_path: String,
        republish_delay: std::time::Duration,
    ) -> Result<Application, GossipError> {
        let secret_key =
            SecretKey::from_str(&secret_key).context("Expected to get secret key from string.")?;
        let cancel_token = CancellationToken::new();
        let db_url = format!("{db_directory_path}/{}.db", secret_key.public());

        info!("Database URL: {}", db_url);

        let db_pool = setup_db(&db_url).await?;
        let tryst_repo = TrystRepository::new(db_pool.clone());

        let invite_manager = InviteManager::new(secret_key.clone(), tryst_repo.clone());

        // --- Public Showcase Discovery Setup ---
        // peer discovery falls back to the standard iroh-pkarr-dht mechanism
        // to preserve scientific novelty of the Tryst protocol pending peer-review.
        let discovery = iroh::discovery::pkarr::dht::DhtDiscovery::builder()
            .secret_key(secret_key.to_owned())
            .ttl(republish_delay.as_secs_f32() as u32)
            .include_direct_addresses(false)
            .build()
            .context("Failed to build standard discovery.")?;

        /*
        // NOTE: Tryst Protocol Implementation
        // Logic is currently withheld to preserve scientific novelty pending
        // research publication. See README.md for details.

        let tryst_discovery =
            tryst::discovery::Builder::new(secret_key.clone(), tryst_repo)
                .ttl(DEFAULT_PKARR_TTL)
                .include_direct_addresses(false)
                .republish_delay(republish_delay)
                .build()
                .context("Expected to build tryst discovery.")?;
        */

        let network_endpoint = NetworkEndpoint::start(secret_key.clone(), discovery)
            .await
            .context("Failed to start network endpoint.")?;
        let iroh_endpoint = network_endpoint.inner.endpoint.to_owned();

        let (tx, rx) = tokio::sync::mpsc::channel::<AppActorRequest>(1);
        let mut app_actor = AppActor::new(
            network_endpoint,
            db_pool.clone(),
            secret_key.clone(),
            rx,
            cancel_token.child_token(),
        );
        let app_actor_handle = AppActorHandle { tx };
        let app_actor_join_handle = tokio::task::spawn(async move {
            match app_actor.run().await {
                Ok(()) => info!("AppActor is successfully closed."),
                Err(err) => error!("AppActor failed with error: {err}."),
            };

            app_actor.cleanup().await;
        });

        let app = Application {
            inner: Arc::new(Inner {
                secret_key,
                db_pool,
                endpoint: iroh_endpoint,
                invite_manager,
                app_actor_handle,
                app_actor_join_handle: Mutex::new(Some(app_actor_join_handle)),
                cancel_token,
            }),
        };

        info!("Application initialized successfully.");

        Ok(app)
    }
}

#[uniffi::export]
impl Application {
    #[uniffi::constructor(async_runtime = "tokio")]
    #[tracing::instrument(skip_all, err)]
    pub async fn new(
        secret_key: String,
        db_directory_path: String,
    ) -> Result<Application, GossipError> {
        Self::new_with_config(
            secret_key,
            db_directory_path,
            std::time::Duration::from_secs(DEFAULT_PKARR_TTL as u64),
        )
        .await
    }

    #[uniffi::method(async_runtime = "tokio")]
    #[tracing::instrument(skip(self))]
    pub async fn shutdown(&self) {
        self.inner.cancel_token.cancel();

        let mut actor_join_handle: Option<JoinHandle<()>> = None;

        if let Ok(mut guard) = self.inner.app_actor_join_handle.lock() {
            actor_join_handle = guard.take();
        }

        match actor_join_handle {
            Some(handle) => match handle.await {
                Ok(()) => info!("AppActor finished successfully."),
                Err(e) => warn!("AppActor task failed: {}", e),
            },
            None => {
                warn!("Duplicated access of AppActor handle. It is already awaited.")
            }
        }

        info!("Application shutdown complete.");
    }

    #[uniffi::method]
    pub fn endpoint_id(&self) -> String {
        self.inner.secret_key.public().to_string()
    }

    #[uniffi::method(async_runtime = "tokio")]
    pub async fn all_remote_peers(&self) -> Result<Vec<String>, GossipError> {
        let peers = self
            .inner
            .list_chats_remote_peers_ids()
            .await?
            .iter()
            .map(|node_id| node_id.to_string())
            .collect::<Vec<_>>();

        Ok(peers)
    }

    #[uniffi::method(async_runtime = "tokio")]
    #[tracing::instrument(skip(self), err)]
    pub async fn chat_manager(&self, remote_peer_id: String) -> Result<ChatManager, GossipError> {
        let remote_id = EndpointId::from_str(&remote_peer_id).map_err(LibError::InvalidPeerId)?;
        let (tx, rx) = oneshot::channel::<Result<ChatManager, LibError>>();

        if let Err(err) = self
            .inner
            .app_actor_handle
            .tx
            .send(AppActorRequest::GetChatManager {
                remote_id,
                respond_to: tx,
            })
            .await
        {
            let err = GossipError::from(anyhow::anyhow!(
                "Failed to send GetChatManager message to AppActor. Error: {}",
                err
            ));
            error!("{err}");
            return Err(err);
        }

        match rx.await {
            Ok(chat_manager_res) => chat_manager_res.map_err(GossipError::from),
            Err(_) => {
                let err = GossipError::from(anyhow::anyhow!(
                    "Failed to receive ChatManager from AppActor. Channel is closed."
                ));
                error!("{err}");
                Err(err)
            }
        }
    }

    #[uniffi::method(async_runtime = "tokio")]
    #[tracing::instrument(skip(self), err)]
    pub async fn create_tryst_invite(
        &self,
        invite_type: CreateInviteType,
    ) -> Result<String, GossipError> {
        let invite_type = match invite_type {
            //CreateInviteType::Open => invite::CreateInviteType::Open,
            CreateInviteType::Sealed {
                recipient_pk: pk_str,
            } => {
                let recipient_pk = PublicKey::from_str(&pk_str).context(
                    "Failed to parse PublicKey from provided \
                     string to create tryst invite.",
                )?;
                invite::CreateInviteType::Sealed { recipient_pk }
            }
        };

        self.inner
            .invite_manager
            .create_invite_str(invite_type)
            .await
            .map_err(From::from)
    }

    #[uniffi::method(async_runtime = "tokio")]
    #[tracing::instrument(skip(self), err)]
    pub async fn receive_tryst_invite(&self, invite_str: String) -> Result<(), GossipError> {
        let sender_endpoint_id = self
            .inner
            .invite_manager
            .receive_invite_str(&invite_str)
            .await?;

        // By retrieving the ChatManager for the sender, we implicitly:
        // 1. Create a persistent database record for this chat session.
        // 2. Trigger an outgoing connection attempt to the remote peer (the invitee).
        // This ensures the remote peer receives an incoming connection request,
        // allowing them to also initialize the chat session and create their own DB record.
        let _ = self.chat_manager(sender_endpoint_id.to_string()).await?;

        Ok(())
    }
}

impl AppActor {
    #[tracing::instrument(skip_all)]
    pub fn new(
        network_endpoint: NetworkEndpoint,
        db_pool: SqlitePool,
        secret_key: SecretKey,
        request_rx: mpsc::Receiver<AppActorRequest>,
        cancel_token: CancellationToken,
    ) -> Self {
        Self {
            network_endpoint,
            db_pool,
            chat_managers: DashMap::new(),
            secret_key,
            request_rx,
            cancel_token,
        }
    }

    #[tracing::instrument(skip_all)]
    async fn run(&mut self) -> anyhow::Result<()> {
        let chat_proto = self.network_endpoint.inner.chat.clone();

        loop {
            tokio::select! {
                biased;
                _ = self.cancel_token.cancelled() => {
                    info!("AppActor: Cancellation token called.");
                    self.db_pool.close().await;
                    return Ok(());
                }
                maybe_msg = self.request_rx.recv() => {
                    let Some(msg) = maybe_msg else {
                        return Err(anyhow!("AppActor: Exiting message loop due to channel closure."));
                    };

                    self.handle_actor_request(msg).await;
                }
                accept_conn_res = chat_proto.accept_conn() => {
                    match accept_conn_res {
                        Ok(conn_handle) => {
                            debug!(remote_id = ?conn_handle.remote_id(), "AppActor: Accepted new chat connection handle.");

                            if let Err(err) = self.handle_chat_manager(conn_handle.remote_id(), Some(conn_handle)).await {
                                warn!(?err, "AppActor: Failed to pass Accepted conn to ChatManager.");
                            }
                        },
                        Err(err) => {
                            warn!(?err, "AppActor: Failed accepting chat connection.");
                        },
                    }
                }
            }
        }
    }

    #[tracing::instrument(skip_all)]
    async fn handle_actor_request(&mut self, msg: AppActorRequest) {
        match msg {
            AppActorRequest::GetChatManager {
                remote_id,
                respond_to,
            } => {
                let result = self.handle_chat_manager(remote_id, None).await;

                if respond_to.send(result).is_err() {
                    warn!("AppActor: Failed to send ChatManager response, receiver dropped.");
                }
            }
        }
    }

    #[tracing::instrument(skip_all, fields(%remote_id), err)]
    async fn handle_chat_manager(
        &self,
        remote_id: EndpointId,
        conn_handle: Option<ChatConnectionHandle>,
    ) -> Result<ChatManager, LibError> {
        let chat_id = ChatId {
            my_id: self.secret_key.public(),
            remote_id,
        };

        if let Some(manager) = self.chat_managers.get(&chat_id).map(|r| r.to_owned()) {
            if let Some(handle) = conn_handle {
                debug!("AppActor: Injecting new connection handle into existing manager.");
                manager.pass_accepted_conn_handle(handle).await;
            }

            return Ok(manager);
        }

        debug!("AppActor: Creating new ChatManager.");

        let doc = ChatRepository::new(self.db_pool.clone(), chat_id).await?;
        let protocol_tx = self.network_endpoint.inner.chat.sender();

        let chat_manager = match self.chat_managers.entry(chat_id) {
            Entry::Occupied(occupied) => {
                debug!("AppActor: Race condition lost. Using existing manager.");
                let manager = occupied.get().clone();

                if let Some(handle) = conn_handle {
                    debug!("AppActor: Injecting connection handle into race-winner manager.");
                    let manager_clone = manager.clone();
                    tokio::spawn(async move {
                        manager_clone.pass_accepted_conn_handle(handle).await;
                    });
                }

                manager
            }
            Entry::Vacant(vacant) => {
                let cancel_token_child = self.cancel_token.child_token();
                let local_id = self.network_endpoint.inner.endpoint.id();
                let chat_conn = match conn_handle {
                    Some(handle) => ChatConnection::new_connected(local_id, handle, protocol_tx),
                    None => ChatConnection::new(local_id, remote_id, protocol_tx),
                };

                let new_manager = ChatManager::new(doc, chat_conn, cancel_token_child);

                vacant.insert(new_manager).value().clone()
            }
        };

        Ok(chat_manager)
    }

    #[tracing::instrument(skip_all)]
    async fn cleanup(self) {
        if let Err(err) = self.network_endpoint.inner.router.shutdown().await {
            error!("AppActor: Error during network endpoint shutdown: {}", err);
        }

        info!("AppActor: Endpoint connection shut down.");
    }
}

#[tracing::instrument(skip_all, err)]
pub async fn setup_db(db_url: &str) -> Result<SqlitePool, LibError> {
    if !Sqlite::database_exists(db_url).await? {
        tracing::debug!("Database does not exist, creating new one.");
        Sqlite::create_database(db_url).await?;
    } else {
        tracing::debug!("Database already exists.");
    }

    let db_pool = SqlitePoolOptions::new().connect(db_url).await?;
    info!("Database successfully connected.");

    sqlx::migrate!("./migrations").run(&db_pool).await?;
    info!("Database migrations applied.");

    Ok(db_pool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        manager::tests::TestChatManagerStateReceiver,
        persistence::{ChatMessage, ChatMessageContent},
        protocols::chat::UniffiChatConnectionState,
    };
    use std::time::Duration;
    use tracing::info;
    use tracing_test::traced_test;

    #[derive(Debug, Clone)]
    struct TestPeerConn {
        _public_key: PublicKey,
        _secret_key: SecretKey,
        secret_key_str: String,
        other_peer_pk: PublicKey,
        app: Application,
        chat_manager: ChatManager,
        chat_state: TestChatManagerStateReceiver,
    }

    fn get_peer_keys() -> (SecretKey, PublicKey, String) {
        let sk_str = generate_private_key();
        let sk = secret_key_from_hex_lower(&sk_str).unwrap();
        let pk = sk.public();

        (sk, pk, sk_str)
    }

    async fn start_application(secret_key_str: String) -> Application {
        tokio::fs::create_dir_all("./target/tests/dbs")
            .await
            .expect("Expect to create dir for tests dbs.");

        let app = Application::new(secret_key_str, "./target/tests/dbs".to_string())
            .await
            .expect("Expected to create Application.");

        app
    }

    async fn get_manager_and_state_for_chat(
        app: &Application,
        remote_endpoint_pk: PublicKey,
    ) -> (ChatManager, TestChatManagerStateReceiver) {
        let chat_manager = app
            .chat_manager(remote_endpoint_pk.to_string())
            .await
            .expect("Expected to get chat manager");
        let chat_state = {
            let all_messages = chat_manager
                .get_all_messages()
                .await
                .expect("Expected to get all messages.");
            TestChatManagerStateReceiver::new(all_messages)
        };
        chat_manager
            .subscribe(Arc::new(chat_state.clone()))
            .await
            .expect("Expected to subscribe to chat manager state.");

        (chat_manager, chat_state)
    }

    async fn setup_connected_peers_pair() -> (TestPeerConn, TestPeerConn) {
        let (sk_a, pk_a, sk_a_str) = get_peer_keys();
        let (sk_b, pk_b, sk_b_str) = get_peer_keys();

        let app_a = start_application(sk_a_str.to_owned()).await;
        let app_b = start_application(sk_b_str.to_owned()).await;
        app_a.inner.endpoint.online().await;
        app_b.inner.endpoint.online().await;

        let tryst_invite_str = app_a
            .create_tryst_invite(CreateInviteType::Sealed {
                recipient_pk: pk_b.to_string(),
            })
            .await
            .unwrap();
        app_b.receive_tryst_invite(tryst_invite_str).await.unwrap();

        let (a_chat_manager, a_chat_state) = get_manager_and_state_for_chat(&app_a, pk_b).await;
        let (b_chat_manager, b_chat_state) = get_manager_and_state_for_chat(&app_b, pk_a).await;

        let peer_a = TestPeerConn {
            _public_key: pk_a,
            _secret_key: sk_a,
            secret_key_str: sk_a_str,
            other_peer_pk: pk_b,
            app: app_a,
            chat_manager: a_chat_manager,
            chat_state: a_chat_state,
        };
        let peer_b = TestPeerConn {
            _public_key: pk_b,
            _secret_key: sk_b,
            secret_key_str: sk_b_str,
            other_peer_pk: pk_a,
            app: app_b,
            chat_manager: b_chat_manager,
            chat_state: b_chat_state,
        };

        peer_a.wait_for_success_conn_with_timeout(&peer_b).await;

        (peer_a, peer_b)
    }

    impl TestPeerConn {
        async fn restart(&mut self) {
            self.app.shutdown().await;

            self.app = start_application(self.secret_key_str.to_owned()).await;
            self.app.inner.endpoint.online().await;

            let (new_chat_manager, new_chat_state) =
                get_manager_and_state_for_chat(&self.app, self.other_peer_pk).await;
            self.chat_manager = new_chat_manager;
            self.chat_state = new_chat_state;
        }

        async fn wait_for_success_conn_with_timeout(&self, other: &TestPeerConn) {
            let _ = tokio::time::timeout(std::time::Duration::from_secs(60), async {
                let mut self_rx = self.chat_state.conn_state_watcher.lock().await;
                let mut other_rx = other.chat_state.conn_state_watcher.lock().await;

                loop {
                    let self_state = self_rx.borrow_and_update().to_owned();
                    let other_state = other_rx.borrow_and_update().to_owned();

                    if matches!(self_state, UniffiChatConnectionState::Connected { .. })
                        && matches!(other_state, UniffiChatConnectionState::Connected { .. })
                    {
                        break;
                    }

                    tokio::select! {
                        _ = self_rx.changed() => {},
                        _ = other_rx.changed() => {},
                    }
                }
            })
            .await;

            let self_rx = self.chat_state.conn_state_watcher.lock().await;
            let other_rx = other.chat_state.conn_state_watcher.lock().await;

            assert!(
                matches!(
                    *self_rx.borrow(),
                    UniffiChatConnectionState::Connected { .. }
                ),
                "Expected chat to be connected."
            );
            assert!(
                matches!(
                    *other_rx.borrow(),
                    UniffiChatConnectionState::Connected { .. }
                ),
                "Expected chat to be connected."
            );
        }

        async fn send_random_text_msg(&self) -> ChatMessage {
            let new_msg = self
                .chat_manager
                .send_message(ChatMessageContent::Text(uuid::Uuid::new_v4().to_string()))
                .await
                .unwrap();

            let mut current_msgs = self
                .chat_state
                .all_msgs
                .lock()
                .expect("Expected to get chat history lock for tests.");

            if current_msgs.iter().find(|m| m.id == new_msg.id).is_none() {
                current_msgs.push(new_msg.clone());
            }

            new_msg
        }

        async fn assert_equal_chats_msgs(&self, other: &TestPeerConn) {
            let timeout = std::time::Duration::from_secs(90);

            let mut rx_a = self.chat_state.msg_update_rx.lock().await.clone();
            let mut rx_b = other.chat_state.msg_update_rx.lock().await.clone();

            let check_equal = || {
                let msgs_a = self.chat_state.all_msgs.lock().unwrap();
                let msgs_b = other.chat_state.all_msgs.lock().unwrap();
                !msgs_a.is_empty() && *msgs_a == *msgs_b
            };

            if check_equal() {
                return;
            }

            let wait_fut = async {
                loop {
                    tokio::select! {
                        res = rx_a.changed() => {
                            if res.is_ok() && check_equal() { return; }
                        },
                        res = rx_b.changed() => {
                            if res.is_ok() && check_equal() { return; }
                        },
                    }
                }
            };

            if tokio::time::timeout(timeout, wait_fut).await.is_err() {
                let msgs_a = self.chat_state.all_msgs.lock().unwrap();
                let msgs_b = other.chat_state.all_msgs.lock().unwrap();
                assert_eq!(
                    *msgs_a, *msgs_b,
                    "Peers failed to converge within {:?}.",
                    timeout
                );
            }
        }

        fn assert_collected_all_msgs(&self, all_messages: &mut [ChatMessage]) {
            all_messages.sort_by(|a, b| {
                a.created_at
                    .cmp(&b.created_at)
                    .then_with(|| a.id.cmp(&b.id))
            });

            assert_eq!(
                *self.chat_state.all_msgs.lock().unwrap(),
                all_messages,
                "Peers don't have all the messages."
            );
        }
    }

    /// Integration Tests
    ///
    /// These tests verify the end-to-end engineering flow: identity generation,
    /// trust establishment (invites), peer discovery, QUIC transport, and
    /// CRDT state convergence.
    ///
    /// Note: Peer discovery falls back to the standard BitTorrent DHT in this
    /// public showcase. Tests are subject to network latency and propagation
    /// delays; generous timeouts are provided.

    #[tokio::test]
    #[traced_test]
    async fn test_initial_connection_with_sealed_invite() {
        let _ = setup_connected_peers_pair().await;
    }

    #[tokio::test]
    #[traced_test]
    async fn test_peers_conn_with_multiple_disconnection() {
        let (mut peer_a, mut peer_b) = setup_connected_peers_pair().await;

        for _ in 0..4 {
            match rand::random_bool(0.5) {
                true => {
                    peer_a.restart().await;
                }
                false => {
                    peer_b.restart().await;
                }
            };

            peer_a.wait_for_success_conn_with_timeout(&peer_b).await;
        }
    }

    #[tokio::test]
    #[traced_test]
    async fn test_peers_conn_with_online_messages() {
        let (peer_a, peer_b) = setup_connected_peers_pair().await;

        let mut all_messages = vec![];

        for _ in 0..30 {
            match rand::random_bool(0.5) {
                true => all_messages.push(peer_a.send_random_text_msg().await),
                false => all_messages.push(peer_b.send_random_text_msg().await),
            };
        }

        peer_a.wait_for_success_conn_with_timeout(&peer_b).await;
        peer_a.assert_equal_chats_msgs(&peer_b).await;
        peer_a.assert_collected_all_msgs(&mut all_messages);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_peers_conn_with_messages_and_disconnections() {
        let (mut peer_a, mut peer_b) = setup_connected_peers_pair().await;

        let mut all_messages = vec![];

        for _ in 0..10 {
            for _ in 0..20 {
                match rand::random_bool(0.5) {
                    true => all_messages.push(peer_a.send_random_text_msg().await),
                    false => all_messages.push(peer_b.send_random_text_msg().await),
                };
            }

            match rand::random_bool(0.5) {
                true => {
                    peer_a.restart().await;
                }
                false => {
                    peer_b.restart().await;
                }
            };
        }

        peer_a.wait_for_success_conn_with_timeout(&peer_b).await;
        peer_a.assert_equal_chats_msgs(&peer_b).await;
        peer_a.assert_collected_all_msgs(&mut all_messages);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_peers_conn_with_offline_messages_and_various_disconnections() {
        let (mut peer_a, mut peer_b) = setup_connected_peers_pair().await;

        let mut all_messages = vec![];

        let loop_limit = 6;
        for loop_i in 0..loop_limit {
            let is_even = loop_i % 2 == 0;
            for _ in 0..10 {
                match rand::random_bool(0.5) {
                    true => all_messages.push(peer_a.send_random_text_msg().await),
                    false => all_messages.push(peer_b.send_random_text_msg().await),
                };
            }

            if rand::random_bool(0.5) {
                peer_a.app.shutdown().await;
                debug!("Peer A is shutdown.");

                for _ in 0..10 {
                    all_messages.push(peer_b.send_random_text_msg().await)
                }

                if is_even {
                    peer_b.app.shutdown().await;
                    debug!("Peer B is also shutdown.");
                }

                peer_a.restart().await;

                if is_even {
                    for _ in 0..10 {
                        all_messages.push(peer_a.send_random_text_msg().await)
                    }

                    peer_b.restart().await;
                }

                debug!("Peer A is restarted.");
            } else {
                peer_b.app.shutdown().await;
                debug!("Peer B is shutdown.");

                for _ in 0..10 {
                    all_messages.push(peer_a.send_random_text_msg().await);
                }

                if is_even {
                    peer_a.app.shutdown().await;
                    debug!("Peer A is also shutdown.");
                }

                peer_b.restart().await;

                if is_even {
                    for _ in 0..10 {
                        all_messages.push(peer_b.send_random_text_msg().await);
                    }

                    peer_a.restart().await;
                }
                debug!("Peer B is restarted.");
            }
        }

        peer_a.wait_for_success_conn_with_timeout(&peer_b).await;
        peer_a.assert_equal_chats_msgs(&peer_b).await;
        peer_a.assert_collected_all_msgs(&mut all_messages);
    }

    #[tokio::test]
    #[traced_test]
    async fn test_heavy_concurrency_stress() {
        let (peer_a, peer_b) = setup_connected_peers_pair().await;

        let message_count = 70;

        let task_a = {
            let peer = peer_a.clone();
            tokio::spawn(async move {
                let mut all_messages = vec![];

                for _ in 0..message_count {
                    all_messages.push(peer.send_random_text_msg().await);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }

                all_messages
            })
        };

        let task_b = {
            let peer = peer_b.clone();
            tokio::spawn(async move {
                let mut all_messages = vec![];

                for _ in 0..message_count {
                    all_messages.push(peer.send_random_text_msg().await);
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }

                all_messages
            })
        };

        let (res_a, res_b) = tokio::join!(task_a, task_b);
        let mut all_messages = [res_a.unwrap(), res_b.unwrap()].concat();
        info!("TEST: All messages sent. Waiting for convergence...");

        peer_a.assert_equal_chats_msgs(&peer_b).await;
        peer_a.assert_collected_all_msgs(&mut all_messages);
    }
}
