// Copyright 2026 The Matrix.org Foundation C.I.C.
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

use std::time::Duration;

use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm};

/// A listener for the sync loop.
///
/// Called after each successful sync response when using
/// [`Client::sync_v2`](crate::client::Client::sync_v2).
#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait SyncListenerV2: SyncOutsideWasm + SendOutsideWasm {
    /// Called after each successful sync response.
    fn on_update(&self, response: SyncResponseV2);
}

/// Settings for a sync v2 call.
#[derive(uniffi::Record)]
pub struct SyncSettingsV2 {
    /// Timeout in milliseconds for the server long-poll.
    /// If not set, defaults to 30 seconds.
    #[uniffi(default = None)]
    pub timeout_ms: Option<u64>,
    /// Whether to request full state on the first sync.
    #[uniffi(default = false)]
    pub full_state: bool,
}

impl From<SyncSettingsV2> for matrix_sdk::config::SyncSettings {
    fn from(value: SyncSettingsV2) -> Self {
        let mut settings = matrix_sdk::config::SyncSettings::new();
        if let Some(timeout_ms) = value.timeout_ms {
            settings = settings.timeout(Duration::from_millis(timeout_ms));
        }
        if value.full_state {
            settings = settings.full_state(true);
        }
        settings
    }
}

/// The response from a sync v2 call.
#[derive(uniffi::Record)]
pub struct SyncResponseV2 {
    /// The batch token to supply in the `since` param of the next `/sync`
    /// request.
    pub next_batch: String,
    /// Updates to rooms.
    pub rooms: SyncResponseRoomsV2,
}

/// Room updates from a sync v2 response.
#[derive(uniffi::Record)]
pub struct SyncResponseRoomsV2 {
    /// Room IDs of rooms the user has been invited to.
    pub invited: Vec<String>,
    /// Room IDs of joined rooms that had updates.
    pub joined: Vec<String>,
    /// Room IDs of rooms the user has left.
    pub left: Vec<String>,
    /// Room IDs of rooms the user has knocked on.
    pub knocked: Vec<String>,
}

impl From<matrix_sdk::sync::SyncResponse> for SyncResponseV2 {
    fn from(value: matrix_sdk::sync::SyncResponse) -> Self {
        Self {
            next_batch: value.next_batch,
            rooms: SyncResponseRoomsV2 {
                invited: value.rooms.invited.keys().map(ToString::to_string).collect(),
                joined: value.rooms.joined.keys().map(ToString::to_string).collect(),
                left: value.rooms.left.keys().map(ToString::to_string).collect(),
                knocked: value.rooms.knocked.keys().map(ToString::to_string).collect(),
            },
        }
    }
}
