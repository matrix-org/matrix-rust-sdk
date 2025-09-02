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

//! Types used for (de)serialization of event cache store data.
//!
//! These types are wrappers around the types found in
//! [`crate::event_cache_store::types`] and prepare those types for
//! serialization in IndexedDB. They are constructed by extracting
//! relevant values from the inner types, storing those values in indexed
//! fields, and then storing the full types in a possibly encrypted form. This
//! allows the data to be encrypted, while still allowing for efficient querying
//! and retrieval of data.
//!
//! Each top-level type represents an object store in IndexedDB and each
//! field - except the content field - represents an index on that object store.
//! These types mimic the structure of the object stores and indices created in
//! [`crate::event_cache_store::migrations`].

use std::sync::LazyLock;

use matrix_sdk_base::{
    event_cache::store::media::{IgnoreMediaRetentionPolicy, MediaRetentionPolicy},
    linked_chunk::{ChunkIdentifier, LinkedChunkId},
    media::{MediaRequestParameters, UniqueKey},
};
use matrix_sdk_crypto::CryptoStoreError;
use ruma::{events::relation::RelationType, EventId, OwnedEventId, RoomId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    event_cache_store::{
        migrations::current::keys,
        serializer::{
            foreign::ignore_media_retention_policy,
            traits::{
                Indexed, IndexedKey, IndexedKeyBounds, IndexedKeyComponentBounds,
                IndexedPrefixKeyBounds, IndexedPrefixKeyComponentBounds,
            },
        },
        types::{Chunk, Event, Gap, Lease, Media, Position},
    },
    serializer::{IndexeddbSerializer, MaybeEncrypted},
};

/// The first unicode character, and hence the lower bound for IndexedDB keys
/// (or key components) which are represented as strings.
///
/// This value is useful for constructing a key range over all strings when used
/// in conjunction with [`INDEXED_KEY_UPPER_CHARACTER`].
const INDEXED_KEY_LOWER_CHARACTER: char = '\u{0000}';

/// The last unicode character in the [Basic Multilingual Plane][1]. This seems
/// like a reasonable place to set the upper bound for IndexedDB keys (or key
/// components) which are represented as strings, though one could
/// theoretically set it to `\u{10FFFF}`.
///
/// This value is useful for constructing a key range over all strings when used
/// in conjunction with [`INDEXED_KEY_LOWER_CHARACTER`].
///
/// [1]: https://en.wikipedia.org/wiki/Plane_(Unicode)#Basic_Multilingual_Plane
const INDEXED_KEY_UPPER_CHARACTER: char = '\u{FFFF}';

/// Identical to [`INDEXED_KEY_LOWER_CHARACTER`] but represented as a [`String`]
static INDEXED_KEY_LOWER_STRING: LazyLock<String> =
    LazyLock::new(|| String::from(INDEXED_KEY_LOWER_CHARACTER));

/// Identical to [`INDEXED_KEY_UPPER_CHARACTER`] but represented as a [`String`]
static INDEXED_KEY_UPPER_STRING: LazyLock<String> =
    LazyLock::new(|| String::from(INDEXED_KEY_UPPER_CHARACTER));

/// A [`ChunkIdentifier`] constructed with `0`.
///
/// This value is useful for constructing a key range over all keys which
/// contain [`ChunkIdentifier`]s when used in conjunction with
/// [`INDEXED_KEY_UPPER_CHUNK_IDENTIFIER`].
static INDEXED_KEY_LOWER_CHUNK_IDENTIFIER: LazyLock<ChunkIdentifier> =
    LazyLock::new(|| ChunkIdentifier::new(0));

/// A [`ChunkIdentifier`] constructed with [`js_sys::Number::MAX_SAFE_INTEGER`].
///
/// This value is useful for constructing a key range over all keys which
/// contain [`ChunkIdentifier`]s when used in conjunction with
/// [`INDEXED_KEY_LOWER_CHUNK_IDENTIFIER`].
static INDEXED_KEY_UPPER_CHUNK_IDENTIFIER: LazyLock<ChunkIdentifier> =
    LazyLock::new(|| ChunkIdentifier::new(js_sys::Number::MAX_SAFE_INTEGER as u64));

/// An [`OwnedEventId`] constructed with [`INDEXED_KEY_LOWER_CHARACTER`].
///
/// This value is useful for constructing a key range over all keys which
/// contain [`EventId`]s when used in conjunction with
/// [`INDEXED_KEY_UPPER_EVENT_ID`].
static INDEXED_KEY_LOWER_EVENT_ID: LazyLock<OwnedEventId> = LazyLock::new(|| {
    OwnedEventId::try_from(format!("${INDEXED_KEY_LOWER_CHARACTER}")).expect("valid event id")
});

