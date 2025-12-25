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

//! Types used for (de)serialization of media store data.
//!
//! These types are wrappers around the types found in
//! [`crate::media_store::types`] and prepare those types for
//! serialization in IndexedDB. They are constructed by extracting
//! relevant values from the inner types, storing those values in indexed
//! fields, and then storing the full types in a possibly encrypted form. This
//! allows the data to be encrypted, while still allowing for efficient querying
//! and retrieval of data.
//!
//! Each top-level type represents an object store in IndexedDB and each
//! field - except the content field - represents an index on that object store.
//! These types mimic the structure of the object stores and indices created in
//! [`crate::media_store::migrations`].

use std::ops::Deref;

use matrix_sdk_base::media::{
    MediaRequestParameters, UniqueKey,
    store::{IgnoreMediaRetentionPolicy, MediaRetentionPolicy},
};
use matrix_sdk_crypto::CryptoStoreError;
use ruma::MxcUri;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::{
    media_store::{
        migrations::current::keys,
        serializer::{
            constants::{
                INDEXED_KEY_LOWER_MEDIA_CONTENT_ID, INDEXED_KEY_LOWER_MEDIA_CONTENT_SIZE,
                INDEXED_KEY_LOWER_UNIX_TIME, INDEXED_KEY_UPPER_MEDIA_CONTENT_ID,
                INDEXED_KEY_UPPER_MEDIA_CONTENT_SIZE, INDEXED_KEY_UPPER_UNIX_TIME,
            },
            foreign::{ignore_media_retention_policy, unix_time},
        },
        types::{Lease, MediaCleanupTime, MediaContent, MediaMetadata, UnixTime},
    },
    serializer::{
        indexed_type::{
            constants::{INDEXED_KEY_LOWER_STRING, INDEXED_KEY_UPPER_STRING},
            traits::{
                Indexed, IndexedKey, IndexedKeyComponentBounds, IndexedPrefixKeyComponentBounds,
            },
        },
        safe_encode::types::{MaybeEncrypted, SafeEncodeSerializer},
    },
};

/// A representation of the primary key of the [`CORE`][1] object store.
/// The key may or may not be hashed depending on the
/// provided [`IndexeddbSerializer`].
///
/// [1]: crate::media_store::migrations::v1::create_core_object_store
pub type IndexedCoreIdKey = String;

/// A (possibly) encrypted representation of a [`Lease`]
pub type IndexedLeaseContent = MaybeEncrypted;

/// A (possibly) encrypted representation of a [`MediaRetentionPolicy`]
pub type IndexedMediaRetentionPolicyContent = MaybeEncrypted;

/// A (possibly) encrypted representation of the last time the store was
/// cleaned - i.e., as a [`UnixTime`]
pub type IndexedMediaCleanupTimeContent = MaybeEncrypted;

/// A (possibly) encrypted representation of a [`MediaMetadata`]
pub type IndexedMediaMetadataContent = MaybeEncrypted;

/// A representation of the size in bytes of the [`IndexedMediaContent`] which
/// is suitable for use in an IndexedDB key
pub type IndexedMediaContentSize = usize;

/// A representation of the identifier [`MediaContent::content_id`]
pub type IndexedMediaContentId = Uuid;

/// A (possibly) encrypted representation of [`MediaContent::data`]
pub type IndexedMediaContentData = Vec<u8>;

/// Represents the [`LEASES`][1] object store.
///
/// [1]: crate::media_store::migrations::v1::create_lease_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedLease {
    /// The primary key of the object store.
    pub id: IndexedLeaseIdKey,
    /// The (possibly encrypted) content - i.e., a [`Lease`].
    pub content: IndexedLeaseContent,
}

impl Indexed for Lease {
    type IndexedType = IndexedLease;

    const OBJECT_STORE: &'static str = keys::LEASES;

    type Error = CryptoStoreError;

