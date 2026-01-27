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

mod builder;

use std::ops::{Deref, DerefMut, Not};

use builder::{BufferOfValuesForLocalEvents, Builder};
use eyeball::{AsyncLock, ObservableWriteGuard, SharedObservable, Subscriber};
pub use matrix_sdk_base::latest_event::{
    LatestEventValue, LocalLatestEventValue, RemoteLatestEventValue,
};
use matrix_sdk_base::{RoomInfoNotableUpdateReasons, StateChanges};
use ruma::{EventId, OwnedEventId, UserId, events::room::power_levels::RoomPowerLevels};
use tracing::{error, info, instrument, warn};

use crate::{event_cache::RoomEventCache, room::WeakRoom, send_queue::RoomSendQueueUpdate};

/// The latest event of a room or a thread.
///
/// Use [`LatestEvent::subscribe`] to get a stream of updates.
#[derive(Debug)]
pub(super) struct LatestEvent {
    /// The room owning this latest event.
    weak_room: WeakRoom,

    /// The thread (if any) owning this latest event.
    _thread_id: Option<OwnedEventId>,

    /// A buffer of the current [`LatestEventValue`]s computed for local events
    /// seen by the send queue. See [`BufferOfValuesForLocalEvents`] to learn
    /// more.
    buffer_of_values_for_local_events: BufferOfValuesForLocalEvents,

    /// The latest event value.
    current_value: SharedObservable<LatestEventValue, AsyncLock>,
}

impl LatestEvent {
    pub fn new(
        weak_room: &WeakRoom,
        thread_id: Option<&EventId>,
    ) -> With<Self, IsLatestEventValueNone> {
        let latest_event_value = match thread_id {
            Some(_thread_id) => LatestEventValue::default(),
            None => weak_room.get().map(|room| room.latest_event()).unwrap_or_default(),
        };
        let is_none = latest_event_value.is_none();

        With {
            result: Self {
                weak_room: weak_room.clone(),
                _thread_id: thread_id.map(ToOwned::to_owned),
                buffer_of_values_for_local_events: BufferOfValuesForLocalEvents::new(),
                current_value: SharedObservable::new_async(latest_event_value),
            },
            with: is_none,
        }
    }

    /// Return a [`Subscriber`] to new values.
    pub async fn subscribe(&self) -> Subscriber<LatestEventValue, AsyncLock> {
        self.current_value.subscribe().await
    }

    #[cfg(test)]
    pub async fn get(&self) -> LatestEventValue {
        self.current_value.get().await
    }

    /// Update the inner latest event value, based on the event cache
    /// (specifically with the [`RoomEventCache`]), if and only if there is no
    /// local latest event value waiting.
    ///
    /// It is only necessary to compute a new [`LatestEventValue`] from the
    /// event cache if there is no [`LatestEventValue`] to be compute from the
    /// send queue. Indeed, anything coming from the send queue has the priority
    /// over the anything coming from the event cache. We believe it provides a
    /// better user experience.
    pub async fn update_with_event_cache(
        &mut self,
        room_event_cache: &RoomEventCache,
        own_user_id: &UserId,
        power_levels: Option<&RoomPowerLevels>,
    ) {
        if self.buffer_of_values_for_local_events.is_empty().not() {
            // At least one `LatestEventValue` exists for local events (i.e. coming from the
            // send queue). In this case, we don't overwrite the current value with a newly
            // computed one from the event cache.
            return;
        }

        let current_value_event_id = self.current_value.read().await.event_id();
        let new_value = Builder::new_remote(
            room_event_cache,
            current_value_event_id,
            own_user_id,
            power_levels,
        )
        .await;

        info!(value = ?new_value, "Computed a remote `LatestEventValue`");

        if let Some(new_value) = new_value {
            self.update(new_value).await;
        }
    }

    /// Update the inner latest event value, based on the send queue
    /// (specifically with the [`RoomSendQueueUpdate`]).
    pub async fn update_with_send_queue(
        &mut self,
        send_queue_update: &RoomSendQueueUpdate,
        room_event_cache: &RoomEventCache,
        own_user_id: &UserId,
        power_levels: Option<&RoomPowerLevels>,
    ) {
        let current_value_event_id = self.current_value.read().await.event_id();
        let new_value = Builder::new_local(
            send_queue_update,
            &mut self.buffer_of_values_for_local_events,
            room_event_cache,
            current_value_event_id,
            own_user_id,
            power_levels,
        )
        .await;

        info!(value = ?new_value, "Computed a local `LatestEventValue`");

        if let Some(new_value) = new_value {
            self.update(new_value).await;
        }
    }

