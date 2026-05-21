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
    cross_process_lock::{
        CrossProcessLockGeneration,
        memory_store_helper::{Lease, try_take_leased_lock},
    },
    ring_buffer::RingBuffer,
};
use ruma::{MxcUri, OwnedMxcUri, time::SystemTime};

use super::Result;
use crate::media::{
    MediaRequestParameters, UniqueKey as _,
    store::{
        IgnoreMediaRetentionPolicy, MediaRetentionPolicy, MediaService, MediaStore,
        MediaStoreError, MediaStoreInner,
    },
};

/// In-memory, non-persistent implementation of the `MediaStore`.
///
/// Default if no other is configured at startup.
#[derive(Debug, Clone)]
pub struct MemoryMediaStore {
    inner: Arc<StdRwLock<MemoryMediaStoreInner>>,
    media_service: MediaService,
}

#[derive(Debug)]
struct MemoryMediaStoreInner {
    media: RingBuffer<MediaContent>,
    leases: HashMap<String, Lease>,
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

impl Default for MemoryMediaStore {
    fn default() -> Self {
        // Given that the store is empty, we won't need to clean it up right away.
        let last_media_cleanup_time = SystemTime::now();
        let media_service = MediaService::new();
        media_service.restore(None, Some(last_media_cleanup_time));

        Self {
            inner: Arc::new(StdRwLock::new(MemoryMediaStoreInner {
                media: RingBuffer::new(NUMBER_OF_MEDIAS),
                leases: Default::default(),
                media_retention_policy: None,
                last_media_cleanup_time,
            })),
            media_service,
        }
    }
}

impl MemoryMediaStore {
    /// Create a new empty MemoryMediaStore
    pub fn new() -> Self {
        Self::default()
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl MediaStore for MemoryMediaStore {
    type Error = MediaStoreError;

    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<Option<CrossProcessLockGeneration>, Self::Error> {
        let mut inner = self.inner.write().unwrap();

        Ok(try_take_leased_lock(&mut inner.leases, lease_duration_ms, key, holder))
    }

    async fn add_media_content(
        &self,
        request: &MediaRequestParameters,
        data: Vec<u8>,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), Self::Error> {
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

    async fn get_media_content(
        &self,
        request: &MediaRequestParameters,
    ) -> Result<Option<Vec<u8>>, Self::Error> {
        self.media_service.get_media_content(self, request).await
    }

    async fn remove_media_content(
        &self,
        request: &MediaRequestParameters,
    ) -> Result<(), Self::Error> {
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

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<(), Self::Error> {
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

    async fn clean(&self) -> Result<(), Self::Error> {
        self.media_service.clean(self).await
    }

    async fn optimize(&self) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn get_size(&self) -> Result<Option<usize>, Self::Error> {
        Ok(None)
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl MediaStoreInner for MemoryMediaStore {
    type Error = MediaStoreError;

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
        }

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

    async fn clean_inner(
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
    use super::{MemoryMediaStore, Result};
    use crate::{
        media_store_inner_integration_tests, media_store_integration_tests,
        media_store_integration_tests_time,
    };

    async fn get_media_store() -> Result<MemoryMediaStore> {
        Ok(MemoryMediaStore::new())
    }

    media_store_inner_integration_tests!();
    media_store_integration_tests!();
    media_store_integration_tests_time!();
}
