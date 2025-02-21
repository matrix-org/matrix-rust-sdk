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

use std::future::IntoFuture;

use crate::event_cache_store::indexeddb_serializer::IndexeddbSerializer;
use async_trait::async_trait;
use indexed_db_futures::IdbDatabase;
use indexed_db_futures::IdbQuerySource;
use matrix_sdk_base::deserialized_responses::TimelineEvent;
use matrix_sdk_base::linked_chunk;
use matrix_sdk_base::{
    event_cache::{
        store::{
            media::{
                // EventCacheStoreMedia,
                IgnoreMediaRetentionPolicy,
                MediaRetentionPolicy,
                // MediaService,
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

use ruma::events::policy::rule::room;
use ruma::{
    // time::SystemTime,
    MilliSecondsSinceUnixEpoch,
    MxcUri,
    RoomId,
};

use serde::Deserialize;
use serde::Serialize;
use tracing::trace;
use wasm_bindgen::JsValue;
use web_sys::IdbKeyRange;
use web_sys::IdbTransactionMode;

pub use builder::IndexeddbEventCacheStoreBuilder;
pub use error::IndexeddbEventCacheStoreError;

mod keys {
    pub const CORE: &str = "core";
    pub const EVENTS: &str = "events";
    // Entries in Key-value store
    // pub const MEDIA_RETENTION_POLICY: &str = "media_retention_policy";

    // Tables
    pub const LINKED_CHUNKS: &str = "linked_chunks";
    pub const GAPS: &str = "gaps";
    // pub const MEDIA: &str = "media";
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
    pub(crate) serializer: IndexeddbSerializer,
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
        let id_raw = [chunk_id, KEY_SEPARATOR, &index.to_string()].concat();
        self.serializer.encode_key_as_string(room_id.as_ref(), id_raw)
    }

    pub fn get_chunk_id(&self, id: &Option<String>) -> Option<u64> {
        match id {
            Some(id) => {
                let mut parts = id.splitn(2, KEY_SEPARATOR);
                let room_id = parts.next().unwrap().to_owned();
                let object_id = parts.next().unwrap().parse::<u64>().unwrap();
                Some(object_id)
            }
            None => None,
        }
    }
}

type Result<A, E = IndexeddbEventCacheStoreError> = std::result::Result<A, E>;

#[derive(Debug, Serialize, Deserialize)]
struct Chunk {
    id: String,
    previous: Option<String>,
    next: Option<String>,
    type_str: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct IndexedDbGap {
    prev_token: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct TimelineEventForCache {
    id: String,
    content: TimelineEvent,
    room_id: String,
    position: usize,
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
        for update in updates {
            // web_sys::console::log_1(&format!("🟦 Trying to handle update {:?}", update).into());
            match update {
                Update::NewItemsChunk { previous, new, next } => {
                    let tx = self.inner.transaction_on_one_with_mode(
                        keys::LINKED_CHUNKS,
                        IdbTransactionMode::Readwrite,
                    )?;

                    let object_store = tx.object_store(keys::LINKED_CHUNKS)?;

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

                    let chunk = Chunk {
                        id: id.clone(),
                        previous: previous.clone(),
                        next: next.clone(),
                        type_str: CHUNK_TYPE_EVENT_TYPE_STRING.to_owned(),
                    };

                    let serialized_value = self.serializer.serialize_into_object(&id, &chunk)?;

                    let req = object_store.put_val(&serialized_value)?;

                    req.await?;

                    // Update previous if there
                    if let Some(previous) = previous {
                        let previous_chunk_js_value = object_store
                            .get_owned(&previous)?
                            .await?
                            .expect("Previous chunk not found");

                        let previous_chunk: Chunk =
                            self.serializer.deserialize_into_object(previous_chunk_js_value)?;

                        let updated_previous_chunk = Chunk {
                            id: previous_chunk.id,
                            previous: previous_chunk.previous,
                            next: Some(id.clone()),
                            type_str: previous_chunk.type_str,
                        };

                        let updated_previous_value = self
                            .serializer
                            .serialize_into_object(&previous, &updated_previous_chunk)?;

                        object_store.put_val(&updated_previous_value)?;
                    }

                    // update next if there
                    if let Some(next) = next {
                        let next_chunk_js_value = object_store.get_owned(&next)?.await?.unwrap();
                        let next_chunk: Chunk =
                            self.serializer.deserialize_into_object(next_chunk_js_value)?;

                        let updated_next_chunk = Chunk {
                            id: next_chunk.id,
                            previous: Some(id),
                            next: next_chunk.next,
                            type_str: next_chunk.type_str,
                        };

                        let updated_next_value =
                            self.serializer.serialize_into_object(&next, &updated_next_chunk)?;

                        object_store.put_val(&updated_next_value)?;
                    }
                }
                Update::NewGapChunk { previous, new, next, gap } => {
                    let tx = self.inner.transaction_on_one_with_mode(
                        keys::LINKED_CHUNKS,
                        IdbTransactionMode::Readwrite,
                    )?;

                    let object_store = tx.object_store(keys::LINKED_CHUNKS)?;

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

                    let chunk = Chunk {
                        id: id.clone(),
                        previous: previous.clone(),
                        next: next.clone(),
                        type_str: CHUNK_TYPE_GAP_TYPE_STRING.to_owned(),
                    };

                    let serialized_value = self.serializer.serialize_into_object(&id, &chunk)?;

                    object_store.add_val(&serialized_value)?;

                    if let Some(previous) = previous {
                        let previous_chunk_js_value = object_store
                            .get_owned(&previous)?
                            .await?
                            .expect("Previous chunk not found");

                        let previous_chunk: Chunk =
                            self.serializer.deserialize_into_object(previous_chunk_js_value)?;

                        let updated_previous_chunk = Chunk {
                            id: previous_chunk.id,
                            previous: previous_chunk.previous,
                            next: Some(id.clone()),
                            type_str: previous_chunk.type_str,
                        };

                        let updated_previous_value = self
                            .serializer
                            .serialize_into_object(&previous, &updated_previous_chunk)?;

                        object_store.put_val(&updated_previous_value)?;
                    }

                    // update next if there
                    if let Some(next) = next {
                        let next_chunk_js_value = object_store.get_owned(&next)?.await?.unwrap();
                        let next_chunk: Chunk =
                            self.serializer.deserialize_into_object(next_chunk_js_value)?;

                        let updated_next_chunk = Chunk {
                            id: next_chunk.id,
                            previous: Some(id.clone()),
                            next: next_chunk.next,
                            type_str: next_chunk.type_str,
                        };

                        let updated_next_value =
                            self.serializer.serialize_into_object(&next, &updated_next_chunk)?;

                        object_store.put_val(&updated_next_value)?;
                    }

                    let tx = self
                        .inner
                        .transaction_on_one_with_mode(keys::GAPS, IdbTransactionMode::Readwrite)?;

                    let object_store = tx.object_store(keys::GAPS)?;

                    let gap = IndexedDbGap { prev_token: gap.prev_token };

                    let serialized_gap = self.serializer.serialize_into_object(&id, &gap)?;

                    object_store.add_val(&serialized_gap)?;
                }
                Update::RemoveChunk(id) => {
                    let tx = self.inner.transaction_on_one_with_mode(
                        keys::LINKED_CHUNKS,
                        IdbTransactionMode::Readwrite,
                    )?;

                    let object_store = tx.object_store(keys::LINKED_CHUNKS)?;

                    let id = self.get_id(room_id.as_ref(), id.index().to_string().as_ref());

                    trace!("Removing chunk {id:?}");

                    // Remove the chunk itself
                    let chunk_to_delete_js_value =
                        object_store.get_owned(id.clone())?.await?.unwrap();
                    let chunk_to_delete: Chunk =
                        self.serializer.deserialize_into_object(chunk_to_delete_js_value)?;

                    if let Some(previous) = chunk_to_delete.previous.clone() {
                        let previous_chunk_js_value =
                            object_store.get_owned(&previous)?.await?.unwrap();
                        let previous_chunk: Chunk =
                            self.serializer.deserialize_into_object(previous_chunk_js_value)?;

                        let updated_previous_chunk = Chunk {
                            id: previous.clone(),
                            previous: previous_chunk.previous,
                            next: chunk_to_delete.next.clone(),
                            type_str: previous_chunk.type_str,
                        };
                        let updated_previous_value = self
                            .serializer
                            .serialize_into_object(&previous, &updated_previous_chunk)?;
                        object_store.put_val(&updated_previous_value)?;
                    }

                    if let Some(next) = chunk_to_delete.next {
                        let next_chunk_js_value = object_store.get_owned(&next)?.await?.unwrap();
                        let next_chunk: Chunk =
                            self.serializer.deserialize_into_object(next_chunk_js_value)?;

                        let updated_next_chunk = Chunk {
                            id: next.clone(),
                            previous: chunk_to_delete.previous,
                            next: next_chunk.next,
                            type_str: next_chunk.type_str,
                        };
                        let updated_next_value =
                            self.serializer.serialize_into_object(&next, &updated_next_chunk)?;

                        object_store.put_val(&updated_next_value)?;
                    }

                    object_store.delete_owned(id)?;
                }
                Update::PushItems { at, items } => {
                    let chunk_id = at.chunk_identifier().index();

                    trace!(%room_id, "pushing {} items @ {chunk_id}", items.len());

                    let tx = self.inner.transaction_on_one_with_mode(
                        keys::EVENTS,
                        IdbTransactionMode::Readwrite,
                    )?;

                    let object_store = tx.object_store(keys::EVENTS)?;

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

                        object_store.put_val(&value)?.into_future().await?;
                    }
                }
                Update::ReplaceItem { at, item } => {
                    let chunk_id = at.chunk_identifier().index();
                    let index = at.index();

                    trace!(%room_id, "replacing item @ {chunk_id}:{index}");

                    let tx = self.inner.transaction_on_one_with_mode(
                        keys::EVENTS,
                        IdbTransactionMode::Readwrite,
                    )?;

                    let object_store = tx.object_store(keys::EVENTS)?;

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

                    object_store.put_val(&value)?;
                }
                Update::RemoveItem { at } => {
                    let chunk_id = at.chunk_identifier().index();
                    let index = at.index();

                    trace!(%room_id, "removing item @ {chunk_id}:{index}");

                    let tx = self.inner.transaction_on_one_with_mode(
                        keys::EVENTS,
                        IdbTransactionMode::Readwrite,
                    )?;

                    let object_store = tx.object_store(keys::EVENTS)?;

                    let event_id = format!("{}{}{}", chunk_id, KEY_SEPARATOR, index);
                    let event_id_js_value = JsValue::from_str(&event_id);

                    object_store.delete(&event_id_js_value)?;
                }
                Update::DetachLastItems { at } => {
                    let chunk_id = at.chunk_identifier().index();
                    let index = at.index();

                    trace!(%room_id, "detaching last items @ {chunk_id}:{index}");

                    let tx = self.inner.transaction_on_one_with_mode(
                        keys::EVENTS,
                        IdbTransactionMode::Readwrite,
                    )?;

                    let object_store = tx.object_store(keys::EVENTS)?;

                    let key_range = self
                        .serializer
                        .encode_to_range(keys::EVENTS, format!("{room_id}-{chunk_id}"))?;

                    let items = object_store.get_all_with_key(&key_range)?.await?;

                    for item in items {
                        let event: TimelineEventForCache =
                            self.serializer.deserialize_value(item)?;
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
                    let linked_chunks_key_range =
                        self.serializer.encode_to_range(keys::LINKED_CHUNKS, room_id)?;

                    let tx = self.inner.transaction_on_one_with_mode(
                        keys::LINKED_CHUNKS,
                        IdbTransactionMode::Readwrite,
                    )?;

                    let object_store = tx.object_store(keys::LINKED_CHUNKS)?;

                    let linked_chunks =
                        object_store.get_all_with_key(&linked_chunks_key_range)?.await?;

                    for linked_chunk in linked_chunks {
                        let linked_chunk: Chunk =
                            self.serializer.deserialize_value(linked_chunk)?;
                        // Delete all events for chunk
                        let events_key_range = self.serializer.encode_to_range(
                            keys::EVENTS,
                            format!("{}{}{}", room_id, KEY_SEPARATOR, linked_chunk.id),
                        )?;

                        let events_tx = self.inner.transaction_on_one_with_mode(
                            keys::EVENTS,
                            IdbTransactionMode::Readwrite,
                        )?;

                        let events_object_store = events_tx.object_store(keys::EVENTS)?;

                        let events =
                            events_object_store.get_all_with_key(&events_key_range)?.await?;

                        for event in events {
                            let event: TimelineEventForCache =
                                self.serializer.deserialize_value(event)?;
                            let event_id = JsValue::from_str(&event.id);
                            events_object_store.delete(&event_id)?;
                        }

                        let linked_chunk_id = JsValue::from_str(&linked_chunk.id);
                        object_store.delete(&linked_chunk_id)?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Return all the raw components of a linked chunk, so the caller may
    /// reconstruct the linked chunk later.
    async fn reload_linked_chunk(&self, room_id: &RoomId) -> Result<Vec<RawChunk<Event, Gap>>> {
        let tx = self
            .inner
            .transaction_on_one_with_mode(keys::LINKED_CHUNKS, IdbTransactionMode::Readonly)?;

        let object_store = tx.object_store(keys::LINKED_CHUNKS)?;

        let key_range = IdbKeyRange::lower_bound(&JsValue::from_str(&room_id.as_str())).unwrap();

        let linked_chunks = object_store.get_all_with_key_owned(key_range)?.await?;

        web_sys::console::log_1(&format!("🟦 found chunks: {}", linked_chunks.length()).into());

        let mut raw_chunks = Vec::new();

        for linked_chunk in linked_chunks {
            let linked_chunk: Chunk = self.serializer.deserialize_into_object(linked_chunk)?;
            // TODO unwrap
            let chunk_id = self.get_chunk_id(&Some(linked_chunk.id.clone())).unwrap();
            let previous_chunk_id = self.get_chunk_id(&linked_chunk.previous);
            let next_chunk_id = self.get_chunk_id(&linked_chunk.next);

            if (linked_chunk.type_str == CHUNK_TYPE_GAP_TYPE_STRING) {
                let gaps_tx = self
                    .inner
                    .transaction_on_one_with_mode(keys::GAPS, IdbTransactionMode::Readonly)?;

                let gaps_object_store = gaps_tx.object_store(keys::GAPS)?;

                let gap_id = linked_chunk.id;
                // web_sys::console::log_1(&format!("🟦 Trying to get gap {:?}", gap_id).into());
                let gap_id_js_value = JsValue::from_str(&gap_id);
                let gap_js_value = gaps_object_store.get(&gap_id_js_value)?.await?;
                // web_sys::console::log_1(&format!("🟦 got gap {:?}", gap_js_value).into());
                let gap: IndexedDbGap =
                    self.serializer.deserialize_into_object(gap_js_value.unwrap())?;
                // web_sys::console::log_1(&format!("🟦 deserializing gap {:?}", gap).into());

                let gap = Gap { prev_token: gap.prev_token };

                let raw_chunk = RawChunk {
                    identifier: ChunkIdentifier::new(chunk_id),
                    content: linked_chunk::ChunkContent::Gap(gap),
                    previous: previous_chunk_id.map(ChunkIdentifier::new),
                    next: next_chunk_id.map(ChunkIdentifier::new),
                };

                // web_sys::console::log_1(&format!("🟩 pushing gap chunk {:?}", raw_chunk).into());
                raw_chunks.push(raw_chunk);
            } else {
                let events_tx = self
                    .inner
                    .transaction_on_one_with_mode(keys::EVENTS, IdbTransactionMode::Readonly)?;

                let events_object_store = events_tx.object_store(keys::EVENTS)?;

                let events_key_range =
                    IdbKeyRange::lower_bound(&JsValue::from_str(&chunk_id.to_string())).unwrap();

                let events = events_object_store.get_all_with_key(&events_key_range)?.await?;
                // web_sys::console::log_1(
                //     &format!("🟦 Found events for chunk {:?}", events.length()).into(),
                // );
                let mut events_vec = Vec::new();

                for event in events {
                    // web_sys::console::log_1(&format!("🟦 deserializing {:?}", event).into());
                    let event: TimelineEventForCache =
                        self.serializer.deserialize_into_object(event)?;
                    // web_sys::console::log_1(&format!("🟦 Event for chunk {:?}", event).into());
                    events_vec.push(event.content);
                }

                let raw_chunk = RawChunk {
                    identifier: ChunkIdentifier::new(chunk_id),
                    content: linked_chunk::ChunkContent::Items(events_vec),
                    previous: previous_chunk_id.map(ChunkIdentifier::new),
                    next: next_chunk_id.map(ChunkIdentifier::new),
                };

                // web_sys::console::log_1(&format!("🟩 pushing event chunk {:?}", raw_chunk).into());
                raw_chunks.push(raw_chunk);
            }
        }

        // web_sys::console::log_1(
        //     &format!("🟦 Returning reconstructed chunks {:?}", raw_chunks).into(),
        // );

        Ok(raw_chunks)
    }

    /// Clear persisted events for all the rooms.
    ///
    /// This will empty and remove all the linked chunks stored previously,
    /// using the above [`Self::handle_linked_chunk_updates`] methods.
    async fn clear_all_rooms_chunks(&self) -> Result<()> {
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
        _request: &MediaRequestParameters,
        _content: Vec<u8>,
        _ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<()> {
        Ok(())
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
        _from: &MediaRequestParameters,
        _to: &MediaRequestParameters,
    ) -> Result<()> {
        Ok(())
    }

    /// Get a media file's content out of the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    async fn get_media_content(
        &self,
        _request: &MediaRequestParameters,
    ) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    /// Remove a media file's content from the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    async fn remove_media_content(&self, _request: &MediaRequestParameters) -> Result<()> {
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
    async fn get_media_content_for_uri(&self, _uri: &MxcUri) -> Result<Option<Vec<u8>>> {
        Ok(None)
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
    async fn remove_media_content_for_uri(&self, _uri: &MxcUri) -> Result<()> {
        Ok(())
    }

    fn media_retention_policy(&self) -> MediaRetentionPolicy {
        // TODO on the sqlite version this has a media_service... what is that?
        // It seems there is a Trait EventCacheStoreMedia that might need to be implemented
        MediaRetentionPolicy::default()
    }

    /// Set the `MediaRetentionPolicy` to use for deciding whether to store or
    /// keep media content.
    ///
    /// # Arguments
    ///
    /// * `policy` - The `MediaRetentionPolicy` to use.
    async fn set_media_retention_policy(&self, _policy: MediaRetentionPolicy) -> Result<()> {
        Ok(())
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
        _request: &MediaRequestParameters,
        _ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<()> {
        Ok(())
    }

    /// Clean up the media cache with the current `MediaRetentionPolicy`.
    ///
    /// If there is already an ongoing cleanup, this is a noop.
    async fn clean_up_media_cache(&self) -> Result<()> {
        Ok(())
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
