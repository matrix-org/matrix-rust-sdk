// TODO: target-os conditional would be good.

#![allow(unused_qualifications)]

pub mod authentication_service;
pub mod backward_stream;
pub mod client;
pub mod client_builder;
mod helpers;
pub mod messages;
pub mod room;
pub mod session_verification;
pub mod sliding_sync;
mod uniffi_api;

use client::Client;
use client_builder::ClientBuilder;
use matrix_sdk::Session;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};
pub use uniffi_api::*;

pub static RUNTIME: Lazy<Runtime> =
    Lazy::new(|| Runtime::new().expect("Can't start Tokio runtime"));

pub use matrix_sdk::ruma::{api::client::account::register, UserId};

pub use self::{
    authentication_service::*, backward_stream::*, client::*, messages::*, room::*,
    session_verification::*, sliding_sync::*,
};

#[derive(Default, Debug)]
pub struct ClientState {
    is_guest: bool,
    has_first_synced: bool,
    is_syncing: bool,
    should_stop_syncing: bool,
    is_soft_logout: bool,
}

#[derive(Serialize, Deserialize)]
struct RestoreToken {
    is_guest: bool,
    homeurl: String,
    session: Session,
    #[serde(default)]
    is_soft_logout: bool,
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

#[uniffi::export]
fn setup_tracing(configuration: String) {
    tracing_subscriber::registry()
        .with(EnvFilter::new(configuration))
        .with(fmt::layer().with_ansi(false))
        .init();
}

mod uniffi_types {
    pub use matrix_sdk::ruma::events::room::{message::RoomMessageEventContent, MediaSource};
}
