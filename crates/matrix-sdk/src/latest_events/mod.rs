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

//! The Latest Events API provides a lazy, reactive and efficient way to compute
//! the latest event for a room or a thread.
//!
//! The latest event represents the last displayable and relevant event a room
//! or a thread has been received. It is usually displayed in a _summary_, e.g.
//! below the room title in a room list.
//!
//! The entry point is [`LatestEvents`]. It is preferable to get a reference to
//! it from [`Client::latest_events`][crate::Client::latest_events], which
//! already plugs everything to build it. [`LatestEvents`] is using the
//! [`EventCache`] and the [`SendQueue`] to respectively get known remote events
//! (i.e. synced from the server), or local events (i.e. ones being sent).
//!
//! ## Laziness
//!
//! [`LatestEvents`] is lazy, it means that, despite [`LatestEvents`] is
//! listening to all [`EventCache`] or [`SendQueue`] updates, it will only do
//! something if one is expected to get the latest event for a particular room
//! or a particular thread. Concretely, it means that until
//! [`LatestEvents::listen_to_room`] is called for a particular room, no latest
//! event will ever be computed for that room (and similarly with
//! [`LatestEvents::listen_to_thread`]).
//!
//! If one is no longer interested to get the latest event for a particular room
//! or thread, the [`LatestEvents::forget_room`] and
//! [`LatestEvents::forget_thread`] methods must be used.
//!
//! ## Reactive
//!
//! [`LatestEvents`] is designed to be reactive. Using
//! [`LatestEvents::listen_and_subscribe_to_room`] will provide a
//! [`Subscriber`], which brings all the tooling to get the current value or the
//! future values with a stream.

mod error;
mod latest_event;

use std::{
    collections::{HashMap, HashSet},
    ops::{ControlFlow, Not},
    sync::Arc,
};

pub use error::LatestEventsError;
use eyeball::{AsyncLock, Subscriber};
use futures_util::FutureExt;
use latest_event::LatestEvent;
pub use latest_event::{LatestEventValue, LocalLatestEventValue, RemoteLatestEventValue};
use matrix_sdk_common::executor::{AbortOnDrop, JoinHandleExt as _, spawn};
use ruma::{EventId, OwnedEventId, OwnedRoomId, RoomId};
use tokio::{
    select,
    sync::{
        OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock, RwLockReadGuard, RwLockWriteGuard,
        broadcast, mpsc,
    },
};
use tracing::{error, warn};

use crate::{
    client::WeakClient,
    event_cache::{EventCache, EventCacheError, RoomEventCache, RoomEventCacheGenericUpdate},
    room::WeakRoom,
    send_queue::{RoomSendQueueUpdate, SendQueue, SendQueueUpdate},
};

/// The entry point to fetch the [`LatestEventValue`] for rooms or threads.
#[derive(Clone, Debug)]
pub struct LatestEvents {
    state: Arc<LatestEventsState>,
}

/// The state of [`LatestEvents`].
#[derive(Debug)]
struct LatestEventsState {
    /// All the registered rooms, i.e. rooms the latest events are computed for.
    registered_rooms: Arc<RegisteredRooms>,

    /// The task handle of the
    /// [`listen_to_event_cache_and_send_queue_updates_task`].
    _listen_task_handle: AbortOnDrop<()>,

    /// The task handle of the [`compute_latest_events_task`].
    _computation_task_handle: AbortOnDrop<()>,
}

impl LatestEvents {
    /// Create a new [`LatestEvents`].
    pub(crate) fn new(
        weak_client: WeakClient,
        event_cache: EventCache,
        send_queue: SendQueue,
    ) -> Self {
        let (room_registration_sender, room_registration_receiver) = mpsc::channel(32);
        let (latest_event_queue_sender, latest_event_queue_receiver) = mpsc::unbounded_channel();

        let registered_rooms =
            Arc::new(RegisteredRooms::new(room_registration_sender, weak_client, &event_cache));

        // The task listening to the event cache and the send queue updates.
        let listen_task_handle = spawn(listen_to_event_cache_and_send_queue_updates_task(
            registered_rooms.clone(),
            room_registration_receiver,
            event_cache,
            send_queue,
            latest_event_queue_sender,
        ))
        .abort_on_drop();

        // The task computing the new latest events.
        let computation_task_handle = spawn(compute_latest_events_task(
            registered_rooms.clone(),
            latest_event_queue_receiver,
        ))
        .abort_on_drop();

        Self {
            state: Arc::new(LatestEventsState {
                registered_rooms,
                _listen_task_handle: listen_task_handle,
                _computation_task_handle: computation_task_handle,
            }),
        }
    }

    /// Start listening to updates (if not already) for a particular room.
    ///
    /// It returns `true` if the room exists, `false` otherwise.
    pub async fn listen_to_room(&self, room_id: &RoomId) -> Result<bool, LatestEventsError> {
        Ok(self.state.registered_rooms.for_room(room_id).await?.is_some())
    }

    /// Check whether the system listens to a particular room.
    ///
    /// Note: It's a test only method.
    #[cfg(test)]
    pub async fn is_listening_to_room(&self, room_id: &RoomId) -> bool {
        self.state.registered_rooms.rooms.read().await.contains_key(room_id)
    }

