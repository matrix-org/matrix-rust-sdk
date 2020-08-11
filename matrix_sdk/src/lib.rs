// Copyright 2020 Damir Jelić
// Copyright 2020 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! This crate implements a [Matrix](https://matrix.org/) client library.
//!
//! ##  Crate Feature Flags
//!
//! The following crate feature flags are available:
//!
//! * `encryption`: Enables end-to-end encryption support in the library.
//! * `sqlite-cryptostore`: Enables a SQLite based store for the encryption
//! keys. If this is disabled and `encryption` support is enabled the keys will
//! by default be stored only in memory and thus lost after the client is
//! destroyed.

#![deny(
    missing_debug_implementations,
    dead_code,
    missing_docs,
    trivial_casts,
    trivial_numeric_casts,
    unused_extern_crates,
    unused_import_braces,
    unused_qualifications
)]

#[cfg(not(target_arch = "wasm32"))]
pub use matrix_sdk_base::JsonStore;
pub use matrix_sdk_base::{
    CustomEvent, Error as BaseError, EventEmitter, Room, RoomState, Session, StateStore, SyncRoom,
};
#[cfg(feature = "encryption")]
pub use matrix_sdk_base::{Device, TrustState};
#[cfg(feature = "messages")]
pub use matrix_sdk_base::{MessageQueue, PossiblyRedactedExt};
pub use matrix_sdk_common::*;
pub use reqwest::header::InvalidHeaderValue;

mod client;
mod error;
mod http_client;
mod request_builder;

#[cfg(feature = "encryption")]
mod sas;

pub use client::{Client, ClientConfig, SyncSettings};
pub use error::{Error, Result};
pub use http_client::HttpSend;
pub use request_builder::{
    MessagesRequestBuilder, RegistrationBuilder, RoomBuilder, RoomListFilterBuilder,
};
#[cfg(feature = "encryption")]
pub use sas::Sas;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");
