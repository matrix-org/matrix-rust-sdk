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

//! Storage usage reporting, shared by the stores.

use std::collections::BTreeMap;

use ruma::OwnedRoomId;

/// How much of a store's storage is used, in bytes, overall and per room.
///
/// Sizes are the stored payloads' sizes, an approximation of the space taken
/// on disk (which also depends on the store's indexes and page layout).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StorageUsage {
    /// The size of everything the store holds for this kind of data, in
    /// bytes, including data not attributable to any of the rooms below.
    pub total_bytes: u64,

    /// The size attributable to each of the rooms asked about, in bytes;
    /// rooms without any data are absent.
    pub per_room: BTreeMap<OwnedRoomId, u64>,
}
