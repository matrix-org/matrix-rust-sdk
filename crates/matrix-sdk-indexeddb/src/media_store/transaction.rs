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
    store::{IgnoreMediaRetentionPolicy, MediaRetentionPolicy},
    MediaRequestParameters,
};
use ruma::MxcUri;

use crate::{
    media_store::{
        serializer::indexed_types::{
            IndexedCoreIdKey, IndexedLeaseIdKey, IndexedMedia, IndexedMediaContent,
            IndexedMediaContentIdKey, IndexedMediaContentSizeKey, IndexedMediaIdKey,
            IndexedMediaLastAccessKey, IndexedMediaMetadata, IndexedMediaMetadataContentSizeKey,
            IndexedMediaMetadataIdKey, IndexedMediaMetadataLastAccessKey,
            IndexedMediaMetadataRetentionKey, IndexedMediaMetadataUriKey,
            IndexedMediaRetentionMetadataKey, IndexedMediaUriKey,
        },
        types::{Lease, Media, MediaCleanupTime, MediaContent, MediaMetadata, UnixTime},
    },
    serializer::{IndexedKeyRange, IndexedPrefixKeyComponentBounds, IndexedTypeSerializer},
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

    /// Query IndexedDB for the stored [`MediaCleanupTime`]
    pub async fn get_media_cleanup_time(
        &self,
    ) -> Result<Option<MediaCleanupTime>, TransactionError> {
        self.transaction.get_item_by_key_components::<MediaCleanupTime, IndexedCoreIdKey>(()).await
    }

    /// Puts a media clean up time into IndexedDB. If one already exists, it
    /// will be overwritten.
    pub async fn put_media_cleanup_time(
        &self,
        time: impl Into<MediaCleanupTime>,
    ) -> Result<(), TransactionError> {
        let time: MediaCleanupTime = time.into();
        self.transaction.put_item(&time).await
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
            let last_access = media.last_access;
            media.last_access = current_time.into();
            self.put_item(&media).await?;
            media.last_access = last_access;
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
            let last_access = media.last_access;
            media.last_access = current_time;
            self.put_item(&media).await?;
            media.last_access = last_access;
            medias.push(media);
        }
        Ok(medias)
    }

    /// Query IndexedDB for all [content size](IndexedMediaContentSizeKey) keys
    /// whose associated [`Media`] matches the given
    /// [`IgnoreMediaRetentionPolicy`].
    pub async fn get_media_keys_by_content_size(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<Vec<IndexedMediaContentSizeKey>, TransactionError> {
        self.get_keys::<Media, IndexedMediaContentSizeKey>(IndexedKeyRange::all_with_prefix(
            ignore_policy,
            self.serializer().inner(),
        ))
        .await
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
    /// [1]: IndexedMediaRetentionMetadataKey
    pub async fn fold_media_keys_by_retention_metadata_while<Acc, F>(
        &self,
        direction: CursorDirection,
        ignore_policy: IgnoreMediaRetentionPolicy,
        init: Acc,
        f: F,
    ) -> Result<(Acc, Option<IndexedMediaRetentionMetadataKey>), TransactionError>
    where
        F: FnMut(&Acc, &IndexedMediaRetentionMetadataKey) -> Option<Acc>,
    {
        self.fold_keys_while::<Media, IndexedMediaRetentionMetadataKey, Acc, F>(
            direction,
            IndexedKeyRange::all_with_prefix(ignore_policy, self.serializer().inner()),
            init,
            f,
        )
        .await
    }

    /// Query IndexedDB for the accumulated size of all [`Media`] which match
    /// the given [`IgnoreMediaRetentionPolicy`]. Returns [`None`] if the size
    /// of the cache overflows [`usize::MAX`].
    ///
    /// Note that this operation is not constant, but rather iterates over all
    /// keys and extracts the content size from each key.
    pub async fn get_cache_size(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<Option<usize>, TransactionError> {
        Ok(self
            .get_media_keys_by_content_size(ignore_policy)
            .await?
            .iter()
            .try_fold(0usize, |size, key| size.checked_add(key.content_size())))
    }

    /// Adds [`Media`] to IndexedDB. If an item with the same key already
    /// exists, it will be rejected. When the item is successfully added, the
    /// function returns the intermediary type [`IndexedMedia`] in case
    /// inspection is needed.
    pub async fn add_media(&self, media: &Media) -> Result<IndexedMedia, TransactionError> {
        self.add_item(media).await
    }

    /// Puts [`Media`] in IndexedDB object. If an item with the same key already
    /// exists, it will be overwritten.
    pub async fn put_media(&self, media: &Media) -> Result<(), TransactionError> {
        self.put_item(media).await
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
            indexed.content_size.ignore_policy().is_yes()
                || !policy.exceeds_max_file_size(indexed.content_size.content_size() as u64)
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

    /// Delete [`Media`] that matches the given [`IgnoreMediaRetentionPolicy`]
    /// and the given content size range from IndexedDB
    pub async fn delete_media_by_content_size(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        content_size: impl Into<IndexedKeyRange<usize>>,
    ) -> Result<(), TransactionError> {
        let range = content_size.into().map(|size| (ignore_policy, size));
        self.delete_items_by_key_components::<Media, IndexedMediaContentSizeKey>(range).await
    }

    /// Delete [`Media`] that matches the given [`IgnoreMediaRetentionPolicy`]
    /// and is strictly larger than the given content size from IndexedDB
    pub async fn delete_media_by_content_size_greater_than(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        content_size: usize,
    ) -> Result<(), TransactionError> {
        let (_, upper) =
            IndexedMediaContentSizeKey::upper_key_components_with_prefix(ignore_policy);
        self.delete_media_by_content_size(ignore_policy, (content_size + 1, upper)).await
    }

    /// Delete [`Media`] that matches the given [`IgnoreMediaRetentionPolicy`]
    /// and the given last access time range from IndexedDB
    pub async fn delete_media_by_last_access(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        last_access: impl Into<IndexedKeyRange<UnixTime>>,
    ) -> Result<(), TransactionError> {
        let range = last_access.into().map(|last_access| (ignore_policy, last_access));
        self.delete_items_by_key_components::<Media, IndexedMediaLastAccessKey>(range).await
    }

    /// Delete [`Media`] that matches the given [`IgnoreMediaRetentionPolicy`]
    /// and is earlier than the given last access time from IndexedDB
    pub async fn delete_media_by_last_access_earlier_than(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        time: UnixTime,
    ) -> Result<(), TransactionError> {
        let (_, lower) = IndexedMediaLastAccessKey::lower_key_components_with_prefix(ignore_policy);
        self.delete_media_by_last_access(ignore_policy, (lower, time)).await
    }

    /// Delete [`Media`] that matches the given [`IgnoreMediaRetentionPolicy`]
    /// and the given last access time and content size range from IndexedDB
    pub async fn delete_media_by_retention_metadata(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        range: impl Into<IndexedKeyRange<(UnixTime, usize)>>,
    ) -> Result<(), TransactionError> {
        let range = range
            .into()
            .map(|(last_access, content_size)| (ignore_policy, last_access, content_size));
        self.delete_items_by_key_components::<Media, IndexedMediaRetentionMetadataKey>(range).await
    }

    /// Delete [`Media`] that matches the given [`IgnoreMediaRetentionPolicy`]
    /// and is sorted before the given last access time and content size
    /// from IndexedDB
    pub async fn delete_media_by_retention_metadata_to(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        last_access: UnixTime,
        content_size: usize,
    ) -> Result<(), TransactionError> {
        let (_, lower_last_access, lower_content_size) =
            IndexedMediaRetentionMetadataKey::lower_key_components_with_prefix(ignore_policy);
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

    /// Query IndexedDB for all [content size][1] keys whose associated
    /// [`MediaMetadata`] matches the given [`IgnoreMediaRetentionPolicy`].
    ///
    /// [1]: crate::media_store::serializer::indexed_types::IndexedMediaMetadataContentSizeKey
    pub async fn get_media_metadata_keys_by_content_size(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<Vec<IndexedMediaMetadataContentSizeKey>, TransactionError> {
        self.get_keys::<MediaMetadata, IndexedMediaMetadataContentSizeKey>(
            IndexedKeyRange::all_with_prefix(ignore_policy, self.serializer().inner()),
        )
        .await
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
    /// already exists, it will be overwritten.
    pub async fn put_media_metadata(
        &self,
        media_metadata: &MediaMetadata,
    ) -> Result<(), TransactionError> {
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
        let range = content_size.into().map(|size| (ignore_policy, size));
        self.delete_items_by_key_components::<MediaMetadata, IndexedMediaMetadataContentSizeKey>(
            range,
        )
        .await
    }

    /// Delete [`MediaMetadata`] that matches the given
    /// [`IgnoreMediaRetentionPolicy`] and is strictly larger than the given
    /// content size from IndexedDB
    pub async fn delete_media_metadata_by_content_size_greater_than(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        content_size: usize,
    ) -> Result<(), TransactionError> {
        let (_, upper) =
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
        let range = last_access.into().map(|last_access| (ignore_policy, last_access));
        self.delete_items_by_key_components::<MediaMetadata, IndexedMediaMetadataLastAccessKey>(
            range,
        )
        .await
    }

    /// Delete [`MediaMetadata`] that matches the given
    /// [`IgnoreMediaRetentionPolicy`] and is earlier than the given last
    /// access time from IndexedDB
    pub async fn delete_media_metadata_by_last_access_earlier_than(
        &self,
        ignore_policy: IgnoreMediaRetentionPolicy,
        time: UnixTime,
    ) -> Result<(), TransactionError> {
        let (_, lower) =
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
        let range = range
            .into()
            .map(|(last_access, content_size)| (ignore_policy, last_access, content_size));
        self.delete_items_by_key_components::<MediaMetadata, IndexedMediaMetadataRetentionKey>(
            range,
        )
        .await
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
        let (_, lower_last_access, lower_content_size) =
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
        id: u64,
    ) -> Result<Option<MediaContent>, TransactionError> {
        self.get_item_by_key_components::<MediaContent, IndexedMediaContentIdKey>(id).await
    }

    /// Query IndexedDB for the maximum [`IndexedMediaContentIdKey`] associated
    /// with a [`MediaContent`]
    pub async fn get_max_media_content_key_by_id(
        &self,
    ) -> Result<Option<IndexedMediaContentIdKey>, TransactionError> {
        self.get_max_key::<MediaContent, IndexedMediaContentIdKey>(IndexedKeyRange::all(
            self.serializer().inner(),
        ))
        .await
    }

    /// Query IndexedDB for the next available [`MediaContent::id`]
    pub async fn get_next_media_content_id(&self) -> Result<u64, TransactionError> {
        Ok(match self.get_max_media_content_key_by_id().await? {
            Some(key) => key.checked_add(1).ok_or(TransactionError::NumericalOverflow)?,
            None => 0,
        })
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
    /// already exists, it will be overwritten.
    pub async fn put_media_content(&self, content: &MediaContent) -> Result<(), TransactionError> {
        self.put_item(content).await
    }

    /// Adds [`MediaContent`] to IndexedDB if the size of
    /// [`IndexedMediaContent::content`][1] does not exceed
    /// [`MediaRetentionPolicy::max_file_size]. If an item with the same key
    /// already exists, it will be overwritten.
    ///
    /// [1]: crate::media_store::serializer::indexed_types::IndexedMediaContent::content
    pub async fn put_media_content_if_policy_compliant(
        &self,
        media: &MediaContent,
        policy: MediaRetentionPolicy,
    ) -> Result<(), TransactionError> {
        self.put_item_if(media, |indexed| {
            !policy.exceeds_max_file_size(indexed.content.len() as u64)
        })
        .await
    }

    /// Delete [`MediaContent`] that match the given identifier from IndexedDB
    pub async fn delete_media_content_by_id(&self, id: u64) -> Result<(), TransactionError> {
        self.delete_item_by_key::<MediaContent, IndexedMediaContentIdKey>(id).await
    }
}
