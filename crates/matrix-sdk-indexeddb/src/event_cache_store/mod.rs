// Copyright 2020 The Matrix.org Foundation C.I.C.
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
// limitations under the License.
mod builder;
mod error;
mod indexeddb_serializer;
mod migrations;
mod operations;

use crate::event_cache_store::indexeddb_serializer::IndexeddbSerializer;
use async_trait::async_trait;
use indexed_db_futures::IdbDatabase;
use indexed_db_futures::IdbQuerySource;
use js_sys::Object;
use js_sys::Reflect;
use js_sys::Uint8Array;
use matrix_sdk_base::deserialized_responses::TimelineEvent;
use matrix_sdk_base::event_cache::store::media::EventCacheStoreMedia;
use matrix_sdk_base::linked_chunk;
use matrix_sdk_base::media::UniqueKey;
use matrix_sdk_base::{
    event_cache::{
        store::{
            media::{
                // EventCacheStoreMedia,
                IgnoreMediaRetentionPolicy,
                MediaRetentionPolicy,
                MediaService,
            },
            EventCacheStore,
        },
        Event, Gap,
    },
    linked_chunk::{
        // ChunkContent,
        ChunkIdentifier,
        RawChunk,
        Update,
    },
    media::MediaRequestParameters,
    // UniqueKey
};

use std::borrow::Cow;
use std::future::IntoFuture;
use std::sync::Arc;

use matrix_sdk_store_encryption::StoreCipher;
use operations::{get_media_retention_policy, set_media_retention_policy};
use ruma::{time::SystemTime, MilliSecondsSinceUnixEpoch, MxcUri, RoomId};

use serde::Deserialize;
use serde::Serialize;
use tracing::trace;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen::JsValue;
use web_sys::IdbKeyRange;
use web_sys::IdbTransactionMode;

pub use builder::IndexeddbEventCacheStoreBuilder;
pub use error::IndexeddbEventCacheStoreError;

mod keys {
    pub const CORE: &str = "core";
    pub const EVENTS: &str = "events";
    pub const LINKED_CHUNKS: &str = "linked_chunks";
    pub const GAPS: &str = "gaps";
    pub const MEDIA: &str = "media";

    // Used as a single key to store/retrieve the media retention policy
    pub const MEDIA_RETENTION_POLICY: &str = "media_retention_policy";
}

pub const KEY_SEPARATOR: &str = "\u{001D}";
/// The string used to identify a chunk of type events, in the `type` field in
/// the database.
const CHUNK_TYPE_EVENT_TYPE_STRING: &str = "E";
/// The string used to identify a chunk of type gap, in the `type` field in the
/// database.
const CHUNK_TYPE_GAP_TYPE_STRING: &str = "G";

pub struct IndexeddbEventCacheStore {
    pub(crate) inner: IdbDatabase,
    pub(crate) store_cipher: Option<Arc<StoreCipher>>,
    pub(crate) serializer: IndexeddbSerializer,
    pub(crate) media_service: Arc<MediaService>,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for IndexeddbEventCacheStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexeddbEventCacheStore").finish()
    }
}

impl IndexeddbEventCacheStore {
    pub fn builder() -> IndexeddbEventCacheStoreBuilder {
        IndexeddbEventCacheStoreBuilder::new()
    }

    pub fn get_id(&self, room_id: &str, object_id: &str) -> String {
        let id_raw = [room_id, KEY_SEPARATOR, object_id].concat();
        self.serializer.encode_key_as_string(room_id.as_ref(), id_raw)
    }

    pub fn get_event_id(&self, room_id: &str, chunk_id: &str, index: usize) -> String {
        let id_raw = [room_id, KEY_SEPARATOR, chunk_id, KEY_SEPARATOR, &index.to_string()].concat();
        self.serializer.encode_key_as_string(room_id.as_ref(), id_raw)
    }

    pub fn get_event_id_upper_bound(&self, room_id: &str, chunk_id: &str, limiter: &str) -> String {
        let id_raw = [room_id, KEY_SEPARATOR, chunk_id, KEY_SEPARATOR, limiter].concat();
        self.serializer.encode_key_as_string(room_id.as_ref(), id_raw)
    }

    pub fn get_room_and_chunk_id(&self, id: &str) -> (String, u64) {
        let mut parts = id.splitn(2, KEY_SEPARATOR);
        let room_id = parts.next().unwrap().to_owned();
        let object_id = parts.next().unwrap().parse::<u64>().unwrap();
        (room_id, object_id)
    }

    pub fn get_chunk_id(&self, id: &Option<String>) -> Option<u64> {
        match id {
            Some(id) => {
                let mut parts = id.splitn(2, KEY_SEPARATOR);
                let _room_id = parts.next().unwrap().to_owned();
                let object_id = parts.next().unwrap().parse::<u64>().unwrap();
                Some(object_id)
            }
            None => None,
        }
    }

    fn encode_value(&self, value: Vec<u8>) -> Result<Vec<u8>> {
        if let Some(key) = &self.store_cipher {
            let encrypted = key.encrypt_value_data(value)?;
            Ok(rmp_serde::to_vec_named(&encrypted)?)
        } else {
            Ok(value)
        }
    }

    fn decode_value<'a>(&self, value: &'a [u8]) -> Result<Cow<'a, [u8]>> {
        if let Some(key) = &self.store_cipher {
            let encrypted = rmp_serde::from_slice(value)?;
            let decrypted = key.decrypt_value_data(encrypted)?;
            Ok(Cow::Owned(decrypted))
        } else {
            Ok(Cow::Borrowed(value))
        }
    }
}

async fn js_value_uint8_array_to_vec(data: JsValue) -> Result<Vec<u8>, JsValue> {
    let uint8_array: Uint8Array = data.dyn_into()?;
    let mut vec = vec![0; uint8_array.length() as usize];
    uint8_array.copy_to(&mut vec);
    Ok(vec)
}

