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

use matrix_sdk_base::linked_chunk::{ChunkIdentifier, LinkedChunkId};
use matrix_sdk_crypto::CryptoStoreError;
use ruma::{EventId, RoomId, events::relation::RelationType};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    event_cache_store::{
        migrations::current::keys,
        serializer::constants::{
            INDEXED_KEY_LOWER_CHUNK_IDENTIFIER, INDEXED_KEY_LOWER_EVENT_INDEX,
            INDEXED_KEY_LOWER_EVENT_POSITION, INDEXED_KEY_UPPER_CHUNK_IDENTIFIER,
            INDEXED_KEY_UPPER_EVENT_INDEX, INDEXED_KEY_UPPER_EVENT_POSITION,
        },
        types::{Chunk, Event, Gap, Lease, Position},
    },
    serializer::{
        indexed_type::{
            constants::{
                INDEXED_KEY_LOWER_CHARACTER, INDEXED_KEY_LOWER_STRING, INDEXED_KEY_UPPER_CHARACTER,
                INDEXED_KEY_UPPER_STRING,
            },
            traits::{
                Indexed, IndexedKey, IndexedKeyComponentBounds, IndexedPrefixKeyBounds,
                IndexedPrefixKeyComponentBounds,
            },
        },
        safe_encode::types::{MaybeEncrypted, SafeEncodeSerializer},
    },
};

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
/// [1]: crate::event_cache_store::migrations::v1::create_linked_chunks_object_store
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
        serializer: &SafeEncodeSerializer,
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
        serializer: &SafeEncodeSerializer,
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
        serializer: &SafeEncodeSerializer,
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
        serializer: &SafeEncodeSerializer,
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
        serializer: &SafeEncodeSerializer,
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
        serializer: &SafeEncodeSerializer,
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
        serializer: &SafeEncodeSerializer,
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
        serializer: &SafeEncodeSerializer,
    ) -> Self {
        let linked_chunk_id =
            serializer.hash_key(keys::LINKED_CHUNK_IDS, linked_chunk_id.storage_key());
        Self(linked_chunk_id, (*INDEXED_KEY_LOWER_STRING).clone())
    }

    fn upper_key_with_prefix(
        linked_chunk_id: LinkedChunkId<'_>,
        serializer: &SafeEncodeSerializer,
    ) -> Self {
        let linked_chunk_id =
            serializer.hash_key(keys::LINKED_CHUNK_IDS, linked_chunk_id.storage_key());
        Self(linked_chunk_id, (*INDEXED_KEY_UPPER_STRING).clone())
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
        serializer: &SafeEncodeSerializer,
    ) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id.as_str());
        let event_id = serializer.encode_key_as_string(keys::EVENTS, event_id);
        Self(room_id, event_id)
    }
}

impl IndexedPrefixKeyBounds<Event, &RoomId> for IndexedEventRoomKey {
    fn lower_key_with_prefix(room_id: &RoomId, serializer: &SafeEncodeSerializer) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id.as_str());
        Self(room_id, (*INDEXED_KEY_LOWER_STRING).clone())
    }

    fn upper_key_with_prefix(room_id: &RoomId, serializer: &SafeEncodeSerializer) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id.as_str());
        Self(room_id, (*INDEXED_KEY_UPPER_STRING).clone())
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
        serializer: &SafeEncodeSerializer,
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
        serializer: &SafeEncodeSerializer,
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
    fn lower_key_with_prefix(room_id: &RoomId, serializer: &SafeEncodeSerializer) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id);
        let related_event_id = String::from(INDEXED_KEY_LOWER_CHARACTER);
        let relation_type = String::from(INDEXED_KEY_LOWER_CHARACTER);
        Self(room_id, related_event_id, relation_type)
    }

    fn upper_key_with_prefix(room_id: &RoomId, serializer: &SafeEncodeSerializer) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id);
        let related_event_id = String::from(INDEXED_KEY_UPPER_CHARACTER);
        let relation_type = String::from(INDEXED_KEY_UPPER_CHARACTER);
        Self(room_id, related_event_id, relation_type)
    }
}

impl IndexedPrefixKeyBounds<Event, (&RoomId, &EventId)> for IndexedEventRelationKey {
    fn lower_key_with_prefix(
        (room_id, related_event_id): (&RoomId, &EventId),
        serializer: &SafeEncodeSerializer,
    ) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id);
        let related_event_id =
            serializer.encode_key_as_string(keys::EVENTS_RELATION_RELATED_EVENTS, related_event_id);
        let relation_type = String::from(INDEXED_KEY_LOWER_CHARACTER);
        Self(room_id, related_event_id, relation_type)
    }

    fn upper_key_with_prefix(
        (room_id, related_event_id): (&RoomId, &EventId),
        serializer: &SafeEncodeSerializer,
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
        serializer: &SafeEncodeSerializer,
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
        serializer: &SafeEncodeSerializer,
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

    fn encode(components: Self::KeyComponents<'_>, serializer: &SafeEncodeSerializer) -> Self {
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
