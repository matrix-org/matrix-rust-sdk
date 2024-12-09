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
#![cfg_attr(target_arch = "wasm32", allow(clippy::arc_with_non_send_sync))]
#![warn(missing_docs, missing_debug_implementations)]

pub use matrix_sdk_common::*;
use ruma::{OwnedDeviceId, OwnedUserId};
use serde::{Deserialize, Serialize};

pub use crate::error::{Error, Result};

mod client;
pub mod debug;
pub mod deserialized_responses;
mod error;
pub mod event_cache;
pub mod latest_event;
pub mod media;
pub mod notification_settings;
mod response_processors;
mod rooms;

pub mod read_receipts;
pub use read_receipts::PreviousEventsProvider;
#[cfg(feature = "experimental-sliding-sync")]
pub mod sliding_sync;

pub mod store;
pub mod sync;
#[cfg(any(test, feature = "testing"))]
mod test_utils;
mod utils;

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

pub use client::BaseClient;
#[cfg(any(test, feature = "testing"))]
pub use http;
#[cfg(feature = "e2e-encryption")]
pub use matrix_sdk_crypto as crypto;
pub use once_cell;
pub use rooms::{
    Room, RoomCreateWithCreatorEventContent, RoomDisplayName, RoomHero, RoomInfo,
    RoomInfoNotableUpdate, RoomInfoNotableUpdateReasons, RoomMember, RoomMemberships, RoomState,
    RoomStateFilter,
};
pub use store::{
    ComposerDraft, ComposerDraftType, QueueWedgeError, StateChanges, StateStore, StateStoreDataKey,
    StateStoreDataValue, StoreError,
};
pub use utils::{
    MinimalRoomMemberEvent, MinimalStateEvent, OriginalMinimalStateEvent, RedactedMinimalStateEvent,
};

#[cfg(test)]
matrix_sdk_test::init_tracing_for_tests!();

/// The Matrix user session info.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// The ID of the session's user.
    pub user_id: OwnedUserId,
    /// The ID of the client device.
    pub device_id: OwnedDeviceId,
}
