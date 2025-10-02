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

//! The REDECRYPTOR is a layer that handles redecryption of events in case we
//! couldn't decrypt them imediatelly

use std::{collections::BTreeSet, pin::Pin, sync::Weak};

use as_variant::as_variant;
use futures_core::Stream;
use futures_util::{StreamExt, pin_mut};
use matrix_sdk_base::{
    crypto::{store::types::RoomKeyInfo, types::events::room::encrypted::EncryptedEvent},
    deserialized_responses::{DecryptedRoomEvent, TimelineEvent, TimelineEventKind},
};
use matrix_sdk_common::executor::{JoinHandle, spawn};
use ruma::{OwnedEventId, OwnedRoomId, RoomId, events::AnySyncTimelineEvent, serde::Raw};
use tokio::sync::mpsc::UnboundedSender;
use tokio_stream::wrappers::{UnboundedReceiverStream, errors::BroadcastStreamRecvError};
use tracing::{info, instrument, trace, warn};

use crate::event_cache::{
    EventCache, EventCacheError, EventCacheInner, EventsOrigin, RoomEventCacheUpdate,
};

/// The information sent across the channel to the long-running task requesting
/// that the supplied set of sessions be retried.
pub struct DecryptionRetryRequest {
    room_id: OwnedRoomId,
    session_ids: BTreeSet<String>,
}

type SessionId<'a> = &'a str;

impl EventCache {
    async fn get_utds(
        &self,
        room_id: &RoomId,
        session_id: SessionId<'_>,
    ) -> Result<Vec<(OwnedEventId, Raw<AnySyncTimelineEvent>)>, EventCacheError> {
        let map_timeline_event = |event: TimelineEvent| {
            let event_id = event.event_id();

            // Only pick out events that are UTDs, get just the Raw event as this is what
            // the OlmMachine needs.
            let event =
                as_variant!(event.kind, TimelineEventKind::UnableToDecrypt { event, .. } => event);
            // Zip the event ID and event together so we don't have to pick out the event ID
            // again. We need the event ID to replace the event in the cache.
            event_id.zip(event)
        };

        // Load the relevant events from the event cache store and attempt to redecrypt
        // things.
        let store = self.inner.store.lock().await?;
        let events =
            store.get_room_events(room_id, Some("m.room.encrypted"), Some(session_id)).await?;

        Ok(events.into_iter().filter_map(map_timeline_event).collect())
    }

    #[instrument(skip_all, fields(room_id))]
    async fn on_resolved_utds(
        &self,
        room_id: &RoomId,
        events: Vec<(OwnedEventId, DecryptedRoomEvent)>,
    ) -> Result<(), EventCacheError> {
        // Get the cache for this particular room and lock the state for the duration of
        // the decryption.
        let (room_cache, _drop_handles) = self.for_room(room_id).await?;
        let mut state = room_cache.inner.state.write().await;

        let event_ids: Vec<_> = events.iter().map(|(event_id, _)| event_id).collect();

        trace!(?event_ids, "Replacing successfully re-decrypted events");

        for (event_id, decrypted) in events {
            // The event isn't in the cache, nothing to replace. Realistically this can't
            // happen since we retrieved the list of events from the cache itself and
            // `find_event()` will look into the store as well.
            if let Some((location, mut target_event)) = state.find_event(&event_id).await? {
                target_event.kind = TimelineEventKind::Decrypted(decrypted);

                // TODO: `replace_event_at()` propagates changes to the store for every event,
                // we should probably have a bulk version of this?
                state.replace_event_at(location, target_event).await?
            }
        }

        // We replaced a bunch of events, reactive updates for those replacements have
        // been queued up. We need to send them out to our subscribers now.
        let diffs = state.room_linked_chunk_mut().updates_as_vector_diffs();

        let _ = room_cache.inner.sender.send(RoomEventCacheUpdate::UpdateTimelineEvents {
            diffs,
            origin: EventsOrigin::Cache,
        });

        Ok(())
    }

    async fn decrypt_event(
        &self,
        room_id: &RoomId,
        event: &Raw<EncryptedEvent>,
    ) -> Option<DecryptedRoomEvent> {
        let client = self.inner.client().ok()?;
        let machine = client.olm_machine().await;
        let machine = machine.as_ref()?;

        match machine.decrypt_room_event(event, room_id, client.decryption_settings()).await {
            Ok(decrypted) => Some(decrypted),
            Err(e) => {
                warn!("Failed to redecrypt an event despite receiving a room key for it {e:?}");
                None
            }
        }
    }