    fn to_indexed(
        &self,
        serializer: &SafeEncodeSerializer,
    ) -> Result<Self::IndexedType, Self::Error> {
        Ok(IndexedLease {
            id: <IndexedLeaseIdKey as IndexedKey<Lease>>::encode(&self.key, serializer),
            content: serializer.maybe_encrypt_value(self)?,
        })
    }

    fn from_indexed(
        indexed: Self::IndexedType,
        serializer: &SafeEncodeSerializer,
    ) -> Result<Self, Self::Error> {
        serializer.maybe_decrypt_value(indexed.content)
    }
}

/// The value associated with the [primary key](IndexedLease::id) of the
/// [`LEASES`][1] object store, which is constructed from the value in
/// [`Lease::key`]. This value may or may not be hashed depending on the
/// provided [`IndexeddbSerializer`].
///
/// [1]: crate::media_store::migrations::v1::create_lease_object_store
pub type IndexedLeaseIdKey = String;

impl IndexedKey<Lease> for IndexedLeaseIdKey {
    type KeyComponents<'a> = &'a str;

    fn encode(components: Self::KeyComponents<'_>, serializer: &SafeEncodeSerializer) -> Self {
        serializer.encode_key_as_string(keys::LEASES, components)
    }
}

impl IndexedKeyComponentBounds<Lease> for IndexedLeaseIdKey {
    fn lower_key_components() -> Self::KeyComponents<'static> {
        INDEXED_KEY_LOWER_STRING.as_str()
    }

    fn upper_key_components() -> Self::KeyComponents<'static> {
        INDEXED_KEY_UPPER_STRING.as_str()
    }
}

/// Represents the [`MediaRetentionPolicy`] record in the [`CORE`][1] object
/// store.
///
/// [1]: crate::media_store::migrations::v1::create_core_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedMediaRetentionPolicy {
    /// The primary key of the object store.
    pub id: IndexedCoreIdKey,
    /// The (possibly) encrypted content - i.e., a [`MediaRetentionPolicy`].
    pub content: IndexedMediaRetentionPolicyContent,
}

impl Indexed for MediaRetentionPolicy {
    const OBJECT_STORE: &'static str = keys::CORE;

    type IndexedType = IndexedMediaRetentionPolicy;
    type Error = CryptoStoreError;

    fn to_indexed(
        &self,
        serializer: &SafeEncodeSerializer,
    ) -> Result<Self::IndexedType, Self::Error> {
        Ok(Self::IndexedType {
            id: <IndexedCoreIdKey as IndexedKey<Self>>::encode((), serializer),
            content: serializer.maybe_encrypt_value(self)?,
        })
    }

    fn from_indexed(
        indexed: Self::IndexedType,
        serializer: &SafeEncodeSerializer,
    ) -> Result<Self, Self::Error> {
        serializer.maybe_decrypt_value(indexed.content)
    }
}

impl IndexedKey<MediaRetentionPolicy> for IndexedCoreIdKey {
    type KeyComponents<'a> = ();

    fn encode(_components: Self::KeyComponents<'_>, serializer: &SafeEncodeSerializer) -> Self {
        serializer.encode_key_as_string(keys::CORE, keys::MEDIA_RETENTION_POLICY_KEY)
    }
}

/// Represents the [`MediaCleanupTime`] record in the [`CORE`][1] object store.
///
/// [1]: crate::media_store::migrations::v1::create_core_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedMediaCleanupTime {
    /// The primary key of the object store.
    pub id: IndexedCoreIdKey,
    /// The (possibly) encrypted content - i.e., a [`MediaCleanupTime`]
    pub content: IndexedMediaCleanupTimeContent,
}

impl Indexed for MediaCleanupTime {
    const OBJECT_STORE: &'static str = keys::CORE;

    type IndexedType = IndexedMediaCleanupTime;
    type Error = CryptoStoreError;

    fn to_indexed(
        &self,
        serializer: &SafeEncodeSerializer,
    ) -> Result<Self::IndexedType, Self::Error> {
        Ok(Self::IndexedType {
            id: <IndexedCoreIdKey as IndexedKey<Self>>::encode((), serializer),
            content: serializer.maybe_encrypt_value(self)?,
        })
    }

