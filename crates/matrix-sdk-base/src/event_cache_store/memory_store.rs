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

use std::{num::NonZeroUsize, sync::RwLock as StdRwLock};

use async_trait::async_trait;
use matrix_sdk_common::ring_buffer::RingBuffer;
use ruma::{time::SystemTime, MxcUri, OwnedMxcUri};

use super::{EventCacheStore, EventCacheStoreError, MediaRetentionPolicy, Result};
use crate::media::{MediaRequest, UniqueKey as _};

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
    /// The media retention policy to use on cleanups.
    media_retention_policy: Option<MediaRetentionPolicy>,
    /// Media content.
    media: RingBuffer<MediaContent>,
}

// SAFETY: `new_unchecked` is safe because 20 is not zero.
const NUMBER_OF_MEDIAS: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(20) };

/// A media content.
#[derive(Debug, Clone)]
struct MediaContent {
    /// The Matrix URI of the media.
    uri: OwnedMxcUri,
    /// The unique key of the media request.
    key: String,
    /// The content of the media.
    data: Vec<u8>,
    /// The last access time of the media.
    last_access: SystemTime,
}

impl Default for MemoryStore {
    fn default() -> Self {
        let inner = MemoryStoreInner {
            media_retention_policy: Default::default(),
            media: RingBuffer::new(NUMBER_OF_MEDIAS),
        };

        Self { inner: StdRwLock::new(inner) }
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

    async fn media_retention_policy(&self) -> Result<Option<MediaRetentionPolicy>, Self::Error> {
        Ok(self.inner.read().unwrap().media_retention_policy)
    }

    async fn set_media_retention_policy(
        &self,
        policy: MediaRetentionPolicy,
    ) -> Result<(), Self::Error> {
        let mut inner = self.inner.write().unwrap();
        inner.media_retention_policy = Some(policy);

        Ok(())
    }

    async fn add_media_content(
        &self,
        request: &MediaRequest,
        data: Vec<u8>,
        current_time: SystemTime,
        policy: MediaRetentionPolicy,
    ) -> Result<()> {
        // Avoid duplication. Let's try to remove it first.
        self.remove_media_content(request).await?;

        if policy.exceeds_max_file_size(data.len()) {
            // The content is too big to be cached.
            return Ok(());
        }

        // Now, let's add it.
        let content = MediaContent {
            uri: request.uri().to_owned(),
            key: request.unique_key(),
            data,
            last_access: current_time,
        };
        self.inner.write().unwrap().media.push(content);

        Ok(())
    }

    async fn get_media_content(
        &self,
        request: &MediaRequest,
        current_time: SystemTime,
    ) -> Result<Option<Vec<u8>>> {
        let mut inner = self.inner.write().unwrap();
        let expected_key = request.unique_key();

        // First get the content out of the buffer.
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

    async fn remove_media_content(&self, request: &MediaRequest) -> Result<()> {
        let mut inner = self.inner.write().unwrap();

        let expected_key = request.unique_key();
        let Some(index) = inner.media.iter().position(|media| media.key == expected_key) else {
            return Ok(());
        };

        inner.media.remove(index);

        Ok(())
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        let mut inner = self.inner.write().unwrap();
        let positions = inner
            .media
            .iter()
            .enumerate()
            .filter_map(|(position, media)| (media.uri == uri).then_some(position))
            .collect::<Vec<_>>();

        // Iterate in reverse-order so that positions stay valid after first removals.
        for position in positions.into_iter().rev() {
            inner.media.remove(position);
        }

        Ok(())
    }

    async fn clean_up_media_cache(
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
        if policy.max_file_size.is_some() || policy.max_cache_size.is_some() {
            inner.media.retain(|content| !policy.exceeds_max_file_size(content.data.len()));
        }

        // Then, clean up expired media content.
        if policy.last_access_expiry.is_some() {
            inner
                .media
                .retain(|content| !policy.has_content_expired(current_time, content.last_access));
        }

        // Finally, if the cache size is too big, remove old items until it fits.
        if let Some(max_cache_size) = policy.max_cache_size {
            // Reverse the iterator because in case the cache size is overflowing, we want
            // to count the number of old items to remove, and old items are at
            // the start.
            let (cache_size, overflowing_count) = inner.media.iter().rev().fold(
                (0usize, 0u8),
                |(cache_size, overflowing_count), content| {
                    if overflowing_count > 0 {
                        // Assume that all data is overflowing now. Overflowing count cannot
                        // overflow because the number of items is limited to 20.
                        (cache_size, overflowing_count + 1)
                    } else {
                        match cache_size.checked_add(content.data.len()) {
                            Some(cache_size) => (cache_size, 0),
                            // The cache size is overflowing, let's count the number of overflowing
                            // items to be able to remove them, since the max cache size cannot be
                            // bigger than usize::MAX.
                            None => (cache_size, 1),
                        }
                    }
                },
            );

            // If the cache size is overflowing, remove the number of old items we counted.
            for _position in 0..overflowing_count {
                inner.media.pop();
            }

            if cache_size > max_cache_size {
                let difference = cache_size - max_cache_size;

                // Count the number of old items to remove to reach the difference.
                let mut accumulated_items_size = 0usize;
                let mut remove_items_count = 0u8;
                for content in inner.media.iter() {
                    remove_items_count += 1;
                    // Cannot overflow since we already removed overflowing items.
                    accumulated_items_size += content.data.len();

                    if accumulated_items_size >= difference {
                        break;
                    }
                }

                for _position in 0..remove_items_count {
                    inner.media.pop();
                }
            }
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

    event_cache_store_integration_tests!(with_media_size_tests);
}
