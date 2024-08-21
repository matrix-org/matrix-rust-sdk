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
use ruma::{MxcUri, OwnedMxcUri};

use super::{EventCacheStore, EventCacheStoreError, Result};
use crate::media::{MediaRequest, UniqueKey as _};

/// In-memory, non-persistent implementation of the `EventCacheStore`.
///
/// Default if no other is configured at startup.
#[allow(clippy::type_complexity)]
#[derive(Debug)]
pub struct MemoryStore {
    media: StdRwLock<RingBuffer<(OwnedMxcUri, String /* unique key */, Vec<u8>)>>,
}

// SAFETY: `new_unchecked` is safe because 20 is not zero.
const NUMBER_OF_MEDIAS: NonZeroUsize = unsafe { NonZeroUsize::new_unchecked(20) };

impl Default for MemoryStore {
    fn default() -> Self {
        Self { media: StdRwLock::new(RingBuffer::new(NUMBER_OF_MEDIAS)) }
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

    async fn add_media_content(&self, request: &MediaRequest, data: Vec<u8>) -> Result<()> {
        // Avoid duplication. Let's try to remove it first.
        self.remove_media_content(request).await?;
        // Now, let's add it.
        self.media.write().unwrap().push((request.uri().to_owned(), request.unique_key(), data));

        Ok(())
    }

    async fn get_media_content(&self, request: &MediaRequest) -> Result<Option<Vec<u8>>> {
        let media = self.media.read().unwrap();
        let expected_key = request.unique_key();

        Ok(media.iter().find_map(|(_media_uri, media_key, media_content)| {
            (media_key == &expected_key).then(|| media_content.to_owned())
        }))
    }

    async fn remove_media_content(&self, request: &MediaRequest) -> Result<()> {
        let mut media = self.media.write().unwrap();
        let expected_key = request.unique_key();
        let Some(index) = media
            .iter()
            .position(|(_media_uri, media_key, _media_content)| media_key == &expected_key)
        else {
            return Ok(());
        };

        media.remove(index);

        Ok(())
    }

    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        let mut media = self.media.write().unwrap();
        let expected_key = uri.to_owned();
        let positions = media
            .iter()
            .enumerate()
            .filter_map(|(position, (media_uri, _media_key, _media_content))| {
                (media_uri == &expected_key).then_some(position)
            })
            .collect::<Vec<_>>();

        // Iterate in reverse-order so that positions stay valid after first removals.
        for position in positions.into_iter().rev() {
            media.remove(position);
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
}
