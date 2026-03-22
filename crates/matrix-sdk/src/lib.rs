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
#![warn(missing_debug_implementations, missing_docs)]
#![cfg_attr(target_family = "wasm", allow(clippy::arc_with_non_send_sync))]
#![cfg_attr(docsrs, feature(doc_cfg))]

pub use async_trait::async_trait;
pub use bytes;
pub use matrix_sdk_base::{
    ComposerDraft, ComposerDraftType, DraftAttachment, DraftAttachmentContent, DraftThumbnail,
    EncryptionState, PredecessorRoom, QueueWedgeError, Room as BaseRoom,
    RoomCreateWithCreatorEventContent, RoomDisplayName, RoomHero, RoomInfo,
    RoomMember as BaseRoomMember, RoomMemberships, RoomRecencyStamp, RoomState, SessionMeta,
    StateChanges, StateStore, StoreError, SuccessorRoom, ThreadingSupport, deserialized_responses,
    store::{self, DynStateStore, MemoryStore, StateStoreExt},
};
pub use matrix_sdk_common::*;
pub use reqwest;

mod account;
pub mod attachment;
pub mod authentication;
mod client;
pub mod config;
mod deduplicating_handler;
#[cfg(feature = "e2e-encryption")]
pub mod encryption;
mod error;
pub mod event_cache;
pub mod event_handler;
mod http_client;
pub mod latest_events;
pub mod media;
pub mod notification_settings;
pub mod paginators;
pub mod pusher;
pub mod room;
pub mod room_directory_search;
pub mod room_preview;
pub mod send_queue;
pub mod utils;
pub mod futures {
    //! Named futures returned from methods on types in [the crate root][crate].

    pub use super::client::futures::SendRequest;
}
pub mod sliding_sync;
pub mod sync;
#[cfg(feature = "experimental-widgets")]
pub mod widget;

pub use account::Account;
pub use authentication::{AuthApi, AuthSession, SessionTokens};
#[cfg(feature = "experimental-search")]
pub mod search_index;
pub use client::{
    Client, ClientBuildError, ClientBuilder, LoopCtrl, ServerVendorInfo, SessionChange, StoreSizes,
    sanitize_server_name,
};
pub use error::{
    Error, HttpError, HttpResult, NotificationSettingsError, RefreshTokenError, Result,
    RumaApiError,
};
pub use http_client::TransmissionProgress;
#[cfg(all(feature = "e2e-encryption", feature = "sqlite"))]
pub use matrix_sdk_sqlite::SqliteCryptoStore;
#[cfg(feature = "sqlite")]
pub use matrix_sdk_sqlite::{
    STATE_STORE_DATABASE_NAME, SqliteEventCacheStore, SqliteMediaStore, SqliteStateStore,
    SqliteStoreConfig,
};
pub use media::Media;
pub use pusher::Pusher;
pub use room::Room;
pub use ruma::{IdParseError, OwnedServerName, ServerName};
pub use sliding_sync::{
    SlidingSync, SlidingSyncBuilder, SlidingSyncList, SlidingSyncListBuilder,
    SlidingSyncListLoadingState, SlidingSyncMode, UpdateSummary,
};

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

pub mod live_location_share;
#[cfg(any(test, feature = "testing"))]
pub mod test_utils;

#[cfg(test)]
matrix_sdk_test_utils::init_tracing_for_tests!();
