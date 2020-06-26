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

pub use matrix_sdk_base::Error as BaseError;
#[cfg(not(target_arch = "wasm32"))]
pub use matrix_sdk_base::JsonStore;
pub use matrix_sdk_base::{CustomOrRawEvent, EventEmitter, Room, Session, SyncRoom};
pub use matrix_sdk_base::{RoomState, StateStore};
pub use matrix_sdk_common::*;
pub use reqwest::header::InvalidHeaderValue;

#[cfg(feature = "encryption")]
pub use matrix_sdk_base::{Device, TrustState};

mod client;
mod error;
mod request_builder;
pub use client::{Client, ClientConfig, SyncSettings};
pub use error::{Error, Result};
pub use request_builder::{
    MessagesRequestBuilder, RegistrationBuilder, RoomBuilder, RoomListFilterBuilder,
};

#[cfg(not(target_arch = "wasm32"))]
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");
