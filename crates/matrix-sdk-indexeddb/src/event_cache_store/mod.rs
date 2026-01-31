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

#![cfg_attr(not(test), allow(unused))]

use std::{rc::Rc, time::Duration};

use indexed_db_futures::{Build, database::Database};
#[cfg(target_family = "wasm")]
use matrix_sdk_base::cross_process_lock::{
    CrossProcessLockGeneration, FIRST_CROSS_PROCESS_LOCK_GENERATION,
};
use matrix_sdk_base::{
    event_cache::{Event, Gap, store::EventCacheStore},
    linked_chunk::{
        ChunkIdentifier, ChunkIdentifierGenerator, ChunkMetadata, LinkedChunkId, Position,
        RawChunk, Update,
    },
    timer,
};
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId, events::relation::RelationType,
};
use tracing::{error, instrument, trace};
use web_sys::IdbTransactionMode;

use crate::{
    event_cache_store::{
        migrations::current::keys,
        transaction::IndexeddbEventCacheStoreTransaction,
        types::{ChunkType, InBandEvent, Lease, OutOfBandEvent},
    },
    serializer::indexed_type::{IndexedTypeSerializer, traits::Indexed},
    transaction::TransactionError,
};

mod builder;
mod error;
#[cfg(all(test, target_family = "wasm"))]
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
#[derive(Debug, Clone)]
pub struct IndexeddbEventCacheStore {
    // A handle to the IndexedDB database
    inner: Rc<Database>,
    // A serializer with functionality tailored to `IndexeddbEventCacheStore`
    serializer: IndexedTypeSerializer,
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
            self.inner
                .transaction(stores)
                .with_mode(mode)
                .build()
                .map_err(TransactionError::from)?,
            &self.serializer,
        ))
    }
}

#[cfg(target_family = "wasm")]
#[async_trait::async_trait(?Send)]
impl EventCacheStore for IndexeddbEventCacheStore {
    type Error = IndexeddbEventCacheStoreError;

    #[instrument(skip(self))]
    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<Option<CrossProcessLockGeneration>, IndexeddbEventCacheStoreError> {
        let transaction =
            self.transaction(&[Lease::OBJECT_STORE], IdbTransactionMode::Readwrite)?;

        let now = Duration::from_millis(MilliSecondsSinceUnixEpoch::now().get().into());
        let expiration = now + Duration::from_millis(lease_duration_ms.into());

        let lease = match transaction.get_lease_by_id(key).await? {
            Some(mut lease) => {
                if lease.holder == holder {
                    // We had the lease before, extend it.
                    lease.expiration = expiration;

                    Some(lease)
                } else {
                    // We didn't have it.
                    if lease.expiration < now {
                        // Steal it!
                        lease.holder = holder.to_owned();
                        lease.expiration = expiration;
                        lease.generation += 1;

                        Some(lease)
                    } else {
                        // We tried our best.
                        None
                    }
                }
            }
            None => {
                let lease = Lease {
                    key: key.to_owned(),
                    holder: holder.to_owned(),
                    expiration,
                    generation: FIRST_CROSS_PROCESS_LOCK_GENERATION,
                };

                Some(lease)
            }
        };

        Ok(if let Some(lease) = lease {
            transaction.put_lease(&lease).await?;
            transaction.commit().await?;

            Some(lease.generation)
        } else {
            None
        })
    }