    /// Start listening to updates (if not already) for a particular room, and
    /// return a [`Subscriber`] to get the current and future
    /// [`LatestEventValue`]s.
    ///
    /// It returns `Some` if the room exists, `None` otherwise.
    pub async fn listen_and_subscribe_to_room(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<Subscriber<LatestEventValue, AsyncLock>>, LatestEventsError> {
        let Some(room_latest_events) = self.state.registered_rooms.for_room(room_id).await? else {
            return Ok(None);
        };

        let room_latest_events = room_latest_events.read().await;
        let latest_event = room_latest_events.for_room();

        Ok(Some(latest_event.subscribe().await))
    }

    /// Start listening to updates (if not already) for a particular room and a
    /// particular thread in this room.
    ///
    /// It returns `true` if the room and the thread exists, `false` otherwise.
    pub async fn listen_to_thread(
        &self,
        room_id: &RoomId,
        thread_id: &EventId,
    ) -> Result<bool, LatestEventsError> {
        Ok(self.state.registered_rooms.for_thread(room_id, thread_id).await?.is_some())
    }

    /// Start listening to updates (if not already) for a particular room and a
    /// particular thread in this room, and return a [`Subscriber`] to get the
    /// current and future [`LatestEventValue`]s.
    ///
    /// It returns `Some` if the room and the thread exists, `None` otherwise.
    pub async fn listen_and_subscribe_to_thread(
        &self,
        room_id: &RoomId,
        thread_id: &EventId,
    ) -> Result<Option<Subscriber<LatestEventValue, AsyncLock>>, LatestEventsError> {
        let Some(room_latest_events) =
            self.state.registered_rooms.for_thread(room_id, thread_id).await?
        else {
            return Ok(None);
        };

        let room_latest_events = room_latest_events.read().await;
        let latest_event = room_latest_events
            .for_thread(thread_id)
            .expect("The `LatestEvent` for the thread must have been created");

        Ok(Some(latest_event.subscribe().await))
    }

    /// Forget a room.
    ///
    /// It means that [`LatestEvents`] will stop listening to updates for the
    /// `LatestEvent`s of the room and all its threads.
    ///
    /// If [`LatestEvents`] is not listening for `room_id`, nothing happens.
    pub async fn forget_room(&self, room_id: &RoomId) {
        self.state.registered_rooms.forget_room(room_id).await;
    }

    /// Forget a thread.
    ///
    /// It means that [`LatestEvents`] will stop listening to updates for the
    /// `LatestEvent` of the thread.
    ///
    /// If [`LatestEvents`] is not listening for `room_id` or `thread_id`,
    /// nothing happens.
    pub async fn forget_thread(&self, room_id: &RoomId, thread_id: &EventId) {
        self.state.registered_rooms.forget_thread(room_id, thread_id).await;
    }
}

#[derive(Debug)]
struct RegisteredRooms {
    /// All the registered [`RoomLatestEvents`].
    rooms: RwLock<HashMap<OwnedRoomId, RoomLatestEvents>>,

    /// The sender part of the channel about room registration.
    ///
    /// When a room is registered (with [`LatestEvents::listen_to_room`] or
    /// [`LatestEvents::listen_to_thread`]) or unregistered (with
    /// [`LatestEvents::forget_room`] or [`LatestEvents::forget_thread`]), a
    /// room registration message is passed on this channel.
    ///
    /// The receiver part of the channel is in the
    /// [`listen_to_event_cache_and_send_queue_updates_task`].
    room_registration_sender: mpsc::Sender<RoomRegistration>,

    /// The (weak) client.
    weak_client: WeakClient,

    /// The event cache.
    event_cache: EventCache,
}

impl RegisteredRooms {
    fn new(
        room_registration_sender: mpsc::Sender<RoomRegistration>,
        weak_client: WeakClient,
        event_cache: &EventCache,
    ) -> Self {
        Self {
            rooms: RwLock::new(HashMap::default()),
            room_registration_sender,
            weak_client,
            event_cache: event_cache.clone(),
        }
    }

    /// Get a read lock guard to a [`RoomLatestEvents`] given a room ID and an
    /// optional thread ID.
    ///
    /// The [`RoomLatestEvents`], and the associated [`LatestEvent`], will be
    /// created if missing. It means that write lock is taken if necessary, but
    /// it's always downgraded to a read lock at the end.
    async fn room_latest_event(
        &self,
        room_id: &RoomId,
        thread_id: Option<&EventId>,
    ) -> Result<Option<RwLockReadGuard<'_, RoomLatestEvents>>, LatestEventsError> {
        Ok(match thread_id {
            // Get the room latest event with the aim of fetching the latest event for a particular
            // thread.
            //
            // We need to take a write lock immediately, in case the thead latest event doesn't
            // exist.
            Some(thread_id) => {
                let mut rooms = self.rooms.write().await;

                // The `RoomLatestEvents` doesn't exist. Let's create and insert it.
                if rooms.contains_key(room_id).not() {
                    // Insert the room if it's been successfully created.
                    if let Some(room_latest_event) = RoomLatestEvents::new(
                        WeakRoom::new(self.weak_client.clone(), room_id.to_owned()),
                        &self.event_cache,
                    )
                    .await?
                    {
                        rooms.insert(room_id.to_owned(), room_latest_event);

                        let _ = self
                            .room_registration_sender
                            .send(RoomRegistration::Add(room_id.to_owned()))
                            .await;
                    }
                }

                if let Some(room_latest_event) = rooms.get(room_id) {
                    let mut room_latest_event = room_latest_event.write().await;

                    // In `RoomLatestEvents`, the `LatestEvent` for this thread doesn't exist. Let's
                    // create and insert it.
                    if room_latest_event.has_thread(thread_id).not() {
                        room_latest_event
                            .create_and_insert_latest_event_for_thread(thread_id)
                            .await;
                    }
                }

                RwLockWriteGuard::try_downgrade_map(rooms, |rooms| rooms.get(room_id)).ok()
            }

            // Get the room latest event with the aim of fetching the latest event for a particular
            // room.
            None => {
                match RwLockReadGuard::try_map(self.rooms.read().await, |rooms| rooms.get(room_id))
                    .ok()
                {
                    value @ Some(_) => value,
                    None => {
                        let mut rooms = self.rooms.write().await;

                        if rooms.contains_key(room_id).not() {
                            // Insert the room if it's been successfully created.
                            if let Some(room_latest_event) = RoomLatestEvents::new(
                                WeakRoom::new(self.weak_client.clone(), room_id.to_owned()),
                                &self.event_cache,
                            )
                            .await?
                            {
                                rooms.insert(room_id.to_owned(), room_latest_event);

                                let _ = self
                                    .room_registration_sender
                                    .send(RoomRegistration::Add(room_id.to_owned()))
                                    .await;
                            }
                        }

                        RwLockWriteGuard::try_downgrade_map(rooms, |rooms| rooms.get(room_id)).ok()
                    }
                }
            }
        })
    }