    fn from_indexed(
        indexed: Self::IndexedType,
        serializer: &SafeEncodeSerializer,
    ) -> Result<Self, Self::Error> {
        serializer.maybe_decrypt_value(indexed.content)
    }
}

impl IndexedKey<MediaCleanupTime> for IndexedCoreIdKey {
    type KeyComponents<'a> = ();

    fn encode(_components: Self::KeyComponents<'_>, serializer: &SafeEncodeSerializer) -> Self {
        serializer.encode_key_as_string(keys::CORE, keys::MEDIA_CLEANUP_TIME_KEY)
    }
}

/// Represents the [`MEDIA_METADATA`][1] object store.
///
/// [1]: crate::media_store::migrations::v1::create_media_metadata_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedMediaMetadata {
    /// The primary key of the object store
    pub id: IndexedMediaMetadataIdKey,
    /// The (possibly) hashed [`MxcUri`] of the media derived from
    /// [`MediaRequestParameters::uri`]
    pub uri: IndexedMediaMetadataUriKey,
    /// The size (in bytes) of the media content and whether to ignore the
    /// [`MediaRetentionPolicy`]
    pub content_size: IndexedMediaMetadataContentSizeKey,
    /// The last time the media was accessed and whether to ignore the
    /// [`MediaRetentionPolicy`]
    pub last_access: IndexedMediaMetadataLastAccessKey,
    /// The last the media was accessed, the size (in bytes) of the media
    /// content, and whether to ignore the [`MediaRetentionPolicy`]
    pub retention: IndexedMediaMetadataRetentionKey,
    /// The (possibly) encrypted content - i.e., [`MediaMetadata`]
    pub content: IndexedMediaMetadataContent,
}

impl Indexed for MediaMetadata {
    const OBJECT_STORE: &'static str = keys::MEDIA_METADATA;

    type IndexedType = IndexedMediaMetadata;
    type Error = CryptoStoreError;

    fn to_indexed(
        &self,
        serializer: &SafeEncodeSerializer,
    ) -> Result<Self::IndexedType, Self::Error> {
        Ok(Self::IndexedType {
            id: <IndexedMediaMetadataIdKey as IndexedKey<Self>>::encode(
                &self.request_parameters,
                serializer,
            ),
            uri: <IndexedMediaMetadataUriKey as IndexedKey<Self>>::encode(
                self.request_parameters.uri(),
                serializer,
            ),
            content_size: IndexedMediaMetadataContentSizeKey::encode(
                (self.ignore_policy, self.content_size, self.content_id),
                serializer,
            ),
            last_access: IndexedMediaMetadataLastAccessKey::encode(
                (self.ignore_policy, self.last_access, self.content_id),
                serializer,
            ),
            retention: IndexedMediaMetadataRetentionKey::encode(
                (self.ignore_policy, self.last_access, self.content_size, self.content_id),
                serializer,
            ),
            content: serializer.maybe_encrypt_value(self)?,
        })
    }

    fn from_indexed(
        indexed: Self::IndexedType,
        serializer: &SafeEncodeSerializer,
    ) -> Result<Self, Self::Error> {
        serializer.maybe_decrypt_value(indexed.content)
    }
}

/// The primary key of the [`MEDIA_METADATA`][1] object store, which is
/// constructed from:
///
/// - The (possibly) hashed value returned by
///   [`MediaRequestParameters::unique_key`]
///
/// [1]: crate::media_store::migrations::v1::create_media_metadata_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedMediaMetadataIdKey(String);

impl IndexedKey<MediaMetadata> for IndexedMediaMetadataIdKey {
    type KeyComponents<'a> = &'a MediaRequestParameters;

    fn encode(components: Self::KeyComponents<'_>, serializer: &SafeEncodeSerializer) -> Self {
        Self(serializer.encode_key_as_string(keys::MEDIA_METADATA, components.unique_key()))
    }
}