type Result<A, E = IndexeddbEventCacheStoreError> = std::result::Result<A, E>;

#[derive(Debug, Serialize, Deserialize)]
struct ChunkForCache {
    id: String,
    previous: Option<String>,
    next: Option<String>,
    type_str: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexedDbGapForCache {
    prev_token: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct TimelineEventForCache {
    id: String,
    content: TimelineEvent,
    room_id: String,
    position: usize,
}

#[wasm_bindgen]
struct MediaForCache {
    id: String,
    uri: String,
    format: String,
    data: Vec<u8>,
    last_access: SystemTime,
    ignore_policy: IgnoreMediaRetentionPolicy,
}

impl MediaForCache {
    pub fn to_js_value(self) -> JsValue {
        let obj = Object::new();
        Reflect::set(&obj, &JsValue::from_str("id"), &JsValue::from_str(&self.id)).unwrap();
        Reflect::set(&obj, &JsValue::from_str("uri"), &JsValue::from_str(&self.uri)).unwrap();
        Reflect::set(&obj, &JsValue::from_str("format"), &JsValue::from_str(&self.format)).unwrap();
        Reflect::set(&obj, &JsValue::from_str("data"), &Uint8Array::from(&self.data[..]).into())
            .unwrap();
        Reflect::set(
            &obj,
            &JsValue::from_str("last_access"),
            &JsValue::from_f64(
                self.last_access.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs_f64(),
            ),
        )
        .unwrap();
        Reflect::set(
            &obj,
            &JsValue::from_str("ignore_policy"),
            &JsValue::from_bool(self.ignore_policy.is_yes()),
        )
        .unwrap();
        obj.into()
    }

    pub async fn from_js_value(obj: JsValue) -> Self {
        let id = Reflect::get(&obj, &JsValue::from_str("id")).unwrap().as_string().unwrap();
        let uri = Reflect::get(&obj, &JsValue::from_str("uri")).unwrap().as_string().unwrap();
        let data =
            js_value_uint8_array_to_vec(Reflect::get(&obj, &JsValue::from_str("data")).unwrap())
                .await
                .unwrap();
        let format = Reflect::get(&obj, &JsValue::from_str("format")).unwrap().as_string().unwrap();
        let last_access = SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs_f64(
                Reflect::get(&obj, &JsValue::from_str("last_access")).unwrap().as_f64().unwrap(),
            );
        let ignore_policy = if Reflect::get(&obj, &JsValue::from_str("ignore_policy"))
            .unwrap()
            .as_bool()
            .unwrap()
        {
            IgnoreMediaRetentionPolicy::Yes
        } else {
            IgnoreMediaRetentionPolicy::No
        };
        Self { id, uri, format, data, last_access, ignore_policy }
    }
}

// #[cfg(target_arch = "wasm32")]
// macro_rules! impl_event_cache_store {
//     ({ $($body:tt)* }) => {
//         #[async_trait(?Send)]
//         impl EventCacheStore for IndexeddbEventCacheStore {
//             type Error = IndexeddbEventCacheStoreError;

//             $($body)*
//         }
//     };
// }

// #[cfg(not(target_arch = "wasm32"))]
// macro_rules! impl_event_cache_store {
//     ({ $($body:tt)* }) => {
//         impl IndexeddbEventCacheStore {
//             $($body)*
//         }
//     };
// }

// TODO We need to implement this trait only on wasm32 target
// But it kills autocomplete and inlay types in the IDE
// When things are ready to commit, should be replaced with the macro above
#[async_trait(?Send)]
impl EventCacheStore for IndexeddbEventCacheStore {
    type Error = IndexeddbEventCacheStoreError;

    async fn handle_linked_chunk_updates(
        &self,
        room_id: &RoomId,
        updates: Vec<Update<Event, Gap>>,
    ) -> Result<()> {
        let tx = self.inner.transaction_on_multi_with_mode(
            &[&keys::LINKED_CHUNKS, &keys::GAPS, &keys::EVENTS],
            IdbTransactionMode::Readwrite,
        )?;

        let linked_chunks = tx.object_store(keys::LINKED_CHUNKS)?;
        let gaps = tx.object_store(keys::GAPS)?;
        let events = tx.object_store(keys::EVENTS)?;

        for update in updates {
            match update {
                Update::NewItemsChunk { previous, new, next } => {
                    let previous = previous
                        .as_ref()
                        .map(ChunkIdentifier::index)
                        .map(|n| self.get_id(room_id.as_ref(), n.to_string().as_ref()));

                    let id = self.get_id(room_id.as_ref(), new.index().to_string().as_ref());
                    let next = next
                        .as_ref()
                        .map(ChunkIdentifier::index)
                        .map(|n| self.get_id(room_id.as_ref(), n.to_string().as_ref()));

                    trace!(%room_id, "Inserting new chunk (prev={previous:?}, new={id}, next={next:?})");

                    let chunk = ChunkForCache {
                        id: id.clone(),
                        previous: previous.clone(),
                        next: next.clone(),
                        type_str: CHUNK_TYPE_EVENT_TYPE_STRING.to_owned(),
                    };

                    let chunk_serialized = self.serializer.serialize_into_object(&id, &chunk)?;

                    linked_chunks.add_val_owned(chunk_serialized)?;

                    if let Some(previous) = previous {
                        let previous = linked_chunks.get_owned(&previous)?.await?;

                        if let Some(previous) = previous {
                            let previous: ChunkForCache =
                                self.serializer.deserialize_into_object(previous)?;

                            let previous = ChunkForCache {
                                id: previous.id,
                                previous: previous.previous,
                                next: Some(id.clone()),
                                type_str: previous.type_str,
                            };

                            let previous =
                                self.serializer.serialize_into_object(&previous.id, &previous)?;

                            linked_chunks.put_val_owned(previous)?;
                        }
                    }

                    if let Some(next) = next {
                        let next = linked_chunks.get_owned(&next)?.await?;
                        if let Some(next) = next {
                            let next: ChunkForCache =
                                self.serializer.deserialize_into_object(next)?;

                            let next = ChunkForCache {
                                id: next.id,
                                previous: Some(id.clone()),
                                next: next.next,
                                type_str: next.type_str,
                            };

                            let next = self.serializer.serialize_into_object(&next.id, &next)?;

                            linked_chunks.put_val_owned(next)?;
                        }
                    }
                }
                Update::NewGapChunk { previous, new, next, gap } => {
                    let previous = previous
                        .as_ref()
                        .map(ChunkIdentifier::index)
                        .map(|n| self.get_id(room_id.as_ref(), n.to_string().as_ref()));

                    let id = self.get_id(room_id.as_ref(), new.index().to_string().as_ref());
                    let next = next
                        .as_ref()
                        .map(ChunkIdentifier::index)
                        .map(|n| self.get_id(room_id.as_ref(), n.to_string().as_ref()));

                    trace!(%room_id,"Inserting new gap (prev={previous:?}, new={id}, next={next:?})");

                    let chunk = ChunkForCache {
                        id: id.clone(),
                        previous: previous.clone(),
                        next: next.clone(),
                        type_str: CHUNK_TYPE_GAP_TYPE_STRING.to_owned(),
                    };

                    let chunk_serialized = self.serializer.serialize_into_object(&id, &chunk)?;

                    linked_chunks.add_val_owned(chunk_serialized)?;

                    if let Some(previous) = previous {
                        let previous = linked_chunks.get_owned(&previous)?.await?;
                        if let Some(previous) = previous {
                            let previous: ChunkForCache =
                                self.serializer.deserialize_into_object(previous)?;

                            let previous = ChunkForCache {
                                id: previous.id,
                                previous: previous.previous,
                                next: Some(id.clone()),
                                type_str: previous.type_str,
                            };

                            let previous =
                                self.serializer.serialize_into_object(&previous.id, &previous)?;

                            linked_chunks.put_val(&previous)?;
                        }
                    }

                    // update next if there
                    if let Some(next) = next {
                        let next = linked_chunks.get_owned(&next)?.await?;
                        if let Some(next) = next {
                            let next: ChunkForCache =
                                self.serializer.deserialize_into_object(next)?;

                            let next = ChunkForCache {
                                id: next.id,
                                previous: Some(id.clone()),
                                next: next.next,
                                type_str: next.type_str,
                            };

                            let next = self.serializer.serialize_into_object(&next.id, &next)?;

                            linked_chunks.put_val(&next)?;
                        }
                    }

                    let gap = IndexedDbGapForCache { prev_token: gap.prev_token };

                    let serialized_gap = self.serializer.serialize_into_object(&id, &gap)?;

                    gaps.add_val_owned(serialized_gap)?;
                }

                Update::RemoveChunk(id) => {
                    let id = self.get_id(room_id.as_ref(), id.index().to_string().as_ref());

                    trace!("Removing chunk {id:?}");

                    let chunk = linked_chunks.get_owned(id.clone())?.await?;
                    if let Some(chunk) = chunk {
                        let chunk: ChunkForCache =
                            self.serializer.deserialize_into_object(chunk)?;

                        if let Some(previous) = chunk.previous.clone() {
                            let previous = linked_chunks.get_owned(previous)?.await?;
                            if let Some(previous) = previous {
                                let previous: ChunkForCache =
                                    self.serializer.deserialize_into_object(previous)?;

                                let previous = ChunkForCache {
                                    id: previous.id,
                                    previous: previous.previous,
                                    next: chunk.next.clone(),
                                    type_str: previous.type_str,
                                };
                                let previous = self
                                    .serializer
                                    .serialize_into_object(&previous.id, &previous)?;
                                linked_chunks.put_val(&previous)?;
                            }
                        }

                        if let Some(next) = chunk.next {
                            let next = linked_chunks.get_owned(next)?.await?;
                            if let Some(next) = next {
                                let next: ChunkForCache =
                                    self.serializer.deserialize_into_object(next)?;

                                let next = ChunkForCache {
                                    id: next.id,
                                    previous: chunk.previous,
                                    next: next.next,
                                    type_str: next.type_str,
                                };
                                let next =
                                    self.serializer.serialize_into_object(&next.id, &next)?;

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
                        let id = self.get_event_id(
                            room_id.as_ref(),
                            chunk_id.to_string().as_ref(),
                            index,
                        );

                        let value = TimelineEventForCache {
                            id: id.clone(),
                            content: event,
                            room_id: room_id.to_string(),
                            position: index,
                        };

                        let value = self.serializer.serialize_into_object(&id, &value)?;

                        events.put_val(&value)?.into_future().await?;
                    }
                }
                Update::ReplaceItem { at, item } => {
                    let chunk_id = at.chunk_identifier().index();
                    let index = at.index();

                    trace!(%room_id, "replacing item @ {chunk_id}:{index}");

                    let event_id = self.get_event_id(
                        room_id.to_string().as_ref(),
                        chunk_id.to_string().as_ref(),
                        index,
                    );

                    let timeline_event = TimelineEventForCache {
                        id: event_id.clone(),
                        content: item,
                        room_id: room_id.to_string(),
                        position: index,
                    };

                    let value =
                        self.serializer.serialize_into_object(&event_id, &timeline_event)?;

                    events.put_val(&value)?;
                }
                Update::RemoveItem { at } => {
                    let chunk_id = at.chunk_identifier().index();
                    let index = at.index();

                    trace!(%room_id, "removing item @ {chunk_id}:{index}");

                    let object_store = tx.object_store(keys::EVENTS)?;

                    let event_id: JsValue =
                        self.get_event_id(room_id.as_ref(), &chunk_id.to_string(), index).into();
                    object_store.delete_owned(event_id)?;
                }
                Update::DetachLastItems { at } => {
                    let chunk_id = at.chunk_identifier().index();
                    let index = at.index();

                    trace!(%room_id, "detaching last items @ {chunk_id}:{index}");

                    let object_store = tx.object_store(keys::EVENTS)?;

                    let lower_bound = JsValue::from_str(&self.get_event_id(
                        room_id.as_ref(),
                        &chunk_id.to_string(),
                        index,
                    ));

                    let upper_bound: JsValue = self
                        .get_event_id_upper_bound(
                            room_id.as_ref(),
                            &chunk_id.to_string(),
                            "\u{FFFF}",
                        )
                        .into();

                    let key_range = IdbKeyRange::bound(&lower_bound, &upper_bound).unwrap();

                    let items = object_store.get_all_with_key(&key_range)?.await?;

                    for item in items {
                        let event: TimelineEventForCache =
                            self.serializer.deserialize_into_object(item)?;
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
                    let lower_bound = JsValue::from_str(&room_id.as_ref());
                    let upper_bound = JsValue::from_str(&(room_id.to_string() + "\u{FFFF}"));

                    let chunks_key_range = IdbKeyRange::bound(&lower_bound, &upper_bound).unwrap();

                    let chunks = linked_chunks.get_all_with_key(&chunks_key_range)?.await?;

                    for chunk in chunks {
                        let chunk: ChunkForCache =
                            self.serializer.deserialize_into_object(chunk)?;

                        let lower = JsValue::from_str(&self.get_id(room_id.as_ref(), &chunk.id));
                        let upper = JsValue::from_str(
                            &(self.get_id(room_id.as_ref(), &chunk.id) + "\u{FFFF}"),
                        );

                        let key_range = IdbKeyRange::bound(&lower, &upper).unwrap();

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
    async fn reload_linked_chunk(&self, room_id: &RoomId) -> Result<Vec<RawChunk<Event, Gap>>> {
        let tx = self
            .inner
            .transaction_on_one_with_mode(keys::LINKED_CHUNKS, IdbTransactionMode::Readonly)?;

        let object_store = tx.object_store(keys::LINKED_CHUNKS)?;

        let lower = JsValue::from_str(&room_id.as_ref());
        let upper = JsValue::from_str(&(room_id.to_string() + "\u{FFFF}"));

        let key_range = IdbKeyRange::bound(&lower, &upper).unwrap();

        let linked_chunks = object_store.get_all_with_key_owned(key_range)?.await?;

        let mut raw_chunks = Vec::new();

        for linked_chunk in linked_chunks {
            let linked_chunk: ChunkForCache =
                self.serializer.deserialize_into_object(linked_chunk)?;
            let chunk_id = self.get_chunk_id(&Some(linked_chunk.id.clone())).unwrap();
            let previous_chunk_id = self.get_chunk_id(&linked_chunk.previous);
            let next_chunk_id = self.get_chunk_id(&linked_chunk.next);

            if linked_chunk.type_str == CHUNK_TYPE_GAP_TYPE_STRING {
                let gaps_tx = self
                    .inner
                    .transaction_on_one_with_mode(keys::GAPS, IdbTransactionMode::Readonly)?;

                let gaps_object_store = gaps_tx.object_store(keys::GAPS)?;

                let gap_id = linked_chunk.id;
                let gap_id_js_value = JsValue::from_str(&gap_id);
                let gap_js_value = gaps_object_store.get_owned(&gap_id_js_value)?.await?;

                let gap: IndexedDbGapForCache =
                    self.serializer.deserialize_into_object(gap_js_value.unwrap())?;

                let gap = Gap { prev_token: gap.prev_token };

                let raw_chunk = RawChunk {
                    identifier: ChunkIdentifier::new(chunk_id),
                    content: linked_chunk::ChunkContent::Gap(gap),
                    previous: previous_chunk_id.map(ChunkIdentifier::new),
                    next: next_chunk_id.map(ChunkIdentifier::new),
                };

                raw_chunks.push(raw_chunk);
            } else {
                let events_tx = self
                    .inner
                    .transaction_on_one_with_mode(keys::EVENTS, IdbTransactionMode::Readonly)?;

                let events_object_store = events_tx.object_store(keys::EVENTS)?;

                let lower =
                    JsValue::from_str(&self.get_id(room_id.as_ref(), &chunk_id.to_string()));
                let upper = JsValue::from_str(
                    &(self.get_id(room_id.as_ref(), &chunk_id.to_string()) + "\u{FFFF}"),
                );

                let events_key_range = IdbKeyRange::bound(&lower, &upper).unwrap();

                let events = events_object_store.get_all_with_key(&events_key_range)?.await?;
                let mut events_vec = Vec::new();

                for event in events {
                    let event: TimelineEventForCache =
                        self.serializer.deserialize_into_object(event)?;
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
        }

        Ok(raw_chunks)
    }

    /// Clear persisted events for all the rooms.
    ///
    /// This will empty and remove all the linked chunks stored previously,
    /// using the above [`Self::handle_linked_chunk_updates`] methods.
    async fn clear_all_rooms_chunks(&self) -> Result<()> {
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
    ) -> Result<()> {
        self.media_service.add_media_content(self, request, content, ignore_policy).await
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
    ) -> Result<()> {
        let tx =
            self.inner.transaction_on_one_with_mode(keys::MEDIA, IdbTransactionMode::Readwrite)?;
        let store = tx.object_store(keys::MEDIA)?;

        let obj = store.get_owned(&JsValue::from(from.unique_key().as_str()))?.await?;

        if let Some(obj) = obj {
            let mut object = MediaForCache::from_js_value(obj).await;
            object.id = to.unique_key().to_string();
            let req = store.put_val_owned(&object.to_js_value())?;
            req.into_future().await?;

            let req = store.delete(&JsValue::from(from.unique_key().as_str()))?;
            req.into_future().await?;
        }

        Ok(())
    }

    /// Get a media file's content out of the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    async fn get_media_content(&self, request: &MediaRequestParameters) -> Result<Option<Vec<u8>>> {
        self.media_service.get_media_content(self, request).await
    }

    /// Remove a media file's content from the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    async fn remove_media_content(&self, request: &MediaRequestParameters) -> Result<()> {
        let tx =
            self.inner.transaction_on_one_with_mode(keys::MEDIA, IdbTransactionMode::Readwrite)?;
        let store = tx.object_store(keys::MEDIA)?;

        let req = store.delete(&JsValue::from(request.unique_key().as_str()))?;
        req.into_future().await?;
        Ok(())
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
    async fn get_media_content_for_uri(&self, uri: &MxcUri) -> Result<Option<Vec<u8>>> {
        self.media_service.get_media_content_for_uri(self, uri).await
        // let tx =
        //     self.inner.transaction_on_one_with_mode(keys::MEDIA, IdbTransactionMode::Readonly)?;
        // let store = tx.object_store(keys::MEDIA)?;

        // let lower = JsValue::from_str(&(uri.to_string() + "_"));
        // let upper = JsValue::from_str(&(uri.to_string() + "_" + "\u{FFFF}"));

        // let key_range = IdbKeyRange::bound(&lower, &upper).unwrap();

        // let blob = store.get_owned(&key_range)?.await?;

        // if let Some(blob) = blob {
        //     let data = blob_to_vec(blob).await.unwrap();
        //     Ok(Some(data))
        // } else {
        //     Ok(None)
        // }
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
    async fn remove_media_content_for_uri(&self, uri: &MxcUri) -> Result<()> {
        let tx =
            self.inner.transaction_on_one_with_mode(keys::MEDIA, IdbTransactionMode::Readwrite)?;
        let store = tx.object_store(keys::MEDIA)?;

        let lower = JsValue::from_str(&(uri.to_string() + "_"));
        let upper = JsValue::from_str(&(uri.to_string() + "_" + "\u{FFFF}"));

        let key_range = IdbKeyRange::bound(&lower, &upper).unwrap();

        store.delete_owned(key_range)?.await?;

        Ok(())
    }

    fn media_retention_policy(&self) -> MediaRetentionPolicy {
        self.media_service.media_retention_policy()
    }

    /// Set the `MediaRetentionPolicy` to use for deciding whether to store or
    /// keep media content.
    ///
    /// # Arguments
    ///
    /// * `policy` - The `MediaRetentionPolicy` to use.
    async fn set_media_retention_policy(&self, policy: MediaRetentionPolicy) -> Result<()> {
        self.media_service.set_media_retention_policy(self, policy).await
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
    ) -> Result<()> {
        self.media_service.set_ignore_media_retention_policy(self, request, ignore_policy).await
    }

    /// Clean up the media cache with the current `MediaRetentionPolicy`.
    ///
    /// If there is already an ongoing cleanup, this is a noop.
    async fn clean_up_media_cache(&self) -> Result<()> {
        self.media_service.clean_up_media_cache(self).await
    }

    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<bool> {
        // As of 2023-06-23, the code below hasn't been tested yet.
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

        let prev = object_store.get(&key)?.await?;
        match prev {
            Some(prev) => {
                let lease: Lease = self.serializer.deserialize_value(prev)?;
                if lease.holder == holder || lease.expiration_ts < now_ts {
                    object_store.put_key_val(
                        &key,
                        &self
                            .serializer
                            .serialize_value(&Lease { holder: holder.to_owned(), expiration_ts })?,
                    )?;
                    Ok(true)
                } else {
                    Ok(false)
                }
            }
            None => {
                object_store.put_key_val(
                    &key,
                    &self
                        .serializer
                        .serialize_value(&Lease { holder: holder.to_owned(), expiration_ts })?,
                )?;
                Ok(true)
            }
        }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
// #[async_trait(?Send)]
impl EventCacheStoreMedia for IndexeddbEventCacheStore {
    type Error = IndexeddbEventCacheStoreError;

    async fn media_retention_policy_inner(
        &self,
    ) -> Result<Option<MediaRetentionPolicy>, Self::Error> {
        get_media_retention_policy(&self.inner).await
    }

    async fn set_media_retention_policy_inner(&self, policy: MediaRetentionPolicy) -> Result<()> {
        set_media_retention_policy(&self.inner, policy).await
    }

    async fn add_media_content_inner(
        &self,
        request: &MediaRequestParameters,
        data: Vec<u8>,
        last_access: SystemTime,
        policy: MediaRetentionPolicy,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<()> {
        let data = self.encode_value(data)?;

        if !ignore_policy.is_yes() && policy.exceeds_max_file_size(data.len()) {
            return Ok(());
        }

        let tx =
            self.inner.transaction_on_one_with_mode(keys::MEDIA, IdbTransactionMode::Readwrite)?;
        let store = tx.object_store(keys::MEDIA)?;

        let object = MediaForCache {
            id: request.unique_key(),
            uri: request.source.unique_key(),
            format: request.format.unique_key(),
            data,
            last_access,
            ignore_policy,
        };

        let js_object = object.to_js_value();
        let req = store.put_val_owned(js_object)?;
        req.into_future().await?;
        Ok(())
    }

    async fn set_ignore_media_retention_policy_inner(
        &self,
        request: &MediaRequestParameters,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<()> {
        let id = request.unique_key();
        let tx =
            self.inner.transaction_on_one_with_mode(keys::MEDIA, IdbTransactionMode::Readwrite)?;
        let store = tx.object_store(keys::MEDIA)?;

        let object = store.get_owned(&JsValue::from_str(id.as_str()))?.await?;

        if let Some(object) = object {
            let mut object = MediaForCache::from_js_value(object).await;
            object.ignore_policy = ignore_policy;
            let req = store.put_val_owned(&object.to_js_value())?;
            req.into_future().await?;
        }

        Ok(())
    }

    async fn get_media_content_inner(
        &self,
        request: &MediaRequestParameters,
        current_time: SystemTime,
    ) -> Result<Option<Vec<u8>>> {
        let id = request.unique_key();
        let tx =
            self.inner.transaction_on_one_with_mode(keys::MEDIA, IdbTransactionMode::Readwrite)?;
        let store = tx.object_store(keys::MEDIA)?;

        let data = store.get_owned(&JsValue::from(id.as_str()))?.await?;

        if let Some(data) = data {
            let mut object = MediaForCache::from_js_value(data).await;
            let data_clone = object.data.clone();
            object.last_access = current_time;
            let req = store.put_val_owned(&object.to_js_value())?;
            req.into_future().await?;
            let decoded_data = self.decode_value(&data_clone)?;
            Ok(Some(decoded_data.as_ref().to_vec()))
        } else {
            Ok(None)
        }
    }

    async fn get_media_content_for_uri_inner(
        &self,
        uri: &MxcUri,
        current_time: SystemTime,
    ) -> Result<Option<Vec<u8>>> {
        let tx =
            self.inner.transaction_on_one_with_mode(keys::MEDIA, IdbTransactionMode::Readwrite)?;
        let store = tx.object_store(keys::MEDIA)?;

        let lower = JsValue::from_str(&(uri.to_string() + "_"));
        let upper = JsValue::from_str(&(uri.to_string() + "_" + "\u{FFFF}"));

        let key_range = IdbKeyRange::bound(&lower, &upper).unwrap();

        let data = store.get_owned(&key_range)?.await?;

        if let Some(data) = data {
            let mut object = MediaForCache::from_js_value(data).await;
            let data_clone = object.data.clone();
            object.last_access = current_time;
            let req = store.put_val_owned(&object.to_js_value())?;
            req.into_future().await?;
            let decoded_data = self.decode_value(&data_clone)?;
            Ok(Some(decoded_data.as_ref().to_vec()))
        } else {
            Ok(None)
        }
    }

    async fn clean_up_media_cache_inner(
        &self,
        policy: MediaRetentionPolicy,
        current_time: SystemTime,
    ) -> Result<()> {
        if !policy.has_limitations() {
            return Ok(());
        }

        let tx =
            self.inner.transaction_on_one_with_mode(keys::MEDIA, IdbTransactionMode::Readwrite)?;
        let store = tx.object_store(keys::MEDIA)?;

        // First, check media content that exceed the max filesize.
        if let Some(max_file_size) = policy.computed_max_file_size() {
            let key_range = IdbKeyRange::lower_bound(&JsValue::from(0)).unwrap();
            let items = store.get_all_with_key(&key_range)?.await?;

            for item in items {
                let media: MediaForCache = MediaForCache::from_js_value(item).await;
                if !media.ignore_policy.is_yes() && media.data.len() > max_file_size {
                    store.delete(&JsValue::from_str(&media.id))?.await?;
                }
            }
        }

        // Then, clean up expired media content.
        if let Some(last_access_expiry) = policy.last_access_expiry {
            let current_timestamp =
                current_time.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
            let expiry_secs = last_access_expiry.as_secs();
            let key_range = IdbKeyRange::lower_bound(&JsValue::from(0)).unwrap();
            let items = store.get_all_with_key(&key_range)?.await?;

            for item in items {
                let media: MediaForCache = MediaForCache::from_js_value(item).await;
                let last_access_secs =
                    media.last_access.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
                if !media.ignore_policy.is_yes()
                    && (current_timestamp - last_access_secs) >= expiry_secs
                {
                    store.delete(&JsValue::from_str(&media.id))?.await?;
                }
            }
        }

        // Finally, if the cache size is too big, remove old items until it fits.
        if let Some(max_cache_size) = policy.max_cache_size {
            let key_range = IdbKeyRange::lower_bound(&JsValue::from(0)).unwrap();
            let items = store.get_all_with_key(&key_range)?.await?;

            let mut accumulated_items_size = 0usize;
            let mut limit_reached = false;
            let mut items_to_remove = Vec::new();
            let mut media_items: Vec<MediaForCache> = Vec::new();

            for item in items {
                let media: MediaForCache = MediaForCache::from_js_value(item).await;
                if !media.ignore_policy.is_yes() {
                    media_items.push(media);
                }
            }

            media_items.sort_by_key(|media| std::cmp::Reverse(media.last_access));

            for media in media_items {
                let size = media.data.len();
                if limit_reached {
                    items_to_remove.push(media.id);
                    continue;
                }

                match accumulated_items_size.checked_add(size) {
                    Some(acc) if acc > max_cache_size => {
                        limit_reached = true;
                        items_to_remove.push(media.id);
                    }
                    Some(acc) => accumulated_items_size = acc,
                    None => {
                        limit_reached = true;
                        items_to_remove.push(media.id);
                    }
                }
            }

            for id in items_to_remove {
                store.delete(&JsValue::from_str(&id))?.await?;
            }
        }

        Ok(())
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
                media::IgnoreMediaRetentionPolicy,
                EventCacheStore, EventCacheStoreError,
            },
            Gap,
        },
        event_cache_store_integration_tests, event_cache_store_integration_tests_time,
        event_cache_store_media_integration_tests,
        linked_chunk::{ChunkContent, ChunkIdentifier, Position, Update},
        media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings},
    };
    use matrix_sdk_test::{async_test, DEFAULT_TEST_ROOM_ID};
    use ruma::{events::room::MediaSource, media::Method, mxc_uri, room_id, uint};
    use std::time::Duration;
    use uuid::Uuid;
    use web_sys::{IdbKeyRange, IdbTransactionMode};

    use crate::event_cache_store::ChunkForCache;

    use super::{keys, IndexeddbEventCacheStore, MediaForCache};

    wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

    async fn get_event_cache_store() -> Result<IndexeddbEventCacheStore, EventCacheStoreError> {
        let db_name = format!("test-event-cache-store-{}", Uuid::new_v4().as_hyphenated());
        Ok(IndexeddbEventCacheStore::builder().name(db_name).build().await?)
    }

    #[cfg(target_arch = "wasm32")]
    async fn sleep(duration: Duration) {
        gloo_timers::future::TimeoutFuture::new(duration.as_millis() as u32).await;
    }

    #[cfg(not(target_arch = "wasm32"))]
    async fn sleep(duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    event_cache_store_integration_tests!();
    event_cache_store_integration_tests_time!();
    event_cache_store_media_integration_tests!(with_media_size_tests);

    async fn get_event_cache_store_content_sorted_by_last_access(
        event_cache_store: &IndexeddbEventCacheStore,
    ) -> Vec<Vec<u8>> {
        let tx = event_cache_store
            .inner
            .transaction_on_one_with_mode(keys::MEDIA, IdbTransactionMode::Readonly)
            .expect("creating transaction failed");
        let store = tx.object_store(keys::MEDIA).expect("getting object store failed");

        let items = store
            .get_all()
            .expect("getting all items failed")
            .await
            .expect("awaiting items failed");

        let mut media_items: Vec<MediaForCache> = Vec::new();

        for item in items {
            let media: MediaForCache = MediaForCache::from_js_value(item).await;
            media_items.push(media);
        }

        media_items.sort_by_key(|media| std::cmp::Reverse(media.last_access));

        media_items.into_iter().map(|media| media.data).collect()
    }

    #[async_test]
    async fn test_last_access() {
        let event_cache_store = get_event_cache_store().await.expect("creating media cache failed");
        let uri = mxc_uri!("mxc://localhost/media");
        let file_request = MediaRequestParameters {
            source: MediaSource::Plain(uri.to_owned()),
            format: MediaFormat::File,
        };
        let thumbnail_request = MediaRequestParameters {
            source: MediaSource::Plain(uri.to_owned()),
            format: MediaFormat::Thumbnail(MediaThumbnailSettings::with_method(
                Method::Crop,
                uint!(100),
                uint!(100),
            )),
        };

        let content: Vec<u8> = "hello world".into();
        let thumbnail_content: Vec<u8> = "hello…".into();

        // Add the media.
        event_cache_store
            .add_media_content(&file_request, content.clone(), IgnoreMediaRetentionPolicy::No)
            .await
            .expect("adding file failed");

        // Since the precision of the timestamp is in seconds, wait so the timestamps
        // differ.
        sleep(Duration::from_secs(3)).await;

        event_cache_store
            .add_media_content(
                &thumbnail_request,
                thumbnail_content.clone(),
                IgnoreMediaRetentionPolicy::No,
            )
            .await
            .expect("adding thumbnail failed");

        // File's last access is older than thumbnail.
        let contents =
            get_event_cache_store_content_sorted_by_last_access(&event_cache_store).await;

        assert_eq!(contents.len(), 2, "media cache contents length is wrong");
        assert_eq!(contents[0], thumbnail_content, "thumbnail is not last access");
        assert_eq!(contents[1], content, "file is not second-to-last access");

        // Since the precision of the timestamp is in seconds, wait so the timestamps
        // differ
        sleep(Duration::from_secs(3)).await;

        // Access the file so its last access is more recent.
        let _ = event_cache_store
            .get_media_content(&file_request)
            .await
            .expect("getting file failed")
            .expect("file is missing");

        // File's last access is more recent than thumbnail.
        let contents =
            get_event_cache_store_content_sorted_by_last_access(&event_cache_store).await;

        assert_eq!(contents.len(), 2, "media cache contents length is wrong");
        assert_eq!(contents[0], content, "file is not last access");
        assert_eq!(contents[1], thumbnail_content, "thumbnail is not second-to-last access");
    }

    #[async_test]
    async fn test_linked_chunk_new_items_chunk() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = &DEFAULT_TEST_ROOM_ID;

        store
            .handle_linked_chunk_updates(
                room_id,
                vec![
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
                ],
            )
            .await
            .unwrap();

        let mut chunks = store.reload_linked_chunk(room_id).await.unwrap();

        assert_eq!(chunks.len(), 3);

        {
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
    }

    #[async_test]
    async fn test_linked_chunk_new_gap_chunk() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = &DEFAULT_TEST_ROOM_ID;

        store
            .handle_linked_chunk_updates(
                room_id,
                vec![Update::NewGapChunk {
                    previous: None,
                    new: ChunkIdentifier::new(42),
                    next: None,
                    gap: Gap { prev_token: "raclette".to_owned() },
                }],
            )
            .await
            .unwrap();

        let mut chunks = store.reload_linked_chunk(room_id).await.unwrap();

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

    #[async_test]
    async fn test_linked_chunk_replace_item() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = &DEFAULT_TEST_ROOM_ID;

        store
            .handle_linked_chunk_updates(
                room_id,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(42), 0),
                        items: vec![
                            make_test_event(room_id, "hello"),
                            make_test_event(room_id, "world"),
                        ],
                    },
                    Update::ReplaceItem {
                        at: Position::new(ChunkIdentifier::new(42), 1),
                        item: make_test_event(room_id, "yolo"),
                    },
                ],
            )
            .await
            .unwrap();

        let mut chunks = store.reload_linked_chunk(room_id).await.unwrap();

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

    #[async_test]
    async fn test_linked_chunk_remove_chunk() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = &DEFAULT_TEST_ROOM_ID;

        store
            .handle_linked_chunk_updates(
                room_id,
                vec![
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
                ],
            )
            .await
            .unwrap();

        let mut chunks = store.reload_linked_chunk(room_id).await.unwrap();

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

        // Check that cascading worked. Yes, sqlite, I doubt you.
        let tx = store
            .inner
            .transaction_on_one_with_mode(keys::LINKED_CHUNKS, IdbTransactionMode::Readonly)
            .unwrap();
        let object_store = tx.object_store(keys::LINKED_CHUNKS).unwrap();

        let gaps = object_store.get_all().unwrap().await.unwrap();
        let mut gap_ids = Vec::new();

        for gap in gaps {
            let gap: ChunkForCache = store.serializer.deserialize_into_object(gap).unwrap();
            let chunk_id = store.get_chunk_id(&Some(gap.id)).unwrap();
            gap_ids.push(chunk_id);
        }

        gap_ids.sort();
        assert_eq!(gap_ids, vec![42, 44]);
    }

    #[async_test]
    async fn test_linked_chunk_push_items() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = &DEFAULT_TEST_ROOM_ID;

        store
            .handle_linked_chunk_updates(
                room_id,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(42), 0),
                        items: vec![
                            make_test_event(room_id, "hello"),
                            make_test_event(room_id, "world"),
                        ],
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(42), 2),
                        items: vec![make_test_event(room_id, "who?")],
                    },
                ],
            )
            .await
            .unwrap();

        let mut chunks = store.reload_linked_chunk(room_id).await.unwrap();

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

    #[async_test]
    async fn test_linked_chunk_remove_item() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = *DEFAULT_TEST_ROOM_ID;

        store
            .handle_linked_chunk_updates(
                room_id,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(42), 0),
                        items: vec![
                            make_test_event(room_id, "hello"),
                            make_test_event(room_id, "world"),
                        ],
                    },
                    Update::RemoveItem { at: Position::new(ChunkIdentifier::new(42), 0) },
                ],
            )
            .await
            .unwrap();

        let mut chunks = store.reload_linked_chunk(room_id).await.unwrap();

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

        let lower = store.get_event_id(room_id.as_ref(), "42", 0).into();
        let upper = &(store.get_event_id_upper_bound(room_id.as_ref(), "42", "\u{FFFF}")).into();

        let key_range = IdbKeyRange::bound(&lower, &upper).unwrap();

        let events = object_store.get_all_with_key(&key_range).unwrap().await.unwrap();
        assert_eq!(events.length(), 1);
    }

    #[async_test]
    async fn test_linked_chunk_detach_last_items() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = *DEFAULT_TEST_ROOM_ID;

        store
            .handle_linked_chunk_updates(
                room_id,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(42), 0),
                        items: vec![
                            make_test_event(room_id, "hello"),
                            make_test_event(room_id, "world"),
                            make_test_event(room_id, "howdy"),
                        ],
                    },
                    Update::DetachLastItems { at: Position::new(ChunkIdentifier::new(42), 1) },
                ],
            )
            .await
            .unwrap();

        let mut chunks = store.reload_linked_chunk(room_id).await.unwrap();

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

    #[async_test]
    async fn test_linked_chunk_start_end_reattach_items() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = *DEFAULT_TEST_ROOM_ID;

        // Same updates and checks as test_linked_chunk_push_items, but with extra
        // `StartReattachItems` and `EndReattachItems` updates, which must have no
        // effects.
        store
            .handle_linked_chunk_updates(
                room_id,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
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
                ],
            )
            .await
            .unwrap();

        let mut chunks = store.reload_linked_chunk(room_id).await.unwrap();

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

    #[async_test]
    async fn test_linked_chunk_clear() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = *DEFAULT_TEST_ROOM_ID;

        store
            .handle_linked_chunk_updates(
                room_id,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
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
                ],
            )
            .await
            .unwrap();

        let chunks = store.reload_linked_chunk(room_id).await.unwrap();
        assert!(chunks.is_empty());
    }

    #[async_test]
    async fn test_linked_chunk_multiple_rooms() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room1 = room_id!("!realcheeselovers:raclette.fr");
        let room2 = room_id!("!realcheeselovers:fondue.ch");

        // Check that applying updates to one room doesn't affect the others.
        // Use the same chunk identifier in both rooms to battle-test search.

        store
            .handle_linked_chunk_updates(
                room1,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(42), 0),
                        items: vec![
                            make_test_event(room1, "best cheese is raclette"),
                            make_test_event(room1, "obviously"),
                        ],
                    },
                ],
            )
            .await
            .unwrap();

        store
            .handle_linked_chunk_updates(
                room2,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(42), 0),
                        items: vec![make_test_event(room1, "beaufort is the best")],
                    },
                ],
            )
            .await
            .unwrap();

        // Check chunks from room 1.
        let mut chunks_room1 = store.reload_linked_chunk(room1).await.unwrap();
        assert_eq!(chunks_room1.len(), 1);

        let c = chunks_room1.remove(0);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 2);
            check_test_event(&events[0], "best cheese is raclette");
            check_test_event(&events[1], "obviously");
        });

        // Check chunks from room 2.
        let mut chunks_room2 = store.reload_linked_chunk(room2).await.unwrap();
        assert_eq!(chunks_room2.len(), 1);

        let c = chunks_room2.remove(0);
        assert_matches!(c.content, ChunkContent::Items(events) => {
            assert_eq!(events.len(), 1);
            check_test_event(&events[0], "beaufort is the best");
        });
    }

    #[async_test]
    async fn test_linked_chunk_update_is_a_transaction() {
        let store = get_event_cache_store().await.expect("creating cache store failed");

        let room_id = *DEFAULT_TEST_ROOM_ID;

        // Trigger a violation of the unique constraint on the (room id, chunk id)
        // couple.
        let _err = store
            .handle_linked_chunk_updates(
                room_id,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                ],
            )
            .await
            .unwrap_err();

        // The operation fails with a constraint violation error.
        // assert_matches!(err, EventCacheStoreError::IndexeddbEventCacheStoreError(_));

        // If the updates have been handled transactionally, then no new chunks should
        // have been added; failure of the second update leads to the first one being
        // rolled back.
        let chunks = store.reload_linked_chunk(room_id).await.unwrap();
        assert!(chunks.is_empty());
    }
}