    #[instrument(skip(self, updates))]
    async fn handle_linked_chunk_updates(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        updates: Vec<Update<Event, Gap>>,
    ) -> Result<(), IndexeddbEventCacheStoreError> {
        let _timer = timer!("method");

        let transaction = self.transaction(
            &[keys::LINKED_CHUNKS, keys::GAPS, keys::EVENTS],
            IdbTransactionMode::Readwrite,
        )?;

        for update in updates {
            match update {
                Update::NewItemsChunk { previous, new, next } => {
                    trace!(%linked_chunk_id, "Inserting new chunk (prev={previous:?}, new={new:?}, next={next:?})");
                    transaction
                        .add_chunk(&types::Chunk {
                            linked_chunk_id: linked_chunk_id.to_owned(),
                            identifier: new.index(),
                            previous: previous.map(|i| i.index()),
                            next: next.map(|i| i.index()),
                            chunk_type: ChunkType::Event,
                        })
                        .await?;
                }
                Update::NewGapChunk { previous, new, next, gap } => {
                    trace!(%linked_chunk_id, "Inserting new gap (prev={previous:?}, new={new:?}, next={next:?})");
                    transaction
                        .add_item(&types::Gap {
                            linked_chunk_id: linked_chunk_id.to_owned(),
                            chunk_identifier: new.index(),
                            prev_token: gap.prev_token,
                        })
                        .await?;
                    transaction
                        .add_chunk(&types::Chunk {
                            linked_chunk_id: linked_chunk_id.to_owned(),
                            identifier: new.index(),
                            previous: previous.map(|i| i.index()),
                            next: next.map(|i| i.index()),
                            chunk_type: ChunkType::Gap,
                        })
                        .await?;
                }
                Update::RemoveChunk(chunk_id) => {
                    trace!(%linked_chunk_id, "Removing chunk {chunk_id:?}");
                    transaction.delete_chunk_by_id(linked_chunk_id, chunk_id).await?;
                }
                Update::PushItems { at, items } => {
                    let chunk_identifier = at.chunk_identifier().index();

                    trace!(%linked_chunk_id, "pushing {} items @ {chunk_identifier}", items.len());

                    for (i, item) in items.into_iter().enumerate() {
                        transaction
                            .put_event(&types::Event::InBand(InBandEvent {
                                linked_chunk_id: linked_chunk_id.to_owned(),
                                content: item,
                                position: types::Position {
                                    chunk_identifier,
                                    index: at.index() + i,
                                },
                            }))
                            .await?;
                    }
                }
                Update::ReplaceItem { at, item } => {
                    let chunk_id = at.chunk_identifier().index();
                    let index = at.index();

                    trace!(%linked_chunk_id, "replacing item @ {chunk_id}:{index}");

                    transaction
                        .put_event(&types::Event::InBand(InBandEvent {
                            linked_chunk_id: linked_chunk_id.to_owned(),
                            content: item,
                            position: at.into(),
                        }))
                        .await?;
                }
                Update::RemoveItem { at } => {
                    let chunk_id = at.chunk_identifier().index();
                    let index = at.index();

                    trace!(%linked_chunk_id, "removing item @ {chunk_id}:{index}");

                    transaction.delete_event_by_position(linked_chunk_id, at.into()).await?;
                }
                Update::DetachLastItems { at } => {
                    let chunk_id = at.chunk_identifier().index();
                    let index = at.index();

                    trace!(%linked_chunk_id, "detaching last items @ {chunk_id}:{index}");

                    transaction
                        .delete_events_by_chunk_from_index(linked_chunk_id, at.into())
                        .await?;
                }
                Update::StartReattachItems | Update::EndReattachItems => {
                    // Nothing? See sqlite implementation
                }
                Update::Clear => {
                    trace!(%linked_chunk_id, "clearing room");
                    transaction.delete_chunks_by_linked_chunk_id(linked_chunk_id).await?;
                    transaction.delete_events_by_linked_chunk_id(linked_chunk_id).await?;
                    transaction.delete_gaps_by_linked_chunk_id(linked_chunk_id).await?;
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

        let transaction = self.transaction(
            &[keys::LINKED_CHUNKS, keys::GAPS, keys::EVENTS],
            IdbTransactionMode::Readwrite,
        )?;

        let mut raw_chunks = Vec::new();
        let chunks = transaction.get_chunks_by_linked_chunk_id(linked_chunk_id).await?;
        for chunk in chunks {
            if let Some(raw_chunk) = transaction
                .load_chunk_by_id(linked_chunk_id, ChunkIdentifier::new(chunk.identifier))
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

        let transaction = self.transaction(
            &[keys::LINKED_CHUNKS, keys::EVENTS, keys::GAPS],
            IdbTransactionMode::Readwrite,
        )?;

        let mut raw_chunks = Vec::new();
        let chunks = transaction.get_chunks_by_linked_chunk_id(linked_chunk_id).await?;
        for chunk in chunks {
            let chunk_id = ChunkIdentifier::new(chunk.identifier);
            let num_items =
                transaction.get_events_count_by_chunk(linked_chunk_id, chunk_id).await?;
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

        let transaction = self.transaction(
            &[keys::LINKED_CHUNKS, keys::EVENTS, keys::GAPS],
            IdbTransactionMode::Readonly,
        )?;

        if transaction.get_chunks_count_by_linked_chunk_id(linked_chunk_id).await? == 0 {
            return Ok((None, ChunkIdentifierGenerator::new_from_scratch()));
        }
        // Now that we know we have some chunks in the room, we query IndexedDB
        // for the last chunk in the room by getting the chunk which does not
        // have a next chunk.
        match transaction.get_chunk_by_next_chunk_id(linked_chunk_id, None).await {
            Err(TransactionError::ItemIsNotUnique) => {
                // If there are multiple chunks that do not have a next chunk, that
                // means we have more than one last chunk, which means that we have
                // more than one list in the room.
                Err(IndexeddbEventCacheStoreError::ChunksContainDisjointLists)
            }
            Err(e) => {
                // There was some error querying IndexedDB, but it is not necessarily
                // a violation of our data constraints.
                Err(e.into())
            }
            Ok(None) => {
                // If there is no chunk without a next chunk, that means every chunk
                // points to another chunk, which means that we have a cycle in our list.
                Err(IndexeddbEventCacheStoreError::ChunksContainCycle)
            }
            Ok(Some(last_chunk)) => {
                let last_chunk_identifier = ChunkIdentifier::new(last_chunk.identifier);
                let last_raw_chunk = transaction
                    .load_chunk_by_id(linked_chunk_id, last_chunk_identifier)
                    .await?
                    .ok_or(IndexeddbEventCacheStoreError::UnableToLoadChunk)?;
                let max_chunk_id = transaction
                    .get_max_chunk_by_id(linked_chunk_id)
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

        let transaction = self.transaction(
            &[keys::LINKED_CHUNKS, keys::EVENTS, keys::GAPS],
            IdbTransactionMode::Readonly,
        )?;
        if let Some(chunk) =
            transaction.get_chunk_by_id(linked_chunk_id, before_chunk_identifier).await?
            && let Some(previous_identifier) = chunk.previous
        {
            let previous_identifier = ChunkIdentifier::new(previous_identifier);
            Ok(transaction.load_chunk_by_id(linked_chunk_id, previous_identifier).await?)
        } else {
            Ok(None)
        }
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

        let transaction = self.transaction(&[keys::EVENTS], IdbTransactionMode::Readonly)?;
        let mut duplicated = Vec::new();
        for event_id in events {
            if let Some(types::Event::InBand(event)) =
                transaction.get_event_by_id(linked_chunk_id, &event_id).await?
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

        let transaction = self.transaction(&[keys::EVENTS], IdbTransactionMode::Readonly)?;
        transaction
            .get_event_by_room(room_id, event_id)
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

        let transaction = self.transaction(&[keys::EVENTS], IdbTransactionMode::Readonly)?;

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
                for event in transaction.get_events_by_related_event(room_id, event_id).await? {
                    let position = event.position().map(Into::into);
                    related_events.push((event.into(), position));
                }
            }
        }
        Ok(related_events)
    }

    #[instrument(skip(self))]
    async fn get_room_events(
        &self,
        room_id: &RoomId,
        event_type: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Vec<Event>, IndexeddbEventCacheStoreError> {
        let _timer = timer!("method");

        // TODO: Make this more efficient so we don't load all events and filter them
        // here. We should instead only load the relevant events.

        let transaction = self.transaction(&[keys::EVENTS], IdbTransactionMode::Readonly)?;
        transaction
            .get_room_events(room_id)
            .await
            .map(|vec| {
                vec.into_iter()
                    .map(Event::from)
                    .filter(|e| {
                        event_type.is_none_or(|event_type| {
                            Some(event_type) == e.kind.event_type().as_deref()
                        })
                    })
                    .filter(|e| session_id.is_none_or(|s| Some(s) == e.kind.session_id()))
                    .collect()
            })
            .map_err(Into::into)
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
        let transaction = self.transaction(&[keys::EVENTS], IdbTransactionMode::Readwrite)?;
        let event = match transaction.get_event_by_room(room_id, &event_id).await? {
            Some(inner) => inner.with_content(event),
            None => types::Event::OutOfBand(OutOfBandEvent {
                linked_chunk_id: LinkedChunkId::Room(room_id).to_owned(),
                content: event,
                position: (),
            }),
        };
        transaction.put_event(&event).await?;
        transaction.commit().await?;
        Ok(())
    }

    async fn optimize(&self) -> Result<(), Self::Error> {
        Ok(())
    }

    async fn get_size(&self) -> Result<Option<usize>, Self::Error> {
        Ok(None)
    }
}

#[cfg(all(test, target_family = "wasm"))]
mod tests {
    use matrix_sdk_base::{
        event_cache::store::EventCacheStoreError, event_cache_store_integration_tests,
        event_cache_store_integration_tests_time,
    };
    use uuid::Uuid;

    use crate::{
        event_cache_store::IndexeddbEventCacheStore, indexeddb_event_cache_store_integration_tests,
    };

    mod unencrypted {
        use super::*;

        wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

        async fn get_event_cache_store() -> Result<IndexeddbEventCacheStore, EventCacheStoreError> {
            let name = format!("test-event-cache-store-{}", Uuid::new_v4().as_hyphenated());
            Ok(IndexeddbEventCacheStore::builder().database_name(name).build().await?)
        }

        event_cache_store_integration_tests!();
        event_cache_store_integration_tests_time!();

        indexeddb_event_cache_store_integration_tests!();
    }

    mod encrypted {
        use std::sync::Arc;

        use matrix_sdk_store_encryption::StoreCipher;

        use super::*;

        wasm_bindgen_test::wasm_bindgen_test_configure!(run_in_browser);

        async fn get_event_cache_store() -> Result<IndexeddbEventCacheStore, EventCacheStoreError> {
            let name = format!("test-event-cache-store-{}", Uuid::new_v4().as_hyphenated());
            Ok(IndexeddbEventCacheStore::builder()
                .database_name(name)
                .store_cipher(Arc::new(StoreCipher::new().expect("store cipher")))
                .build()
                .await?)
        }

        event_cache_store_integration_tests!();
        event_cache_store_integration_tests_time!();

        indexeddb_event_cache_store_integration_tests!();
    }
}
