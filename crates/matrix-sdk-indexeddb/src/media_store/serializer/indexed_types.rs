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

use matrix_sdk_base::media::{
    store::{IgnoreMediaRetentionPolicy, MediaRetentionPolicy},
    MediaRequestParameters, UniqueKey,
};
use matrix_sdk_crypto::CryptoStoreError;
use ruma::MxcUri;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    media_store::{
        migrations::current::keys,
        serializer::{
            constants::{
                INDEXED_KEY_LOWER_MEDIA_CONTENT_SIZE, INDEXED_KEY_LOWER_UNIX_TIME,
                INDEXED_KEY_UPPER_MEDIA_CONTENT_SIZE, INDEXED_KEY_UPPER_UNIX_TIME,
            },
            foreign::{ignore_media_retention_policy, unix_time},
        },
        types::{Lease, Media, MediaCleanupTime, UnixTime},
    },
    serializer::{
        Indexed, IndexedKey, IndexedKeyComponentBounds, IndexedPrefixKeyComponentBounds,
        MaybeEncrypted, SafeEncodeSerializer, INDEXED_KEY_LOWER_STRING, INDEXED_KEY_UPPER_STRING,
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

/// A (possibly) encrypted representation of a [`MediaMetadata`][1]
///
/// [1]: crate::media_store::types::MediaMetadata
pub type IndexedMediaMetadata = MaybeEncrypted;

/// A (possibly) encrypted representation of [`Media::content`]
pub type IndexedMediaContent = Vec<u8>;

/// A representation of the size in bytes of the [`IndexedMediaContent`] which
/// is suitable for use in an IndexedDB key
pub type IndexedMediaContentSize = usize;

/// A representation of time in seconds since the [Unix
/// Epoch](std::time::UNIX_EPOCH) which is suitable for use in an IndexedDB key
pub type IndexedSecondsSinceUnixEpoch = u64;

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

/// Represents the [`MEDIA`][1] object store.
///
/// [1]: crate::media_store::migrations::v1::create_media_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedMedia {
    /// The primary key of the object store
    pub id: IndexedMediaIdKey,
    /// The (possibly) hashed [`MxcUri`] of the media derived from
    /// [`MediaRequestParameters::uri`]
    pub uri: IndexedMediaUriKey,
    /// The size (in bytes) of the media content and whether to ignore the
    /// [`MediaRetentionPolicy`]
    pub content_size: IndexedMediaContentSizeKey,
    /// The last time the media was accessed and whether to ignore the
    /// [`MediaRetentionPolicy`]
    pub last_access: IndexedMediaLastAccessKey,
    /// The last the media was accessed, the size (in bytes) of the media
    /// content, and whether to ignore the [`MediaRetentionPolicy`]
    pub retention_metadata: IndexedMediaRetentionMetadataKey,
    /// The (possibly) encrypted metadata - i.e., [`MediaMetadata`][1]
    ///
    /// [1]: crate::media_store::types::MediaMetadata
    pub metadata: IndexedMediaMetadata,
    /// The (possibly) encrypted content - i.e., [`Media::content`]
    pub content: IndexedMediaContent,
}

