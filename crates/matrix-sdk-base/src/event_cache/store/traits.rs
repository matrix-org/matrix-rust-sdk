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
        ChunkContent, ChunkIdentifier, ChunkIdentifierGenerator, ChunkMetadata, LinkedChunkId,
        Position, RawChunk, Update,
    },
    storage_usage::StorageUsage,
};
use ruma::{
    EventId, OwnedEventId, OwnedMxcUri, OwnedRoomId, RoomId, events::relation::RelationType,
};

use super::EventCacheStoreError;
use crate::event_cache::{Event, Gap};

/// An opaque, store-specific reference to a stored event (e.g. its hashed
/// id), handed out by
/// [`EventCacheStore::find_event_refs_by_message_types`] and resolved by
/// [`EventCacheStore::load_events_by_refs`].
pub type StoredEventRef = Vec<u8>;

/// The `msgtype`s of the room messages carrying media contents.
pub const MEDIA_MSGTYPES: &[&str] =
    &["m.image", "m.video", "m.audio", "m.file", "dm.filament.gallery"];

/// Collect every `mxc://` string found in a JSON value, recursively.
fn collect_mxc_uris(value: &serde_json::Value, uris: &mut Vec<OwnedMxcUri>) {
    match value {
        serde_json::Value::String(string) => {
            if string.starts_with("mxc://") {
                let uri = OwnedMxcUri::from(string.as_str());
                if uri.is_valid() {
                    uris.push(uri);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_mxc_uris(value, uris);
            }
        }
        serde_json::Value::Object(map) => {
            for value in map.values() {
                collect_mxc_uris(value, uris);
            }
        }
        _ => {}
    }
}

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

    /// Load the IDs of the rooms whose linked chunks were modified under a
    /// cross-process-lock generation strictly greater than `generation`.
    ///
    /// Stores that journal their writes (see
    /// [`handle_linked_chunk_updates`][Self::handle_linked_chunk_updates])
    /// return `Some` with the touched rooms, letting a process recovering
    /// from a dirtied cross-process lock reload only the state those rooms
    /// need, instead of everything. `None` means the store cannot answer
    /// (no journal, or the journal recorded a store-wide operation): callers
    /// must assume everything changed.
    async fn load_rooms_touched_since(
        &self,
        generation: CrossProcessLockGeneration,
    ) -> Result<Option<Vec<OwnedRoomId>>, Self::Error>;

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

    /// Register a new thread.
    ///
    /// It does nothing regarding events or linked chunks: it simply remembers
    /// that a thread has been created. This is important if one wants to list
    /// all threads, or remove specific events or linked chunks.
    ///
    /// If the thread already exists, it returns successfully.
    async fn remember_thread(
        &self,
        room_id: &RoomId,
        thread_id: &EventId,
    ) -> Result<(), Self::Error>;

    /// Clear persisted events for all the rooms if `room_id` is `None`, or a
    /// single room otherwise.
    ///
    /// This will empty and remove all the linked chunks stored previously,
    /// using the above [`Self::handle_linked_chunk_updates`] methods. It
    /// *also* deletes all the events' content.
    ///
    /// ⚠ This is meant only for super specific use cases, where there shouldn't
    /// be any live in-memory linked chunks. In general, prefer using
    /// `EventCache::clear_all_rooms()` from the common SDK crate.
    async fn clear_all_events(&self, room_id: Option<&RoomId>) -> Result<(), Self::Error>;

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

    /// Find several events by ID (the ones found; in no particular order).
    ///
    /// Same contract as [`Self::find_event`]; stores should override it with
    /// a single query.
    async fn find_events(
        &self,
        room_id: &RoomId,
        event_ids: &[OwnedEventId],
    ) -> Result<Vec<Event>, Self::Error> {
        let mut events = Vec::with_capacity(event_ids.len());
        for event_id in event_ids {
            if let Some(event) = self.find_event(room_id, event_id).await? {
                events.push(event);
            }
        }
        Ok(events)
    }

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

    /// Get all the events of this room's linked chunk which are room messages
    /// with one of the given `msgtype`s, along with their position in the
    /// linked chunk.
    ///
    /// Only events stored in the room's linked chunk are returned (not those
    /// saved "out-of-band" with [`Self::save_event`], nor those of thread or
    /// other linked chunks), since the position is what makes them
    /// orderable. Undecrypted events are never returned: their `msgtype`
    /// isn't known.
    ///
    /// The events are returned in no particular order.
    ///
    /// The default implementation walks the whole linked chunk; stores are
    /// expected to override it with an indexed query.
    async fn find_events_by_message_types(
        &self,
        room_id: &RoomId,
        msgtypes: &[&str],
    ) -> Result<Vec<(Event, Position)>, Self::Error> {
        let chunks = self.load_all_chunks(LinkedChunkId::Room(room_id)).await?;

        Ok(chunks
            .into_iter()
            .flat_map(|chunk| {
                let chunk_id = chunk.identifier;
                let events = match chunk.content {
                    ChunkContent::Items(events) => events,
                    ChunkContent::Gap(_) => Vec::new(),
                };
                events
                    .into_iter()
                    .enumerate()
                    .map(move |(index, event)| (event, Position::new(chunk_id, index)))
            })
            .filter(|(event, _)| {
                event.kind.msgtype().is_some_and(|msgtype| msgtypes.contains(&msgtype.as_str()))
            })
            .collect())
    }

    /// Locate the events of this room's linked chunk which are room messages
    /// with one of the given `msgtype`s: their position in the linked chunk,
    /// and a reference to load them later with
    /// [`Self::load_events_by_refs`], without loading (nor decoding) any of
    /// them now. Same scope as [`Self::find_events_by_message_types`].
    ///
    /// The default implementation loads them all; stores are expected to
    /// override it with an index-only query.
    async fn find_event_refs_by_message_types(
        &self,
        room_id: &RoomId,
        msgtypes: &[&str],
    ) -> Result<Vec<(StoredEventRef, Position)>, Self::Error> {
        Ok(self
            .find_events_by_message_types(room_id, msgtypes)
            .await?
            .into_iter()
            .filter_map(|(event, position)| {
                event.event_id().map(|event_id| (event_id.as_bytes().to_vec(), position))
            })
            .collect())
    }

    /// Load the events referenced by [`Self::find_event_refs_by_message_types`]
    /// (the ones still stored; in no particular order).
    async fn load_events_by_refs(
        &self,
        room_id: &RoomId,
        refs: &[StoredEventRef],
    ) -> Result<Vec<(StoredEventRef, Event)>, Self::Error> {
        let mut events = Vec::with_capacity(refs.len());
        for event_ref in refs {
            let Ok(event_id) = str::from_utf8(event_ref).map(EventId::parse) else { continue };
            let Ok(event_id) = event_id else { continue };
            if let Some(event) = self.find_event(room_id, &event_id).await? {
                events.push((event_ref.clone(), event));
            }
        }
        Ok(events)
    }

    /// The `mxc://` URIs referenced by the stored media messages (images,
    /// videos, audios, files, galleries) of each given room, thumbnails
    /// included: the media contents attributable to the rooms. Rooms without
    /// any are left out.
    ///
    /// The default implementation walks the media messages' content for
    /// `mxc://` strings, room by room.
    async fn media_uris_by_room(
        &self,
        room_ids: &[OwnedRoomId],
    ) -> Result<Vec<(OwnedRoomId, Vec<OwnedMxcUri>)>, Self::Error> {
        let mut by_room = Vec::new();
        for room_id in room_ids {
            let events = self.find_events_by_message_types(room_id, MEDIA_MSGTYPES).await?;

            let mut uris = Vec::new();
            for (event, _) in events {
                if let Ok(Some(content)) = event.raw().get_field::<serde_json::Value>("content") {
                    collect_mxc_uris(&content, &mut uris);
                }
            }
            uris.sort_unstable();
            uris.dedup();

            if !uris.is_empty() {
                by_room.push((room_id.clone(), uris));
            }
        }
        Ok(by_room)
    }

    /// The storage used by the events, overall and per given room.
    ///
    /// Stores that can't measure themselves report no usage.
    async fn storage_usage(&self, room_ids: &[OwnedRoomId]) -> Result<StorageUsage, Self::Error> {
        let _ = room_ids;
        Ok(StorageUsage::default())
    }

    /// Load all the gap chunks of the given linked chunk, with their
    /// identifier.
    ///
    /// The gaps are returned in no particular order.
    ///
    /// The default implementation walks the whole linked chunk; stores are
    /// expected to override it with a direct query.
    async fn load_all_gaps(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Vec<(ChunkIdentifier, Gap)>, Self::Error> {
        let chunks = self.load_all_chunks(linked_chunk_id).await?;

        Ok(chunks
            .into_iter()
            .filter_map(|chunk| match chunk.content {
                ChunkContent::Gap(gap) => Some((chunk.identifier, gap)),
                ChunkContent::Items(_) => None,
            })
            .collect())
    }

    /// Save an event, that might or might not be part of an existing linked
    /// chunk.
    ///
    /// If the event has no event id, it will not be saved, and the function
    /// must return an Ok result early.
    ///
    /// If the event was already stored with the same id, it must be replaced,
    /// without causing an error.
    async fn save_event(&self, room_id: &RoomId, event: Event) -> Result<(), Self::Error>;

    /// Save several events out-of-band, see [`Self::save_event`].
    ///
    /// Stores should override it with a single transaction: the redecryptor
    /// replaces events by the hundred.
    async fn save_events(&self, room_id: &RoomId, events: Vec<Event>) -> Result<(), Self::Error> {
        for event in events {
            self.save_event(room_id, event).await?;
        }
        Ok(())
    }

    /// Close the store, releasing all held resources (database connections,
    /// file descriptors, file locks).
    ///
    /// In-flight operations complete before this method returns. After it
    /// returns, operations will fail until [`Self::reopen()`] is called.
    async fn close(&self) -> Result<(), Self::Error>;

    /// Reopen the store after a [`Self::close()`], re-acquiring database
    /// connections.
    async fn reopen(&self) -> Result<(), Self::Error>;

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

    async fn load_rooms_touched_since(
        &self,
        generation: CrossProcessLockGeneration,
    ) -> Result<Option<Vec<OwnedRoomId>>, Self::Error> {
        self.0.load_rooms_touched_since(generation).await.map_err(Into::into)
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

    async fn remember_thread(
        &self,
        room_id: &RoomId,
        thread_id: &EventId,
    ) -> Result<(), Self::Error> {
        self.0.remember_thread(room_id, thread_id).await.map_err(Into::into)
    }

    async fn clear_all_events(&self, room_id: Option<&RoomId>) -> Result<(), Self::Error> {
        self.0.clear_all_events(room_id).await.map_err(Into::into)
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

    async fn find_events(
        &self,
        room_id: &RoomId,
        event_ids: &[OwnedEventId],
    ) -> Result<Vec<Event>, Self::Error> {
        self.0.find_events(room_id, event_ids).await.map_err(Into::into)
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

    async fn find_events_by_message_types(
        &self,
        room_id: &RoomId,
        msgtypes: &[&str],
    ) -> Result<Vec<(Event, Position)>, Self::Error> {
        self.0.find_events_by_message_types(room_id, msgtypes).await.map_err(Into::into)
    }

    async fn find_event_refs_by_message_types(
        &self,
        room_id: &RoomId,
        msgtypes: &[&str],
    ) -> Result<Vec<(StoredEventRef, Position)>, Self::Error> {
        self.0.find_event_refs_by_message_types(room_id, msgtypes).await.map_err(Into::into)
    }

    async fn load_events_by_refs(
        &self,
        room_id: &RoomId,
        refs: &[StoredEventRef],
    ) -> Result<Vec<(StoredEventRef, Event)>, Self::Error> {
        self.0.load_events_by_refs(room_id, refs).await.map_err(Into::into)
    }

    async fn media_uris_by_room(
        &self,
        room_ids: &[OwnedRoomId],
    ) -> Result<Vec<(OwnedRoomId, Vec<OwnedMxcUri>)>, Self::Error> {
        self.0.media_uris_by_room(room_ids).await.map_err(Into::into)
    }

    async fn storage_usage(&self, room_ids: &[OwnedRoomId]) -> Result<StorageUsage, Self::Error> {
        self.0.storage_usage(room_ids).await.map_err(Into::into)
    }

    async fn load_all_gaps(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Vec<(ChunkIdentifier, Gap)>, Self::Error> {
        self.0.load_all_gaps(linked_chunk_id).await.map_err(Into::into)
    }

    async fn save_event(&self, room_id: &RoomId, event: Event) -> Result<(), Self::Error> {
        self.0.save_event(room_id, event).await.map_err(Into::into)
    }

    async fn save_events(&self, room_id: &RoomId, events: Vec<Event>) -> Result<(), Self::Error> {
        self.0.save_events(room_id, events).await.map_err(Into::into)
    }

    async fn close(&self) -> Result<(), Self::Error> {
        self.0.close().await.map_err(Into::into)
    }

    async fn reopen(&self) -> Result<(), Self::Error> {
        self.0.reopen().await.map_err(Into::into)
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