    /// Start listening to updates (if not already) for a particular room.
    ///
    /// It returns `None` if the room doesn't exist.
    pub async fn for_room(
        &self,
        room_id: &RoomId,
    ) -> Result<Option<RwLockReadGuard<'_, RoomLatestEvents>>, LatestEventsError> {
        self.room_latest_event(room_id, None).await
    }

    /// Start listening to updates (if not already) for a particular room.
    ///
    /// It returns `None` if the room or the thread doesn't exist.
    pub async fn for_thread(
        &self,
        room_id: &RoomId,
        thread_id: &EventId,
    ) -> Result<Option<RwLockReadGuard<'_, RoomLatestEvents>>, LatestEventsError> {
        self.room_latest_event(room_id, Some(thread_id)).await
    }

    /// Forget a room.
    ///
    /// It means that [`LatestEvents`] will stop listening to updates for the
    /// `LatestEvent`s of the room and all its threads.
    ///
    /// If [`LatestEvents`] is not listening for `room_id`, nothing happens.
    pub async fn forget_room(&self, room_id: &RoomId) {
        {
            let mut rooms = self.rooms.write().await;

            // Remove the whole `RoomLatestEvents`.
            rooms.remove(room_id);
        }

        let _ =
            self.room_registration_sender.send(RoomRegistration::Remove(room_id.to_owned())).await;
    }

    /// Forget a thread.
    ///
    /// It means that [`LatestEvents`] will stop listening to updates for the
    /// `LatestEvent` of the thread.
    ///
    /// If [`LatestEvents`] is not listening for `room_id` or `thread_id`,
    /// nothing happens.
    pub async fn forget_thread(&self, room_id: &RoomId, thread_id: &EventId) {
        let rooms = self.rooms.read().await;

        // If the `RoomLatestEvents`, remove the `LatestEvent` in `per_thread`.
        if let Some(room_latest_event) = rooms.get(room_id) {
            let mut room_latest_event = room_latest_event.write().await;

            // Release the lock on `self.rooms`.
            drop(rooms);

            room_latest_event.forget_thread(thread_id);
        }
    }
}

/// Represents whether a room has been registered or forgotten.
///
/// This is used by [`RegisteredRooms::for_room`],
/// [`RegisteredRooms::for_thread`], [`RegisteredRooms::forget_room`] and
/// [`RegisteredRooms::forget_thread`].
#[derive(Debug)]
enum RoomRegistration {
    /// [`LatestEvents`] wants to listen to this room.
    Add(OwnedRoomId),

    /// [`LatestEvents`] wants to no longer listen to this room.
    Remove(OwnedRoomId),
}

/// Represents the kind of updates the [`compute_latest_events_task`] will have
/// to deal with.
enum LatestEventQueueUpdate {
    /// An update from the [`EventCache`] happened.
    EventCache {
        /// The ID of the room that has triggered the update.
        room_id: OwnedRoomId,
    },

    /// An update from the [`SendQueue`] happened.
    SendQueue {
        /// The ID of the room that has triggered the update.
        room_id: OwnedRoomId,

        /// The update itself.
        update: RoomSendQueueUpdate,
    },
}

/// Type holding the [`LatestEvent`] for a room and for all its threads.
#[derive(Debug)]
struct RoomLatestEvents {
    /// The state of this type.
    state: Arc<RwLock<RoomLatestEventsState>>,
}

