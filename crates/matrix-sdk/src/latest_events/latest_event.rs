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
    iter::once,
    ops::{Deref, DerefMut, Not},
};

use eyeball::{AsyncLock, ObservableWriteGuard, SharedObservable, Subscriber};
pub use matrix_sdk_base::latest_event::{
    LatestEventValue, LocalLatestEventValue, RemoteLatestEventValue,
};
use matrix_sdk_base::{
    RoomInfoNotableUpdateReasons, StateChanges, deserialized_responses::TimelineEvent,
    store::SerializableEventContent, timer,
};
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, TransactionId, UserId,
    events::{
        AnyMessageLikeEventContent, AnySyncStateEvent, AnySyncTimelineEvent, SyncStateEvent,
        relation::Replacement,
        room::{
            member::MembershipState,
            message::{MessageType, Relation, RoomMessageEventContent},
            power_levels::RoomPowerLevels,
        },
    },
};
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

    /// A buffer of the current [`LatestEventValue`] computed for local events
    /// seen by the send queue. See [`LatestEventValuesForLocalEvents`] to learn
    /// more.
    buffer_of_values_for_local_events: LatestEventValuesForLocalEvents,

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
                buffer_of_values_for_local_events: LatestEventValuesForLocalEvents::new(),
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

        let new_value =
            LatestEventValueBuilder::new_remote(room_event_cache, own_user_id, power_levels).await;

        self.update(new_value).await;
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
        let new_value = LatestEventValueBuilder::new_local(
            send_queue_update,
            &mut self.buffer_of_values_for_local_events,
            room_event_cache,
            own_user_id,
            power_levels,
        )
        .await;

        self.update(new_value).await;
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

        // Compute a new `RoomInfo`.
        let mut room_info = room.clone_info();
        room_info.set_latest_event(new_value);

        let mut state_changes = StateChanges::default();
        state_changes.add_room(room_info.clone());

        let client = room.client();

        // Take the state store lock.
        let _state_store_lock = client.base_client().state_store_lock().lock().await;

        // Update the `RoomInfo` in the state store.
        if let Err(error) = client.state_store().save_changes(&state_changes).await {
            error!(room_id = ?room.room_id(), ?error, "Failed to save the changes");
        }

        // Update the `RoomInfo` of the room.
        room.set_room_info(room_info, RoomInfoNotableUpdateReasons::LATEST_EVENT);
    }
}

pub(super) struct With<T, W> {
    result: T,
    with: W,
}

impl<T, W> With<T, W> {
    pub fn map<F, O>(this: With<T, W>, f: F) -> With<O, W>
    where
        F: FnOnce(T) -> O,
    {
        With { result: f(this.result), with: this.with }
    }

    pub fn inner(this: With<T, W>) -> T {
        this.result
    }

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
        store::StoreConfig,
    };
    use matrix_sdk_test::{async_test, event_factory::EventFactory};
    use ruma::{
        MilliSecondsSinceUnixEpoch, OwnedTransactionId, event_id,
        events::{AnyMessageLikeEventContent, room::message::RoomMessageEventContent},
        room_id, user_id,
    };
    use stream_assert::{assert_next_matches, assert_pending};

    use super::{
        LatestEvent, LatestEventValue, LocalLatestEventValue, SerializableEventContent, With,
    };
    use crate::{
        client::WeakClient,
        room::WeakRoom,
        send_queue::{LocalEcho, LocalEchoContent, RoomSendQueue, RoomSendQueueUpdate, SendHandle},
        test_utils::mocks::MatrixMockServer,
    };

    fn local_room_message(body: &str) -> LocalLatestEventValue {
        LocalLatestEventValue {
            timestamp: MilliSecondsSinceUnixEpoch::now(),
            content: SerializableEventContent::new(&AnyMessageLikeEventContent::RoomMessage(
                RoomMessageEventContent::text_plain(body),
            ))
            .unwrap(),
        }
    }

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
                LatestEventValue::LocalIsSending(_)
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

/// A builder of [`LatestEventValue`]s.
struct LatestEventValueBuilder;

impl LatestEventValueBuilder {
    /// Create a new [`LatestEventValue::Remote`].
    async fn new_remote(
        room_event_cache: &RoomEventCache,
        own_user_id: &UserId,
        power_levels: Option<&RoomPowerLevels>,
    ) -> LatestEventValue {
        let _timer = timer!(
            tracing::Level::INFO,
            format!("`LatestEventValueBuilder::new_remote` for {:?}", room_event_cache.room_id())
        );

        let value = if let Ok(Some(event)) = room_event_cache
            .rfind_map_event_in_memory_by(|event, previous_event_id| {
                filter_timeline_event(event, previous_event_id, own_user_id, power_levels)
                    .then(|| event.clone())
            })
            .await
        {
            LatestEventValue::Remote(event)
        } else {
            LatestEventValue::default()
        };

        info!(?value, "Computed a remote `LatestEventValue`");

        value
    }

    /// Create a new [`LatestEventValue::LocalIsSending`] or
    /// [`LatestEventValue::LocalCannotBeSent`].
    async fn new_local(
        send_queue_update: &RoomSendQueueUpdate,
        buffer_of_values_for_local_events: &mut LatestEventValuesForLocalEvents,
        room_event_cache: &RoomEventCache,
        own_user_id: &UserId,
        power_levels: Option<&RoomPowerLevels>,
    ) -> LatestEventValue {
        use crate::send_queue::{LocalEcho, LocalEchoContent};

        let _timer = timer!(
            tracing::Level::INFO,
            format!("`LatestEventValueBuilder::new_local` for {:?}", room_event_cache.room_id())
        );

        let value = match send_queue_update {
            // A new local event is being sent.
            //
            // Let's create the `LatestEventValue` and push it in the buffer of values.
            RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                transaction_id,
                content: local_echo_content,
            }) => match local_echo_content {
                LocalEchoContent::Event { serialized_event: serialized_event_content, .. } => {
                    match serialized_event_content.deserialize() {
                        Ok(content) => {
                            if filter_any_message_like_event_content(content, None) {
                                let local_value = LocalLatestEventValue {
                                    timestamp: MilliSecondsSinceUnixEpoch::now(),
                                    content: serialized_event_content.clone(),
                                };

                                // If a local previous `LatestEventValue` exists and has been marked
                                // as “cannot be sent”, it means the new `LatestEventValue` must
                                // also be marked as “cannot be sent”.
                                let value = if let Some(LatestEventValue::LocalCannotBeSent(_)) =
                                    buffer_of_values_for_local_events.last()
                                {
                                    LatestEventValue::LocalCannotBeSent(local_value)
                                } else {
                                    LatestEventValue::LocalIsSending(local_value)
                                };

                                buffer_of_values_for_local_events
                                    .push(transaction_id.to_owned(), value.clone());

                                value
                            } else {
                                LatestEventValue::None
                            }
                        }

                        Err(error) => {
                            error!(
                                ?error,
                                "Failed to deserialize an event from `RoomSendQueueUpdate::NewLocalEvent`"
                            );

                            LatestEventValue::None
                        }
                    }
                }

                LocalEchoContent::React { .. } => LatestEventValue::None,
            },

            // A local event has been cancelled before being sent.
            //
            // Remove the calculated `LatestEventValue` from the buffer of values, and return the
            // last `LatestEventValue` or calculate a new one.
            RoomSendQueueUpdate::CancelledLocalEvent { transaction_id } => {
                if let Some(position) = buffer_of_values_for_local_events.position(transaction_id) {
                    buffer_of_values_for_local_events.remove(position);
                }

                Self::new_local_or_remote(
                    buffer_of_values_for_local_events,
                    room_event_cache,
                    own_user_id,
                    power_levels,
                )
                .await
            }

