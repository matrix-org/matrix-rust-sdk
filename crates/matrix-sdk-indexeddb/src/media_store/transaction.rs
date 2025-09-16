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

use std::ops::Deref;

use indexed_db_futures::prelude::IdbTransaction;
use matrix_sdk_base::media::{store::MediaRetentionPolicy, MediaRequestParameters};
use ruma::MxcUri;

use crate::{
    media_store::{
        serializer::indexed_types::{
            IndexedCoreIdKey, IndexedLeaseIdKey, IndexedMediaIdKey, IndexedMediaUriKey,
        },
        types::{Lease, Media, UnixTime},
    },
    serializer::IndexedTypeSerializer,
    transaction::{Transaction, TransactionError},
};

/// Represents an IndexedDB transaction, but provides a convenient interface for
/// performing operations relevant to the IndexedDB implementation of
/// [`MediaStore`](matrix_sdk_base::media::store::MediaStore).
pub struct IndexeddbMediaStoreTransaction<'a> {
    transaction: Transaction<'a>,
}

impl<'a> Deref for IndexeddbMediaStoreTransaction<'a> {
    type Target = Transaction<'a>;

    fn deref(&self) -> &Self::Target {
        &self.transaction
    }
}

impl<'a> IndexeddbMediaStoreTransaction<'a> {
    pub fn new(transaction: IdbTransaction<'a>, serializer: &'a IndexedTypeSerializer) -> Self {
        Self { transaction: Transaction::new(transaction, serializer) }
    }

    /// Returns the underlying IndexedDB transaction.
    pub fn into_inner(self) -> Transaction<'a> {
        self.transaction
    }

    /// Commit all operations tracked in this transaction to IndexedDB.
    pub async fn commit(self) -> Result<(), TransactionError> {
        self.transaction.commit().await
    }

    /// Query IndexedDB for the lease that matches the given key `id`. If more
    /// than one lease is found, an error is returned.
    pub async fn get_lease_by_id(&self, id: &str) -> Result<Option<Lease>, TransactionError> {
        self.transaction.get_item_by_key_components::<Lease, IndexedLeaseIdKey>(id).await
    }

    /// Puts a lease into IndexedDB. If a media with the same key already
    /// exists, it will be overwritten.
    pub async fn put_lease(&self, lease: &Lease) -> Result<(), TransactionError> {
        self.transaction.put_item(lease).await
    }

    /// Query IndexedDB for the stored [`MediaRetentionPolicy`]
    pub async fn get_media_retention_policy(
        &self,
    ) -> Result<Option<MediaRetentionPolicy>, TransactionError> {
        self.transaction
            .get_item_by_key_components::<MediaRetentionPolicy, IndexedCoreIdKey>(())
            .await
    }

    /// Query IndexedDB for [`Media`] that matches the given
    /// [`MediaRequestParameters`]. If more than one item is found, an error
    /// is returned.
    pub async fn get_media_by_id(
        &self,
        request_parameters: &MediaRequestParameters,
    ) -> Result<Option<Media>, TransactionError> {
        self.get_item_by_key_components::<Media, IndexedMediaIdKey>(request_parameters).await
    }

    /// Query IndexedDB for [`Media`] that matches the given
    /// [`MediaRequestParameters`]. If an item is found, update
    /// [`Media::last_access`] using `current_time`. If more than one item
    /// is found, an error is returned.
    pub async fn access_media_by_id(
        &self,
        request_parameters: &MediaRequestParameters,
        current_time: impl Into<UnixTime>,
    ) -> Result<Option<Media>, TransactionError> {
        if let Some(mut media) = self.get_media_by_id(request_parameters).await? {
            let last_access = media.metadata.last_access;
            media.metadata.last_access = current_time.into();
            self.put_item(&media).await?;
            media.metadata.last_access = last_access;
            Ok(Some(media))
        } else {
            Ok(None)
        }
    }

    /// Query IndexedDB for [`Media`] that match the given [`MxcUri`].
    pub async fn get_media_by_uri(&self, uri: &MxcUri) -> Result<Vec<Media>, TransactionError> {
        self.get_items_by_key_components::<Media, IndexedMediaUriKey>(uri).await
    }

    /// Query IndexedDB for [`Media`] that matches the given
    /// [`MxcUri`]. If an item is found, update [`Media::last_access`]
    /// using `current_time`. If more than one item is found, an error
    /// is returned.
    pub async fn access_media_by_uri(
        &self,
        uri: &MxcUri,
        current_time: impl Into<UnixTime>,
    ) -> Result<Vec<Media>, TransactionError> {
        let current_time = current_time.into();
        let mut medias = Vec::new();
        for mut media in self.get_media_by_uri(uri).await? {
            let last_access = media.metadata.last_access;
            media.metadata.last_access = current_time;
            self.put_item(&media).await?;
            media.metadata.last_access = last_access;
            medias.push(media);
        }
        Ok(medias)
    }

    /// Adds [`Media`] to IndexedDB. If an item with the same key already
    /// exists, it will be rejected.
    pub async fn add_media(&self, media: &Media) -> Result<(), TransactionError> {
        self.add_item(media).await
    }

    /// Adds [`Media`] to IndexedDB if the size of [`IndexedMedia::content`][1]
    /// does not exceed [`MediaRetentionPolicy::max_file_size]. If an item with
    /// the same key already exists, it will be overwritten.
    ///
    /// [1]: crate::event_cache_store::serializer::types::IndexedMedia::content
    pub async fn put_media_if_policy_compliant(
        &self,
        media: &Media,
        policy: MediaRetentionPolicy,
    ) -> Result<(), TransactionError> {
        self.put_item_if(media, |indexed| {
            !indexed.content_size.ignore_policy()
                && !policy.exceeds_max_file_size(indexed.content_size.content_size() as u64)
        })
        .await
    }

    /// Delete [`Media`] that match the given [`MediaRequestParameters`]
    /// from IndexedDB
    pub async fn delete_media_by_id(
        &self,
        request_parameters: &MediaRequestParameters,
    ) -> Result<(), TransactionError> {
        self.delete_item_by_key::<Media, IndexedMediaIdKey>(request_parameters).await
    }

    /// Delete [`Media`] that matches the given [`MxcUri`]
    /// from IndexedDB
    pub async fn delete_media_by_uri(&self, source: &MxcUri) -> Result<(), TransactionError> {
        self.delete_item_by_key::<Media, IndexedMediaUriKey>(source).await
    }
}
