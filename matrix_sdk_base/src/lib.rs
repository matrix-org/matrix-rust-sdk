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
//! * `sled_cryptostore`: Enables a Sled based store for the encryption
//! keys. If this is disabled and `encryption` support is enabled the keys will
//! by default be stored only in memory and thus lost after the client is
//! destroyed.
//! * `markdown`: Support for sending markdown formatted messages.
#![deny(
    missing_debug_implementations,
    missing_docs,
    trivial_casts,
    trivial_numeric_casts,
    unused_extern_crates,
    unused_import_braces,
    unused_qualifications
)]
#![cfg_attr(feature = "docs", feature(doc_cfg))]

pub use crate::{
    error::{Error, Result},
    session::Session,
};
pub use matrix_sdk_common::*;

mod client;
mod error;
mod rooms;
mod session;
mod store;

pub use rooms::{Room, RoomInfo, RoomMember, RoomType};
pub use store::{StateChanges, StateStore, Store, StoreError};

pub use client::{BaseClient, BaseClientConfig};

#[cfg(feature = "encryption")]
#[cfg_attr(feature = "docs", doc(cfg(encryption)))]
pub use matrix_sdk_crypto as crypto;
