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
use tracing::trace;
use wasm_bindgen::JsValue;
use web_sys::{IdbCursorDirection, IdbKeyRange, IdbTransactionMode};

use crate::serializer::{IndexeddbSerializer, IndexeddbSerializerError, MaybeEncrypted};

mod keys {
    pub const CORE: &str = "core";

    // Rooms is not used as an object store, but it
    // is used for the purposes of secure hashing when
    // constructing keys for object stores.
    pub const ROOMS: &str = "rooms";
    pub const EVENTS: &str = "events";
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
struct TimelineEventForCache {
    id: String,
    content: TimelineEvent,
    room_id: String,
    chunk_id: u64,
    position: usize,
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

                    for (i, event) in items.into_iter().enumerate() {
                        let index = at.index() + i;
                        let id = self.encode_key(vec![
                            (keys::ROOMS, room_id.as_ref(), true),
                            (keys::LINKED_CHUNKS, chunk_id.to_string().as_ref(), false),
                            (keys::EVENTS, &index.to_string(), false),
                        ]);

                        let value = TimelineEventForCache {
                            id: id.clone(),
                            content: event,
                            room_id: room_id.to_string(),
                            chunk_id,
                            position: index,
                        };

                        let value = self.serialize_value_with_id(&id, &value)?;

                        events.put_val(&value)?.into_future().await?;
                    }
                }
                Update::ReplaceItem { at, item } => {
                    let chunk_id = at.chunk_identifier().index();
                    let index = at.index();

                    trace!(%room_id, "replacing item @ {chunk_id}:{index}");

                    let event_id = self.encode_key(vec![
                        (keys::ROOMS, room_id.as_ref(), true),
                        (keys::LINKED_CHUNKS, &chunk_id.to_string(), false),
                        (keys::EVENTS, &index.to_string(), false),
                    ]);

                    let timeline_event = TimelineEventForCache {
                        id: event_id.clone(),
                        content: item,
                        room_id: room_id.to_string(),
                        chunk_id,
                        position: index,
                    };

                    let value = self.serialize_value_with_id(&event_id, &timeline_event)?;

                    events.put_val(&value)?;
                }
                Update::RemoveItem { at } => {
                    let chunk_id = at.chunk_identifier().index();
                    let index = at.index();

                    trace!(%room_id, "removing item @ {chunk_id}:{index}");

                    let id = self.encode_key(vec![
                        (keys::ROOMS, room_id.as_ref(), true),
                        (keys::LINKED_CHUNKS, &chunk_id.to_string(), false),
                        (keys::EVENTS, &index.to_string(), false),
                    ]);

                    events.delete_owned(id)?;
                }
                Update::DetachLastItems { at } => {
                    let chunk_id = at.chunk_identifier().index();
                    let index = at.index();

                    trace!(%room_id, "detaching last items @ {chunk_id}:{index}");

                    let object_store = tx.object_store(keys::EVENTS)?;

                    let lower = self.encode_key(vec![
                        (keys::ROOMS, room_id.as_ref(), true),
                        (keys::LINKED_CHUNKS, chunk_id.to_string().as_ref(), false),
                        (keys::EVENTS, &index.to_string(), false),
                    ]);

                    let upper = self.encode_upper_key(vec![
                        (keys::ROOMS, room_id.as_ref(), true),
                        (keys::LINKED_CHUNKS, chunk_id.to_string().as_ref(), false),
                    ]);

                    let key_range = IdbKeyRange::bound(&lower.into(), &upper.into()).unwrap();

                    let items = object_store.get_all_with_key(&key_range)?.await?;

                    for item in items {
                        let event: TimelineEventForCache = self.deserialize_value_with_id(item)?;
                        if event.position >= index {
                            object_store.delete(&JsValue::from_str(&event.id))?;
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

                        let lower = self.encode_key(vec![
                            (keys::ROOMS, room_id.as_ref(), true),
                            (keys::LINKED_CHUNKS, &chunk.id, false),
                        ]);
                        let upper = self.encode_upper_key(vec![
                            (keys::ROOMS, room_id.as_ref(), true),
                            (keys::LINKED_CHUNKS, &chunk.id, false),
                        ]);

                        let key_range = IdbKeyRange::bound(&lower.into(), &upper.into()).unwrap();

                        events.delete_owned(key_range)?;

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
        let tx = self.inner.transaction_on_multi_with_mode(
            &[keys::LINKED_CHUNKS, keys::GAPS, keys::EVENTS],
            IdbTransactionMode::Readonly,
        )?;

        let chunks = tx.object_store(keys::LINKED_CHUNKS)?;
        let gaps = tx.object_store(keys::GAPS)?;
        let events = tx.object_store(keys::EVENTS)?;

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
                    let lower = self.encode_key(vec![
                        (keys::ROOMS, room_id.as_ref(), true),
                        (keys::LINKED_CHUNKS, &chunk_id.to_string(), false),
                    ]);

                    let upper = self.encode_upper_key(vec![
                        (keys::ROOMS, room_id.as_ref(), true),
                        (keys::LINKED_CHUNKS, &chunk_id.to_string(), false),
                    ]);

                    let events_key_range =
                        IdbKeyRange::bound(&lower.into(), &upper.into()).unwrap();

                    let events = events.get_all_with_key(&events_key_range)?.await?;
                    let mut events_vec = Vec::new();

                    for event in events {
                        let event: TimelineEventForCache = self.deserialize_value_with_id(event)?;
                        events_vec.push(event.content);
                    }

                    let raw_chunk = RawChunk {
                        identifier: ChunkIdentifier::new(chunk_id),
                        content: linked_chunk::ChunkContent::Items(events_vec),
                        previous: previous_chunk_id.map(ChunkIdentifier::new),
                        next: next_chunk_id.map(ChunkIdentifier::new),
                    };

                    raw_chunks.push(raw_chunk);
                }
                ChunkForCache::CHUNK_TYPE_GAP_TYPE_STRING => {
                    let id = linked_chunk.id;
                    let gap = gaps.get_owned(id.clone())?.await?.unwrap();

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
            &[keys::LINKED_CHUNKS, keys::GAPS, keys::EVENTS],
            IdbTransactionMode::Readonly,
        )?;

        let chunks = tx.object_store(keys::LINKED_CHUNKS)?;
        let gaps = tx.object_store(keys::GAPS)?;
        let events = tx.object_store(keys::EVENTS)?;

        let lower = self.encode_key(vec![(keys::ROOMS, room_id.as_ref(), true)]);
        let upper = self.encode_upper_key(vec![(keys::ROOMS, room_id.as_ref(), true)]);
        let range = IdbKeyRange::bound(&lower.into(), &upper.into()).unwrap();

        if let Some(cursor) =
            chunks.open_cursor_with_range_and_direction(&range, IdbCursorDirection::Prev)?.await?
        {
            let chunk: ChunkForCache = self.deserialize_value_with_id(cursor.value())?;
            let chunk_id = chunk.raw_id.to_string();
            let content = match chunk.type_str.as_str() {
                ChunkForCache::CHUNK_TYPE_EVENT_TYPE_STRING => {
                    let lower = self.encode_key(vec![
                        (keys::ROOMS, room_id.as_ref(), true),
                        (keys::LINKED_CHUNKS, &chunk_id, false),
                    ]);

                    let upper = self.encode_upper_key(vec![
                        (keys::ROOMS, room_id.as_ref(), true),
                        (keys::LINKED_CHUNKS, &chunk_id, false),
                    ]);

                    let events_key_range =
                        IdbKeyRange::bound(&lower.into(), &upper.into()).unwrap();

                    let events = events.get_all_with_key(&events_key_range)?.await?;
                    let mut events_vec = Vec::new();
                    for event in events {
                        let event: TimelineEventForCache = self.deserialize_value_with_id(event)?;
                        events_vec.push(event.content);
                    }
                    linked_chunk::ChunkContent::Items(events_vec)
                }
                ChunkForCache::CHUNK_TYPE_GAP_TYPE_STRING => {
                    let gap = gaps.get_owned(chunk.id.clone())?.await?.unwrap();
                    let gap: GapForCache = self.deserialize_value_with_id(gap)?;
                    linked_chunk::ChunkContent::Gap(Gap { prev_token: gap.prev_token })
                }
                s => {
                    return Err(IndexeddbEventCacheStoreError::UnknownChunkType(s.to_owned()));
                }
            };

            let identifier = ChunkIdentifier::new(chunk.raw_id);
            let raw_chunk = RawChunk {
                identifier,
                content,
                previous: chunk.raw_previous.map(ChunkIdentifier::new),
                // TODO: This should always be None, and if it's not, something
                // is wrong with our query... should this be an expect()? Or, at least
                // an error?
                next: chunk.raw_next.map(ChunkIdentifier::new),
            };
            let generator =
                ChunkIdentifierGenerator::new_from_previous_chunk_identifier(identifier);
            Ok((Some(raw_chunk), generator))
        } else {
            Ok((None, ChunkIdentifierGenerator::new_from_scratch()))
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
            &[keys::LINKED_CHUNKS, keys::GAPS, keys::EVENTS],
            IdbTransactionMode::Readonly,
        )?;

        let chunks = tx.object_store(keys::LINKED_CHUNKS)?;
        let gaps = tx.object_store(keys::GAPS)?;
        let events = tx.object_store(keys::EVENTS)?;

        let key = self.encode_key(vec![
            (keys::ROOMS, room_id.as_ref(), true),
            (keys::LINKED_CHUNKS, &before_chunk_identifier.index().to_string(), false),
        ]);
        if let Some(value) = chunks.get_owned(&key)?.await? {
            let chunk: ChunkForCache = self.deserialize_value_with_id(value)?;
            if let Some(previous_key) = chunk.previous {
                if let Some(value) = chunks.get_owned(&previous_key)?.await? {
                    let previous_chunk: ChunkForCache = self.deserialize_value_with_id(value)?;
                    let previous_chunk_id = previous_chunk.raw_id.to_string();
                    let content = match previous_chunk.type_str.as_str() {
                        ChunkForCache::CHUNK_TYPE_EVENT_TYPE_STRING => {
                            let lower = self.encode_key(vec![
                                (keys::ROOMS, room_id.as_ref(), true),
                                (keys::LINKED_CHUNKS, &previous_chunk_id, false),
                            ]);

                            let upper = self.encode_upper_key(vec![
                                (keys::ROOMS, room_id.as_ref(), true),
                                (keys::LINKED_CHUNKS, &previous_chunk_id, false),
                            ]);

                            let events_key_range =
                                IdbKeyRange::bound(&lower.into(), &upper.into()).unwrap();

                            let events = events.get_all_with_key(&events_key_range)?.await?;
                            let mut events_vec = Vec::new();
                            for event in events {
                                let event: TimelineEventForCache = self.deserialize_value_with_id(event)?;
                                events_vec.push(event.content);
                            }
                            linked_chunk::ChunkContent::Items(events_vec)
                        }
                        ChunkForCache::CHUNK_TYPE_GAP_TYPE_STRING => {
                            let gap = gaps.get_owned(previous_chunk.id.clone())?.await?.unwrap();
                            let gap: GapForCache = self.deserialize_value_with_id(gap)?;
                            linked_chunk::ChunkContent::Gap(Gap { prev_token: gap.prev_token })
                        }
                        s => {
                            return Err(IndexeddbEventCacheStoreError::UnknownChunkType(s.to_owned()));
                        }
                    };
                    return Ok(Some(RawChunk {
                        content,
                        identifier: ChunkIdentifier::new(previous_chunk.raw_id),
                        previous: previous_chunk.raw_previous.map(ChunkIdentifier::new),
                        // TODO: This should always be `before_chunk_identifier`, and if it's not, something
                        // is wrong with our query... should this be an expect()? Or, at least
                        // an error?
                        next: previous_chunk.raw_next.map(ChunkIdentifier::new)
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

        let tx = self.inner.transaction_on_multi_with_mode(
            &[keys::EVENTS],
            IdbTransactionMode::Readwrite,
        )?;

        let store = tx.object_store(keys::EVENTS)?;

        // TODO: This is very inefficient, as we are reading and
        // deserializing every event in the room in order to pick
        // out a single one. The problem is that the current
        // schema doesn't easily allow us to find an event without
        // knowing which chunk it is in. To improve this, we will
        // need to add another index to our event store.
        let lower = self.encode_key(vec![
            (keys::ROOMS, room_id.as_ref(), true),
        ]);
        let upper = self.encode_upper_key(vec![
            (keys::ROOMS, room_id.as_ref(), true),
        ]);
        let key_range =
            IdbKeyRange::bound(&lower.into(), &upper.into()).unwrap();

        let mut result = Vec::new();
        let values = store.get_all_with_key(&key_range)?.await?;
        for value in values {
            let event: TimelineEventForCache = self.deserialize_value_with_id(value)?;
            if let Some(event_id) = event.content.event_id() {
                if events.contains(&event_id) {
                    let position = Position::new(ChunkIdentifier::new(event.chunk_id), event.position);
                    result.push((event_id, position))
                }
            }
        }
        Ok(result)
    }

    /// Find an event by its ID.
    async fn find_event(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<Event>, IndexeddbEventCacheStoreError> {
        let tx = self.inner.transaction_on_multi_with_mode(
            &[keys::EVENTS],
            IdbTransactionMode::Readwrite,
        )?;

        let events = tx.object_store(keys::EVENTS)?;

        // TODO: This is very inefficient, as we are reading and
        // deserializing every event in the room in order to pick
        // out a single one. The problem is that the current
        // schema doesn't easily allow us to find an event without
        // knowing which chunk it is in. To improve this, we will
        // need to add another index to our event store.
        let lower = self.encode_key(vec![
            (keys::ROOMS, room_id.as_ref(), true),
        ]);
        let upper = self.encode_upper_key(vec![
            (keys::ROOMS, room_id.as_ref(), true),
        ]);
        let events_key_range =
            IdbKeyRange::bound(&lower.into(), &upper.into()).unwrap();

        let values = events.get_all_with_key(&events_key_range)?.await?;
        for value in values {
            let event: TimelineEventForCache = self.deserialize_value_with_id(value)?;
            if event.id == *event_id {
                return Ok(Some(event.content))
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
        let tx = self.inner.transaction_on_multi_with_mode(
            &[keys::EVENTS],
            IdbTransactionMode::Readwrite,
        )?;

        let events = tx.object_store(keys::EVENTS)?;

        // TODO: This is very inefficient, as we are reading and
        // deserializing every event in the room in order to pick
        // out a single one. The problem is that the current
        // schema doesn't easily allow us to find an event without
        // knowing which chunk it is in. To improve this, we will
        // need to add another index to our event store.
        let lower = self.encode_key(vec![
            (keys::ROOMS, room_id.as_ref(), true),
        ]);
        let upper = self.encode_upper_key(vec![
            (keys::ROOMS, room_id.as_ref(), true),
        ]);
        let events_key_range =
            IdbKeyRange::bound(&lower.into(), &upper.into()).unwrap();

        let mut result = Vec::new();
        let values = events.get_all_with_key(&events_key_range)?.await?;
        for value in values {
            let event: TimelineEventForCache = self.deserialize_value_with_id(value)?;
            if let Some((relates_to, relation_type)) = extract_event_relation(event.content.raw()) {
                let filter_contains_relation_type = filter
                    .map(|filter| {
                        filter
                            .iter()
                            .any(|relation| relation.as_ref() == relation_type)
                    })
                    .unwrap_or(true);
                if event_id == relates_to && filter_contains_relation_type {
                    result.push(event.content);
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
        _room_id: &RoomId,
        _event: Event,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        std::future::ready(Err(IndexeddbEventCacheStoreError::Unsupported)).await
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
    use web_sys::{IdbKeyRange, IdbTransactionMode};

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
        let tx = store
            .inner
            .transaction_on_one_with_mode(keys::EVENTS, IdbTransactionMode::Readonly)
            .unwrap();
        let object_store = tx.object_store(keys::EVENTS).unwrap();

        let lower = store.encode_key(vec![
            (keys::ROOMS, room_id.as_ref(), true),
            (keys::LINKED_CHUNKS, "42", false),
            (keys::EVENTS, "0", false),
        ]);
        let upper = store.encode_upper_key(vec![
            (keys::ROOMS, room_id.as_ref(), true),
            (keys::LINKED_CHUNKS, "42", false),
        ]);
        let key_range = IdbKeyRange::bound(&lower.into(), &upper.into()).unwrap();

        let events = object_store.get_all_with_key(&key_range).unwrap().await.unwrap();
        assert_eq!(events.length(), 1);
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
    }
}