    /// Attempt to redecrypt events after a room key with the given session ID
    /// has been received.
    #[instrument(skip_all, fields(room_key_info))]
    async fn retry_decryption(
        &self,
        room_id: &RoomId,
        session_id: SessionId<'_>,
    ) -> Result<(), EventCacheError> {
        trace!("Retrying to decrypt");

        let events = self.get_utds(room_id, session_id).await?;
        let mut decrypted_events = Vec::with_capacity(events.len());

        for (event_id, event) in events {
            // If we managed to decrypt the event, and we should have to since we received
            // the room key for this specific event, then replace the event.
            if let Some(decrypted) = self.decrypt_event(room_id, event.cast_ref_unchecked()).await {
                decrypted_events.push((event_id, decrypted));
            }
        }

        self.on_resolved_utds(room_id, decrypted_events).await?;

        Ok(())
    }
}

pub(crate) struct Redecryptor {
    request_decryption_sender: UnboundedSender<DecryptionRetryRequest>,
    task: JoinHandle<()>,
}

impl Drop for Redecryptor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Redecryptor {
    pub(super) fn new(cache: Weak<EventCacheInner>) -> Self {
        let (request_decryption_sender, receiver) = tokio::sync::mpsc::unbounded_channel();

        let task = spawn(async {
            let request_redecryption_stream = UnboundedReceiverStream::new(receiver);

            Self::listen_for_room_keys_task(cache, request_redecryption_stream).await;
        });

        Self { task, request_decryption_sender }
    }

    #[allow(dead_code)]
    pub(super) fn request_decryption(&self, request: DecryptionRetryRequest) {
        let _ = self.request_decryption_sender.send(request).inspect_err(|_| {
            warn!("Requesting a decryption while the redecryption task has been shut down")
        });
    }

    async fn subscribe_to_room_key_stream(
        cache: &Weak<EventCacheInner>,
    ) -> Option<impl Stream<Item = Result<Vec<RoomKeyInfo>, BroadcastStreamRecvError>>> {
        let event_cache = cache.upgrade()?;
        let client = event_cache.client().ok()?;
        let machine = client.olm_machine().await;

        machine.as_ref().map(|m| m.store().room_keys_received_stream())
    }

    fn upgrade_event_cache(cache: &Weak<EventCacheInner>) -> Option<EventCache> {
        cache.upgrade().map(|inner| EventCache { inner })
    }

    async fn redecryption_loop(
        cache: &Weak<EventCacheInner>,
        decryption_request_stream: &mut Pin<&mut impl Stream<Item = DecryptionRetryRequest>>,
    ) -> bool {
        let Some(room_key_stream) = Self::subscribe_to_room_key_stream(cache).await else {
            return false;
        };

        pin_mut!(room_key_stream);

        loop {
            tokio::select! {
                Some(request) = decryption_request_stream.next() => {
                        let Some(cache) = Self::upgrade_event_cache(cache) else {
                            break false;
                        };

                        for session_id in request.session_ids {
                            let _ = cache
                                .retry_decryption(&request.room_id, &session_id)
                                .await
                                .inspect_err(|e| warn!("Error redecrypting after an explicit request was received {e:?}"));
                        }
                }
                room_keys = room_key_stream.next() => {
                    match room_keys {
                        Some(Ok(room_keys)) => {
                            let Some(cache) = Self::upgrade_event_cache(cache) else {
                                break false;
                            };

                            for key in room_keys {
                                let _ = cache
                                    .retry_decryption(&key.room_id, &key.session_id)
                                    .await
                                    .inspect_err(|e| warn!("Error redecrypting {e:?}"));
                            }
                        },
                        Some(Err(_)) => {
                            todo!("Handle lagging here, how?")
                        },
                        None => {
                            break true
                        }
                    }
                }
                else => break false,
            }
        }
    }

    async fn listen_for_room_keys_task(
        cache: Weak<EventCacheInner>,
        decryption_request_stream: UnboundedReceiverStream<DecryptionRetryRequest>,
    ) {
        pin_mut!(decryption_request_stream);

        while Self::redecryption_loop(&cache, &mut decryption_request_stream).await {
            info!("Regenerating the re-decryption streams")
        }

        info!("Shutting down the event cache redecryptor");
    }
}

#[cfg(not(target_family = "wasm"))]
#[cfg(test)]
mod tests {
    use std::time::Duration;

    use assert_matches2::assert_matches;
    use eyeball_im::VectorDiff;
    use matrix_sdk_base::deserialized_responses::TimelineEventKind;
    use matrix_sdk_test::{
        JoinedRoomBuilder, StateTestEvent, async_test, event_factory::EventFactory,
    };
    use ruma::{device_id, event_id, room_id, user_id};
    use serde_json::json;

    use crate::{
        assert_let_timeout, encryption::EncryptionSettings, event_cache::RoomEventCacheUpdate,
        test_utils::mocks::MatrixMockServer,
    };