/// The value associated with the [`ur`](IndexedMediaMetadata::uri) index of the
/// [`MEDIA_METADATA`][1] object store, which is constructed from:
///
/// - The (possibly) hashed [`MxcUri`] returned by
///   [`MediaRequestParameters::uri`]
///
/// [1]: crate::media_store::migrations::v1::create_media_metadata_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedMediaMetadataUriKey(String);

impl IndexedKey<MediaMetadata> for IndexedMediaMetadataUriKey {
    const INDEX: Option<&'static str> = Some(keys::MEDIA_METADATA_URI);

    type KeyComponents<'a> = &'a MxcUri;

    fn encode(components: Self::KeyComponents<'_>, serializer: &SafeEncodeSerializer) -> Self {
        Self(serializer.encode_key_as_string(keys::MEDIA_METADATA_URI, components))
    }
}

/// The value associated with the
/// [`content_size`](IndexedMediaMetadata::content_size) index of the
/// [`MEDIA_METADATA`][1] object store, which is constructed from:
///
/// - The value of [`IgnoreMediaRetentionPolicy`]
/// - The size in bytes of the associated [`IndexedMediaContent::content`]
/// - The identifier of the associated [`IndexedMediaContent`]
///
/// [1]: crate::media_store::migrations::v1::create_media_metadata_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedMediaMetadataContentSizeKey(
    #[serde(with = "ignore_media_retention_policy")] IgnoreMediaRetentionPolicy,
    IndexedMediaContentSize,
    IndexedMediaContentId,
);

impl IndexedMediaMetadataContentSizeKey {
    /// Returns the [`IgnoreMediaRetentionPolicy`] value of the associated
    /// [`IndexedMediaMetadata`]
    pub fn ignore_policy(&self) -> IgnoreMediaRetentionPolicy {
        self.0
    }

    /// Returns the size in bytes of the associated [`IndexedMediaContent`]
    pub fn content_size(&self) -> usize {
        self.1
    }

    /// Returns the identifier of the associated [`IndexedMediaContent`]
    pub fn content_id(&self) -> Uuid {
        self.2
    }
}

impl IndexedKey<MediaMetadata> for IndexedMediaMetadataContentSizeKey {
    const INDEX: Option<&'static str> = Some(keys::MEDIA_METADATA_CONTENT_SIZE);

    type KeyComponents<'a> =
        (IgnoreMediaRetentionPolicy, IndexedMediaContentSize, IndexedMediaContentId);

    fn encode(
        (ignore_policy, content_size, content_id): Self::KeyComponents<'_>,
        _: &SafeEncodeSerializer,
    ) -> Self {
        Self(ignore_policy, content_size, content_id)
    }
}

impl IndexedKeyComponentBounds<MediaMetadata> for IndexedMediaMetadataContentSizeKey {
    fn lower_key_components() -> Self::KeyComponents<'static> {
        Self::lower_key_components_with_prefix(IgnoreMediaRetentionPolicy::No)
    }

    fn upper_key_components() -> Self::KeyComponents<'static> {
        Self::lower_key_components_with_prefix(IgnoreMediaRetentionPolicy::Yes)
    }
}

impl<'a> IndexedPrefixKeyComponentBounds<'a, MediaMetadata, IgnoreMediaRetentionPolicy>
    for IndexedMediaMetadataContentSizeKey
{
    fn lower_key_components_with_prefix(
        prefix: IgnoreMediaRetentionPolicy,
    ) -> Self::KeyComponents<'a> {
        Self::lower_key_components_with_prefix((prefix, INDEXED_KEY_LOWER_MEDIA_CONTENT_SIZE))
    }

    fn upper_key_components_with_prefix(
        prefix: IgnoreMediaRetentionPolicy,
    ) -> Self::KeyComponents<'a> {
        Self::upper_key_components_with_prefix((prefix, INDEXED_KEY_UPPER_MEDIA_CONTENT_SIZE))
    }
}

