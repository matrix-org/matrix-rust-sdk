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
#![warn(missing_docs, missing_debug_implementations)]

pub use matrix_sdk_common::*;

pub use crate::{
    error::{Error, Result},
    session::{Session, SessionMeta, SessionTokens},
};

mod client;
pub mod debug;
pub mod deserialized_responses;
mod error;
pub mod media;
mod rooms;
mod session;
#[cfg(feature = "experimental-sliding-sync")]
mod sliding_sync;
pub mod store;
pub mod sync;
mod utils;

pub use client::BaseClient;
#[cfg(any(test, feature = "testing"))]
pub use http;
#[cfg(feature = "e2e-encryption")]
pub use matrix_sdk_crypto as crypto;
pub use once_cell;
pub use rooms::{
    DisplayName, Room, RoomInfo, RoomMember, RoomMemberships, RoomState, RoomStateFilter,
};
pub use store::{StateChanges, StateStore, StateStoreDataKey, StateStoreDataValue, StoreError};
pub use utils::{
    MinimalRoomMemberEvent, MinimalStateEvent, OriginalMinimalStateEvent, RedactedMinimalStateEvent,
};

#[cfg(all(test, not(target_arch = "wasm32")))]
#[ctor::ctor]
fn init_logging() {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .init();
}
