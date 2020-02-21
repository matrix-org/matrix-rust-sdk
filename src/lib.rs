//! This crate implements a [Matrix](https://matrix.org/) client library.
#![deny(missing_docs)]

pub use crate::{error::Error, session::Session};
pub use reqwest::header::InvalidHeaderValue;
pub use ruma_client_api as api;
pub use ruma_events as events;
pub use ruma_identifiers as identifiers;

mod async_client;
mod base_client;
mod error;
mod session;

pub use async_client::{AsyncClient, AsyncClientConfig, SyncSettings};
pub use base_client::{Client, Room};

pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");
