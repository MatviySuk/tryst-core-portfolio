use crate::error::LibError;
use anyhow::anyhow;
use automerge::{ObjId, ObjType, ReadDoc, transaction::Transactable};

pub(super) mod keys {
    pub const LIST_PREFIX: &str = "msgs_";
    pub const ID: &str = "id";
    pub const SENDER: &str = "sender_node_id";
    pub const CONTENT: &str = "content";
    pub const CONTENT_TEXT: &str = "Text";
    pub const CREATED_AT: &str = "created_at";
    pub const UPDATED_AT: &str = "updated_at";
}

#[derive(Clone, Copy)]
pub struct Root;

pub struct MessageListId(ObjId);

impl Root {
    fn peer_list_key(peer_id: &str) -> String {
        format!("{}{}", keys::LIST_PREFIX, peer_id)
    }

    pub fn ensure_peer_list<T: Transactable>(
        &self,
        tx: &mut T,
        peer_id: &str,
    ) -> Result<MessageListId, LibError> {
        let key = Self::peer_list_key(peer_id);

        let list_id = match tx
            .get(automerge::ROOT, &key)
            .map_err(|e| LibError::Automerge(anyhow!(e)))?
        {
            Some((_, id)) => id,
            None => tx
                .put_object(automerge::ROOT, &key, ObjType::List)
                .map_err(|e| LibError::Automerge(anyhow!(e)))?,
        };
        Ok(MessageListId(list_id))
    }

    pub fn get_all_peer_lists<T: ReadDoc>(
        &self,
        doc: &T,
    ) -> Result<Vec<MessageListId>, LibError> {
        let mut lists = Vec::new();

        for key in doc.keys(automerge::ROOT) {
            if key.starts_with(keys::LIST_PREFIX)
                && let Some((_, id)) = doc
                    .get(automerge::ROOT, &key)
                    .map_err(|e| LibError::Automerge(anyhow!(e)))?
            {
                lists.push(MessageListId(id));
            }
        }

        Ok(lists)
    }

    pub fn get_peer_list<T: ReadDoc>(
        &self,
        doc: &T,
        peer_id: &str,
    ) -> Result<Option<MessageListId>, LibError> {
        let key = Self::peer_list_key(peer_id);
        match doc
            .get(automerge::ROOT, &key)
            .map_err(|e| LibError::Automerge(anyhow!(e)))?
        {
            Some((_, id)) => Ok(Some(MessageListId(id))),
            None => Ok(None),
        }
    }
}

impl MessageListId {
    pub fn inner(&self) -> &ObjId {
        &self.0
    }

    pub fn len<T: ReadDoc>(&self, doc: &T) -> usize {
        doc.length(&self.0)
    }

    pub fn append_message<T: Transactable>(
        &self,
        tx: &mut T,
        msg: &super::ChatMessage,
    ) -> Result<(), LibError> {
        let idx = tx.length(&self.0);
        let msg_id = tx
            .insert_object(&self.0, idx, ObjType::Map)
            .map_err(|e| LibError::Automerge(anyhow!(e)))?;

        tx.put(&msg_id, keys::ID, &msg.id)
            .map_err(|e| LibError::Automerge(anyhow!(e)))?;
        tx.put(&msg_id, keys::SENDER, &msg.sender_node_id)
            .map_err(|e| LibError::Automerge(anyhow!(e)))?;
        tx.put(&msg_id, keys::CREATED_AT, msg.created_at)
            .map_err(|e| LibError::Automerge(anyhow!(e)))?;
        tx.put(&msg_id, keys::UPDATED_AT, msg.updated_at)
            .map_err(|e| LibError::Automerge(anyhow!(e)))?;

        let content_id = tx
            .put_object(&msg_id, keys::CONTENT, ObjType::Map)
            .map_err(|e| LibError::Automerge(anyhow!(e)))?;

        match &msg.content {
            super::ChatMessageContent::Text(text_val) => {
                tx.put(&content_id, keys::CONTENT_TEXT, text_val)
                    .map_err(|e| LibError::Automerge(anyhow!(e)))?;
            }
        }

        Ok(())
    }
}
