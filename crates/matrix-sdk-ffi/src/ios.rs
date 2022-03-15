pub mod client;
pub mod room;
pub mod messages;
pub mod backward_stream;

use std::{fs, path};
use anyhow::Result;
use sanitize_filename_reader_friendly::sanitize;

use matrix_sdk::{
    Client as MatrixClient,
    config::ClientConfig,
    Session,
};
pub use matrix_sdk::ruma::{
        api::client::r0::{
            account::register,
        },
        UserId,
    };
use lazy_static::lazy_static;
use tokio::runtime;
use url::Url;
use derive_builder::Builder;
use std::sync::Arc;
use serde::{Serialize, Deserialize};
use client::Client;

lazy_static! {
    pub static ref RUNTIME: runtime::Runtime =
        runtime::Runtime::new().expect("Can't start Tokio runtime");
}

pub fn guest_client(base_path: String, homeurl: String) -> Result<Arc<Client>> {
    let homeserver = Url::parse(&homeurl)?;
    let config = new_client_config(base_path, homeurl)?;
    let mut guest_registration = register::Request::new();
    guest_registration.kind = register::RegistrationKind::Guest;
    RUNTIME.block_on(async move {
        let client = MatrixClient::new_with_config(homeserver, config).await?;
        let register = client.register(guest_registration).await?;
        let session = Session {
            access_token: register.access_token.expect("no access token given"),
            user_id: register.user_id,
            device_id: register.device_id.clone().expect("device id is given by server"),
        };
        client.restore_login(session).await?;
        let c = Client::new(client, ClientStateBuilder::default().is_guest(true).build()?);
        Ok(Arc::new(c))
    })
}

pub fn login_with_token(base_path: String, restore_token: String) -> Result<Arc<Client>> {
    let RestoreToken { session, homeurl, is_guest } = serde_json::from_str(&restore_token)?;
    let homeserver = Url::parse(&homeurl)?;
    let config = new_client_config(base_path, session.user_id.to_string())?;
    // First we need to log in.
    RUNTIME.block_on(async move {
        let client = MatrixClient::new_with_config(homeserver, config).await?;
        client.restore_login(session).await?;
        let c = Client::new(client, ClientStateBuilder::default().is_guest(is_guest).build()?);
        Ok(Arc::new(c))
    })
}


pub fn login_new_client(base_path: String, username: String, password: String) -> Result<Arc<Client>> {
    let config = new_client_config(base_path, username.clone())?;
    let user = Box::<UserId>::try_from(username)?;
    // First we need to log in.
    RUNTIME.block_on(async move {
        let client = MatrixClient::new_from_user_id_with_config(&user, config).await?;
        client.login(user, &password, None, None).await?;
        let c = Client::new(client, ClientStateBuilder::default().is_guest(false).build()?);
        Ok(Arc::new(c))
    })
}

fn new_client_config(base_path: String, home: String) -> Result<ClientConfig> {
    let data_path = path::PathBuf::from(base_path)
        .join(sanitize(&home));

    fs::create_dir_all(&data_path)?;

    let config = ClientConfig::new()
        .user_agent("rust-sdk-ios")?
        .store_path(&data_path);
    return Ok(config);
}

#[derive(Default, Builder, Debug)]
pub struct ClientState {
    #[builder(default)]
    is_guest: bool,
    #[builder(default)]
    has_first_synced: bool,
    #[builder(default)]
    is_syncing: bool,
    #[builder(default)]
    should_stop_syncing: bool,
}

#[derive(Serialize, Deserialize)]
struct RestoreToken {
    is_guest: bool,
    homeurl: String,
    session: Session,
}

#[derive(thiserror::Error, Debug)]
pub enum ClientError {

    #[error("client error: {msg}")]
    Generic {
        msg: String,
    }
}

impl From<anyhow::Error> for ClientError {
    fn from(e: anyhow::Error) -> ClientError {
        ClientError::Generic { msg: e.to_string() }
    }
}