/// An [`OwnedEventId`] constructed with [`INDEXED_KEY_UPPER_CHARACTER`].
///
/// This value is useful for constructing a key range over all keys which
/// contain [`EventId`]s when used in conjunction with
/// [`INDEXED_KEY_LOWER_EVENT_ID`].
static INDEXED_KEY_UPPER_EVENT_ID: LazyLock<OwnedEventId> = LazyLock::new(|| {
    OwnedEventId::try_from(format!("${INDEXED_KEY_UPPER_CHARACTER}")).expect("valid event id")
});

/// The lowest possible index that can be used to reference an [`Event`] inside
/// a [`Chunk`] - i.e., `0`.
///
/// This value is useful for constructing a key range over all keys which
/// contain [`Position`]s when used in conjunction with
/// [`INDEXED_KEY_UPPER_EVENT_INDEX`].
const INDEXED_KEY_LOWER_EVENT_INDEX: usize = 0;

/// The highest possible index that can be used to reference an [`Event`] inside
/// a [`Chunk`] - i.e., [`js_sys::Number::MAX_SAFE_INTEGER`].
///
/// This value is useful for constructing a key range over all keys which
/// contain [`Position`]s when used in conjunction with
/// [`INDEXED_KEY_LOWER_EVENT_INDEX`].
const INDEXED_KEY_UPPER_EVENT_INDEX: usize = js_sys::Number::MAX_SAFE_INTEGER as usize;

/// The lowest possible [`Position`] that can be used to reference an [`Event`].
///
/// This value is useful for constructing a key range over all keys which
/// contain [`Position`]s when used in conjunction with
/// [`INDEXED_KEY_UPPER_EVENT_INDEX`].
static INDEXED_KEY_LOWER_EVENT_POSITION: LazyLock<Position> = LazyLock::new(|| Position {
    chunk_identifier: INDEXED_KEY_LOWER_CHUNK_IDENTIFIER.index(),
    index: INDEXED_KEY_LOWER_EVENT_INDEX,
});

/// The highest possible [`Position`] that can be used to reference an
/// [`Event`].
///
/// This value is useful for constructing a key range over all keys which
/// contain [`Position`]s when used in conjunction with
/// [`INDEXED_KEY_LOWER_EVENT_INDEX`].
static INDEXED_KEY_UPPER_EVENT_POSITION: LazyLock<Position> = LazyLock::new(|| Position {
    chunk_identifier: INDEXED_KEY_UPPER_CHUNK_IDENTIFIER.index(),
    index: INDEXED_KEY_UPPER_EVENT_INDEX,
});

/// Representation of a range of keys of type `K`. This is loosely
/// correlated with [IDBKeyRange][1], with a few differences.
///
/// Namely, this enum only provides a single way to express a bounded range
/// which is always inclusive on both bounds. While all ranges can still be
/// represented, [`IDBKeyRange`][1] provides more flexibility in this regard.
///
/// [1]: https://developer.mozilla.org/en-US/docs/Web/API/IDBKeyRange
#[derive(Debug, Copy, Clone)]
pub enum IndexedKeyRange<K> {
    /// Represents a single key of type `K`.
    ///
    /// Identical to [`IDBKeyRange.only`][1].
    ///
    /// [1]: https://developer.mozilla.org/en-US/docs/Web/API/IDBKeyRange/only
    Only(K),
    /// Represents an inclusive range of keys of type `K`
    /// where the first item is the lower bound and the
    /// second item is the upper bound.
    ///
    /// Similar to [`IDBKeyRange.bound`][1].
    ///
    /// [1]: https://developer.mozilla.org/en-US/docs/Web/API/IDBKeyRange/bound
    Bound(K, K),
}

impl<'a, C: 'a> IndexedKeyRange<C> {
    /// Encodes a range of key components of type `K::KeyComponents`
    /// into a range of keys of type `K`.
    pub fn encoded<T, K>(self, serializer: &IndexeddbSerializer) -> IndexedKeyRange<K>
    where
        T: Indexed,
        K: IndexedKey<T, KeyComponents<'a> = C>,
    {
        match self {
            Self::Only(components) => IndexedKeyRange::Only(K::encode(components, serializer)),
            Self::Bound(lower, upper) => {
                IndexedKeyRange::Bound(K::encode(lower, serializer), K::encode(upper, serializer))
            }
        }
    }
}

impl<K> IndexedKeyRange<K> {
    pub fn map<T, F>(self, f: F) -> IndexedKeyRange<T>
    where
        F: Fn(K) -> T,
    {
        match self {
            IndexedKeyRange::Only(key) => IndexedKeyRange::Only(f(key)),
            IndexedKeyRange::Bound(lower, upper) => IndexedKeyRange::Bound(f(lower), f(upper)),
        }
    }

