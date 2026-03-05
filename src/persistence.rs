//! CRDT Persistence and synchronization layer.
//!
//! This module bridges the `automerge` document state with the local `SQLite` database.
//! To manage the unbounded memory growth inherent to CRDTs, this system implements
//! a manual compaction strategy.
//!
//! When a document's operation history exceeds `CHAT_LOGS_LIMIT`, the patch log is
//! truncated and a new "snapshot" is synchronized to disk via `autosurgeon`.
//! Additionally, we use a stripped-down initialization struct (`ChatInit`) to prevent
//! "Empty List vs Empty List" merge conflicts during initial document creation across peers.

use crate::error::LibError;
use tracing::{instrument, warn};

use anyhow::{Context, anyhow};
use automerge::{
    Automerge, PatchLog, Prop,
    patches::Patch,
    sync::{self, SyncDoc},
};
use autosurgeon::{Hydrate, Reconcile, hydrate_prop, reconcile};
use iroh::EndpointId;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use std::str::FromStr;
use tokio::sync::watch;
use uuid::Uuid;

mod schema;

const CHAT_LOGS_LIMIT: usize = 512usize;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Reconcile, Hydrate)]
pub struct ChatHistory {
    pub chat_id: String,
    pub messages: Vec<ChatMessage>,
}

// USED FOR INITIALIZATION (Reconcile)
// We use this stripped-down struct to initialize the document *without* creating
// the 'chunks' list. This prevents "Empty List vs Empty List" conflicts
// that happens in CRDTs.
#[derive(Debug, Clone, Serialize, Reconcile)]
struct ChatInit {
    pub chat_id: String,
}

#[derive(
    uniffi::Record,
    Debug,
    Clone,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    Reconcile,
    Hydrate,
)]
pub struct ChatMessage {
    #[key]
    pub id: String,
    pub sender_node_id: String,
    pub content: ChatMessageContent,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(
    uniffi::Enum,
    Debug,
    Hash,
    Eq,
    Clone,
    PartialEq,
    Serialize,
    Deserialize,
    Reconcile,
    Hydrate,
)]
pub enum ChatMessageContent {
    Text(String),
}

#[derive(Debug, Eq, Hash, PartialEq, Clone, Copy)]
pub struct ChatId {
    pub my_id: EndpointId,
    pub remote_id: EndpointId,
}

impl std::fmt::Display for ChatId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut nodes = [self.my_id, self.remote_id];
        nodes.sort();

        write!(f, "{}_{}", nodes[0], nodes[1])
    }
}

#[derive(Debug)]
pub struct ChatRepository {
    inner: ChatRepositoryInner,
}

#[derive(Debug)]
struct ChatRepositoryInner {
    chat_id: ChatId,
    doc: Automerge,
    sync_state: sync::State,
    db_pool: SqlitePool,
    last_saved_heads: Vec<automerge::ChangeHash>,
    sync_status_tx: watch::Sender<bool>,
}

impl ChatRepository {
    pub async fn new(
        db_pool: SqlitePool,
        chat_id: ChatId,
    ) -> Result<ChatRepository, LibError> {
        let (doc, last_saved_heads) =
            get_persisted_doc_state(chat_id, &db_pool).await?;
        let sync_state = sync::State::new();
        let sync_status_tx = watch::Sender::new(false);

        let chat_doc = ChatRepository {
            inner: ChatRepositoryInner {
                chat_id,
                doc,
                sync_state,
                db_pool: db_pool.clone(),
                last_saved_heads,
                sync_status_tx,
            },
        };

        Ok(chat_doc)
    }

    pub fn subscribe_to_sync_status(&self) -> watch::Receiver<bool> {
        self.inner.sync_status_tx.subscribe()
    }

