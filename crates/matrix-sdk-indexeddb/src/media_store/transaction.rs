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

use indexed_db_futures::{cursor::CursorDirection, transaction as inner};
use matrix_sdk_base::media::{
    MediaRequestParameters,
    store::{IgnoreMediaRetentionPolicy, MediaRetentionPolicy},
};
use ruma::MxcUri;
use uuid::Uuid;

use crate::{
    media_store::{
        serializer::indexed_types::{
            IndexedCoreIdKey, IndexedLease, IndexedLeaseIdKey, IndexedMediaCleanupTime,
            IndexedMediaContent, IndexedMediaContentIdKey, IndexedMediaMetadata,
            IndexedMediaMetadataContentSizeKey, IndexedMediaMetadataIdKey,
            IndexedMediaMetadataLastAccessKey, IndexedMediaMetadataRetentionKey,
            IndexedMediaMetadataUriKey,
        },
        types::{Lease, Media, MediaCleanupTime, MediaContent, MediaMetadata, UnixTime},
    },
    serializer::indexed_type::{
        IndexedTypeSerializer, range::IndexedKeyRange, traits::IndexedPrefixKeyComponentBounds,
    },
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
    pub fn new(transaction: inner::Transaction<'a>, serializer: &'a IndexedTypeSerializer) -> Self {
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
    /// exists, it will be overwritten. When the item is successfully put, the
    /// function returns the intermediary type [`IndexedLease`] in case
    /// inspection is needed.
    pub async fn put_lease(&self, lease: &Lease) -> Result<IndexedLease, TransactionError> {
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

    /// Query IndexedDB for the stored [`MediaCleanupTime`]
    pub async fn get_media_cleanup_time(
        &self,
    ) -> Result<Option<MediaCleanupTime>, TransactionError> {
        self.transaction.get_item_by_key_components::<MediaCleanupTime, IndexedCoreIdKey>(()).await
    }

    /// Puts a media clean up time into IndexedDB. If one already exists, it
    /// will be overwritten. When the item is successfully put, the
    /// function returns the intermediary type [`IndexedMediaCLeanupTime`] in
    /// case inspection is needed.
    pub async fn put_media_cleanup_time(
        &self,
        time: impl Into<MediaCleanupTime>,
    ) -> Result<IndexedMediaCleanupTime, TransactionError> {
        let time: MediaCleanupTime = time.into();
        self.transaction.put_item(&time).await
    }

    /// Query IndexedDB for [`MediaMetadata`] and [`MediaContent`] that matches
    /// the given [`MediaRequestParameters`]. If an item is found, update
    /// [`MediaMetadata::last_access`] using `current_time`. If more than one
    /// item is found, an error is returned.
    pub async fn access_media_by_id(
        &self,
        request_parameters: &MediaRequestParameters,
        current_time: impl Into<UnixTime>,
    ) -> Result<Option<Media>, TransactionError> {
        if let Some(metadata) =
            self.access_media_metadata_by_id(request_parameters, current_time).await?
        {
            let content = self
                .get_media_content_by_id(metadata.content_id)
                .await?
                .ok_or(TransactionError::ItemNotFound)?;
            Ok(Some(Media {
                request_parameters: metadata.request_parameters,
                last_access: metadata.last_access,
                ignore_policy: metadata.ignore_policy,
                content: content.data,
            }))
        } else {
            Ok(None)
        }
    }

    /// Query IndexedDB for [`MediaMetadata`] and [`MediaContent`] that matches
    /// the given [`MxcUri`]. If an item is found, update
    /// [`MediaMetadata::last_access`] using `current_time`. If more than
    /// one item is found, an error is returned.
    pub async fn access_media_by_uri(
        &self,
        uri: &MxcUri,
        current_time: impl Into<UnixTime>,
    ) -> Result<Vec<Media>, TransactionError> {
        let mut medias = Vec::new();
        for metadata in self.access_media_metadata_by_uri(uri, current_time).await? {
            let content = self
                .get_media_content_by_id(metadata.content_id)
                .await?
                .ok_or(TransactionError::ItemNotFound)?;
            medias.push(Media {
                request_parameters: metadata.request_parameters,
                last_access: metadata.last_access,
                ignore_policy: metadata.ignore_policy,
                content: content.data,
            });
        }
        Ok(medias)
    }

    /// Query IndexedDB for the size recorded in each
    /// [`MediaMetadata::content_size`] which match
    /// the given [`IgnoreMediaRetentionPolicy`]. Returns the sum of all sizes
    /// or [`None`] if the size of the cache overflows [`usize::MAX`].
    ///
    /// Note that this operation is not constant, but rather iterates over all
    /// keys and extracts the content size from each key.
    pub async fn get_cache_size(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<Option<usize>, TransactionError> {
        Ok(self
            .get_all_media_metadata_keys_by_content_size(ignore_policy)
            .await?
            .iter()
            .try_fold(0usize, |size, key| size.checked_add(key.content_size())))
    }

    /// Adds [`MediaMetadata`] and [`MediaContent`] to IndexedDB if the size of
    /// [`IndexedMediaContent::content`] does not exceed
    /// [`MediaRetentionPolicy::max_file_size]. If an item with the same key
    /// already exists, it will be overwritten.  When the item is
    /// successfully put, the function returns the intermediary types
    /// [`IndexedMediaMetadata`] and [`IndexedMediaContent`] in case inspection
    /// is needed.
    pub async fn put_media_if_policy_compliant(
        &self,
        media: Media,
        policy: MediaRetentionPolicy,
    ) -> Result<Option<(IndexedMediaMetadata, IndexedMediaContent)>, TransactionError> {
        let content_id = match self.get_media_metadata_by_id(&media.request_parameters).await? {
            Some(metadata) => metadata.content_id,
            None => Uuid::new_v4(),
        };
        let content = MediaContent { content_id, data: media.content };
        let option = if media.ignore_policy.is_yes() {
            self.put_media_content(&content).await.map(Some)?
        } else {
            self.put_media_content_if_policy_compliant(&content, policy).await?
        };
        if let Some(indexed_content) = option {
            let indexed_metadata = self
                .put_media_metadata(&MediaMetadata {
                    request_parameters: media.request_parameters,
                    last_access: media.last_access,
                    ignore_policy: media.ignore_policy,
                    content_id,
                    content_size: indexed_content.content.len(),
                })
                .await?;
            Ok(Some((indexed_metadata, indexed_content)))
        } else {
            Ok(None)
        }
    }

    /// Delete [`MediaMetadata`] and [`MediaContent`] that matches the given
    /// [`MediaRequestParameters`] from IndexedDB
    pub async fn delete_media_by_id(
        &self,
        request_parameters: &MediaRequestParameters,
    ) -> Result<(), TransactionError> {
        if let Some(metadata) = self.get_media_metadata_by_id(request_parameters).await? {
            self.delete_media_content_by_id(metadata.content_id).await?;
        }
        self.delete_media_metadata_by_id(request_parameters).await
    }

    /// Delete [`MediaMetadata`] and [`MediaContent`] that matches the given
    /// [`MxcUri`] from IndexedDB
    pub async fn delete_media_by_uri(&self, uri: &MxcUri) -> Result<(), TransactionError> {
        for metadata in self.get_media_metadata_by_uri(uri).await? {
            self.delete_media_content_by_id(metadata.content_id).await?;
        }
        self.delete_media_metadata_by_uri(uri).await
    }

    /// Delete [`MediaMetadata`] and [`MediaContent`] that matches the given
    /// [`IgnoreMediaRetentionPolicy`] and the given content size range from
    /// IndexedDB
    pub async fn delete_media_by_content_size(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        content_size: impl Into<IndexedKeyRange<usize>>,
    ) -> Result<(), TransactionError> {
        let range = content_size.into();
        for key in self.get_media_metadata_keys_by_content_size(ignore_policy, range).await? {
            self.delete_media_content_by_id(key.content_id()).await?;
        }
        self.delete_media_metadata_by_content_size(ignore_policy, range).await
    }

    /// Delete [`MediaMetadata`] and [`MediaContent`] that matches the given
    /// [`IgnoreMediaRetentionPolicy`] and is strictly larger than the given
    /// content size from IndexedDB
    pub async fn delete_media_by_content_size_greater_than(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        content_size: usize,
    ) -> Result<(), TransactionError> {
        let (_, upper, _) =
            IndexedMediaMetadataContentSizeKey::upper_key_components_with_prefix(ignore_policy);
        self.delete_media_by_content_size(ignore_policy, (content_size + 1, upper)).await
    }

    /// Delete [`MediaMetadata`] and [`MediaContent`] that matches the given
    /// [`IgnoreMediaRetentionPolicy`] and the given last access time range
    /// from IndexedDB
    pub async fn delete_media_by_last_access(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        last_access: impl Into<IndexedKeyRange<UnixTime>>,
    ) -> Result<(), TransactionError> {
        let range = last_access.into();
        for key in self.get_media_metadata_keys_by_last_access(ignore_policy, range).await? {
            self.delete_media_content_by_id(key.content_id()).await?;
        }
        self.delete_media_metadata_by_last_access(ignore_policy, range).await
    }

    /// Delete [`MediaMetadata`] and [`MediaContent`] that matches the given
    /// [`IgnoreMediaRetentionPolicy`] and is earlier than the given last
    /// access time from IndexedDB
    pub async fn delete_media_by_last_access_earlier_than(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        time: UnixTime,
    ) -> Result<(), TransactionError> {
        let (_, lower, _) =
            IndexedMediaMetadataLastAccessKey::lower_key_components_with_prefix(ignore_policy);
        self.delete_media_by_last_access(ignore_policy, (lower, time)).await
    }

    /// Delete [`MediaMetadata`] and [`MediaContent`] that matches the given
    /// [`IgnoreMediaRetentionPolicy`] and the given last access time and
    /// content size range from IndexedDB
    pub async fn delete_media_by_retention_metadata(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        range: impl Into<IndexedKeyRange<(UnixTime, usize)>>,
    ) -> Result<(), TransactionError> {
        let range = range.into();
        for key in self.get_media_metadata_keys_by_retention(ignore_policy, range).await? {
            self.delete_media_content_by_id(key.content_id()).await?;
        }
        self.delete_media_metadata_by_retention(ignore_policy, range).await
    }

    /// Delete [`MediaMetadata`] and [`MediaContent`] that matches the given
    /// [`IgnoreMediaRetentionPolicy`] and is sorted before the given last
    /// access time and content size from IndexedDB
    pub async fn delete_media_by_retention_metadata_to(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        last_access: UnixTime,
        content_size: usize,
    ) -> Result<(), TransactionError> {
        let (_, lower_last_access, lower_content_size, _) =
            IndexedMediaMetadataRetentionKey::lower_key_components_with_prefix(ignore_policy);
        let lower = (lower_last_access, lower_content_size);
        self.delete_media_by_retention_metadata(ignore_policy, (lower, (last_access, content_size)))
            .await
    }

    /// Query IndexedDB for [`MediaMetadata`] that matches the given
    /// [`MediaRequestParameters`]. If more than one item is found, an error
    /// is returned.
    pub async fn get_media_metadata_by_id(
        &self,
        request_parameters: &MediaRequestParameters,
    ) -> Result<Option<MediaMetadata>, TransactionError> {
        self.get_item_by_key_components::<MediaMetadata, IndexedMediaMetadataIdKey>(
            request_parameters,
        )
        .await
    }

    /// Query IndexedDB for [`MediaMetadata`] that matches the given
    /// [`MediaRequestParameters`]. If an item is found, update
    /// [`MediaMetadata::last_access`] using `current_time`. If more than one
    /// item is found, an error is returned.
    pub async fn access_media_metadata_by_id(
        &self,
        request_parameters: &MediaRequestParameters,
        current_time: impl Into<UnixTime>,
    ) -> Result<Option<MediaMetadata>, TransactionError> {
        if let Some(mut media_metadata) = self.get_media_metadata_by_id(request_parameters).await? {
            let last_access = media_metadata.last_access;
            media_metadata.last_access = current_time.into();
            self.put_item(&media_metadata).await?;
            media_metadata.last_access = last_access;
            Ok(Some(media_metadata))
        } else {
            Ok(None)
        }
    }

    /// Query IndexedDB for [`MediaMetadata`] that match the given [`MxcUri`].
    pub async fn get_media_metadata_by_uri(
        &self,
        uri: &MxcUri,
    ) -> Result<Vec<MediaMetadata>, TransactionError> {
        self.get_items_by_key_components::<MediaMetadata, IndexedMediaMetadataUriKey>(uri).await
    }

    /// Query IndexedDB for [`MediaMetadata`] that matches the given
    /// [`MxcUri`]. If an item is found, update [`MediaMetadata::last_access`]
    /// using `current_time`. If more than one item is found, an error
    /// is returned.
    pub async fn access_media_metadata_by_uri(
        &self,
        uri: &MxcUri,
        current_time: impl Into<UnixTime>,
    ) -> Result<Vec<MediaMetadata>, TransactionError> {
        let current_time = current_time.into();
        let mut media_metadatas = Vec::new();
        for mut media_metadata in self.get_media_metadata_by_uri(uri).await? {
            let last_access = media_metadata.last_access;
            media_metadata.last_access = current_time;
            self.put_item(&media_metadata).await?;
            media_metadata.last_access = last_access;
            media_metadatas.push(media_metadata);
        }
        Ok(media_metadatas)
    }

    /// Query IndexedDB for [content size](IndexedMediaMetadataContentSizeKey)
    /// keys whose associated [`MediaMetadata`] matches the given
    /// [`IgnoreMediaRetentionPolicy`] and content size range.
    pub async fn get_media_metadata_keys_by_content_size(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        content_size: impl Into<IndexedKeyRange<usize>>,
    ) -> Result<Vec<IndexedMediaMetadataContentSizeKey>, TransactionError> {
        let range = Into::<IndexedKeyRange<usize>>::into(content_size)
            .map(|last_access| (ignore_policy, last_access))
            .into_prefix(self.serializer().inner());
        self.get_keys::<MediaMetadata, IndexedMediaMetadataContentSizeKey>(range).await
    }

    /// Query IndexedDB for all [content
    /// size](IndexedMediaMetadataContentSizeKey) keys whose associated
    /// [`MediaMetadata`] matches the given [`IgnoreMediaRetentionPolicy`].
    pub async fn get_all_media_metadata_keys_by_content_size(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<Vec<IndexedMediaMetadataContentSizeKey>, TransactionError> {
        let (_, lower, _) =
            IndexedMediaMetadataContentSizeKey::lower_key_components_with_prefix(ignore_policy);
        let (_, upper, _) =
            IndexedMediaMetadataContentSizeKey::upper_key_components_with_prefix(ignore_policy);
        self.get_media_metadata_keys_by_content_size(ignore_policy, (lower, upper)).await
    }

    /// Query IndexedDB for [last access](IndexedMediaMetadataLastAccessKey)
    /// keys whose associated [`MediaMetadata`] matches the given
    /// [`IgnoreMediaRetentionPolicy`] and last access time range.
    pub async fn get_media_metadata_keys_by_last_access(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        last_access: impl Into<IndexedKeyRange<UnixTime>>,
    ) -> Result<Vec<IndexedMediaMetadataLastAccessKey>, TransactionError> {
        let range = Into::<IndexedKeyRange<UnixTime>>::into(last_access)
            .map(|last_access| (ignore_policy, last_access))
            .into_prefix(self.serializer().inner());
        self.get_keys::<MediaMetadata, IndexedMediaMetadataLastAccessKey>(range).await
    }

    /// Query IndexedDB for [retention](IndexedMediaMetadataRetentionKey)
    /// keys whose associated [`MediaMetadata`] matches the given
    /// [`IgnoreMediaRetentionPolicy`] and last access time and content size
    /// range.
    pub async fn get_media_metadata_keys_by_retention(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        range: impl Into<IndexedKeyRange<(UnixTime, usize)>>,
    ) -> Result<Vec<IndexedMediaMetadataRetentionKey>, TransactionError> {
        let range = Into::<IndexedKeyRange<(UnixTime, usize)>>::into(range)
            .map(|(last_access, content_size)| (ignore_policy, last_access, content_size))
            .into_prefix(self.serializer().inner());
        self.get_keys::<MediaMetadata, IndexedMediaMetadataRetentionKey>(range).await
    }

    /// Query IndexedDB for [retention metadata][1] keys that match the given
    /// key range. Iterate over the keys in the given
    /// [`direction`](CursorDirection) using a cursor and fold them into an
    /// accumulator while the given function `f` returns [`Some`].
    ///
    /// This function returns the final value of the accumulator and the key, if
    /// any, which caused the fold to short circuit.
    ///
    /// Note that the use of cursor means that keys are read lazily from
    /// IndexedDB.
    ///
    /// [1]: crate::media_store::serializer::indexed_types::IndexedMediaMetadataRetentionKey
    pub async fn fold_media_metadata_keys_by_retention_while<Acc, F>(
        &self,
        direction: CursorDirection,
        ignore_policy: IgnoreMediaRetentionPolicy,
        init: Acc,
        f: F,
    ) -> Result<(Acc, Option<IndexedMediaMetadataRetentionKey>), TransactionError>
    where
        F: FnMut(&Acc, &IndexedMediaMetadataRetentionKey) -> Option<Acc>,
    {
        self.fold_keys_while::<MediaMetadata, IndexedMediaMetadataRetentionKey, Acc, F>(
            direction,
            IndexedKeyRange::all_with_prefix(ignore_policy, self.serializer().inner()),
            init,
            f,
        )
        .await
    }

    /// Adds [`MediaMetadata`] to IndexedDB. If an item with the same key
    /// already exists, it will be rejected. When the item is successfully
    /// added, the function returns the intermediary type
    /// [`IndexedMediaMetadata`] in case inspection is needed.
    pub async fn add_media_metadata(
        &self,
        media_metadata: &MediaMetadata,
    ) -> Result<IndexedMediaMetadata, TransactionError> {
        self.add_item(media_metadata).await
    }

    /// Puts [`MediaMetadata`] in IndexedDB object. If an item with the same key
    /// already exists, it will be overwritten. When the item is successfully
    /// put, the function returns the intermediary type
    /// [`IndexedMediaMetadata`] in case inspection is needed.
    pub async fn put_media_metadata(
        &self,
        media_metadata: &MediaMetadata,
    ) -> Result<IndexedMediaMetadata, TransactionError> {
        self.put_item(media_metadata).await
    }

    /// Delete [`MediaMetadata`] that match the given [`MediaRequestParameters`]
    /// from IndexedDB
    pub async fn delete_media_metadata_by_id(
        &self,
        request_parameters: &MediaRequestParameters,
    ) -> Result<(), TransactionError> {
        self.delete_item_by_key::<MediaMetadata, IndexedMediaMetadataIdKey>(request_parameters)
            .await
    }

    /// Delete [`MediaMetadata`] that matches the given [`MxcUri`]
    /// from IndexedDB
    pub async fn delete_media_metadata_by_uri(
        &self,
        source: &MxcUri,
    ) -> Result<(), TransactionError> {
        self.delete_item_by_key::<MediaMetadata, IndexedMediaMetadataUriKey>(source).await
    }

    /// Delete [`MediaMetadata`] that matches the given
    /// [`IgnoreMediaRetentionPolicy`] and the given content size range from
    /// IndexedDB
    pub async fn delete_media_metadata_by_content_size(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        content_size: impl Into<IndexedKeyRange<usize>>,
    ) -> Result<(), TransactionError> {
        let range = Into::<IndexedKeyRange<usize>>::into(content_size)
            .map(|size| (ignore_policy, size))
            .into_prefix(self.serializer().inner());
        self.delete_items_by_key::<MediaMetadata, IndexedMediaMetadataContentSizeKey>(range).await
    }

    /// Delete [`MediaMetadata`] that matches the given
    /// [`IgnoreMediaRetentionPolicy`] and is strictly larger than the given
    /// content size from IndexedDB
    pub async fn delete_media_metadata_by_content_size_greater_than(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        content_size: usize,
    ) -> Result<(), TransactionError> {
        let (_, upper, _) =
            IndexedMediaMetadataContentSizeKey::upper_key_components_with_prefix(ignore_policy);
        self.delete_media_metadata_by_content_size(ignore_policy, (content_size + 1, upper)).await
    }

    /// Delete [`MediaMetadata`] that matches the given
    /// [`IgnoreMediaRetentionPolicy`] and the given last access time range
    /// from IndexedDB
    pub async fn delete_media_metadata_by_last_access(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        last_access: impl Into<IndexedKeyRange<UnixTime>>,
    ) -> Result<(), TransactionError> {
        let range = Into::<IndexedKeyRange<UnixTime>>::into(last_access)
            .map(|last_access| (ignore_policy, last_access))
            .into_prefix(self.serializer().inner());
        self.delete_items_by_key::<MediaMetadata, IndexedMediaMetadataLastAccessKey>(range).await
    }

    /// Delete [`MediaMetadata`] that matches the given
    /// [`IgnoreMediaRetentionPolicy`] and is earlier than the given last
    /// access time from IndexedDB
    pub async fn delete_media_metadata_by_last_access_earlier_than(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        time: UnixTime,
    ) -> Result<(), TransactionError> {
        let (_, lower, _) =
            IndexedMediaMetadataLastAccessKey::lower_key_components_with_prefix(ignore_policy);
        self.delete_media_metadata_by_last_access(ignore_policy, (lower, time)).await
    }

    /// Delete [`MediaMetadata`] that matches the given
    /// [`IgnoreMediaRetentionPolicy`] and the given last access time and
    /// content size range from IndexedDB
    pub async fn delete_media_metadata_by_retention(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        range: impl Into<IndexedKeyRange<(UnixTime, usize)>>,
    ) -> Result<(), TransactionError> {
        let range = Into::<IndexedKeyRange<(UnixTime, usize)>>::into(range)
            .map(|(last_access, content_size)| (ignore_policy, last_access, content_size))
            .into_prefix(self.serializer().inner());
        self.delete_items_by_key::<MediaMetadata, IndexedMediaMetadataRetentionKey>(range).await
    }

    /// Delete [`MediaMetadata`] that matches the given
    /// [`IgnoreMediaRetentionPolicy`] and is sorted before the given last
    /// access time and content size from IndexedDB
    pub async fn delete_media_metadata_by_retention_to(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        last_access: UnixTime,
        content_size: usize,
    ) -> Result<(), TransactionError> {
        let (_, lower_last_access, lower_content_size, _) =
            IndexedMediaMetadataRetentionKey::lower_key_components_with_prefix(ignore_policy);
        let lower = (lower_last_access, lower_content_size);
        self.delete_media_metadata_by_retention(ignore_policy, (lower, (last_access, content_size)))
            .await
    }

    /// Query IndexedDB for [`Media`] that matches the given
    /// identifier. If more than one item is found, an error
    /// is returned.
    pub async fn get_media_content_by_id(
        &self,
        id: Uuid,
    ) -> Result<Option<MediaContent>, TransactionError> {
        self.get_item_by_key_components::<MediaContent, IndexedMediaContentIdKey>(id).await
    }

    /// Adds [`MediaContent`] to IndexedDB. If an item with the same key already
    /// exists, it will be rejected. When the item is successfully added, the
    /// function returns the intermediary type [`IndexedMediaContent`] in case
    /// inspection is needed.
    pub async fn add_media_content(
        &self,
        content: &MediaContent,
    ) -> Result<IndexedMediaContent, TransactionError> {
        self.add_item(content).await
    }

    /// Puts [`MediaContent`] in IndexedDB object. If an item with the same key
    /// already exists, it will be overwritten. When the item is successfully
    /// put, the function returns the intermediary type
    /// [`IndexedMediaContent`] in case inspection is needed.
    pub async fn put_media_content(
        &self,
        content: &MediaContent,
    ) -> Result<IndexedMediaContent, TransactionError> {
        self.put_item(content).await
    }

    /// Adds [`MediaContent`] to IndexedDB if the size of
    /// [`IndexedMediaContent::content`] does not exceed
    /// [`MediaRetentionPolicy::max_file_size]. If an item with the same key
    /// already exists, it will be overwritten. When the item is successfully
    /// put, the function returns the intermediary type
    /// [`IndexedMediaContent`] in case inspection is needed.
    pub async fn put_media_content_if_policy_compliant(
        &self,
        media: &MediaContent,
        policy: MediaRetentionPolicy,
    ) -> Result<Option<IndexedMediaContent>, TransactionError> {
        self.put_item_if(media, |indexed| {
            !policy.exceeds_max_file_size(indexed.content.len() as u64)
        })
        .await
    }

    /// Delete [`MediaContent`] that match the given identifier from IndexedDB
    pub async fn delete_media_content_by_id(&self, id: Uuid) -> Result<(), TransactionError> {
        self.delete_item_by_key::<MediaContent, IndexedMediaContentIdKey>(id).await
    }
}
