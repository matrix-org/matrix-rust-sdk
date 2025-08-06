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

use matrix_sdk_base::linked_chunk::ChunkIdentifier;
use matrix_sdk_crypto::CryptoStoreError;
use ruma::{events::relation::RelationType, EventId, OwnedEventId, RoomId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    event_cache_store::{
        migrations::current::keys,
        serializer::traits::{
            Indexed, IndexedKey, IndexedKeyBounds, IndexedKeyComponentBounds,
            IndexedPrefixKeyBounds, IndexedPrefixKeyComponentBounds,
        },
        types::{Chunk, Event, Gap, Position},
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
                (&self.room_id, ChunkIdentifier::new(self.identifier)),
                serializer,
            ),
            next: IndexedNextChunkIdKey::encode(
                (&self.room_id, self.next.map(ChunkIdentifier::new)),
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
/// - The (possibly) encrypted Room ID
/// - The Chunk ID.
///
/// [1]: crate::event_cache_store::migrations::v1::create_linked_chunks_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedChunkIdKey(IndexedRoomId, IndexedChunkId);

impl IndexedKey<Chunk> for IndexedChunkIdKey {
    type KeyComponents<'a> = (&'a RoomId, ChunkIdentifier);

    fn encode(
        (room_id, chunk_id): Self::KeyComponents<'_>,
        serializer: &IndexeddbSerializer,
    ) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id);
        let chunk_id = chunk_id.index();
        Self(room_id, chunk_id)
    }
}

impl<'a> IndexedPrefixKeyComponentBounds<'a, Chunk, &'a RoomId> for IndexedChunkIdKey {
    fn lower_key_components_with_prefix(room_id: &'a RoomId) -> Self::KeyComponents<'a> {
        (room_id, *INDEXED_KEY_LOWER_CHUNK_IDENTIFIER)
    }

    fn upper_key_components_with_prefix(room_id: &'a RoomId) -> Self::KeyComponents<'a> {
        (room_id, *INDEXED_KEY_UPPER_CHUNK_IDENTIFIER)
    }
}

pub type IndexedRoomId = String;
pub type IndexedChunkId = u64;
pub type IndexedChunkContent = MaybeEncrypted;

/// The value associated with the [`next`](IndexedChunk::next) index of the
/// [`LINKED_CHUNKS`][1] object store, which is constructed from:
///
/// - The (possibly) encrypted Room ID
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
    None((IndexedRoomId,)),
    /// The identifier of the next chunk in the list.
    Some(IndexedChunkIdKey),
}

impl IndexedNextChunkIdKey {
    pub fn none(room_id: IndexedRoomId) -> Self {
        Self::None((room_id,))
    }
}

impl IndexedKey<Chunk> for IndexedNextChunkIdKey {
    const INDEX: Option<&'static str> = Some(keys::LINKED_CHUNKS_NEXT);

    type KeyComponents<'a> = (&'a RoomId, Option<ChunkIdentifier>);

    fn encode(
        (room_id, next_chunk_id): Self::KeyComponents<'_>,
        serializer: &IndexeddbSerializer,
    ) -> Self {
        next_chunk_id
            .map(|id| {
                Self::Some(<IndexedChunkIdKey as IndexedKey<Chunk>>::encode(
                    (room_id, id),
                    serializer,
                ))
            })
            .unwrap_or_else(|| {
                let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id);
                Self::none(room_id)
            })
    }
}

impl<'a> IndexedPrefixKeyComponentBounds<'a, Chunk, &'a RoomId> for IndexedNextChunkIdKey {
    fn lower_key_components_with_prefix(room_id: &'a RoomId) -> Self::KeyComponents<'a> {
        (room_id, None)
    }

    fn upper_key_components_with_prefix(room_id: &'a RoomId) -> Self::KeyComponents<'a> {
        (room_id, Some(*INDEXED_KEY_UPPER_CHUNK_IDENTIFIER))
    }
}

/// Represents the [`EVENTS`][1] object store.
///
/// [1]: crate::event_cache_store::migrations::v1::create_events_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedEvent {
    /// The primary key of the object store.
    pub id: IndexedEventIdKey,
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
        let id = IndexedEventIdKey::encode((self.room_id(), &event_id), serializer);
        let position = self.position().map(|position| {
            IndexedEventPositionKey::encode((self.room_id(), position), serializer)
        });
        let relation = self.relation().map(|(related_event, relation_type)| {
            IndexedEventRelationKey::encode(
                (self.room_id(), &related_event, &RelationType::from(relation_type)),
                serializer,
            )
        });
        Ok(IndexedEvent { id, position, relation, content: serializer.maybe_encrypt_value(self)? })
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
/// - The (possibly) encrypted Room ID
/// - The (possibly) encrypted Event ID.
///
/// [1]: crate::event_cache_store::migrations::v1::create_events_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedEventIdKey(IndexedRoomId, IndexedEventId);

impl IndexedKey<Event> for IndexedEventIdKey {
    type KeyComponents<'a> = (&'a RoomId, &'a EventId);

    fn encode((room_id, event_id): (&RoomId, &EventId), serializer: &IndexeddbSerializer) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id);
        let event_id = serializer.encode_key_as_string(keys::EVENTS, event_id);
        Self(room_id, event_id)
    }
}

