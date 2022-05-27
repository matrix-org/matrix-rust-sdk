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

#[cfg(feature = "experimental-timeline")]
pub use crate::timeline_stream::TimelineStreamError;
pub use crate::{
    error::{Error, Result},
    session::Session,
};

mod client;
mod error;
pub mod media;
mod rooms;
mod session;
pub mod store;
#[cfg(feature = "experimental-timeline")]
mod timeline_stream;
mod utils;

pub use client::BaseClient;
#[cfg(any(test, feature = "testing"))]
pub use http;
#[cfg(feature = "e2e-encryption")]
pub use matrix_sdk_crypto as crypto;
pub use once_cell;
pub use rooms::{DisplayName, Room, RoomInfo, RoomMember, RoomType};
pub use store::{StateChanges, StateStore, Store, StoreError};
pub use utils::{
    MinimalRoomMemberEvent, MinimalStateEvent, OriginalMinimalStateEvent, RedactedMinimalStateEvent,
};
