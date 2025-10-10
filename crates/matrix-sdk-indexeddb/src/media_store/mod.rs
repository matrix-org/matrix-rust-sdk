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

// Allow dead code here, as this module is still in the process
// of being developed, so some functions will be used later on.
// Once development is complete, we can remove this line and
// clean up any unused code.
#![allow(dead_code)]

mod builder;
mod error;
mod migrations;
mod serializer;
mod transaction;
mod types;
use std::{rc::Rc, time::Duration};

pub use builder::IndexeddbMediaStoreBuilder;
pub use error::IndexeddbMediaStoreError;
use indexed_db_futures::{cursor::CursorDirection, database::Database, Build};
use matrix_sdk_base::{
    media::{
        store::{
            IgnoreMediaRetentionPolicy, MediaRetentionPolicy, MediaService, MediaStore,
            MediaStoreInner, MemoryMediaStore,
        },
        MediaRequestParameters,
    },
    timer,
};
use ruma::{time::SystemTime, MilliSecondsSinceUnixEpoch, MxcUri};
use tracing::instrument;
use web_sys::IdbTransactionMode;

use crate::{
    media_store::{
        transaction::IndexeddbMediaStoreTransaction,
        types::{Lease, Media, MediaCleanupTime, MediaMetadata, UnixTime},
    },
    serializer::{Indexed, IndexedTypeSerializer},
    transaction::TransactionError,
};

/// A type for providing an IndexedDB implementation of [`MediaStore`][1].
/// This is meant to be used as a backend to [`MediaStore`][1] in browser
/// contexts.
///
/// [1]: matrix_sdk_base::media::store::MediaStore
#[derive(Debug, Clone)]
pub struct IndexeddbMediaStore {
    // A handle to the IndexedDB database
    inner: Rc<Database>,
    // A serializer with functionality tailored to `IndexeddbMediaStore`
    serializer: IndexedTypeSerializer,
    // A service for conveniently delegating media-related queries to an `MediaStoreInner`
    // implementation
    media_service: MediaService,
    // An in-memory store for providing temporary implementations for
    // functions of `MediaStore`.
    //
    // NOTE: This will be removed once we have IndexedDB-backed implementations for all
    // functions in `MediaStore`.
    memory_store: MemoryMediaStore,
}

impl IndexeddbMediaStore {
    /// Provides a type with which to conveniently build an
    /// [`IndexeddbMediaStore`]
    pub fn builder() -> IndexeddbMediaStoreBuilder {
        IndexeddbMediaStoreBuilder::default()
    }

    /// Initializes a new transaction on the underlying IndexedDB database and
    /// returns a handle which can be used to combine database operations
    /// into an atomic unit.
    pub fn transaction<'a>(
        &'a self,
        stores: &[&str],
        mode: IdbTransactionMode,
    ) -> Result<IndexeddbMediaStoreTransaction<'a>, IndexeddbMediaStoreError> {
        Ok(IndexeddbMediaStoreTransaction::new(
            self.inner
                .transaction(stores)
                .with_mode(mode)
                .build()
                .map_err(TransactionError::from)?,
            &self.serializer,
        ))
    }
}

#[cfg(target_family = "wasm")]
#[async_trait::async_trait(?Send)]
impl MediaStore for IndexeddbMediaStore {
    type Error = IndexeddbMediaStoreError;

    #[instrument(skip(self))]
    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<bool, IndexeddbMediaStoreError> {
        let _timer = timer!("method");

        let now = Duration::from_millis(MilliSecondsSinceUnixEpoch::now().get().into());

        let transaction =
            self.transaction(&[Lease::OBJECT_STORE], IdbTransactionMode::Readwrite)?;

        if let Some(lease) = transaction.get_lease_by_id(key).await? {
            if lease.holder != holder && !lease.has_expired(now) {
                return Ok(false);
            }
        }

        transaction
            .put_lease(&Lease {
                key: key.to_owned(),
                holder: holder.to_owned(),
                expiration: now + Duration::from_millis(lease_duration_ms.into()),
            })
            .await?;
        transaction.commit().await?;
        Ok(true)
    }

    #[instrument(skip_all)]
    async fn add_media_content(
        &self,
        request: &MediaRequestParameters,
        content: Vec<u8>,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), IndexeddbMediaStoreError> {
        let _timer = timer!("method");
        self.media_service.add_media_content(self, request, content, ignore_policy).await
    }

    #[instrument(skip_all)]
    async fn replace_media_key(
        &self,
        from: &MediaRequestParameters,
        to: &MediaRequestParameters,
    ) -> Result<(), IndexeddbMediaStoreError> {
        let _timer = timer!("method");

        let transaction =
            self.transaction(&[Media::OBJECT_STORE], IdbTransactionMode::Readwrite)?;
        if let Some(mut media) = transaction.get_media_by_id(from).await? {
            // delete before adding, in case `from` and `to` generate the same key
            transaction.delete_media_by_id(from).await?;
            media.metadata.request_parameters = to.clone();
            transaction.add_media(&media).await?;
            transaction.commit().await?;
        }
        Ok(())
    }