    pub fn all<T>(serializer: &IndexeddbSerializer) -> IndexedKeyRange<K>
    where
        T: Indexed,
        K: IndexedKeyBounds<T>,
    {
        IndexedKeyRange::Bound(K::lower_key(serializer), K::upper_key(serializer))
    }

    pub fn all_with_prefix<T, P>(prefix: P, serializer: &IndexeddbSerializer) -> IndexedKeyRange<K>
    where
        T: Indexed,
        K: IndexedPrefixKeyBounds<T, P>,
        P: Clone,
    {
        IndexedKeyRange::Bound(
            K::lower_key_with_prefix(prefix.clone(), serializer),
            K::upper_key_with_prefix(prefix, serializer),
        )
    }
}

impl<K> From<(K, K)> for IndexedKeyRange<K> {
    fn from(value: (K, K)) -> Self {
        Self::Bound(value.0, value.1)
    }
}

impl<K> From<K> for IndexedKeyRange<K> {
    fn from(value: K) -> Self {
        Self::Only(value)
    }
}

/// A representation of the primary key of the [`CORE`][1] object store.
/// The key may or may not be hashed depending on the
/// provided [`IndexeddbSerializer`].
///
/// [1]: crate::event_cache_store::migrations::v1::create_core_object_store
pub type IndexedCoreIdKey = String;

/// A (possibly) encrypted representation of a [`Lease`]
pub type IndexedLeaseContent = MaybeEncrypted;

/// A (possibly) hashed representation of a [`LinkedChunkId`] which is suitable
/// for use in an IndexedDB key
pub type IndexedLinkedChunkId = Vec<u8>;

/// A (possibly) hashed representation of an [`RoomId`] which is suitable for
/// use in an IndexedDB key
pub type IndexedRoomId = String;

/// A representation of a [`ChunkIdentifier`] which is suitable for use in an
/// IndexedDB key
pub type IndexedChunkId = u64;

/// A (possibly) encrypted representation of an [`Event`]
pub type IndexedChunkContent = MaybeEncrypted;

/// A (possibly) hashed representation of an [`EventId`] which is suitable for
/// use in an IndexedDB key
pub type IndexedEventId = String;

/// A representation of the position of an [`Event`] in a [`Chunk`] which is
/// suitable for use in an IndexedDB key
pub type IndexedEventPositionIndex = usize;

/// A (possibly) hashed representation of the relationship between two events
/// (see [`RelationType`](ruma::events::relation::RelationType)) which is
/// suitable for use in an IndexedDB key
pub type IndexedRelationType = String;

/// A (possibly) encrypted representation of an [`Event`]
pub type IndexedEventContent = MaybeEncrypted;

/// A (possibly) encrypted representation of a [`Gap`]
pub type IndexedGapContent = MaybeEncrypted;

/// A (possibly) encrypted representation of a [`MediaRetentionPolicy`]
pub type IndexedMediaRetentionPolicyContent = MaybeEncrypted;

/// A (possibly) encrypted representation of a [`MediaMetadata`][1]
///
/// [1]: crate::event_cache_store::types::MediaMetadata
pub type IndexedMediaMetadata = MaybeEncrypted;

/// A (possibly) encrypted representation of [`Media::content`]
pub type IndexedMediaContent = Vec<u8>;

/// A representation of the size in bytes of the [`IndexedMediaContent`] which
/// is suitable for use in an IndexedDB key
pub type IndexedMediaContentSize = usize;

/// Represents the [`LEASES`][1] object store.
///
/// [1]: crate::event_cache_store::migrations::v1::create_lease_object_store
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
        serializer: &IndexeddbSerializer,
    ) -> Result<Self::IndexedType, Self::Error> {
        Ok(IndexedLease {
            id: <IndexedLeaseIdKey as IndexedKey<Lease>>::encode(&self.key, serializer),
            content: serializer.maybe_encrypt_value(self)?,
        })
    }

    fn from_indexed(
        indexed: Self::IndexedType,
        serializer: &IndexeddbSerializer,
    ) -> Result<Self, Self::Error> {
        serializer.maybe_decrypt_value(indexed.content)
    }
}

/// The value associated with the [primary key](IndexedLease::id) of the
/// [`LEASES`][1] object store, which is constructed from the value in
/// [`Lease::key`]. This value may or may not be hashed depending on the
/// provided [`IndexeddbSerializer`].
///
/// [1]: crate::event_cache_store::migrations::v1::create_linked_chunks_object_store
pub type IndexedLeaseIdKey = String;