impl<'a>
    IndexedPrefixKeyComponentBounds<
        'a,
        MediaMetadata,
        (IgnoreMediaRetentionPolicy, IndexedMediaContentSize),
    > for IndexedMediaMetadataContentSizeKey
{
    fn lower_key_components_with_prefix(
        (ignore_policy, content_size): (IgnoreMediaRetentionPolicy, IndexedMediaContentSize),
    ) -> Self::KeyComponents<'a> {
        (ignore_policy, content_size, INDEXED_KEY_LOWER_MEDIA_CONTENT_ID)
    }

    fn upper_key_components_with_prefix(
        (ignore_policy, content_size): (IgnoreMediaRetentionPolicy, IndexedMediaContentSize),
    ) -> Self::KeyComponents<'a> {
        (ignore_policy, content_size, INDEXED_KEY_UPPER_MEDIA_CONTENT_ID)
    }
}

/// The value associated with the
/// [`last_access`](IndexedMediaMetadata::last_access) index of the
/// [`MEDIA_METADATA`][1] object store, which is constructed from:
///
/// - The value of [`IgnoreMediaRetentionPolicy`]
/// - The last time the associated [`IndexedMediaContent`] was accessed,
///   represented as a [`UnixTime`]
/// - The identifier of the associated [`IndexedMediaContent`]
///
/// [1]: crate::media_store::migrations::v1::create_media_metadata_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedMediaMetadataLastAccessKey(
    #[serde(with = "ignore_media_retention_policy")] IgnoreMediaRetentionPolicy,
    #[serde(with = "unix_time")] UnixTime,
    IndexedMediaContentId,
);

impl IndexedMediaMetadataLastAccessKey {
    /// Returns the [`IgnoreMediaRetentionPolicy`] value of the associated
    /// [`IndexedMedia`]
    pub fn ignore_policy(&self) -> IgnoreMediaRetentionPolicy {
        self.0
    }

    /// Returns the last time the associated [`IndexedMediaContent`] was
    /// accessed as a [`UnixTime`]
    pub fn last_access(&self) -> UnixTime {
        self.1
    }

    /// Returns the identifier of the associated [`IndexedMediaContent`]
    pub fn content_id(&self) -> Uuid {
        self.2
    }
}

impl IndexedKey<MediaMetadata> for IndexedMediaMetadataLastAccessKey {
    const INDEX: Option<&'static str> = Some(keys::MEDIA_METADATA_LAST_ACCESS);

    type KeyComponents<'a> = (IgnoreMediaRetentionPolicy, UnixTime, IndexedMediaContentId);

    fn encode(
        (ignore_policy, last_access, content_id): Self::KeyComponents<'_>,
        _: &SafeEncodeSerializer,
    ) -> Self {
        Self(ignore_policy, last_access, content_id)
    }
}

impl IndexedKeyComponentBounds<MediaMetadata> for IndexedMediaMetadataLastAccessKey {
    fn lower_key_components() -> Self::KeyComponents<'static> {
        Self::lower_key_components_with_prefix(IgnoreMediaRetentionPolicy::No)
    }

    fn upper_key_components() -> Self::KeyComponents<'static> {
        Self::lower_key_components_with_prefix(IgnoreMediaRetentionPolicy::Yes)
    }
}

impl<'a> IndexedPrefixKeyComponentBounds<'a, MediaMetadata, IgnoreMediaRetentionPolicy>
    for IndexedMediaMetadataLastAccessKey
{
    fn lower_key_components_with_prefix(
        prefix: IgnoreMediaRetentionPolicy,
    ) -> Self::KeyComponents<'a> {
        Self::lower_key_components_with_prefix((prefix, INDEXED_KEY_LOWER_UNIX_TIME))
    }

    fn upper_key_components_with_prefix(
        prefix: IgnoreMediaRetentionPolicy,
    ) -> Self::KeyComponents<'a> {
        Self::upper_key_components_with_prefix((prefix, INDEXED_KEY_UPPER_UNIX_TIME))
    }
}

