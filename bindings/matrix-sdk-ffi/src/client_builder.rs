use std::{fs, path::PathBuf, sync::Arc};

use anyhow::anyhow;
use matrix_sdk::{
    config::StoreConfig,
    ruma::{
        api::{error::UnknownVersionError, MatrixVersion},
        ServerName, UserId,
    },
    Client as MatrixClient, ClientBuilder as MatrixClientBuilder,
};
use sanitize_filename_reader_friendly::sanitize;
use zeroize::Zeroizing;

use super::{client::Client, RUNTIME};
use crate::{error::ClientError, helpers::unwrap_or_clone_arc};

#[derive(Clone)]
pub struct ClientBuilder {
    base_path: Option<String>,
    username: Option<String>,
    server_name: Option<String>,
    homeserver_url: Option<String>,
    server_versions: Option<Vec<String>>,
    passphrase: Zeroizing<Option<String>>,
    user_agent: Option<String>,
    sliding_sync_proxy: Option<String>,
    inner: MatrixClientBuilder,
}

impl ClientBuilder {
    pub fn new() -> Self {
        Self {
            base_path: None,
            username: None,
            server_name: None,
            homeserver_url: None,
            server_versions: None,
            passphrase: Zeroizing::new(None),
            user_agent: None,
            sliding_sync_proxy: None,
            inner: MatrixClient::builder(),
        }
    }
}

#[uniffi::export]
impl ClientBuilder {
    pub fn base_path(self: Arc<Self>, path: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.base_path = Some(path);
        Arc::new(builder)
    }

    pub fn username(self: Arc<Self>, username: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.username = Some(username);
        Arc::new(builder)
    }

    pub fn server_versions(self: Arc<Self>, versions: Vec<String>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.server_versions = Some(versions);
        Arc::new(builder)
    }

    pub fn server_name(self: Arc<Self>, server_name: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.server_name = Some(server_name);
        Arc::new(builder)
    }

    pub fn homeserver_url(self: Arc<Self>, url: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.homeserver_url = Some(url);
        Arc::new(builder)
    }

    pub fn passphrase(self: Arc<Self>, passphrase: Option<String>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.passphrase = Zeroizing::new(passphrase);
        Arc::new(builder)
    }

    pub fn user_agent(self: Arc<Self>, user_agent: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.user_agent = Some(user_agent);
        Arc::new(builder)
    }

    pub fn sliding_sync_proxy(self: Arc<Self>, sliding_sync_proxy: Option<String>) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.sliding_sync_proxy = sliding_sync_proxy;
        Arc::new(builder)
    }

    pub fn build(self: Arc<Self>) -> Result<Arc<Client>, ClientError> {
        Ok(self.build_inner()?)
    }
}

impl ClientBuilder {
    pub(crate) fn build_inner(self: Arc<Self>) -> anyhow::Result<Arc<Client>> {
        let builder = unwrap_or_clone_arc(self);
        let mut inner_builder = builder.inner;

        if let (Some(base_path), Some(username)) = (builder.base_path, &builder.username) {
            // Determine store path
            let data_path = PathBuf::from(base_path).join(sanitize(username));
            fs::create_dir_all(&data_path)?;

            let mut state_store =
                matrix_sdk_sled::SledStateStore::builder().path(data_path.to_owned());

            if let Some(passphrase) = builder.passphrase.as_deref() {
                state_store = state_store.passphrase(passphrase.to_owned());
            }

            let state_store = state_store.build()?;

            let crypto_store = RUNTIME.block_on(matrix_sdk_sqlite::SqliteCryptoStore::open(
                &data_path,
                builder.passphrase.as_deref(),
            ))?;

            let store_config =
                StoreConfig::new().state_store(state_store).crypto_store(crypto_store);

            inner_builder = inner_builder.store_config(store_config)
        }

        // Determine server either from URL, server name or user ID.
        if let Some(homeserver_url) = builder.homeserver_url {
            inner_builder = inner_builder.homeserver_url(homeserver_url);
        } else if let Some(server_name) = builder.server_name {
            let server_name = ServerName::parse(server_name)?;
            inner_builder = inner_builder.server_name(&server_name);
        } else if let Some(username) = builder.username {
            let user = UserId::parse(username)?;
            inner_builder = inner_builder.server_name(user.server_name());
        } else {
            return Err(anyhow!(
                "Failed to build: One of homeserver_url, server_name or username must be called."
            ));
        }

        if let Some(user_agent) = builder.user_agent {
            inner_builder = inner_builder.user_agent(user_agent);
        }

        if let Some(server_versions) = builder.server_versions {
            inner_builder = inner_builder.server_versions(
                server_versions
                    .iter()
                    .map(|s| MatrixVersion::try_from(s.as_str()))
                    .collect::<Result<Vec<MatrixVersion>, UnknownVersionError>>()?,
            );
        }

        RUNTIME.block_on(async move {
            let client = inner_builder.build().await?;
            let c = Client::new(client);
            c.set_sliding_sync_proxy(builder.sliding_sync_proxy);
            Ok(Arc::new(c))
        })
    }
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}