impl IndexedKey<Lease> for IndexedLeaseIdKey {
    type KeyComponents<'a> = &'a str;

    fn encode(components: Self::KeyComponents<'_>, serializer: &IndexeddbSerializer) -> Self {
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

/// Represents the [`LINKED_CHUNKS`][1] object store.
///
/// [1]: crate::event_cache_store::migrations::v1::create_linked_chunks_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedChunk {
    /// The primary key of the object store.
    pub id: IndexedChunkIdKey,
    /// An indexed key on the object store, which represents the
    /// [`IndexedChunkIdKey`] of the next chunk in the linked list, if it
    /// exists.
    pub next: IndexedNextChunkIdKey,
    /// The (possibly) encrypted content of the chunk.
    pub content: IndexedChunkContent,
}

impl Indexed for Chunk {
    const OBJECT_STORE: &'static str = keys::LINKED_CHUNKS;

    type IndexedType = IndexedChunk;
    type Error = CryptoStoreError;

    fn to_indexed(
        &self,
        serializer: &IndexeddbSerializer,
    ) -> Result<Self::IndexedType, Self::Error> {
        Ok(IndexedChunk {
            id: <IndexedChunkIdKey as IndexedKey<Chunk>>::encode(
                (self.linked_chunk_id.as_ref(), ChunkIdentifier::new(self.identifier)),
                serializer,
            ),
            next: IndexedNextChunkIdKey::encode(
                (self.linked_chunk_id.as_ref(), self.next.map(ChunkIdentifier::new)),
                serializer,
            ),
            content: serializer.maybe_encrypt_value(self)?,
        })
    }

    fn from_indexed(
        indexed: Self::IndexedType,
        serializer: &IndexeddbSerializer,
    ) -> Result<Self, Self::Error> {
        serializer.maybe_decrypt_value(indexed.content)
    }
}

/// The value associated with the [primary key](IndexedChunk::id) of the
/// [`LINKED_CHUNKS`][1] object store, which is constructed from:
///
/// - The (possibly) hashed Linked Chunk ID
/// - The Chunk ID.
///
/// [1]: crate::event_cache_store::migrations::v1::create_linked_chunks_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedChunkIdKey(IndexedLinkedChunkId, IndexedChunkId);

impl IndexedKey<Chunk> for IndexedChunkIdKey {
    type KeyComponents<'a> = (LinkedChunkId<'a>, ChunkIdentifier);

    fn encode(
        (linked_chunk_id, chunk_id): Self::KeyComponents<'_>,
        serializer: &IndexeddbSerializer,
    ) -> Self {
        let linked_chunk_id =
            serializer.hash_key(keys::LINKED_CHUNK_IDS, linked_chunk_id.storage_key());
        let chunk_id = chunk_id.index();
        Self(linked_chunk_id, chunk_id)
    }
}

impl<'a> IndexedPrefixKeyComponentBounds<'a, Chunk, LinkedChunkId<'a>> for IndexedChunkIdKey {
    fn lower_key_components_with_prefix(
        linked_chunk_id: LinkedChunkId<'a>,
    ) -> Self::KeyComponents<'a> {
        (linked_chunk_id, *INDEXED_KEY_LOWER_CHUNK_IDENTIFIER)
    }

    fn upper_key_components_with_prefix(
        linked_chunk_id: LinkedChunkId<'a>,
    ) -> Self::KeyComponents<'a> {
        (linked_chunk_id, *INDEXED_KEY_UPPER_CHUNK_IDENTIFIER)
    }
}

/// The value associated with the [`next`](IndexedChunk::next) index of the
/// [`LINKED_CHUNKS`][1] object store, which is constructed from:
///
/// - The (possibly) hashed Linked Chunk ID
/// - The Chunk ID, if there is a next chunk in the list.
///
/// Note: it would be more convenient to represent this type with an optional
/// Chunk ID, but unfortunately, this creates an issue when querying for objects
/// that don't have a `next` value, because `None` serializes to `null` which
/// is an invalid value in any part of an IndexedDB query.
///
/// Furthermore, each variant must serialize to the same type, so the `None`
/// variant must contain a non-empty tuple.
///
/// [1]: crate::event_cache_store::migrations::v1::create_linked_chunks_object_store
#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum IndexedNextChunkIdKey {
    /// There is no next chunk.
    None((IndexedLinkedChunkId,)),
    /// The identifier of the next chunk in the list.
    Some(IndexedChunkIdKey),
}

impl IndexedNextChunkIdKey {
    pub fn none(linked_chunk_id: IndexedLinkedChunkId) -> Self {
        Self::None((linked_chunk_id,))
    }
}