    #[instrument(skip_all)]
    async fn get_media_content(
        &self,
        request: &MediaRequestParameters,
    ) -> Result<Option<Vec<u8>>, IndexeddbMediaStoreError> {
        let _timer = timer!("method");
        self.media_service.get_media_content(self, request).await
    }

    #[instrument(skip_all)]
    async fn remove_media_content(
        &self,
        request: &MediaRequestParameters,
    ) -> Result<(), IndexeddbMediaStoreError> {
        let _timer = timer!("method");

        let transaction =
            self.transaction(&[Media::OBJECT_STORE], IdbTransactionMode::Readwrite)?;
        transaction.delete_media_by_id(request).await?;
        transaction.commit().await.map_err(Into::into)
    }

    #[instrument(skip(self))]
    async fn get_media_content_for_uri(
        &self,
        uri: &MxcUri,
    ) -> Result<Option<Vec<u8>>, IndexeddbMediaStoreError> {
        let _timer = timer!("method");
        self.media_service.get_media_content_for_uri(self, uri).await
    }

    #[instrument(skip(self))]
    async fn remove_media_content_for_uri(
        &self,
        uri: &MxcUri,
    ) -> Result<(), IndexeddbMediaStoreError> {
        let _timer = timer!("method");

        let transaction =
            self.transaction(&[Media::OBJECT_STORE], IdbTransactionMode::Readwrite)?;
        transaction.delete_media_by_uri(uri).await?;
        transaction.commit().await.map_err(Into::into)
    }

    #[instrument(skip_all)]
    async fn set_media_retention_policy(
        &self,
        policy: MediaRetentionPolicy,
    ) -> Result<(), IndexeddbMediaStoreError> {
        let _timer = timer!("method");
        self.media_service.set_media_retention_policy(self, policy).await
    }

    #[instrument(skip_all)]
    fn media_retention_policy(&self) -> MediaRetentionPolicy {
        let _timer = timer!("method");
        self.media_service.media_retention_policy()
    }

    #[instrument(skip_all)]
    async fn set_ignore_media_retention_policy(
        &self,
        request: &MediaRequestParameters,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), IndexeddbMediaStoreError> {
        let _timer = timer!("method");
        self.media_service.set_ignore_media_retention_policy(self, request, ignore_policy).await
    }

    #[instrument(skip_all)]
    async fn clean(&self) -> Result<(), IndexeddbMediaStoreError> {
        let _timer = timer!("method");
        self.media_service.clean(self).await
    }
}

#[cfg(target_family = "wasm")]
#[async_trait::async_trait(?Send)]
impl MediaStoreInner for IndexeddbMediaStore {
    type Error = IndexeddbMediaStoreError;

    #[instrument(skip_all)]
    async fn media_retention_policy_inner(
        &self,
    ) -> Result<Option<MediaRetentionPolicy>, IndexeddbMediaStoreError> {
        let _timer = timer!("method");
        self.transaction(&[MediaRetentionPolicy::OBJECT_STORE], IdbTransactionMode::Readonly)?
            .get_media_retention_policy()
            .await
            .map_err(Into::into)
    }

    #[instrument(skip_all)]
    async fn set_media_retention_policy_inner(
        &self,
        policy: MediaRetentionPolicy,
    ) -> Result<(), IndexeddbMediaStoreError> {
        let _timer = timer!("method");
        self.transaction(&[MediaRetentionPolicy::OBJECT_STORE], IdbTransactionMode::Readwrite)?
            .put_item(&policy)
            .await
            .map_err(Into::into)
    }

    #[instrument(skip_all)]
    async fn add_media_content_inner(
        &self,
        request: &MediaRequestParameters,
        content: Vec<u8>,
        current_time: SystemTime,
        policy: MediaRetentionPolicy,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), IndexeddbMediaStoreError> {
        let _timer = timer!("method");

        let transaction =
            self.transaction(&[Media::OBJECT_STORE], IdbTransactionMode::Readwrite)?;

        let media = Media {
            metadata: MediaMetadata {
                request_parameters: request.clone(),
                last_access: current_time.into(),
                ignore_policy,
            },
            content,
        };

        transaction.put_media_if_policy_compliant(&media, policy).await?;
        transaction.commit().await.map_err(Into::into)
    }

    #[instrument(skip_all)]
    async fn set_ignore_media_retention_policy_inner(
        &self,
        request: &MediaRequestParameters,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), IndexeddbMediaStoreError> {
        let _timer = timer!("method");
        self.memory_store
            .set_ignore_media_retention_policy_inner(request, ignore_policy)
            .await
            .map_err(IndexeddbMediaStoreError::MemoryStore)
    }