    #[async_test]
    async fn test_redecryptor() {
        let room_id = room_id!("!test:localhost");

        let alice_user_id = user_id!("@alice:localhost");
        let alice_device_id = device_id!("ALICEDEVICE");
        let bob_user_id = user_id!("@bob:localhost");
        let bob_device_id = device_id!("BOBDEVICE");

        let matrix_mock_server = MatrixMockServer::new().await;
        matrix_mock_server.mock_crypto_endpoints_preset().await;

        let encryption_settings =
            EncryptionSettings { auto_enable_cross_signing: true, ..Default::default() };

        // Create some clients for Alice and Bob.

        let alice = matrix_mock_server
            .client_builder_for_crypto_end_to_end(alice_user_id, alice_device_id)
            .on_builder(|builder| {
                builder
                    .with_enable_share_history_on_invite(true)
                    .with_encryption_settings(encryption_settings)
            })
            .build()
            .await;

        let bob = matrix_mock_server
            .client_builder_for_crypto_end_to_end(bob_user_id, bob_device_id)
            .on_builder(|builder| {
                builder
                    .with_enable_share_history_on_invite(true)
                    .with_encryption_settings(encryption_settings)
            })
            .build()
            .await;

        bob.event_cache().subscribe().expect("Bob should be able to enable the event cache");

        // Ensure that Alice and Bob are aware of their devices and identities.
        matrix_mock_server.exchange_e2ee_identities(&alice, &bob).await;

        let event_factory = EventFactory::new().room(room_id);
        let alice_member_event = event_factory.member(alice_user_id).into_raw();
        let bob_member_event = event_factory.member(bob_user_id).into_raw();

        // Let us now create a room for them.
        let room_builder = JoinedRoomBuilder::new(room_id)
            .add_state_event(StateTestEvent::Create)
            .add_state_event(StateTestEvent::Encryption);

        matrix_mock_server
            .mock_sync()
            .ok_and_run(&alice, |builder| {
                builder.add_joined_room(room_builder.clone());
            })
            .await;

        matrix_mock_server
            .mock_sync()
            .ok_and_run(&bob, |builder| {
                builder.add_joined_room(room_builder);
            })
            .await;

        let room = alice
            .get_room(room_id)
            .expect("Alice should have access to the room now that we synced");

        // Alice will send a single event to the room, but this will trigger a to-device
        // message containing the room key to be sent as well. We capture both the event
        // and the to-device message.

        let event_type = "m.room.message";
        let content = json!({"body": "It's a secret to everybody", "msgtype": "m.text"});

        let event_id = event_id!("$some_id");
        let (event_receiver, mock) =
            matrix_mock_server.mock_room_send().ok_with_capture(event_id, alice_user_id);
        let (_guard, room_key) = matrix_mock_server.mock_capture_put_to_device(alice_user_id).await;

        {
            let _guard = mock.mock_once().mount_as_scoped().await;

            matrix_mock_server
                .mock_get_members()
                .ok(vec![alice_member_event.clone(), bob_member_event.clone()])
                .mock_once()
                .mount()
                .await;

            room.send_raw(event_type, content)
                .await
                .expect("We should be able to send an initial message");
        };

        // Let's now see what Bob's event cache does.

        let (room_cache, _) = bob
            .event_cache()
            .for_room(room_id)
            .await
            .expect("We should be able to get to the event cache for a specific room");

        let (_, mut subscriber) = room_cache.subscribe().await;

        // Let us retrieve the captured event and to-device message.
        let event = event_receiver.await.expect("Alice should have sent the event by now");
        let room_key = room_key.await;

        // We regenerate the Olm machine to check if the room key stream is recreated to
        // correctly.
        bob.inner
            .base_client
            .regenerate_olm(None)
            .await
            .expect("We should be able to regenerate the Olm machine");

        // Let us forward the event to Bob.
        matrix_mock_server
            .mock_sync()
            .ok_and_run(&bob, |builder| {
                builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(event));
            })
            .await;

        // Alright, Bob has received an update from the cache.

        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = subscriber.recv()
        );

        // There should be a single new event, and it should be a UTD as we did not
        // receive the room key yet.
        assert_eq!(diffs.len(), 1);
        assert_matches!(&diffs[0], VectorDiff::Append { values });
        assert_matches!(&values[0].kind, TimelineEventKind::UnableToDecrypt { .. });

        // Now we send the room key to Bob.
        matrix_mock_server
            .mock_sync()
            .ok_and_run(&bob, |builder| {
                builder.add_to_device_event(
                    room_key
                        .deserialize_as()
                        .expect("We should be able to deserialize the room key"),
                );
            })
            .await;

        // Bob should receive a new update from the cache.
        assert_let_timeout!(
            Duration::from_secs(1),
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = subscriber.recv()
        );

        // It should replace the UTD with a decrypted event.
        assert_eq!(diffs.len(), 1);
        assert_matches!(&diffs[0], VectorDiff::Set { index, value });
        assert_eq!(*index, 0);
        assert_matches!(&value.kind, TimelineEventKind::Decrypted { .. });
    }
}