            // A local event has successfully been sent!
            //
            // Mark all “cannot be sent” values as “is sending” after the one matching
            // `transaction_id`. Indeed, if an event has been sent, it means the send queue is
            // working, so if any value has been marked as “cannot be sent”, it must be marked as
            // “is sending”. Then, remove the calculated `LatestEventValue` from the buffer of
            // values. Finally, return the last `LatestEventValue` or calculate a new
            // one.
            RoomSendQueueUpdate::SentEvent { transaction_id, .. } => {
                let position =
                    buffer_of_values_for_local_events.mark_is_sending_after(transaction_id);

                // First, compute the new value. Then we remove the sent local event from the
                // buffer.
                //
                // Why in this order? Because even if the Send Queue inserts the sent event in
                // the Event Cache, the Send Queue sends its updates (this update) before the
                // insertion in the Event Cache. Thus, the Event Cache isn't yet aware of the
                // newly sent event yet at this time of execution. Computing the
                // `LatestEventValue` from the local ones ensure no hide-and-seek game with the
                // event before it lands in the Event Cache.
                let value = Self::new_local_or_remote(
                    buffer_of_values_for_local_events,
                    room_event_cache,
                    own_user_id,
                    power_levels,
                )
                .await;

                if let Some(position) = position {
                    buffer_of_values_for_local_events.remove(position);
                }

                value
            }

            // A local event has been replaced by another one.
            //
            // Replace the latest event value matching `transaction_id` in the buffer if it exists
            // (note: it should!), and return the last `LatestEventValue` or calculate a new one.
            RoomSendQueueUpdate::ReplacedLocalEvent {
                transaction_id,
                new_content: new_serialized_event_content,
            } => {
                if let Some(position) = buffer_of_values_for_local_events.position(transaction_id) {
                    match new_serialized_event_content.deserialize() {
                        Ok(content) => {
                            if filter_any_message_like_event_content(content, None) {
                                buffer_of_values_for_local_events.replace_content(
                                    position,
                                    new_serialized_event_content.clone(),
                                );
                            } else {
                                buffer_of_values_for_local_events.remove(position);
                            }
                        }

                        Err(error) => {
                            error!(
                                ?error,
                                "Failed to deserialize an event from `RoomSendQueueUpdate::ReplacedLocalEvent`"
                            );

                            return LatestEventValue::None;
                        }
                    }
                }

                Self::new_local_or_remote(
                    buffer_of_values_for_local_events,
                    room_event_cache,
                    own_user_id,
                    power_levels,
                )
                .await
            }

            // An error has occurred.
            //
            // Mark the latest event value matching `transaction_id`, and all its following values,
            // as “cannot be sent”.
            RoomSendQueueUpdate::SendError { transaction_id, .. } => {
                buffer_of_values_for_local_events.mark_cannot_be_sent_from(transaction_id);

                Self::new_local_or_remote(
                    buffer_of_values_for_local_events,
                    room_event_cache,
                    own_user_id,
                    power_levels,
                )
                .await
            }

            // A local event has been unwedged and sending is being retried.
            //
            // Mark the latest event value matching `transaction_id`, and all its following values,
            // as “is sending”.
            RoomSendQueueUpdate::RetryEvent { transaction_id } => {
                buffer_of_values_for_local_events.mark_is_sending_from(transaction_id);

                Self::new_local_or_remote(
                    buffer_of_values_for_local_events,
                    room_event_cache,
                    own_user_id,
                    power_levels,
                )
                .await
            }

            // A media upload has made progress.
            //
            // Nothing to do here.
            RoomSendQueueUpdate::MediaUpload { .. } => LatestEventValue::None,
        };

        info!(?value, "Computed a local `LatestEventValue`");

        value
    }

    /// Get the last [`LatestEventValue`] from the local latest event values if
    /// any, or create a new [`LatestEventValue`] from the remote events.
    ///
    /// If the buffer of latest event values is not empty, let's return the last
    /// one. Otherwise, it means we no longer have any local event: let's
    /// fallback on remote event!
    async fn new_local_or_remote(
        buffer_of_values_for_local_events: &mut LatestEventValuesForLocalEvents,
        room_event_cache: &RoomEventCache,
        own_user_id: &UserId,
        power_levels: Option<&RoomPowerLevels>,
    ) -> LatestEventValue {
        if let Some(value) = buffer_of_values_for_local_events.last() {
            value.clone()
        } else {
            Self::new_remote(room_event_cache, own_user_id, power_levels).await
        }
    }
}

/// A buffer of the current [`LatestEventValue`] computed for local events
/// seen by the send queue. It is used by
/// [`LatestEvent::buffer_of_values_for_local_events`].
///
/// The system does only receive [`RoomSendQueueUpdate`]s. It's not designed to
/// iterate over local events in the send queue when a local event is changed
/// (cancelled, or updated for example). That's why we keep our own buffer here.
/// Imagine the system receives 4 [`RoomSendQueueUpdate`]:
///
/// 1. [`RoomSendQueueUpdate::NewLocalEvent`]: new local event,
/// 2. [`RoomSendQueueUpdate::NewLocalEvent`]: new local event,
/// 3. [`RoomSendQueueUpdate::ReplacedLocalEvent`]: replaced the first local
///    event,
/// 4. [`RoomSendQueueUpdate::CancelledLocalEvent`]: cancelled the second local
///    event.
///
/// `NewLocalEvent`s will trigger the computation of new
/// `LatestEventValue`s, but `CancelledLocalEvent` for example doesn't hold
/// any information to compute a new `LatestEventValue`, so we need to
/// remember the previous values, until the local events are sent and
/// removed from this buffer.
///
/// Another reason why we need a buffer is to handle wedged local event. Imagine
/// the system receives 3 [`RoomSendQueueUpdate`]:
///
/// 1. [`RoomSendQueueUpdate::NewLocalEvent`]: new local event,
/// 2. [`RoomSendQueueUpdate::NewLocalEvent`]: new local event,
/// 3. [`RoomSendQueueUpdate::SendError`]: the first local event has failed to
///    be sent.
///
/// Because a `SendError` is received (targeting the first `NewLocalEvent`), the
/// send queue is stopped. However, the `LatestEventValue` targets the second
/// `NewLocalEvent`. The system must consider that when a local event is wedged,
/// all the following local events must also be marked as “cannot be sent”. And
/// vice versa, when the send queue is able to send an event again, all the
/// following local events must be marked as “is sending”.
///
/// This type isolates a couple of methods designed to manage these specific
/// behaviours.
#[derive(Debug)]
struct LatestEventValuesForLocalEvents {
    buffer: Vec<(OwnedTransactionId, LatestEventValue)>,
}

impl LatestEventValuesForLocalEvents {
    /// Create a new [`LatestEventValuesForLocalEvents`].
    fn new() -> Self {
        Self { buffer: Vec::with_capacity(2) }
    }

    /// Check the buffer is empty.
    fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    /// Get the last [`LatestEventValue`].
    fn last(&self) -> Option<&LatestEventValue> {
        self.buffer.last().map(|(_, value)| value)
    }

    /// Find the position of the [`LatestEventValue`] matching `transaction_id`.
    fn position(&self, transaction_id: &TransactionId) -> Option<usize> {
        self.buffer
            .iter()
            .position(|(transaction_id_candidate, _)| transaction_id == transaction_id_candidate)
    }

    /// Push a new [`LatestEventValue`].
    ///
    /// # Panics
    ///
    /// Panics if `value` is not of kind [`LatestEventValue::LocalIsSending`] or
    /// [`LatestEventValue::LocalCannotBeSent`].
    fn push(&mut self, transaction_id: OwnedTransactionId, value: LatestEventValue) {
        assert!(
            matches!(
                value,
                LatestEventValue::LocalIsSending(_) | LatestEventValue::LocalCannotBeSent(_)
            ),
            "`value` must be either `LocalIsSending` or `LocalCannotBeSent`"
        );

        self.buffer.push((transaction_id, value));
    }

