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

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
};

use matrix_sdk_base::RoomInfoNotableUpdateReasons;
use ruma::{EventId, OwnedEventId};
use tokio::sync::{OnceCell, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
use tracing::error;

use super::{
    LatestEvent,
    latest_event::{IsLatestEventValueNone, LatestEventValue, With},
};
use crate::{
    Room,
    event_cache::{EventCache, EventCacheError, RoomEventCache},
    room::WeakRoom,
    send_queue::RoomSendQueueUpdate,
};

/// Tracks the automatic latest-event backfill of one room, so that a room gets
/// at most one backfill attempt per genuine event-cache update: a backfill
/// whose recomputation still yields no value must not immediately trigger
/// another backfill, otherwise the loop would never converge.
///
/// The state lives outside the [`RoomLatestEvents`] lock: it is read and
/// written by the computation task and by detached backfill tasks.
#[derive(Clone, Debug)]
pub(super) struct BackfillState {
    state: Arc<AtomicU8>,
}

const BACKFILL_IDLE: u8 = 0;
const BACKFILL_IN_FLIGHT: u8 = 1;
const BACKFILL_ATTEMPTED: u8 = 2;

impl BackfillState {
    fn new() -> Self {
        Self { state: Arc::new(AtomicU8::new(BACKFILL_IDLE)) }
    }

    /// A genuine event-cache update arrived for the room: allow a new backfill
    /// attempt.
    pub fn reset(&self) {
        self.state.store(BACKFILL_IDLE, Ordering::Release);
    }

    /// Try to claim the backfill attempt. Returns `false` if one is already in
    /// flight, or if one has already been attempted for the current data.
    pub fn try_begin(&self) -> bool {
        self.state
            .compare_exchange(
                BACKFILL_IDLE,
                BACKFILL_IN_FLIGHT,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }

    /// The claimed backfill attempt finished. If the state has been `reset` in
    /// the meantime (a genuine update arrived mid-flight), the reset wins and a
    /// new attempt remains allowed.
    pub fn mark_attempted(&self) {
        let _ = self.state.compare_exchange(
            BACKFILL_IN_FLIGHT,
            BACKFILL_ATTEMPTED,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

/// Type holding the [`LatestEvent`] for a room and for all its threads.
#[derive(Debug)]
pub(super) struct RoomLatestEvents {
    /// The state of this type.
    state: Arc<RwLock<RoomLatestEventsState>>,

    /// The state of the automatic latest-event backfill for this room.
    backfill: BackfillState,
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
            backfill: BackfillState::new(),
        })
    }

    /// The state of the automatic latest-event backfill for this room.
    pub fn backfill(&self) -> &BackfillState {
        &self.backfill
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

    /// The room these latest events belong to, if it still exists.
    pub fn room(&self) -> Option<Room> {
        self.inner.weak_room.get()
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

    /// The room these latest events belong to, if it still exists.
    pub fn room(&self) -> Option<Room> {
        self.inner.weak_room.get()
    }

    /// Update the latest events for the room and its threads, based on the
    /// event cache data.
    ///
    /// Returns the new values (for the room, then its threads, in that order)
    /// that must be persisted in the `RoomInfo` (see
    /// [`super::persist_latest_event_value`]).
    pub async fn update_with_event_cache(&mut self) -> Vec<LatestEventValue> {
        // Get the power levels of the user for the current room if the `WeakRoom` is
        // still valid.
        //
        // Get it once for all the updates of all the latest events for this room (be
        // the room and its threads).
        let Some(room) = self.inner.weak_room.get() else {
            // No room? Let's stop the update.
            error!(room = ?self.inner.weak_room, "Room is unknown");

            return Vec::new();
        };
        let own_user_id = room.own_user_id();
        let power_levels = room.power_levels().await.ok();

        let inner = &mut *self.inner;
        let for_the_room = &mut inner.for_the_room;
        let per_thread = &mut inner.per_thread;

        // Lazy-load the `RoomEventCache`.
        let room_event_cache = match inner
            .room_event_cache
            .get_or_try_init(|| async {
                // It's fine to drop the `EventCacheDropHandles` here as the caller
                // (`LatestEventState`) owns a clone of the `EventCache`.
                let (room_event_cache, _drop_handles) =
                    inner.event_cache.room(room.room_id()).await?;

                Ok::<RoomEventCache, EventCacheError>(room_event_cache)
            })
            .await
        {
            Ok(room_event_cache) => room_event_cache,
            Err(err) => {
                error!(room_id = ?room.room_id(), ?err, "Failed to fetch the `RoomEventCache`");
                return Vec::new();
            }
        };

        let mut pending_values = Vec::new();

        pending_values.extend(
            for_the_room
                .update_with_event_cache(room_event_cache, own_user_id, power_levels.as_ref())
                .await,
        );

        for latest_event in per_thread.values_mut() {
            pending_values.extend(
                latest_event
                    .update_with_event_cache(room_event_cache, own_user_id, power_levels.as_ref())
                    .await,
            );
        }

        pending_values
    }

    /// Update the latest events for the room and its threads, based on the
    /// send queue update.
    ///
    /// Returns the new values (for the room, then its threads, in that order)
    /// that must be persisted in the `RoomInfo` (see
    /// [`super::persist_latest_event_value`]).
    pub async fn update_with_send_queue(
        &mut self,
        send_queue_update: &RoomSendQueueUpdate,
    ) -> Vec<LatestEventValue> {
        // Get the power levels of the user for the current room if the `WeakRoom` is
        // still valid.
        //
        // Get it once for all the updates of all the latest events for this room (be
        // the room and its threads).
        let Some(room) = self.inner.weak_room.get() else {
            // No room? Let's stop the update.
            return Vec::new();
        };
        let own_user_id = room.own_user_id();
        let power_levels = room.power_levels().await.ok();

        let inner = &mut *self.inner;
        let for_the_room = &mut inner.for_the_room;
        let per_thread = &mut inner.per_thread;

        // Lazy-load the `RoomEventCache`.
        let room_event_cache = match inner
            .room_event_cache
            .get_or_try_init(|| async {
                // It's fine to drop the `EventCacheDropHandles` here as the caller
                // (`LatestEventState`) owns a clone of the `EventCache`.
                let (room_event_cache, _drop_handles) =
                    inner.event_cache.room(room.room_id()).await?;

                Ok::<RoomEventCache, EventCacheError>(room_event_cache)
            })
            .await
        {
            Ok(room_event_cache) => room_event_cache,
            Err(err) => {
                error!(room_id = ?room.room_id(), ?err, "Failed to fetch the `RoomEventCache`");
                return Vec::new();
            }
        };

        let mut pending_values = Vec::new();

        pending_values.extend(
            for_the_room
                .update_with_send_queue(
                    send_queue_update,
                    room_event_cache,
                    own_user_id,
                    power_levels.as_ref(),
                )
                .await,
        );

        for latest_event in per_thread.values_mut() {
            pending_values.extend(
                latest_event
                    .update_with_send_queue(
                        send_queue_update,
                        room_event_cache,
                        own_user_id,
                        power_levels.as_ref(),
                    )
                    .await,
            );
        }

        pending_values
    }

    /// Update the latest events for the room and its threads, based on the room
    /// info.
    ///
    /// Returns the new values that must be persisted in the `RoomInfo` (see
    /// [`super::persist_latest_event_value`]).
    pub async fn update_with_room_info(
        &mut self,
        reasons: RoomInfoNotableUpdateReasons,
    ) -> Vec<LatestEventValue> {
        // Get the state of the current room if the `WeakRoom` is still valid.
        let Some(room) = self.inner.weak_room.get() else {
            // No room? Let's stop the update.
            return Vec::new();
        };

        self.inner.for_the_room.update_with_room_info(room, reasons).await.into_iter().collect()
    }
}