    /// Update [`Self::current_value`], and persist the `new_value` in the
    /// store.
    ///
    /// If the `new_value` is [`LatestEventValue::None`], it is accepted: if the
    /// previous latest event value has been redacted and no other candidate has
    /// been found, we want to latest event value to be `None`, so that it is
    /// erased correctly.
    async fn update(&mut self, new_value: LatestEventValue) {
        // Ideally, we would set `new_value` if and only if it is different from the
        // previous value. However, `LatestEventValue` cannot implement `PartialEq` at
        // the time of writing (2025-12-12). So we are only updating if
        // `LatestEventValue` is not `None` and if the previous value isn't `None`;
        // basically, replacing `None` with `None` will not update the value.
        {
            let mut guard = self.current_value.write().await;
            let previous_value = guard.deref();

            if (previous_value.is_none() && new_value.is_none()).not() {
                ObservableWriteGuard::set(&mut guard, new_value.clone());

                // Release the write guard over the current value before hitting the store.
                drop(guard);

                self.store(new_value).await;
            }
        }
    }

    /// Update the `RoomInfo` associated to this room to set the new
    /// [`LatestEventValue`], and persist it in the
    /// [`StateStore`][matrix_sdk_base::StateStore] (the one from
    /// [`Client::state_store`][crate::Client::state_store]).
    #[instrument(skip_all)]
    async fn store(&mut self, new_value: LatestEventValue) {
        let Some(room) = self.weak_room.get() else {
            warn!(room_id = ?self.weak_room.room_id(), "Cannot store the latest event value because the room cannot be accessed");
            return;
        };

        let client = room.client();

        // Take the state store lock.
        let _state_store_lock = client.base_client().state_store_lock().lock().await;

        // Compute a new `RoomInfo`.
        let mut room_info = room.clone_info();
        room_info.set_latest_event(new_value);

        let mut state_changes = StateChanges::default();
        state_changes.add_room(room_info.clone());

        // Update the `RoomInfo` in the state store.
        if let Err(error) = client.state_store().save_changes(&state_changes).await {
            error!(room_id = ?room.room_id(), ?error, "Failed to save the changes");
        }

        // Update the `RoomInfo` of the room.
        room.set_room_info(room_info, RoomInfoNotableUpdateReasons::LATEST_EVENT);
    }
}

/// Semantic type similar to a tuple where the left part is the main result and
/// the right part is an “attached” value.
pub(super) struct With<T, W> {
    /// The main value.
    result: T,

    /// The “attached” value.
    with: W,
}

impl<T, W> With<T, W> {
    /// Map the main result without changing the “attached” value.
    pub fn map<F, O>(this: With<T, W>, f: F) -> With<O, W>
    where
        F: FnOnce(T) -> O,
    {
        With { result: f(this.result), with: this.with }
    }

    /// Get the main result.
    pub fn inner(this: With<T, W>) -> T {
        this.result
    }

    /// Get a tuple.
    pub fn unzip(this: With<T, W>) -> (T, W) {
        (this.result, this.with)
    }
}

impl<T, W> Deref for With<T, W> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.result
    }
}

impl<T, W> DerefMut for With<T, W> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.result
    }
}

pub(super) type IsLatestEventValueNone = bool;

#[cfg(all(not(target_family = "wasm"), test))]
mod tests_latest_event {
    use std::ops::Not;

    use assert_matches::assert_matches;
    use matrix_sdk_base::{
        RoomInfoNotableUpdateReasons, RoomState,
        linked_chunk::{ChunkIdentifier, LinkedChunkId, Position, Update},
        store::{SerializableEventContent, StoreConfig},
    };
    use matrix_sdk_test::{async_test, event_factory::EventFactory};
    use ruma::{
        MilliSecondsSinceUnixEpoch, OwnedTransactionId, event_id,
        events::{AnyMessageLikeEventContent, room::message::RoomMessageEventContent},
        room_id, user_id,
    };
    use stream_assert::{assert_next_matches, assert_pending};

    use super::{LatestEvent, LatestEventValue, With};
    use crate::{
        client::WeakClient,
        latest_events::local_room_message,
        room::WeakRoom,
        send_queue::{LocalEcho, LocalEchoContent, RoomSendQueue, RoomSendQueueUpdate, SendHandle},
        test_utils::mocks::MatrixMockServer,
    };

    fn new_local_echo_content(
        room_send_queue: &RoomSendQueue,
        transaction_id: &OwnedTransactionId,
        body: &str,
    ) -> LocalEchoContent {
        LocalEchoContent::Event {
            serialized_event: SerializableEventContent::new(
                &AnyMessageLikeEventContent::RoomMessage(RoomMessageEventContent::text_plain(body)),
            )
            .unwrap(),
            send_handle: SendHandle::new(
                room_send_queue.clone(),
                transaction_id.clone(),
                MilliSecondsSinceUnixEpoch::now(),
            ),
            send_error: None,
        }
    }

