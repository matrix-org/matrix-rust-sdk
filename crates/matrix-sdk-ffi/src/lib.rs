// TODO: target-os conditional would be good.

#![allow(unused_qualifications)]

pub mod backward_stream;
pub mod client;
pub mod messages;
pub mod room;

use std::{fs, path, sync::Arc};

use client::Client;
use matrix_sdk::{store::make_store_config, Client as MatrixClient, ClientBuilder, Session};
use once_cell::sync::Lazy;
use sanitize_filename_reader_friendly::sanitize;
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;

pub static RUNTIME: Lazy<Runtime> =
    Lazy::new(|| Runtime::new().expect("Can't start Tokio runtime"));

pub use matrix_sdk::ruma::{api::client::account::register, UserId};

pub use self::{backward_stream::*, client::*, messages::*, room::*};

pub fn guest_client(base_path: String, homeurl: String) -> anyhow::Result<Arc<Client>> {
    let builder = new_client_builder(base_path, homeurl.clone())?.homeserver_url(&homeurl);
    let mut guest_registration = register::v3::Request::new();
    guest_registration.kind = register::RegistrationKind::Guest;
    RUNTIME.block_on(async move {
        let client = builder.build().await?;
        let register = client.register(guest_registration).await?;
        let session = Session {
            access_token: register.access_token.expect("no access token given"),
            user_id: register.user_id,
            device_id: register.device_id.clone().expect("device id is given by server"),
        };
        client.restore_login(session).await?;
        let c = Client::new(client, ClientState { is_guest: true, ..ClientState::default() });
        Ok(Arc::new(c))
    })
}

pub fn login_with_token(base_path: String, restore_token: String) -> anyhow::Result<Arc<Client>> {
    let RestoreToken { session, homeurl, is_guest } = serde_json::from_str(&restore_token)?;
    let builder = new_client_builder(base_path, session.user_id.to_string())?
        .homeserver_url(&homeurl)
        .user_id(&session.user_id);
    // First we need to log in.
    RUNTIME.block_on(async move {
        let client = builder.build().await?;
        client.restore_login(session).await?;
        let c = Client::new(client, ClientState { is_guest, ..ClientState::default() });
        Ok(Arc::new(c))
    })
}

pub fn login_new_client(
    base_path: String,
    username: String,
    password: String,
) -> anyhow::Result<Arc<Client>> {
    let builder = new_client_builder(base_path, username.clone())?;
    let user = Box::<UserId>::try_from(username)?;
    // First we need to log in.
    RUNTIME.block_on(async move {
        let client = builder.user_id(&user).build().await?;
        client.login(user, &password, None, None).await?;
        let c = Client::new(client, ClientState { is_guest: false, ..ClientState::default() });
        Ok(Arc::new(c))
    })
}

fn new_client_builder(base_path: String, home: String) -> anyhow::Result<ClientBuilder> {
    let data_path = path::PathBuf::from(base_path).join(sanitize(&home));

    fs::create_dir_all(&data_path)?;
    let store_config = make_store_config(&data_path, None)?;

    Ok(MatrixClient::builder().user_agent("rust-sdk-ios").store_config(store_config))
}

#[derive(Default, Debug)]
pub struct ClientState {
    is_guest: bool,
    has_first_synced: bool,
    is_syncing: bool,
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
    Generic { msg: String },
}

impl From<anyhow::Error> for ClientError {
    fn from(e: anyhow::Error) -> ClientError {
        ClientError::Generic { msg: e.to_string() }
    }
}

uniffi_macros::include_scaffolding!("api");
