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
#![cfg_attr(docsrs, feature(doc_auto_cfg))]

pub use async_trait::async_trait;
pub use bytes;
#[cfg(feature = "e2e-encryption")]
pub use matrix_sdk_base::crypto;
pub use matrix_sdk_base::{
    deserialized_responses,
    store::{DynStateStore, MemoryStore, StateStoreExt},
    DisplayName, Room as BaseRoom, RoomCreateWithCreatorEventContent, RoomInfo,
    RoomMember as BaseRoomMember, RoomMemberships, RoomState, SessionMeta, StateChanges,
    StateStore, StoreError,
};
pub use matrix_sdk_common::*;
pub use reqwest;

mod account;
pub mod attachment;
mod authentication;
mod client;
pub mod config;
mod deduplicating_handler;
#[cfg(feature = "e2e-encryption")]
pub mod encryption;
mod error;
pub mod event_cache;
pub mod event_handler;
mod http_client;
pub mod matrix_auth;
pub mod media;
pub mod notification_settings;
#[cfg(feature = "experimental-oidc")]
pub mod oidc;
pub mod pusher;
pub mod room;
pub mod room_directory_search;
pub mod room_preview;
pub mod utils;
pub mod futures {
    //! Named futures returned from methods on types in [the crate root][crate].

    pub use super::client::futures::SendRequest;
}
#[cfg(feature = "experimental-sliding-sync")]
pub mod sliding_sync;
pub mod sync;
#[cfg(feature = "experimental-widgets")]
pub mod widget;

pub use account::Account;
pub use authentication::{AuthApi, AuthSession, SessionTokens};
pub use client::{
    sanitize_server_name, Client, ClientBuildError, ClientBuilder, LoopCtrl, SessionChange,
};
#[cfg(feature = "image-proc")]
pub use error::ImageError;
pub use error::{
    Error, HttpError, HttpResult, NotificationSettingsError, RefreshTokenError, Result,
    RumaApiError,
};
pub use http_client::TransmissionProgress;
#[cfg(all(feature = "e2e-encryption", feature = "sqlite"))]
pub use matrix_sdk_sqlite::SqliteCryptoStore;
#[cfg(feature = "sqlite")]
pub use matrix_sdk_sqlite::SqliteStateStore;
pub use media::Media;
pub use pusher::Pusher;
pub use room::Room;
pub use ruma::{IdParseError, OwnedServerName, ServerName};
#[cfg(feature = "experimental-sliding-sync")]
pub use sliding_sync::{
    RoomListEntry, SlidingSync, SlidingSyncBuilder, SlidingSyncList, SlidingSyncListBuilder,
    SlidingSyncListLoadingState, SlidingSyncMode, SlidingSyncRoom, UpdateSummary,
};

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

#[cfg(any(test, feature = "testing"))]
pub mod test_utils;

#[cfg(test)]
matrix_sdk_test::init_tracing_for_tests!();
