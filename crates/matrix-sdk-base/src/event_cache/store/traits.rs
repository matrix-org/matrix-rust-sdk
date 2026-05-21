// Copyright 2024 The Matrix.org Foundation C.I.C.
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

use std::{fmt, sync::Arc};

use async_trait::async_trait;
use matrix_sdk_common::{
    AsyncTraitDeps,
    cross_process_lock::CrossProcessLockGeneration,
    linked_chunk::{
        ChunkIdentifier, ChunkIdentifierGenerator, ChunkMetadata, LinkedChunkId, Position,
        RawChunk, Update,
    },
};
use ruma::{EventId, OwnedEventId, RoomId, events::relation::RelationType};

use super::EventCacheStoreError;
use crate::event_cache::{Event, Gap};

/// A default capacity for linked chunks, when manipulating in conjunction with
/// an `EventCacheStore` implementation.
// TODO: move back?
pub const DEFAULT_CHUNK_CAPACITY: usize = 128;

/// An abstract trait that can be used to implement different store backends
/// for the event cache of the SDK.
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
pub trait EventCacheStore: AsyncTraitDeps {
    /// The error type used by this event cache store.
    type Error: fmt::Debug + Into<EventCacheStoreError>;

    /// Try to take a lock using the given store.
    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<Option<CrossProcessLockGeneration>, Self::Error>;

    /// An [`Update`] reflects an operation that has happened inside a linked
    /// chunk. The linked chunk is used by the event cache to store the events
    /// in-memory. This method aims at forwarding this update inside this store.
    async fn handle_linked_chunk_updates(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        updates: Vec<Update<Event, Gap>>,
    ) -> Result<(), Self::Error>;

    /// Remove all data tied to a given room from the cache.
    async fn remove_room(&self, room_id: &RoomId) -> Result<(), Self::Error> {
        // Right now, this means removing all the linked chunk. If implementations
        // override this behavior, they should *also* include this code.
        self.handle_linked_chunk_updates(LinkedChunkId::Room(room_id), vec![Update::Clear]).await
    }

