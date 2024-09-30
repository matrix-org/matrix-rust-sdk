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

use std::sync::{Arc, Mutex as StdMutex};

use ruma::{time::SystemTime, MxcUri};
use tokio::sync::Mutex as AsyncMutex;

use super::{DynEventCacheStore, MediaRetentionPolicy, Result};
use crate::media::MediaRequest;

/// A wrapper around an [`EventCacheStore`] implementation to abstract common
/// operations.
///
/// [`EventCacheStore`]: super::EventCacheStore
#[derive(Debug, Clone)]
pub struct EventCacheStoreWrapper {
    /// The inner store implementation.
    store: Arc<DynEventCacheStore>,
    /// The media retention policy.
    media_retention_policy: Arc<StdMutex<Option<MediaRetentionPolicy>>>,
    /// Guard to only have a single media cleanup at a time.
    media_cleanup_guard: Arc<AsyncMutex<()>>,
}

impl EventCacheStoreWrapper {
    /// Create a new `EventCacheStoreWrapper` around the given store.
    pub(crate) fn new(store: Arc<DynEventCacheStore>) -> Self {
        Self {
            store,
            media_retention_policy: Default::default(),
            media_cleanup_guard: Default::default(),
        }
    }

    /// The media retention policy.
    pub async fn media_retention_policy(&self) -> Result<MediaRetentionPolicy> {
        if let Some(policy) = *self.media_retention_policy.lock().unwrap() {
            return Ok(policy);
        }

        let policy = self.store.media_retention_policy().await?.unwrap_or_default();
        *self.media_retention_policy.lock().unwrap() = Some(policy);

        Ok(policy)
    }

    /// Set the media retention policy.
    pub async fn set_media_retention_policy(&self, policy: MediaRetentionPolicy) -> Result<()> {
        self.store.set_media_retention_policy(policy).await?;

        *self.media_retention_policy.lock().unwrap() = Some(policy);
        Ok(())
    }

    /// Add a media file's content in the media cache.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    /// * `content` - The content of the file.
    pub async fn add_media_content(&self, request: &MediaRequest, content: Vec<u8>) -> Result<()> {
        let policy = self.media_retention_policy().await?;

        if policy.exceeds_max_file_size(content.len()) {
            // The media content should not be cached.
            return Ok(());
        }

        // We let the store implementation check the max file size again because the
        // size of the content should change if it is encrypted.
        self.store.add_media_content(request, content, SystemTime::now(), policy).await
    }

    /// Get a media file's content out of the media cache.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    pub async fn get_media_content(&self, request: &MediaRequest) -> Result<Option<Vec<u8>>> {
        self.store.get_media_content(request, SystemTime::now()).await
    }

    /// Remove a media file's content from the media cache.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    pub async fn remove_media_content(&self, request: &MediaRequest) -> Result<()> {
        self.store.remove_media_content(request).await
    }

    /// Remove all the media files' content associated to an `MxcUri` from the
    /// media cache.
    ///
    /// # Arguments
    ///
    /// * `uri` - The `MxcUri` of the media files.
    pub async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        self.store.remove_media_content_for_uri(uri).await
    }

    /// Clean up the media cache with the current media retention policy.
    ///
    /// This is a noop if another cleanup is ongoing.
    pub async fn clean_up_media_cache(&self) -> Result<()> {
        let Ok(_guard) = self.media_cleanup_guard.try_lock() else {
            return Ok(());
        };

        let policy = self.media_retention_policy().await?;
        if !policy.has_limitations() {
            // We can safely skip all the checks.
            return Ok(());
        }

        self.store.clean_up_media_cache(policy, SystemTime::now()).await
    }
}
