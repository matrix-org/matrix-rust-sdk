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

#![doc = include_str!("../README.md")]
#![cfg_attr(docsrs, feature(doc_auto_cfg))]
#![warn(missing_docs)]
#![deny(
    missing_debug_implementations,
    trivial_casts,
    trivial_numeric_casts,
    unused_extern_crates,
    unused_import_braces,
    unused_qualifications
)]

#[cfg(all(feature = "sled_state_store", feature = "indexeddb_state_store"))]
compile_error!("sled_state_store and indexeddb_state_store are mutually exclusive and cannot be enabled together");

#[cfg(all(feature = "indexeddb_state_store", not(target_arch = "wasm32")))]
compile_error!("indexeddb_state_store only works for wasm32 target");

#[cfg(all(feature = "sled_cryptostore", feature = "indexeddb_state_store"))]
compile_error!("sled_cryptostore and indexeddb_state_store are mutually exclusive and cannot be enabled together");

pub use matrix_sdk_common::*;

pub use crate::{
    error::{Error, Result},
    session::Session,
    timeline_stream::TimelineStreamError,
};

mod client;
mod error;
pub mod media;
mod rooms;
mod session;
pub mod store;
mod timeline_stream;

#[cfg(feature = "testing")]
pub use http;

pub use client::{BaseClient, BaseClientConfig};
#[cfg(feature = "encryption")]
pub use matrix_sdk_crypto as crypto;
pub use rooms::{Room, RoomInfo, RoomMember, RoomType};
pub use store::{StateChanges, StateStore, Store, StoreError};
