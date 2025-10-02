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

use ruma::{EventId, OwnedEventId};
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

use super::{LatestEvent, LatestEventsError};
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
    pub async fn new(
        weak_room: WeakRoom,
        event_cache: &EventCache,
    ) -> Result<Option<Self>, LatestEventsError> {
        let room_id = weak_room.room_id();
        let room_event_cache = match event_cache.for_room(room_id).await {
            // It's fine to drop the `EventCacheDropHandles` here as the caller
            // (`LatestEventState`) owns a clone of the `EventCache`.
            Ok((room_event_cache, _drop_handles)) => room_event_cache,
            Err(EventCacheError::RoomNotFound { .. }) => return Ok(None),
            Err(err) => return Err(LatestEventsError::EventCache(err)),
        };

        Ok(Some(Self {
            state: Arc::new(RwLock::new(RoomLatestEventsState {
                for_the_room: Self::create_latest_event_for_inner(
                    &weak_room,
                    None,
                    &room_event_cache,
                )
                .await,
                per_thread: HashMap::new(),
                weak_room,
                room_event_cache,
            })),
        }))
    }

    async fn create_latest_event_for_inner(
        weak_room: &WeakRoom,
        thread_id: Option<&EventId>,
        room_event_cache: &RoomEventCache,
    ) -> LatestEvent {
        LatestEvent::new(weak_room, thread_id, room_event_cache).await
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

    /// The room event cache associated to this room.
    room_event_cache: RoomEventCache,

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
    async fn create_latest_event_for(&self, thread_id: Option<&EventId>) -> LatestEvent {
        RoomLatestEvents::create_latest_event_for_inner(
            &self.inner.weak_room,
            thread_id,
            &self.inner.room_event_cache,
        )
        .await
    }

    /// Check whether this [`RoomLatestEvents`] has a latest event for a
    /// particular thread.
    pub fn has_thread(&self, thread_id: &EventId) -> bool {
        self.inner.per_thread.contains_key(thread_id)
    }

    /// Create the [`LatestEvent`] for thread `thread_id` and insert it in this
    /// [`RoomLatestEvents`].
    pub async fn create_and_insert_latest_event_for_thread(&mut self, thread_id: &EventId) {
        let latest_event = self.create_latest_event_for(Some(thread_id)).await;

        self.inner.per_thread.insert(thread_id.to_owned(), latest_event);
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
        let room = self.inner.weak_room.get();
        let (own_user_id, power_levels) = match &room {
            Some(room) => {
                let power_levels = room.power_levels().await.ok();

                (Some(room.own_user_id()), power_levels)
            }

            None => (None, None),
        };

        let inner = &mut *self.inner;
        let for_the_room = &mut inner.for_the_room;
        let per_thread = &mut inner.per_thread;
        let room_event_cache = &inner.room_event_cache;

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
        let room = self.inner.weak_room.get();
        let (own_user_id, power_levels) = match &room {
            Some(room) => {
                let power_levels = room.power_levels().await.ok();

                (Some(room.own_user_id()), power_levels)
            }

            None => (None, None),
        };

        let inner = &mut *self.inner;
        let for_the_room = &mut inner.for_the_room;
        let per_thread = &mut inner.per_thread;
        let room_event_cache = &inner.room_event_cache;

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