    #[instrument(skip(self, msg), err)]
    pub async fn add_message(
        &mut self,
        msg: ChatMessageContent,
    ) -> std::result::Result<(ChatMessage, Option<sync::Message>), LibError> {
        let timestamp_millis_now = chrono::Utc::now().timestamp_millis();
        let new_msg_id = Uuid::new_v4().to_string();
        let my_peer_id = self.inner.chat_id.my_id.to_string();

        let new_msg_struct = ChatMessage {
            id: new_msg_id.clone(),
            sender_node_id: my_peer_id.clone(),
            content: msg.clone(),
            created_at: timestamp_millis_now,
            updated_at: timestamp_millis_now,
        };

        self.inner
            .doc
            .transact::<_, _, LibError>(|tx| {
                let root = schema::Root;
                let msgs_list = root.ensure_peer_list(tx, &my_peer_id)?;

                msgs_list.append_message(tx, &new_msg_struct)?;

                Ok(())
            })
            .map_err(|e| e.error)?;

        self.persist_chat().await?;

        let sync_message = self.generate_sync_message();

        Ok((new_msg_struct, sync_message))
    }

    pub fn chat_id(&self) -> ChatId {
        self.inner.chat_id
    }

    #[instrument(skip(self), err)]
    pub fn get_all_messages(&self) -> Result<Vec<ChatMessage>, LibError> {
        self.get_messages(0, usize::MAX - 1)
    }

    /// Fetches messages with pagination.
    ///
    /// * `offset`: How many messages to skip from the END (most recent).
    /// * `limit`: How many messages to return.
    #[instrument(skip(self), err)]
    pub fn get_messages(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<ChatMessage>, LibError> {
        let root = schema::Root;
        let lists = root.get_all_peer_lists(&self.inner.doc)?;

        let mut candidates = Vec::new();
        let fetch_depth = offset + limit;

        for list in lists {
            let len = list.len(&self.inner.doc);
            if len == 0 {
                continue;
            }

            let start = len.saturating_sub(fetch_depth);
            for i in start..len {
                let msg = self.hydrate_message_at_index(list.inner(), i)?;
                candidates.push(msg);
            }
        }

        candidates.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });

        let total = candidates.len();
        let end = total.saturating_sub(offset);
        let start = end.saturating_sub(limit);

        if start >= end {
            return Ok(vec![]);
        }

