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
//! # Enabling logging
//!
//! Users of the matrix-sdk crate can enable log output by depending on the `tracing-subscriber`
//! crate and including the following line in their application (e.g. at the start of `main`):
//!
//! ```rust
//! tracing_subscriber::fmt::init();
//! ```
//!
//! The log output is controlled via the `RUST_LOG` environment variable by setting it to one of
//! the `error`, `warn`, `info`, `debug` or `trace` levels. The output is printed to stdout.
//!
//! The `RUST_LOG` variable also supports a more advanced syntax for filtering log output more
//! precisely, for instance with crate-level granularity. For more information on this, check out
//! the [tracing_subscriber
//! documentation](https://tracing.rs/tracing_subscriber/filter/struct.envfilter).
//!
//! # Crate Feature Flags
//!
//! The following crate feature flags are available:
//!
//! * `encryption`: Enables end-to-end encryption support in the library.
//! * `sled_cryptostore`: Enables a Sled based store for the encryption
//! keys. If this is disabled and `encryption` support is enabled the keys will
//! by default be stored only in memory and thus lost after the client is
//! destroyed.
//! * `markdown`: Support for sending markdown formatted messages.
//! * `socks`: Enables SOCKS support in reqwest, the default HTTP client.
//! * `sso_login`: Enables SSO login with a local http server.

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
#![cfg_attr(feature = "docs", feature(doc_cfg))]

#[cfg(not(any(feature = "native-tls", feature = "rustls-tls",)))]
compile_error!("one of 'native-tls' or 'rustls-tls' features must be enabled");

#[cfg(all(feature = "native-tls", feature = "rustls-tls",))]
compile_error!("only one of 'native-tls' or 'rustls-tls' features can be enabled");

#[cfg(all(feature = "sso_login", target_arch = "wasm32"))]
compile_error!("'sso_login' cannot be enabled on 'wasm32' arch");

#[cfg(feature = "encryption")]
#[cfg_attr(feature = "docs", doc(cfg(encryption)))]
pub use matrix_sdk_base::crypto::{EncryptionInfo, LocalTrust};
pub use matrix_sdk_base::{
    Error as BaseError, Room as BaseRoom, RoomInfo, RoomMember as BaseRoomMember, RoomType,
    Session, StateChanges, StoreError,
};

pub use bytes::Bytes;
pub use matrix_sdk_common::*;
pub use reqwest;

mod client;
mod error;
mod event_handler;
mod http_client;
/// High-level room API
pub mod room;
/// High-level room API
mod room_member;

#[cfg(feature = "encryption")]
mod device;
#[cfg(feature = "encryption")]
mod sas;
#[cfg(feature = "encryption")]
mod verification_request;

pub use client::{Client, ClientConfig, LoopCtrl, RequestConfig, SyncSettings};
#[cfg(feature = "encryption")]
#[cfg_attr(feature = "docs", doc(cfg(encryption)))]
pub use device::Device;
pub use error::{Error, HttpError, Result};
pub use event_handler::{CustomEvent, EventHandler};
pub use http_client::HttpSend;
pub use room_member::RoomMember;
#[cfg(feature = "encryption")]
#[cfg_attr(feature = "docs", doc(cfg(encryption)))]
pub use sas::Sas;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");