    /// Replace the content of the [`LatestEventValue`] at position `position`.
    ///
    /// # Panics
    ///
    /// Panics if:
    /// - `position` is strictly greater than buffer's length,
    /// - the [`LatestEventValue`] is not of kind
    ///   [`LatestEventValue::LocalIsSending`] or
    ///   [`LatestEventValue::LocalCannotBeSent`].
    fn replace_content(&mut self, position: usize, new_content: SerializableEventContent) {
        let (_, value) = self.buffer.get_mut(position).expect("`position` must be valid");

        match value {
            LatestEventValue::LocalIsSending(LocalLatestEventValue { content, .. }) => {
                *content = new_content;
            }

            LatestEventValue::LocalCannotBeSent(LocalLatestEventValue { content, .. }) => {
                *content = new_content;
            }

            _ => panic!("`value` must be either `LocalIsSending` or `LocalCannotBeSent`"),
        }
    }

    /// Remove the [`LatestEventValue`] at position `position`.
    ///
    /// # Panics
    ///
    /// Panics if `position` is strictly greater than buffer's length.
    fn remove(&mut self, position: usize) -> (OwnedTransactionId, LatestEventValue) {
        self.buffer.remove(position)
    }

    /// Mark the `LatestEventValue` matching `transaction_id`, and all the
    /// following values, as “cannot be sent”.
    fn mark_cannot_be_sent_from(&mut self, transaction_id: &TransactionId) {
        let mut values = self.buffer.iter_mut();

        if let Some(first_value_to_wedge) = values
            .by_ref()
            .find(|(transaction_id_candidate, _)| transaction_id == transaction_id_candidate)
        {
            // Iterate over the found value and the following ones.
            for (_, value_to_wedge) in once(first_value_to_wedge).chain(values) {
                if let LatestEventValue::LocalIsSending(content) = value_to_wedge {
                    *value_to_wedge = LatestEventValue::LocalCannotBeSent(content.clone());
                }
            }
        }
    }

    /// Mark the `LatestEventValue` matching `transaction_id`, and all the
    /// following values, as “is sending”.
    fn mark_is_sending_from(&mut self, transaction_id: &TransactionId) {
        let mut values = self.buffer.iter_mut();

        if let Some(first_value_to_unwedge) = values
            .by_ref()
            .find(|(transaction_id_candidate, _)| transaction_id == transaction_id_candidate)
        {
            // Iterate over the found value and the following ones.
            for (_, value_to_unwedge) in once(first_value_to_unwedge).chain(values) {
                if let LatestEventValue::LocalCannotBeSent(content) = value_to_unwedge {
                    *value_to_unwedge = LatestEventValue::LocalIsSending(content.clone());
                }
            }
        }
    }

    /// Mark all the following values after the `LatestEventValue` matching
    /// `transaction_id` as “is sending”.
    ///
    /// Note that contrary to [`Self::mark_is_sending_from`], the
    /// `LatestEventValue` is untouched. However, its position is returned
    /// (if any).
    fn mark_is_sending_after(&mut self, transaction_id: &TransactionId) -> Option<usize> {
        let mut values = self.buffer.iter_mut();

        if let Some(position) = values
            .by_ref()
            .position(|(transaction_id_candidate, _)| transaction_id == transaction_id_candidate)
        {
            // Iterate over all values after the found one.
            for (_, value_to_unwedge) in values {
                if let LatestEventValue::LocalCannotBeSent(content) = value_to_unwedge {
                    *value_to_unwedge = LatestEventValue::LocalIsSending(content.clone());
                }
            }

            Some(position)
        } else {
            None
        }
    }
}

fn filter_timeline_event(
    event: &TimelineEvent,
    previous_event: Option<&TimelineEvent>,
    own_user_id: &UserId,
    power_levels: Option<&RoomPowerLevels>,
) -> bool {
    // Cast the event into an `AnySyncTimelineEvent`. If deserializing fails, we
    // ignore the event.
    let event = match event.raw().deserialize() {
        Ok(event) => event,
        Err(error) => {
            error!(
                ?error,
                "Failed to deserialize the event when looking for a suitable latest event"
            );

            return false;
        }
    };

    match event {
        AnySyncTimelineEvent::MessageLike(message_like_event) => {
            match message_like_event.original_content() {
                Some(any_message_like_event_content) => filter_any_message_like_event_content(
                    any_message_like_event_content,
                    previous_event,
                ),

                // The event has been redacted.
                None => false,
            }
        }

        AnySyncTimelineEvent::State(state) => {
            filter_any_sync_state_event(state, own_user_id, power_levels)
        }
    }
}

fn filter_any_message_like_event_content(
    event: AnyMessageLikeEventContent,
    previous_event: Option<&TimelineEvent>,
) -> bool {
    match event {
        // `m.room.message`
        AnyMessageLikeEventContent::RoomMessage(RoomMessageEventContent {
            msgtype,
            relates_to,
            ..
        }) => {
            // Don't show incoming verification requests.
            if let MessageType::VerificationRequest(_) = msgtype {
                return false;
            }

            // Not all relations are accepted. Let's filter them.
            match relates_to {
                Some(Relation::Replacement(Replacement { event_id, .. })) => {
                    // If the edit relates to the immediate previous event, this is an acceptable
                    // latest event candidate, otherwise let's ignore it.
                    Some(event_id) == previous_event.and_then(|event| event.event_id())
                }

                _ => true,
            }
        }

        // `org.matrix.msc3381.poll.start`
        // `m.call.invite`
        // `m.rtc.notification`
        // `m.sticker`
        AnyMessageLikeEventContent::UnstablePollStart(_)
        | AnyMessageLikeEventContent::CallInvite(_)
        | AnyMessageLikeEventContent::RtcNotification(_)
        | AnyMessageLikeEventContent::Sticker(_) => true,

        // `m.room.redaction`
        // `m.room.encrypted`
        AnyMessageLikeEventContent::RoomRedaction(_)
        | AnyMessageLikeEventContent::RoomEncrypted(_) => {
            // These events are **explicitly** not suitable.
            false
        }

        // Everything else is considered not suitable.
        _ => false,
    }
}

fn filter_any_sync_state_event(
    event: AnySyncStateEvent,
    own_user_id: &UserId,
    power_levels: Option<&RoomPowerLevels>,
) -> bool {
    match event {
        AnySyncStateEvent::RoomMember(member) => {
            match member.membership() {
                MembershipState::Knock => {
                    let can_accept_or_decline_knocks = match power_levels {
                        Some(room_power_levels) => {
                            room_power_levels.user_can_invite(own_user_id)
                                || room_power_levels
                                    .user_can_kick_user(own_user_id, member.state_key())
                        }
                        None => false,
                    };

                    // The current user can act on the knock changes, so they should be
                    // displayed
                    if can_accept_or_decline_knocks {
                        // We can only decide whether the user can accept or decline knocks if the
                        // event isn't redacted.
                        return matches!(member, SyncStateEvent::Original(_));
                    }

                    false
                }

                MembershipState::Invite => {
                    // The current _is_ invited (not someone else).
                    match member {
                        // We can only decide whether the user is invited if the event isn't
                        // redacted.
                        SyncStateEvent::Original(state) => state.state_key.deref() == own_user_id,

                        _ => false,
                    }
                }

                _ => false,
            }
        }

        _ => false,
    }
}

#[cfg(test)]
mod tests_latest_event_content {
    use std::ops::Not;

    use matrix_sdk_test::event_factory::EventFactory;
    use ruma::{
        event_id,
        events::{room::message::RoomMessageEventContent, rtc::notification::NotificationType},
        owned_user_id, user_id,
    };

    use super::filter_timeline_event;

    macro_rules! assert_latest_event_content {
        ( event | $event_factory:ident | $event_builder:block
          is a candidate ) => {
            assert_latest_event_content!(@_ | $event_factory | $event_builder, true);
        };

        ( event | $event_factory:ident | $event_builder:block
          is not a candidate ) => {
            assert_latest_event_content!(@_ | $event_factory | $event_builder, false);
        };

        ( @_ | $event_factory:ident | $event_builder:block, $expect:literal ) => {
            let user_id = user_id!("@mnt_io:matrix.org");
            let event_factory = EventFactory::new().sender(user_id);
            let event = {
                let $event_factory = event_factory;
                $event_builder
            };

            assert_eq!(filter_timeline_event(&event, None, user_id!("@mnt_io:matrix.org"), None), $expect );
        };
    }

