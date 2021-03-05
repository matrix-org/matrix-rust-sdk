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
//! * `unstable-synapse-quirks`: Enables support to deal with inconsistencies
//! of Synapse in compliance with the Matrix API specification.
//! * `markdown`: Support for sending markdown formatted messages.
//! * `socks`: Enables SOCKS support in reqwest, the default HTTP client.

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

#[cfg(feature = "encryption")]
#[cfg_attr(feature = "docs", doc(cfg(encryption)))]
pub use matrix_sdk_base::crypto::{EncryptionInfo, LocalTrust};
pub use matrix_sdk_base::{
    CustomEvent, Error as BaseError, EventHandler, Room, RoomInfo, RoomMember, RoomType, Session,
    StateChanges, StoreError,
};

pub use matrix_sdk_common::*;
pub use reqwest;

mod client;
mod error;
mod http_client;
/// High-level room API
pub mod room;

#[cfg(feature = "encryption")]
mod device;
#[cfg(feature = "encryption")]
mod sas;
#[cfg(feature = "encryption")]
mod verification_request;

pub use client::{Client, ClientConfig, LoopCtrl, SyncSettings};
#[cfg(feature = "encryption")]
#[cfg_attr(feature = "docs", doc(cfg(encryption)))]
pub use device::Device;
pub use error::{Error, HttpError, Result};
pub use http_client::HttpSend;
#[cfg(feature = "encryption")]
#[cfg_attr(feature = "docs", doc(cfg(encryption)))]
pub use sas::Sas;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");