        Ok(candidates[start..end].to_vec())
    }

    /// There are 3 states of sync message.
    /// 1. Some(Some(sync::Message)) -- There are some changes that the other peer should receive.
    /// 2. Some(None) --  There are no more changes and other peer should receive it.
    /// 3. None -- There are no more changes and nothing should be send to other peer.
    #[instrument(skip_all, fields(chat_id = %self.inner.chat_id), err)]
    pub async fn receive_sync_message(
        &mut self,
        in_sync_msg: Option<sync::Message>,
    ) -> Result<(Vec<ChatMessage>, Option<Option<sync::Message>>), LibError> {
        let in_sync_msg_none = in_sync_msg.is_none();

        let new_messages = if let Some(in_sync_msg) = in_sync_msg {
            let mut patch_log = PatchLog::new(true);

            self.inner
                .doc
                .receive_sync_message_log_patches(
                    &mut self.inner.sync_state,
                    in_sync_msg,
                    &mut patch_log,
                )
                .map_err(|e| LibError::Automerge(anyhow!(e)))?;

            self.persist_chat().await?;

            let patches = self.inner.doc.make_patches(&mut patch_log);
            self.extract_new_messages_from_patches(patches)?
        } else {
            vec![]
        };

        let out_sync_msg = self.generate_sync_message();
        let final_out_sync_msg = if in_sync_msg_none && out_sync_msg.is_none() {
            None
        } else {
            Some(out_sync_msg)
        };

        Ok((new_messages, final_out_sync_msg))
    }

    pub fn generate_sync_message(&mut self) -> Option<sync::Message> {
        let message = self
            .inner
            .doc
            .generate_sync_message(&mut self.inner.sync_state);

        let is_synced = message.is_none();

        self.inner.sync_status_tx.send_if_modified(|current| {
            if *current != is_synced {
                *current = is_synced;
                true
            } else {
                false
            }
        });

        message
    }

    fn extract_new_messages_from_patches(
        &self,
        patches: Vec<Patch>,
    ) -> Result<Vec<ChatMessage>, LibError> {
        let mut new_msgs = Vec::new();
        let root = schema::Root;

        for patch in patches {
            if patch.path.len() >= 2 {
                if let Some((_, Prop::Map(key))) = patch.path.first()
                    && key.starts_with(schema::keys::LIST_PREFIX)
                    && let Some((_, Prop::Seq(index))) = patch.path.get(1)
                {
                    let peer_id_suffix = key
                        .strip_prefix(schema::keys::LIST_PREFIX)
                        .ok_or_else(|| {
                            LibError::Unexpected(anyhow::anyhow!(
                                "Internal logic error: expected prefix missing"
                            ))
                        })?;

                    if let Some(list_id) =
                        root.get_peer_list(&self.inner.doc, peer_id_suffix)?
                    {
                        let msg =
                            self.hydrate_message_at_index(list_id.inner(), *index)?;
                        new_msgs.push(msg);
                    }
                }
            }
        }

        new_msgs.sort_by_key(|m| m.created_at);
        Ok(new_msgs)
    }

    fn hydrate_message_at_index(
        &self,
        list_id: &automerge::ObjId,
        idx: usize,
    ) -> Result<ChatMessage, LibError> {
        let msg: ChatMessage = hydrate_prop(&self.inner.doc, list_id, idx)
            .map_err(|e| LibError::Automerge(anyhow::anyhow!(e)))?;
        Ok(msg)
    }

    #[instrument(skip(self), err)]
    async fn persist_chat(&mut self) -> std::result::Result<(), LibError> {
        let chat_id_str = self.inner.chat_id.to_string();

        let new_changes = self.inner.doc.save_after(&self.inner.last_saved_heads);

        if !new_changes.is_empty() {
            sqlx::query!(
                "INSERT INTO chat_log (chat_id, data) VALUES (?, ?)",
                chat_id_str,
                new_changes
            )
            .execute(&self.inner.db_pool)
            .await?;

            self.inner.last_saved_heads = self.inner.doc.get_heads();
        }

        Ok(())
    }

    #[instrument(skip(self), fields(chat_id = ?self.inner.chat_id))]
    pub(crate) async fn reset_doc_state_to_persisted(
        &mut self,
    ) -> Result<(), LibError> {
        let (doc, last_saved_heads) =
            get_persisted_doc_state(self.inner.chat_id, &self.inner.db_pool).await?;

        self.inner.doc = doc;
        self.inner.last_saved_heads = last_saved_heads;
        self.reset_sync_state();

        Ok(())
    }

    #[instrument(skip(self), fields(chat_id = ?self.inner.chat_id))]
    pub(crate) fn reset_sync_state(&mut self) {
        self.inner.sync_state = sync::State::new();
    }
}

impl crate::app::Inner {
    pub async fn list_chats_remote_peers_ids(
        &self,
    ) -> Result<Vec<EndpointId>, LibError> {
        let my_id_str = self.secret_key.public().to_string();
        let chat_ids: Vec<String> = sqlx::query_scalar!(
            r#"SELECT chat_id AS "chat_id!: String" FROM chats"#
        )
        .fetch_all(&self.db_pool)
        .await?;

        let mut peer_ids = Vec::<EndpointId>::new();
        for chat_id in chat_ids {
            let parts = chat_id.split_once('_');

            match parts {
                Some((first, second)) => {
                    let remote_node_id_str = match (
                        first == my_id_str,
                        second == my_id_str,
                    ) {
                        (true, false) => second,
                        (false, true) => first,
                        (true, true) => {
                            warn!(
                                "Chats with yourself is not supported yet for chat_id: {}",
                                chat_id
                            );
                            continue;
                        }
                        (false, false) => {
                            warn!(
                                "Found chat_id '{}' that does not include own ID.",
                                chat_id
                            );
                            continue;
                        }
                    };

                    let remote_node_id =
                        EndpointId::from_str(remote_node_id_str).context(format!(
                            "Expected to parse EndpointId from string. Value: {remote_node_id_str}",
                        ))?;
                    peer_ids.push(remote_node_id);
                }
                None => {
                    warn!(
                        "Found potentially invalid chat_id format in DB: {}",
                        chat_id
                    );
                    continue;
                }
            }
        }

        peer_ids.sort_unstable();
        peer_ids.dedup();

        Ok(peer_ids)
    }
}