impl IndexedKey<Chunk> for IndexedNextChunkIdKey {
    const INDEX: Option<&'static str> = Some(keys::LINKED_CHUNKS_NEXT);

    type KeyComponents<'a> = (LinkedChunkId<'a>, Option<ChunkIdentifier>);

    fn encode(
        (linked_chunk_id, next_chunk_id): Self::KeyComponents<'_>,
        serializer: &IndexeddbSerializer,
    ) -> Self {
        next_chunk_id
            .map(|id| {
                Self::Some(<IndexedChunkIdKey as IndexedKey<Chunk>>::encode(
                    (linked_chunk_id, id),
                    serializer,
                ))
            })
            .unwrap_or_else(|| {
                let room_id =
                    serializer.hash_key(keys::LINKED_CHUNK_IDS, linked_chunk_id.storage_key());
                Self::none(room_id)
            })
    }
}

impl<'a> IndexedPrefixKeyComponentBounds<'a, Chunk, LinkedChunkId<'a>> for IndexedNextChunkIdKey {
    fn lower_key_components_with_prefix(
        linked_chunk_id: LinkedChunkId<'a>,
    ) -> Self::KeyComponents<'a> {
        (linked_chunk_id, None)
    }

    fn upper_key_components_with_prefix(
        linked_chunk_id: LinkedChunkId<'a>,
    ) -> Self::KeyComponents<'a> {
        (linked_chunk_id, Some(*INDEXED_KEY_UPPER_CHUNK_IDENTIFIER))
    }
}

/// Represents the [`EVENTS`][1] object store.
///
/// [1]: crate::event_cache_store::migrations::v1::create_events_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedEvent {
    /// The primary key of the object store.
    pub id: IndexedEventIdKey,
    /// An indexed key on the object store, which represents the room in which
    /// the event exists
    pub room: IndexedEventRoomKey,
    /// An indexed key on the object store, which represents the position of the
    /// event, if it is in a chunk.
    pub position: Option<IndexedEventPositionKey>,
    /// An indexed key on the object store, which represents the relationship
    /// between this event and another event, if one exists.
    pub relation: Option<IndexedEventRelationKey>,
    /// The (possibly) encrypted content of the event.
    pub content: IndexedEventContent,
}

#[derive(Debug, Error)]
pub enum IndexedEventError {
    #[error("no event id")]
    NoEventId,
    #[error("crypto store: {0}")]
    CryptoStore(#[from] CryptoStoreError),
}

impl Indexed for Event {
    const OBJECT_STORE: &'static str = keys::EVENTS;

    type IndexedType = IndexedEvent;
    type Error = IndexedEventError;

    fn to_indexed(
        &self,
        serializer: &IndexeddbSerializer,
    ) -> Result<Self::IndexedType, Self::Error> {
        let event_id = self.event_id().ok_or(Self::Error::NoEventId)?;
        let id = IndexedEventIdKey::encode((self.linked_chunk_id(), &event_id), serializer);
        let room = IndexedEventRoomKey::encode((self.room_id(), &event_id), serializer);
        let position = self.position().map(|position| {
            IndexedEventPositionKey::encode((self.linked_chunk_id(), position), serializer)
        });
        let relation = self.relation().map(|(related_event, relation_type)| {
            IndexedEventRelationKey::encode(
                (self.room_id(), &related_event, &RelationType::from(relation_type)),
                serializer,
            )
        });
        Ok(IndexedEvent {
            id,
            room,
            position,
            relation,
            content: serializer.maybe_encrypt_value(self)?,
        })
    }

    fn from_indexed(
        indexed: Self::IndexedType,
        serializer: &IndexeddbSerializer,
    ) -> Result<Self, Self::Error> {
        serializer.maybe_decrypt_value(indexed.content).map_err(Into::into)
    }
}

/// The value associated with the [primary key](IndexedEvent::id) of the
/// [`EVENTS`][1] object store, which is constructed from:
///
/// - The (possibly) hashed Linked Chunk ID
/// - The (possibly) hashed Event ID.
///
/// [1]: crate::event_cache_store::migrations::v1::create_events_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedEventIdKey(IndexedLinkedChunkId, IndexedEventId);

impl IndexedKey<Event> for IndexedEventIdKey {
    type KeyComponents<'a> = (LinkedChunkId<'a>, &'a EventId);

