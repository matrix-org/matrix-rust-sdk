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
// limitations under the License

use std::time::Duration;

use matrix_sdk_base::{
    deserialized_responses::TimelineEvent,
    linked_chunk::{ChunkIdentifier, LinkedChunkId, OwnedLinkedChunkId},
    media::{store::IgnoreMediaRetentionPolicy, MediaRequestParameters},
};
use ruma::{OwnedEventId, OwnedRoomId, RoomId};
use serde::{Deserialize, Serialize};

/// Representation of a time-based lock on the entire
/// [`IndexeddbMediaStore`](crate::media_store::IndexeddbMediaStore)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lease {
    pub key: String,
    pub holder: String,
    pub expiration: Duration,
}

impl Lease {
    /// Determines whether the lease is expired at a given time `t`
    pub fn has_expired(&self, t: Duration) -> bool {
        self.expiration < t
    }
}

/// A representation of media data which can be stored in IndexedDB.
#[derive(Debug, Serialize, Deserialize)]
pub struct Media {
    /// The metadata associated with [`Media::content`]
    pub metadata: MediaMetadata,
    /// The content of the media
    pub content: Vec<u8>,
}

/// A representation of media metadata which can be stored in IndexedDB.
#[derive(Debug, Serialize, Deserialize)]
pub struct MediaMetadata {
    /// The parameters specifying the type and source of the media contained in
    /// [`Media::content`]
    pub request_parameters: MediaRequestParameters,
    /// The last time the media was accessed in IndexedDB
    pub last_access: Duration,
    /// Whether to ignore the [`MediaRetentionPolicy`][1] stored in IndexedDB
    ///
    /// [1]: matrix_sdk_base::media::store::MediaRetentionPolicy
    #[serde(with = "crate::media_store::serializer::foreign::ignore_media_retention_policy")]
    pub ignore_policy: IgnoreMediaRetentionPolicy,
}