    #[test]
    fn test_room_message() {
        assert_latest_event_content!(
            event | event_factory | { event_factory.text_msg("hello").into_event() }
            is a candidate
        );
    }

    #[test]
    fn test_room_message_replacement() {
        let user_id = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(user_id);
        let event = event_factory
            .text_msg("bonjour")
            .edit(event_id!("$ev0"), RoomMessageEventContent::text_plain("hello").into())
            .into_event();

        // Without a previous event.
        //
        // This is an edge case where either the event cache has been emptied and only
        // the edit is received via the sync for example, or either the previous event
        // is part of another chunk that is not loaded in memory yet. In this case,
        // let's not consider the event as a `LatestEventValue` candidate.
        {
            let previous_event_id = None;

            assert!(filter_timeline_event(&event, previous_event_id, user_id, None).not());
        }

        // With a previous event, but not the one being replaced.
        {
            let previous_event =
                Some(event_factory.text_msg("no!").event_id(event_id!("$ev1")).into_event());

            assert!(filter_timeline_event(&event, previous_event.as_ref(), user_id, None).not());
        }

        // With a previous event, and that's the one being replaced!
        {
            let previous_event =
                Some(event_factory.text_msg("hello").event_id(event_id!("$ev0")).into_event());

            assert!(filter_timeline_event(&event, previous_event.as_ref(), user_id, None));
        }
    }

    #[test]
    fn test_redaction() {
        assert_latest_event_content!(
            event | event_factory | { event_factory.redaction(event_id!("$ev0")).into_event() }
            is not a candidate
        );
    }

    #[test]
    fn test_redacted() {
        assert_latest_event_content!(
            event | event_factory | {
                event_factory
                    .redacted(
                        user_id!("@mnt_io:matrix.org"),
                        ruma::events::room::message::RedactedRoomMessageEventContent::new(),
                    )
                    .event_id(event_id!("$ev0"))
                    .into_event()
            }
            is not a candidate
        );
    }

    #[test]
    fn test_poll() {
        assert_latest_event_content!(
            event | event_factory | {
                event_factory
                    .poll_start("the people need to know", "comté > gruyère", vec!["yes", "oui"])
                    .into_event()
            }
            is a candidate
        );
    }

    #[test]
    fn test_call_invite() {
        assert_latest_event_content!(
            event | event_factory | {
                event_factory
                    .call_invite(
                        ruma::OwnedVoipId::from("vvooiipp".to_owned()),
                        ruma::UInt::from(1234u32),
                        ruma::events::call::SessionDescription::new(
                            "type".to_owned(),
                            "sdp".to_owned(),
                        ),
                        ruma::VoipVersionId::V1,
                    )
                    .into_event()
            }
            is a candidate
        );
    }

    #[test]
    fn test_rtc_notification() {
        assert_latest_event_content!(
            event | event_factory | {
                event_factory
                     .rtc_notification(
                        NotificationType::Ring,
                    )
                    .mentions(vec![owned_user_id!("@alice:server.name")])
                    .relates_to_membership_state_event(ruma::OwnedEventId::try_from("$abc:server.name").unwrap())
                    .lifetime(60)
                    .into_event()
            }
            is a candidate
        );
    }

    #[test]
    fn test_sticker() {
        assert_latest_event_content!(
            event | event_factory | {
                event_factory
                    .sticker(
                        "wink wink",
                        ruma::events::room::ImageInfo::new(),
                        ruma::OwnedMxcUri::from("mxc://foo/bar"),
                    )
                    .into_event()
            }
            is a candidate
        );
    }

    #[test]
    fn test_encrypted_room_message() {
        assert_latest_event_content!(
            event | event_factory | {
                event_factory
                    .event(ruma::events::room::encrypted::RoomEncryptedEventContent::new(
                        ruma::events::room::encrypted::EncryptedEventScheme::MegolmV1AesSha2(
                            ruma::events::room::encrypted::MegolmV1AesSha2ContentInit {
                                ciphertext: "cipher".to_owned(),
                                sender_key: "sender_key".to_owned(),
                                device_id: "device_id".into(),
                                session_id: "session_id".to_owned(),
                            }
                            .into(),
                        ),
                        None,
                    ))
                    .into_event()
            }
            is not a candidate
        );
    }

    #[test]
    fn test_reaction() {
        // Take a random message-like event.
        assert_latest_event_content!(
            event | event_factory | { event_factory.reaction(event_id!("$ev0"), "+1").into_event() }
            is not a candidate
        );
    }

    #[test]
    fn test_state_event() {
        assert_latest_event_content!(
            event | event_factory | { event_factory.room_topic("new room topic").into_event() }
            is not a candidate
        );
    }

    #[test]
    fn test_knocked_state_event_without_power_levels() {
        assert_latest_event_content!(
            event | event_factory | {
                event_factory
                    .member(user_id!("@other_mnt_io:server.name"))
                    .membership(ruma::events::room::member::MembershipState::Knock)
                    .into_event()
            }
            is not a candidate
        );
    }

    #[test]
    fn test_knocked_state_event_with_power_levels() {
        use ruma::{
            events::room::{
                member::MembershipState,
                power_levels::{RoomPowerLevels, RoomPowerLevelsSource},
            },
            room_version_rules::AuthorizationRules,
        };

        let user_id = user_id!("@mnt_io:matrix.org");
        let other_user_id = user_id!("@other_mnt_io:server.name");
        let event_factory = EventFactory::new().sender(user_id);
        let event =
            event_factory.member(other_user_id).membership(MembershipState::Knock).into_event();

        let mut room_power_levels =
            RoomPowerLevels::new(RoomPowerLevelsSource::None, &AuthorizationRules::V1, []);
        room_power_levels.users.insert(user_id.to_owned(), 5.into());
        room_power_levels.users.insert(other_user_id.to_owned(), 4.into());

        // Cannot accept. Cannot decline.
        {
            room_power_levels.invite = 10.into();
            room_power_levels.kick = 10.into();
            assert!(
                filter_timeline_event(&event, None, user_id, Some(&room_power_levels)).not(),
                "cannot accept, cannot decline",
            );
        }

        // Can accept. Cannot decline.
        {
            room_power_levels.invite = 0.into();
            room_power_levels.kick = 10.into();
            assert!(
                filter_timeline_event(&event, None, user_id, Some(&room_power_levels)),
                "can accept, cannot decline",
            );
        }

        // Cannot accept. Can decline.
        {
            room_power_levels.invite = 10.into();
            room_power_levels.kick = 0.into();
            assert!(
                filter_timeline_event(&event, None, user_id, Some(&room_power_levels)),
                "cannot accept, can decline",
            );
        }

        // Can accept. Can decline.
        {
            room_power_levels.invite = 0.into();
            room_power_levels.kick = 0.into();
            assert!(
                filter_timeline_event(&event, None, user_id, Some(&room_power_levels)),
                "can accept, can decline",
            );
        }

        // Cannot accept. Can decline. But with an other user ID with at least the same
        // levels, i.e. the current user cannot kick another user with the same
        // or higher levels.
        {
            room_power_levels.users.insert(user_id.to_owned(), 5.into());
            room_power_levels.users.insert(other_user_id.to_owned(), 5.into());

            room_power_levels.invite = 10.into();
            room_power_levels.kick = 0.into();

            assert!(
                filter_timeline_event(&event, None, user_id, Some(&room_power_levels)).not(),
                "cannot accept, can decline, at least same user levels",
            );
        }
    }

    #[test]
    fn test_invite_state_event() {
        use ruma::events::room::member::MembershipState;

        // The current user is receiving an invite.
        assert_latest_event_content!(
            event | event_factory | {
                event_factory
                    .member(user_id!("@mnt_io:matrix.org"))
                    .membership(MembershipState::Invite)
                    .into_event()
            }
            is a candidate
        );
    }

