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

use std::sync::Arc;

use async_trait::async_trait;
pub use builder::IndexeddbEventCacheStoreBuilder;
use gloo_utils::format::JsValueSerdeExt;
use indexed_db_futures::IdbDatabase;
use matrix_sdk_base::{
    deserialized_responses::TimelineEvent,
    event_cache::{
        store::{
            media::{IgnoreMediaRetentionPolicy, MediaRetentionPolicy},
            EventCacheStore, EventCacheStoreError,
        },
        Event, Gap,
    },
    linked_chunk::{ChunkIdentifier, ChunkIdentifierGenerator, Position, RawChunk, Update},
    media::MediaRequestParameters,
};
use matrix_sdk_crypto::CryptoStoreError;
use matrix_sdk_store_encryption::StoreCipher;
use ruma::{events::relation::RelationType, EventId, MxcUri, OwnedEventId, RoomId};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use wasm_bindgen::JsValue;

use crate::serializer::{IndexeddbSerializer, MaybeEncrypted};

mod keys {
    pub const CORE: &str = "core";
    pub const EVENTS: &str = "events";
    pub const LINKED_CHUNKS: &str = "linked_chunks";
    pub const GAPS: &str = "gaps";
}

#[derive(Debug, thiserror::Error)]
pub enum IndexeddbEventCacheStoreError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("DomException {name} ({code}): {message}")]
    DomException { name: String, message: String, code: u16 },
    #[error("unsupported")]
    Unsupported,
    #[error(transparent)]
    CryptoStoreError(#[from] CryptoStoreError),
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
            IndexeddbEventCacheStoreError::Json(e) => EventCacheStoreError::Serialization(e),
            IndexeddbEventCacheStoreError::DomException { .. } => EventCacheStoreError::backend(e),
            IndexeddbEventCacheStoreError::Unsupported => EventCacheStoreError::backend(e),
            IndexeddbEventCacheStoreError::CryptoStoreError(_) => EventCacheStoreError::backend(e),
        }
    }
}

type Result<T, E = IndexeddbEventCacheStoreError> = std::result::Result<T, E>;

pub struct IndexeddbEventCacheStore {
    pub inner: IdbDatabase,
    pub store_cipher: Option<Arc<StoreCipher>>,
    pub serializer: IndexeddbSerializer,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for IndexeddbEventCacheStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexeddbEventCacheStore")
            .field("inner", &self.inner)
            .field("store_cipher", &self.store_cipher.as_ref().map(|_| "<StoreCipher>"))
            .field("serializer", &self.serializer)
            .finish()
    }
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

#[allow(dead_code)]
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

#[allow(dead_code)]
impl ChunkForCache {
    /// Used to set field `type_str` to represent a chunk that contains events
    const CHUNK_TYPE_EVENT_TYPE_STRING: &str = "E";
    /// Used to set field `type_str` to represent a chunk that is a gap
    const CHUNK_TYPE_GAP_TYPE_STRING: &str = "G";
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
struct GapForCache {
    prev_token: String,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
struct TimelineEventForCache {
    id: String,
    content: TimelineEvent,
    room_id: String,
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
        _lease_duration_ms: u32,
        _key: &str,
        _holder: &str,
    ) -> Result<bool, IndexeddbEventCacheStoreError> {
        std::future::ready(Err(IndexeddbEventCacheStoreError::Unsupported)).await
    }

    /// An [`Update`] reflects an operation that has happened inside a linked
    /// chunk. The linked chunk is used by the event cache to store the events
    /// in-memory. This method aims at forwarding this update inside this store.
    async fn handle_linked_chunk_updates(
        &self,
        _room_id: &RoomId,
        _updates: Vec<Update<Event, Gap>>,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        std::future::ready(Err(IndexeddbEventCacheStoreError::Unsupported)).await
    }

    /// Return all the raw components of a linked chunk, so the caller may
    /// reconstruct the linked chunk later.
    #[doc(hidden)]
    async fn load_all_chunks(
        &self,
        _room_id: &RoomId,
    ) -> Result<Vec<RawChunk<Event, Gap>>, IndexeddbEventCacheStoreError> {
        std::future::ready(Err(IndexeddbEventCacheStoreError::Unsupported)).await
    }

