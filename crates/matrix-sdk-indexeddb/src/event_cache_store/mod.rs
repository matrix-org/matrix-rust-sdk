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

mod builder;
mod migrations;

use std::future::IntoFuture;

use async_trait::async_trait;
pub use builder::IndexeddbEventCacheStoreBuilder;
use gloo_utils::format::JsValueSerdeExt;
use indexed_db_futures::{IdbDatabase, IdbQuerySource};
use matrix_sdk_base::{
    deserialized_responses::TimelineEvent,
    event_cache::{
        store::{
            extract_event_relation,
            media::{IgnoreMediaRetentionPolicy, MediaRetentionPolicy},
            EventCacheStore, EventCacheStoreError, MemoryStore,
        },
        Event, Gap,
    },
    linked_chunk::{self, ChunkIdentifier, ChunkIdentifierGenerator, Position, RawChunk, Update},
    media::MediaRequestParameters,
};
use matrix_sdk_crypto::CryptoStoreError;
use ruma::{
    events::relation::RelationType, EventId, MilliSecondsSinceUnixEpoch, MxcUri, OwnedEventId,
    RoomId,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tracing::{error, trace};
use wasm_bindgen::JsValue;
use web_sys::{IdbKeyRange, IdbTransactionMode};

use crate::serializer::{IndexeddbSerializer, IndexeddbSerializerError, MaybeEncrypted};

mod keys {
    pub const CORE: &str = "core";

    // Rooms is not used as an object store, but it
    // is used for the purposes of secure hashing when
    // constructing keys for object stores.
    pub const ROOMS: &str = "rooms";
    pub const EVENTS: &str = "events";
    pub const EVENT_POSITIONS: &str = "event_positions";
    pub const LINKED_CHUNKS: &str = "linked_chunks";
    pub const GAPS: &str = "gaps";
}

#[derive(Debug, thiserror::Error)]
pub enum IndexeddbEventCacheStoreError {
    #[error(transparent)]
    Serialization(#[from] IndexeddbSerializerError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("DomException {name} ({code}): {message}")]
    DomException { name: String, message: String, code: u16 },
    #[error("unsupported")]
    Unsupported,
    #[error(transparent)]
    CryptoStoreError(#[from] CryptoStoreError),
    #[error("unknown chunk type: {0}")]
    UnknownChunkType(String),
    #[error("chunks contain cycle")]
    ChunksContainCycle,
    #[error("chunks contain disjoint lists")]
    ChunksContainDisjointLists,
    #[error("no event id")]
    NoEventId,
    #[error("media store: {0}")]
    MediaStore(#[from] EventCacheStoreError),
}

impl From<web_sys::DomException> for IndexeddbEventCacheStoreError {
    fn from(frm: web_sys::DomException) -> IndexeddbEventCacheStoreError {
        IndexeddbEventCacheStoreError::DomException {
            name: frm.name(),
            message: frm.message(),
            code: frm.code(),
        }
    }
}

impl From<serde_wasm_bindgen::Error> for IndexeddbEventCacheStoreError {
    fn from(e: serde_wasm_bindgen::Error) -> Self {
        IndexeddbEventCacheStoreError::Json(serde::de::Error::custom(e.to_string()))
    }
}

impl From<IndexeddbEventCacheStoreError> for EventCacheStoreError {
    fn from(e: IndexeddbEventCacheStoreError) -> Self {
        match e {
            IndexeddbEventCacheStoreError::Serialization(
                IndexeddbSerializerError::Serialization(e),
            ) => EventCacheStoreError::Serialization(e),
            IndexeddbEventCacheStoreError::MediaStore(e) => e,
            IndexeddbEventCacheStoreError::Json(e) => EventCacheStoreError::Serialization(e),
            _ => EventCacheStoreError::backend(e),
        }
    }
}

type Result<T, E = IndexeddbEventCacheStoreError> = std::result::Result<T, E>;

#[derive(Debug)]
pub struct IndexeddbEventCacheStore {
    inner: IdbDatabase,
    serializer: IndexeddbSerializer,
    media_store: MemoryStore,
}

/// A type that wraps a (de)serialized value `value` and associates it
/// with an identifier, `id`.
///
/// This is useful for (de)serializing values to/from an object store
/// and ensuring that they are well-formed, as each of the object stores
/// uses `id` as its key path.
#[derive(Debug, Deserialize, Serialize)]
struct ValueWithId {
    id: String,
    value: MaybeEncrypted,
}

impl IndexeddbEventCacheStore {
    pub const KEY_SEPARATOR: char = '\u{001D}';
    pub const KEY_LOWER_CHARACTER: char = '\u{0000}';
    pub const KEY_UPPER_CHARACTER: char = '\u{FFFF}';

    pub fn builder() -> IndexeddbEventCacheStoreBuilder {
        IndexeddbEventCacheStoreBuilder::new()
    }

    /// Encodes each tuple in `parts` as a key and then joins them with
    /// `Self::KEY_SEPARATOR`.
    ///
    /// Each tuple is composed of three fields.
    ///
    /// - `&str` - name of an object store
    /// - `&str` - key of an object in the object store
    /// - `bool` - whether to encrypt the fields above in the final key
    ///
    /// Selective encryption is employed to maintain ordering, so that range
    /// queries are possible.
    pub fn encode_key(&self, parts: Vec<(&str, &str, bool)>) -> String {
        let mut end_key = String::new();
        for (i, (table_name, key, should_encrypt)) in parts.into_iter().enumerate() {
            if i > 0 {
                end_key.push(Self::KEY_SEPARATOR);
            }
            let encoded_key = if should_encrypt {
                self.serializer.encode_key_as_string(table_name, key)
            } else {
                key.to_owned()
            };
            end_key.push_str(&encoded_key);
        }
        end_key
    }

    /// Same as `Self::encode_key`, but appends `Self::KEY_SEPARATOR` and
    /// `Self::KEY_UPPER_CHARACTER`.
    ///
    /// This is useful when constructing range queries, as it provides an upper
    /// key for a collection of `parts`.
    pub fn encode_upper_key(&self, parts: Vec<(&str, &str, bool)>) -> String {
        let mut key = self.encode_key(parts);
        key.push(Self::KEY_SEPARATOR);
        key.push(Self::KEY_UPPER_CHARACTER);
        key
    }

    /// Serializes `value` and wraps it with a `ValueWithId` using `id`.
    ///
    /// This helps to ensure that values are well-formed before putting them
    /// into an object store, as each of the object stores uses `id` as its key
    /// path.
    pub fn serialize_value_with_id(
        &self,
        id: &str,
        value: &impl Serialize,
    ) -> Result<JsValue, IndexeddbEventCacheStoreError> {
        let serialized = self.serializer.maybe_encrypt_value(value)?;
        let res_obj = ValueWithId { id: id.to_owned(), value: serialized };
        Ok(serde_wasm_bindgen::to_value(&res_obj)?)
    }

    /// Deserializes a `value` as a `ValueWithId` and then returns the result of
    /// deserializing the inner `value`.
    ///
    /// The corresponding serialization function, `serialize_value_with_id`
    /// helps to ensure that values are well-formed before putting them into
    /// an object store, as each of the object stores uses `id` as its key
    /// path.
    pub fn deserialize_value_with_id<T: DeserializeOwned>(
        &self,
        value: JsValue,
    ) -> Result<T, IndexeddbEventCacheStoreError> {
        let obj: ValueWithId = value.into_serde()?;
        let deserialized: T = self.serializer.maybe_decrypt_value(obj.value)?;
        Ok(deserialized)
    }

    fn encode_in_band_event_key(&self, room_id: &str, position: &PositionForCache) -> String {
        self.encode_key(vec![
            (keys::ROOMS, room_id, true),
            (keys::LINKED_CHUNKS, &position.chunk_id.to_string(), false),
            (keys::EVENTS, &position.index.to_string(), false),
        ])
    }

    fn encode_upper_in_band_event_key_for_room(&self, room_id: &str) -> String {
        self.encode_upper_key(vec![(keys::ROOMS, room_id, true)])
    }

    fn encode_upper_in_band_event_key_for_chunk(&self, room_id: &str, chunk_id: u64) -> String {
        self.encode_upper_key(vec![
            (keys::ROOMS, room_id, true),
            (keys::LINKED_CHUNKS, &chunk_id.to_string(), false),
        ])
    }

    fn encode_in_band_event_range_for_chunk(&self, room_id: &str, chunk_id: u64) -> IdbKeyRange {
        let lower =
            self.encode_in_band_event_key(room_id, &PositionForCache { chunk_id, index: 0 });
        let upper = self.encode_upper_in_band_event_key_for_chunk(room_id, chunk_id);
        IdbKeyRange::bound(&lower.into(), &upper.into()).expect("construct key range")
    }

    fn encode_in_band_event_range_for_chunk_from(
        &self,
        room_id: &str,
        position: &PositionForCache,
    ) -> IdbKeyRange {
        let lower = self.encode_in_band_event_key(room_id, position);
        let upper = self.encode_upper_in_band_event_key_for_chunk(room_id, position.chunk_id);
        IdbKeyRange::bound(&lower.into(), &upper.into()).expect("construct key range")
    }

    fn encode_in_band_event_range_for_room(&self, room_id: &str) -> IdbKeyRange {
        let lower = self.encode_in_band_event_key(room_id, &PositionForCache::default());
        let upper = self.encode_upper_in_band_event_key_for_room(room_id);
        IdbKeyRange::bound(&lower.into(), &upper.into()).expect("construct key range")
    }

    fn encode_event_id_range_for_room(&self, room_id: &str) -> IdbKeyRange {
        let lower = self.encode_key(vec![
            (keys::ROOMS, room_id, true),
            (keys::EVENTS, &String::from(Self::KEY_LOWER_CHARACTER), false),
        ]);
        let upper = self.encode_key(vec![
            (keys::ROOMS, room_id, true),
            (keys::EVENTS, &String::from(Self::KEY_UPPER_CHARACTER), false),
        ]);
        IdbKeyRange::bound(&lower.into(), &upper.into()).expect("construct key range")
    }

    fn encode_event_id_key(&self, room_id: &str, event_id: &EventId) -> String {
        self.encode_key(vec![(keys::ROOMS, room_id, true), (keys::EVENTS, event_id.as_ref(), true)])
    }

    fn serialize_in_band_event(
        &self,
        event: &InBandEventForCache,
    ) -> Result<JsValue, IndexeddbEventCacheStoreError> {
        let event_id = event.content.event_id().ok_or(IndexeddbEventCacheStoreError::NoEventId)?;
        let id = self.encode_event_id_key(&event.room_id, &event_id);
        let position = self.encode_in_band_event_key(&event.room_id, &event.position);
        Ok(serde_wasm_bindgen::to_value(&IndexedEvent {
            id,
            position: Some(position),
            content: self.serializer.maybe_encrypt_value(event)?,
        })?)
    }

    fn serialize_out_of_band_event(
        &self,
        event: &OutOfBandEventForCache,
    ) -> Result<JsValue, IndexeddbEventCacheStoreError> {
        let event_id = event.content.event_id().ok_or(IndexeddbEventCacheStoreError::NoEventId)?;
        let id = self.encode_event_id_key(&event.room_id, &event_id);
        Ok(serde_wasm_bindgen::to_value(&IndexedEvent {
            id,
            position: None,
            content: self.serializer.maybe_encrypt_value(event)?,
        })?)
    }

    fn serialize_event(
        &self,
        event: &EventForCache,
    ) -> Result<JsValue, IndexeddbEventCacheStoreError> {
        match event {
            EventForCache::InBand(i) => self.serialize_in_band_event(i),
            EventForCache::OutOfBand(o) => self.serialize_out_of_band_event(o),
        }
    }

    fn deserialize_generic_event<P: DeserializeOwned>(
        &self,
        value: JsValue,
    ) -> Result<GenericEventForCache<P>, IndexeddbEventCacheStoreError> {
        let indexed: IndexedEvent = value.into_serde()?;
        self.serializer
            .maybe_decrypt_value::<GenericEventForCache<P>>(indexed.content)
            .map_err(Into::into)
    }

    fn deserialize_in_band_event(
        &self,
        value: JsValue,
    ) -> Result<InBandEventForCache, IndexeddbEventCacheStoreError> {
        self.deserialize_generic_event(value)
    }

    fn deserialize_event(
        &self,
        value: JsValue,
    ) -> Result<EventForCache, IndexeddbEventCacheStoreError> {
        let indexed: IndexedEvent = value.into_serde()?;
        self.serializer.maybe_decrypt_value(indexed.content).map_err(Into::into)
    }

    async fn get_all_events_by_position<K: wasm_bindgen::JsCast>(
        &self,
        key: &K,
    ) -> Result<js_sys::Array, IndexeddbEventCacheStoreError> {
        self.inner
            .transaction_on_one_with_mode(keys::EVENTS, IdbTransactionMode::Readonly)?
            .object_store(keys::EVENTS)?
            .index(keys::EVENT_POSITIONS)?
            .get_all_with_key(key)?
            .await
            .map_err(Into::into)
    }

    async fn get_all_events_by_id<K: wasm_bindgen::JsCast>(
        &self,
        key: &K,
    ) -> Result<js_sys::Array, IndexeddbEventCacheStoreError> {
        self.inner
            .transaction_on_one_with_mode(keys::EVENTS, IdbTransactionMode::Readonly)?
            .object_store(keys::EVENTS)?
            .get_all_with_key(key)?
            .await
            .map_err(Into::into)
    }

    async fn get_all_in_band_events_by_room(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<InBandEventForCache>, IndexeddbEventCacheStoreError> {
        let range = self.encode_in_band_event_range_for_room(room_id.as_ref());
        let values = self.get_all_events_by_position(&range).await?;
        let mut events = Vec::new();
        for event in values {
            let event: InBandEventForCache = self.deserialize_in_band_event(event)?;
            events.push(event);
        }
        Ok(events)
    }

    async fn get_all_events_by_room(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<EventForCache>, IndexeddbEventCacheStoreError> {
        let range = self.encode_event_id_range_for_room(room_id.as_ref());
        let values = self.get_all_events_by_id(&range).await?;
        let mut events = Vec::new();
        for event in values {
            let event: EventForCache = self.deserialize_event(event)?;
            events.push(event);
        }
        Ok(events)
    }

    async fn get_all_events_by_chunk(
        &self,
        room_id: &RoomId,
        chunk_id: u64,
    ) -> Result<Vec<InBandEventForCache>, IndexeddbEventCacheStoreError> {
        let range = self.encode_in_band_event_range_for_chunk(room_id.as_ref(), chunk_id);
        let values = self.get_all_events_by_position(&range).await?;
        let mut events = Vec::new();
        for event in values {
            let event: InBandEventForCache = self.deserialize_in_band_event(event)?;
            events.push(event);
        }
        Ok(events)
    }

    async fn get_all_timeline_events_by_chunk(
        &self,
        room_id: &RoomId,
        chunk_id: u64,
    ) -> Result<Vec<TimelineEvent>, IndexeddbEventCacheStoreError> {
        Ok(self
            .get_all_events_by_chunk(room_id, chunk_id)
            .await?
            .into_iter()
            .map(|event| event.content)
            .collect())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct ChunkForCache {
    id: String,
    raw_id: u64,
    previous: Option<String>,
    raw_previous: Option<u64>,
    next: Option<String>,
    raw_next: Option<u64>,
    type_str: String,
}

impl ChunkForCache {
    /// Used to set field `type_str` to represent a chunk that contains events
    const CHUNK_TYPE_EVENT_TYPE_STRING: &str = "E";
    /// Used to set field `type_str` to represent a chunk that is a gap
    const CHUNK_TYPE_GAP_TYPE_STRING: &str = "G";
}

#[derive(Debug, Serialize, Deserialize)]
struct GapForCache {
    prev_token: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct GenericEventForCache<P> {
    content: TimelineEvent,
    room_id: String,
    position: P,
}

#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize)]
struct PositionForCache {
    chunk_id: u64,
    index: usize,
}

type InBandEventForCache = GenericEventForCache<PositionForCache>;
type OutOfBandEventForCache = GenericEventForCache<()>;

#[derive(Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum EventForCache {
    InBand(InBandEventForCache),
    OutOfBand(OutOfBandEventForCache),
}

impl EventForCache {
    pub fn content(&self) -> &TimelineEvent {
        match self {
            EventForCache::InBand(i) => &i.content,
            EventForCache::OutOfBand(o) => &o.content,
        }
    }

    pub fn take_content(self) -> TimelineEvent {
        match self {
            EventForCache::InBand(i) => i.content,
            EventForCache::OutOfBand(o) => o.content,
        }
    }

    pub fn event_id(&self) -> Option<OwnedEventId> {
        match self {
            EventForCache::InBand(i) => i.content.event_id(),
            EventForCache::OutOfBand(o) => o.content.event_id(),
        }
    }

    pub fn position(&self) -> Option<PositionForCache> {
        match self {
            EventForCache::InBand(i) => Some(i.position),
            EventForCache::OutOfBand(_) => None,
        }
    }
}

impl From<PositionForCache> for Position {
    fn from(value: PositionForCache) -> Self {
        Self::new(ChunkIdentifier::new(value.chunk_id), value.index)
    }
}

impl From<Position> for PositionForCache {
    fn from(value: Position) -> Self {
        Self { chunk_id: value.chunk_identifier().index(), index: value.index() }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexedEvent {
    id: String,
    position: Option<String>,
    content: MaybeEncrypted,
}

// Small hack to have the following macro invocation act as the appropriate
// trait impl block on wasm, but still be compiled on non-wasm as a regular
// impl block otherwise.
//
// The trait impl doesn't compile on non-wasm due to unfulfilled trait bounds,
// this hack allows us to still have most of rust-analyzer's IDE functionality
// within the impl block without having to set it up to check things against
// the wasm target (which would disable many other parts of the codebase).
#[cfg(target_arch = "wasm32")]
macro_rules! impl_event_cache_store {
    ( $($body:tt)* ) => {
        #[async_trait(?Send)]
        impl EventCacheStore for IndexeddbEventCacheStore {
            type Error = IndexeddbEventCacheStoreError;

            $($body)*
        }
    };
}

#[cfg(not(target_arch = "wasm32"))]
macro_rules! impl_event_cache_store {
    ( $($body:tt)* ) => {
        impl IndexeddbEventCacheStore {
            $($body)*
        }
    };
}

impl_event_cache_store! {
    /// Try to take a lock using the given store.
    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<bool, IndexeddbEventCacheStoreError> {
        let key = JsValue::from_str(key);
        let txn =
            self.inner.transaction_on_one_with_mode(keys::CORE, IdbTransactionMode::Readwrite)?;
        let object_store = txn.object_store(keys::CORE)?;

        #[derive(serde::Deserialize, serde::Serialize)]
        struct Lease {
            holder: String,
            expiration_ts: u64,
        }

        let now_ts: u64 = MilliSecondsSinceUnixEpoch::now().get().into();
        let expiration_ts = now_ts + lease_duration_ms as u64;
        let value =
            self.serializer.serialize_value(&Lease { holder: holder.to_owned(), expiration_ts })?;

        let prev = object_store.get(&key)?.await?;
        match prev {
            Some(prev) => {
                let lease: Lease = self.serializer.deserialize_value(prev)?;
                if lease.holder == holder || lease.expiration_ts < now_ts {
                    object_store.put_key_val(&key, &value)?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            None => {
                object_store.put_key_val(&key, &value)?;
                Ok(true)
            }
        }
    }

    /// An [`Update`] reflects an operation that has happened inside a linked
    /// chunk. The linked chunk is used by the event cache to store the events
    /// in-memory. This method aims at forwarding this update inside this store.
    async fn handle_linked_chunk_updates(
        &self,
        room_id: &RoomId,
        updates: Vec<Update<Event, Gap>>,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        let tx = self.inner.transaction_on_multi_with_mode(
            &[keys::LINKED_CHUNKS, keys::GAPS, keys::EVENTS],
            IdbTransactionMode::Readwrite,
        )?;

        let linked_chunks = tx.object_store(keys::LINKED_CHUNKS)?;
        let gaps = tx.object_store(keys::GAPS)?;
        let events = tx.object_store(keys::EVENTS)?;
        let event_positions = events.index(keys::EVENT_POSITIONS)?;

        for update in updates {
            match update {
                Update::NewItemsChunk { previous, new, next } => {
                    let previous_id = previous.map(|n| {
                        self.encode_key(vec![
                            (keys::ROOMS, room_id.as_ref(), true),
                            (keys::LINKED_CHUNKS, &n.index().to_string(), false),
                        ])
                    });
                    let id = self.encode_key(vec![
                        (keys::ROOMS, room_id.as_ref(), true),
                        (keys::LINKED_CHUNKS, &new.index().to_string(), false),
                    ]);
                    let next_id = next.map(|n| {
                        self.encode_key(vec![
                            (keys::ROOMS, room_id.as_ref(), true),
                            (keys::LINKED_CHUNKS, &n.index().to_string(), false),
                        ])
                    });

                    trace!(%room_id, "Inserting new chunk (prev={previous:?}, new={new:?}, next={next:?})");

                    let chunk = ChunkForCache {
                        id: id.clone(),
                        raw_id: new.index(),
                        previous: previous_id.clone(),
                        raw_previous: previous.map(|n| n.index()),
                        next: next_id.clone(),
                        raw_next: next.map(|n| n.index()),
                        type_str: ChunkForCache::CHUNK_TYPE_EVENT_TYPE_STRING.to_owned(),
                    };

                    let chunk_serialized = self.serialize_value_with_id(&id, &chunk)?;

                    linked_chunks.add_val_owned(chunk_serialized)?;

                    if let Some(previous_id) = previous_id {
                        let previous = linked_chunks.get_owned(&previous_id)?.await?;

                        if let Some(previous) = previous {
                            let previous: ChunkForCache =
                                self.deserialize_value_with_id(previous)?;

                            let previous = ChunkForCache {
                                id: previous.id,
                                raw_id: previous.raw_id,
                                previous: previous.previous,
                                raw_previous: previous.raw_previous,
                                next: Some(id.clone()),
                                raw_next: Some(new.index()),
                                type_str: previous.type_str,
                            };

                            let previous = self.serialize_value_with_id(&previous_id, &previous)?;

                            linked_chunks.put_val_owned(previous)?;
                        }
                    }

                    if let Some(next_id) = next_id {
                        let next = linked_chunks.get_owned(&next_id)?.await?;
                        if let Some(next) = next {
                            let next: ChunkForCache = self.deserialize_value_with_id(next)?;

                            let next = ChunkForCache {
                                id: next.id,
                                raw_id: next.raw_id,
                                previous: Some(id.clone()),
                                raw_previous: Some(new.index()),
                                next: next.next,
                                raw_next: next.raw_next,
                                type_str: next.type_str,
                            };

                            let next = self.serialize_value_with_id(&next_id, &next)?;

                            linked_chunks.put_val_owned(next)?;
                        }
                    }
                }
                Update::NewGapChunk { previous, new, next, gap } => {
                    let previous_id = previous.map(|n| {
                        self.encode_key(vec![
                            (keys::ROOMS, room_id.as_ref(), true),
                            (keys::LINKED_CHUNKS, &n.index().to_string(), false),
                        ])
                    });
                    let id = self.encode_key(vec![
                        (keys::ROOMS, room_id.as_ref(), true),
                        (keys::LINKED_CHUNKS, &new.index().to_string(), false),
                    ]);
                    let next_id = next.map(|n| {
                        self.encode_key(vec![
                            (keys::ROOMS, room_id.as_ref(), true),
                            (keys::LINKED_CHUNKS, &n.index().to_string(), false),
                        ])
                    });
                    trace!(%room_id, "Inserting new gap (prev={previous:?}, new={id}, next={next:?})");

                    let chunk = ChunkForCache {
                        id: id.clone(),
                        raw_id: new.index(),
                        previous: previous_id.clone(),
                        raw_previous: previous.map(|n| n.index()),
                        next: next_id.clone(),
                        raw_next: next.map(|n| n.index()),
                        type_str: ChunkForCache::CHUNK_TYPE_GAP_TYPE_STRING.to_owned(),
                    };

                    let chunk_serialized = self.serialize_value_with_id(&id, &chunk)?;

                    linked_chunks.add_val_owned(chunk_serialized)?;

                    if let Some(previous_id) = previous_id {
                        let previous = linked_chunks.get_owned(&previous_id)?.await?;
                        if let Some(previous) = previous {
                            let previous: ChunkForCache =
                                self.deserialize_value_with_id(previous)?;

                            let previous = ChunkForCache {
                                id: previous.id,
                                raw_id: previous.raw_id,
                                previous: previous.previous,
                                raw_previous: previous.raw_previous,
                                next: Some(id.clone()),
                                raw_next: Some(new.index()),
                                type_str: previous.type_str,
                            };

                            let previous = self.serialize_value_with_id(&previous.id, &previous)?;

                            linked_chunks.put_val(&previous)?;
                        }
                    }

                    // update next if there
                    if let Some(next_id) = next_id {
                        let next = linked_chunks.get_owned(&next_id)?.await?;
                        if let Some(next) = next {
                            let next: ChunkForCache = self.deserialize_value_with_id(next)?;

                            let next = ChunkForCache {
                                id: next.id,
                                raw_id: next.raw_id,
                                previous: Some(id.clone()),
                                raw_previous: Some(new.index()),
                                next: next.next,
                                raw_next: next.raw_next,
                                type_str: next.type_str,
                            };

                            let next = self.serialize_value_with_id(&next.id, &next)?;

                            linked_chunks.put_val(&next)?;
                        }
                    }

                    let gap = GapForCache { prev_token: gap.prev_token };

                    let serialized_gap = self.serialize_value_with_id(&id, &gap)?;

                    gaps.add_val_owned(serialized_gap)?;
                }

                Update::RemoveChunk(id) => {
                    let id = self.encode_key(vec![
                        (keys::ROOMS, room_id.as_ref(), true),
                        (keys::LINKED_CHUNKS, &id.index().to_string(), false),
                    ]);

                    trace!("Removing chunk {id:?}");

                    let chunk = linked_chunks.get_owned(id.clone())?.await?;
                    if let Some(chunk) = chunk {
                        let chunk: ChunkForCache = self.deserialize_value_with_id(chunk)?;

                        if let Some(previous) = chunk.previous.clone() {
                            let previous = linked_chunks.get_owned(previous)?.await?;
                            if let Some(previous) = previous {
                                let previous: ChunkForCache =
                                    self.deserialize_value_with_id(previous)?;

                                let previous = ChunkForCache {
                                    id: previous.id,
                                    raw_id: previous.raw_id,
                                    previous: previous.previous,
                                    raw_previous: previous.raw_previous,
                                    next: chunk.next.clone(),
                                    raw_next: chunk.raw_next,
                                    type_str: previous.type_str,
                                };
                                let previous =
                                    self.serialize_value_with_id(&previous.id, &previous)?;
                                linked_chunks.put_val(&previous)?;
                            }
                        }

                        if let Some(next) = chunk.next {
                            let next = linked_chunks.get_owned(next)?.await?;
                            if let Some(next) = next {
                                let next: ChunkForCache = self.deserialize_value_with_id(next)?;

                                let next = ChunkForCache {
                                    id: next.id,
                                    raw_id: next.raw_id,
                                    previous: chunk.previous,
                                    raw_previous: chunk.raw_previous,
                                    next: next.next,
                                    raw_next: next.raw_next,
                                    type_str: next.type_str,
                                };
                                let next = self.serialize_value_with_id(&next.id, &next)?;

                                linked_chunks.put_val(&next)?;
                            }
                        }

                        linked_chunks.delete_owned(id)?;
                    }
                }
                Update::PushItems { at, items } => {
                    let chunk_id = at.chunk_identifier().index();

                    trace!(%room_id, "pushing {} items @ {chunk_id}", items.len());

                    for (i, item) in items.into_iter().enumerate() {
                        let value = self.serialize_in_band_event(&InBandEventForCache {
                            content: item,
                            room_id: room_id.to_string(),
                            position: PositionForCache {
                                chunk_id, index: at.index() + i,
                            },
                        })?;
                        events.put_val(&value)?.into_future().await?;
                    }
                }
                Update::ReplaceItem { at, item } => {
                    let chunk_id = at.chunk_identifier().index();
                    let index = at.index();

                    trace!(%room_id, "replacing item @ {chunk_id}:{index}");

                    // First remove the event in the given position, if it exists
                    let key = self.encode_in_band_event_key(room_id.as_ref(), &at.into());
                    if let Some(cursor) = event_positions.open_cursor_with_range(&JsValue::from(key))?.await? {
                        cursor.delete()?.await?;
                    }

                    // Then put the new event in the given position
                    let value = self.serialize_in_band_event(&InBandEventForCache {
                        content: item,
                        room_id: room_id.to_string(),
                        position: at.into(),
                    })?;
                    events.put_val(&value)?.await?;
                }
                Update::RemoveItem { at } => {
                    let chunk_id = at.chunk_identifier().index();
                    let index = at.index();

                    trace!(%room_id, "removing item @ {chunk_id}:{index}");

                    let key = self.encode_in_band_event_key(room_id.as_ref(), &at.into());
                    if let Some(cursor) = event_positions.open_cursor_with_range(&JsValue::from(key))?.await? {
                        cursor.delete()?.await?;
                    }
                }
                Update::DetachLastItems { at } => {
                    let chunk_id = at.chunk_identifier().index();
                    let index = at.index();

                    trace!(%room_id, "detaching last items @ {chunk_id}:{index}");

                    let key_range = self.encode_in_band_event_range_for_chunk_from(room_id.as_ref(), &at.into());
                    if let Some(cursor) = event_positions.open_cursor_with_range(&key_range)?.await? {
                        while cursor.key().is_some() {
                            let event: InBandEventForCache = self.deserialize_in_band_event(cursor.value())?;
                            if event.position.index >= index {
                                cursor.delete()?.await?;
                            }
                            cursor.continue_cursor()?.await?;
                        }
                    }
                }
                Update::StartReattachItems | Update::EndReattachItems => {
                    // Nothing? See sqlite implementation
                }
                Update::Clear => {
                    trace!(%room_id, "clearing all events");
                    let lower = self.encode_key(vec![(keys::ROOMS, room_id.as_ref(), true)]);
                    let upper = self.encode_upper_key(vec![(keys::ROOMS, room_id.as_ref(), true)]);

                    let chunks_key_range =
                        IdbKeyRange::bound(&lower.into(), &upper.into()).unwrap();

                    let chunks = linked_chunks.get_all_with_key(&chunks_key_range)?.await?;

                    for chunk in chunks {
                        let chunk: ChunkForCache = self.deserialize_value_with_id(chunk)?;
                        // Delete all events for this chunk
                        let events_key_range = self.encode_in_band_event_range_for_chunk(room_id.as_ref(), chunk.raw_id);
                        events.delete_owned(events_key_range)?;
                        linked_chunks.delete_owned(&chunk.id)?;
                    }
                }
            }
        }

        tx.await.into_result()?;
        Ok(())
    }

    /// Return all the raw components of a linked chunk, so the caller may
    /// reconstruct the linked chunk later.
    #[doc(hidden)]
    async fn load_all_chunks(
        &self,
        room_id: &RoomId,
    ) -> Result<Vec<RawChunk<Event, Gap>>, IndexeddbEventCacheStoreError> {
        let tx = self.inner.transaction_on_one_with_mode(
            keys::LINKED_CHUNKS,
            IdbTransactionMode::Readonly,
        )?;

        let chunks = tx.object_store(keys::LINKED_CHUNKS)?;

        let lower = self.encode_key(vec![(keys::ROOMS, room_id.as_ref(), true)]);
        let upper = self.encode_upper_key(vec![(keys::ROOMS, room_id.as_ref(), true)]);

        let key_range = IdbKeyRange::bound(&lower.into(), &upper.into()).unwrap();

        let linked_chunks = chunks.get_all_with_key_owned(key_range)?.await?;

        let mut raw_chunks = Vec::new();

        for linked_chunk in linked_chunks {
            let linked_chunk: ChunkForCache = self.deserialize_value_with_id(linked_chunk)?;
            let chunk_id = linked_chunk.raw_id;
            let previous_chunk_id = linked_chunk.raw_previous;
            let next_chunk_id = linked_chunk.raw_next;

            match linked_chunk.type_str.as_str() {
                ChunkForCache::CHUNK_TYPE_EVENT_TYPE_STRING => {
                    let events = self
                        .get_all_timeline_events_by_chunk(room_id.as_ref(), chunk_id)
                        .await?;
                    let raw_chunk = RawChunk {
                        identifier: ChunkIdentifier::new(chunk_id),
                        content: linked_chunk::ChunkContent::Items(events),
                        previous: previous_chunk_id.map(ChunkIdentifier::new),
                        next: next_chunk_id.map(ChunkIdentifier::new),
                    };
                    raw_chunks.push(raw_chunk);
                }
                ChunkForCache::CHUNK_TYPE_GAP_TYPE_STRING => {
                    let id = linked_chunk.id;

                    let gap = self
                        .inner
                        .transaction_on_one_with_mode(keys::GAPS, IdbTransactionMode::Readonly)?
                        .object_store(keys::GAPS)?
                        .get_owned(id.clone())?
                        .await?
                        .unwrap();
                    let gap: GapForCache = self.deserialize_value_with_id(gap)?;
                    let gap = Gap { prev_token: gap.prev_token };

                    let raw_chunk = RawChunk {
                        identifier: ChunkIdentifier::new(chunk_id),
                        content: linked_chunk::ChunkContent::Gap(gap),
                        previous: previous_chunk_id.map(ChunkIdentifier::new),
                        next: next_chunk_id.map(ChunkIdentifier::new),
                    };

                    raw_chunks.push(raw_chunk);
                }
                _ => {
                    return Err(IndexeddbEventCacheStoreError::UnknownChunkType(
                        linked_chunk.type_str.clone(),
                    ));
                }
            }
        }
        Ok(raw_chunks)
    }

    /// Load the last chunk of the `LinkedChunk` holding all events of the room
    /// identified by `room_id`.
    ///
    /// This is used to iteratively load events for the `EventCache`.
    async fn load_last_chunk(
        &self,
        room_id: &RoomId,
    ) -> Result<
        (Option<RawChunk<Event, Gap>>, ChunkIdentifierGenerator),
        IndexeddbEventCacheStoreError,
    > {
        let tx = self.inner.transaction_on_multi_with_mode(
            &[keys::LINKED_CHUNKS, keys::GAPS],
            IdbTransactionMode::Readonly,
        )?;

        let chunks = tx.object_store(keys::LINKED_CHUNKS)?;
        let gaps = tx.object_store(keys::GAPS)?;

        let lower = self.encode_key(vec![(keys::ROOMS, room_id.as_ref(), true)]);
        let upper = self.encode_upper_key(vec![(keys::ROOMS, room_id.as_ref(), true)]);
        let range = IdbKeyRange::bound(&lower.into(), &upper.into()).unwrap();

        let values = chunks.get_all_with_key(&range)?.await?;
        if values.length() == 0 {
            // There are no chunks in the object store, so there is no last chunk
            Ok((None, ChunkIdentifierGenerator::new_from_scratch()))
        } else {
            let mut max_chunk_id = 0;
            let mut last_chunks = Vec::new();
            for value in values {
                let chunk: ChunkForCache = self.deserialize_value_with_id(value)?;
                if chunk.raw_id > max_chunk_id {
                    max_chunk_id = chunk.raw_id;
                }
                if chunk.raw_next.is_none() {
                    last_chunks.push(chunk);
                }
            }
            if last_chunks.len() > 1 {
                // There are some chunks in the object store, but there is more than
                // one last chunk, which means that we have disjoint lists.
                return Err(IndexeddbEventCacheStoreError::ChunksContainDisjointLists);
            }
            let Some(last_chunk) = last_chunks.pop() else {
                // There are chunks in the object store, but there is no last chunk,
                // which means that we have a cycle somewhere.
                return Err(IndexeddbEventCacheStoreError::ChunksContainCycle);
            };

            let content = match last_chunk.type_str.as_str() {
                ChunkForCache::CHUNK_TYPE_EVENT_TYPE_STRING => {
                    let events = self.get_all_timeline_events_by_chunk(room_id.as_ref(), last_chunk.raw_id).await?;
                    linked_chunk::ChunkContent::Items(events)
                }
                ChunkForCache::CHUNK_TYPE_GAP_TYPE_STRING => {
                    let gap = gaps.get_owned(last_chunk.id.clone())?.await?.unwrap();
                    let gap: GapForCache = self.deserialize_value_with_id(gap)?;
                    linked_chunk::ChunkContent::Gap(Gap { prev_token: gap.prev_token })
                }
                s => {
                    return Err(IndexeddbEventCacheStoreError::UnknownChunkType(s.to_owned()));
                }
            };

            let identifier = ChunkIdentifier::new(last_chunk.raw_id);
            let raw_chunk = RawChunk {
                identifier,
                content,
                previous: last_chunk.raw_previous.map(ChunkIdentifier::new),
                next: None,
            };
            let generator = ChunkIdentifierGenerator::new_from_previous_chunk_identifier(
                ChunkIdentifier::new(max_chunk_id),
            );
            Ok((Some(raw_chunk), generator))
        }
    }

    /// Load the chunk before the chunk identified by `before_chunk_identifier`
    /// of the `LinkedChunk` holding all events of the room identified by
    /// `room_id`
    ///
    /// This is used to iteratively load events for the `EventCache`.
    async fn load_previous_chunk(
        &self,
        room_id: &RoomId,
        before_chunk_identifier: ChunkIdentifier,
    ) -> Result<Option<RawChunk<Event, Gap>>, IndexeddbEventCacheStoreError> {
        let tx = self.inner.transaction_on_multi_with_mode(
            &[keys::LINKED_CHUNKS, keys::GAPS],
            IdbTransactionMode::Readonly,
        )?;

        let chunks = tx.object_store(keys::LINKED_CHUNKS)?;
        let gaps = tx.object_store(keys::GAPS)?;

        let key = self.encode_key(vec![
            (keys::ROOMS, room_id.as_ref(), true),
            (keys::LINKED_CHUNKS, &before_chunk_identifier.index().to_string(), false),
        ]);
        if let Some(value) = chunks.get_owned(&key)?.await? {
            let chunk: ChunkForCache = self.deserialize_value_with_id(value)?;
            if let Some(previous_key) = chunk.previous {
                if let Some(value) = chunks.get_owned(&previous_key)?.await? {
                    let previous_chunk: ChunkForCache = self.deserialize_value_with_id(value)?;
                    let content = match previous_chunk.type_str.as_str() {
                        ChunkForCache::CHUNK_TYPE_EVENT_TYPE_STRING => {
                            let events = self.get_all_timeline_events_by_chunk(room_id.as_ref(), previous_chunk.raw_id).await?;
                            linked_chunk::ChunkContent::Items(events)
                        }
                        ChunkForCache::CHUNK_TYPE_GAP_TYPE_STRING => {
                            let gap = gaps.get_owned(previous_chunk.id.clone())?.await?.unwrap();
                            let gap: GapForCache = self.deserialize_value_with_id(gap)?;
                            linked_chunk::ChunkContent::Gap(Gap { prev_token: gap.prev_token })
                        }
                        s => {
                            return Err(IndexeddbEventCacheStoreError::UnknownChunkType(
                                s.to_owned(),
                            ));
                        }
                    };
                    return Ok(Some(RawChunk {
                        content,
                        identifier: ChunkIdentifier::new(previous_chunk.raw_id),
                        previous: previous_chunk.raw_previous.map(ChunkIdentifier::new),
                        // TODO: This should always be `before_chunk_identifier`, and if it's not,
                        // something is wrong with our query... should this
                        // be an expect()? Or, at least an error?
                        next: previous_chunk.raw_next.map(ChunkIdentifier::new),
                    }));
                }
            }
        }
        Ok(None)
    }

    /// Clear persisted events for all the rooms.
    ///
    /// This will empty and remove all the linked chunks stored previously,
    /// using the above [`Self::handle_linked_chunk_updates`] methods. It
    /// must *also* delete all the events' content, if they were stored in a
    /// separate table.
    ///
    /// ⚠ This is meant only for super specific use cases, where there shouldn't
    /// be any live in-memory linked chunks. In general, prefer using
    /// `EventCache::clear_all_rooms()` from the common SDK crate.
    async fn clear_all_rooms_chunks(&self) -> Result<(), IndexeddbEventCacheStoreError> {
        let tx = self.inner.transaction_on_multi_with_mode(
            &[keys::LINKED_CHUNKS, keys::EVENTS, keys::GAPS],
            IdbTransactionMode::Readwrite,
        )?;

        let object_store = tx.object_store(keys::LINKED_CHUNKS)?;
        object_store.clear()?.await?;

        let object_store = tx.object_store(keys::EVENTS)?;
        object_store.clear()?.await?;

        let object_store = tx.object_store(keys::GAPS)?;
        object_store.clear()?.await?;

        tx.await.into_result()?;
        Ok(())
    }

    /// Given a set of event IDs, return the duplicated events along with their
    /// position if there are any.
    async fn filter_duplicated_events(
        &self,
        room_id: &RoomId,
        events: Vec<OwnedEventId>,
    ) -> Result<Vec<(OwnedEventId, Position)>, IndexeddbEventCacheStoreError> {
        if events.is_empty() {
            return Ok(Vec::new());
        }

        // TODO: This is very inefficient, as we are reading and
        // deserializing every event in the room in order to pick
        // out a single one. The problem is that the current
        // schema doesn't easily allow us to find an event without
        // knowing which chunk it is in. To improve this, we will
        // need to add another index to our event store.
        let mut duplicated = Vec::new();
        for event in self.get_all_in_band_events_by_room(room_id).await? {
            if let Some(event_id) = event.content.event_id() {
                if events.contains(&event_id) {
                    duplicated.push((event_id, event.position.into()))
                }
            }
        }
        Ok(duplicated)
    }

    /// Find an event by its ID.
    async fn find_event(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<Event>, IndexeddbEventCacheStoreError> {
        // TODO: This is very inefficient, as we are reading and
        // deserializing every event in the room in order to pick
        // out a single one. The problem is that the current
        // schema doesn't easily allow us to find an event without
        // knowing which chunk it is in. To improve this, we will
        // need to add another index to our event store.
        for event in self.get_all_events_by_room(room_id).await? {
            if event.event_id().is_some_and(|inner| inner == *event_id) {
                return Ok(Some(event.take_content()));
            }
        }
        Ok(None)
    }

    /// Find all the events that relate to a given event.
    ///
    /// An additional filter can be provided to only retrieve related events for
    /// a certain relationship.
    async fn find_event_relations(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
        filter: Option<&[RelationType]>,
    ) -> Result<Vec<Event>, IndexeddbEventCacheStoreError> {
        // TODO: This is very inefficient, as we are reading and
        // deserializing every event in the room in order to pick
        // out a single one. The problem is that the current
        // schema doesn't easily allow us to find an event without
        // knowing which chunk it is in. To improve this, we will
        // need to add another index to our event store.
        let mut result = Vec::new();
        for event in self.get_all_events_by_room(room_id).await? {
            if let Some((relates_to, relation_type)) = extract_event_relation(event.content().raw())
            {
                let filter_contains_relation_type = filter
                    .map(|filter| filter.iter().any(|relation| relation.as_ref() == relation_type))
                    .unwrap_or(true);
                if event_id == relates_to && filter_contains_relation_type {
                    result.push(event.take_content());
                }
            }
        }
        Ok(result)
    }

    /// Save an event, that might or might not be part of an existing linked
    /// chunk.
    ///
    /// If the event has no event id, it will not be saved, and the function
    /// must return an Ok result early.
    ///
    /// If the event was already stored with the same id, it must be replaced,
    /// without causing an error.
    async fn save_event(
        &self,
        room_id: &RoomId,
        event: Event,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        let Some(event_id) = event.event_id() else {
            error!(%room_id, "Trying to save an event with no ID");
            return Ok(());
        };

        // TODO: This is very inefficient, as we are reading and
        // deserializing every event in the room in order to pick
        // out a single one. The problem is that the current
        // schema doesn't easily allow us to find an event without
        // knowing which chunk it is in. To improve this, we will
        // need to add another index to our event store.
        let mut position = None;
        for candidate in self.get_all_events_by_room(room_id).await? {
            if let Some(id) = candidate.event_id() {
                if event_id == id {
                    position = candidate.position();
                    break;
                }
            }
        }
        let event = if let Some(position) = position {
            EventForCache::InBand(InBandEventForCache {
                content: event,
                room_id: room_id.to_string(),
                position
            })
        } else {
            EventForCache::OutOfBand(OutOfBandEventForCache {
                content: event,
                room_id: room_id.to_string(),
                position: (),
            })
        };
        let value = self.serialize_event(&event)?;
        self
            .inner
            .transaction_on_multi_with_mode(&[keys::EVENTS], IdbTransactionMode::Readwrite)?
            .object_store(keys::EVENTS)?
            .put_val_owned(value)?;
        Ok(())
    }

    /// Add a media file's content in the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    ///
    /// * `content` - The content of the file.
    async fn add_media_content(
        &self,
        request: &MediaRequestParameters,
        content: Vec<u8>,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        self.media_store
            .add_media_content(request, content, ignore_policy)
            .await
            .map_err(Into::into)
    }

    /// Replaces the given media's content key with another one.
    ///
    /// This should be used whenever a temporary (local) MXID has been used, and
    /// it must now be replaced with its actual remote counterpart (after
    /// uploading some content, or creating an empty MXC URI).
    ///
    /// ⚠ No check is performed to ensure that the media formats are consistent,
    /// i.e. it's possible to update with a thumbnail key a media that was
    /// keyed as a file before. The caller is responsible of ensuring that
    /// the replacement makes sense, according to their use case.
    ///
    /// This should not raise an error when the `from` parameter points to an
    /// unknown media, and it should silently continue in this case.
    ///
    /// # Arguments
    ///
    /// * `from` - The previous `MediaRequest` of the file.
    ///
    /// * `to` - The new `MediaRequest` of the file.
    async fn replace_media_key(
        &self,
        from: &MediaRequestParameters,
        to: &MediaRequestParameters,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        self.media_store.replace_media_key(from, to).await.map_err(Into::into)
    }

    /// Get a media file's content out of the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    async fn get_media_content(
        &self,
        request: &MediaRequestParameters,
    ) -> Result<Option<Vec<u8>>, IndexeddbEventCacheStoreError> {
        self.media_store.get_media_content(request).await.map_err(Into::into)
    }

    /// Remove a media file's content from the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    async fn remove_media_content(
        &self,
        request: &MediaRequestParameters,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        self.media_store.remove_media_content(request).await.map_err(Into::into)
    }

    /// Get a media file's content associated to an `MxcUri` from the
    /// media store.
    ///
    /// In theory, there could be several files stored using the same URI and a
    /// different `MediaFormat`. This API is meant to be used with a media file
    /// that has only been stored with a single format.
    ///
    /// If there are several media files for a given URI in different formats,
    /// this API will only return one of them. Which one is left as an
    /// implementation detail.
    ///
    /// # Arguments
    ///
    /// * `uri` - The `MxcUri` of the media file.
    async fn get_media_content_for_uri(
        &self,
        uri: &MxcUri,
    ) -> Result<Option<Vec<u8>>, IndexeddbEventCacheStoreError> {
        self.media_store.get_media_content_for_uri(uri).await.map_err(Into::into)
    }

    /// Remove all the media files' content associated to an `MxcUri` from the
    /// media store.
    ///
    /// This should not raise an error when the `uri` parameter points to an
    /// unknown media, and it should return an Ok result in this case.
    ///
    /// # Arguments
    ///
    /// * `uri` - The `MxcUri` of the media files.
    async fn remove_media_content_for_uri(
        &self,
        uri: &MxcUri,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        self.media_store.remove_media_content_for_uri(uri).await.map_err(Into::into)
    }

    /// Set the `MediaRetentionPolicy` to use for deciding whether to store or
    /// keep media content.
    ///
    /// # Arguments
    ///
    /// * `policy` - The `MediaRetentionPolicy` to use.
    async fn set_media_retention_policy(
        &self,
        policy: MediaRetentionPolicy,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        self.media_store.set_media_retention_policy(policy).await.map_err(Into::into)
    }

    /// Get the current `MediaRetentionPolicy`.
    fn media_retention_policy(&self) -> MediaRetentionPolicy {
        self.media_store.media_retention_policy()
    }

    /// Set whether the current [`MediaRetentionPolicy`] should be ignored for
    /// the media.
    ///
    /// The change will be taken into account in the next cleanup.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequestParameters` of the file.
    ///
    /// * `ignore_policy` - Whether the current `MediaRetentionPolicy` should be
    ///   ignored.
    async fn set_ignore_media_retention_policy(
        &self,
        request: &MediaRequestParameters,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        self.media_store
            .set_ignore_media_retention_policy(request, ignore_policy)
            .await
            .map_err(Into::into)
    }

    /// Clean up the media cache with the current `MediaRetentionPolicy`.
    ///
    /// If there is already an ongoing cleanup, this is a noop.
    async fn clean_up_media_cache(&self) -> Result<(), IndexeddbEventCacheStoreError> {
        self.media_store.clean_up_media_cache().await.map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use indexed_db_futures::IdbQuerySource;
    use matrix_sdk_base::{
        event_cache::{
            store::{
                integration_tests::{check_test_event, make_test_event},
                EventCacheStore,
            },
            Gap,
        },
        linked_chunk::{ChunkContent, ChunkIdentifier, Position, Update},
    };
    use matrix_sdk_test::DEFAULT_TEST_ROOM_ID;
    use ruma::room_id;
    use web_sys::IdbTransactionMode;

    use super::*;

    async fn test_linked_chunk_new_items_chunk(store: IndexeddbEventCacheStore) {
        let room_id = &DEFAULT_TEST_ROOM_ID;
        let updates = vec![
            Update::NewItemsChunk {
                previous: None,
                new: ChunkIdentifier::new(42),
                next: None, // Note: the store must link the next entry itself.
            },
            Update::NewItemsChunk {
                previous: Some(ChunkIdentifier::new(42)),
                new: ChunkIdentifier::new(13),
                next: Some(ChunkIdentifier::new(37)), /* But it's fine to explicitly pass
                                                       * the next link ahead of time. */
            },
            Update::NewItemsChunk {
                previous: Some(ChunkIdentifier::new(13)),
                new: ChunkIdentifier::new(37),
                next: None,
            },
        ];
        store.handle_linked_chunk_updates(room_id, updates).await.unwrap();

        let mut chunks = store.load_all_chunks(room_id).await.unwrap();
        assert_eq!(chunks.len(), 3);

        // Chunks are ordered from smaller to bigger IDs.
        let c = chunks.remove(0);
        assert_eq!(c.identifier, ChunkIdentifier::new(13));
        assert_eq!(c.previous, Some(ChunkIdentifier::new(42)));
        assert_eq!(c.next, Some(ChunkIdentifier::new(37)));
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert!(events.is_empty());
        });

        let c = chunks.remove(0);
        assert_eq!(c.identifier, ChunkIdentifier::new(37));
        assert_eq!(c.previous, Some(ChunkIdentifier::new(13)));
        assert_eq!(c.next, None);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert!(events.is_empty());
        });

        let c = chunks.remove(0);
        assert_eq!(c.identifier, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, Some(ChunkIdentifier::new(13)));
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert!(events.is_empty());
        });
    }

    async fn test_add_gap_chunk_and_delete_it_immediately(store: IndexeddbEventCacheStore) {
        let room_id = &DEFAULT_TEST_ROOM_ID;
        let updates = vec![Update::NewGapChunk {
            previous: None,
            new: ChunkIdentifier::new(1),
            next: None,
            gap: Gap { prev_token: "cheese".to_owned() },
        }];
        store.handle_linked_chunk_updates(room_id, updates).await.unwrap();

        let updates = vec![
            Update::NewGapChunk {
                previous: Some(ChunkIdentifier::new(1)),
                new: ChunkIdentifier::new(3),
                next: None,
                gap: Gap {
                    prev_token: "t9-4880969790_757284974_23234261_m3457690681~38.3457690701_3823363516_264464613_1459149788_11091595867_0_439828".to_owned()
                },
            },
            Update::RemoveChunk(ChunkIdentifier::new(3)),
        ];
        store.handle_linked_chunk_updates(room_id, updates).await.unwrap();

        let chunks = store.load_all_chunks(room_id).await.unwrap();
        assert_eq!(chunks.len(), 1);
    }

    async fn test_linked_chunk_new_gap_chunk(store: IndexeddbEventCacheStore) {
        let room_id = &DEFAULT_TEST_ROOM_ID;
        let updates = vec![Update::NewGapChunk {
            previous: None,
            new: ChunkIdentifier::new(42),
            next: None,
            gap: Gap { prev_token: "raclette".to_owned() },
        }];
        store.handle_linked_chunk_updates(room_id, updates).await.unwrap();

        let mut chunks = store.load_all_chunks(room_id).await.unwrap();
        assert_eq!(chunks.len(), 1);

        // Chunks are ordered from smaller to bigger IDs.
        let c = chunks.remove(0);
        assert_eq!(c.identifier, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, None);
        assert_matches!(c.content, ChunkContent::Gap(gap) => {
            assert_eq!(gap.prev_token, "raclette");
        });
    }

    async fn test_linked_chunk_replace_item(store: IndexeddbEventCacheStore) {
        let room_id = &DEFAULT_TEST_ROOM_ID;
        let updates = vec![
            Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
            Update::PushItems {
                at: Position::new(ChunkIdentifier::new(42), 0),
                items: vec![make_test_event(room_id, "hello"), make_test_event(room_id, "world")],
            },
            Update::ReplaceItem {
                at: Position::new(ChunkIdentifier::new(42), 1),
                item: make_test_event(room_id, "yolo"),
            },
        ];
        store.handle_linked_chunk_updates(room_id, updates).await.unwrap();

        let mut chunks = store.load_all_chunks(room_id).await.unwrap();
        assert_eq!(chunks.len(), 1);

        let c = chunks.remove(0);
        assert_eq!(c.identifier, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, None);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 2);
            check_test_event(&events[0], "hello");
            check_test_event(&events[1], "yolo");
        });
    }

    async fn test_linked_chunk_remove_chunk(store: IndexeddbEventCacheStore) {
        let room_id = &DEFAULT_TEST_ROOM_ID;
        let updates = vec![
            Update::NewGapChunk {
                previous: None,
                new: ChunkIdentifier::new(42),
                next: None,
                gap: Gap { prev_token: "raclette".to_owned() },
            },
            Update::NewGapChunk {
                previous: Some(ChunkIdentifier::new(42)),
                new: ChunkIdentifier::new(43),
                next: None,
                gap: Gap { prev_token: "fondue".to_owned() },
            },
            Update::NewGapChunk {
                previous: Some(ChunkIdentifier::new(43)),
                new: ChunkIdentifier::new(44),
                next: None,
                gap: Gap { prev_token: "tartiflette".to_owned() },
            },
            Update::RemoveChunk(ChunkIdentifier::new(43)),
        ];
        store.handle_linked_chunk_updates(room_id, updates).await.unwrap();

        let mut chunks = store.load_all_chunks(room_id).await.unwrap();
        assert_eq!(chunks.len(), 2);

        // Chunks are ordered from smaller to bigger IDs.
        let c = chunks.remove(0);
        assert_eq!(c.identifier, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, Some(ChunkIdentifier::new(44)));
        assert_matches!(c.content, ChunkContent::Gap(gap) => {
            assert_eq!(gap.prev_token, "raclette");
        });

        let c = chunks.remove(0);
        assert_eq!(c.identifier, ChunkIdentifier::new(44));
        assert_eq!(c.previous, Some(ChunkIdentifier::new(42)));
        assert_eq!(c.next, None);
        assert_matches!(c.content, ChunkContent::Gap(gap) => {
            assert_eq!(gap.prev_token, "tartiflette");
        });

        // TODO: Is this necessary?
        // Check that cascading worked.
        let tx = store
            .inner
            .transaction_on_one_with_mode(keys::LINKED_CHUNKS, IdbTransactionMode::Readonly)
            .unwrap();
        let object_store = tx.object_store(keys::LINKED_CHUNKS).unwrap();

        let gaps = object_store.get_all().unwrap().await.unwrap();
        let mut gap_ids = Vec::new();
        for gap in gaps {
            let gap: ChunkForCache = store.deserialize_value_with_id(gap).unwrap();
            let chunk_id = gap.raw_id;
            gap_ids.push(chunk_id);
        }
        gap_ids.sort();
        assert_eq!(gap_ids, vec![42, 44]);
    }

    async fn test_linked_chunk_push_items(store: IndexeddbEventCacheStore) {
        let room_id = &DEFAULT_TEST_ROOM_ID;
        let updates = vec![
            Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
            Update::PushItems {
                at: Position::new(ChunkIdentifier::new(42), 0),
                items: vec![make_test_event(room_id, "hello"), make_test_event(room_id, "world")],
            },
            Update::PushItems {
                at: Position::new(ChunkIdentifier::new(42), 2),
                items: vec![make_test_event(room_id, "who?")],
            },
        ];
        store.handle_linked_chunk_updates(room_id, updates).await.unwrap();

        let mut chunks = store.load_all_chunks(room_id).await.unwrap();
        assert_eq!(chunks.len(), 1);

        let c = chunks.remove(0);
        assert_eq!(c.identifier, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, None);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 3);
            check_test_event(&events[0], "hello");
            check_test_event(&events[1], "world");
            check_test_event(&events[2], "who?");
        });
    }

    async fn test_linked_chunk_remove_item(store: IndexeddbEventCacheStore) {
        let room_id = *DEFAULT_TEST_ROOM_ID;
        let updates = vec![
            Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
            Update::PushItems {
                at: Position::new(ChunkIdentifier::new(42), 0),
                items: vec![make_test_event(room_id, "hello"), make_test_event(room_id, "world")],
            },
            Update::RemoveItem { at: Position::new(ChunkIdentifier::new(42), 0) },
        ];
        store.handle_linked_chunk_updates(room_id, updates).await.unwrap();

        let mut chunks = store.load_all_chunks(room_id).await.unwrap();
        assert_eq!(chunks.len(), 1);

        let c = chunks.remove(0);
        assert_eq!(c.identifier, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, None);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 1);
            check_test_event(&events[0], "world");
        });

        // Make sure the position has been updated for the remaining event.
        let events = store.get_all_events_by_chunk(room_id, 42).await.unwrap();
        assert_eq!(events.len(), 1);
    }

    async fn test_linked_chunk_detach_last_items(store: IndexeddbEventCacheStore) {
        let room_id = *DEFAULT_TEST_ROOM_ID;
        let updates = vec![
            Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
            Update::PushItems {
                at: Position::new(ChunkIdentifier::new(42), 0),
                items: vec![
                    make_test_event(room_id, "hello"),
                    make_test_event(room_id, "world"),
                    make_test_event(room_id, "howdy"),
                ],
            },
            Update::DetachLastItems { at: Position::new(ChunkIdentifier::new(42), 1) },
        ];
        store.handle_linked_chunk_updates(room_id, updates).await.unwrap();

        let mut chunks = store.load_all_chunks(room_id).await.unwrap();
        assert_eq!(chunks.len(), 1);

        let c = chunks.remove(0);
        assert_eq!(c.identifier, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, None);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 1);
            check_test_event(&events[0], "hello");
        });
    }

    async fn test_linked_chunk_start_end_reattach_items(store: IndexeddbEventCacheStore) {
        let room_id = *DEFAULT_TEST_ROOM_ID;
        // Same updates and checks as test_linked_chunk_push_items, but with extra
        // `StartReattachItems` and `EndReattachItems` updates, which must have no
        // effects.
        let updates = vec![
            Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
            Update::PushItems {
                at: Position::new(ChunkIdentifier::new(42), 0),
                items: vec![
                    make_test_event(room_id, "hello"),
                    make_test_event(room_id, "world"),
                    make_test_event(room_id, "howdy"),
                ],
            },
            Update::StartReattachItems,
            Update::EndReattachItems,
        ];
        store.handle_linked_chunk_updates(room_id, updates).await.unwrap();

        let mut chunks = store.load_all_chunks(room_id).await.unwrap();
        assert_eq!(chunks.len(), 1);

        let c = chunks.remove(0);
        assert_eq!(c.identifier, ChunkIdentifier::new(42));
        assert_eq!(c.previous, None);
        assert_eq!(c.next, None);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 3);
            check_test_event(&events[0], "hello");
            check_test_event(&events[1], "world");
            check_test_event(&events[2], "howdy");
        });
    }

    async fn test_linked_chunk_clear(store: IndexeddbEventCacheStore) {
        let room_id = *DEFAULT_TEST_ROOM_ID;
        let updates = vec![
            Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
            Update::NewGapChunk {
                previous: Some(ChunkIdentifier::new(42)),
                new: ChunkIdentifier::new(54),
                next: None,
                gap: Gap { prev_token: "fondue".to_owned() },
            },
            Update::PushItems {
                at: Position::new(ChunkIdentifier::new(42), 0),
                items: vec![
                    make_test_event(room_id, "hello"),
                    make_test_event(room_id, "world"),
                    make_test_event(room_id, "howdy"),
                ],
            },
            Update::Clear,
        ];
        store.handle_linked_chunk_updates(room_id, updates).await.unwrap();

        let chunks = store.load_all_chunks(room_id).await.unwrap();
        assert!(chunks.is_empty());
    }

    async fn test_linked_chunk_multiple_rooms(store: IndexeddbEventCacheStore) {
        // Check that applying updates to one room doesn't affect the others.
        // Use the same chunk identifier in both rooms to battle-test search.
        let room1 = room_id!("!realcheeselovers:raclette.fr");
        let updates1 = vec![
            Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
            Update::PushItems {
                at: Position::new(ChunkIdentifier::new(42), 0),
                items: vec![
                    make_test_event(room1, "best cheese is raclette"),
                    make_test_event(room1, "obviously"),
                ],
            },
        ];
        store.handle_linked_chunk_updates(room1, updates1).await.unwrap();

        let room2 = room_id!("!realcheeselovers:fondue.ch");
        let updates2 = vec![
            Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
            Update::PushItems {
                at: Position::new(ChunkIdentifier::new(42), 0),
                items: vec![make_test_event(room1, "beaufort is the best")],
            },
        ];
        store.handle_linked_chunk_updates(room2, updates2).await.unwrap();

        // Check chunks from room 1.
        let mut chunks1 = store.load_all_chunks(room1).await.unwrap();
        assert_eq!(chunks1.len(), 1);

        let c = chunks1.remove(0);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 2);
            check_test_event(&events[0], "best cheese is raclette");
            check_test_event(&events[1], "obviously");
        });

        // Check chunks from room 2.
        let mut chunks2 = store.load_all_chunks(room2).await.unwrap();
        assert_eq!(chunks2.len(), 1);

        let c = chunks2.remove(0);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 1);
            check_test_event(&events[0], "beaufort is the best");
        });
    }

    async fn test_linked_chunk_update_is_a_transaction(store: IndexeddbEventCacheStore) {
        let room_id = *DEFAULT_TEST_ROOM_ID;
        // Trigger a violation of the unique constraint on the (room id, chunk id)
        // couple.
        let updates = vec![
            Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
            Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
        ];
        let err = store.handle_linked_chunk_updates(room_id, updates).await.unwrap_err();

        // The operation fails with a constraint violation error.
        assert_matches!(err, IndexeddbEventCacheStoreError::DomException { .. });

        // If the updates have been handled transactionally, then no new chunks should
        // have been added; failure of the second update leads to the first one being
        // rolled back.
        let chunks = store.load_all_chunks(room_id).await.unwrap();
        assert!(chunks.is_empty());
    }

    async fn test_filter_duplicate_events_no_events(store: IndexeddbEventCacheStore) {
        let room_id = *DEFAULT_TEST_ROOM_ID;
        let duplicates = store.filter_duplicated_events(room_id, Vec::new()).await.unwrap();
        assert!(duplicates.is_empty());
    }

    async fn test_load_last_chunk(store: IndexeddbEventCacheStore) {
        let room_id = room_id!("!r0:matrix.org");
        let event = |msg: &str| make_test_event(room_id, msg);

        // Case #1: no last chunk.
        let (last_chunk, chunk_identifier_generator) =
            store.load_last_chunk(room_id).await.unwrap();
        assert!(last_chunk.is_none());
        assert_eq!(chunk_identifier_generator.current(), 0);

        // Case #2: only one chunk is present.
        let updates = vec![
            Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(42), next: None },
            Update::PushItems {
                at: Position::new(ChunkIdentifier::new(42), 0),
                items: vec![event("saucisse de morteau"), event("comté")],
            },
        ];
        store.handle_linked_chunk_updates(room_id, updates).await.unwrap();

        let (last_chunk, chunk_identifier_generator) =
            store.load_last_chunk(room_id).await.unwrap();
        assert_matches!(last_chunk, Some(last_chunk) => {
            assert_eq!(last_chunk.identifier, 42);
            assert!(last_chunk.previous.is_none());
            assert!(last_chunk.next.is_none());
            assert_matches!(last_chunk.content, ChunkContent::Items(items) => {
                assert_eq!(items.len(), 2);
                check_test_event(&items[0], "saucisse de morteau");
                check_test_event(&items[1], "comté");
            });
        });
        assert_eq!(chunk_identifier_generator.current(), 42);

        // Case #3: more chunks are present.
        let updates = vec![
            Update::NewItemsChunk {
                previous: Some(ChunkIdentifier::new(42)),
                new: ChunkIdentifier::new(7),
                next: None,
            },
            Update::PushItems {
                at: Position::new(ChunkIdentifier::new(7), 0),
                items: vec![event("fondue"), event("gruyère"), event("mont d'or")],
            },
        ];
        store.handle_linked_chunk_updates(room_id, updates).await.unwrap();

        let (last_chunk, chunk_identifier_generator) =
            store.load_last_chunk(room_id).await.unwrap();
        assert_matches!(last_chunk, Some(last_chunk) => {
            assert_eq!(last_chunk.identifier, 7);
            assert_matches!(last_chunk.previous, Some(previous) => {
                assert_eq!(previous, 42);
            });
            assert!(last_chunk.next.is_none());
            assert_matches!(last_chunk.content, ChunkContent::Items(items) => {
                assert_eq!(items.len(), 3);
                check_test_event(&items[0], "fondue");
                check_test_event(&items[1], "gruyère");
                check_test_event(&items[2], "mont d'or");
            });
        });
        assert_eq!(chunk_identifier_generator.current(), 42);
    }

    async fn test_load_last_chunk_with_a_cycle(store: IndexeddbEventCacheStore) {
        let room_id = room_id!("!r0:matrix.org");
        let updates = vec![
            Update::NewItemsChunk { previous: None, new: ChunkIdentifier::new(0), next: None },
            Update::NewItemsChunk {
                // Because `previous` connects to chunk #0, it will create a cycle.
                // Chunk #0 will have a `next` set to chunk #1! Consequently, the last chunk
                // **does not exist**. We have to detect this cycle.
                previous: Some(ChunkIdentifier::new(0)),
                new: ChunkIdentifier::new(1),
                next: Some(ChunkIdentifier::new(0)),
            },
        ];
        store.handle_linked_chunk_updates(room_id, updates).await.unwrap();
        store.load_last_chunk(room_id).await.unwrap_err();
    }

    async fn test_load_previous_chunk(store: IndexeddbEventCacheStore) {
        let room_id = room_id!("!r0:matrix.org");
        let event = |msg: &str| make_test_event(room_id, msg);

        // Case #1: no chunk at all, equivalent to having an nonexistent
        // `before_chunk_identifier`.
        let previous_chunk =
            store.load_previous_chunk(room_id, ChunkIdentifier::new(153)).await.unwrap();
        assert!(previous_chunk.is_none());

        // Case #2: there is one chunk only: we request the previous on this
        // one, it doesn't exist.
        let updates = vec![Update::NewItemsChunk {
            previous: None,
            new: ChunkIdentifier::new(42),
            next: None,
        }];
        store.handle_linked_chunk_updates(room_id, updates).await.unwrap();

        let previous_chunk =
            store.load_previous_chunk(room_id, ChunkIdentifier::new(42)).await.unwrap();
        assert!(previous_chunk.is_none());

        // Case #3: there are two chunks.
        let updates = vec![
            // new chunk before the one that exists.
            Update::NewItemsChunk {
                previous: None,
                new: ChunkIdentifier::new(7),
                next: Some(ChunkIdentifier::new(42)),
            },
            Update::PushItems {
                at: Position::new(ChunkIdentifier::new(7), 0),
                items: vec![event("brigand du jorat"), event("morbier")],
            },
        ];
        store.handle_linked_chunk_updates(room_id, updates).await.unwrap();

        let previous_chunk =
            store.load_previous_chunk(room_id, ChunkIdentifier::new(42)).await.unwrap();

        assert_matches!(previous_chunk, Some(previous_chunk) => {
            assert_eq!(previous_chunk.identifier, 7);
            assert!(previous_chunk.previous.is_none());
            assert_matches!(previous_chunk.next, Some(next) => {
                assert_eq!(next, 42);
            });
            assert_matches!(previous_chunk.content, ChunkContent::Items(items) => {
                assert_eq!(items.len(), 2);
                check_test_event(&items[0], "brigand du jorat");
                check_test_event(&items[1], "morbier");
            });
        });
    }

    mod unencrypted {
        use matrix_sdk_base::{
            event_cache::store::EventCacheStoreError, event_cache_store_integration_tests,
            event_cache_store_integration_tests_time,
        };
        use matrix_sdk_test::async_test;
        use uuid::Uuid;

        use crate::IndexeddbEventCacheStore;

        wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

        async fn get_event_cache_store() -> Result<IndexeddbEventCacheStore, EventCacheStoreError> {
            let name = format!("test-event-cache-store-{}", Uuid::new_v4().as_hyphenated());
            Ok(IndexeddbEventCacheStore::builder().name(name).build().await?)
        }

        #[cfg(target_arch = "wasm32")]
        event_cache_store_integration_tests!();

        #[cfg(target_arch = "wasm32")]
        event_cache_store_integration_tests_time!();

        #[async_test]
        async fn test_linked_chunk_new_items_chunk() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_new_items_chunk(store).await
        }

        #[async_test]
        async fn test_add_gap_chunk_and_delete_it_immediately() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_add_gap_chunk_and_delete_it_immediately(store).await
        }

        #[async_test]
        async fn test_linked_chunk_new_gap_chunk() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_new_gap_chunk(store).await
        }

        #[async_test]
        async fn test_linked_chunk_replace_item() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_replace_item(store).await
        }

        #[async_test]
        async fn test_linked_chunk_remove_chunk() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_remove_chunk(store).await
        }

        #[async_test]
        async fn test_linked_chunk_push_items() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_push_items(store).await
        }

        #[async_test]
        async fn test_linked_chunk_remove_item() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_remove_item(store).await
        }

        #[async_test]
        async fn test_linked_chunk_detach_last_items() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_detach_last_items(store).await
        }

        #[async_test]
        async fn test_linked_chunk_start_end_reattach_items() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_start_end_reattach_items(store).await
        }

        #[async_test]
        async fn test_linked_chunk_clear() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_clear(store).await
        }

        #[async_test]
        async fn test_linked_chunk_multiple_rooms() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_multiple_rooms(store).await
        }

        #[async_test]
        async fn test_linked_chunk_update_is_a_transaction() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_update_is_a_transaction(store).await
        }

        #[async_test]
        async fn test_filter_duplicate_events_no_events() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_filter_duplicate_events_no_events(store).await
        }

        #[async_test]
        async fn test_load_last_chunk() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_load_last_chunk(store).await
        }

        #[async_test]
        async fn test_load_last_chunk_with_a_cycle() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_load_last_chunk_with_a_cycle(store).await
        }

        #[async_test]
        async fn test_load_previous_chunk() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_load_previous_chunk(store).await
        }
    }

    mod encrypted {
        use std::sync::Arc;

        use matrix_sdk_base::{
            event_cache::store::EventCacheStoreError, event_cache_store_integration_tests,
            event_cache_store_integration_tests_time,
        };
        use matrix_sdk_store_encryption::StoreCipher;
        use matrix_sdk_test::async_test;
        use uuid::Uuid;

        use crate::IndexeddbEventCacheStore;

        wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

        async fn get_event_cache_store() -> Result<IndexeddbEventCacheStore, EventCacheStoreError> {
            let name = format!("test-event-cache-store-{}", Uuid::new_v4().as_hyphenated());
            Ok(IndexeddbEventCacheStore::builder()
                .name(name)
                .store_cipher(Arc::new(StoreCipher::new()?))
                .build()
                .await?)
        }

        #[cfg(target_arch = "wasm32")]
        event_cache_store_integration_tests!();

        #[cfg(target_arch = "wasm32")]
        event_cache_store_integration_tests_time!();

        #[async_test]
        async fn test_linked_chunk_new_items_chunk() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_new_items_chunk(store).await
        }

        #[async_test]
        async fn test_add_gap_chunk_and_delete_it_immediately() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_add_gap_chunk_and_delete_it_immediately(store).await
        }

        #[async_test]
        async fn test_linked_chunk_new_gap_chunk() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_new_gap_chunk(store).await
        }

        #[async_test]
        async fn test_linked_chunk_replace_item() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_replace_item(store).await
        }

        #[async_test]
        async fn test_linked_chunk_remove_chunk() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_remove_chunk(store).await
        }

        #[async_test]
        async fn test_linked_chunk_push_items() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_push_items(store).await
        }

        #[async_test]
        async fn test_linked_chunk_remove_item() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_remove_item(store).await
        }

        #[async_test]
        async fn test_linked_chunk_detach_last_items() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_detach_last_items(store).await
        }

        #[async_test]
        async fn test_linked_chunk_start_end_reattach_items() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_start_end_reattach_items(store).await
        }

        #[async_test]
        async fn test_linked_chunk_clear() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_clear(store).await
        }

        #[async_test]
        async fn test_linked_chunk_multiple_rooms() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_multiple_rooms(store).await
        }

        #[async_test]
        async fn test_linked_chunk_update_is_a_transaction() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_linked_chunk_update_is_a_transaction(store).await
        }

        #[async_test]
        async fn test_filter_duplicate_events_no_events() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_filter_duplicate_events_no_events(store).await
        }

        #[async_test]
        async fn test_load_last_chunk() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_load_last_chunk(store).await
        }

        #[async_test]
        async fn test_load_last_chunk_with_a_cycle() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_load_last_chunk_with_a_cycle(store).await
        }

        #[async_test]
        async fn test_load_previous_chunk() {
            let store = get_event_cache_store().await.expect("Failed to get event cache store");
            super::test_load_previous_chunk(store).await
        }
    }
}
