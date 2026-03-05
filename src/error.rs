use thiserror::Error;

#[derive(Error, Debug)]
pub enum LibError {
    #[error("SQLx database error: {0}")]
    Sqlx(#[from] sqlx::Error),

    #[error("SQLx database migrations error: {0}")]
    SqlxMigrations(#[from] sqlx::migrate::MigrateError),

    #[error("iroh Connection error: {0}")]
    PeerConnection(#[from] iroh::endpoint::ConnectError),

    #[error("Automerge/Autosurgeon error: {0}")]
    Automerge(anyhow::Error),

    #[error("Chat not found for ID: {0}")]
    ChatNotFound(String),

    #[error("Invalid Peer ID for Chat ID generation: {0}")]
    InvalidPeerId(#[from] iroh::KeyParsingError),

    #[error("Failed to startup network endpoint. Check internet connection.")]
    EndpointStartup,

    #[error("Mutex lock poisoned")]
    LockPoisoned,

    #[error("Unexpected error: {0}")]
    Unexpected(#[from] anyhow::Error),
}

impl<T> From<std::sync::PoisonError<T>> for LibError {
    fn from(_: std::sync::PoisonError<T>) -> Self {
        LibError::LockPoisoned
    }
}

#[derive(uniffi::Error, Error, Debug, Clone)]
pub enum GossipError {
    #[error("Storage failure: {0}")]
    Storage(String),
    #[error("Network connectivity issue: {0}")]
    Connectivity(String),
    #[error("P2P protocol error: {0}")]
    Protocol(String),
    #[error("Internal system error: {0}")]
    Internal(String),
}

impl GossipError {
    #[uniffi::method]
    pub fn message(&self) -> String {
        self.to_string()
    }
}

impl From<LibError> for GossipError {
    fn from(err: LibError) -> Self {
        match err {
            LibError::Sqlx(e) => Self::Storage(e.to_string()),
            LibError::SqlxMigrations(e) => Self::Storage(e.to_string()),
            LibError::PeerConnection(e) => Self::Connectivity(e.to_string()),
            LibError::EndpointStartup => {
                Self::Connectivity("Failed to startup network endpoint.".to_string())
            }
            LibError::Automerge(e) => Self::Protocol(e.to_string()),
            LibError::ChatNotFound(id) => {
                Self::Protocol(format!("Chat not found for ID: {id}"))
            }
            LibError::InvalidPeerId(e) => Self::Protocol(e.to_string()),
            LibError::LockPoisoned => {
                Self::Internal("Internal lock poisoned".to_string())
            }
            LibError::Unexpected(e) => Self::Internal(e.to_string()),
        }
    }
}

impl From<anyhow::Error> for GossipError {
    fn from(value: anyhow::Error) -> Self {
        Self::Internal(value.to_string())
    }
}