    /// Return all the raw components of a linked chunk, so the caller may
    /// reconstruct the linked chunk later.
    #[doc(hidden)]
    async fn load_all_chunks(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Vec<RawChunk<Event, Gap>>, Self::Error>;

    /// Load all of the chunks' metadata for the given [`LinkedChunkId`].
    ///
    /// Chunks are unordered, and there's no guarantee that the chunks would
    /// form a valid linked chunk after reconstruction.
    async fn load_all_chunks_metadata(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Vec<ChunkMetadata>, Self::Error>;

    /// Load the last chunk of the `LinkedChunk` holding all events of the room
    /// identified by `room_id`.
    ///
    /// This is used to iteratively load events for the `EventCache`.
    async fn load_last_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<(Option<RawChunk<Event, Gap>>, ChunkIdentifierGenerator), Self::Error>;

    /// Load the chunk before the chunk identified by `before_chunk_identifier`
    /// of the `LinkedChunk` holding all events of the room identified by
    /// `room_id`
    ///
    /// This is used to iteratively load events for the `EventCache`.
    async fn load_previous_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        before_chunk_identifier: ChunkIdentifier,
    ) -> Result<Option<RawChunk<Event, Gap>>, Self::Error>;

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
    async fn clear_all_linked_chunks(&self) -> Result<(), Self::Error>;

    /// Given a set of event IDs, return the duplicated events along with their
    /// position if there are any.
    async fn filter_duplicated_events(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        events: Vec<OwnedEventId>,
    ) -> Result<Vec<(OwnedEventId, Position)>, Self::Error>;

    /// Find an event by its ID in a room.
    ///
    /// This method must return events saved either in any linked chunks, *or*
    /// events saved "out-of-band" with the [`Self::save_event`] method.
    async fn find_event(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<Event>, Self::Error>;

    /// Find all the events (alongside their position in the room's linked
    /// chunk, if available) that relate to a given event.
    ///
    /// The only events which don't have a position are those which have been
    /// saved out-of-band using [`Self::save_event`].
    ///
    /// Note: it doesn't process relations recursively: for instance, if
    /// requesting only thread events, it will NOT return the aggregated
    /// events affecting the returned events. It is the responsibility of
    /// the caller to do so, if needed.
    ///
    /// An additional filter can be provided to only retrieve related events for
    /// a certain relationship.
    ///
    /// This method must return events saved either in any linked chunks, *or*
    /// events saved "out-of-band" with the [`Self::save_event`] method.
    async fn find_event_relations(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
        filter: Option<&[RelationType]>,
    ) -> Result<Vec<(Event, Option<Position>)>, Self::Error>;

    /// Get all events in this room.
    ///
    /// This method must return events saved either in any linked chunks, *or*
    /// events saved "out-of-band" with the [`Self::save_event`] method.
    async fn get_room_events(
        &self,
        room_id: &RoomId,
        event_type: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Vec<Event>, Self::Error>;

    /// Save an event, that might or might not be part of an existing linked
    /// chunk.
    ///
    /// If the event has no event id, it will not be saved, and the function
    /// must return an Ok result early.
    ///
    /// If the event was already stored with the same id, it must be replaced,
    /// without causing an error.
    async fn save_event(&self, room_id: &RoomId, event: Event) -> Result<(), Self::Error>;

    /// Perform database optimizations if any are available, i.e. vacuuming in
    /// SQLite.
    ///
    /// **Warning:** this was added to check if SQLite fragmentation was the
    /// source of performance issues, **DO NOT use in production**.
    #[doc(hidden)]
    async fn optimize(&self) -> Result<(), Self::Error>;

    /// Returns the size of the store in bytes, if known.
    async fn get_size(&self) -> Result<Option<usize>, Self::Error>;
}

#[repr(transparent)]
struct EraseEventCacheStoreError<T>(T);

#[cfg(not(tarpaulin_include))]
impl<T: fmt::Debug> fmt::Debug for EraseEventCacheStoreError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl<T: EventCacheStore> EventCacheStore for EraseEventCacheStoreError<T> {
    type Error = EventCacheStoreError;

    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<Option<CrossProcessLockGeneration>, Self::Error> {
        self.0.try_take_leased_lock(lease_duration_ms, key, holder).await.map_err(Into::into)
    }

    async fn handle_linked_chunk_updates(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        updates: Vec<Update<Event, Gap>>,
    ) -> Result<(), Self::Error> {
        self.0.handle_linked_chunk_updates(linked_chunk_id, updates).await.map_err(Into::into)
    }

    async fn load_all_chunks(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Vec<RawChunk<Event, Gap>>, Self::Error> {
        self.0.load_all_chunks(linked_chunk_id).await.map_err(Into::into)
    }

    async fn load_all_chunks_metadata(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Vec<ChunkMetadata>, Self::Error> {
        self.0.load_all_chunks_metadata(linked_chunk_id).await.map_err(Into::into)
    }

    async fn load_last_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<(Option<RawChunk<Event, Gap>>, ChunkIdentifierGenerator), Self::Error> {
        self.0.load_last_chunk(linked_chunk_id).await.map_err(Into::into)
    }

    async fn load_previous_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        before_chunk_identifier: ChunkIdentifier,
    ) -> Result<Option<RawChunk<Event, Gap>>, Self::Error> {
        self.0
            .load_previous_chunk(linked_chunk_id, before_chunk_identifier)
            .await
            .map_err(Into::into)
    }

    async fn clear_all_linked_chunks(&self) -> Result<(), Self::Error> {
        self.0.clear_all_linked_chunks().await.map_err(Into::into)
    }

    async fn filter_duplicated_events(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        events: Vec<OwnedEventId>,
    ) -> Result<Vec<(OwnedEventId, Position)>, Self::Error> {
        self.0.filter_duplicated_events(linked_chunk_id, events).await.map_err(Into::into)
    }

    async fn find_event(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<Event>, Self::Error> {
        self.0.find_event(room_id, event_id).await.map_err(Into::into)
    }

    async fn find_event_relations(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
        filter: Option<&[RelationType]>,
    ) -> Result<Vec<(Event, Option<Position>)>, Self::Error> {
        self.0.find_event_relations(room_id, event_id, filter).await.map_err(Into::into)
    }

    async fn get_room_events(
        &self,
        room_id: &RoomId,
        event_type: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Vec<Event>, Self::Error> {
        self.0.get_room_events(room_id, event_type, session_id).await.map_err(Into::into)
    }

    async fn save_event(&self, room_id: &RoomId, event: Event) -> Result<(), Self::Error> {
        self.0.save_event(room_id, event).await.map_err(Into::into)
    }

    async fn optimize(&self) -> Result<(), Self::Error> {
        self.0.optimize().await.map_err(Into::into)?;
        Ok(())
    }

    async fn get_size(&self) -> Result<Option<usize>, Self::Error> {
        Ok(self.0.get_size().await.map_err(Into::into)?)
    }
}

/// A type-erased [`EventCacheStore`].
pub type DynEventCacheStore = dyn EventCacheStore<Error = EventCacheStoreError>;

/// A type that can be type-erased into `Arc<dyn EventCacheStore>`.
///
/// This trait is not meant to be implemented directly outside
/// `matrix-sdk-base`, but it is automatically implemented for everything that
/// implements `EventCacheStore`.
pub trait IntoEventCacheStore {
    #[doc(hidden)]
    fn into_event_cache_store(self) -> Arc<DynEventCacheStore>;
}

impl IntoEventCacheStore for Arc<DynEventCacheStore> {
    fn into_event_cache_store(self) -> Arc<DynEventCacheStore> {
        self
    }
}

impl<T> IntoEventCacheStore for T
where
    T: EventCacheStore + Sized + 'static,
{
    fn into_event_cache_store(self) -> Arc<DynEventCacheStore> {
        Arc::new(EraseEventCacheStoreError(self))
    }
}

// Turns a given `Arc<T>` into `Arc<DynEventCacheStore>` by attaching the
// `EventCacheStore` impl vtable of `EraseEventCacheStoreError<T>`.
impl<T> IntoEventCacheStore for Arc<T>
where
    T: EventCacheStore + 'static,
{
    fn into_event_cache_store(self) -> Arc<DynEventCacheStore> {
        let ptr: *const T = Arc::into_raw(self);
        let ptr_erased = ptr as *const EraseEventCacheStoreError<T>;
        // SAFETY: EraseEventCacheStoreError is repr(transparent) so T and
        //         EraseEventCacheStoreError<T> have the same layout and ABI
        unsafe { Arc::from_raw(ptr_erased) }
    }
}