    #[async_test]
    async fn test_new_loads_from_room_info() {
        let room_id = room_id!("!r0");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let weak_client = WeakClient::from_client(&client);

        // Create the room.
        let room = client.base_client().get_or_create_room(room_id, RoomState::Joined);
        let weak_room = WeakRoom::new(weak_client, room_id.to_owned());

        // First time `LatestEvent` is created: we get `None`.
        {
            let (latest_event, is_none) = With::unzip(LatestEvent::new(&weak_room, None));

            // By default, it's `None`.
            assert_matches!(latest_event.current_value.get().await, LatestEventValue::None);
            assert!(is_none);
        }

        // Set the `RoomInfo`.
        {
            let mut room_info = room.clone_info();
            room_info.set_latest_event(LatestEventValue::LocalIsSending(local_room_message("foo")));
            room.set_room_info(room_info, Default::default());
        }

        // Second time. We get `LocalIsSending` from `RoomInfo`.
        {
            let (latest_event, is_none) = With::unzip(LatestEvent::new(&weak_room, None));

            // By default, it's `None`.
            assert_matches!(
                latest_event.current_value.get().await,
                LatestEventValue::LocalIsSending(_)
            );
            assert!(is_none.not());
        }
    }

    #[async_test]
    async fn test_update_do_not_ignore_none_value() {
        let room_id = room_id!("!r0");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let weak_client = WeakClient::from_client(&client);

        // Create the room.
        client.base_client().get_or_create_room(room_id, RoomState::Joined);
        let weak_room = WeakRoom::new(weak_client, room_id.to_owned());

        // Get a `RoomEventCache`.
        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let mut latest_event = LatestEvent::new(&weak_room, None);

        // First off, check the default value is `None`!
        assert_matches!(latest_event.current_value.get().await, LatestEventValue::None);

        // Second, set a new value.
        latest_event.update(LatestEventValue::LocalIsSending(local_room_message("foo"))).await;

        assert_matches!(
            latest_event.current_value.get().await,
            LatestEventValue::LocalIsSending(_)
        );

        // Finally, set a new `None` value. It must NOT be ignored.
        latest_event.update(LatestEventValue::None).await;

        assert_matches!(latest_event.current_value.get().await, LatestEventValue::None);
    }

    #[async_test]
    async fn test_update_ignore_none_if_previous_value_is_none() {
        let room_id = room_id!("!r0");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let weak_client = WeakClient::from_client(&client);

        // Create the room.
        client.base_client().get_or_create_room(room_id, RoomState::Joined);
        let weak_room = WeakRoom::new(weak_client, room_id.to_owned());

        let mut latest_event = LatestEvent::new(&weak_room, None);

        let mut stream = latest_event.subscribe().await;

        assert_pending!(stream);

        // Set a non-`None` value.
        latest_event.update(LatestEventValue::LocalIsSending(local_room_message("foo"))).await;
        // We get it.
        assert_next_matches!(stream, LatestEventValue::LocalIsSending(_));

        // Set a `None` value.
        latest_event.update(LatestEventValue::None).await;
        // We get it.
        assert_next_matches!(stream, LatestEventValue::None);

        // Set a `None` value, again!
        latest_event.update(LatestEventValue::None).await;
        // We get it? No!
        assert_pending!(stream);

        // Set a `None` value, again, and again!
        latest_event.update(LatestEventValue::None).await;
        // No means No!
        assert_pending!(stream);

        // Set a non-`None` value.
        latest_event.update(LatestEventValue::LocalIsSending(local_room_message("bar"))).await;
        // We get it. Oof.
        assert_next_matches!(stream, LatestEventValue::LocalIsSending(_));

        assert_pending!(stream);
    }

