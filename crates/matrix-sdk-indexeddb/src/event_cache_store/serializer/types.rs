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

use serde::{Deserialize, Serialize};

use crate::serializer::MaybeEncrypted;

#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedChunk {
    pub id: IndexedChunkIdKey,
    pub next: IndexedChunkIdKey,
    pub content: MaybeEncrypted, /* ChunkForCache */
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedChunkIdKey(String /* Room ID */, String /* Chunk ID */);

#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedEvent {
    pub id: IndexedEventIdKey,
    pub position: Option<IndexedEventPositionKey>,
    pub relation: Option<IndexedEventRelationKey>,
    pub content: MaybeEncrypted, /* EventForCache */
}

#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedEventIdKey(String /* Room ID */, String /* Event ID */);

#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedEventPositionKey(
    String, /* Room ID */
    String, /* Chunk ID */
    String, /* Index */
);

#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedEventRelationKey(
    String, /* Room ID */
    String, /* Related Event ID */
    String, /* Relation Type */
);

#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedGap {
    pub id: String,
    pub content: MaybeEncrypted, /* GapForCache */
}
