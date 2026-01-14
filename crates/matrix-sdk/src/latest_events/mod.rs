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
mod room_latest_events;

use std::{
    collections::HashMap,
    ops::{ControlFlow, DerefMut, Not},
    sync::Arc,
};

pub use error::LatestEventsError;
use eyeball::{AsyncLock, Subscriber};
use latest_event::{LatestEvent, With};
pub use latest_event::{LatestEventValue, LocalLatestEventValue, RemoteLatestEventValue};
use matrix_sdk_base::timer;
use matrix_sdk_common::executor::{AbortOnDrop, JoinHandleExt as _, spawn};
use room_latest_events::RoomLatestEvents;
use ruma::{EventId, OwnedRoomId, RoomId};
use tokio::{
    select,
    sync::{RwLock, RwLockReadGuard, RwLockWriteGuard, broadcast, mpsc},
};
use tracing::{error, warn};

use crate::{
    client::WeakClient,
    event_cache::{EventCache, RoomEventCacheGenericUpdate},
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
        let (latest_event_queue_sender, latest_event_queue_receiver) = mpsc::unbounded_channel();

        let registered_rooms =
            Arc::new(RegisteredRooms::new(weak_client, &event_cache, &latest_event_queue_sender));

        // The task listening to the event cache and the send queue updates.
        let listen_task_handle = spawn(listen_to_event_cache_and_send_queue_updates_task(
            registered_rooms.clone(),
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

    /// The (weak) client.
    weak_client: WeakClient,

    /// The event cache.
    event_cache: EventCache,

    /// The sender part of the channel used by [`compute_latest_events_task`].
    ///
    /// This is used to _trigger_ a computation of a `LatestEventValue` if the
    /// restored value is `None`.
    latest_event_queue_sender: mpsc::UnboundedSender<LatestEventQueueUpdate>,
}

impl RegisteredRooms {
    fn new(
        weak_client: WeakClient,
        event_cache: &EventCache,
        latest_event_queue_sender: &mpsc::UnboundedSender<LatestEventQueueUpdate>,
    ) -> Self {
        Self {
            rooms: RwLock::new(HashMap::default()),
            weak_client,
            event_cache: event_cache.clone(),
            latest_event_queue_sender: latest_event_queue_sender.clone(),
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
        fn create_and_insert_room_latest_events(
            room_id: &RoomId,
            rooms: &mut HashMap<OwnedRoomId, RoomLatestEvents>,
            weak_client: &WeakClient,
            event_cache: &EventCache,
            latest_event_queue_sender: &mpsc::UnboundedSender<LatestEventQueueUpdate>,
        ) {
            let (room_latest_events, is_latest_event_value_none) =
                With::unzip(RoomLatestEvents::new(
                    WeakRoom::new(weak_client.clone(), room_id.to_owned()),
                    event_cache,
                ));

            // Insert the new `RoomLatestEvents`.
            rooms.insert(room_id.to_owned(), room_latest_events);

            // If the `LatestEventValue` restored by `RoomLatestEvents` is of kind `None`,
            // let's try to re-compute it without waiting on the Event Cache (so the sync
            // usually) or the Send Queue. Maybe the system has migrated to a new version
            // and the `LatestEventValue` has been erased, while it is still possible to
            // compute a correct value.
            if is_latest_event_value_none {
                let _ = latest_event_queue_sender
                    .send(LatestEventQueueUpdate::EventCache { room_id: room_id.to_owned() });
            }
        }

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
                    create_and_insert_room_latest_events(
                        room_id,
                        rooms.deref_mut(),
                        &self.weak_client,
                        &self.event_cache,
                        &self.latest_event_queue_sender,
                    );
                }

                if let Some(room_latest_event) = rooms.get(room_id) {
                    let mut room_latest_event = room_latest_event.write().await;

                    // In `RoomLatestEvents`, the `LatestEvent` for this thread doesn't exist. Let's
                    // create and insert it.
                    if room_latest_event.has_thread(thread_id).not() {
                        room_latest_event.create_and_insert_latest_event_for_thread(thread_id);
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
                        let _timer = timer!(
                            tracing::Level::INFO,
                            format!("Creating `RoomLatestEvents` for {room_id:?}"),
                        );

                        let mut rooms = self.rooms.write().await;

                        if rooms.contains_key(room_id).not() {
                            create_and_insert_room_latest_events(
                                room_id,
                                rooms.deref_mut(),
                                &self.weak_client,
                                &self.event_cache,
                                &self.latest_event_queue_sender,
                            );
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

/// Represents the kind of updates the [`compute_latest_events_task`] will have
/// to deal with.
#[derive(Debug)]
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

/// The task responsible to listen to the [`EventCache`] and the [`SendQueue`].
/// When an update is received and is considered relevant, a message is sent to
/// the [`compute_latest_events_task`] to compute a new [`LatestEvent`].
///
/// When an update is considered relevant, a message is sent over the
/// `latest_event_queue_sender` channel. See [`compute_latest_events_task`].
async fn listen_to_event_cache_and_send_queue_updates_task(
    registered_rooms: Arc<RegisteredRooms>,
    event_cache: EventCache,
    send_queue: SendQueue,
    latest_event_queue_sender: mpsc::UnboundedSender<LatestEventQueueUpdate>,
) {
    let mut event_cache_generic_updates_subscriber =
        event_cache.subscribe_to_room_generic_updates();
    let mut send_queue_generic_updates_subscriber = send_queue.subscribe();

    loop {
        if listen_to_event_cache_and_send_queue_updates(
            &registered_rooms.rooms,
            &mut event_cache_generic_updates_subscriber,
            &mut send_queue_generic_updates_subscriber,
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
    registered_rooms: &RwLock<HashMap<OwnedRoomId, RoomLatestEvents>>,
    event_cache_generic_updates_subscriber: &mut broadcast::Receiver<RoomEventCacheGenericUpdate>,
    send_queue_generic_updates_subscriber: &mut broadcast::Receiver<SendQueueUpdate>,
    latest_event_queue_sender: &mpsc::UnboundedSender<LatestEventQueueUpdate>,
) -> ControlFlow<()> {
    select! {
        room_event_cache_generic_update = event_cache_generic_updates_subscriber.recv() => {
            if let Ok(RoomEventCacheGenericUpdate { room_id }) = room_event_cache_generic_update {
                if registered_rooms.read().await.contains_key(&room_id) {
                    let _ = latest_event_queue_sender.send(LatestEventQueueUpdate::EventCache {
                        room_id
                    });
                }
            } else {
                warn!("`event_cache_generic_updates` channel has been closed");

                return ControlFlow::Break(());
            }
        }

        send_queue_generic_update = send_queue_generic_updates_subscriber.recv() => {
            if let Ok(SendQueueUpdate { room_id, update }) = send_queue_generic_update {
                if registered_rooms.read().await.contains_key(&room_id) {
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

#[cfg(test)]
fn local_room_message(body: &str) -> LocalLatestEventValue {
    use matrix_sdk_base::store::SerializableEventContent;
    use ruma::{
        MilliSecondsSinceUnixEpoch,
        events::{AnyMessageLikeEventContent, room::message::RoomMessageEventContent},
    };

    LocalLatestEventValue {
        timestamp: MilliSecondsSinceUnixEpoch::now(),
        content: SerializableEventContent::new(&AnyMessageLikeEventContent::RoomMessage(
            RoomMessageEventContent::text_plain(body),
        ))
        .unwrap(),
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::{collections::HashMap, ops::Not};

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
    use tokio::task::yield_now;

    use super::{
        LatestEventValue, RegisteredRooms, RemoteLatestEventValue, RoomEventCacheGenericUpdate,
        RoomLatestEvents, RoomSendQueueUpdate, RwLock, SendQueueUpdate, WeakClient, WeakRoom, With,
        broadcast, listen_to_event_cache_and_send_queue_updates, mpsc,
    };
    use crate::{
        latest_events::{LatestEventQueueUpdate, local_room_message},
        test_utils::mocks::MatrixMockServer,
    };

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
            assert!(rooms.get(room_id_0).unwrap().read().await.per_thread().is_empty());
            // Room 1 contains zero thread latest events.
            assert!(rooms.get(room_id_1).unwrap().read().await.per_thread().is_empty());
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
            assert!(rooms.get(room_id_0).unwrap().read().await.per_thread().is_empty());
            // Room 1 contains one thread latest event…
            let room_1 = rooms.get(room_id_1).unwrap().read().await;
            assert_eq!(room_1.per_thread().len(), 1);
            // … which is thread 1.0.
            assert!(room_1.per_thread().contains_key(thread_id_1_0));
            // Room 2 contains one thread latest event…
            let room_2 = rooms.get(room_id_2).unwrap().read().await;
            assert_eq!(room_2.per_thread().len(), 1);
            // … which is thread 2.0.
            assert!(room_2.per_thread().contains_key(thread_id_2_0));
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
            assert!(rooms.get(room_id_0).unwrap().read().await.per_thread().is_empty());
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
            assert_eq!(room_0.per_thread().len(), 1);
            // … which is thread 0.0.
            assert!(room_0.per_thread().contains_key(thread_id_0_0));
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
            assert!(rooms.get(room_id_0).unwrap().read().await.per_thread().is_empty());
        }
    }

    #[async_test]
    async fn test_inputs_task_can_listen_to_room_event_cache() {
        let room_id = owned_room_id!("!r0");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let weak_client = WeakClient::from_client(&client);
        let weak_room = WeakRoom::new(weak_client, room_id.clone());

        let event_cache = client.event_cache();

        let registered_rooms = RwLock::new(HashMap::new());
        let (room_event_cache_generic_update_sender, mut room_event_cache_generic_update_receiver) =
            broadcast::channel(1);
        let (_send_queue_generic_update_sender, mut send_queue_generic_update_receiver) =
            broadcast::channel(1);
        let (latest_event_queue_sender, latest_event_queue_receiver) = mpsc::unbounded_channel();

        // New event cache update, but the `LatestEvents` isn't listening to it.
        {
            room_event_cache_generic_update_sender
                .send(RoomEventCacheGenericUpdate { room_id: room_id.clone() })
                .unwrap();

            // Run the task.
            assert!(
                listen_to_event_cache_and_send_queue_updates(
                    &registered_rooms,
                    &mut room_event_cache_generic_update_receiver,
                    &mut send_queue_generic_update_receiver,
                    &latest_event_queue_sender,
                )
                .await
                .is_continue()
            );

            // No latest event computation has been triggered.
            assert!(latest_event_queue_receiver.is_empty());
        }

        // New event cache update, but this time, the `LatestEvents` is listening to it.
        {
            registered_rooms.write().await.insert(
                room_id.clone(),
                With::inner(RoomLatestEvents::new(weak_room, event_cache)),
            );
            room_event_cache_generic_update_sender
                .send(RoomEventCacheGenericUpdate { room_id: room_id.clone() })
                .unwrap();

            assert!(
                listen_to_event_cache_and_send_queue_updates(
                    &registered_rooms,
                    &mut room_event_cache_generic_update_receiver,
                    &mut send_queue_generic_update_receiver,
                    &latest_event_queue_sender,
                )
                .await
                .is_continue()
            );

            // A latest event computation has been triggered!
            assert!(latest_event_queue_receiver.is_empty().not());
        }
    }

    #[async_test]
    async fn test_inputs_task_can_listen_to_send_queue() {
        let room_id = owned_room_id!("!r0");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let weak_client = WeakClient::from_client(&client);
        let weak_room = WeakRoom::new(weak_client, room_id.clone());

        let event_cache = client.event_cache();

        let registered_rooms = RwLock::new(HashMap::new());

        let (_room_event_cache_generic_update_sender, mut room_event_cache_generic_update_receiver) =
            broadcast::channel(1);
        let (send_queue_generic_update_sender, mut send_queue_generic_update_receiver) =
            broadcast::channel(1);
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
                    &registered_rooms,
                    &mut room_event_cache_generic_update_receiver,
                    &mut send_queue_generic_update_receiver,
                    &latest_event_queue_sender,
                )
                .await
                .is_continue()
            );

            // No latest event computation has been triggered.
            assert!(latest_event_queue_receiver.is_empty());
        }

        // New send queue update, but this time, the `LatestEvents` is listening to it.
        {
            registered_rooms.write().await.insert(
                room_id.clone(),
                With::inner(RoomLatestEvents::new(weak_room, event_cache)),
            );
            send_queue_generic_update_sender
                .send(SendQueueUpdate {
                    room_id: room_id.clone(),
                    update: RoomSendQueueUpdate::SentEvent {
                        transaction_id: OwnedTransactionId::from("txnid1"),
                        event_id: event_id!("$ev1").to_owned(),
                    },
                })
                .unwrap();

            assert!(
                listen_to_event_cache_and_send_queue_updates(
                    &registered_rooms,
                    &mut room_event_cache_generic_update_receiver,
                    &mut send_queue_generic_update_receiver,
                    &latest_event_queue_sender,
                )
                .await
                .is_continue()
            );

            // A latest event computation has been triggered!
            assert!(latest_event_queue_receiver.is_empty().not());
        }
    }

    #[async_test]
    async fn test_inputs_task_stops_when_event_cache_channel_is_closed() {
        let registered_rooms = RwLock::new(HashMap::new());
        let (room_event_cache_generic_update_sender, mut room_event_cache_generic_update_receiver) =
            broadcast::channel(1);
        let (_send_queue_generic_update_sender, mut send_queue_generic_update_receiver) =
            broadcast::channel(1);
        let (latest_event_queue_sender, latest_event_queue_receiver) = mpsc::unbounded_channel();

        // Drop the sender to close the channel.
        drop(room_event_cache_generic_update_sender);

        // Run the task.
        assert!(
            listen_to_event_cache_and_send_queue_updates(
                &registered_rooms,
                &mut room_event_cache_generic_update_receiver,
                &mut send_queue_generic_update_receiver,
                &latest_event_queue_sender,
            )
            .await
            // It breaks!
            .is_break()
        );

        assert!(latest_event_queue_receiver.is_empty());
    }

    #[async_test]
    async fn test_inputs_task_stops_when_send_queue_channel_is_closed() {
        let registered_rooms = RwLock::new(HashMap::new());
        let (_room_event_cache_generic_update_sender, mut room_event_cache_generic_update_receiver) =
            broadcast::channel(1);
        let (send_queue_generic_update_sender, mut send_queue_generic_update_receiver) =
            broadcast::channel(1);
        let (latest_event_queue_sender, latest_event_queue_receiver) = mpsc::unbounded_channel();

        // Drop the sender to close the channel.
        drop(send_queue_generic_update_sender);

        // Run the task.
        assert!(
            listen_to_event_cache_and_send_queue_updates(
                &registered_rooms,
                &mut room_event_cache_generic_update_receiver,
                &mut send_queue_generic_update_receiver,
                &latest_event_queue_sender,
            )
            .await
            // It breaks!
            .is_break()
        );

        assert!(latest_event_queue_receiver.is_empty());
    }

    #[async_test]
    async fn test_latest_event_value_is_updated_via_event_cache() {
        let room_id = owned_room_id!("!r0");
        let user_id = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(user_id).room(&room_id);
        let event_id_2 = event_id!("$ev2");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        // Create the room.
        client.base_client().get_or_create_room(&room_id, RoomState::Joined);

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let latest_events = client.latest_events().await;

        // Subscribe to the latest event values for this room.
        let mut latest_event_stream =
            latest_events.listen_and_subscribe_to_room(&room_id).await.unwrap().unwrap();

        // The stream is pending: no new latest event for the moment.
        assert_pending!(latest_event_stream);

        // Update the event cache with a sync.
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(&room_id)
                    .add_timeline_event(event_factory.text_msg("raclette !").event_id(event_id_2)),
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

        assert_pending!(latest_event_stream);
    }

    #[async_test]
    async fn test_latest_event_value_is_initialized_by_the_event_cache_lazily() {
        let room_id = owned_room_id!("!r0");
        let user_id = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(user_id).room(&room_id);
        let event_id_0 = event_id!("$ev0");

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
                .expect("Could not acquire the event cache lock")
                .as_clean()
                .expect("Could not acquire a clean event cache lock")
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

        let mut latest_event_stream =
            latest_events.listen_and_subscribe_to_room(&room_id).await.unwrap().unwrap();

        // We have a race if the system is busy. Initially, the latest event
        // value is `LatestEventValue::None`, then an Event Cache generic update
        // is broadcasted manually, computing a new `LatestEventValue`. So let's
        // wait on the system to finish this, and assert the final
        // `LatestEventValue`.
        yield_now().await;
        assert_matches!(latest_event_stream.next_now().await, LatestEventValue::Remote(_));

        assert_pending!(latest_event_stream);
    }

    /// This tests a part of
    /// [`test_latest_event_value_is_initialized_by_the_event_cache_lazily`].
    ///
    /// When `RegisteredRooms::room_latest_events` restores a
    /// `LatestEventValue::None` (via `RoomLatestEvents::new`),
    /// a `LatestEventQueueUpdate::EventCache` is broadcasted to compute a
    /// `LatestEventValue` from the Event Cache lazily.
    #[async_test]
    async fn test_latest_event_value_is_initialized_by_the_event_cache_lazily_inner() {
        let room_id_0 = owned_room_id!("!r0");
        let room_id_1 = owned_room_id!("!r1");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        // Create the rooms.
        let room_0 = client.base_client().get_or_create_room(&room_id_0, RoomState::Joined);
        let room_1 = client.base_client().get_or_create_room(&room_id_1, RoomState::Joined);

        // Set up the rooms.
        // `room_0` always has a `LatestEventValue::None` as its the default value.
        let mut room_info_1 = room_0.clone_info();
        room_info_1.set_latest_event(LatestEventValue::LocalIsSending(local_room_message("foo")));
        room_1.set_room_info(room_info_1, Default::default());

        let weak_client = WeakClient::from_client(&client);

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let (latest_event_queue_sender, mut latest_event_queue_receiver) =
            mpsc::unbounded_channel();

        let registered_rooms =
            RegisteredRooms::new(weak_client, event_cache, &latest_event_queue_sender);

        // Room 0 has a `LatestEventValue::None`, a
        // `LatestEventQueueUpdate::EventCache` will be broadcasted.
        {
            let room_latest_events = registered_rooms.for_room(&room_id_0).await.unwrap().unwrap();
            assert_matches!(
                room_latest_events.read().await.for_room().get().await,
                LatestEventValue::None
            );
            assert_matches!(
                latest_event_queue_receiver.recv().await,
                Some(LatestEventQueueUpdate::EventCache { room_id }) => {
                    assert_eq!(room_id, room_id_0);
                }
            );
            assert!(latest_event_queue_receiver.is_empty());
        }

        // Room 1 has a `LatestEventValue::Local*`, a
        // `LatestEventQueueUpdate::EventCache` will NOT be broadcasted.
        {
            let room_latest_events = registered_rooms.for_room(&room_id_1).await.unwrap().unwrap();
            assert_matches!(
                room_latest_events.read().await.for_room().get().await,
                LatestEventValue::LocalIsSending(_)
            );
            assert!(latest_event_queue_receiver.is_empty());
        }
    }
}
