use std::{fs, path::PathBuf, sync::Arc};

use anyhow::anyhow;
use matrix_sdk::{
    ruma::{ServerName, UserId},
    Client as MatrixClient, ClientBuilder as MatrixClientBuilder,
};
use sanitize_filename_reader_friendly::sanitize;

use super::{client::Client, ClientState, RUNTIME};
use crate::helpers::unwrap_or_clone_arc;

#[derive(Clone)]
pub struct ClientBuilder {
    base_path: Option<String>,
    username: Option<String>,
    server_name: Option<String>,
    homeserver_url: Option<String>,
    user_agent: Option<String>,
    inner: MatrixClientBuilder,
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

    pub fn user_agent(self: Arc<Self>, user_agent: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.user_agent = Some(user_agent);
        Arc::new(builder)
    }
}

impl ClientBuilder {
    pub fn new() -> Self {
        Self {
            base_path: None,
            username: None,
            server_name: None,
            homeserver_url: None,
            user_agent: None,
            inner: MatrixClient::builder(),
        }
    }

    pub fn build(self: Arc<Self>) -> anyhow::Result<Arc<Client>> {
        let builder = unwrap_or_clone_arc(self);
        let mut inner_builder = builder.inner;

        if let (Some(base_path), Some(username)) = (builder.base_path, &builder.username) {
            // Determine store path
            let data_path = PathBuf::from(base_path).join(sanitize(username));
            fs::create_dir_all(&data_path)?;

            inner_builder = RUNTIME.block_on(inner_builder.sled_store(data_path, None))?;
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

        RUNTIME.block_on(async move {
            let client = inner_builder.build().await?;
            let c = Client::new(client, ClientState::default());
            Ok(Arc::new(c))
        })
    }
}

impl Default for ClientBuilder {
    fn default() -> Self {
        Self::new()
    }
}
