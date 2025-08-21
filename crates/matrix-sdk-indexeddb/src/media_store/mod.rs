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

mod builder;
mod error;

use matrix_sdk_base::{
    event_cache::store::{
        media::{IgnoreMediaRetentionPolicy, MediaRetentionPolicy, MediaStore},
        MemoryMediaStore,
    },
    media::MediaRequestParameters,
    timer,
};
use ruma::MxcUri;
use tracing::instrument;

use crate::media_store::{builder::IndexeddbMediaStoreBuilder, error::IndexeddbMediaStoreError};

/// A type for providing an IndexedDB implementation of [`MediaStore`][1].
/// This is meant to be used as a backend to [`MediaStore`][1] in browser
/// contexts.
///
/// [1]: matrix_sdk_base::event_cache::store::MediaStore
#[derive(Debug)]
pub struct IndexeddbMediaStore {
    // An in-memory store for providing temporary implementations for
    // functions of `MediaStore`.
    //
    // NOTE: This will be removed once we have IndexedDB-backed implementations for all
    // functions in `MediaStore`.
    memory_store: MemoryMediaStore,
}

impl IndexeddbMediaStore {
    /// Provides a type with which to conveniently build an
    /// [`IndexeddbEventCacheStore`]
    pub fn builder() -> IndexeddbMediaStoreBuilder {
        IndexeddbMediaStoreBuilder::default()
    }
}

// Small hack to have the following macro invocation act as the appropriate
// trait impl block on wasm, but still be compiled on non-wasm as a regular
// impl block otherwise.
//
// The trait impl doesn't compile on non-wasm due to unfulfilled trait bounds,
// this hack allows us to still have most of rust-analyzer's IDE functionality
// within the impl block without having to set it up to check things against
// the wasm target (which would disable many other parts of the codebase).
#[cfg(target_arch = "wasm32")]
macro_rules! impl_media_store {
    ( $($body:tt)* ) => {
        #[async_trait::async_trait(?Send)]
        impl MediaStore for IndexeddbMediaStore {
            type Error = IndexeddbMediaStoreError;

            $($body)*
        }
    };
}

#[cfg(not(target_arch = "wasm32"))]
macro_rules! impl_media_store {
    ( $($body:tt)* ) => {
        impl IndexeddbMediaStore {
            $($body)*
        }
    };
}

impl_media_store! {
    #[instrument(skip(self))]
    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<bool, IndexeddbMediaStoreError> {
        let _timer = timer!("method");
        self.memory_store
            .try_take_leased_lock(lease_duration_ms, key, holder)
            .await
            .map_err(IndexeddbMediaStoreError::MemoryStore)
    }

    #[instrument(skip_all)]
    async fn add_media_content(
        &self,
        request: &MediaRequestParameters,
        content: Vec<u8>,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), IndexeddbMediaStoreError> {
        let _timer = timer!("method");
        self.memory_store
            .add_media_content(request, content, ignore_policy)
            .await
            .map_err(IndexeddbMediaStoreError::MemoryStore)
    }

    #[instrument(skip_all)]
    async fn replace_media_key(
        &self,
        from: &MediaRequestParameters,
        to: &MediaRequestParameters,
    ) -> Result<(), IndexeddbMediaStoreError> {
        let _timer = timer!("method");
        self.memory_store
            .replace_media_key(from, to)
            .await
            .map_err(IndexeddbMediaStoreError::MemoryStore)
    }

    #[instrument(skip_all)]
    async fn get_media_content(
        &self,
        request: &MediaRequestParameters,
    ) -> Result<Option<Vec<u8>>, IndexeddbMediaStoreError> {
        let _timer = timer!("method");
        self.memory_store
            .get_media_content(request)
            .await
            .map_err(IndexeddbMediaStoreError::MemoryStore)
    }

    #[instrument(skip_all)]
    async fn remove_media_content(
        &self,
        request: &MediaRequestParameters,
    ) -> Result<(), IndexeddbMediaStoreError> {
        let _timer = timer!("method");
        self.memory_store
            .remove_media_content(request)
            .await
            .map_err(IndexeddbMediaStoreError::MemoryStore)
    }

    #[instrument(skip(self))]
    async fn get_media_content_for_uri(
        &self,
        uri: &MxcUri,
    ) -> Result<Option<Vec<u8>>, IndexeddbMediaStoreError> {
        let _timer = timer!("method");
        self.memory_store
            .get_media_content_for_uri(uri)
            .await
            .map_err(IndexeddbMediaStoreError::MemoryStore)
    }

    #[instrument(skip(self))]
    async fn remove_media_content_for_uri(
        &self,
        uri: &MxcUri,
    ) -> Result<(), IndexeddbMediaStoreError> {
        let _timer = timer!("method");
        self.memory_store
            .remove_media_content_for_uri(uri)
            .await
            .map_err(IndexeddbMediaStoreError::MemoryStore)
    }

    #[instrument(skip_all)]
    async fn set_media_retention_policy(
        &self,
        policy: MediaRetentionPolicy,
    ) -> Result<(), IndexeddbMediaStoreError> {
        let _timer = timer!("method");
        self.memory_store
            .set_media_retention_policy(policy)
            .await
            .map_err(IndexeddbMediaStoreError::MemoryStore)
    }

    #[instrument(skip_all)]
    fn media_retention_policy(&self) -> MediaRetentionPolicy {
        let _timer = timer!("method");
        self.memory_store.media_retention_policy()
    }

    #[instrument(skip_all)]
    async fn set_ignore_media_retention_policy(
        &self,
        request: &MediaRequestParameters,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), IndexeddbMediaStoreError> {
        let _timer = timer!("method");
        self.memory_store
            .set_ignore_media_retention_policy(request, ignore_policy)
            .await
            .map_err(IndexeddbMediaStoreError::MemoryStore)
    }

    #[instrument(skip_all)]
    async fn clean_up_media_cache(&self) -> Result<(), IndexeddbMediaStoreError> {
        let _timer = timer!("method");
        self.memory_store
            .clean_up_media_cache()
            .await
            .map_err(IndexeddbMediaStoreError::MemoryStore)
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_base::{
        event_cache::store::media::{MediaStore, MediaStoreError},
        media_store_integration_tests, media_store_integration_tests_time,
    };
    use matrix_sdk_test::async_test;
    use uuid::Uuid;

    use crate::media_store::{error::IndexeddbMediaStoreError, IndexeddbMediaStore};

    impl From<IndexeddbMediaStoreError> for MediaStoreError {
        fn from(value: IndexeddbMediaStoreError) -> Self {
            Self::Backend(Box::new(value))
        }
    }

    mod unencrypted {
        use super::*;

        wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

        async fn get_media_store() -> Result<IndexeddbMediaStore, MediaStoreError> {
            let name = format!("test-media-store-{}", Uuid::new_v4().as_hyphenated());
            Ok(IndexeddbMediaStore::builder().build()?)
        }

        #[cfg(target_family = "wasm")]
        media_store_integration_tests!();

        #[cfg(target_family = "wasm")]
        media_store_integration_tests_time!();
    }

    mod encrypted {
        use super::*;

        wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

        async fn get_event_cache_store() -> Result<IndexeddbMediaStore, MediaStoreError> {
            let name = format!("test-media-store-{}", Uuid::new_v4().as_hyphenated());
            Ok(IndexeddbMediaStore::builder().build()?)
        }

        #[cfg(target_family = "wasm")]
        media_store_integration_tests!();

        #[cfg(target_family = "wasm")]
        media_store_integration_tests_time!();
    }
}