    fn encode(
        (linked_chunk_id, event_id): Self::KeyComponents<'_>,
        serializer: &IndexeddbSerializer,
    ) -> Self {
        let linked_chunk_id =
            serializer.hash_key(keys::LINKED_CHUNK_IDS, linked_chunk_id.storage_key());
        let event_id = serializer.encode_key_as_string(keys::EVENTS, event_id);
        Self(linked_chunk_id, event_id)
    }
}

impl IndexedPrefixKeyBounds<Event, LinkedChunkId<'_>> for IndexedEventIdKey {
    fn lower_key_with_prefix(
        linked_chunk_id: LinkedChunkId<'_>,
        serializer: &IndexeddbSerializer,
    ) -> Self {
        Self::encode((linked_chunk_id, &*INDEXED_KEY_LOWER_EVENT_ID), serializer)
    }

    fn upper_key_with_prefix(
        linked_chunk_id: LinkedChunkId<'_>,
        serializer: &IndexeddbSerializer,
    ) -> Self {
        Self::encode((linked_chunk_id, &*INDEXED_KEY_UPPER_EVENT_ID), serializer)
    }
}

/// The value associated with the [primary key](IndexedEvent::id) of the
/// [`EVENTS`][1] object store, which is constructed from:
///
/// - The (possibly) hashed Room ID
/// - The (possibly) hashed Event ID.
///
/// [1]: crate::event_cache_store::migrations::v1::create_events_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedEventRoomKey(IndexedRoomId, IndexedEventId);

impl IndexedKey<Event> for IndexedEventRoomKey {
    const INDEX: Option<&'static str> = Some(keys::EVENTS_ROOM);

    type KeyComponents<'a> = (&'a RoomId, &'a EventId);

    fn encode(
        (room_id, event_id): Self::KeyComponents<'_>,
        serializer: &IndexeddbSerializer,
    ) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id.as_str());
        let event_id = serializer.encode_key_as_string(keys::EVENTS, event_id);
        Self(room_id, event_id)
    }
}

impl IndexedPrefixKeyBounds<Event, &RoomId> for IndexedEventRoomKey {
    fn lower_key_with_prefix(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self {
        Self::encode((room_id, &*INDEXED_KEY_LOWER_EVENT_ID), serializer)
    }

    fn upper_key_with_prefix(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self {
        Self::encode((room_id, &*INDEXED_KEY_UPPER_EVENT_ID), serializer)
    }
}

/// The value associated with the [`position`](IndexedEvent::position) index of
/// the [`EVENTS`][1] object store, which is constructed from:
///
/// - The (possibly) hashed Linked Chunk ID
/// - The Chunk ID
/// - The index of the event in the chunk.
///
/// [1]: crate::event_cache_store::migrations::v1::create_events_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedEventPositionKey(IndexedLinkedChunkId, IndexedChunkId, IndexedEventPositionIndex);

impl IndexedKey<Event> for IndexedEventPositionKey {
    const INDEX: Option<&'static str> = Some(keys::EVENTS_POSITION);

    type KeyComponents<'a> = (LinkedChunkId<'a>, Position);

    fn encode(
        (linked_chunk_id, position): Self::KeyComponents<'_>,
        serializer: &IndexeddbSerializer,
    ) -> Self {
        let linked_chunk_id =
            serializer.hash_key(keys::LINKED_CHUNK_IDS, linked_chunk_id.storage_key());
        Self(linked_chunk_id, position.chunk_identifier, position.index)
    }
}

impl<'a> IndexedPrefixKeyComponentBounds<'a, Event, LinkedChunkId<'a>> for IndexedEventPositionKey {
    fn lower_key_components_with_prefix(
        linked_chunk_id: LinkedChunkId<'a>,
    ) -> Self::KeyComponents<'a> {
        (linked_chunk_id, *INDEXED_KEY_LOWER_EVENT_POSITION)
    }

    fn upper_key_components_with_prefix(
        linked_chunk_id: LinkedChunkId<'a>,
    ) -> Self::KeyComponents<'a> {
        (linked_chunk_id, *INDEXED_KEY_UPPER_EVENT_POSITION)
    }
}