#[derive(Debug, Error)]
pub enum IndexedMediaError {
    #[error("crypto store: {0}")]
    CryptoStore(#[from] CryptoStoreError),
    #[error("serialization: {0}")]
    Serialization(#[from] rmp_serde::encode::Error),
    #[error("deserialization: {0}")]
    Deserialization(#[from] rmp_serde::decode::Error),
}

impl Indexed for Media {
    const OBJECT_STORE: &'static str = keys::MEDIA;

    type IndexedType = IndexedMedia;
    type Error = IndexedMediaError;

    fn to_indexed(
        &self,
        serializer: &SafeEncodeSerializer,
    ) -> Result<Self::IndexedType, Self::Error> {
        let content = rmp_serde::to_vec_named(&serializer.maybe_encrypt_value(&self.content)?)?;
        Ok(Self::IndexedType {
            id: <IndexedMediaIdKey as IndexedKey<Self>>::encode(
                &self.metadata.request_parameters,
                serializer,
            ),
            uri: <IndexedMediaUriKey as IndexedKey<Self>>::encode(
                self.metadata.request_parameters.uri(),
                serializer,
            ),
            content_size: IndexedMediaContentSizeKey::encode(
                (self.metadata.ignore_policy, content.len()),
                serializer,
            ),
            last_access: IndexedMediaLastAccessKey::encode(
                (self.metadata.ignore_policy, self.metadata.last_access),
                serializer,
            ),
            retention_metadata: IndexedMediaRetentionMetadataKey::encode(
                (self.metadata.ignore_policy, self.metadata.last_access, content.len()),
                serializer,
            ),
            metadata: serializer.maybe_encrypt_value(&self.metadata)?,
            content,
        })
    }

    fn from_indexed(
        indexed: Self::IndexedType,
        serializer: &SafeEncodeSerializer,
    ) -> Result<Self, Self::Error> {
        Ok(Self {
            metadata: serializer.maybe_decrypt_value(indexed.metadata)?,
            content: serializer.maybe_decrypt_value(rmp_serde::from_slice(&indexed.content)?)?,
        })
    }
}

/// The primary key of the [`MEDIA`][1] object store, which is constructed from:
///
/// - The (possibly) hashed value returned by
///   [`MediaRequestParameters::unique_key`]
///
/// [1]: crate::media_store::migrations::v1::create_media_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedMediaIdKey(String);

impl IndexedKey<Media> for IndexedMediaIdKey {
    type KeyComponents<'a> = &'a MediaRequestParameters;

    fn encode(components: Self::KeyComponents<'_>, serializer: &SafeEncodeSerializer) -> Self {
        Self(serializer.encode_key_as_string(keys::MEDIA, components.unique_key()))
    }
}

/// The value associated with the [`source`](IndexedMedia::source) index of the
/// [`MEDIA`][1] object store, which is constructed from:
///
/// - The (possibly) hashed [`MxcUri`] returned by
///   [`MediaRequestParameters::uri`]
///
/// [1]: crate::media_store::migrations::v1::create_media_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedMediaUriKey(String);

impl IndexedKey<Media> for IndexedMediaUriKey {
    const INDEX: Option<&'static str> = Some(keys::MEDIA_URI);

    type KeyComponents<'a> = &'a MxcUri;

    fn encode(components: Self::KeyComponents<'_>, serializer: &SafeEncodeSerializer) -> Self {
        Self(serializer.encode_key_as_string(keys::MEDIA_URI, components))
    }
}

/// The value associated with the [`content_size`](IndexedMedia::content_size)
/// index of the [`MEDIA`][1] object store, which is constructed from:
///
/// - The value of [`IgnoreMediaRetentionPolicy`]
/// - The size in bytes of the associated [`IndexedMedia::content`]
///
/// [1]: crate::media_store::migrations::v1::create_media_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedMediaContentSizeKey(
    #[serde(with = "ignore_media_retention_policy")] IgnoreMediaRetentionPolicy,
    IndexedMediaContentSize,
);

impl IndexedMediaContentSizeKey {
    /// Returns whether the associated [`IndexedMedia`] record should ignore the
    /// global [`MediaRetentionPolicy`]
    pub fn ignore_policy(&self) -> bool {
        self.0.is_yes()
    }

    /// Returns the size in bytes of the associated [`IndexedMedia::content`]
    pub fn content_size(&self) -> usize {
        self.1
    }
}

impl IndexedKey<Media> for IndexedMediaContentSizeKey {
    const INDEX: Option<&'static str> = Some(keys::MEDIA_CONTENT_SIZE);

    type KeyComponents<'a> = (IgnoreMediaRetentionPolicy, IndexedMediaContentSize);

    fn encode(
        (ignore_policy, content_size): Self::KeyComponents<'_>,
        _: &SafeEncodeSerializer,
    ) -> Self {
        Self(ignore_policy, content_size)
    }
}

impl IndexedKeyComponentBounds<Media> for IndexedMediaContentSizeKey {
    fn lower_key_components() -> Self::KeyComponents<'static> {
        Self::lower_key_components_with_prefix(IgnoreMediaRetentionPolicy::No)
    }

    fn upper_key_components() -> Self::KeyComponents<'static> {
        Self::lower_key_components_with_prefix(IgnoreMediaRetentionPolicy::Yes)
    }
}

impl<'a> IndexedPrefixKeyComponentBounds<'a, Media, IgnoreMediaRetentionPolicy>
    for IndexedMediaContentSizeKey
{
    fn lower_key_components_with_prefix(
        prefix: IgnoreMediaRetentionPolicy,
    ) -> Self::KeyComponents<'a> {
        (prefix, INDEXED_KEY_LOWER_MEDIA_CONTENT_SIZE)
    }

    fn upper_key_components_with_prefix(
        prefix: IgnoreMediaRetentionPolicy,
    ) -> Self::KeyComponents<'a> {
        (prefix, INDEXED_KEY_UPPER_MEDIA_CONTENT_SIZE)
    }
}

/// The value associated with the [`last_access`](IndexedMedia::last_access)
/// index of the [`MEDIA`][1] object store, which is constructed from:
///
/// - The value of [`IgnoreMediaRetentionPolicy`]
/// - The last time the associated [`IndexedMedia`] was accessed, represented as
///   a [`UnixTime`]
///
/// [1]: crate::media_store::migrations::v1::create_media_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedMediaLastAccessKey(
    #[serde(with = "ignore_media_retention_policy")] IgnoreMediaRetentionPolicy,
    #[serde(with = "unix_time")] UnixTime,
);

impl IndexedMediaLastAccessKey {
    /// Returns the [`IgnoreMediaRetentionPolicy`] value of the associated
    /// [`IndexedMedia`]
    pub fn ignore_policy(&self) -> IgnoreMediaRetentionPolicy {
        self.0
    }

