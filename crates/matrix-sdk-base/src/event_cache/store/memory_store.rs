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

use std::{collections::HashMap, num::NonZeroUsize, sync::RwLock as StdRwLock, time::Instant};

use async_trait::async_trait;
use matrix_sdk_common::{
    linked_chunk::{relational::RelationalLinkedChunk, LinkedChunk, Update},
    ring_buffer::RingBuffer,
    store_locks::memory_store_helper::try_take_leased_lock,
};
use ruma::{MxcUri, OwnedMxcUri, RoomId};

use super::{EventCacheStore, EventCacheStoreError, Result, DEFAULT_CHUNK_CAPACITY};
use crate::{
    event_cache::{Event, Gap},
    media::{MediaRequestParameters, UniqueKey as _},
};

/// In-memory, non-persistent implementation of the `EventCacheStore`.
///
/// Default if no other is configured at startup.
#[allow(clippy::type_complexity)]
#[derive(Debug)]
pub struct MemoryStore {
    inner: StdRwLock<MemoryStoreInner>,
}

#[derive(Debug)]
struct MemoryStoreInner {
    media: RingBuffer<(OwnedMxcUri, String /* unique key */, Vec<u8>)>,
    leases: HashMap<String, (String, Instant)>,
    events: RelationalLinkedChunk<Event, Gap>,
}

// SAFETY: `new_unchecked` is safe because 20 is not zero.
const NUMBER_OF_MEDIAS: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(20) };

impl Default for MemoryStore {
    fn default() -> Self {
        Self {
            inner: StdRwLock::new(MemoryStoreInner {
                media: RingBuffer::new(NUMBER_OF_MEDIAS),
                leases: Default::default(),
                events: RelationalLinkedChunk::new(),
            }),
        }
    }
}

impl MemoryStore {
    /// Create a new empty MemoryStore
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
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
        room_id: &RoomId,
        updates: Vec<Update<Event, Gap>>,
    ) -> Result<(), Self::Error> {
        let mut inner = self.inner.write().unwrap();
        inner.events.apply_updates(room_id, updates);

        Ok(())
    }

    async fn reload_linked_chunk(
        &self,
        _room_id: &RoomId,
    ) -> Result<Option<LinkedChunk<DEFAULT_CHUNK_CAPACITY, Event, Gap>>, Self::Error> {
        // TODO(hywan)
        Ok(Default::default())
    }

    async fn add_media_content(
        &self,
        request: &MediaRequestParameters,
        data: Vec<u8>,
    ) -> Result<()> {
        // Avoid duplication. Let's try to remove it first.
        self.remove_media_content(request).await?;

        // Now, let's add it.
        let mut inner = self.inner.write().unwrap();
        inner.media.push((request.uri().to_owned(), request.unique_key(), data));

        Ok(())
    }

    async fn replace_media_key(
        &self,
        from: &MediaRequestParameters,
        to: &MediaRequestParameters,
    ) -> Result<(), Self::Error> {
        let expected_key = from.unique_key();

        let mut inner = self.inner.write().unwrap();

        if let Some((mxc, key, _)) = inner.media.iter_mut().find(|(_, key, _)| *key == expected_key)
        {
            *mxc = to.uri().to_owned();
            *key = to.unique_key();
        }

        Ok(())
    }

    async fn get_media_content(&self, request: &MediaRequestParameters) -> Result<Option<Vec<u8>>> {
        let expected_key = request.unique_key();

        let inner = self.inner.read().unwrap();

        Ok(inner.media.iter().find_map(|(_media_uri, media_key, media_content)| {
            (media_key == &expected_key).then(|| media_content.to_owned())
        }))
    }

    async fn remove_media_content(&self, request: &MediaRequestParameters) -> Result<()> {
        let expected_key = request.unique_key();

        let mut inner = self.inner.write().unwrap();

        let Some(index) = inner
            .media
            .iter()
            .position(|(_media_uri, media_key, _media_content)| media_key == &expected_key)
        else {
            return Ok(());
        };

        inner.media.remove(index);

        Ok(())
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        let mut inner = self.inner.write().unwrap();

        let expected_key = uri.to_owned();
        let positions = inner
            .media
            .iter()
            .enumerate()
            .filter_map(|(position, (media_uri, _media_key, _media_content))| {
                (media_uri == &expected_key).then_some(position)
            })
            .collect::<Vec<_>>();

        // Iterate in reverse-order so that positions stay valid after first removals.
        for position in positions.into_iter().rev() {
            inner.media.remove(position);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{EventCacheStore, MemoryStore, Result};

    async fn get_event_cache_store() -> Result<impl EventCacheStore> {
        Ok(MemoryStore::new())
    }

    event_cache_store_integration_tests!();
    event_cache_store_integration_tests_time!();
}
