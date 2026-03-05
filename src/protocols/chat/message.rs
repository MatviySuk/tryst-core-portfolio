use bytes::Bytes;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub enum ProtoMessage {
    Sync(Option<SyncMessage>),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SyncMessage(Bytes);

impl TryFrom<SyncMessage> for automerge::sync::Message {
    type Error = automerge::sync::ReadMessageError;

    fn try_from(value: SyncMessage) -> Result<Self, Self::Error> {
        Self::decode(&value.0)
    }
}

impl From<automerge::sync::Message> for SyncMessage {
    fn from(value: automerge::sync::Message) -> SyncMessage {
        let bytes = value.encode().into();

        SyncMessage(bytes)
    }
}
