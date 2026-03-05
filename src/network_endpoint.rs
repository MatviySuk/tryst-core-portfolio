use crate::error::LibError;
use crate::protocols::chat::protocol_handler::{ALPN as CHAT_ALPN, ChatProtocol};
use anyhow::Context;
use iroh::SecretKey;
use std::fmt::Debug;
use tracing::instrument;

#[derive(Clone, Debug)]
pub struct NetworkEndpoint {
    pub(crate) inner: Inner,
}

#[derive(Clone, Debug)]
pub(crate) struct Inner {
    pub(crate) endpoint: iroh::Endpoint,
    pub(crate) router: iroh::protocol::Router,
    pub(crate) chat: ChatProtocol,
}

impl NetworkEndpoint {
    #[instrument(skip_all, err)]
    pub(crate) async fn start(
        secret_key: SecretKey,
        discovery: impl iroh::discovery::Discovery,
    ) -> Result<NetworkEndpoint, LibError> {
        let endpoint = iroh::endpoint::Builder::empty(iroh::RelayMode::Default)
            .secret_key(secret_key)
            .discovery(discovery)
            .bind()
            .await
            .context("Failed to bind endpoint connection with selected discovery methods.")?;

        let chat = ChatProtocol::new(endpoint.clone());

        let router = iroh::protocol::Router::builder(endpoint.clone())
            .accept(CHAT_ALPN, chat.clone())
            .spawn();

        Ok(NetworkEndpoint {
            inner: Inner {
                endpoint,
                router,
                chat,
            },
        })
    }
}