impl RoomLatestEvents {
    /// Create a new [`RoomLatestEvents`].
    async fn new(
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
    async fn read(&self) -> RoomLatestEventsReadGuard {
        RoomLatestEventsReadGuard { inner: self.state.clone().read_owned().await }
    }

    /// Lock this type with exclusive write access, and return an owned lock
    /// guard.
    async fn write(&self) -> RoomLatestEventsWriteGuard {
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
struct RoomLatestEventsReadGuard {
    inner: OwnedRwLockReadGuard<RoomLatestEventsState>,
}

impl RoomLatestEventsReadGuard {
    /// Get the [`LatestEvent`] for the room.
    fn for_room(&self) -> &LatestEvent {
        &self.inner.for_the_room
    }

    /// Get the [`LatestEvent`] for the thread if it exists.
    fn for_thread(&self, thread_id: &EventId) -> Option<&LatestEvent> {
        self.inner.per_thread.get(thread_id)
    }
}

/// The owned lock guard returned by [`RoomLatestEvents::write`].
struct RoomLatestEventsWriteGuard {
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
    fn has_thread(&self, thread_id: &EventId) -> bool {
        self.inner.per_thread.contains_key(thread_id)
    }

    /// Create the [`LatestEvent`] for thread `thread_id` and insert it in this
    /// [`RoomLatestEvents`].
    async fn create_and_insert_latest_event_for_thread(&mut self, thread_id: &EventId) {
        let latest_event = self.create_latest_event_for(Some(thread_id)).await;

        self.inner.per_thread.insert(thread_id.to_owned(), latest_event);
    }

    /// Forget the thread `thread_id`.
    fn forget_thread(&mut self, thread_id: &EventId) {
        self.inner.per_thread.remove(thread_id);
    }

    /// Update the latest events for the room and its threads, based on the
    /// event cache data.
    async fn update_with_event_cache(&mut self) {
        // Get the power levels of the user for the current room if the `WeakRoom` is
        // still valid.
        //
        // Get it once for all the updates of all the latest events for this room (be
        // the room and its threads).
        let room = self.inner.weak_room.get();
        let power_levels = match &room {
            Some(room) => {
                let power_levels = room.power_levels().await.ok();

                Some(room.own_user_id()).zip(power_levels)
            }

            None => None,
        };

        let inner = &mut *self.inner;
        let for_the_room = &mut inner.for_the_room;
        let per_thread = &mut inner.per_thread;
        let room_event_cache = &inner.room_event_cache;

        for_the_room.update_with_event_cache(room_event_cache, &power_levels).await;

        for latest_event in per_thread.values_mut() {
            latest_event.update_with_event_cache(room_event_cache, &power_levels).await;
        }
    }

    /// Update the latest events for the room and its threads, based on the
    /// send queue update.
    async fn update_with_send_queue(&mut self, send_queue_update: &RoomSendQueueUpdate) {
        // Get the power levels of the user for the current room if the `WeakRoom` is
        // still valid.
        //
        // Get it once for all the updates of all the latest events for this room (be
        // the room and its threads).
        let room = self.inner.weak_room.get();
        let power_levels = match &room {
            Some(room) => {
                let power_levels = room.power_levels().await.ok();

                Some(room.own_user_id()).zip(power_levels)
            }

            None => None,
        };

        let inner = &mut *self.inner;
        let for_the_room = &mut inner.for_the_room;
        let per_thread = &mut inner.per_thread;
        let room_event_cache = &inner.room_event_cache;

        for_the_room
            .update_with_send_queue(send_queue_update, room_event_cache, &power_levels)
            .await;

        for latest_event in per_thread.values_mut() {
            latest_event
                .update_with_send_queue(send_queue_update, room_event_cache, &power_levels)
                .await;
        }
    }
}

/// The task responsible to listen to the [`EventCache`] and the [`SendQueue`].
/// When an update is received and is considered relevant, a message is sent to
/// the [`compute_latest_events_task`] to compute a new [`LatestEvent`].
///
/// This task also listens to [`RoomRegistration`] (see
/// [`LatestEventsState::room_registration_sender`] for the sender part). It
/// keeps an internal list of registered rooms, which helps to filter out
/// updates we aren't interested by.
///
/// When an update is considered relevant, a message is sent over the
/// `latest_event_queue_sender` channel. See [`compute_latest_events_task`].
async fn listen_to_event_cache_and_send_queue_updates_task(
    registered_rooms: Arc<RegisteredRooms>,
    mut room_registration_receiver: mpsc::Receiver<RoomRegistration>,
    event_cache: EventCache,
    send_queue: SendQueue,
    latest_event_queue_sender: mpsc::UnboundedSender<LatestEventQueueUpdate>,
) {
    let mut event_cache_generic_updates_subscriber =
        event_cache.subscribe_to_room_generic_updates();
    let mut send_queue_generic_updates_subscriber = send_queue.subscribe();

    // Initialise the list of rooms that are listened.
    //
    // Technically, we can use `registered_rooms.rooms` every time to get this
    // information, but it would involve a read-lock. In order to reduce the
    // pressure on this lock, we use this intermediate structure.
    let mut listened_rooms =
        HashSet::from_iter(registered_rooms.rooms.read().await.keys().cloned());

    loop {
        if listen_to_event_cache_and_send_queue_updates(
            &mut room_registration_receiver,
            &mut event_cache_generic_updates_subscriber,
            &mut send_queue_generic_updates_subscriber,
            &mut listened_rooms,
            &latest_event_queue_sender,
        )
        .await
        .is_break()
        {
            warn!("`listen_to_event_cache_and_send_queue_updates_task` has stopped");

            break;
        }
    }
}

/// The core of [`listen_to_event_cache_and_send_queue_updates_task`].
///
/// Having this function detached from its task is helpful for testing and for
/// state isolation.
async fn listen_to_event_cache_and_send_queue_updates(
    room_registration_receiver: &mut mpsc::Receiver<RoomRegistration>,
    event_cache_generic_updates_subscriber: &mut broadcast::Receiver<RoomEventCacheGenericUpdate>,
    send_queue_generic_updates_subscriber: &mut broadcast::Receiver<SendQueueUpdate>,
    listened_rooms: &mut HashSet<OwnedRoomId>,
    latest_event_queue_sender: &mpsc::UnboundedSender<LatestEventQueueUpdate>,
) -> ControlFlow<()> {
    // We need a biased select here: `room_registration_receiver` must have the
    // priority over other futures.
    select! {
        biased;

        update = room_registration_receiver.recv().fuse() => {
            match update {
                Some(RoomRegistration::Add(room_id)) => {
                    listened_rooms.insert(room_id);
                }
                Some(RoomRegistration::Remove(room_id)) => {
                    listened_rooms.remove(&room_id);
                }
                None => {
                    error!("`room_registration` channel has been closed");

                    return ControlFlow::Break(());
                }
            }
        }

        room_event_cache_generic_update = event_cache_generic_updates_subscriber.recv().fuse() => {
            if let Ok(RoomEventCacheGenericUpdate { room_id }) = room_event_cache_generic_update {
                if listened_rooms.contains(&room_id) {
                    let _ = latest_event_queue_sender.send(LatestEventQueueUpdate::EventCache {
                        room_id
                    });
                }
            } else {
                warn!("`event_cache_generic_updates` channel has been closed");

                return ControlFlow::Break(());
            }
        }

        send_queue_generic_update = send_queue_generic_updates_subscriber.recv().fuse() => {
            if let Ok(SendQueueUpdate { room_id, update }) = send_queue_generic_update {
                if listened_rooms.contains(&room_id) {
                    let _ = latest_event_queue_sender.send(LatestEventQueueUpdate::SendQueue {
                        room_id,
                        update
                    });
                }
            } else {
                warn!("`send_queue_generic_updates` channel has been closed");

                return ControlFlow::Break(());
            }
        }
    }

    ControlFlow::Continue(())
}

/// The task responsible to compute new [`LatestEvent`] for a particular room or
/// thread.
///
/// The messages are coming from
/// [`listen_to_event_cache_and_send_queue_updates_task`].
async fn compute_latest_events_task(
    registered_rooms: Arc<RegisteredRooms>,
    mut latest_event_queue_receiver: mpsc::UnboundedReceiver<LatestEventQueueUpdate>,
) {
    const BUFFER_SIZE: usize = 16;

    let mut buffer = Vec::with_capacity(BUFFER_SIZE);

    while latest_event_queue_receiver.recv_many(&mut buffer, BUFFER_SIZE).await > 0 {
        compute_latest_events(&registered_rooms, &buffer).await;
        buffer.clear();
    }

    warn!("`compute_latest_events_task` has stopped");
}

async fn compute_latest_events(
    registered_rooms: &RegisteredRooms,
    latest_event_queue_updates: &[LatestEventQueueUpdate],
) {
    for latest_event_queue_update in latest_event_queue_updates {
        match latest_event_queue_update {
            LatestEventQueueUpdate::EventCache { room_id } => {
                let rooms = registered_rooms.rooms.read().await;

                if let Some(room_latest_events) = rooms.get(room_id) {
                    let mut room_latest_events = room_latest_events.write().await;

                    // Release the lock on `registered_rooms`.
                    // It is possible because `room_latest_events` is an owned lock guard.
                    drop(rooms);

                    room_latest_events.update_with_event_cache().await;
                } else {
                    error!(?room_id, "Failed to find the room");

                    continue;
                }
            }

            LatestEventQueueUpdate::SendQueue { room_id, update } => {
                let rooms = registered_rooms.rooms.read().await;

                if let Some(room_latest_events) = rooms.get(room_id) {
                    let mut room_latest_events = room_latest_events.write().await;

                    // Release the lock on `registered_rooms`.
                    // It is possible because `room_latest_events` is an owned lock guard.
                    drop(rooms);

                    room_latest_events.update_with_send_queue(update).await;
                } else {
                    error!(?room_id, "Failed to find the room");

                    continue;
                }
            }
        }
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::ops::Not;

    use assert_matches::assert_matches;
    use matrix_sdk_base::{
        RoomState,
        deserialized_responses::TimelineEventKind,
        linked_chunk::{ChunkIdentifier, LinkedChunkId, Position, Update},
    };
    use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
    use ruma::{
        OwnedTransactionId, event_id,
        events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent},
        owned_room_id, room_id, user_id,
    };
    use stream_assert::assert_pending;

    use super::{
        HashSet, LatestEventValue, RemoteLatestEventValue, RoomEventCacheGenericUpdate,
        RoomRegistration, RoomSendQueueUpdate, SendQueueUpdate, broadcast,
        listen_to_event_cache_and_send_queue_updates, mpsc,
    };
    use crate::test_utils::mocks::MatrixMockServer;

    #[async_test]
    async fn test_latest_events_are_lazy() {
        let room_id_0 = room_id!("!r0");
        let room_id_1 = room_id!("!r1");
        let room_id_2 = room_id!("!r2");
        let thread_id_1_0 = event_id!("$ev1.0");
        let thread_id_2_0 = event_id!("$ev2.0");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        client.base_client().get_or_create_room(room_id_0, RoomState::Joined);
        client.base_client().get_or_create_room(room_id_1, RoomState::Joined);
        client.base_client().get_or_create_room(room_id_2, RoomState::Joined);

        client.event_cache().subscribe().unwrap();

        let latest_events = client.latest_events().await;

        // Despites there are many rooms, zero `RoomLatestEvents` are created.
        assert!(latest_events.state.registered_rooms.rooms.read().await.is_empty());

        // Now let's listen to two rooms.
        assert!(latest_events.listen_to_room(room_id_0).await.unwrap());
        assert!(latest_events.listen_to_room(room_id_1).await.unwrap());

        {
            let rooms = latest_events.state.registered_rooms.rooms.read().await;
            // There are two rooms…
            assert_eq!(rooms.len(), 2);
            // … which are room 0 and room 1.
            assert!(rooms.contains_key(room_id_0));
            assert!(rooms.contains_key(room_id_1));

            // Room 0 contains zero thread latest events.
            assert!(rooms.get(room_id_0).unwrap().read().await.inner.per_thread.is_empty());
            // Room 1 contains zero thread latest events.
            assert!(rooms.get(room_id_1).unwrap().read().await.inner.per_thread.is_empty());
        }

        // Now let's listen to one thread respectively for two rooms.
        assert!(latest_events.listen_to_thread(room_id_1, thread_id_1_0).await.unwrap());
        assert!(latest_events.listen_to_thread(room_id_2, thread_id_2_0).await.unwrap());

        {
            let rooms = latest_events.state.registered_rooms.rooms.read().await;
            // There are now three rooms…
            assert_eq!(rooms.len(), 3);
            // … yup, room 2 is now created.
            assert!(rooms.contains_key(room_id_0));
            assert!(rooms.contains_key(room_id_1));
            assert!(rooms.contains_key(room_id_2));

            // Room 0 contains zero thread latest events.
            assert!(rooms.get(room_id_0).unwrap().read().await.inner.per_thread.is_empty());
            // Room 1 contains one thread latest event…
            let room_1 = rooms.get(room_id_1).unwrap().read().await;
            assert_eq!(room_1.inner.per_thread.len(), 1);
            // … which is thread 1.0.
            assert!(room_1.inner.per_thread.contains_key(thread_id_1_0));
            // Room 2 contains one thread latest event…
            let room_2 = rooms.get(room_id_2).unwrap().read().await;
            assert_eq!(room_2.inner.per_thread.len(), 1);
            // … which is thread 2.0.
            assert!(room_2.inner.per_thread.contains_key(thread_id_2_0));
        }
    }

    #[async_test]
    async fn test_forget_room() {
        let room_id_0 = room_id!("!r0");
        let room_id_1 = room_id!("!r1");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        client.base_client().get_or_create_room(room_id_0, RoomState::Joined);
        client.base_client().get_or_create_room(room_id_1, RoomState::Joined);

        client.event_cache().subscribe().unwrap();

        let latest_events = client.latest_events().await;

        // Now let's fetch one room.
        assert!(latest_events.listen_to_room(room_id_0).await.unwrap());

        {
            let rooms = latest_events.state.registered_rooms.rooms.read().await;
            // There are one room…
            assert_eq!(rooms.len(), 1);
            // … which is room 0.
            assert!(rooms.contains_key(room_id_0));

            // Room 0 contains zero thread latest events.
            assert!(rooms.get(room_id_0).unwrap().read().await.inner.per_thread.is_empty());
        }

        // Now let's forget about room 0.
        latest_events.forget_room(room_id_0).await;

        {
            let rooms = latest_events.state.registered_rooms.rooms.read().await;
            // There are now zero rooms.
            assert!(rooms.is_empty());
        }
    }

    #[async_test]
    async fn test_forget_thread() {
        let room_id_0 = room_id!("!r0");
        let room_id_1 = room_id!("!r1");
        let thread_id_0_0 = event_id!("$ev0.0");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        client.base_client().get_or_create_room(room_id_0, RoomState::Joined);
        client.base_client().get_or_create_room(room_id_1, RoomState::Joined);

        client.event_cache().subscribe().unwrap();

        let latest_events = client.latest_events().await;

        // Now let's fetch one thread .
        assert!(latest_events.listen_to_thread(room_id_0, thread_id_0_0).await.unwrap());

        {
            let rooms = latest_events.state.registered_rooms.rooms.read().await;
            // There is one room…
            assert_eq!(rooms.len(), 1);
            // … which is room 0.
            assert!(rooms.contains_key(room_id_0));

            // Room 0 contains one thread latest event…
            let room_0 = rooms.get(room_id_0).unwrap().read().await;
            assert_eq!(room_0.inner.per_thread.len(), 1);
            // … which is thread 0.0.
            assert!(room_0.inner.per_thread.contains_key(thread_id_0_0));
        }

        // Now let's forget about the thread.
        latest_events.forget_thread(room_id_0, thread_id_0_0).await;

        {
            let rooms = latest_events.state.registered_rooms.rooms.read().await;
            // There is still one room…
            assert_eq!(rooms.len(), 1);
            // … which is room 0.
            assert!(rooms.contains_key(room_id_0));

            // But the thread has been removed.
            assert!(rooms.get(room_id_0).unwrap().read().await.inner.per_thread.is_empty());
        }
    }

    #[async_test]
    async fn test_inputs_task_can_listen_to_room_registration() {
        let room_id_0 = owned_room_id!("!r0");
        let room_id_1 = owned_room_id!("!r1");

        let (room_registration_sender, mut room_registration_receiver) = mpsc::channel(1);
        let (_room_event_cache_generic_update_sender, mut room_event_cache_generic_update_receiver) =
            broadcast::channel(1);
        let (_send_queue_generic_update_sender, mut send_queue_generic_update_receiver) =
            broadcast::channel(1);
        let mut listened_rooms = HashSet::new();
        let (latest_event_queue_sender, latest_event_queue_receiver) = mpsc::unbounded_channel();

        // Send a _room update_ for the first time.
        {
            // It mimics `LatestEvents::for_room`.
            room_registration_sender.send(RoomRegistration::Add(room_id_0.clone())).await.unwrap();

            // Run the task.
            assert!(
                listen_to_event_cache_and_send_queue_updates(
                    &mut room_registration_receiver,
                    &mut room_event_cache_generic_update_receiver,
                    &mut send_queue_generic_update_receiver,
                    &mut listened_rooms,
                    &latest_event_queue_sender,
                )
                .await
                .is_continue()
            );

            assert_eq!(listened_rooms.len(), 1);
            assert!(listened_rooms.contains(&room_id_0));
            assert!(latest_event_queue_receiver.is_empty());
        }

        // Send a _room update_ for the second time. It's the same room.
        {
            room_registration_sender.send(RoomRegistration::Add(room_id_0.clone())).await.unwrap();

            // Run the task.
            assert!(
                listen_to_event_cache_and_send_queue_updates(
                    &mut room_registration_receiver,
                    &mut room_event_cache_generic_update_receiver,
                    &mut send_queue_generic_update_receiver,
                    &mut listened_rooms,
                    &latest_event_queue_sender,
                )
                .await
                .is_continue()
            );

            // This is the second time this room is added. Nothing happens.
            assert_eq!(listened_rooms.len(), 1);
            assert!(listened_rooms.contains(&room_id_0));
            assert!(latest_event_queue_receiver.is_empty());
        }

        // Send another _room update_. It's a different room.
        {
            room_registration_sender
                .send(RoomRegistration::Add(room_id_1.to_owned()))
                .await
                .unwrap();

            // Run the task.
            assert!(
                listen_to_event_cache_and_send_queue_updates(
                    &mut room_registration_receiver,
                    &mut room_event_cache_generic_update_receiver,
                    &mut send_queue_generic_update_receiver,
                    &mut listened_rooms,
                    &latest_event_queue_sender,
                )
                .await
                .is_continue()
            );

            // This is the first time this room is added. It must appear.
            assert_eq!(listened_rooms.len(), 2);
            assert!(listened_rooms.contains(&room_id_0));
            assert!(listened_rooms.contains(&room_id_1));
            assert!(latest_event_queue_receiver.is_empty());
        }
    }

    #[async_test]
    async fn test_inputs_task_stops_when_room_registration_channel_is_closed() {
        let (_room_registration_sender, mut room_registration_receiver) = mpsc::channel(1);
        let (_room_event_cache_generic_update_sender, mut room_event_cache_generic_update_receiver) =
            broadcast::channel(1);
        let (_send_queue_generic_update_sender, mut send_queue_generic_update_receiver) =
            broadcast::channel(1);
        let mut listened_rooms = HashSet::new();
        let (latest_event_queue_sender, latest_event_queue_receiver) = mpsc::unbounded_channel();

        // Close the receiver to close the channel.
        room_registration_receiver.close();

        // Run the task.
        assert!(
            listen_to_event_cache_and_send_queue_updates(
                &mut room_registration_receiver,
                &mut room_event_cache_generic_update_receiver,
                &mut send_queue_generic_update_receiver,
                &mut listened_rooms,
                &latest_event_queue_sender,
            )
            .await
            // It breaks!
            .is_break()
        );

        assert_eq!(listened_rooms.len(), 0);
        assert!(latest_event_queue_receiver.is_empty());
    }

    #[async_test]
    async fn test_inputs_task_can_listen_to_room_event_cache() {
        let room_id = owned_room_id!("!r0");

        let (room_registration_sender, mut room_registration_receiver) = mpsc::channel(1);
        let (room_event_cache_generic_update_sender, mut room_event_cache_generic_update_receiver) =
            broadcast::channel(1);
        let (_send_queue_generic_update_sender, mut send_queue_generic_update_receiver) =
            broadcast::channel(1);
        let mut listened_rooms = HashSet::new();
        let (latest_event_queue_sender, latest_event_queue_receiver) = mpsc::unbounded_channel();

        // New event cache update, but the `LatestEvents` isn't listening to it.
        {
            room_event_cache_generic_update_sender
                .send(RoomEventCacheGenericUpdate { room_id: room_id.clone() })
                .unwrap();

            // Run the task.
            assert!(
                listen_to_event_cache_and_send_queue_updates(
                    &mut room_registration_receiver,
                    &mut room_event_cache_generic_update_receiver,
                    &mut send_queue_generic_update_receiver,
                    &mut listened_rooms,
                    &latest_event_queue_sender,
                )
                .await
                .is_continue()
            );

            assert!(listened_rooms.is_empty());

            // No latest event computation has been triggered.
            assert!(latest_event_queue_receiver.is_empty());
        }

        // New event cache update, but this time, the `LatestEvents` is listening to it.
        {
            room_registration_sender.send(RoomRegistration::Add(room_id.clone())).await.unwrap();
            room_event_cache_generic_update_sender
                .send(RoomEventCacheGenericUpdate { room_id: room_id.clone() })
                .unwrap();

            // Run the task to handle the `RoomRegistration` and the
            // `RoomEventCacheGenericUpdate`.
            for _ in 0..2 {
                assert!(
                    listen_to_event_cache_and_send_queue_updates(
                        &mut room_registration_receiver,
                        &mut room_event_cache_generic_update_receiver,
                        &mut send_queue_generic_update_receiver,
                        &mut listened_rooms,
                        &latest_event_queue_sender,
                    )
                    .await
                    .is_continue()
                );
            }

            assert_eq!(listened_rooms.len(), 1);
            assert!(listened_rooms.contains(&room_id));

            // A latest event computation has been triggered!
            assert!(latest_event_queue_receiver.is_empty().not());
        }
    }

    #[async_test]
    async fn test_inputs_task_can_listen_to_send_queue() {
        let room_id = owned_room_id!("!r0");

        let (room_registration_sender, mut room_registration_receiver) = mpsc::channel(1);
        let (_room_event_cache_generic_update_sender, mut room_event_cache_generic_update_receiver) =
            broadcast::channel(1);
        let (send_queue_generic_update_sender, mut send_queue_generic_update_receiver) =
            broadcast::channel(1);
        let mut listened_rooms = HashSet::new();
        let (latest_event_queue_sender, latest_event_queue_receiver) = mpsc::unbounded_channel();

        // New send queue update, but the `LatestEvents` isn't listening to it.
        {
            send_queue_generic_update_sender
                .send(SendQueueUpdate {
                    room_id: room_id.clone(),
                    update: RoomSendQueueUpdate::SentEvent {
                        transaction_id: OwnedTransactionId::from("txnid0"),
                        event_id: event_id!("$ev0").to_owned(),
                    },
                })
                .unwrap();

            // Run the task.
            assert!(
                listen_to_event_cache_and_send_queue_updates(
                    &mut room_registration_receiver,
                    &mut room_event_cache_generic_update_receiver,
                    &mut send_queue_generic_update_receiver,
                    &mut listened_rooms,
                    &latest_event_queue_sender,
                )
                .await
                .is_continue()
            );

            assert!(listened_rooms.is_empty());

            // No latest event computation has been triggered.
            assert!(latest_event_queue_receiver.is_empty());
        }

        // New send queue update, but this time, the `LatestEvents` is listening to it.
        {
            room_registration_sender.send(RoomRegistration::Add(room_id.clone())).await.unwrap();
            send_queue_generic_update_sender
                .send(SendQueueUpdate {
                    room_id: room_id.clone(),
                    update: RoomSendQueueUpdate::SentEvent {
                        transaction_id: OwnedTransactionId::from("txnid1"),
                        event_id: event_id!("$ev1").to_owned(),
                    },
                })
                .unwrap();

            // Run the task to handle the `RoomRegistration` and the `SendQueueUpdate`.
            for _ in 0..2 {
                assert!(
                    listen_to_event_cache_and_send_queue_updates(
                        &mut room_registration_receiver,
                        &mut room_event_cache_generic_update_receiver,
                        &mut send_queue_generic_update_receiver,
                        &mut listened_rooms,
                        &latest_event_queue_sender,
                    )
                    .await
                    .is_continue()
                );
            }

            assert_eq!(listened_rooms.len(), 1);
            assert!(listened_rooms.contains(&room_id));

            // A latest event computation has been triggered!
            assert!(latest_event_queue_receiver.is_empty().not());
        }
    }

    #[async_test]
    async fn test_inputs_task_stops_when_event_cache_channel_is_closed() {
        let (_room_registration_sender, mut room_registration_receiver) = mpsc::channel(1);
        let (room_event_cache_generic_update_sender, mut room_event_cache_generic_update_receiver) =
            broadcast::channel(1);
        let (_send_queue_generic_update_sender, mut send_queue_generic_update_receiver) =
            broadcast::channel(1);
        let mut listened_rooms = HashSet::new();
        let (latest_event_queue_sender, latest_event_queue_receiver) = mpsc::unbounded_channel();

        // Drop the sender to close the channel.
        drop(room_event_cache_generic_update_sender);

        // Run the task.
        assert!(
            listen_to_event_cache_and_send_queue_updates(
                &mut room_registration_receiver,
                &mut room_event_cache_generic_update_receiver,
                &mut send_queue_generic_update_receiver,
                &mut listened_rooms,
                &latest_event_queue_sender,
            )
            .await
            // It breaks!
            .is_break()
        );

        assert_eq!(listened_rooms.len(), 0);
        assert!(latest_event_queue_receiver.is_empty());
    }

    #[async_test]
    async fn test_inputs_task_stops_when_send_queue_channel_is_closed() {
        let (_room_registration_sender, mut room_registration_receiver) = mpsc::channel(1);
        let (_room_event_cache_generic_update_sender, mut room_event_cache_generic_update_receiver) =
            broadcast::channel(1);
        let (send_queue_generic_update_sender, mut send_queue_generic_update_receiver) =
            broadcast::channel(1);
        let mut listened_rooms = HashSet::new();
        let (latest_event_queue_sender, latest_event_queue_receiver) = mpsc::unbounded_channel();

        // Drop the sender to close the channel.
        drop(send_queue_generic_update_sender);

        // Run the task.
        assert!(
            listen_to_event_cache_and_send_queue_updates(
                &mut room_registration_receiver,
                &mut room_event_cache_generic_update_receiver,
                &mut send_queue_generic_update_receiver,
                &mut listened_rooms,
                &latest_event_queue_sender,
            )
            .await
            // It breaks!
            .is_break()
        );

        assert_eq!(listened_rooms.len(), 0);
        assert!(latest_event_queue_receiver.is_empty());
    }

    #[async_test]
    async fn test_latest_event_value_is_updated_via_event_cache() {
        let room_id = owned_room_id!("!r0");
        let user_id = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(user_id).room(&room_id);
        let event_id_0 = event_id!("$ev0");
        let event_id_1 = event_id!("$ev1");
        let event_id_2 = event_id!("$ev2");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        // Prelude.
        {
            // Create the room.
            client.base_client().get_or_create_room(&room_id, RoomState::Joined);

            // Initialise the event cache store.
            client
                .event_cache_store()
                .lock()
                .await
                .unwrap()
                .handle_linked_chunk_updates(
                    LinkedChunkId::Room(&room_id),
                    vec![
                        Update::NewItemsChunk {
                            previous: None,
                            new: ChunkIdentifier::new(0),
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(0), 0),
                            items: vec![
                                event_factory.text_msg("hello").event_id(event_id_0).into(),
                                event_factory.text_msg("world").event_id(event_id_1).into(),
                            ],
                        },
                    ],
                )
                .await
                .unwrap();
        }

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let latest_events = client.latest_events().await;

        // Subscribe to the latest event values for this room.
        let mut latest_event_stream =
            latest_events.listen_and_subscribe_to_room(&room_id).await.unwrap().unwrap();

        // The initial latest event value is set to `event_id_1` because it's… the…
        // latest event!
        assert_matches!(
            latest_event_stream.get().await,
            LatestEventValue::Remote(RemoteLatestEventValue { kind: TimelineEventKind::PlainText { event }, .. }) => {
                assert_matches!(
                    event.deserialize().unwrap(),
                    AnySyncTimelineEvent::MessageLike(
                        AnySyncMessageLikeEvent::RoomMessage(
                            SyncMessageLikeEvent::Original(message_content)
                        )
                    ) => {
                        assert_eq!(message_content.content.body(), "world");
                    }
                );
            }
        );

        // The stream is pending: no new latest event for the moment.
        assert_pending!(latest_event_stream);

        // Update the event cache with a sync.
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(&room_id).add_timeline_event(
                    event_factory.text_msg("raclette !").event_id(event_id_2).into_raw(),
                ),
            )
            .await;

        // The event cache has received its update from the sync. It has emitted a
        // generic update, which has been received by `LatestEvents` tasks, up to the
        // `compute_latest_events` which has updated the latest event value.
        assert_matches!(
            latest_event_stream.next().await,
            Some(LatestEventValue::Remote(RemoteLatestEventValue { kind: TimelineEventKind::PlainText { event }, .. })) => {
                assert_matches!(
                    event.deserialize().unwrap(),
                    AnySyncTimelineEvent::MessageLike(
                        AnySyncMessageLikeEvent::RoomMessage(
                            SyncMessageLikeEvent::Original(message_content)
                        )
                    ) => {
                        assert_eq!(message_content.content.body(), "raclette !");
                    }
                );
            }
        );
    }
}
