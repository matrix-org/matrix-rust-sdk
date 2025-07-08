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

mod latest_event;

use std::{
    collections::{HashMap, HashSet},
    ops::{ControlFlow, Not},
    sync::Arc,
};

use eyeball::{AsyncLock, Subscriber};
use futures_util::{select, FutureExt};
use latest_event::LatestEvent;
pub use latest_event::LatestEventValue;
use matrix_sdk_common::executor::{spawn, AbortOnDrop, JoinHandleExt as _};
use ruma::{EventId, OwnedEventId, OwnedRoomId, RoomId};
use tokio::sync::{broadcast, mpsc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use tracing::error;

use crate::{
    event_cache::{EventCache, RoomEventCacheGenericUpdate},
    send_queue::SendQueue,
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
    pub(crate) fn new(event_cache: EventCache, send_queue: SendQueue) -> Self {
        let (room_registration_sender, room_registration_receiver) = mpsc::channel(32);
        let (latest_event_queue_sender, latest_event_queue_receiver) = mpsc::unbounded_channel();

        let registered_rooms = Arc::new(RegisteredRooms::new(room_registration_sender));

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
    pub async fn listen_to_room(&self, room_id: &RoomId) -> bool {
        self.state.registered_rooms.for_room(room_id).await.is_some()
    }

    /// Start listening to updates (if not already) for a particular room, and
    /// return a [`Subscriber`] to get the current and future
    /// [`LatestEventValue`]s.
    ///
    /// It returns `Some` if the room exists, `None` otherwise.
    pub async fn listen_and_subscribe_to_room(
        &self,
        room_id: &RoomId,
    ) -> Option<Subscriber<LatestEventValue, AsyncLock>> {
        let latest_event = self.state.registered_rooms.for_room(room_id).await?;

        Some(latest_event.subscribe().await)
    }

    /// Start listening to updates (if not already) for a particular room and a
    /// particular thread in this room.
    ///
    /// It returns `true` if the room and the thread exists, `false` otherwise.
    pub async fn listen_to_thread(&self, room_id: &RoomId, thread_id: &EventId) -> bool {
        self.state.registered_rooms.for_thread(room_id, thread_id).await.is_some()
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
    ) -> Option<Subscriber<LatestEventValue, AsyncLock>> {
        let latest_event = self.state.registered_rooms.for_thread(room_id, thread_id).await?;

        Some(latest_event.subscribe().await)
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
}

impl RegisteredRooms {
    fn new(room_registration_sender: mpsc::Sender<RoomRegistration>) -> Self {
        Self { rooms: RwLock::new(HashMap::default()), room_registration_sender }
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
    ) -> Option<RwLockReadGuard<'_, RoomLatestEvents>> {
        match thread_id {
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
                    if let Some(room_latest_event) = RoomLatestEvents::new(room_id).await {
                        rooms.insert(room_id.to_owned(), room_latest_event);

                        let _ = self
                            .room_registration_sender
                            .send(RoomRegistration::Add(room_id.to_owned()))
                            .await;
                    }
                }

                if let Some(room_latest_event) = rooms.get_mut(room_id) {
                    // In `RoomLatestEvents`, the `LatestEvent` for this thread doesn't exist. Let's
                    // create and insert it.
                    if room_latest_event.per_thread.contains_key(thread_id).not() {
                        if let Some(latest_event) =
                            RoomLatestEvents::create_latest_event_for(room_id, Some(thread_id))
                                .await
                        {
                            room_latest_event.per_thread.insert(thread_id.to_owned(), latest_event);
                        }
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
                            if let Some(room_latest_event) = RoomLatestEvents::new(room_id).await {
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
        }
    }

    /// Start listening to updates (if not already) for a particular room, and
    /// fetch the [`LatestEvent`] for this room.
    ///
    /// It returns `None` if the room doesn't exist.
    pub async fn for_room(&self, room_id: &RoomId) -> Option<RwLockReadGuard<'_, LatestEvent>> {
        self.room_latest_event(room_id, None).await.map(|lock_guard| {
            RwLockReadGuard::map(lock_guard, |room_latest_event| room_latest_event.for_room())
        })
    }

    /// Start listening to updates (if not already) for a particular room, and
    /// fetch the [`LatestEvent`] for a particular thread in this room.
    ///
    /// It returns `None` if the room or the thread doesn't exist.
    pub async fn for_thread(
        &self,
        room_id: &RoomId,
        thread_id: &EventId,
    ) -> Option<RwLockReadGuard<'_, LatestEvent>> {
        self.room_latest_event(room_id, Some(thread_id)).await.and_then(|lock_guard| {
            RwLockReadGuard::try_map(lock_guard, |room_latest_event| {
                room_latest_event.for_thread(thread_id)
            })
            .ok()
        })
    }

    /// Forget a room.
    ///
    /// It means that [`LatestEvents`] will stop listening to updates for the
    /// `LatestEvent`s of the room and all its threads.
    ///
    /// If [`LatestEvents`] is not listening for `room_id`, nothing happens.
    pub async fn forget_room(&self, room_id: &RoomId) {
        let mut rooms = self.rooms.write().await;

        // Remove the whole `RoomLatestEvents`.
        rooms.remove(room_id);

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
        let mut rooms = self.rooms.write().await;

        // If the `RoomLatestEvents`, remove the `LatestEvent` in `per_thread`.
        if let Some(room_latest_event) = rooms.get_mut(room_id) {
            room_latest_event.per_thread.remove(thread_id);
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

/// Type holding the [`LatestEvent`] for a room and for all its threads.
#[derive(Debug)]
struct RoomLatestEvents {
    /// The latest event of the room.
    for_the_room: LatestEvent,

    /// The latest events for each thread.
    per_thread: HashMap<OwnedEventId, LatestEvent>,
}

impl RoomLatestEvents {
    async fn new(room_id: &RoomId) -> Option<Self> {
        Some(Self {
            for_the_room: Self::create_latest_event_for(room_id, None).await?,
            per_thread: HashMap::new(),
        })
    }

    #[allow(clippy::unused_async)]
    async fn create_latest_event_for(
        room_id: &RoomId,
        thread_id: Option<&EventId>,
    ) -> Option<LatestEvent> {
        LatestEvent::new(room_id, thread_id)
    }

    /// Get the [`LatestEvent`] for the room.
    fn for_room(&self) -> &LatestEvent {
        &self.for_the_room
    }

    /// Get the [`LatestEvent`] for the thread if it exists.
    fn for_thread(&self, thread_id: &EventId) -> Option<&LatestEvent> {
        self.per_thread.get(thread_id)
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
    _send_queue: SendQueue,
    latest_event_queue_sender: mpsc::UnboundedSender<OwnedRoomId>,
) {
    let mut event_cache_generic_updates_subscriber =
        event_cache.subscribe_to_room_generic_updates();

    // Initialise the list of rooms that are listened.
    //
    // Technically, we can use `rooms` to get this information, but it would involve
    // a read-lock. In order to reduce the pressure on this lock, we use
    // this intermediate structure.
    let mut listened_rooms = {
        let rooms = registered_rooms.rooms.read().await;

        HashSet::from_iter(rooms.keys().cloned())
    };

    loop {
        if listen_to_event_cache_and_send_queue_updates(
            &mut room_registration_receiver,
            &mut event_cache_generic_updates_subscriber,
            &mut listened_rooms,
            &latest_event_queue_sender,
        )
        .await
        .is_break()
        {
            error!("`listen_to_event_cache_and_send_queue_updates_task` has stopped");

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
    listened_rooms: &mut HashSet<OwnedRoomId>,
    latest_event_queue_sender: &mpsc::UnboundedSender<OwnedRoomId>,
) -> ControlFlow<()> {
    select! {
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
            if let Ok(room_event_cache_generic_update) = room_event_cache_generic_update {
                let room_id = room_event_cache_generic_update.room_id();

                if listened_rooms.contains(room_id) {
                    let _ = latest_event_queue_sender.send(room_id.to_owned());
                }
            } else {
                error!("`event_cache_generic_updates` channel has been closed");

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
    mut latest_event_queue_receiver: mpsc::UnboundedReceiver<OwnedRoomId>,
) {
    let mut buffer = Vec::with_capacity(16);

    while latest_event_queue_receiver.recv_many(&mut buffer, 16).await > 0 {
        compute_latest_events(&registered_rooms, &buffer).await;
        buffer.clear();
    }

    error!("`compute_latest_events_task` has stopped");
}

#[allow(clippy::unused_async)]
async fn compute_latest_events(_registered_rooms: &RegisteredRooms, _for_rooms: &[OwnedRoomId]) {
    // todo
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::ops::Not;

    use matrix_sdk_base::RoomState;
    use matrix_sdk_test::async_test;
    use ruma::{event_id, owned_room_id, room_id};

    use super::{
        broadcast, compute_latest_events, listen_to_event_cache_and_send_queue_updates, mpsc,
        HashSet, RegisteredRooms, RoomRegistration,
    };
    use crate::{
        event_cache::RoomEventCacheGenericUpdate, test_utils::logged_in_client_with_server,
    };

    #[async_test]
    async fn test_latest_events_are_lazy() {
        let room_id_0 = room_id!("!r0");
        let room_id_1 = room_id!("!r1");
        let room_id_2 = room_id!("!r2");
        let thread_id_1_0 = event_id!("$ev1.0");
        let thread_id_2_0 = event_id!("$ev2.0");

        let (client, _server) = logged_in_client_with_server().await;

        client.base_client().get_or_create_room(room_id_0, RoomState::Joined);
        client.base_client().get_or_create_room(room_id_1, RoomState::Joined);
        client.base_client().get_or_create_room(room_id_2, RoomState::Joined);

        let latest_events = client.latest_events().await;

        // Despites there are many rooms, zero `RoomLatestEvents` are created.
        assert!(latest_events.state.registered_rooms.rooms.read().await.is_empty());

        // Now let's listen to two rooms.
        assert!(latest_events.listen_to_room(room_id_0).await);
        assert!(latest_events.listen_to_room(room_id_1).await);

        {
            let rooms = latest_events.state.registered_rooms.rooms.read().await;
            // There are two rooms…
            assert_eq!(rooms.len(), 2);
            // … which are room 0 and room 1.
            assert!(rooms.contains_key(room_id_0));
            assert!(rooms.contains_key(room_id_1));

            // Room 0 contains zero thread latest events.
            assert!(rooms.get(room_id_0).unwrap().per_thread.is_empty());
            // Room 1 contains zero thread latest events.
            assert!(rooms.get(room_id_1).unwrap().per_thread.is_empty());
        }

        // Now let's listen to one thread respectively for two rooms.
        assert!(latest_events.listen_to_thread(room_id_1, thread_id_1_0).await);
        assert!(latest_events.listen_to_thread(room_id_2, thread_id_2_0).await);

        {
            let rooms = latest_events.state.registered_rooms.rooms.read().await;
            // There are now three rooms…
            assert_eq!(rooms.len(), 3);
            // … yup, room 2 is now created.
            assert!(rooms.contains_key(room_id_0));
            assert!(rooms.contains_key(room_id_1));
            assert!(rooms.contains_key(room_id_2));

            // Room 0 contains zero thread latest events.
            assert!(rooms.get(room_id_0).unwrap().per_thread.is_empty());
            // Room 1 contains one thread latest event…
            assert_eq!(rooms.get(room_id_1).unwrap().per_thread.len(), 1);
            // … which is thread 1.0.
            assert!(rooms.get(room_id_1).unwrap().per_thread.contains_key(thread_id_1_0));
            // Room 2 contains one thread latest event…
            assert_eq!(rooms.get(room_id_2).unwrap().per_thread.len(), 1);
            // … which is thread 2.0.
            assert!(rooms.get(room_id_2).unwrap().per_thread.contains_key(thread_id_2_0));
        }
    }

    #[async_test]
    async fn test_forget_room() {
        let room_id_0 = room_id!("!r0");
        let room_id_1 = room_id!("!r1");

        let (client, _server) = logged_in_client_with_server().await;

        client.base_client().get_or_create_room(room_id_0, RoomState::Joined);
        client.base_client().get_or_create_room(room_id_1, RoomState::Joined);

        let latest_events = client.latest_events().await;

        // Now let's fetch one room.
        assert!(latest_events.listen_to_room(room_id_0).await);

        {
            let rooms = latest_events.state.registered_rooms.rooms.read().await;
            // There are one room…
            assert_eq!(rooms.len(), 1);
            // … which is room 0.
            assert!(rooms.contains_key(room_id_0));

            // Room 0 contains zero thread latest events.
            assert!(rooms.get(room_id_0).unwrap().per_thread.is_empty());
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

        let (client, _server) = logged_in_client_with_server().await;

        client.base_client().get_or_create_room(room_id_0, RoomState::Joined);
        client.base_client().get_or_create_room(room_id_1, RoomState::Joined);

        let latest_events = client.latest_events().await;

        // Now let's fetch one thread .
        assert!(latest_events.listen_to_thread(room_id_0, thread_id_0_0).await);

        {
            let rooms = latest_events.state.registered_rooms.rooms.read().await;
            // There is one room…
            assert_eq!(rooms.len(), 1);
            // … which is room 0.
            assert!(rooms.contains_key(room_id_0));

            // Room 0 contains one thread latest event…
            assert_eq!(rooms.get(room_id_0).unwrap().per_thread.len(), 1);
            // … which is thread 0.0.
            assert!(rooms.get(room_id_0).unwrap().per_thread.contains_key(thread_id_0_0));
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
            assert!(rooms.get(room_id_0).unwrap().per_thread.is_empty());
        }
    }

    #[async_test]
    async fn test_inputs_task_can_listen_to_room_registration() {
        let room_id_0 = owned_room_id!("!r0");
        let room_id_1 = owned_room_id!("!r1");

        let (room_registration_sender, mut room_registration_receiver) = mpsc::channel(1);
        let (_room_event_cache_generic_update_sender, mut room_event_cache_generic_update_receiver) =
            broadcast::channel(1);
        let mut listened_rooms = HashSet::new();
        let (latest_event_queue_sender, latest_event_queue_receiver) = mpsc::unbounded_channel();

        // Send a _room update_ for the first time.
        {
            // It mimics `LatestEvents::for_room`.
            room_registration_sender.send(RoomRegistration::Add(room_id_0.clone())).await.unwrap();

            // Run the task.
            assert!(listen_to_event_cache_and_send_queue_updates(
                &mut room_registration_receiver,
                &mut room_event_cache_generic_update_receiver,
                &mut listened_rooms,
                &latest_event_queue_sender,
            )
            .await
            .is_continue());

            assert_eq!(listened_rooms.len(), 1);
            assert!(listened_rooms.contains(&room_id_0));
            assert!(latest_event_queue_receiver.is_empty());
        }

        // Send a _room update_ for the second time. It's the same room.
        {
            room_registration_sender.send(RoomRegistration::Add(room_id_0.clone())).await.unwrap();

            // Run the task.
            assert!(listen_to_event_cache_and_send_queue_updates(
                &mut room_registration_receiver,
                &mut room_event_cache_generic_update_receiver,
                &mut listened_rooms,
                &latest_event_queue_sender,
            )
            .await
            .is_continue());

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
            assert!(listen_to_event_cache_and_send_queue_updates(
                &mut room_registration_receiver,
                &mut room_event_cache_generic_update_receiver,
                &mut listened_rooms,
                &latest_event_queue_sender,
            )
            .await
            .is_continue());

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
        let mut listened_rooms = HashSet::new();
        let (latest_event_queue_sender, latest_event_queue_receiver) = mpsc::unbounded_channel();

        // Close the receiver to close the channel.
        room_registration_receiver.close();

        // Run the task.
        assert!(listen_to_event_cache_and_send_queue_updates(
            &mut room_registration_receiver,
            &mut room_event_cache_generic_update_receiver,
            &mut listened_rooms,
            &latest_event_queue_sender,
        )
        .await
        // It breaks!
        .is_break());

        assert_eq!(listened_rooms.len(), 0);
        assert!(latest_event_queue_receiver.is_empty());
    }

    #[async_test]
    async fn test_inputs_task_can_listen_to_room_event_cache() {
        let room_id = owned_room_id!("!r0");

        let (room_registration_sender, mut room_registration_receiver) = mpsc::channel(1);
        let (room_event_cache_generic_update_sender, mut room_event_cache_generic_update_receiver) =
            broadcast::channel(1);
        let mut listened_rooms = HashSet::new();
        let (latest_event_queue_sender, latest_event_queue_receiver) = mpsc::unbounded_channel();

        // New event cache update, but the `LatestEvents` isn't listening to it.
        {
            room_event_cache_generic_update_sender
                .send(RoomEventCacheGenericUpdate::UpdateTimeline { room_id: room_id.clone() })
                .unwrap();

            // Run the task.
            assert!(listen_to_event_cache_and_send_queue_updates(
                &mut room_registration_receiver,
                &mut room_event_cache_generic_update_receiver,
                &mut listened_rooms,
                &latest_event_queue_sender,
            )
            .await
            .is_continue());

            assert!(listened_rooms.is_empty());

            // No latest event computation has been triggered.
            assert!(latest_event_queue_receiver.is_empty());
        }

        // New event cache update, but this time, the `LatestEvents` is listening to it.
        {
            room_registration_sender.send(RoomRegistration::Add(room_id.clone())).await.unwrap();
            room_event_cache_generic_update_sender
                .send(RoomEventCacheGenericUpdate::UpdateTimeline { room_id: room_id.clone() })
                .unwrap();

            // Run the task to handle the `RoomRegistration` and the
            // `RoomEventCacheGenericUpdate`.
            for _ in 0..2 {
                assert!(listen_to_event_cache_and_send_queue_updates(
                    &mut room_registration_receiver,
                    &mut room_event_cache_generic_update_receiver,
                    &mut listened_rooms,
                    &latest_event_queue_sender,
                )
                .await
                .is_continue());
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
        let mut listened_rooms = HashSet::new();
        let (latest_event_queue_sender, latest_event_queue_receiver) = mpsc::unbounded_channel();

        // Drop the sender to close the channel.
        drop(room_event_cache_generic_update_sender);

        // Run the task.
        assert!(listen_to_event_cache_and_send_queue_updates(
            &mut room_registration_receiver,
            &mut room_event_cache_generic_update_receiver,
            &mut listened_rooms,
            &latest_event_queue_sender,
        )
        .await
        // It breaks!
        .is_break());

        assert_eq!(listened_rooms.len(), 0);
        assert!(latest_event_queue_receiver.is_empty());
    }

    #[async_test]
    async fn test_compute_latest_events() {
        let room_id = owned_room_id!("!r0");

        let (room_registration_sender, _room_registration_receiver) = mpsc::channel(1);
        let registered_rooms = RegisteredRooms::new(room_registration_sender);

        compute_latest_events(&registered_rooms, &[room_id]).await;
    }
}
