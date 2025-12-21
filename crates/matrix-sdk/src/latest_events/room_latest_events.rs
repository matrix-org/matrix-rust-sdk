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
// limitations under the License.

use std::{collections::HashMap, sync::Arc};

use async_once_cell::OnceCell;
use ruma::{EventId, OwnedEventId};
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
use tracing::error;

use super::{
    LatestEvent,
    latest_event::{IsLatestEventValueNone, With},
};
use crate::{
    event_cache::{EventCache, EventCacheError, RoomEventCache},
    room::WeakRoom,
    send_queue::RoomSendQueueUpdate,
};

/// Type holding the [`LatestEvent`] for a room and for all its threads.
#[derive(Debug)]
pub(super) struct RoomLatestEvents {
    /// The state of this type.
    state: Arc<RwLock<RoomLatestEventsState>>,
}

impl RoomLatestEvents {
    /// Create a new [`RoomLatestEvents`].
    pub fn new(
        weak_room: WeakRoom,
        event_cache: &EventCache,
    ) -> With<Self, IsLatestEventValueNone> {
        let latest_event_with = Self::create_latest_event(&weak_room, None);

        With::map(latest_event_with, |for_the_room| Self {
            state: Arc::new(RwLock::new(RoomLatestEventsState {
                for_the_room,
                per_thread: HashMap::new(),
                weak_room,
                event_cache: event_cache.clone(),
                room_event_cache: OnceCell::new(),
            })),
        })
    }

    fn create_latest_event(
        weak_room: &WeakRoom,
        thread_id: Option<&EventId>,
    ) -> With<LatestEvent, IsLatestEventValueNone> {
        LatestEvent::new(weak_room, thread_id)
    }

    /// Lock this type with shared read access, and return an owned lock guard.
    pub async fn read(&self) -> RoomLatestEventsReadGuard {
        RoomLatestEventsReadGuard { inner: self.state.clone().read_owned().await }
    }

    /// Lock this type with exclusive write access, and return an owned lock
    /// guard.
    pub async fn write(&self) -> RoomLatestEventsWriteGuard {
        RoomLatestEventsWriteGuard { inner: self.state.clone().write_owned().await }
    }
}

/// The state of [`RoomLatestEvents`].
#[derive(Debug)]
struct RoomLatestEventsState {
    /// The latest event of the room.
    for_the_room: LatestEvent,

    /// The latest events for each thread.
    per_thread: HashMap<OwnedEventId, LatestEvent>,

    /// The event cache.
    event_cache: EventCache,

    /// The room event cache (lazily-loaded).
    room_event_cache: OnceCell<RoomEventCache>,

    /// The (weak) room.
    ///
    /// It used to to get the power-levels of the user for this room when
    /// computing the latest events.
    weak_room: WeakRoom,
}

/// The owned lock guard returned by [`RoomLatestEvents::read`].
pub(super) struct RoomLatestEventsReadGuard {
    inner: OwnedRwLockReadGuard<RoomLatestEventsState>,
}

impl RoomLatestEventsReadGuard {
    /// Get the [`LatestEvent`] for the room.
    pub fn for_room(&self) -> &LatestEvent {
        &self.inner.for_the_room
    }

    /// Get the [`LatestEvent`] for the thread if it exists.
    pub fn for_thread(&self, thread_id: &EventId) -> Option<&LatestEvent> {
        self.inner.per_thread.get(thread_id)
    }

    #[cfg(test)]
    pub fn per_thread(&self) -> &HashMap<OwnedEventId, LatestEvent> {
        &self.inner.per_thread
    }
}

/// The owned lock guard returned by [`RoomLatestEvents::write`].
pub(super) struct RoomLatestEventsWriteGuard {
    inner: OwnedRwLockWriteGuard<RoomLatestEventsState>,
}

impl RoomLatestEventsWriteGuard {
    /// Check whether this [`RoomLatestEvents`] has a latest event for a
    /// particular thread.
    pub fn has_thread(&self, thread_id: &EventId) -> bool {
        self.inner.per_thread.contains_key(thread_id)
    }

