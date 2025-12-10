// Copyright 2025 The Matrix.org Foundation C.I.C.
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

//! Types for `io.element.msc4268.room_key_bundle` to-device events, per
//! [MSC4268].
//!
//! [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268

use ruma::{OwnedRoomId, events::room::EncryptedFile};
use serde::{Deserialize, Serialize};

use super::EventType;

// TODO: We need implement zeroize for this type.

/// The `io.element.msc4268.room_key_bundle` event content. See [MSC4268].
///
/// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomKeyBundleContent {
    /// The room that these keys are for.
    pub room_id: OwnedRoomId,

    /// The location and encryption info of the key bundle.
    pub file: EncryptedFile,
}

impl EventType for RoomKeyBundleContent {
    const EVENT_TYPE: &'static str = "io.element.msc4268.room_key_bundle";
}