    #[async_test]
    async fn test_local_has_priority_over_remote() {
        let room_id = room_id!("!r0").to_owned();
        let user_id = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(user_id).room(&room_id);

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        client.base_client().get_or_create_room(&room_id, RoomState::Joined);
        let room = client.get_room(&room_id).unwrap();
        let weak_room = WeakRoom::new(WeakClient::from_client(&client), room_id.clone());

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        // Fill the event cache with one event.
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
                        items: vec![event_factory.text_msg("A").event_id(event_id!("$ev0")).into()],
                    },
                ],
            )
            .await
            .unwrap();

        let (room_event_cache, _) = event_cache.for_room(&room_id).await.unwrap();

        let send_queue = client.send_queue();
        let room_send_queue = send_queue.for_room(room);

        let mut latest_event = LatestEvent::new(&weak_room, None);

        // First, let's create a `LatestEventValue` from the event cache. It must work.
        {
            latest_event.update_with_event_cache(&room_event_cache, user_id, None).await;

            assert_matches!(latest_event.current_value.get().await, LatestEventValue::Remote(_));
        }

        // Second, let's create a `LatestEventValue` from the send queue. It
        // must overwrite the current `LatestEventValue`.
        let transaction_id = OwnedTransactionId::from("txnid0");

        {
            let content = new_local_echo_content(&room_send_queue, &transaction_id, "B");

            let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                transaction_id: transaction_id.clone(),
                content,
            });

            latest_event.update_with_send_queue(&update, &room_event_cache, user_id, None).await;

            assert_matches!(
                latest_event.current_value.get().await,
                LatestEventValue::LocalIsSending(_)
            );
        }

        // Third, let's create a `LatestEventValue` from the event cache.
        // Nothing must happen, it cannot overwrite the current
        // `LatestEventValue` because the local event isn't sent yet.
        {
            latest_event.update_with_event_cache(&room_event_cache, user_id, None).await;

            assert_matches!(
                latest_event.current_value.get().await,
                LatestEventValue::LocalIsSending(_)
            );
        }

        // Fourth, let's a `LatestEventValue` from the send queue. It must stay the
        // same, but now the local event is sent.
        {
            let update = RoomSendQueueUpdate::SentEvent {
                transaction_id,
                event_id: event_id!("$ev1").to_owned(),
            };

            latest_event.update_with_send_queue(&update, &room_event_cache, user_id, None).await;

            assert_matches!(
                latest_event.current_value.get().await,
                LatestEventValue::LocalHasBeenSent { .. }
            );
        }

        // Finally, let's create a `LatestEventValue` from the event cache. _Now_ it's
        // possible, because there is no more local events.
        {
            latest_event.update_with_event_cache(&room_event_cache, user_id, None).await;

            assert_matches!(latest_event.current_value.get().await, LatestEventValue::Remote(_));
        }
    }

    #[async_test]
    async fn test_store_latest_event_value() {
        let room_id = room_id!("!r0").to_owned();
        let user_id = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(user_id).room(&room_id);

        let server = MatrixMockServer::new().await;

        let store_config = StoreConfig::new("cross-process-lock-holder".to_owned());

        // Load the client for the first time, and run some operations.
        {
            let client = server
                .client_builder()
                .on_builder(|builder| builder.store_config(store_config.clone()))
                .build()
                .await;
            let mut room_info_notable_update_receiver = client.room_info_notable_update_receiver();
            let room = client.base_client().get_or_create_room(&room_id, RoomState::Joined);
            let weak_room = WeakRoom::new(WeakClient::from_client(&client), room_id.clone());

            let event_cache = client.event_cache();
            event_cache.subscribe().unwrap();

            // Fill the event cache with one event.
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
                                event_factory.text_msg("A").event_id(event_id!("$ev0")).into(),
                            ],
                        },
                    ],
                )
                .await
                .unwrap();

            let (room_event_cache, _) = event_cache.for_room(&room_id).await.unwrap();

            // Check there is no `LatestEventValue` for the moment.
            {
                let latest_event = room.latest_event();

                assert_matches!(latest_event, LatestEventValue::None);
            }

            // Generate a new `LatestEventValue`.
            {
                let mut latest_event = LatestEvent::new(&weak_room, None);
                latest_event.update_with_event_cache(&room_event_cache, user_id, None).await;

                assert_matches!(
                    latest_event.current_value.get().await,
                    LatestEventValue::Remote(_)
                );
            }

            // We see the `RoomInfoNotableUpdateReasons`.
            {
                let update = room_info_notable_update_receiver.recv().await.unwrap();

                assert_eq!(update.room_id, room_id);
                assert!(update.reasons.contains(RoomInfoNotableUpdateReasons::LATEST_EVENT));
            }

            // Check it's in the `RoomInfo` and in `Room`.
            {
                let latest_event = room.latest_event();

                assert_matches!(latest_event, LatestEventValue::Remote(_));
            }
        }

        // Reload the client with the same store config, and see the `LatestEventValue`
        // is inside the `RoomInfo`.
        {
            let client = server
                .client_builder()
                .on_builder(|builder| builder.store_config(store_config))
                .build()
                .await;
            let room = client.get_room(&room_id).unwrap();
            let latest_event = room.latest_event();

            assert_matches!(latest_event, LatestEventValue::Remote(_));
        }
    }
}