impl<'a> IndexedPrefixKeyComponentBounds<'a, Event, (LinkedChunkId<'a>, ChunkIdentifier)>
    for IndexedEventPositionKey
{
    fn lower_key_components_with_prefix(
        (linked_chunk_id, chunk_id): (LinkedChunkId<'a>, ChunkIdentifier),
    ) -> Self::KeyComponents<'a> {
        (
            linked_chunk_id,
            Position { chunk_identifier: chunk_id.index(), index: INDEXED_KEY_LOWER_EVENT_INDEX },
        )
    }

    fn upper_key_components_with_prefix(
        (linked_chunk_id, chunk_id): (LinkedChunkId<'a>, ChunkIdentifier),
    ) -> Self::KeyComponents<'a> {
        (
            linked_chunk_id,
            Position { chunk_identifier: chunk_id.index(), index: INDEXED_KEY_UPPER_EVENT_INDEX },
        )
    }
}

/// The value associated with the [`relation`](IndexedEvent::relation) index of
/// the [`EVENTS`][1] object store, which is constructed from:
///
/// - The (possibly) hashed Room ID
/// - The (possibly) hashed Event ID of the related event
/// - The type of relationship between the events
///
/// [1]: crate::event_cache_store::migrations::v1::create_events_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedEventRelationKey(IndexedRoomId, IndexedEventId, IndexedRelationType);

impl IndexedKey<Event> for IndexedEventRelationKey {
    const INDEX: Option<&'static str> = Some(keys::EVENTS_RELATION);

    type KeyComponents<'a> = (&'a RoomId, &'a EventId, &'a RelationType);

    fn encode(
        (room_id, related_event_id, relation_type): Self::KeyComponents<'_>,
        serializer: &IndexeddbSerializer,
    ) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id);
        let related_event_id =
            serializer.encode_key_as_string(keys::EVENTS_RELATION_RELATED_EVENTS, related_event_id);
        let relation_type = serializer
            .encode_key_as_string(keys::EVENTS_RELATION_RELATION_TYPES, relation_type.to_string());
        Self(room_id, related_event_id, relation_type)
    }
}

impl IndexedPrefixKeyBounds<Event, &RoomId> for IndexedEventRelationKey {
    fn lower_key_with_prefix(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id);
        let related_event_id = String::from(INDEXED_KEY_LOWER_CHARACTER);
        let relation_type = String::from(INDEXED_KEY_LOWER_CHARACTER);
        Self(room_id, related_event_id, relation_type)
    }

    fn upper_key_with_prefix(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id);
        let related_event_id = String::from(INDEXED_KEY_UPPER_CHARACTER);
        let relation_type = String::from(INDEXED_KEY_UPPER_CHARACTER);
        Self(room_id, related_event_id, relation_type)
    }
}

impl IndexedPrefixKeyBounds<Event, (&RoomId, &EventId)> for IndexedEventRelationKey {
    fn lower_key_with_prefix(
        (room_id, related_event_id): (&RoomId, &EventId),
        serializer: &IndexeddbSerializer,
    ) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id);
        let related_event_id =
            serializer.encode_key_as_string(keys::EVENTS_RELATION_RELATED_EVENTS, related_event_id);
        let relation_type = String::from(INDEXED_KEY_LOWER_CHARACTER);
        Self(room_id, related_event_id, relation_type)
    }

    fn upper_key_with_prefix(
        (room_id, related_event_id): (&RoomId, &EventId),
        serializer: &IndexeddbSerializer,
    ) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id);
        let related_event_id =
            serializer.encode_key_as_string(keys::EVENTS_RELATION_RELATED_EVENTS, related_event_id);
        let relation_type = String::from(INDEXED_KEY_UPPER_CHARACTER);
        Self(room_id, related_event_id, relation_type)
    }
}

/// Represents the [`GAPS`][1] object store.
///
/// [1]: crate::event_cache_store::migrations::v1::create_gaps_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedGap {
    /// The primary key of the object store
    pub id: IndexedGapIdKey,
    /// The (possibly) encrypted content of the gap
    pub content: IndexedGapContent,
}

impl Indexed for Gap {
    const OBJECT_STORE: &'static str = keys::GAPS;

    type IndexedType = IndexedGap;
    type Error = CryptoStoreError;

    fn to_indexed(
        &self,
        serializer: &IndexeddbSerializer,
    ) -> Result<Self::IndexedType, Self::Error> {
        Ok(IndexedGap {
            id: <IndexedGapIdKey as IndexedKey<Gap>>::encode(
                (self.linked_chunk_id.as_ref(), ChunkIdentifier::new(self.chunk_identifier)),
                serializer,
            ),
            content: serializer.maybe_encrypt_value(self)?,
        })
    }

    fn from_indexed(
        indexed: Self::IndexedType,
        serializer: &IndexeddbSerializer,
    ) -> Result<Self, Self::Error> {
        serializer.maybe_decrypt_value(indexed.content)
    }
}

/// The primary key of the [`GAPS`][1] object store, which is constructed from:
///
/// - The (possibly) hashed Linked Chunk ID
/// - The Chunk ID
///
/// [1]: crate::event_cache_store::migrations::v1::create_gaps_object_store
pub type IndexedGapIdKey = IndexedChunkIdKey;

impl IndexedKey<Gap> for IndexedGapIdKey {
    type KeyComponents<'a> = <IndexedChunkIdKey as IndexedKey<Chunk>>::KeyComponents<'a>;

