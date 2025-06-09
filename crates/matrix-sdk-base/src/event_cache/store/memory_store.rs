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
    num::NonZeroUsize,
    sync::{Arc, RwLock as StdRwLock},
};

use async_trait::async_trait;
use matrix_sdk_common::{
    linked_chunk::{
        relational::RelationalLinkedChunk, ChunkIdentifier, ChunkIdentifierGenerator,
        LinkedChunkId, Position, RawChunk, Update,
    },
    ring_buffer::RingBuffer,
    store_locks::memory_store_helper::try_take_leased_lock,
};
use ruma::{
    events::relation::RelationType,
    time::{Instant, SystemTime},
    EventId, MxcUri, OwnedEventId, OwnedMxcUri, RoomId,
};
use tracing::error;

use super::{
    compute_filters_string, extract_event_relation,
    media::{EventCacheStoreMedia, IgnoreMediaRetentionPolicy, MediaRetentionPolicy, MediaService},
    EventCacheStore, EventCacheStoreError, Result,
};
use crate::{
    event_cache::{Event, Gap},
    media::{MediaRequestParameters, UniqueKey as _},
};

/// In-memory, non-persistent implementation of the `EventCacheStore`.
///
/// Default if no other is configured at startup.
#[derive(Debug, Clone)]
pub struct MemoryStore {
    inner: Arc<StdRwLock<MemoryStoreInner>>,
    media_service: MediaService,
}

#[derive(Debug)]
struct MemoryStoreInner {
    media: RingBuffer<MediaContent>,
    leases: HashMap<String, (String, Instant)>,
    events: RelationalLinkedChunk<OwnedEventId, Event, Gap>,
    media_retention_policy: Option<MediaRetentionPolicy>,
    last_media_cleanup_time: SystemTime,
}

/// A media content in the `MemoryStore`.
#[derive(Debug)]
struct MediaContent {
    /// The URI of the content.
    uri: OwnedMxcUri,

    /// The unique key of the content.
    key: String,

    /// The bytes of the content.
    data: Vec<u8>,

    /// Whether we should ignore the [`MediaRetentionPolicy`] for this content.
    ignore_policy: bool,

    /// The time of the last access of the content.
    last_access: SystemTime,
}

const NUMBER_OF_MEDIAS: NonZeroUsize = NonZeroUsize::new(20).unwrap();