    #[test]
    fn test_invite_state_event_for_someone_else() {
        use ruma::events::room::member::MembershipState;

        // The current user sees an invite but for someone else.
        assert_latest_event_content!(
            event | event_factory | {
                event_factory
                    .member(user_id!("@other_mnt_io:server.name"))
                    .membership(MembershipState::Invite)
                    .into_event()
            }
            is not a candidate
        );
    }

    #[test]
    fn test_room_message_verification_request() {
        use ruma::{OwnedDeviceId, events::room::message};

        assert_latest_event_content!(
            event | event_factory | {
                event_factory
                    .event(RoomMessageEventContent::new(message::MessageType::VerificationRequest(
                        message::KeyVerificationRequestEventContent::new(
                            "body".to_owned(),
                            vec![],
                            OwnedDeviceId::from("device_id"),
                            user_id!("@user:server.name").to_owned(),
                        ),
                    )))
                    .into_event()
            }
            is not a candidate
        );
    }
}

#[cfg(test)]
mod tests_latest_event_values_for_local_events {
    use assert_matches::assert_matches;
    use ruma::{
        MilliSecondsSinceUnixEpoch, OwnedTransactionId,
        events::{AnyMessageLikeEventContent, room::message::RoomMessageEventContent},
        serde::Raw,
    };
    use serde_json::json;

    use super::{
        LatestEventValue, LatestEventValuesForLocalEvents, LocalLatestEventValue,
        RemoteLatestEventValue, SerializableEventContent,
    };

    fn remote_room_message(body: &str) -> RemoteLatestEventValue {
        RemoteLatestEventValue::from_plaintext(
            Raw::from_json_string(
                json!({
                    "content": RoomMessageEventContent::text_plain(body),
                    "type": "m.room.message",
                    "event_id": "$ev0",
                    "origin_server_ts": 42,
                    "sender": "@mnt_io:matrix.org",
                })
                .to_string(),
            )
            .unwrap(),
        )
    }

    fn local_room_message(body: &str) -> LocalLatestEventValue {
        LocalLatestEventValue {
            timestamp: MilliSecondsSinceUnixEpoch::now(),
            content: SerializableEventContent::new(&AnyMessageLikeEventContent::RoomMessage(
                RoomMessageEventContent::text_plain(body),
            ))
            .unwrap(),
        }
    }

    #[test]
    fn test_last() {
        let mut buffer = LatestEventValuesForLocalEvents::new();

        assert!(buffer.last().is_none());

        buffer.push(
            OwnedTransactionId::from("txnid"),
            LatestEventValue::LocalIsSending(local_room_message("tome")),
        );

        assert_matches!(buffer.last(), Some(LatestEventValue::LocalIsSending(_)));
    }

    #[test]
    fn test_position() {
        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id = OwnedTransactionId::from("txnid");

        assert!(buffer.position(&transaction_id).is_none());

        buffer.push(
            transaction_id.clone(),
            LatestEventValue::LocalIsSending(local_room_message("raclette")),
        );
        buffer.push(
            OwnedTransactionId::from("othertxnid"),
            LatestEventValue::LocalIsSending(local_room_message("tome")),
        );

        assert_eq!(buffer.position(&transaction_id), Some(0));
    }

    #[test]
    #[should_panic]
    fn test_push_none() {
        let mut buffer = LatestEventValuesForLocalEvents::new();

        buffer.push(OwnedTransactionId::from("txnid"), LatestEventValue::None);
    }

    #[test]
    #[should_panic]
    fn test_push_remote() {
        let mut buffer = LatestEventValuesForLocalEvents::new();

        buffer.push(
            OwnedTransactionId::from("txnid"),
            LatestEventValue::Remote(remote_room_message("tome")),
        );
    }

    #[test]
    fn test_push_local() {
        let mut buffer = LatestEventValuesForLocalEvents::new();

        buffer.push(
            OwnedTransactionId::from("txnid0"),
            LatestEventValue::LocalIsSending(local_room_message("tome")),
        );
        buffer.push(
            OwnedTransactionId::from("txnid1"),
            LatestEventValue::LocalCannotBeSent(local_room_message("raclette")),
        );

        // no panic.
    }

    #[test]
    fn test_replace_content() {
        let mut buffer = LatestEventValuesForLocalEvents::new();

        buffer.push(
            OwnedTransactionId::from("txnid0"),
            LatestEventValue::LocalIsSending(local_room_message("gruyère")),
        );

        let LocalLatestEventValue { content: new_content, .. } = local_room_message("comté");

        buffer.replace_content(0, new_content);

        assert_matches!(
            buffer.last(),
            Some(LatestEventValue::LocalIsSending(local_event)) => {
                assert_matches!(
                    local_event.content.deserialize().unwrap(),
                    AnyMessageLikeEventContent::RoomMessage(content) => {
                        assert_eq!(content.body(), "comté");
                    }
                );
            }
        );
    }

    #[test]
    fn test_remove() {
        let mut buffer = LatestEventValuesForLocalEvents::new();

        buffer.push(
            OwnedTransactionId::from("txnid"),
            LatestEventValue::LocalIsSending(local_room_message("gryuère")),
        );

        assert!(buffer.last().is_some());

        buffer.remove(0);

        assert!(buffer.last().is_none());
    }

    #[test]
    fn test_mark_cannot_be_sent_from() {
        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");
        let transaction_id_2 = OwnedTransactionId::from("txnid2");

        buffer.push(
            transaction_id_0,
            LatestEventValue::LocalIsSending(local_room_message("gruyère")),
        );
        buffer.push(
            transaction_id_1.clone(),
            LatestEventValue::LocalIsSending(local_room_message("brigand")),
        );
        buffer.push(
            transaction_id_2,
            LatestEventValue::LocalIsSending(local_room_message("raclette")),
        );

        buffer.mark_cannot_be_sent_from(&transaction_id_1);

        assert_eq!(buffer.buffer.len(), 3);
        assert_matches!(buffer.buffer[0].1, LatestEventValue::LocalIsSending(_));
        assert_matches!(buffer.buffer[1].1, LatestEventValue::LocalCannotBeSent(_));
        assert_matches!(buffer.buffer[2].1, LatestEventValue::LocalCannotBeSent(_));
    }

    #[test]
    fn test_mark_is_sending_from() {
        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");
        let transaction_id_2 = OwnedTransactionId::from("txnid2");

        buffer.push(
            transaction_id_0,
            LatestEventValue::LocalCannotBeSent(local_room_message("gruyère")),
        );
        buffer.push(
            transaction_id_1.clone(),
            LatestEventValue::LocalCannotBeSent(local_room_message("brigand")),
        );
        buffer.push(
            transaction_id_2,
            LatestEventValue::LocalCannotBeSent(local_room_message("raclette")),
        );

        buffer.mark_is_sending_from(&transaction_id_1);

        assert_eq!(buffer.buffer.len(), 3);
        assert_matches!(buffer.buffer[0].1, LatestEventValue::LocalCannotBeSent(_));
        assert_matches!(buffer.buffer[1].1, LatestEventValue::LocalIsSending(_));
        assert_matches!(buffer.buffer[2].1, LatestEventValue::LocalIsSending(_));
    }

    #[test]
    fn test_mark_is_sending_after() {
        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");
        let transaction_id_2 = OwnedTransactionId::from("txnid2");

        buffer.push(
            transaction_id_0,
            LatestEventValue::LocalCannotBeSent(local_room_message("gruyère")),
        );
        buffer.push(
            transaction_id_1.clone(),
            LatestEventValue::LocalCannotBeSent(local_room_message("brigand")),
        );
        buffer.push(
            transaction_id_2,
            LatestEventValue::LocalCannotBeSent(local_room_message("raclette")),
        );

        buffer.mark_is_sending_after(&transaction_id_1);

        assert_eq!(buffer.buffer.len(), 3);
        assert_matches!(buffer.buffer[0].1, LatestEventValue::LocalCannotBeSent(_));
        assert_matches!(buffer.buffer[1].1, LatestEventValue::LocalCannotBeSent(_));
        assert_matches!(buffer.buffer[2].1, LatestEventValue::LocalIsSending(_));
    }
}

