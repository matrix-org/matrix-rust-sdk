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

use std::collections::BTreeMap;

use ruma::{OwnedRoomId, api::client::backup::RoomKeyBackup};

/// A request that will back up a batch of room keys to the server.
#[derive(Clone, Debug)]
pub struct KeysBackupRequest {
    /// The backup version that these room keys should be part of.
    pub version: String,
    /// The map from room id to a backed up room key that we're going to upload
    /// to the server.
    pub rooms: BTreeMap<OwnedRoomId, RoomKeyBackup>,
}