impl<'a> IndexedPrefixKeyComponentBounds<'a, MediaMetadata, (IgnoreMediaRetentionPolicy, UnixTime)>
    for IndexedMediaMetadataLastAccessKey
{
    fn lower_key_components_with_prefix(
        (ignore_policy, last_access): (IgnoreMediaRetentionPolicy, UnixTime),
    ) -> Self::KeyComponents<'a> {
        (ignore_policy, last_access, INDEXED_KEY_LOWER_MEDIA_CONTENT_ID)
    }

    fn upper_key_components_with_prefix(
        (ignore_policy, last_access): (IgnoreMediaRetentionPolicy, UnixTime),
    ) -> Self::KeyComponents<'a> {
        (ignore_policy, last_access, INDEXED_KEY_UPPER_MEDIA_CONTENT_ID)
    }
}

/// The value associated with the [`retention`](IndexedMediaMetadata::retention)
/// index of the [`MEDIA_METADATA`][1] object store, which is constructed from:
///
/// - The value of [`IgnoreMediaRetentionPolicy`]
/// - The last time the associated [`IndexedMediaContent`] was accessed,
///   represented as a [`UnixTime`]
/// - The size in bytes of the associated [`IndexedMediaContent`]
/// - The identifier of the associated [`IndexedMediaContent`]
///
/// [1]: crate::media_store::migrations::v1::create_media_metadata_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedMediaMetadataRetentionKey(
    #[serde(with = "ignore_media_retention_policy")] IgnoreMediaRetentionPolicy,
    #[serde(with = "unix_time")] UnixTime,
    IndexedMediaContentSize,
    IndexedMediaContentId,
);

impl IndexedMediaMetadataRetentionKey {
    /// Returns the [`IgnoreMediaRetentionPolicy`] value of the associated
    /// [`IndexedMedia`]
    pub fn ignore_policy(&self) -> IgnoreMediaRetentionPolicy {
        self.0
    }

    /// Returns the last time the associated [`IndexedMediaContent`] was
    /// accessed as a [`UnixTime`]
    pub fn last_access(&self) -> UnixTime {
        self.1
    }

    /// Returns the size in bytes of the associated [`IndexedMediaContent`]
    pub fn content_size(&self) -> usize {
        self.2
    }

    /// Returns the identifier of the associated [`IndexedMediaContent`]
    pub fn content_id(&self) -> Uuid {
        self.3
    }
}

impl IndexedKey<MediaMetadata> for IndexedMediaMetadataRetentionKey {
    const INDEX: Option<&'static str> = Some(keys::MEDIA_METADATA_RETENTION);

    type KeyComponents<'a> =
        (IgnoreMediaRetentionPolicy, UnixTime, IndexedMediaContentSize, IndexedMediaContentId);

    fn encode(
        (ignore_policy, last_access, content_size, content_id): Self::KeyComponents<'_>,
        _: &SafeEncodeSerializer,
    ) -> Self {
        Self(ignore_policy, last_access, content_size, content_id)
    }
}

impl IndexedKeyComponentBounds<MediaMetadata> for IndexedMediaMetadataRetentionKey {
    fn lower_key_components() -> Self::KeyComponents<'static> {
        Self::lower_key_components_with_prefix(IgnoreMediaRetentionPolicy::No)
    }

    fn upper_key_components() -> Self::KeyComponents<'static> {
        Self::lower_key_components_with_prefix(IgnoreMediaRetentionPolicy::Yes)
    }
}

impl<'a> IndexedPrefixKeyComponentBounds<'a, MediaMetadata, IgnoreMediaRetentionPolicy>
    for IndexedMediaMetadataRetentionKey
{
    fn lower_key_components_with_prefix(
        prefix: IgnoreMediaRetentionPolicy,
    ) -> Self::KeyComponents<'a> {
        Self::lower_key_components_with_prefix((
            prefix,
            INDEXED_KEY_LOWER_UNIX_TIME,
            INDEXED_KEY_LOWER_MEDIA_CONTENT_SIZE,
        ))
    }

    fn upper_key_components_with_prefix(
        prefix: IgnoreMediaRetentionPolicy,
    ) -> Self::KeyComponents<'a> {
        Self::upper_key_components_with_prefix((
            prefix,
            INDEXED_KEY_UPPER_UNIX_TIME,
            INDEXED_KEY_UPPER_MEDIA_CONTENT_SIZE,
        ))
    }
}