#[instrument(skip(db_pool, chat_id), err)]
async fn get_persisted_doc_state(
    chat_id: ChatId,
    db_pool: &SqlitePool,
) -> Result<(Automerge, Vec<automerge::ChangeHash>), LibError> {
    let chat_id_str = chat_id.to_string();

    loop {
        let snapshot_row = sqlx::query!(
            "SELECT document FROM chats WHERE chat_id = ?",
            chat_id_str
        )
        .fetch_optional(db_pool)
        .await?;

        if let Some(row) = snapshot_row {
            let mut doc = Automerge::load(&row.document)
                .map_err(|e| LibError::Automerge(anyhow::anyhow!(e)))?;

            let log_rows = sqlx::query!(
                r#"SELECT id AS "id!", data FROM chat_log WHERE chat_id = ? ORDER BY id ASC"#,
                chat_id_str
            )
            .fetch_all(db_pool)
            .await?;

            if !log_rows.is_empty() {
                let mut change_chunks = Vec::with_capacity(log_rows.len());
                for row in &log_rows {
                    change_chunks.push(row.data.clone());
                }
                doc.load_incremental(&change_chunks.concat())
                    .map_err(|e| LibError::Automerge(anyhow::anyhow!(e)))?;

                if log_rows.len() > CHAT_LOGS_LIMIT {
                    tracing::debug!("Compacting chat log for {}", chat_id_str);

                    let new_snapshot = doc.save();
                    let max_log_id: i64 = log_rows.last().map(|r| r.id).unwrap_or(0);

                    let mut tx = db_pool.begin().await?;

                    sqlx::query!(
                        "UPDATE chats SET document = ? WHERE chat_id = ?",
                        new_snapshot,
                        chat_id_str
                    )
                    .execute(&mut *tx)
                    .await?;

                    sqlx::query!(
                        "DELETE FROM chat_log WHERE chat_id = ? AND id <= ?",
                        chat_id_str,
                        max_log_id
                    )
                    .execute(&mut *tx)
                    .await?;

                    tx.commit().await?;
                }
            }

            let heads = doc.get_heads();
            return Ok((doc, heads));
        } else {
            let mut doc = Automerge::new();

            doc.transact::<_, _, anyhow::Error>(|tx| {
                reconcile(
                    tx,
                    &ChatInit {
                        chat_id: chat_id.to_string(),
                    },
                )
                .map_err(|e| LibError::Automerge(anyhow::anyhow!(e)))?;
                Ok(())
            })
            .map_err(|e| e.error)?;

            let initial_bytes = doc.save();

            let result = sqlx::query!(
                "INSERT OR IGNORE INTO chats (chat_id, document) VALUES (?, ?)",
                chat_id_str,
                initial_bytes
            )
            .execute(db_pool)
            .await?;

            if result.rows_affected() > 0 {
                let heads = doc.get_heads();
                return Ok((doc, heads));
            } else {
                tracing::debug!(
                    "Race condition detected in get_persisted_doc_state. Retrying."
                );
                continue;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;
    use sqlx::SqlitePool;
    use tracing::info;
    use tracing_test::traced_test;

    async fn setup_db() -> SqlitePool {
        let db_pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::migrate!("./migrations")
            .run(&db_pool)
            .await
            .expect("Expect to run sqlite migration.");
        db_pool
    }

    fn generate_chat_id() -> (ChatId, SecretKey, SecretKey) {
        let k1 = SecretKey::generate(&mut rand::rng());
        let k2 = SecretKey::generate(&mut rand::rng());
        let chat_id = ChatId {
            my_id: k1.public(),
            remote_id: k2.public(),
        };
        (chat_id, k1, k2)
    }

    async fn sync_repos(
        cause_repo: &mut ChatRepository,
        receive_repo: &mut ChatRepository,
        cause_repo_sync_msg: Option<sync::Message>,
    ) -> Result<(), LibError> {
        let mut sync_from_cause: Option<sync::Message> = cause_repo_sync_msg;
        let mut sync_from_receive: Option<sync::Message>;
        let mut counter = 0;

        loop {
            counter += 1;

            let (_, new_sync_from_receive) =
                receive_repo.receive_sync_message(sync_from_cause).await?;

            match new_sync_from_receive {
                None => break,
                Some(other) => sync_from_receive = other,
            }

            let (_, new_sync_from_cause) =
                cause_repo.receive_sync_message(sync_from_receive).await?;

            match new_sync_from_cause {
                None => break,
                Some(other) => sync_from_cause = other,
            }

            assert!(
                counter < 10,
                "Failed to sync 2 repos. Sync loop counter reached limit."
            );
        }

        Ok(())
    }

    #[tokio::test]
    #[traced_test]
    async fn test_persistence_restore() -> Result<(), LibError> {
        let pool = setup_db().await;
        let (chat_id, _, _) = generate_chat_id();

        {
            let mut repo = ChatRepository::new(pool.clone(), chat_id).await?;
            repo.add_message(ChatMessageContent::Text(
                "Hello Persistence".to_string(),
            ))
            .await?;

            let msgs = repo.get_all_messages()?;
            assert_eq!(msgs.len(), 1);
        }

        {
            let repo_restored = ChatRepository::new(pool.clone(), chat_id).await?;
            let msgs = repo_restored.get_all_messages()?;

            assert_eq!(msgs.len(), 1);

            let ChatMessageContent::Text(txt) = &msgs[0].content;
            assert_eq!(txt, "Hello Persistence");
        }

        Ok(())
    }

    #[tokio::test]
    #[traced_test]
    async fn test_incremental_logging() -> Result<(), LibError> {
        let pool = setup_db().await;
        let (chat_id, _, _) = generate_chat_id();
        let chat_id_str = chat_id.to_string();
        let mut repo = ChatRepository::new(pool.clone(), chat_id).await?;
        let limit = 3;

        for i in 0..limit {
            repo.add_message(ChatMessageContent::Text(format!("Msg {}", i)))
                .await?;
        }

        let log_count: i64 = sqlx::query_scalar!(
            "SELECT count(*) FROM chat_log WHERE chat_id = ?",
            chat_id_str
        )
        .fetch_one(&pool)
        .await?;

        assert_eq!(log_count, limit, "Expected {limit} incremental log entries");
        Ok(())
    }

    #[tokio::test]
    #[traced_test]
    async fn test_compaction_trigger() -> Result<(), LibError> {
        info!("Starting Compaction Test");
        let pool = setup_db().await;
        let (chat_id, _, _) = generate_chat_id();
        let chat_id_str = chat_id.to_string();
        let limit = CHAT_LOGS_LIMIT + CHAT_LOGS_LIMIT / 2;

        {
            let mut repo = ChatRepository::new(pool.clone(), chat_id).await?;
            for i in 0..limit {
                repo.add_message(ChatMessageContent::Text(format!(
                    "Compaction Check {}",
                    i
                )))
                .await?;
            }

            let log_count: i64 = sqlx::query_scalar!(
                "SELECT count(*) FROM chat_log WHERE chat_id = ?",
                chat_id_str
            )
            .fetch_one(&pool)
            .await?;
            assert_eq!(log_count, limit as i64, "Logs should pile up before reload");
        }

        info!("Reloading Repo to trigger compaction...");
        let repo_reloaded = ChatRepository::new(pool.clone(), chat_id).await?;

        let log_count_after: i64 = sqlx::query_scalar!(
            "SELECT count(*) FROM chat_log WHERE chat_id = ?",
            chat_id_str
        )
        .fetch_one(&pool)
        .await?;

        assert_eq!(
            log_count_after, 0,
            "Compaction should have cleared the chat_log"
        );

        let all_msgs = repo_reloaded.get_all_messages()?;

        assert_eq!(
            all_msgs.len(),
            limit,
            "All messages must survive compaction"
        );

        Ok(())
    }

    #[tokio::test]
    #[traced_test]
    async fn test_race_condition_simulation() -> Result<(), LibError> {
        let pool = setup_db().await;
        let (chat_id, _, _) = generate_chat_id();

        let pool_clone1 = pool.clone();
        let pool_clone2 = pool.clone();
        let cid_clone1 = chat_id.clone();
        let cid_clone2 = chat_id.clone();

        let handle1 = tokio::spawn(async move {
            ChatRepository::new(pool_clone1, cid_clone1).await
        });

        let handle2 = tokio::spawn(async move {
            ChatRepository::new(pool_clone2, cid_clone2).await
        });

        let (res1, res2) = tokio::join!(handle1, handle2);

        let repo1 = res1.unwrap();
        let repo2 = res2.unwrap();

        assert!(repo1.is_ok());
        assert!(repo2.is_ok());

        Ok(())
    }

    #[tokio::test]
    #[traced_test]
    async fn test_two_peers_sync() -> Result<(), LibError> {
        let pool1 = setup_db().await;
        let pool2 = setup_db().await;

        let k1 = SecretKey::generate(&mut rand::rng());
        let k2 = SecretKey::generate(&mut rand::rng());
        let p1_id = k1.public();
        let p2_id = k2.public();

        let chat_id_1 = ChatId {
            my_id: p1_id,
            remote_id: p2_id,
        };
        let chat_id_2 = ChatId {
            my_id: p2_id,
            remote_id: p1_id,
        };

        let mut repo1 = ChatRepository::new(pool1, chat_id_1).await?;
        let mut repo2 = ChatRepository::new(pool2, chat_id_2).await?;

        let (_, sync_from_repo1) = repo1
            .add_message(ChatMessageContent::Text("Hello from P1".into()))
            .await?;

        sync_repos(&mut repo1, &mut repo2, sync_from_repo1).await?;

        let msgs2 = repo2.get_all_messages()?;
        assert_eq!(msgs2.len(), 1, "Repo 2 had to receive the message");

        let content = &msgs2[0].content;
        let ChatMessageContent::Text(t) = content;
        assert_eq!(t, "Hello from P1");

        Ok(())
    }

    #[tokio::test]
    #[traced_test]
    async fn test_pagination_logic() -> Result<(), LibError> {
        let pool = setup_db().await;
        let (chat_id, _, _) = generate_chat_id();
        let mut repo = ChatRepository::new(pool, chat_id).await?;

        for i in 0..305 {
            repo.add_message(ChatMessageContent::Text(format!("Msg {}", i)))
                .await?;
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }

        let latest = repo.get_messages(0, 1)?;
        assert_eq!(latest.len(), 1);
        let ChatMessageContent::Text(t) = &latest[0].content;
        assert_eq!(t, "Msg 304");

        let prev = repo.get_messages(1, 5)?;
        assert_eq!(prev.len(), 5);

        let ChatMessageContent::Text(t_first) = &prev[0].content;
        assert_eq!(t_first, "Msg 299");

        let ChatMessageContent::Text(t_last) = &prev[4].content;
        assert_eq!(t_last, "Msg 303");

        let page = repo.get_messages(0, 100)?;
        assert_eq!(page.len(), 100);

        let ChatMessageContent::Text(t_oldest_in_page) = &page[0].content;
        assert_eq!(t_oldest_in_page, "Msg 205");

        let ChatMessageContent::Text(t_newest_in_page) = &page[99].content;
        assert_eq!(t_newest_in_page, "Msg 304");

        let oldest = repo.get_messages(300, 10)?;
        assert_eq!(oldest.len(), 5);

        let ChatMessageContent::Text(t_genesis) = &oldest[0].content;
        assert_eq!(t_genesis, "Msg 0");

        Ok(())
    }
}