#[cfg(all(not(target_family = "wasm"), test))]
mod tests_latest_event_value_builder {
    use std::sync::Arc;

    use assert_matches::assert_matches;
    use matrix_sdk_base::{
        RoomState,
        deserialized_responses::TimelineEventKind,
        linked_chunk::{ChunkIdentifier, LinkedChunkId, Position, Update},
        store::SerializableEventContent,
    };
    use matrix_sdk_test::{async_test, event_factory::EventFactory};
    use ruma::{
        MilliSecondsSinceUnixEpoch, OwnedRoomId, OwnedTransactionId, event_id,
        events::{
            AnyMessageLikeEventContent, AnySyncMessageLikeEvent, AnySyncTimelineEvent,
            SyncMessageLikeEvent, reaction::ReactionEventContent, relation::Annotation,
            room::message::RoomMessageEventContent,
        },
        room_id, user_id,
    };

    use super::{
        LatestEventValue, LatestEventValueBuilder, LatestEventValuesForLocalEvents,
        RemoteLatestEventValue, RoomEventCache, RoomSendQueueUpdate,
    };
    use crate::{
        Client, Error,
        send_queue::{AbstractProgress, LocalEcho, LocalEchoContent, RoomSendQueue, SendHandle},
        test_utils::mocks::MatrixMockServer,
    };

    macro_rules! assert_remote_value_matches_room_message_with_body {
        ( $latest_event_value:expr => with body = $body:expr ) => {
            assert_matches!(
                $latest_event_value,
                LatestEventValue::Remote(RemoteLatestEventValue { kind: TimelineEventKind::PlainText { event }, .. }) => {
                    assert_matches!(
                        event.deserialize().unwrap(),
                        AnySyncTimelineEvent::MessageLike(
                            AnySyncMessageLikeEvent::RoomMessage(
                                SyncMessageLikeEvent::Original(message_content)
                            )
                        ) => {
                            assert_eq!(message_content.content.body(), $body);
                        }
                    );
                }
            );
        };
    }

    macro_rules! assert_local_value_matches_room_message_with_body {
        ( $latest_event_value:expr, $pattern:path => with body = $body:expr ) => {
            assert_matches!(
                $latest_event_value,
                $pattern (local_event) => {
                    assert_matches!(
                        local_event.content.deserialize().unwrap(),
                        AnyMessageLikeEventContent::RoomMessage(message_content) => {
                            assert_eq!(message_content.body(), $body);
                        }
                    );
                }
            );
        };
    }

    #[async_test]
    async fn test_remote_is_scanning_event_backwards_from_event_cache() {
        let room_id = room_id!("!r0");
        let user_id = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(user_id).room(room_id);
        let event_id_0 = event_id!("$ev0");
        let event_id_1 = event_id!("$ev1");
        let event_id_2 = event_id!("$ev2");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        // Prelude.
        {
            // Create the room.
            client.base_client().get_or_create_room(room_id, RoomState::Joined);

            // Initialise the event cache store.
            client
                .event_cache_store()
                .lock()
                .await
                .expect("Could not acquire the event cache lock")
                .as_clean()
                .expect("Could not acquire a clean event cache lock")
                .handle_linked_chunk_updates(
                    LinkedChunkId::Room(room_id),
                    vec![
                        Update::NewItemsChunk {
                            previous: None,
                            new: ChunkIdentifier::new(0),
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(0), 0),
                            items: vec![
                                // a latest event candidate
                                event_factory.text_msg("hello").event_id(event_id_0).into(),
                                // a latest event candidate
                                event_factory.text_msg("world").event_id(event_id_1).into(),
                                // not a latest event candidate
                                event_factory
                                    .room_topic("new room topic")
                                    .event_id(event_id_2)
                                    .into(),
                            ],
                        },
                    ],
                )
                .await
                .unwrap();
        }

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let (room_event_cache, _) = event_cache.for_room(room_id).await.unwrap();