    /// Create the [`LatestEvent`] for thread `thread_id` and insert it in this
    /// [`RoomLatestEvents`].
    pub fn create_and_insert_latest_event_for_thread(&mut self, thread_id: &EventId) {
        let latest_event_with =
            RoomLatestEvents::create_latest_event(&self.inner.weak_room, Some(thread_id));

        self.inner.per_thread.insert(thread_id.to_owned(), With::inner(latest_event_with));
    }

    /// Forget the thread `thread_id`.
    pub fn forget_thread(&mut self, thread_id: &EventId) {
        self.inner.per_thread.remove(thread_id);
    }

    /// Update the latest events for the room and its threads, based on the
    /// event cache data.
    pub async fn update_with_event_cache(&mut self) {
        // Get the power levels of the user for the current room if the `WeakRoom` is
        // still valid.
        //
        // Get it once for all the updates of all the latest events for this room (be
        // the room and its threads).
        let Some(room) = self.inner.weak_room.get() else {
            // No room? Let's stop the update.
            error!(room = ?self.inner.weak_room, "Room is unknown");

            return;
        };
        let own_user_id = room.own_user_id();
        let power_levels = room.power_levels().await.ok();

        let inner = &mut *self.inner;
        let for_the_room = &mut inner.for_the_room;
        let per_thread = &mut inner.per_thread;

        // Lazy-load the `RoomEventCache`.
        let room_event_cache = match inner
            .room_event_cache
            .get_or_try_init(async {
                // It's fine to drop the `EventCacheDropHandles` here as the caller
                // (`LatestEventState`) owns a clone of the `EventCache`.
                let (room_event_cache, _drop_handles) =
                    inner.event_cache.for_room(room.room_id()).await?;

                Ok::<RoomEventCache, EventCacheError>(room_event_cache)
            })
            .await
        {
            Ok(room_event_cache) => room_event_cache,
            Err(err) => {
                error!(room_id = ?room.room_id(), ?err, "Failed to fetch the `RoomEventCache`");
                return;
            }
        };

        for_the_room
            .update_with_event_cache(room_event_cache, own_user_id, power_levels.as_ref())
            .await;

        for latest_event in per_thread.values_mut() {
            latest_event
                .update_with_event_cache(room_event_cache, own_user_id, power_levels.as_ref())
                .await;
        }
    }

    /// Update the latest events for the room and its threads, based on the
    /// send queue update.
    pub async fn update_with_send_queue(&mut self, send_queue_update: &RoomSendQueueUpdate) {
        // Get the power levels of the user for the current room if the `WeakRoom` is
        // still valid.
        //
        // Get it once for all the updates of all the latest events for this room (be
        // the room and its threads).
        let Some(room) = self.inner.weak_room.get() else {
            // No room? Let's stop the update.
            return;
        };
        let own_user_id = room.own_user_id();
        let power_levels = room.power_levels().await.ok();

        let inner = &mut *self.inner;
        let for_the_room = &mut inner.for_the_room;
        let per_thread = &mut inner.per_thread;

        // Lazy-load the `RoomEventCache`.
        let room_event_cache = match inner
            .room_event_cache
            .get_or_try_init(async {
                // It's fine to drop the `EventCacheDropHandles` here as the caller
                // (`LatestEventState`) owns a clone of the `EventCache`.
                let (room_event_cache, _drop_handles) =
                    inner.event_cache.for_room(room.room_id()).await?;

                Ok::<RoomEventCache, EventCacheError>(room_event_cache)
            })
            .await
        {
            Ok(room_event_cache) => room_event_cache,
            Err(err) => {
                error!(room_id = ?room.room_id(), ?err, "Failed to fetch the `RoomEventCache`");
                return;
            }
        };

        for_the_room
            .update_with_send_queue(
                send_queue_update,
                room_event_cache,
                own_user_id,
                power_levels.as_ref(),
            )
            .await;

        for latest_event in per_thread.values_mut() {
            latest_event
                .update_with_send_queue(
                    send_queue_update,
                    room_event_cache,
                    own_user_id,
                    power_levels.as_ref(),
                )
                .await;
        }
    }
}
