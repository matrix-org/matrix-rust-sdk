//! Crate `nio-client` is a [Matrix](https://matrix.org/) client library.
//!
#![warn(missing_docs)]

pub use crate::{error::Error, session::Session};
pub use ruma_client_api as api;
pub use ruma_events as events;
pub use reqwest::header::InvalidHeaderValue;

mod async_client;
mod base_client;
mod error;
mod session;

pub use async_client::{AsyncClient, AsyncClientConfig, SyncSettings};
pub use base_client::Client;