    fn encode(components: Self::KeyComponents<'_>, serializer: &IndexeddbSerializer) -> Self {
        <IndexedChunkIdKey as IndexedKey<Chunk>>::encode(components, serializer)
    }
}

impl<'a> IndexedPrefixKeyComponentBounds<'a, Gap, LinkedChunkId<'a>> for IndexedGapIdKey {
    fn lower_key_components_with_prefix(
        linked_chunk_id: LinkedChunkId<'a>,
    ) -> Self::KeyComponents<'a> {
        <Self as IndexedPrefixKeyComponentBounds<Chunk, _>>::lower_key_components_with_prefix(
            linked_chunk_id,
        )
    }

    fn upper_key_components_with_prefix(
        linked_chunk_id: LinkedChunkId<'a>,
    ) -> Self::KeyComponents<'a> {
        <Self as IndexedPrefixKeyComponentBounds<Chunk, _>>::upper_key_components_with_prefix(
            linked_chunk_id,
        )
    }
}

/// Represents the [`MediaRetentionPolicy`] record in the [`CORE`][1] object
/// store.
///
/// [1]: crate::event_cache_store::migrations::v1::create_core_object_store
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
        serializer: &IndexeddbSerializer,
    ) -> Result<Self::IndexedType, Self::Error> {
        Ok(Self::IndexedType {
            id: <IndexedCoreIdKey as IndexedKey<Self>>::encode((), serializer),
            content: serializer.maybe_encrypt_value(self)?,
        })
    }

    fn from_indexed(
        indexed: Self::IndexedType,
        serializer: &IndexeddbSerializer,
    ) -> Result<Self, Self::Error> {
        serializer.maybe_decrypt_value(indexed.content)
    }
}

impl IndexedKey<MediaRetentionPolicy> for IndexedCoreIdKey {
    type KeyComponents<'a> = ();

    fn encode(components: Self::KeyComponents<'_>, serializer: &IndexeddbSerializer) -> Self {
        serializer.encode_key_as_string(keys::CORE, keys::MEDIA_RETENTION_POLICY_KEY)
    }
}

/// Represents the [`MEDIA`][1] object store.
///
/// [1]: crate::event_cache_store::migrations::v1::create_media_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedMedia {
    /// The primary key of the object store
    pub id: IndexedMediaIdKey,
    /// The size (in bytes) of the media content and whether to ignore the
    /// [`MediaRetentionPolicy`]
    pub content_size: IndexedMediaContentSizeKey,
    /// The (possibly) encrypted metadata - i.e., [`MediaMetadata`][1]
    ///
    /// [1]: crate::event_cache_store::types::MediaMetadata
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
        serializer: &IndexeddbSerializer,
    ) -> Result<Self::IndexedType, Self::Error> {
        let content = rmp_serde::to_vec_named(&serializer.maybe_encrypt_value(&self.content)?)?;
        Ok(Self::IndexedType {
            id: <IndexedMediaIdKey as IndexedKey<Self>>::encode(
                &self.metadata.request_parameters,
                serializer,
            ),
            content_size: IndexedMediaContentSizeKey::encode(
                (self.metadata.ignore_policy, content.len()),
                serializer,
            ),
            metadata: serializer.maybe_encrypt_value(&self.metadata)?,
            content,
        })
    }

    fn from_indexed(
        indexed: Self::IndexedType,
        serializer: &IndexeddbSerializer,
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
/// [1]: crate::event_cache_store::migrations::v1::create_media_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedMediaIdKey(String);

impl IndexedKey<Media> for IndexedMediaIdKey {
    type KeyComponents<'a> = &'a MediaRequestParameters;

    fn encode(components: Self::KeyComponents<'_>, serializer: &IndexeddbSerializer) -> Self {
        Self(serializer.encode_key_as_string(keys::MEDIA, components.unique_key()))
    }
}

/// The value associated with the [`content_size`](IndexedMedia::content_size)
/// index of the [`MEDIA`][1] object store, which is constructed from:
///
/// - The value of [`IgnoreMediaRetentionPolicy`]
/// - The size in bytes of the associated [`IndexedMedia::content`]
///
/// [1]: crate::event_cache_store::migrations::v1::create_media_object_store
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
    type KeyComponents<'a> = (IgnoreMediaRetentionPolicy, IndexedMediaContentSize);

    fn encode(
        (ignore_policy, content_size): Self::KeyComponents<'_>,
        _: &IndexeddbSerializer,
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
        (prefix, IndexedMediaContentSize::MIN)
    }

    fn upper_key_components_with_prefix(
        prefix: IgnoreMediaRetentionPolicy,
    ) -> Self::KeyComponents<'a> {
        (prefix, IndexedMediaContentSize::MAX)
    }
}