    /// Returns the last time the associated [`IndexedMedia`] record was
    /// accessed as a [`UnixTime`]
    pub fn last_access(&self) -> UnixTime {
        self.1
    }
}

impl IndexedKey<Media> for IndexedMediaLastAccessKey {
    const INDEX: Option<&'static str> = Some(keys::MEDIA_LAST_ACCESS);

    type KeyComponents<'a> = (IgnoreMediaRetentionPolicy, UnixTime);

    fn encode(
        (ignore_policy, last_access): Self::KeyComponents<'_>,
        _: &SafeEncodeSerializer,
    ) -> Self {
        Self(ignore_policy, last_access)
    }
}

impl IndexedKeyComponentBounds<Media> for IndexedMediaLastAccessKey {
    fn lower_key_components() -> Self::KeyComponents<'static> {
        Self::lower_key_components_with_prefix(IgnoreMediaRetentionPolicy::No)
    }

    fn upper_key_components() -> Self::KeyComponents<'static> {
        Self::lower_key_components_with_prefix(IgnoreMediaRetentionPolicy::Yes)
    }
}

impl<'a> IndexedPrefixKeyComponentBounds<'a, Media, IgnoreMediaRetentionPolicy>
    for IndexedMediaLastAccessKey
{
    fn lower_key_components_with_prefix(
        prefix: IgnoreMediaRetentionPolicy,
    ) -> Self::KeyComponents<'a> {
        (prefix, INDEXED_KEY_LOWER_UNIX_TIME)
    }

    fn upper_key_components_with_prefix(
        prefix: IgnoreMediaRetentionPolicy,
    ) -> Self::KeyComponents<'a> {
        (prefix, INDEXED_KEY_UPPER_UNIX_TIME)
    }
}

/// The value associated with the
/// [`retention_metadata`](IndexedMedia::retention_metadata) index of the
/// [`MEDIA`][1] object store, which is constructed from:
///
/// - The value of [`IgnoreMediaRetentionPolicy`]
/// - The last time the associated [`IndexedMedia`] was accessed, represented as
///   a [`UnixTime`]
/// - The size in bytes of the associated [`IndexedMedia::content`]
///
/// [1]: crate::media_store::migrations::v1::create_media_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedMediaRetentionMetadataKey(
    #[serde(with = "ignore_media_retention_policy")] IgnoreMediaRetentionPolicy,
    #[serde(with = "unix_time")] UnixTime,
    IndexedMediaContentSize,
);

impl IndexedMediaRetentionMetadataKey {
    /// Returns the [`IgnoreMediaRetentionPolicy`] value of the associated
    /// [`IndexedMedia`]
    pub fn ignore_policy(&self) -> IgnoreMediaRetentionPolicy {
        self.0
    }

    /// Returns the last time the associated [`IndexedMedia`] record was
    /// accessed as a [`UnixTime`]
    pub fn last_access(&self) -> UnixTime {
        self.1
    }

    /// Returns the size in bytes of the associated [`IndexedMedia::content`]
    pub fn content_size(&self) -> usize {
        self.2
    }
}

impl IndexedKey<Media> for IndexedMediaRetentionMetadataKey {
    const INDEX: Option<&'static str> = Some(keys::MEDIA_RETENTION_METADATA);

    type KeyComponents<'a> = (IgnoreMediaRetentionPolicy, UnixTime, IndexedMediaContentSize);

    fn encode(
        (ignore_policy, last_access, content_size): Self::KeyComponents<'_>,
        _: &SafeEncodeSerializer,
    ) -> Self {
        Self(ignore_policy, last_access, content_size)
    }
}

impl IndexedKeyComponentBounds<Media> for IndexedMediaRetentionMetadataKey {
    fn lower_key_components() -> Self::KeyComponents<'static> {
        Self::lower_key_components_with_prefix(IgnoreMediaRetentionPolicy::No)
    }

    fn upper_key_components() -> Self::KeyComponents<'static> {
        Self::lower_key_components_with_prefix(IgnoreMediaRetentionPolicy::Yes)
    }
}

impl<'a> IndexedPrefixKeyComponentBounds<'a, Media, IgnoreMediaRetentionPolicy>
    for IndexedMediaRetentionMetadataKey
{
    fn lower_key_components_with_prefix(
        prefix: IgnoreMediaRetentionPolicy,
    ) -> Self::KeyComponents<'a> {
        (prefix, INDEXED_KEY_LOWER_UNIX_TIME, INDEXED_KEY_LOWER_MEDIA_CONTENT_SIZE)
    }

    fn upper_key_components_with_prefix(
        prefix: IgnoreMediaRetentionPolicy,
    ) -> Self::KeyComponents<'a> {
        (prefix, INDEXED_KEY_UPPER_UNIX_TIME, INDEXED_KEY_UPPER_MEDIA_CONTENT_SIZE)
    }
}
