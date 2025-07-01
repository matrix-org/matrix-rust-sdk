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

#![allow(unused)]

use indexed_db_futures::IdbDatabase;
use matrix_sdk_base::{
    event_cache::{
        store::{
            media::{IgnoreMediaRetentionPolicy, MediaRetentionPolicy},
            EventCacheStore, MemoryStore,
        },
        Event, Gap,
    },
    linked_chunk::{
        ChunkIdentifier, ChunkIdentifierGenerator, ChunkMetadata, LinkedChunkId, Position,
        RawChunk, Update,
    },
    media::MediaRequestParameters,
};
use ruma::{events::relation::RelationType, EventId, MxcUri, OwnedEventId, RoomId};
use web_sys::IdbTransactionMode;

use crate::event_cache_store::{
    serializer::IndexeddbEventCacheStoreSerializer,
    transaction::IndexeddbEventCacheStoreTransaction,
};

mod builder;
mod error;
mod migrations;
mod serializer;
mod transaction;
mod types;

pub use builder::IndexeddbEventCacheStoreBuilder;
pub use error::IndexeddbEventCacheStoreError;

/// A type for providing an IndexedDB implementation of [`EventCacheStore`][1].
/// This is meant to be used as a backend to [`EventCacheStore`][1] in browser
/// contexts.
///
/// [1]: matrix_sdk_base::event_cache::store::EventCacheStore
#[derive(Debug)]
pub struct IndexeddbEventCacheStore {
    // A handle to the IndexedDB database
    inner: IdbDatabase,
    // A serializer with functionality tailored to `IndexeddbEventCacheStore`
    serializer: IndexeddbEventCacheStoreSerializer,
    // An in-memory store for providing temporary implementations for
    // functions of `EventCacheStore`.
    //
    // NOTE: This will be removed once we have IndexedDB-backed implementations for all
    // functions in `EventCacheStore`.
    memory_store: MemoryStore,
}

impl IndexeddbEventCacheStore {
    /// Provides a type with which to conveniently build an
    /// [`IndexeddbEventCacheStore`]
    pub fn builder() -> IndexeddbEventCacheStoreBuilder {
        IndexeddbEventCacheStoreBuilder::default()
    }
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
        #[async_trait::async_trait(?Send)]
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
    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<bool, IndexeddbEventCacheStoreError> {
        self.memory_store
            .try_take_leased_lock(lease_duration_ms, key, holder)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    async fn handle_linked_chunk_updates(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        updates: Vec<Update<Event, Gap>>,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        self.memory_store
            .handle_linked_chunk_updates(linked_chunk_id, updates)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    async fn load_all_chunks(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Vec<RawChunk<Event, Gap>>, IndexeddbEventCacheStoreError> {
        self.memory_store
            .load_all_chunks(linked_chunk_id)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    async fn load_all_chunks_metadata(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Vec<ChunkMetadata>, IndexeddbEventCacheStoreError> {
        self.memory_store
            .load_all_chunks_metadata(linked_chunk_id)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    async fn load_last_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<
        (Option<RawChunk<Event, Gap>>, ChunkIdentifierGenerator),
        IndexeddbEventCacheStoreError,
    > {
        self.memory_store
            .load_last_chunk(linked_chunk_id)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    async fn load_previous_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        before_chunk_identifier: ChunkIdentifier,
    ) -> Result<Option<RawChunk<Event, Gap>>, IndexeddbEventCacheStoreError> {
        self.memory_store
            .load_previous_chunk(linked_chunk_id, before_chunk_identifier)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    async fn clear_all_linked_chunks(&self) -> Result<(), IndexeddbEventCacheStoreError> {
        self.memory_store
            .clear_all_linked_chunks()
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    async fn filter_duplicated_events(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        events: Vec<OwnedEventId>,
    ) -> Result<Vec<(OwnedEventId, Position)>, IndexeddbEventCacheStoreError> {
        self.memory_store
            .filter_duplicated_events(linked_chunk_id, events)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    async fn find_event(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<Event>, IndexeddbEventCacheStoreError> {
        self.memory_store
            .find_event(room_id, event_id)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    async fn find_event_relations(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
        filters: Option<&[RelationType]>,
    ) -> Result<Vec<(Event, Option<Position>)>, IndexeddbEventCacheStoreError> {
        self.memory_store
            .find_event_relations(room_id, event_id, filters)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    async fn save_event(
        &self,
        room_id: &RoomId,
        event: Event,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        self.memory_store
            .save_event(room_id, event)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    async fn add_media_content(
        &self,
        request: &MediaRequestParameters,
        content: Vec<u8>,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        self.memory_store
            .add_media_content(request, content, ignore_policy)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    async fn replace_media_key(
        &self,
        from: &MediaRequestParameters,
        to: &MediaRequestParameters,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        self.memory_store
            .replace_media_key(from, to)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    async fn get_media_content(
        &self,
        request: &MediaRequestParameters,
    ) -> Result<Option<Vec<u8>>, IndexeddbEventCacheStoreError> {
        self.memory_store
            .get_media_content(request)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    async fn remove_media_content(
        &self,
        request: &MediaRequestParameters,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        self.memory_store
            .remove_media_content(request)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    async fn get_media_content_for_uri(
        &self,
        uri: &MxcUri,
    ) -> Result<Option<Vec<u8>>, IndexeddbEventCacheStoreError> {
        self.memory_store
            .get_media_content_for_uri(uri)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    async fn remove_media_content_for_uri(
        &self,
        uri: &MxcUri,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        self.memory_store
            .remove_media_content_for_uri(uri)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    async fn set_media_retention_policy(
        &self,
        policy: MediaRetentionPolicy,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        self.memory_store
            .set_media_retention_policy(policy)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    fn media_retention_policy(&self) -> MediaRetentionPolicy {
        self.memory_store.media_retention_policy()
    }

    async fn set_ignore_media_retention_policy(
        &self,
        request: &MediaRequestParameters,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        self.memory_store
            .set_ignore_media_retention_policy(request, ignore_policy)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    async fn clean_up_media_cache(&self) -> Result<(), IndexeddbEventCacheStoreError> {
        self.memory_store
            .clean_up_media_cache()
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }
}
