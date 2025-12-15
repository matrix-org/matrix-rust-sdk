// Copyright 2024 The Matrix.org Foundation C.I.C.
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

use std::{
    collections::HashMap,
    sync::{Arc, RwLock as StdRwLock},
};

use async_trait::async_trait;
use matrix_sdk_common::{
    cross_process_lock::{
        CrossProcessLockGeneration,
        memory_store_helper::{Lease, try_take_leased_lock},
    },
    linked_chunk::{
        ChunkIdentifier, ChunkIdentifierGenerator, ChunkMetadata, LinkedChunkId, Position,
        RawChunk, Update, relational::RelationalLinkedChunk,
    },
};
use ruma::{EventId, OwnedEventId, RoomId, events::relation::RelationType};
use tracing::error;

use super::{EventCacheStore, EventCacheStoreError, Result, extract_event_relation};
use crate::event_cache::{Event, Gap};

/// In-memory, non-persistent implementation of the `EventCacheStore`.
///
/// Default if no other is configured at startup.
#[derive(Debug, Clone)]
pub struct MemoryStore {
    inner: Arc<StdRwLock<MemoryStoreInner>>,
}

#[derive(Debug)]
struct MemoryStoreInner {
    leases: HashMap<String, Lease>,
    events: RelationalLinkedChunk<OwnedEventId, Event, Gap>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self {
            inner: Arc::new(StdRwLock::new(MemoryStoreInner {
                leases: Default::default(),
                events: RelationalLinkedChunk::new(),
            })),
        }
    }
}

impl MemoryStore {
    /// Create a new empty MemoryStore
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl EventCacheStore for MemoryStore {
    type Error = EventCacheStoreError;

    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<Option<CrossProcessLockGeneration>, Self::Error> {
        let mut inner = self.inner.write().unwrap();

        Ok(try_take_leased_lock(&mut inner.leases, lease_duration_ms, key, holder))
    }

    async fn handle_linked_chunk_updates(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        updates: Vec<Update<Event, Gap>>,
    ) -> Result<(), Self::Error> {
        let mut inner = self.inner.write().unwrap();
        inner.events.apply_updates(linked_chunk_id, updates);

        Ok(())
    }

    async fn load_all_chunks(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Vec<RawChunk<Event, Gap>>, Self::Error> {
        let inner = self.inner.read().unwrap();
        inner
            .events
            .load_all_chunks(linked_chunk_id)
            .map_err(|err| EventCacheStoreError::InvalidData { details: err })
    }

    async fn load_all_chunks_metadata(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Vec<ChunkMetadata>, Self::Error> {
        let inner = self.inner.read().unwrap();
        inner
            .events
            .load_all_chunks_metadata(linked_chunk_id)
            .map_err(|err| EventCacheStoreError::InvalidData { details: err })
    }

    async fn load_last_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<(Option<RawChunk<Event, Gap>>, ChunkIdentifierGenerator), Self::Error> {
        let inner = self.inner.read().unwrap();
        inner
            .events
            .load_last_chunk(linked_chunk_id)
            .map_err(|err| EventCacheStoreError::InvalidData { details: err })
    }

    async fn load_previous_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        before_chunk_identifier: ChunkIdentifier,
    ) -> Result<Option<RawChunk<Event, Gap>>, Self::Error> {
        let inner = self.inner.read().unwrap();
        inner
            .events
            .load_previous_chunk(linked_chunk_id, before_chunk_identifier)
            .map_err(|err| EventCacheStoreError::InvalidData { details: err })
    }

    async fn clear_all_linked_chunks(&self) -> Result<(), Self::Error> {
        self.inner.write().unwrap().events.clear();
        Ok(())
    }

    async fn filter_duplicated_events(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        mut events: Vec<OwnedEventId>,
    ) -> Result<Vec<(OwnedEventId, Position)>, Self::Error> {
        if events.is_empty() {
            return Ok(Vec::new());
        }

        let inner = self.inner.read().unwrap();

        let mut duplicated_events = Vec::new();

        for (event, position) in
            inner.events.unordered_linked_chunk_items(&linked_chunk_id.to_owned())
        {
            if let Some(known_event_id) = event.event_id() {
                // This event is a duplicate!
                if let Some(index) =
                    events.iter().position(|new_event_id| &known_event_id == new_event_id)
                {
                    duplicated_events.push((events.remove(index), position));
                }
            }
        }

        Ok(duplicated_events)
    }

    async fn find_event(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<Event>, Self::Error> {
        let inner = self.inner.read().unwrap();

        let event = inner
            .events
            .items(room_id)
            .find_map(|(event, _pos)| (event.event_id()? == event_id).then_some(event.clone()));

        Ok(event)
    }

    async fn find_event_relations(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
        filters: Option<&[RelationType]>,
    ) -> Result<Vec<(Event, Option<Position>)>, Self::Error> {
        let inner = self.inner.read().unwrap();

        let related_events = inner
            .events
            .items(room_id)
            .filter_map(|(event, pos)| {
                // Must have a relation.
                let (related_to, rel_type) = extract_event_relation(event.raw())?;
                let rel_type = RelationType::from(rel_type.as_str());

                // Must relate to the target item.
                if related_to != event_id {
                    return None;
                }

                // Must not be filtered out.
                if let Some(filters) = &filters {
                    filters.contains(&rel_type).then_some((event.clone(), pos))
                } else {
                    Some((event.clone(), pos))
                }
            })
            .collect();

        Ok(related_events)
    }

    async fn get_room_events(
        &self,
        room_id: &RoomId,
        event_type: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Vec<Event>, Self::Error> {
        let inner = self.inner.read().unwrap();

        let event: Vec<_> = inner
            .events
            .items(room_id)
            .map(|(event, _pos)| event.clone())
            .filter(|e| {
                event_type
                    .is_none_or(|event_type| Some(event_type) == e.kind.event_type().as_deref())
            })
            .filter(|e| session_id.is_none_or(|s| Some(s) == e.kind.session_id()))
            .collect();

        Ok(event)
    }

    async fn save_event(&self, room_id: &RoomId, event: Event) -> Result<(), Self::Error> {
        if event.event_id().is_none() {
            error!(%room_id, "Trying to save an event with no ID");
            return Ok(());
        }
        self.inner.write().unwrap().events.save_item(room_id.to_owned(), event);
        Ok(())
    }

    async fn optimize(&self) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn get_size(&self) -> Result<Option<usize>, Self::Error> {
        Ok(None)
    }
}

#[cfg(test)]
#[allow(unused_imports)] // There seems to be a false positive when importing the test macros.
mod tests {
    use super::{MemoryStore, Result};
    use crate::{event_cache_store_integration_tests, event_cache_store_integration_tests_time};

    async fn get_event_cache_store() -> Result<MemoryStore> {
        Ok(MemoryStore::new())
    }

    event_cache_store_integration_tests!();
    #[cfg(not(target_family = "wasm"))]
    event_cache_store_integration_tests_time!();
}
