use std::{fs, path::PathBuf, sync::Arc};

use anyhow::Context;
use matrix_sdk::{
    ruma::UserId, store::make_store_config, Client as MatrixClient,
    ClientBuilder as MatrixClientBuilder,
};
use sanitize_filename_reader_friendly::sanitize;

use super::{client::Client, ClientState, RUNTIME};

#[derive(Clone)]
pub struct ClientBuilder {
    base_path: Option<String>,
    username: Option<String>,
    homeserver_url: Option<String>,
    inner: MatrixClientBuilder,
}

impl ClientBuilder {
    pub fn new() -> Self {
        Self {
            base_path: None,
            username: None,
            homeserver_url: None,
            inner: MatrixClient::builder().user_agent("rust-sdk-ios"),
        }
    }

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

    pub fn homeserver_url(self: Arc<Self>, url: String) -> Arc<Self> {
        let mut builder = unwrap_or_clone_arc(self);
        builder.homeserver_url = Some(url);
        Arc::new(builder)
    }

    pub fn build(self: Arc<Self>) -> anyhow::Result<Arc<Client>> {
        let builder = unwrap_or_clone_arc(self);

        let base_path = builder.base_path.context("Base path was not set")?;
        let username = builder
            .username
            .context("Username to determine homeserver and home path was not set")?;

        // Determine store path
        let data_path = PathBuf::from(base_path).join(sanitize(&username));
        fs::create_dir_all(&data_path)?;
        let store_config = make_store_config(&data_path, None)?;

        let mut inner_builder = builder.inner.store_config(store_config);

        // Determine server either from explicitly set homeserver or from userId
        if let Some(homeserver_url) = builder.homeserver_url {
            inner_builder = inner_builder.homeserver_url(homeserver_url);
        } else {
            let user = UserId::parse(username)?;
            inner_builder = inner_builder.server_name(user.server_name());
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

fn unwrap_or_clone_arc<T: Clone>(arc: Arc<T>) -> T {
    Arc::try_unwrap(arc).unwrap_or_else(|x| (*x).clone())
}
