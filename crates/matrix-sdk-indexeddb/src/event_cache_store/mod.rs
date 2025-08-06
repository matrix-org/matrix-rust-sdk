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
    timer,
};
use ruma::{events::relation::RelationType, EventId, MxcUri, OwnedEventId, RoomId};
use tracing::{error, instrument, trace};
use web_sys::IdbTransactionMode;

use crate::event_cache_store::{
    migrations::current::keys,
    serializer::IndexeddbEventCacheStoreSerializer,
    transaction::{IndexeddbEventCacheStoreTransaction, IndexeddbEventCacheStoreTransactionError},
    types::{ChunkType, InBandEvent, OutOfBandEvent},
};

mod builder;
mod error;
#[cfg(test)]
mod integration_tests;
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

    /// Initializes a new transaction on the underlying IndexedDB database and
    /// returns a handle which can be used to combine database operations
    /// into an atomic unit.
    pub fn transaction<'a>(
        &'a self,
        stores: &[&str],
        mode: IdbTransactionMode,
    ) -> Result<IndexeddbEventCacheStoreTransaction<'a>, IndexeddbEventCacheStoreError> {
        Ok(IndexeddbEventCacheStoreTransaction::new(
            self.inner.transaction_on_multi_with_mode(stores, mode)?,
            &self.serializer,
        ))
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
    #[instrument(skip(self))]
    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<bool, IndexeddbEventCacheStoreError> {
        let _timer = timer!("method");
        self.memory_store
            .try_take_leased_lock(lease_duration_ms, key, holder)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    #[instrument(skip(self, updates))]
    async fn handle_linked_chunk_updates(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        updates: Vec<Update<Event, Gap>>,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        let _timer = timer!("method");

        let linked_chunk_id = linked_chunk_id.to_owned();
        let room_id = linked_chunk_id.room_id();

        let transaction = self.transaction(
            &[keys::LINKED_CHUNKS, keys::GAPS, keys::EVENTS],
            IdbTransactionMode::Readwrite,
        )?;

        for update in updates {
            match update {
                Update::NewItemsChunk { previous, new, next } => {
                    trace!(%room_id, "Inserting new chunk (prev={previous:?}, new={new:?}, next={next:?})");
                    transaction
                        .add_chunk(
                            room_id,
                            &types::Chunk {
                                room_id: room_id.to_owned(),
                                identifier: new.index(),
                                previous: previous.map(|i| i.index()),
                                next: next.map(|i| i.index()),
                                chunk_type: ChunkType::Event,
                            },
                        )
                        .await?;
                }
                Update::NewGapChunk { previous, new, next, gap } => {
                    trace!(%room_id, "Inserting new gap (prev={previous:?}, new={new:?}, next={next:?})");
                    transaction
                        .add_item(
                            room_id,
                            &types::Gap {
                                chunk_identifier: new.index(),
                                prev_token: gap.prev_token,
                            },
                        )
                        .await?;
                    transaction
                        .add_chunk(
                            room_id,
                            &types::Chunk {
                                room_id: room_id.to_owned(),
                                identifier: new.index(),
                                previous: previous.map(|i| i.index()),
                                next: next.map(|i| i.index()),
                                chunk_type: ChunkType::Gap,
                            },
                        )
                        .await?;
                }
                Update::RemoveChunk(chunk_id) => {
                    trace!("Removing chunk {chunk_id:?}");
                    transaction.delete_chunk_by_id(room_id, chunk_id).await?;
                }
                Update::PushItems { at, items } => {
                    let chunk_identifier = at.chunk_identifier().index();

                    trace!(%room_id, "pushing {} items @ {chunk_identifier}", items.len());

                    for (i, item) in items.into_iter().enumerate() {
                        transaction
                            .put_item(
                                room_id,
                                &types::Event::InBand(InBandEvent {
                                    content: item,
                                    position: types::Position {
                                        chunk_identifier,
                                        index: at.index() + i,
                                    },
                                }),
                            )
                            .await?;
                    }
                }
                Update::ReplaceItem { at, item } => {
                    let chunk_id = at.chunk_identifier().index();
                    let index = at.index();

                    trace!(%room_id, "replacing item @ {chunk_id}:{index}");

                    transaction
                        .put_event(
                            room_id,
                            &types::Event::InBand(InBandEvent {
                                content: item,
                                position: at.into(),
                            }),
                        )
                        .await?;
                }
                Update::RemoveItem { at } => {
                    let chunk_id = at.chunk_identifier().index();
                    let index = at.index();

                    trace!(%room_id, "removing item @ {chunk_id}:{index}");

                    transaction.delete_event_by_position(room_id, at.into()).await?;
                }
                Update::DetachLastItems { at } => {
                    let chunk_id = at.chunk_identifier().index();
                    let index = at.index();

                    trace!(%room_id, "detaching last items @ {chunk_id}:{index}");

                    transaction.delete_events_by_chunk_from_index(room_id, at.into()).await?;
                }
                Update::StartReattachItems | Update::EndReattachItems => {
                    // Nothing? See sqlite implementation
                }
                Update::Clear => {
                    trace!(%room_id, "clearing room");
                    transaction.delete_chunks_in_room(room_id).await?;
                    transaction.delete_events_in_room(room_id).await?;
                    transaction.delete_gaps_in_room(room_id).await?;
                }
            }
        }
        transaction.commit().await?;
        Ok(())
    }

    #[instrument(skip(self))]
    async fn load_all_chunks(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Vec<RawChunk<Event, Gap>>, IndexeddbEventCacheStoreError> {
        let _ = timer!("method");

        let linked_chunk_id = linked_chunk_id.to_owned();
        let room_id = linked_chunk_id.room_id();

        let transaction = self.transaction(
            &[keys::LINKED_CHUNKS, keys::GAPS, keys::EVENTS],
            IdbTransactionMode::Readwrite,
        )?;

        let mut raw_chunks = Vec::new();
        let chunks = transaction.get_chunks_in_room(room_id).await?;
        for chunk in chunks {
            if let Some(raw_chunk) = transaction
                .load_chunk_by_id(room_id, ChunkIdentifier::new(chunk.identifier))
                .await?
            {
                raw_chunks.push(raw_chunk);
            }
        }
        Ok(raw_chunks)
    }

    #[instrument(skip(self))]
    async fn load_all_chunks_metadata(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Vec<ChunkMetadata>, IndexeddbEventCacheStoreError> {
        // TODO: This call could possibly take a very long time and the
        // amount of time increases linearly with the number of chunks
        // it needs to load from the database. This will likely require
        // some refactoring to deal with performance issues.
        //
        // For details on the performance penalties associated with this
        // call, see https://github.com/matrix-org/matrix-rust-sdk/pull/5407.
        //
        // For how this was improved in the SQLite implementation, see
        // https://github.com/matrix-org/matrix-rust-sdk/pull/5382.
        let _ = timer!("method");

        let linked_chunk_id = linked_chunk_id.to_owned();
        let room_id = linked_chunk_id.room_id();

        let transaction = self.transaction(
            &[keys::LINKED_CHUNKS, keys::EVENTS, keys::GAPS],
            IdbTransactionMode::Readwrite,
        )?;

        let mut raw_chunks = Vec::new();
        let chunks = transaction.get_chunks_in_room(room_id).await?;
        for chunk in chunks {
            let chunk_id = ChunkIdentifier::new(chunk.identifier);
            let num_items = transaction.get_events_count_by_chunk(room_id, chunk_id).await?;
            raw_chunks.push(ChunkMetadata {
                num_items,
                previous: chunk.previous.map(ChunkIdentifier::new),
                identifier: ChunkIdentifier::new(chunk.identifier),
                next: chunk.next.map(ChunkIdentifier::new),
            });
        }
        Ok(raw_chunks)
    }

    #[instrument(skip(self))]
    async fn load_last_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<
        (Option<RawChunk<Event, Gap>>, ChunkIdentifierGenerator),
        IndexeddbEventCacheStoreError,
    > {
        let _timer = timer!("method");

        let linked_chunk_id = linked_chunk_id.to_owned();
        let room_id = linked_chunk_id.room_id();
        let transaction = self.transaction(
            &[keys::LINKED_CHUNKS, keys::EVENTS, keys::GAPS],
            IdbTransactionMode::Readonly,
        )?;

        if transaction.get_chunks_count_in_room(room_id).await? == 0 {
            return Ok((None, ChunkIdentifierGenerator::new_from_scratch()));
        }
        // Now that we know we have some chunks in the room, we query IndexedDB
        // for the last chunk in the room by getting the chunk which does not
        // have a next chunk.
        match transaction.get_chunk_by_next_chunk_id(room_id, None).await {
            Err(IndexeddbEventCacheStoreTransactionError::ItemIsNotUnique) => {
                // If there are multiple chunks that do not have a next chunk, that
                // means we have more than one last chunk, which means that we have
                // more than one list in the room.
                Err(IndexeddbEventCacheStoreError::ChunksContainDisjointLists)
            }
            Err(e) => {
                // There was some error querying IndexedDB, but it is not necessarily
                // a violation of our data constraints.
                Err(e.into())
            },
            Ok(None) => {
                // If there is no chunk without a next chunk, that means every chunk
                // points to another chunk, which means that we have a cycle in our list.
                Err(IndexeddbEventCacheStoreError::ChunksContainCycle)
            },
            Ok(Some(last_chunk)) => {
                let last_chunk_identifier = ChunkIdentifier::new(last_chunk.identifier);
                let last_raw_chunk = transaction
                    .load_chunk_by_id(room_id, last_chunk_identifier)
                    .await?
                    .ok_or(IndexeddbEventCacheStoreError::UnableToLoadChunk)?;
                let max_chunk_id = transaction
                    .get_max_chunk_by_id(room_id)
                    .await?
                    .map(|chunk| ChunkIdentifier::new(chunk.identifier))
                    .ok_or(IndexeddbEventCacheStoreError::NoMaxChunkId)?;
                let generator =
                    ChunkIdentifierGenerator::new_from_previous_chunk_identifier(max_chunk_id);
                Ok((Some(last_raw_chunk), generator))
            }
        }
    }

    #[instrument(skip(self))]
    async fn load_previous_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        before_chunk_identifier: ChunkIdentifier,
    ) -> Result<Option<RawChunk<Event, Gap>>, IndexeddbEventCacheStoreError> {
        let _timer = timer!("method");

        let linked_chunk_id = linked_chunk_id.to_owned();
        let room_id = linked_chunk_id.room_id();
        let transaction = self.transaction(
            &[keys::LINKED_CHUNKS, keys::EVENTS, keys::GAPS],
            IdbTransactionMode::Readonly,
        )?;
        if let Some(chunk) = transaction.get_chunk_by_id(room_id, before_chunk_identifier).await? {
            if let Some(previous_identifier) = chunk.previous {
                let previous_identifier = ChunkIdentifier::new(previous_identifier);
                return Ok(transaction.load_chunk_by_id(room_id, previous_identifier).await?);
            }
        }
        Ok(None)
    }

    #[instrument(skip(self))]
    async fn clear_all_linked_chunks(&self) -> Result<(), IndexeddbEventCacheStoreError> {
        let _timer = timer!("method");

        let transaction = self.transaction(
            &[keys::LINKED_CHUNKS, keys::EVENTS, keys::GAPS],
            IdbTransactionMode::Readwrite,
        )?;
        transaction.clear::<types::Chunk>().await?;
        transaction.clear::<types::Event>().await?;
        transaction.clear::<types::Gap>().await?;
        transaction.commit().await?;
        Ok(())
    }

    #[instrument(skip(self, events))]
    async fn filter_duplicated_events(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        events: Vec<OwnedEventId>,
    ) -> Result<Vec<(OwnedEventId, Position)>, IndexeddbEventCacheStoreError> {
        let _timer = timer!("method");

        if events.is_empty() {
            return Ok(Vec::new());
        }

        let linked_chunk_id = linked_chunk_id.to_owned();
        let room_id = linked_chunk_id.room_id();
        let transaction =
            self.transaction(&[keys::EVENTS], IdbTransactionMode::Readonly)?;
        let mut duplicated = Vec::new();
        for event_id in events {
            if let Some(types::Event::InBand(event)) =
                transaction.get_event_by_id(room_id, &event_id).await?
            {
                duplicated.push((event_id, event.position.into()));
            }
        }
        Ok(duplicated)
    }

    #[instrument(skip(self, event_id))]
    async fn find_event(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<Event>, IndexeddbEventCacheStoreError> {
        let _timer = timer!("method");

        let transaction =
            self.transaction(&[keys::EVENTS], IdbTransactionMode::Readonly)?;
        transaction
            .get_event_by_id(room_id, event_id)
            .await
            .map(|ok| ok.map(Into::into))
            .map_err(Into::into)
    }

    #[instrument(skip(self, event_id, filters))]
    async fn find_event_relations(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
        filters: Option<&[RelationType]>,
    ) -> Result<Vec<(Event, Option<Position>)>, IndexeddbEventCacheStoreError> {
        let _timer = timer!("method");

        let transaction =
            self.transaction(&[keys::EVENTS], IdbTransactionMode::Readonly)?;

        let mut related_events = Vec::new();
        match filters {
            Some(relation_types) if !relation_types.is_empty() => {
                for relation_type in relation_types {
                    let relation = (event_id, relation_type);
                    let events = transaction.get_events_by_relation(room_id, relation).await?;
                    for event in events {
                        let position = event.position().map(Into::into);
                        related_events.push((event.into(), position));
                    }
                }
            }
            _ => {
                for event in
                    transaction.get_events_by_related_event(room_id, event_id).await?
                {
                    let position = event.position().map(Into::into);
                    related_events.push((event.into(), position));
                }
            }
        }
        Ok(related_events)
    }

    #[instrument(skip(self, event))]
    async fn save_event(
        &self,
        room_id: &RoomId,
        event: Event,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        let _timer = timer!("method");

        let Some(event_id) = event.event_id() else {
            error!(%room_id, "Trying to save an event with no ID");
            return Ok(());
        };
        let transaction =
            self.transaction(&[keys::EVENTS], IdbTransactionMode::Readwrite)?;
        let event = match transaction.get_event_by_id(room_id, &event_id).await? {
            Some(mut inner) => inner.with_content(event),
            None => types::Event::OutOfBand(OutOfBandEvent { content: event, position: () }),
        };
        transaction.put_event(room_id, &event).await?;
        transaction.commit().await?;
        Ok(())
    }

    #[instrument(skip_all)]
    async fn add_media_content(
        &self,
        request: &MediaRequestParameters,
        content: Vec<u8>,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        let _timer = timer!("method");
        self.memory_store
            .add_media_content(request, content, ignore_policy)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    #[instrument(skip_all)]
    async fn replace_media_key(
        &self,
        from: &MediaRequestParameters,
        to: &MediaRequestParameters,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        let _timer = timer!("method");
        self.memory_store
            .replace_media_key(from, to)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    #[instrument(skip_all)]
    async fn get_media_content(
        &self,
        request: &MediaRequestParameters,
    ) -> Result<Option<Vec<u8>>, IndexeddbEventCacheStoreError> {
        let _timer = timer!("method");
        self.memory_store
            .get_media_content(request)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    #[instrument(skip_all)]
    async fn remove_media_content(
        &self,
        request: &MediaRequestParameters,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        let _timer = timer!("method");
        self.memory_store
            .remove_media_content(request)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    #[instrument(skip(self))]
    async fn get_media_content_for_uri(
        &self,
        uri: &MxcUri,
    ) -> Result<Option<Vec<u8>>, IndexeddbEventCacheStoreError> {
        let _timer = timer!("method");
        self.memory_store
            .get_media_content_for_uri(uri)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    #[instrument(skip(self))]
    async fn remove_media_content_for_uri(
        &self,
        uri: &MxcUri,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        let _timer = timer!("method");
        self.memory_store
            .remove_media_content_for_uri(uri)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    #[instrument(skip_all)]
    async fn set_media_retention_policy(
        &self,
        policy: MediaRetentionPolicy,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        let _timer = timer!("method");
        self.memory_store
            .set_media_retention_policy(policy)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    #[instrument(skip_all)]
    fn media_retention_policy(&self) -> MediaRetentionPolicy {
        let _timer = timer!("method");
        self.memory_store.media_retention_policy()
    }

    #[instrument(skip_all)]
    async fn set_ignore_media_retention_policy(
        &self,
        request: &MediaRequestParameters,
        ignore_policy: IgnoreMediaRetentionPolicy,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        let _timer = timer!("method");
        self.memory_store
            .set_ignore_media_retention_policy(request, ignore_policy)
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }

    #[instrument(skip_all)]
    async fn clean_up_media_cache(&self) -> Result<(), IndexeddbEventCacheStoreError> {
        let _timer = timer!("method");
        self.memory_store
            .clean_up_media_cache()
            .await
            .map_err(IndexeddbEventCacheStoreError::MemoryStore)
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_base::event_cache::store::{EventCacheStore, EventCacheStoreError};
    use matrix_sdk_test::async_test;
    use uuid::Uuid;

    use crate::{
        event_cache_store::IndexeddbEventCacheStore, event_cache_store_integration_tests,
        indexeddb_event_cache_store_integration_tests,
    };

    mod unencrypted {
        use super::*;

        wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

        async fn get_event_cache_store() -> Result<IndexeddbEventCacheStore, EventCacheStoreError> {
            let name = format!("test-event-cache-store-{}", Uuid::new_v4().as_hyphenated());
            Ok(IndexeddbEventCacheStore::builder().database_name(name).build().await?)
        }

        #[cfg(target_family = "wasm")]
        event_cache_store_integration_tests!();

        #[cfg(target_family = "wasm")]
        indexeddb_event_cache_store_integration_tests!();
    }

    mod encrypted {
        use super::*;

        wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

        async fn get_event_cache_store() -> Result<IndexeddbEventCacheStore, EventCacheStoreError> {
            let name = format!("test-event-cache-store-{}", Uuid::new_v4().as_hyphenated());
            Ok(IndexeddbEventCacheStore::builder().database_name(name).build().await?)
        }

        #[cfg(target_family = "wasm")]
        event_cache_store_integration_tests!();

        #[cfg(target_family = "wasm")]
        indexeddb_event_cache_store_integration_tests!();
    }
}
