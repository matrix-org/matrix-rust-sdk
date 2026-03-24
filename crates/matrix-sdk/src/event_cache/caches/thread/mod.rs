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

//! Threads-related data structures.

pub mod pagination;
mod state;
mod updates;

use std::{fmt, sync::Arc};

use matrix_sdk_base::event_cache::{Event, store::EventCacheStoreLock};
use ruma::{EventId, OwnedEventId, OwnedRoomId, OwnedUserId};
pub(super) use state::LockedThreadEventCacheState;
use tokio::sync::{
    Notify,
    broadcast::{Receiver, Sender},
};
use tracing::{error, trace};

use self::{pagination::ThreadPagination, updates::ThreadEventCacheUpdateSender};
use super::{
    super::Result, EventsOrigin, TimelineVectorDiffs, room::RoomEventCacheLinkedChunkUpdate,
};
use crate::room::WeakRoom;

/// All the information related to a single thread.
pub(super) struct ThreadEventCache {
    inner: Arc<ThreadEventCacheInner>,
}

/// The (non-cloneable) details of the `RoomEventCache`.
struct ThreadEventCacheInner {
    /// The thread root ID.
    thread_id: OwnedEventId,

    /// The room where this thread belongs to.
    weak_room: WeakRoom,

    /// State for this thread's event cache.
    state: LockedThreadEventCacheState,

    /// A notifier that we received a new pagination token.
    pagination_batch_token_notifier: Notify,

    /// Update sender for this room.
    update_sender: ThreadEventCacheUpdateSender,
}

impl fmt::Debug for ThreadEventCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadEventCache").finish_non_exhaustive()
    }
}

impl ThreadEventCache {
    /// Create a new empty thread event cache.
    pub async fn new(
        room_id: OwnedRoomId,
        thread_id: OwnedEventId,
        own_user_id: OwnedUserId,
        weak_room: WeakRoom,
        store: EventCacheStoreLock,
        linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,
    ) -> Result<Self> {
        let update_sender = ThreadEventCacheUpdateSender::new();

        Ok(Self {
            inner: Arc::new(ThreadEventCacheInner {
                thread_id: thread_id.clone(),
                weak_room,
                state: LockedThreadEventCacheState::new(
                    room_id,
                    thread_id,
                    own_user_id,
                    store,
                    update_sender.clone(),
                    linked_chunk_update_sender,
                )
                .await?,
                pagination_batch_token_notifier: Notify::new(),
                update_sender,
            }),
        })
    }

    /// Subscribe to live events from this thread.
    pub async fn subscribe(&self) -> Result<(Vec<Event>, Receiver<TimelineVectorDiffs>)> {
        let state = self.inner.state.read().await?;

        let events =
            state.thread_linked_chunk().events().map(|(_position, item)| item.clone()).collect();

        let recv = self.inner.update_sender.new_thread_receiver();

        Ok((events, recv))
    }

    /// Return a [`ThreadPagination`] useful for running back-pagination queries
    /// in this thread.
    pub fn pagination(&self) -> ThreadPagination {
        ThreadPagination::new(self.inner.clone())
    }

    /// Clear a thread, after a gappy sync for instance.
    pub async fn clear(&mut self) -> Result<()> {
        let updates_as_vector_diffs = self.inner.state.write().await?.reset().await?;

        if !updates_as_vector_diffs.is_empty() {
            let _ = self.inner.update_sender.send(TimelineVectorDiffs {
                diffs: updates_as_vector_diffs,
                origin: EventsOrigin::Cache,
            });
        }

        Ok(())
    }

    /// Push some live events to this thread, and propagate the updates to
    /// the listeners.
    pub async fn add_live_events(
        &mut self,
        events: Vec<Event>,
        prev_batch_token: &Option<String>,
    ) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        trace!("adding new events");

        let (stored_prev_batch_token, timeline_event_diffs) =
            self.inner.state.write().await?.handle_sync(events, prev_batch_token).await?;

        // Now that all events have been added, we can trigger the
        // `pagination_token_notifier`.
        if stored_prev_batch_token {
            self.inner.pagination_batch_token_notifier.notify_one();
        }

        if !timeline_event_diffs.is_empty() {
            let _ = self.inner.update_sender.send(TimelineVectorDiffs {
                diffs: timeline_event_diffs,
                origin: EventsOrigin::Sync,
            });
        }

        Ok(())
    }

    /// Replaces a single event, be it saved in memory or in the store.
    ///
    /// If it was saved in memory, this will emit a notification to
    /// observers that a single item has been replaced. Otherwise,
    /// such a notification is not emitted, because observers are
    /// unlikely to observe the store updates directly.
    pub(super) async fn replace_event_if_present(
        &mut self,
        event_id: &EventId,
        new_event: Event,
    ) -> Result<()> {
        let mut state = self.inner.state.write().await?;

        if let Err(err) = state.replace_event_if_present(event_id, new_event).await {
            error!(%err, "failed to replace an event");
            return Err(err);
        }

        let timeline_event_diffs = state.thread_linked_chunk_mut().updates_as_vector_diffs();

        if !timeline_event_diffs.is_empty() {
            let _ = self.inner.update_sender.send(TimelineVectorDiffs {
                diffs: timeline_event_diffs,
                origin: EventsOrigin::Sync,
            });
        }

        Ok(())
    }

    /// Returns the latest event ID in this thread, if any.
    pub async fn latest_event_id(&self) -> Result<Option<OwnedEventId>> {
        Ok(self
            .inner
            .state
            .read()
            .await?
            .thread_linked_chunk()
            .revents()
            .next()
            .and_then(|(_position, event)| event.event_id()))
    }
}