        assert_remote_value_matches_room_message_with_body!(
            // We get `event_id_1` because `event_id_2` isn't a candidate,
            // and `event_id_0` hasn't been read yet (because events are read
            // backwards).
            LatestEventValueBuilder::new_remote(&room_event_cache, user_id, None).await => with body = "world"
        );
    }

    async fn local_prelude() -> (Client, OwnedRoomId, RoomSendQueue, RoomEventCache) {
        let room_id = room_id!("!r0").to_owned();

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        client.base_client().get_or_create_room(&room_id, RoomState::Joined);
        let room = client.get_room(&room_id).unwrap();

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let (room_event_cache, _) = event_cache.for_room(&room_id).await.unwrap();

        let send_queue = client.send_queue();
        let room_send_queue = send_queue.for_room(room);

        (client, room_id, room_send_queue, room_event_cache)
    }

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
    async fn test_local_new_local_event() {
        let (client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;
        let user_id = client.user_id().unwrap();

        let mut buffer = LatestEventValuesForLocalEvents::new();

        // Receiving one `NewLocalEvent`.
        {
            let transaction_id = OwnedTransactionId::from("txnid0");
            let content = new_local_echo_content(&room_send_queue, &transaction_id, "A");

            let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho { transaction_id, content });

            // The `LatestEventValue` matches the new local event.
            assert_local_value_matches_room_message_with_body!(
                LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "A"
            );
        }

        // Receiving another `NewLocalEvent`, ensuring it's pushed back in the buffer.
        {
            let transaction_id = OwnedTransactionId::from("txnid1");
            let content = new_local_echo_content(&room_send_queue, &transaction_id, "B");

            let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho { transaction_id, content });

            // The `LatestEventValue` matches the new local event.
            assert_local_value_matches_room_message_with_body!(
                LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "B"
            );
        }

        assert_eq!(buffer.buffer.len(), 2);
    }

    #[async_test]
    async fn test_local_new_local_event_when_previous_local_event_cannot_be_sent() {
        let (client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;
        let user_id = client.user_id().unwrap();

        let mut buffer = LatestEventValuesForLocalEvents::new();

        // Receiving one `NewLocalEvent`.
        let transaction_id_0 = {
            let transaction_id = OwnedTransactionId::from("txnid0");
            let content = new_local_echo_content(&room_send_queue, &transaction_id, "A");

            let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                transaction_id: transaction_id.clone(),
                content,
            });

            // The `LatestEventValue` matches the new local event.
            assert_local_value_matches_room_message_with_body!(
                LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "A"
            );

            transaction_id
        };

        // Receiving a `SendError` targeting the first event. The
        // `LatestEventValue` must change to indicate it “cannot be sent”.
        {
            let update = RoomSendQueueUpdate::SendError {
                transaction_id: transaction_id_0.clone(),
                error: Arc::new(Error::UnknownError("oopsy".to_owned().into())),
                is_recoverable: true,
            };

            // The `LatestEventValue` has changed, it still matches the latest local
            // event but it's marked as “cannot be sent”.
            assert_local_value_matches_room_message_with_body!(
                LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                LatestEventValue::LocalCannotBeSent => with body = "A"
            );

            assert_eq!(buffer.buffer.len(), 1);
            assert_matches!(&buffer.buffer[0].1, LatestEventValue::LocalCannotBeSent(_));
        }

        // Receiving another `NewLocalEvent`, ensuring it's pushed back in the buffer,
        // and as a `LocalCannotBeSent` because the previous value is itself
        // `LocalCannotBeSent`.
        {
            let transaction_id = OwnedTransactionId::from("txnid1");
            let content = new_local_echo_content(&room_send_queue, &transaction_id, "B");

            let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho { transaction_id, content });

            // The `LatestEventValue` matches the new local event.
            assert_local_value_matches_room_message_with_body!(
                LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                LatestEventValue::LocalCannotBeSent => with body = "B"
            );
        }

        assert_eq!(buffer.buffer.len(), 2);
        assert_matches!(&buffer.buffer[0].1, LatestEventValue::LocalCannotBeSent(_));
        assert_matches!(&buffer.buffer[1].1, LatestEventValue::LocalCannotBeSent(_));
    }

    #[async_test]
    async fn test_local_cancelled_local_event() {
        let (client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;
        let user_id = client.user_id().unwrap();

        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");
        let transaction_id_2 = OwnedTransactionId::from("txnid2");

        // Receiving three `NewLocalEvent`s.
        {
            for (transaction_id, body) in
                [(&transaction_id_0, "A"), (&transaction_id_1, "B"), (&transaction_id_2, "C")]
            {
                let content = new_local_echo_content(&room_send_queue, transaction_id, body);

                let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                    transaction_id: transaction_id.clone(),
                    content,
                });

                // The `LatestEventValue` matches the new local event.
                assert_local_value_matches_room_message_with_body!(
                    LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                    LatestEventValue::LocalIsSending => with body = body
                );
            }

            assert_eq!(buffer.buffer.len(), 3);
        }

        // Receiving a `CancelledLocalEvent` targeting the second event. The
        // `LatestEventValue` must not change.
        {
            let update = RoomSendQueueUpdate::CancelledLocalEvent {
                transaction_id: transaction_id_1.clone(),
            };

            // The `LatestEventValue` hasn't changed, it still matches the latest local
            // event.
            assert_local_value_matches_room_message_with_body!(
                LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "C"
            );

            assert_eq!(buffer.buffer.len(), 2);
        }

        // Receiving a `CancelledLocalEvent` targeting the second (so the last) event.
        // The `LatestEventValue` must point to the first local event.
        {
            let update = RoomSendQueueUpdate::CancelledLocalEvent {
                transaction_id: transaction_id_2.clone(),
            };

            // The `LatestEventValue` has changed, it matches the previous (so the first)
            // local event.
            assert_local_value_matches_room_message_with_body!(
                LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "A"
            );

            assert_eq!(buffer.buffer.len(), 1);
        }

        // Receiving a `CancelledLocalEvent` targeting the first (so the last) event.
        // The `LatestEventValue` cannot be computed from the send queue and will
        // fallback to the event cache. The event cache is empty in this case, so we get
        // nothing.
        {
            let update =
                RoomSendQueueUpdate::CancelledLocalEvent { transaction_id: transaction_id_0 };

            // The `LatestEventValue` has changed, it's empty!
            assert_matches!(
                LatestEventValueBuilder::new_local(
                    &update,
                    &mut buffer,
                    &room_event_cache,
                    user_id,
                    None
                )
                .await,
                LatestEventValue::None
            );

            assert!(buffer.buffer.is_empty());
        }
    }

    #[async_test]
    async fn test_local_sent_event() {
        let (client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;
        let user_id = client.user_id().unwrap();

        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");

        // Receiving two `NewLocalEvent`s.
        {
            for (transaction_id, body) in [(&transaction_id_0, "A"), (&transaction_id_1, "B")] {
                let content = new_local_echo_content(&room_send_queue, transaction_id, body);

                let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                    transaction_id: transaction_id.clone(),
                    content,
                });

                // The `LatestEventValue` matches the new local event.
                assert_local_value_matches_room_message_with_body!(
                    LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                    LatestEventValue::LocalIsSending => with body = body
                );
            }

            assert_eq!(buffer.buffer.len(), 2);
        }

        // Receiving a `SentEvent` targeting the first event. The `LatestEventValue`
        // must not change.
        {
            let update = RoomSendQueueUpdate::SentEvent {
                transaction_id: transaction_id_0.clone(),
                event_id: event_id!("$ev0").to_owned(),
            };

            // The `LatestEventValue` hasn't changed, it still matches the latest local
            // event.
            assert_local_value_matches_room_message_with_body!(
                LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "B"
            );

            assert_eq!(buffer.buffer.len(), 1);
        }

        // Receiving a `SentEvent` targeting the first event. The `LatestEventValue`
        // hasn't changed, this is still this event.
        {
            let update = RoomSendQueueUpdate::SentEvent {
                transaction_id: transaction_id_1,
                event_id: event_id!("$ev1").to_owned(),
            };

            // The `LatestEventValue` hasn't changed.
            assert_local_value_matches_room_message_with_body!(
                LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "B"
            );

            assert!(buffer.buffer.is_empty());
        }
    }

    #[async_test]
    async fn test_local_replaced_local_event() {
        let (client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;
        let user_id = client.user_id().unwrap();

        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");

        // Receiving two `NewLocalEvent`s.
        {
            for (transaction_id, body) in [(&transaction_id_0, "A"), (&transaction_id_1, "B")] {
                let content = new_local_echo_content(&room_send_queue, transaction_id, body);

                let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                    transaction_id: transaction_id.clone(),
                    content,
                });

                // The `LatestEventValue` matches the new local event.
                assert_local_value_matches_room_message_with_body!(
                    LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                    LatestEventValue::LocalIsSending => with body = body
                );
            }

            assert_eq!(buffer.buffer.len(), 2);
        }

        // Receiving a `ReplacedLocalEvent` targeting the first event. The
        // `LatestEventValue` must not change.
        {
            let transaction_id = &transaction_id_0;
            let LocalEchoContent::Event { serialized_event: new_content, .. } =
                new_local_echo_content(&room_send_queue, transaction_id, "A.")
            else {
                panic!("oopsy");
            };

            let update = RoomSendQueueUpdate::ReplacedLocalEvent {
                transaction_id: transaction_id.clone(),
                new_content,
            };

            // The `LatestEventValue` hasn't changed, it still matches the latest local
            // event.
            assert_local_value_matches_room_message_with_body!(
                LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "B"
            );

            assert_eq!(buffer.buffer.len(), 2);
        }

        // Receiving a `ReplacedLocalEvent` targeting the second (so the last) event.
        // The `LatestEventValue` is changing.
        {
            let transaction_id = &transaction_id_1;
            let LocalEchoContent::Event { serialized_event: new_content, .. } =
                new_local_echo_content(&room_send_queue, transaction_id, "B.")
            else {
                panic!("oopsy");
            };

            let update = RoomSendQueueUpdate::ReplacedLocalEvent {
                transaction_id: transaction_id.clone(),
                new_content,
            };

            // The `LatestEventValue` has changed, it still matches the latest local
            // event but with its new content.
            assert_local_value_matches_room_message_with_body!(
                LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "B."
            );

            assert_eq!(buffer.buffer.len(), 2);
        }
    }

    #[async_test]
    async fn test_local_replaced_local_event_by_a_non_suitable_event() {
        let (client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;
        let user_id = client.user_id().unwrap();

        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id = OwnedTransactionId::from("txnid0");

        // Receiving one `NewLocalEvent`.
        {
            let content = new_local_echo_content(&room_send_queue, &transaction_id, "A");

            let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                transaction_id: transaction_id.clone(),
                content,
            });

            // The `LatestEventValue` matches the new local event.
            assert_local_value_matches_room_message_with_body!(
                LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "A"
            );

            assert_eq!(buffer.buffer.len(), 1);
        }

        // Receiving a `ReplacedLocalEvent` targeting the first event. Sadly, the new
        // event cannot be mapped to a `LatestEventValue`! The first event is removed
        // from the buffer, and the `LatestEventValue` becomes `None` because there is
        // no other alternative.
        {
            let new_content = SerializableEventContent::new(&AnyMessageLikeEventContent::Reaction(
                ReactionEventContent::new(Annotation::new(
                    event_id!("$ev0").to_owned(),
                    "+1".to_owned(),
                )),
            ))
            .unwrap();

            let update = RoomSendQueueUpdate::ReplacedLocalEvent {
                transaction_id: transaction_id.clone(),
                new_content,
            };

            // The `LatestEventValue` has changed!
            assert_matches!(
                LatestEventValueBuilder::new_local(
                    &update,
                    &mut buffer,
                    &room_event_cache,
                    user_id,
                    None
                )
                .await,
                LatestEventValue::None
            );

            assert_eq!(buffer.buffer.len(), 0);
        }
    }

    #[async_test]
    async fn test_local_send_error() {
        let (client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;
        let user_id = client.user_id().unwrap();

        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");

        // Receiving two `NewLocalEvent`s.
        {
            for (transaction_id, body) in [(&transaction_id_0, "A"), (&transaction_id_1, "B")] {
                let content = new_local_echo_content(&room_send_queue, transaction_id, body);

                let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                    transaction_id: transaction_id.clone(),
                    content,
                });

                // The `LatestEventValue` matches the new local event.
                assert_local_value_matches_room_message_with_body!(
                    LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                    LatestEventValue::LocalIsSending => with body = body
                );
            }

            assert_eq!(buffer.buffer.len(), 2);
        }

        // Receiving a `SendError` targeting the first event. The
        // `LatestEventValue` must change to indicate it's “cannot be sent”.
        {
            let update = RoomSendQueueUpdate::SendError {
                transaction_id: transaction_id_0.clone(),
                error: Arc::new(Error::UnknownError("oopsy".to_owned().into())),
                is_recoverable: true,
            };

            // The `LatestEventValue` has changed, it still matches the latest local
            // event but it's marked as “cannot be sent”.
            assert_local_value_matches_room_message_with_body!(
                LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                LatestEventValue::LocalCannotBeSent => with body = "B"
            );

            assert_eq!(buffer.buffer.len(), 2);
            assert_matches!(&buffer.buffer[0].1, LatestEventValue::LocalCannotBeSent(_));
            assert_matches!(&buffer.buffer[1].1, LatestEventValue::LocalCannotBeSent(_));
        }

        // Receiving a `SentEvent` targeting the first event. The `LatestEventValue`
        // must change: since an event has been sent, the following events are now
        // “is sending”.
        {
            let update = RoomSendQueueUpdate::SentEvent {
                transaction_id: transaction_id_0.clone(),
                event_id: event_id!("$ev0").to_owned(),
            };

            // The `LatestEventValue` has changed, it still matches the latest local
            // event but it's “is sending”.
            assert_local_value_matches_room_message_with_body!(
                LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "B"
            );

            assert_eq!(buffer.buffer.len(), 1);
            assert_matches!(&buffer.buffer[0].1, LatestEventValue::LocalIsSending(_));
        }
    }

    #[async_test]
    async fn test_local_retry_event() {
        let (client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;
        let user_id = client.user_id().unwrap();

        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id_0 = OwnedTransactionId::from("txnid0");
        let transaction_id_1 = OwnedTransactionId::from("txnid1");

        // Receiving two `NewLocalEvent`s.
        {
            for (transaction_id, body) in [(&transaction_id_0, "A"), (&transaction_id_1, "B")] {
                let content = new_local_echo_content(&room_send_queue, transaction_id, body);

                let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                    transaction_id: transaction_id.clone(),
                    content,
                });

                // The `LatestEventValue` matches the new local event.
                assert_local_value_matches_room_message_with_body!(
                    LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                    LatestEventValue::LocalIsSending => with body = body
                );
            }

            assert_eq!(buffer.buffer.len(), 2);
        }

        // Receiving a `SendError` targeting the first event. The
        // `LatestEventValue` must change to indicate it's “cannot be sent”.
        {
            let update = RoomSendQueueUpdate::SendError {
                transaction_id: transaction_id_0.clone(),
                error: Arc::new(Error::UnknownError("oopsy".to_owned().into())),
                is_recoverable: true,
            };

            // The `LatestEventValue` has changed, it still matches the latest local
            // event but it's marked as “cannot be sent”.
            assert_local_value_matches_room_message_with_body!(
                LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                LatestEventValue::LocalCannotBeSent => with body = "B"
            );

            assert_eq!(buffer.buffer.len(), 2);
            assert_matches!(&buffer.buffer[0].1, LatestEventValue::LocalCannotBeSent(_));
            assert_matches!(&buffer.buffer[1].1, LatestEventValue::LocalCannotBeSent(_));
        }

        // Receiving a `RetryEvent` targeting the first event. The `LatestEventValue`
        // must change: this local event and its following must be “is sending”.
        {
            let update =
                RoomSendQueueUpdate::RetryEvent { transaction_id: transaction_id_0.clone() };

            // The `LatestEventValue` has changed, it still matches the latest local
            // event but it's “is sending”.
            assert_local_value_matches_room_message_with_body!(
                LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "B"
            );

            assert_eq!(buffer.buffer.len(), 2);
            assert_matches!(&buffer.buffer[0].1, LatestEventValue::LocalIsSending(_));
            assert_matches!(&buffer.buffer[1].1, LatestEventValue::LocalIsSending(_));
        }
    }

    #[async_test]
    async fn test_local_media_upload() {
        let (client, _room_id, room_send_queue, room_event_cache) = local_prelude().await;
        let user_id = client.user_id().unwrap();

        let mut buffer = LatestEventValuesForLocalEvents::new();
        let transaction_id = OwnedTransactionId::from("txnid");

        // Receiving a `NewLocalEvent`.
        {
            let content = new_local_echo_content(&room_send_queue, &transaction_id, "A");

            let update = RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                transaction_id: transaction_id.clone(),
                content,
            });

            // The `LatestEventValue` matches the new local event.
            assert_local_value_matches_room_message_with_body!(
                LatestEventValueBuilder::new_local(&update, &mut buffer, &room_event_cache, user_id, None).await,
                LatestEventValue::LocalIsSending => with body = "A"
            );

            assert_eq!(buffer.buffer.len(), 1);
        }

        // Receiving a `MediaUpload` targeting the first event. The
        // `LatestEventValue` must not change as `MediaUpload` are ignored.
        {
            let update = RoomSendQueueUpdate::MediaUpload {
                related_to: transaction_id,
                file: None,
                index: 0,
                progress: AbstractProgress { current: 0, total: 0 },
            };

            // The `LatestEventValue` has changed somehow, it tells no new
            // `LatestEventValue` is computed.
            assert_matches!(
                LatestEventValueBuilder::new_local(
                    &update,
                    &mut buffer,
                    &room_event_cache,
                    user_id,
                    None
                )
                .await,
                LatestEventValue::None
            );

            assert_eq!(buffer.buffer.len(), 1);
        }
    }

    #[async_test]
    async fn test_local_fallbacks_to_remote_when_empty() {
        let room_id = room_id!("!r0");
        let user_id = user_id!("@mnt_io:matrix.org");
        let event_factory = EventFactory::new().sender(user_id).room(room_id);
        let event_id_0 = event_id!("$ev0");
        let event_id_1 = event_id!("$ev1");

        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        // Prelude.
        {
            // Create the room.
            client.base_client().get_or_create_room(room_id, RoomState::Joined);

            // Initialise the event cache store.
            client
                .event_cache_store()
                .lock()
                .await
                .expect("Could not acquire the event cache lock")
                .as_clean()
                .expect("Could not acquire a clean event cache lock")
                .handle_linked_chunk_updates(
                    LinkedChunkId::Room(room_id),
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

        let (room_event_cache, _) = event_cache.for_room(room_id).await.unwrap();

        let mut buffer = LatestEventValuesForLocalEvents::new();

        // We get a `Remote` because there is no `Local*` values!
        assert_remote_value_matches_room_message_with_body!(
            LatestEventValueBuilder::new_local(
                // An update that won't be create a new `LatestEventValue`.
                &RoomSendQueueUpdate::SentEvent {
                    transaction_id: OwnedTransactionId::from("txnid"),
                    event_id: event_id_1.to_owned(),
                },
                &mut buffer,
                &room_event_cache,
                user_id,
                None,
            )
            .await
             => with body = "hello"
        );
    }
}
