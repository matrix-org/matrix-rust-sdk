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

use matrix_sdk_base::linked_chunk::ChunkIdentifier;
use matrix_sdk_crypto::CryptoStoreError;
use ruma::{events::relation::RelationType, EventId, OwnedEventId, RoomId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    event_cache_store::{
        migrations::current::keys,
        serializer::traits::{Indexed, IndexedKey, IndexedKeyBounds},
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
    type IndexedType = IndexedChunk;
    type Error = CryptoStoreError;

    fn to_indexed(
        &self,
        room_id: &RoomId,
        serializer: &IndexeddbSerializer,
    ) -> Result<Self::IndexedType, Self::Error> {
        Ok(IndexedChunk {
            id: <IndexedChunkIdKey as IndexedKey<Chunk>>::encode(
                room_id,
                &ChunkIdentifier::new(self.identifier),
                serializer,
            ),
            next: IndexedNextChunkIdKey::encode(
                room_id,
                &self.next.map(ChunkIdentifier::new),
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
    type KeyComponents = ChunkIdentifier;

    fn encode(
        room_id: &RoomId,
        chunk_id: &ChunkIdentifier,
        serializer: &IndexeddbSerializer,
    ) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id);
        let chunk_id = chunk_id.index();
        Self(room_id, chunk_id)
    }
}

impl IndexedKeyBounds<Chunk> for IndexedChunkIdKey {
    fn lower_key_components() -> Self::KeyComponents {
        ChunkIdentifier::new(0)
    }

    fn upper_key_components() -> Self::KeyComponents {
        ChunkIdentifier::new(js_sys::Number::MAX_SAFE_INTEGER as u64)
    }

    fn encode_lower(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self {
        <Self as IndexedKey<Chunk>>::encode(room_id, &ChunkIdentifier::new(0), serializer)
    }

    fn encode_upper(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self {
        <Self as IndexedKey<Chunk>>::encode(
            room_id,
            &ChunkIdentifier::new(js_sys::Number::MAX_SAFE_INTEGER as u64),
            serializer,
        )
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
    type KeyComponents = Option<ChunkIdentifier>;

    fn encode(
        room_id: &RoomId,
        next_chunk_id: &Option<ChunkIdentifier>,
        serializer: &IndexeddbSerializer,
    ) -> Self {
        next_chunk_id
            .map(|id| {
                Self::Some(<IndexedChunkIdKey as IndexedKey<Chunk>>::encode(
                    room_id, &id, serializer,
                ))
            })
            .unwrap_or_else(|| {
                let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id);
                Self::none(room_id)
            })
    }
}

impl IndexedKeyBounds<Chunk> for IndexedNextChunkIdKey {
    fn lower_key_components() -> Self::KeyComponents {
        None
    }

    fn upper_key_components() -> Self::KeyComponents {
        Some(ChunkIdentifier::new(js_sys::Number::MAX_SAFE_INTEGER as u64))
    }

    fn encode_lower(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self {
        <Self as IndexedKey<Chunk>>::encode(room_id, &None, serializer)
    }

    fn encode_upper(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self {
        <Self as IndexedKey<Chunk>>::encode(
            room_id,
            &Some(ChunkIdentifier::new(js_sys::Number::MAX_SAFE_INTEGER as u64)),
            serializer,
        )
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
    type IndexedType = IndexedEvent;
    type Error = IndexedEventError;

    fn to_indexed(
        &self,
        room_id: &RoomId,
        serializer: &IndexeddbSerializer,
    ) -> Result<Self::IndexedType, Self::Error> {
        let event_id = self.event_id().ok_or(Self::Error::NoEventId)?;
        let id = IndexedEventIdKey::encode(room_id, &event_id, serializer);
        let position = self
            .position()
            .map(|position| IndexedEventPositionKey::encode(room_id, &position, serializer));
        let relation = self.relation().map(|(related_event, relation_type)| {
            IndexedEventRelationKey::encode(
                room_id,
                &(related_event, RelationType::from(relation_type)),
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
    type KeyComponents = OwnedEventId;

    fn encode(room_id: &RoomId, event_id: &OwnedEventId, serializer: &IndexeddbSerializer) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id);
        let event_id = serializer.encode_key_as_string(keys::EVENTS, event_id);
        Self(room_id, event_id)
    }
}

impl IndexedKeyBounds<Event> for IndexedEventIdKey {
    fn lower_key_components() -> Self::KeyComponents {
        OwnedEventId::try_from(format!("${INDEXED_KEY_LOWER_CHARACTER}")).expect("valid event id")
    }

    fn upper_key_components() -> Self::KeyComponents {
        OwnedEventId::try_from(format!("${INDEXED_KEY_UPPER_CHARACTER}")).expect("valid event id")
    }

    fn encode_lower(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id);
        let event_id = String::from(INDEXED_KEY_LOWER_CHARACTER);
        Self(room_id, event_id)
    }

    fn encode_upper(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id);
        let event_id = String::from(INDEXED_KEY_UPPER_CHARACTER);
        Self(room_id, event_id)
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
    type KeyComponents = Position;

    fn encode(room_id: &RoomId, position: &Position, serializer: &IndexeddbSerializer) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id);
        Self(room_id, position.chunk_identifier, position.index)
    }
}

impl IndexedKeyBounds<Event> for IndexedEventPositionKey {
    fn lower_key_components() -> Self::KeyComponents {
        Position { chunk_identifier: 0, index: 0 }
    }

    fn upper_key_components() -> Self::KeyComponents {
        Position {
            chunk_identifier: js_sys::Number::MAX_SAFE_INTEGER as u64,
            index: js_sys::Number::MAX_SAFE_INTEGER as usize,
        }
    }

    fn encode_lower(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self {
        <Self as IndexedKey<Event>>::encode(
            room_id,
            &Position { chunk_identifier: 0, index: 0 },
            serializer,
        )
    }

    fn encode_upper(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self {
        <Self as IndexedKey<Event>>::encode(
            room_id,
            &Position {
                chunk_identifier: js_sys::Number::MAX_SAFE_INTEGER as u64,
                index: js_sys::Number::MAX_SAFE_INTEGER as usize,
            },
            serializer,
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

impl IndexedKey<Event> for IndexedEventRelationKey {
    type KeyComponents = (OwnedEventId, RelationType);

    fn encode(
        room_id: &RoomId,
        (related_event_id, relation_type): &(OwnedEventId, RelationType),
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

impl IndexedKeyBounds<Event> for IndexedEventRelationKey {
    fn lower_key_components() -> Self::KeyComponents {
        (
            OwnedEventId::try_from(format!("${INDEXED_KEY_LOWER_CHARACTER}"))
                .expect("valid event id"),
            RelationType::from(INDEXED_KEY_LOWER_CHARACTER.to_string()),
        )
    }

    fn upper_key_components() -> Self::KeyComponents {
        (
            OwnedEventId::try_from(format!("${INDEXED_KEY_UPPER_CHARACTER}"))
                .expect("valid event id"),
            RelationType::from(INDEXED_KEY_UPPER_CHARACTER.to_string()),
        )
    }

    fn encode_lower(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id);
        let related_event_id = String::from(INDEXED_KEY_LOWER_CHARACTER);
        let relation_type = String::from(INDEXED_KEY_LOWER_CHARACTER);
        Self(room_id, related_event_id, relation_type)
    }

    fn encode_upper(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self {
        let room_id = serializer.encode_key_as_string(keys::ROOMS, room_id);
        let related_event_id = String::from(INDEXED_KEY_UPPER_CHARACTER);
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
    type IndexedType = IndexedGap;
    type Error = CryptoStoreError;

    fn to_indexed(
        &self,
        room_id: &RoomId,
        serializer: &IndexeddbSerializer,
    ) -> Result<Self::IndexedType, Self::Error> {
        Ok(IndexedGap {
            id: <IndexedGapIdKey as IndexedKey<Gap>>::encode(
                room_id,
                &ChunkIdentifier::new(self.chunk_identifier),
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
    type KeyComponents = <IndexedChunkIdKey as IndexedKey<Chunk>>::KeyComponents;

    fn encode(
        room_id: &RoomId,
        components: &Self::KeyComponents,
        serializer: &IndexeddbSerializer,
    ) -> Self {
        <IndexedChunkIdKey as IndexedKey<Chunk>>::encode(room_id, components, serializer)
    }
}

impl IndexedKeyBounds<Gap> for IndexedGapIdKey {
    fn lower_key_components() -> Self::KeyComponents {
        <Self as IndexedKeyBounds<Chunk>>::lower_key_components()
    }

    fn upper_key_components() -> Self::KeyComponents {
        <Self as IndexedKeyBounds<Chunk>>::upper_key_components()
    }

    fn encode_lower(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self {
        <IndexedChunkIdKey as IndexedKeyBounds<Chunk>>::encode_lower(room_id, serializer)
    }

    fn encode_upper(room_id: &RoomId, serializer: &IndexeddbSerializer) -> Self {
        <IndexedChunkIdKey as IndexedKeyBounds<Chunk>>::encode_upper(room_id, serializer)
    }
}

pub type IndexedGapContent = MaybeEncrypted;