impl<'a>
    IndexedPrefixKeyComponentBounds<
        'a,
        MediaMetadata,
        (IgnoreMediaRetentionPolicy, UnixTime, IndexedMediaContentSize),
    > for IndexedMediaMetadataRetentionKey
{
    fn lower_key_components_with_prefix(
        (ignore_policy, last_access, content_size): (
            IgnoreMediaRetentionPolicy,
            UnixTime,
            IndexedMediaContentSize,
        ),
    ) -> Self::KeyComponents<'a> {
        (ignore_policy, last_access, content_size, INDEXED_KEY_LOWER_MEDIA_CONTENT_ID)
    }

    fn upper_key_components_with_prefix(
        (ignore_policy, last_access, content_size): (
            IgnoreMediaRetentionPolicy,
            UnixTime,
            IndexedMediaContentSize,
        ),
    ) -> Self::KeyComponents<'a> {
        (ignore_policy, last_access, content_size, INDEXED_KEY_UPPER_MEDIA_CONTENT_ID)
    }
}

/// Represents the [`MEDIA_CONTENT`][1] object store.
///
/// [1]: crate::media_store::migrations::v1::create_media_content_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedMediaContent {
    /// The primary key of the object store
    pub id: IndexedMediaContentIdKey,
    /// The (possibly) encrypted content - i.e., [`MediaContent::data`]
    pub content: IndexedMediaContentData,
}

#[derive(Debug, Error)]
pub enum IndexedMediaContentError {
    #[error("crypto store: {0}")]
    CryptoStore(#[from] CryptoStoreError),
    #[error("serialization: {0}")]
    Serialization(#[from] rmp_serde::encode::Error),
    #[error("deserialization: {0}")]
    Deserialization(#[from] rmp_serde::decode::Error),
}

impl Indexed for MediaContent {
    const OBJECT_STORE: &'static str = keys::MEDIA_CONTENT;

    type IndexedType = IndexedMediaContent;
    type Error = IndexedMediaContentError;

    fn to_indexed(
        &self,
        serializer: &SafeEncodeSerializer,
    ) -> Result<Self::IndexedType, Self::Error> {
        Ok(Self::IndexedType {
            id: IndexedMediaContentIdKey::encode(self.content_id, serializer),
            content: if serializer.has_store_cipher() {
                rmp_serde::to_vec_named(&serializer.maybe_encrypt_value(&self.data)?)?
            } else {
                self.data.clone()
            },
        })
    }

    fn from_indexed(
        indexed: Self::IndexedType,
        serializer: &SafeEncodeSerializer,
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            content_id: *indexed.id,
            data: if serializer.has_store_cipher() {
                serializer.maybe_decrypt_value(rmp_serde::from_slice(&indexed.content)?)?
            } else {
                indexed.content
            },
        })
    }
}

/// The primary key of the [`MEDIA_CONTENT`][1] object store, which is
/// constructed from:
///
/// - The identifier associated with the [`IndexedMediaContent`]
///
/// [1]: crate::media_store::migrations::v1::create_media_content_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedMediaContentIdKey(IndexedMediaContentId);

impl Deref for IndexedMediaContentIdKey {
    type Target = Uuid;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl IndexedKey<MediaContent> for IndexedMediaContentIdKey {
    type KeyComponents<'a> = IndexedMediaContentId;

    fn encode(components: Self::KeyComponents<'_>, _: &SafeEncodeSerializer) -> Self {
        Self(components)
    }
}

impl IndexedKeyComponentBounds<MediaContent> for IndexedMediaContentIdKey {
    fn lower_key_components() -> Self::KeyComponents<'static> {
        INDEXED_KEY_LOWER_MEDIA_CONTENT_ID
    }

    fn upper_key_components() -> Self::KeyComponents<'static> {
        INDEXED_KEY_UPPER_MEDIA_CONTENT_ID
    }
}