impl IndexedPrefixKeyBounds<Event, &RoomId> for IndexedEventIdKey {
    fn lower_key_with_prefix(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self {
        Self::encode((room_id, &*INDEXED_KEY_LOWER_EVENT_ID), serializer)
    }

    fn upper_key_with_prefix(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self {
        Self::encode((room_id, &*INDEXED_KEY_UPPER_EVENT_ID), serializer)
    }
}

pub type IndexedEventId = String;

/// The value associated with the [`position`](IndexedEvent::position) index of
/// the [`EVENTS`][1] object store, which is constructed from:
///
/// - The (possibly) encrypted Room ID
/// - The Chunk ID
/// - The index of the event in the chunk.
///
/// [1]: crate::event_cache_store::migrations::v1::create_events_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedEventPositionKey(IndexedRoomId, IndexedChunkId, IndexedEventPositionIndex);

impl IndexedKey<Event> for IndexedEventPositionKey {
    const INDEX: Option<&'static str> = Some(keys::EVENTS_POSITION);

    type KeyComponents<'a> = (&'a RoomId, Position);

    fn encode(
        (room_id, position): Self::KeyComponents<'_>,
        serializer: &IndexeddbSerializer,
    ) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id);
        Self(room_id, position.chunk_identifier, position.index)
    }
}

impl<'a> IndexedPrefixKeyComponentBounds<'a, Event, &'a RoomId> for IndexedEventPositionKey {
    fn lower_key_components_with_prefix(room_id: &'a RoomId) -> Self::KeyComponents<'a> {
        (room_id, *INDEXED_KEY_LOWER_EVENT_POSITION)
    }

    fn upper_key_components_with_prefix(room_id: &'a RoomId) -> Self::KeyComponents<'a> {
        (room_id, *INDEXED_KEY_UPPER_EVENT_POSITION)
    }
}

impl<'a> IndexedPrefixKeyComponentBounds<'a, Event, (&'a RoomId, ChunkIdentifier)>
    for IndexedEventPositionKey
{
    fn lower_key_components_with_prefix(
        (room_id, chunk_id): (&'a RoomId, ChunkIdentifier),
    ) -> Self::KeyComponents<'a> {
        (
            room_id,
            Position { chunk_identifier: chunk_id.index(), index: INDEXED_KEY_LOWER_EVENT_INDEX },
        )
    }

    fn upper_key_components_with_prefix(
        (room_id, chunk_id): (&'a RoomId, ChunkIdentifier),
    ) -> Self::KeyComponents<'a> {
        (
            room_id,
            Position { chunk_identifier: chunk_id.index(), index: INDEXED_KEY_UPPER_EVENT_INDEX },
        )
    }
}

pub type IndexedEventPositionIndex = usize;

/// The value associated with the [`relation`](IndexedEvent::relation) index of
/// the [`EVENTS`][1] object store, which is constructed from:
///
/// - The (possibly) encrypted Room ID
/// - The (possibly) encrypted Event ID of the related event
/// - The type of relationship between the events
///
/// [1]: crate::event_cache_store::migrations::v1::create_events_object_store
#[derive(Debug, Serialize, Deserialize)]
pub struct IndexedEventRelationKey(IndexedRoomId, IndexedEventId, IndexedRelationType);

impl IndexedEventRelationKey {
    /// Returns an identical key, but with the related event field updated to
    /// the given related event. This is helpful when searching for all
    /// events which are related to the given event.
    pub fn with_related_event_id(
        &self,
        related_event_id: &EventId,
        serializer: &IndexeddbSerializer,
    ) -> Self {
        let room_id = self.0.clone();
        let related_event_id =
            serializer.encode_key_as_string(keys::EVENTS_RELATION_RELATED_EVENTS, related_event_id);
        let relation_type = self.2.clone();
        Self(room_id, related_event_id, relation_type)
    }
}

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

/// A representation of the relationship between two events (see
/// [`RelationType`](ruma::events::relation::RelationType))
pub type IndexedRelationType = String;

pub type IndexedEventContent = MaybeEncrypted;

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
                (&self.room_id, ChunkIdentifier::new(self.chunk_identifier)),
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
/// - The (possibly) encrypted Room ID
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

impl<'a> IndexedPrefixKeyComponentBounds<'a, Gap, &'a RoomId> for IndexedGapIdKey {
    fn lower_key_components_with_prefix(room_id: &'a RoomId) -> Self::KeyComponents<'a> {
        <Self as IndexedPrefixKeyComponentBounds<Chunk, _>>::lower_key_components_with_prefix(
            room_id,
        )
    }

    fn upper_key_components_with_prefix(room_id: &'a RoomId) -> Self::KeyComponents<'a> {
        <Self as IndexedPrefixKeyComponentBounds<Chunk, _>>::upper_key_components_with_prefix(
            room_id,
        )
    }
}

pub type IndexedGapContent = MaybeEncrypted;
