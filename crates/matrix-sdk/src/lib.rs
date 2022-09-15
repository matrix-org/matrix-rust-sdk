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
pub use matrix_sdk_base::{
    DisplayName, Room as BaseRoom, RoomInfo, RoomMember as BaseRoomMember, RoomType, Session,
    StateChanges, StoreError,
};
pub use matrix_sdk_common::*;
pub use reqwest;
#[doc(no_inline)]
pub use ruma;

mod account;
pub mod attachment;
mod client;
pub mod config;
mod error;
pub mod event_handler;
mod http_client;
pub mod media;
pub mod room;
pub mod store;
mod sync;

#[cfg(feature = "sliding-sync")]
mod sliding_sync;

#[cfg(feature = "e2e-encryption")]
pub mod encryption;

pub use account::Account;
#[cfg(feature = "sso-login")]
pub use client::SsoLoginBuilder;
pub use client::{Client, ClientBuildError, ClientBuilder, LoginBuilder, LoopCtrl};
#[cfg(feature = "image-proc")]
pub use error::ImageError;
pub use error::{Error, HttpError, HttpResult, RefreshTokenError, Result, RumaApiError};
pub use http_client::HttpSend;
pub use media::Media;
#[cfg(feature = "sliding-sync")]
pub use sliding_sync::{
    RoomListEntry, SlidingSync, SlidingSyncBuilder, SlidingSyncMode, SlidingSyncRoom,
    SlidingSyncState, SlidingSyncView, SlidingSyncViewBuilder, UpdateSummary,
};

#[cfg(test)]
mod test_utils;

#[cfg(all(test, not(target_arch = "wasm32")))]
#[ctor::ctor]
fn init_logging() {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::from_default_env())
        .with(tracing_subscriber::fmt::layer().with_test_writer())
        .init();
}
