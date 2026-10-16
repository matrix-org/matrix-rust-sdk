// Copyright 2026 The Matrix.org Foundation C.I.C.
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

//! A [`MemoryStore`] whose writes can be blocked or made to fail, for tests.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use matrix_sdk_common::{
    cross_process_lock::CrossProcessLockGeneration,
    linked_chunk::{
        ChunkIdentifier, ChunkIdentifierGenerator, ChunkMetadata, LinkedChunkId, Position,
        RawChunk, Update,
    },
    sleep::sleep,
};
use ruma::{EventId, OwnedEventId, RoomId, events::relation::RelationType};
use tokio::sync::Notify;

use super::{
    super::{Event, Gap, thread::ThreadInfo},
    EventCacheStore, EventCacheStoreError, MemoryStore, Result,
};

/// A [`MemoryStore`] whose linked chunk writes can be blocked.
///
/// This is meant for tests that need to control when, or whether, the event
/// cache persists its data: for instance to check what the rest of the SDK
/// does while the store is slow, or when it returns an error.
///
/// Only [`EventCacheStore::handle_linked_chunk_updates`] is affected. All the
/// other methods are forwarded to the inner [`MemoryStore`] as is.
///
/// Like the SQLite store, a closed store (see [`EventCacheStore::close`])
/// rejects new linked chunk writes until it is reopened, while a write that
/// was already in progress completes.
#[derive(Debug, Clone, Default)]
pub struct ControlledMemoryStore {
    inner: MemoryStore,
    writes_blocked: Arc<AtomicBool>,
    closed: Arc<AtomicBool>,
    write_started: Arc<Notify>,
    write_finished: Arc<Notify>,
}

impl ControlledMemoryStore {
    /// Create a new store, which doesn't block writes.
    pub fn new() -> Self {
        Self::default()
    }

    /// Block linked chunk writes until [`Self::unblock_writes`] is called.
    ///
    /// A blocked write still counts as started for
    /// [`Self::wait_for_write_to_start`].
    pub fn block_writes(&self) {
        self.writes_blocked.store(true, Ordering::SeqCst);
    }

    /// Let the blocked linked chunk writes, and the next ones, go through.
    pub fn unblock_writes(&self) {
        self.writes_blocked.store(false, Ordering::SeqCst);
    }

    /// Wait until a linked chunk write starts.
    ///
    /// Returns at once if a write has started since the last call.
    pub async fn wait_for_write_to_start(&self) {
        self.write_started.notified().await;
    }

    /// Wait until a linked chunk write finishes.
    ///
    /// Returns at once if a write has finished since the last call.
    pub async fn wait_for_write_to_finish(&self) {
        self.write_finished.notified().await;
    }
}

#[cfg_attr(target_family = "wasm", async_trait(?Send))]
#[cfg_attr(not(target_family = "wasm"), async_trait)]
impl EventCacheStore for ControlledMemoryStore {
    type Error = EventCacheStoreError;

    async fn handle_linked_chunk_updates(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        updates: Vec<Update<Event, Gap>>,
    ) -> Result<(), Self::Error> {
        self.write_started.notify_one();

        if self.closed.load(Ordering::SeqCst) {
            self.write_finished.notify_one();

            return Err(EventCacheStoreError::backend(std::io::Error::other(
                "the event cache store is closed",
            )));
        }

        // Polling is good enough for tests, and simpler than a proper wake up.
        while self.writes_blocked.load(Ordering::SeqCst) {
            sleep(Duration::from_millis(10)).await;
        }

        let result = self.inner.handle_linked_chunk_updates(linked_chunk_id, updates).await;

        self.write_finished.notify_one();

        result
    }

    async fn close(&self) -> Result<(), Self::Error> {
        self.closed.store(true, Ordering::SeqCst);
        self.inner.close().await
    }

    async fn reopen(&self) -> Result<(), Self::Error> {
        self.closed.store(false, Ordering::SeqCst);
        self.inner.reopen().await
    }

    async fn try_take_leased_lock(
        &self,
        lease_duration_ms: u32,
        key: &str,
        holder: &str,
    ) -> Result<Option<CrossProcessLockGeneration>, Self::Error> {
        self.inner.try_take_leased_lock(lease_duration_ms, key, holder).await
    }

    async fn load_all_chunks(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Vec<RawChunk<Event, Gap>>, Self::Error> {
        self.inner.load_all_chunks(linked_chunk_id).await
    }

    async fn load_all_chunks_metadata(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Vec<ChunkMetadata>, Self::Error> {
        self.inner.load_all_chunks_metadata(linked_chunk_id).await
    }

    async fn load_last_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<(Option<RawChunk<Event, Gap>>, ChunkIdentifierGenerator), Self::Error> {
        self.inner.load_last_chunk(linked_chunk_id).await
    }

    async fn load_previous_chunk(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        before_chunk_identifier: ChunkIdentifier,
    ) -> Result<Option<RawChunk<Event, Gap>>, Self::Error> {
        self.inner.load_previous_chunk(linked_chunk_id, before_chunk_identifier).await
    }

    async fn load_thread_info(
        &self,
        room_id: &RoomId,
        thread_id: &EventId,
        insert_default_if_missing: bool,
    ) -> Result<Option<ThreadInfo>, Self::Error> {
        self.inner.load_thread_info(room_id, thread_id, insert_default_if_missing).await
    }

    async fn update_thread_info(
        &self,
        room_id: &RoomId,
        thread_id: &EventId,
        thread_info: &ThreadInfo,
    ) -> Result<(), Self::Error> {
        self.inner.update_thread_info(room_id, thread_id, thread_info).await
    }

    async fn clear_all_events(&self, room_id: Option<&RoomId>) -> Result<(), Self::Error> {
        self.inner.clear_all_events(room_id).await
    }

    async fn filter_duplicated_events(
        &self,
        linked_chunk_id: LinkedChunkId<'_>,
        events: Vec<OwnedEventId>,
    ) -> Result<Vec<(OwnedEventId, Position)>, Self::Error> {
        self.inner.filter_duplicated_events(linked_chunk_id, events).await
    }

    async fn find_event(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<Option<Event>, Self::Error> {
        self.inner.find_event(room_id, event_id).await
    }

    async fn find_event_relations(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
        filters: Option<&[RelationType]>,
    ) -> Result<Vec<(Event, Option<Position>)>, Self::Error> {
        self.inner.find_event_relations(room_id, event_id, filters).await
    }

    async fn get_room_events(
        &self,
        room_id: &RoomId,
        event_type: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<Vec<Event>, Self::Error> {
        self.inner.get_room_events(room_id, event_type, session_id).await
    }

    async fn save_event(&self, room_id: &RoomId, event: Event) -> Result<(), Self::Error> {
        self.inner.save_event(room_id, event).await
    }

    async fn optimize(&self) -> Result<(), Self::Error> {
        self.inner.optimize().await
    }

    async fn get_size(&self) -> Result<Option<usize>, Self::Error> {
        self.inner.get_size().await
    }
}