impl Default for MemoryStore {
    fn default() -> Self {
        // Given that the store is empty, we won't need to clean it up right away.
        let last_media_cleanup_time = SystemTime::now();
        let media_service = MediaService::new();
        media_service.restore(None, Some(last_media_cleanup_time));

        Self {
            inner: Arc::new(StdRwLock::new(MemoryStoreInner {
                media: RingBuffer::new(NUMBER_OF_MEDIAS),
                leases: Default::default(),
                events: RelationalLinkedChunk::new(),
                media_retention_policy: None,
                last_media_cleanup_time,
            })),
            media_service,
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
    ) -> Result<bool, Self::Error> {
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
        // Collect all duplicated events.
        let inner = self.inner.read().unwrap();

        let mut duplicated_events = Vec::new();

        for (event, position) in inner.events.unordered_linked_chunk_items(linked_chunk_id) {
            // If `events` is empty, we can short-circuit.
            if events.is_empty() {
                break;
            }

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

        let event = inner.events.items().find_map(|(event, this_linked_chunk_id)| {
            (room_id == this_linked_chunk_id.room_id() && event.event_id()? == event_id)
                .then_some(event.clone())
        });

        Ok(event)
    }

    async fn find_event_relations(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
        filters: Option<&[RelationType]>,
    ) -> Result<Vec<Event>, Self::Error> {
        let inner = self.inner.read().unwrap();

        let filters = compute_filters_string(filters);

        let related_events = inner
            .events
            .items()
            .filter_map(|(event, this_linked_chunk_id)| {
                // Must be in the same room.
                if room_id != this_linked_chunk_id.room_id() {
                    return None;
                }

                // Must have a relation.
                let (related_to, rel_type) = extract_event_relation(event.raw())?;

                // Must relate to the target item.
                if related_to != event_id {
                    return None;
                }

                // Must not be filtered out.
                if let Some(filters) = &filters {
                    filters.contains(&rel_type).then_some(event.clone())
                } else {
                    Some(event.clone())
                }
            })
            .collect();

        Ok(related_events)
    }

    async fn save_event(&self, room_id: &RoomId, event: Event) -> Result<(), Self::Error> {
        if event.event_id().is_none() {
            error!(%room_id, "Trying to save an event with no ID");
            return Ok(());
        }
        self.inner.write().unwrap().events.save_item(room_id.to_owned(), event);
        Ok(())
    }

    async fn add_media_content(
        &self,
        request: &MediaRequestParameters,
        data: Vec<u8>,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<()> {
        self.media_service.add_media_content(self, request, data, ignore_policy).await
    }

    async fn replace_media_key(
        &self,
        from: &MediaRequestParameters,
        to: &MediaRequestParameters,
    ) -> Result<(), Self::Error> {
        let expected_key = from.unique_key();

        let mut inner = self.inner.write().unwrap();

        if let Some(media_content) =
            inner.media.iter_mut().find(|media_content| media_content.key == expected_key)
        {
            media_content.uri = to.uri().to_owned();
            media_content.key = to.unique_key();
        }

        Ok(())
    }

    async fn get_media_content(&self, request: &MediaRequestParameters) -> Result<Option<Vec<u8>>> {
        self.media_service.get_media_content(self, request).await
    }

    async fn remove_media_content(&self, request: &MediaRequestParameters) -> Result<()> {
        let expected_key = request.unique_key();

        let mut inner = self.inner.write().unwrap();

        let Some(index) =
            inner.media.iter().position(|media_content| media_content.key == expected_key)
        else {
            return Ok(());
        };

        inner.media.remove(index);

        Ok(())
    }

    async fn get_media_content_for_uri(
        &self,
        uri: &MxcUri,
    ) -> Result<Option<Vec<u8>>, Self::Error> {
        self.media_service.get_media_content_for_uri(self, uri).await
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        let mut inner = self.inner.write().unwrap();

        let positions = inner
            .media
            .iter()
            .enumerate()
            .filter_map(|(position, media_content)| (media_content.uri == uri).then_some(position))
            .collect::<Vec<_>>();

        // Iterate in reverse-order so that positions stay valid after first removals.
        for position in positions.into_iter().rev() {
            inner.media.remove(position);
        }

        Ok(())
    }

    async fn set_media_retention_policy(
        &self,
        policy: MediaRetentionPolicy,
    ) -> Result<(), Self::Error> {
        self.media_service.set_media_retention_policy(self, policy).await
    }

    fn media_retention_policy(&self) -> MediaRetentionPolicy {
        self.media_service.media_retention_policy()
    }

    async fn set_ignore_media_retention_policy(
        &self,
        request: &MediaRequestParameters,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), Self::Error> {
        self.media_service.set_ignore_media_retention_policy(self, request, ignore_policy).await
    }

    async fn clean_up_media_cache(&self) -> Result<(), Self::Error> {
        self.media_service.clean_up_media_cache(self).await
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl EventCacheStoreMedia for MemoryStore {
    type Error = EventCacheStoreError;

    async fn media_retention_policy_inner(
        &self,
    ) -> Result<Option<MediaRetentionPolicy>, Self::Error> {
        Ok(self.inner.read().unwrap().media_retention_policy)
    }

    async fn set_media_retention_policy_inner(
        &self,
        policy: MediaRetentionPolicy,
    ) -> Result<(), Self::Error> {
        self.inner.write().unwrap().media_retention_policy = Some(policy);
        Ok(())
    }

    async fn add_media_content_inner(
        &self,
        request: &MediaRequestParameters,
        data: Vec<u8>,
        last_access: SystemTime,
        policy: MediaRetentionPolicy,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), Self::Error> {
        // Avoid duplication. Let's try to remove it first.
        self.remove_media_content(request).await?;

        let ignore_policy = ignore_policy.is_yes();

        if !ignore_policy && policy.exceeds_max_file_size(data.len() as u64) {
            // Do not store it.
            return Ok(());
        };

        // Now, let's add it.
        let mut inner = self.inner.write().unwrap();
        inner.media.push(MediaContent {
            uri: request.uri().to_owned(),
            key: request.unique_key(),
            data,
            ignore_policy,
            last_access,
        });

        Ok(())
    }

    async fn set_ignore_media_retention_policy_inner(
        &self,
        request: &MediaRequestParameters,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), Self::Error> {
        let mut inner = self.inner.write().unwrap();
        let expected_key = request.unique_key();

        if let Some(media_content) = inner.media.iter_mut().find(|media| media.key == expected_key)
        {
            media_content.ignore_policy = ignore_policy.is_yes();
        }

        Ok(())
    }

    async fn get_media_content_inner(
        &self,
        request: &MediaRequestParameters,
        current_time: SystemTime,
    ) -> Result<Option<Vec<u8>>, Self::Error> {
        let mut inner = self.inner.write().unwrap();
        let expected_key = request.unique_key();

        // First get the content out of the buffer, we are going to put it back at the
        // end.
        let Some(index) = inner.media.iter().position(|media| media.key == expected_key) else {
            return Ok(None);
        };
        let Some(mut content) = inner.media.remove(index) else {
            return Ok(None);
        };

        // Clone the data.
        let data = content.data.clone();

        // Update the last access time.
        content.last_access = current_time;

        // Put it back in the buffer.
        inner.media.push(content);

        Ok(Some(data))
    }

    async fn get_media_content_for_uri_inner(
        &self,
        expected_uri: &MxcUri,
        current_time: SystemTime,
    ) -> Result<Option<Vec<u8>>, Self::Error> {
        let mut inner = self.inner.write().unwrap();

        // First get the content out of the buffer, we are going to put it back at the
        // end.
        let Some(index) = inner.media.iter().position(|media| media.uri == expected_uri) else {
            return Ok(None);
        };
        let Some(mut content) = inner.media.remove(index) else {
            return Ok(None);
        };

        // Clone the data.
        let data = content.data.clone();

        // Update the last access time.
        content.last_access = current_time;

        // Put it back in the buffer.
        inner.media.push(content);

        Ok(Some(data))
    }

    async fn clean_up_media_cache_inner(
        &self,
        policy: MediaRetentionPolicy,
        current_time: SystemTime,
    ) -> Result<(), Self::Error> {
        if !policy.has_limitations() {
            // We can safely skip all the checks.
            return Ok(());
        }

        let mut inner = self.inner.write().unwrap();

        // First, check media content that exceed the max filesize.
        if policy.computed_max_file_size().is_some() {
            inner.media.retain(|content| {
                content.ignore_policy || !policy.exceeds_max_file_size(content.data.len() as u64)
            });
        }

        // Then, clean up expired media content.
        if policy.last_access_expiry.is_some() {
            inner.media.retain(|content| {
                content.ignore_policy
                    || !policy.has_content_expired(current_time, content.last_access)
            });
        }

        // Finally, if the cache size is too big, remove old items until it fits.
        if let Some(max_cache_size) = policy.max_cache_size {
            // Reverse the iterator because in case the cache size is overflowing, we want
            // to count the number of old items to remove. Items are sorted by last access
            // and old items are at the start.
            let (_, items_to_remove) = inner.media.iter().enumerate().rev().fold(
                (0u64, Vec::with_capacity(NUMBER_OF_MEDIAS.into())),
                |(mut cache_size, mut items_to_remove), (index, content)| {
                    if content.ignore_policy {
                        // Do not count it.
                        return (cache_size, items_to_remove);
                    }

                    let remove_item = if items_to_remove.is_empty() {
                        // We have not reached the max cache size yet.
                        if let Some(sum) = cache_size.checked_add(content.data.len() as u64) {
                            cache_size = sum;
                            // Start removing items if we have exceeded the max cache size.
                            cache_size > max_cache_size
                        } else {
                            // The cache size is overflowing, remove the remaining items, since the
                            // max cache size cannot be bigger than
                            // usize::MAX.
                            true
                        }
                    } else {
                        // We have reached the max cache size already, just remove it.
                        true
                    };

                    if remove_item {
                        items_to_remove.push(index);
                    }

                    (cache_size, items_to_remove)
                },
            );

            // The indexes are already in reverse order so we can just iterate in that order
            // to remove them starting by the end.
            for index in items_to_remove {
                inner.media.remove(index);
            }
        }

        inner.last_media_cleanup_time = current_time;

        Ok(())
    }

    async fn last_media_cleanup_time_inner(&self) -> Result<Option<SystemTime>, Self::Error> {
        Ok(Some(self.inner.read().unwrap().last_media_cleanup_time))
    }
}

#[cfg(test)]
mod tests {
    use super::{MemoryStore, Result};
    use crate::event_cache_store_media_integration_tests;

    async fn get_event_cache_store() -> Result<MemoryStore> {
        Ok(MemoryStore::new())
    }

    event_cache_store_integration_tests!();
    event_cache_store_integration_tests_time!();
    event_cache_store_media_integration_tests!(with_media_size_tests);
}