    /// Load the last chunk of the `LinkedChunk` holding all events of the room
    /// identified by `room_id`.
    ///
    /// This is used to iteratively load events for the `EventCache`.
    async fn load_last_chunk(
        &self,
        _room_id: &RoomId,
    ) -> Result<
        (Option<RawChunk<Event, Gap>>, ChunkIdentifierGenerator),
        IndexeddbEventCacheStoreError,
    > {
        std::future::ready(Err(IndexeddbEventCacheStoreError::Unsupported)).await
    }

    /// Load the chunk before the chunk identified by `before_chunk_identifier`
    /// of the `LinkedChunk` holding all events of the room identified by
    /// `room_id`
    ///
    /// This is used to iteratively load events for the `EventCache`.
    async fn load_previous_chunk(
        &self,
        _room_id: &RoomId,
        _before_chunk_identifier: ChunkIdentifier,
    ) -> Result<Option<RawChunk<Event, Gap>>, IndexeddbEventCacheStoreError> {
        std::future::ready(Err(IndexeddbEventCacheStoreError::Unsupported)).await
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
        std::future::ready(Err(IndexeddbEventCacheStoreError::Unsupported)).await
    }

    /// Given a set of event IDs, return the duplicated events along with their
    /// position if there are any.
    async fn filter_duplicated_events(
        &self,
        _room_id: &RoomId,
        _events: Vec<OwnedEventId>,
    ) -> Result<Vec<(OwnedEventId, Position)>, IndexeddbEventCacheStoreError> {
        std::future::ready(Err(IndexeddbEventCacheStoreError::Unsupported)).await
    }

    /// Find an event by its ID.
    async fn find_event(
        &self,
        _room_id: &RoomId,
        _event_id: &EventId,
    ) -> Result<Option<Event>, IndexeddbEventCacheStoreError> {
        std::future::ready(Err(IndexeddbEventCacheStoreError::Unsupported)).await
    }

    /// Find all the events that relate to a given event.
    ///
    /// An additional filter can be provided to only retrieve related events for
    /// a certain relationship.
    async fn find_event_relations(
        &self,
        _room_id: &RoomId,
        _event_id: &EventId,
        _filter: Option<&[RelationType]>,
    ) -> Result<Vec<Event>, IndexeddbEventCacheStoreError> {
        std::future::ready(Err(IndexeddbEventCacheStoreError::Unsupported)).await
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
        _request: &MediaRequestParameters,
        _content: Vec<u8>,
        _ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        std::future::ready(Err(IndexeddbEventCacheStoreError::Unsupported)).await
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
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        std::future::ready(Err(IndexeddbEventCacheStoreError::Unsupported)).await
    }

    /// Get a media file's content out of the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    async fn get_media_content(
        &self,
        _request: &MediaRequestParameters,
    ) -> Result<Option<Vec<u8>>, IndexeddbEventCacheStoreError> {
        std::future::ready(Err(IndexeddbEventCacheStoreError::Unsupported)).await
    }

    /// Remove a media file's content from the media store.
    ///
    /// # Arguments
    ///
    /// * `request` - The `MediaRequest` of the file.
    async fn remove_media_content(
        &self,
        _request: &MediaRequestParameters,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        std::future::ready(Err(IndexeddbEventCacheStoreError::Unsupported)).await
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
        _uri: &MxcUri,
    ) -> Result<Option<Vec<u8>>, IndexeddbEventCacheStoreError> {
        std::future::ready(Err(IndexeddbEventCacheStoreError::Unsupported)).await
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
        _uri: &MxcUri,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        std::future::ready(Err(IndexeddbEventCacheStoreError::Unsupported)).await
    }

    /// Set the `MediaRetentionPolicy` to use for deciding whether to store or
    /// keep media content.
    ///
    /// # Arguments
    ///
    /// * `policy` - The `MediaRetentionPolicy` to use.
    async fn set_media_retention_policy(
        &self,
        _policy: MediaRetentionPolicy,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        std::future::ready(Err(IndexeddbEventCacheStoreError::Unsupported)).await
    }

    /// Get the current `MediaRetentionPolicy`.
    fn media_retention_policy(&self) -> MediaRetentionPolicy {
        MediaRetentionPolicy::empty()
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
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        std::future::ready(Err(IndexeddbEventCacheStoreError::Unsupported)).await
    }

    /// Clean up the media cache with the current `MediaRetentionPolicy`.
    ///
    /// If there is already an ongoing cleanup, this is a noop.
    async fn clean_up_media_cache(&self) -> Result<(), IndexeddbEventCacheStoreError> {
        std::future::ready(Err(IndexeddbEventCacheStoreError::Unsupported)).await
    }
}
