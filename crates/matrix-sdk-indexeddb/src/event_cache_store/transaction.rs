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

use indexed_db_futures::{prelude::IdbTransaction, IdbQuerySource};
use matrix_sdk_base::{
    event_cache::{Event as RawEvent, Gap as RawGap},
    linked_chunk::{ChunkContent, ChunkIdentifier, RawChunk},
};
use ruma::{events::relation::RelationType, OwnedEventId, RoomId};
use serde::{de::DeserializeOwned, Serialize};
use thiserror::Error;
use web_sys::IdbCursorDirection;

use crate::event_cache_store::{
    serializer::{
        traits::{Indexed, IndexedKey, IndexedKeyBounds, IndexedKeyComponentBounds},
        types::{
            IndexedChunkIdKey, IndexedEventIdKey, IndexedEventPositionKey, IndexedEventRelationKey,
            IndexedGapIdKey, IndexedKeyRange, IndexedNextChunkIdKey,
        },
        IndexeddbEventCacheStoreSerializer,
    },
    types::{Chunk, ChunkType, Event, Gap, Position},
};

#[derive(Debug, Error)]
pub enum IndexeddbEventCacheStoreTransactionError {
    #[error("DomException {name} ({code}): {message}")]
    DomException { name: String, message: String, code: u16 },
}

impl From<web_sys::DomException> for IndexeddbEventCacheStoreTransactionError {
    fn from(value: web_sys::DomException) -> Self {
        Self::DomException { name: value.name(), message: value.message(), code: value.code() }
    }
}

pub struct IndexeddbEventCacheStoreTransaction<'a> {
    transaction: IdbTransaction<'a>,
    serializer: &'a IndexeddbEventCacheStoreSerializer,
}

impl<'a> IndexeddbEventCacheStoreTransaction<'a> {
    pub fn new(
        transaction: IdbTransaction<'a>,
        serializer: &'a IndexeddbEventCacheStoreSerializer,
    ) -> Self {
        Self { transaction, serializer }
    }

    pub fn into_inner(self) -> IdbTransaction<'a> {
        self.transaction
    }

    pub async fn commit(self) -> Result<(), IndexeddbEventCacheStoreTransactionError> {
        self.transaction.await.into_result().map_err(Into::into)
    }
}