    #[instrument(skip_all)]
    async fn get_media_content_inner(
        &self,
        request: &MediaRequestParameters,
        current_time: SystemTime,
    ) -> Result<Option<Vec<u8>>, IndexeddbMediaStoreError> {
        let _timer = timer!("method");

        let transaction =
            self.transaction(&[Media::OBJECT_STORE], IdbTransactionMode::Readwrite)?;
        let media = transaction.access_media_by_id(request, current_time).await?;
        transaction.commit().await?;
        Ok(media.map(|m| m.content))
    }

    #[instrument(skip_all)]
    async fn get_media_content_for_uri_inner(
        &self,
        uri: &MxcUri,
        current_time: SystemTime,
    ) -> Result<Option<Vec<u8>>, IndexeddbMediaStoreError> {
        let _timer = timer!("method");

        let transaction =
            self.transaction(&[Media::OBJECT_STORE], IdbTransactionMode::Readwrite)?;
        let media = transaction.access_media_by_uri(uri, current_time).await?.pop();
        transaction.commit().await?;
        Ok(media.map(|m| m.content))
    }

    #[instrument(skip_all)]
    async fn clean_inner(
        &self,
        policy: MediaRetentionPolicy,
        current_time: SystemTime,
    ) -> Result<(), IndexeddbMediaStoreError> {
        let _timer = timer!("method");

        if !policy.has_limitations() {
            return Ok(());
        }

        let transaction = self.transaction(
            &[Media::OBJECT_STORE, MediaCleanupTime::OBJECT_STORE],
            IdbTransactionMode::Readwrite,
        )?;

        let ignore_policy = IgnoreMediaRetentionPolicy::No;
        let current_time = UnixTime::from(current_time);

        if let Some(max_file_size) = policy.computed_max_file_size() {
            transaction
                .delete_media_by_content_size_greater_than(ignore_policy, max_file_size as usize)
                .await?;
        }

        if let Some(expiry) = policy.last_access_expiry {
            transaction
                .delete_media_by_last_access_earlier_than(ignore_policy, current_time - expiry)
                .await?;
        }

        if let Some(max_cache_size) = policy.max_cache_size {
            let cache_size = transaction
                .get_cache_size(ignore_policy)
                .await?
                .ok_or(Self::Error::CacheSizeTooBig)?;
            if cache_size > (max_cache_size as usize) {
                let (_, upper_key) = transaction
                    .fold_media_keys_by_retention_metadata_while(
                        CursorDirection::Prev,
                        ignore_policy,
                        0usize,
                        |total, key| {
                            web_sys::console::log_1(&format!("total={total}, key={key:?}").into());
                            match total.checked_add(key.content_size()) {
                                None => None,
                                Some(total) if total > max_cache_size as usize => None,
                                Some(total) => Some(total),
                            }
                        },
                    )
                    .await?;
                if let Some(upper_key) = upper_key {
                    transaction
                        .delete_media_by_retention_metadata_to(
                            upper_key.ignore_policy(),
                            upper_key.last_access(),
                            upper_key.content_size(),
                        )
                        .await?;
                }
            }
        }

        transaction.put_media_cleanup_time(current_time).await?;
        transaction.commit().await.map_err(Into::into)
    }

    #[instrument(skip_all)]
    async fn last_media_cleanup_time_inner(
        &self,
    ) -> Result<Option<SystemTime>, IndexeddbMediaStoreError> {
        let _timer = timer!("method");
        let time = self
            .transaction(&[MediaCleanupTime::OBJECT_STORE], IdbTransactionMode::Readonly)?
            .get_media_cleanup_time()
            .await?;
        Ok(time.map(Into::into))
    }
}

#[cfg(all(test, target_family = "wasm"))]
mod tests {
    use matrix_sdk_base::{
        media::store::MediaStoreError, media_store_integration_tests,
        media_store_integration_tests_time,
    };
    use uuid::Uuid;

    use crate::media_store::IndexeddbMediaStore;

    mod unencrypted {
        use super::*;

        wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

        async fn get_media_store() -> Result<IndexeddbMediaStore, MediaStoreError> {
            let name = format!("test-media-store-{}", Uuid::new_v4().as_hyphenated());
            Ok(IndexeddbMediaStore::builder().database_name(name).build().await?)
        }

        #[cfg(target_family = "wasm")]
        media_store_integration_tests!();

        #[cfg(target_family = "wasm")]
        media_store_integration_tests_time!();
    }

    mod encrypted {
        use super::*;

        wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

        async fn get_media_store() -> Result<IndexeddbMediaStore, MediaStoreError> {
            let name = format!("test-media-store-{}", Uuid::new_v4().as_hyphenated());
            Ok(IndexeddbMediaStore::builder().database_name(name).build().await?)
        }

        #[cfg(target_family = "wasm")]
        media_store_integration_tests!();

        #[cfg(target_family = "wasm")]
        media_store_integration_tests_time!();
    }
}
