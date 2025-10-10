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

//! The Redecryptor (Rd) is a layer and long-running background task which
//! handles redecryption of events in case we couldn't decrypt them imediatelly.
//!
//! Rd listens to the OlmMachine for received room keys and new
//! m.room_key.withheld events.
//!
//! If a new room key has been received it attempts to find any UTDs in the
//! [`EventCache`]. If Rd decrypts any UTDs from the event cache it will replace
//! the events in the cache and send out new [`RoomEventCacheUpdates`] to any of
//! its listeners.
//!
//! If a new withheld info has been received it attempts to find any relevant
//! events and updates the [`EncryptionInfo`] of an event.
//!
//! There's an additional gotcha, the [`OlmMachine`] might get recreated by
//! calls to [`BaseClient::regenerate_olm()`]. When this happens we will receive
//! a `None` on the room keys stream and we need to re-listen to it.
//!
//! Another gotcha is that room keys might be received on another process if the
//! Client is operating on a iOS device. A separate process is used in this case
//! to receive push notifications. In this case the room key will be received
//! and Rd won't get notified about it. To work around this decryption requests
//! can be explicitly sent to Rd.
//!
//!
//!                              ┌─────────────┐
//!                              │             │
//!                  ┌───────────┤  Timeline   │◄────────────┐
//!                  │           │             │             │
//!                  │           └─────▲───────┘             │
//!                  │                 │                     │
//!                  │                 │                     │
//!                  │                 │                     │
//!              Decryption            │                 Redecryptor
//!                request             │                   report
//!                  │       RoomEventCacheUpdates           │         
//!                  │                 │                     │
//!                  │                 │                     │
//!                  │      ┌──────────┴────────────┐        │
//!                  │      │                       │        │
//!                  └──────►      Redecryptor      │────────┘
//!                         │                       │
//!                         └───────────▲───────────┘
//!                                     │
//!                                     │
//!                                     │
//!                         Received room keys stream
//!                                     │
//!                                     │
//!                                     │
//!                             ┌───────┴──────┐
//!                             │              │
//!                             │  OlmMachine  │
//!                             │              │
//!                             └──────────────┘

use std::{collections::BTreeSet, pin::Pin, sync::Weak};

use as_variant::as_variant;
use futures_core::Stream;
use futures_util::{StreamExt, pin_mut};
#[cfg(doc)]
use matrix_sdk_base::{BaseClient, crypto::OlmMachine};
use matrix_sdk_base::{
    crypto::{
        store::types::{RoomKeyInfo, RoomKeyWithheldInfo},
        types::events::room::encrypted::EncryptedEvent,
    },
    deserialized_responses::{DecryptedRoomEvent, TimelineEvent, TimelineEventKind},
    locks::Mutex,
};
#[cfg(doc)]
use matrix_sdk_common::deserialized_responses::EncryptionInfo;
use matrix_sdk_common::executor::{JoinHandle, spawn};
use ruma::{
    OwnedEventId, OwnedRoomId, RoomId,
    events::{AnySyncTimelineEvent, room::encrypted::OriginalSyncRoomEncryptedEvent},
    push::Action,
    serde::Raw,
};
use tokio::sync::{
    broadcast,
    mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
};
use tokio_stream::wrappers::{
    BroadcastStream, UnboundedReceiverStream, errors::BroadcastStreamRecvError,
};
use tracing::{info, instrument, trace, warn};

use crate::{
    Room,
    event_cache::{
        EventCache, EventCacheError, EventCacheInner, EventsOrigin, RoomEventCacheUpdate,
    },
    room::PushContext,
};

type SessionId<'a> = &'a str;
type OwnedSessionId = String;

/// The information sent across the channel to the long-running task requesting
/// that the supplied set of sessions be retried.
#[derive(Debug, Clone)]
pub struct DecryptionRetryRequest {
    /// The room ID of the room the events belong to.
    pub room_id: OwnedRoomId,
    /// Events that are not decrypted.
    pub utd_session_ids: BTreeSet<OwnedSessionId>,
    /// Events that are decrypted but might need to have their
    /// [`EncryptionInfo`] refreshed.
    pub refresh_info_session_ids: BTreeSet<OwnedSessionId>,
}

/// A report coming from the redecryptor.
#[derive(Debug, Clone)]
pub enum RedecryptorReport {
    /// Events which we were able to decrypt.
    ResolvedUtds {
        /// The room ID of the room the events belong to.
        room_id: OwnedRoomId,
        /// The list of event IDs of the decrypted events.
        events: BTreeSet<OwnedEventId>,
    },
    /// The redecryptor might have missed some room keys so it might not have
    /// re-decrypted events that are now decryptable.
    Lagging,
}

pub(super) struct RedecryptorChannels {
    utd_reporter: broadcast::Sender<RedecryptorReport>,
    pub(super) decryption_request_sender: UnboundedSender<DecryptionRetryRequest>,
    pub(super) decryption_request_receiver:
        Mutex<Option<UnboundedReceiver<DecryptionRetryRequest>>>,
}

impl RedecryptorChannels {
    pub(super) fn new() -> Self {
        let (utd_reporter, _) = broadcast::channel(100);
        let (decryption_request_sender, decryption_request_receiver) = unbounded_channel();

        Self {
            utd_reporter,
            decryption_request_sender,
            decryption_request_receiver: Mutex::new(Some(decryption_request_receiver)),
        }
    }
}

impl EventCache {
    /// Retrieve a set of events that we weren't able to decrypt.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The ID of the room where the events were sent to.
    /// * `session_id` - The unique ID of the room key that was used to encrypt
    ///   the event.
    async fn get_utds(
        &self,
        room_id: &RoomId,
        session_id: SessionId<'_>,
    ) -> Result<Vec<(OwnedEventId, Raw<AnySyncTimelineEvent>)>, EventCacheError> {
        let filter = |event: TimelineEvent| {
            let event_id = event.event_id();

            // Only pick out events that are UTDs, get just the Raw event as this is what
            // the OlmMachine needs.
            let event =
                as_variant!(event.kind, TimelineEventKind::UnableToDecrypt { event, .. } => event);
            // Zip the event ID and event together so we don't have to pick out the event ID
            // again. We need the event ID to replace the event in the cache.
            event_id.zip(event)
        };

        let store = self.inner.store.lock().await?;
        let events =
            store.get_room_events(room_id, Some("m.room.encrypted"), Some(session_id)).await?;

        Ok(events.into_iter().filter_map(filter).collect())
    }

    async fn get_decrypted_events(
        &self,
        room_id: &RoomId,
        session_id: SessionId<'_>,
    ) -> Result<Vec<(OwnedEventId, DecryptedRoomEvent)>, EventCacheError> {
        let filter = |event: TimelineEvent| {
            let event_id = event.event_id();

            let event = as_variant!(event.kind, TimelineEventKind::Decrypted(event) => event);
            // Zip the event ID and event together so we don't have to pick out the event ID
            // again. We need the event ID to replace the event in the cache.
            event_id.zip(event)
        };

        let store = self.inner.store.lock().await?;
        let events = store.get_room_events(room_id, None, Some(session_id)).await?;

        Ok(events.into_iter().filter_map(filter).collect())
    }

    /// Handle a chunk of events that we were previously unable to decrypt but
    /// have now successfully decrypted.
    ///
    /// This function will replace the existing UTD events in memory and the
    /// store and send out a [`RoomEventCacheUpdate`] for the newly
    /// decrypted events.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The ID of the room where the events were sent to.
    /// * `events` - A chunk of events that were successfully decrypted.
    #[instrument(skip_all, fields(room_id))]
    async fn on_resolved_utds(
        &self,
        room_id: &RoomId,
        events: Vec<(OwnedEventId, DecryptedRoomEvent, Option<Vec<Action>>)>,
    ) -> Result<(), EventCacheError> {
        // Get the cache for this particular room and lock the state for the duration of
        // the decryption.
        let (room_cache, _drop_handles) = self.for_room(room_id).await?;
        let mut state = room_cache.inner.state.write().await;

        let event_ids: BTreeSet<_> =
            events.iter().cloned().map(|(event_id, _, _)| event_id).collect();

        trace!(?event_ids, "Replacing successfully re-decrypted events");

        for (event_id, decrypted, actions) in events {
            // The event isn't in the cache, nothing to replace. Realistically this can't
            // happen since we retrieved the list of events from the cache itself and
            // `find_event()` will look into the store as well.
            if let Some((location, mut target_event)) = state.find_event(&event_id).await? {
                target_event.kind = TimelineEventKind::Decrypted(decrypted);

                if let Some(actions) = actions {
                    target_event.set_push_actions(actions);
                }

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

        // We report that we resolved some UTDs, this is mainly for listeners that don't
        // care about the actual events, just about the fact that UTDs got
        // resolved. Not sure if we'll have more such listeners but the UTD hook
        // is one such thing.
        let report =
            RedecryptorReport::ResolvedUtds { room_id: room_id.to_owned(), events: event_ids };
        let _ = self.inner.redecryption_channels.utd_reporter.send(report);

        Ok(())
    }

    /// Attempt to decrypt a single event.
    async fn decrypt_event(
        &self,
        room_id: &RoomId,
        room: Option<&Room>,
        push_context: Option<&PushContext>,
        event: &Raw<EncryptedEvent>,
    ) -> Option<(DecryptedRoomEvent, Option<Vec<Action>>)> {
        if let Some(room) = room {
            match room
                .decrypt_event(
                    event.cast_ref_unchecked::<OriginalSyncRoomEncryptedEvent>(),
                    push_context,
                )
                .await
            {
                Ok(maybe_decrypted) => {
                    let actions = maybe_decrypted.push_actions().map(|a| a.to_vec());

                    if let TimelineEventKind::Decrypted(decrypted) = maybe_decrypted.kind {
                        Some((decrypted, actions))
                    } else {
                        warn!(
                            "Failed to redecrypt an event despite receiving a room key or request to redecrypt"
                        );
                        None
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to redecrypt an event despite receiving a room key or request to redecrypt {e:?}"
                    );
                    None
                }
            }
        } else {
            let client = self.inner.client().ok()?;
            let machine = client.olm_machine().await;
            let machine = machine.as_ref()?;

            match machine.decrypt_room_event(event, room_id, client.decryption_settings()).await {
                Ok(decrypted) => Some((decrypted, None)),
                Err(e) => {
                    warn!(
                        "Failed to redecrypt an event despite receiving a room key or a request to redecrypt {e:?}"
                    );
                    None
                }
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

        // Get all the relevant UTDs.
        let events = self.get_utds(room_id, session_id).await?;

        let room = self.inner.client().ok().and_then(|client| client.get_room(room_id));
        let push_context =
            if let Some(room) = &room { room.push_context().await.ok().flatten() } else { None };

        // Let's attempt to decrypt them them.
        let mut decrypted_events = Vec::with_capacity(events.len());

        for (event_id, event) in events {
            // If we managed to decrypt the event, and we should have to since we received
            // the room key for this specific event, then replace the event.
            if let Some((decrypted, actions)) = self
                .decrypt_event(
                    room_id,
                    room.as_ref(),
                    push_context.as_ref(),
                    event.cast_ref_unchecked(),
                )
                .await
            {
                decrypted_events.push((event_id, decrypted, actions));
            }
        }

        // Replace the events and notify listeners that UTDs have been replaced with
        // decrypted events.
        self.on_resolved_utds(room_id, decrypted_events).await?;

        Ok(())
    }

    async fn update_encryption_info(
        &self,
        room_id: &RoomId,
        session_id: SessionId<'_>,
    ) -> Result<(), EventCacheError> {
        let Ok(client) = self.inner.client() else {
            return Ok(());
        };

        let Some(room) = client.get_room(room_id) else {
            return Ok(());
        };

        // Get all the relevant events.
        let events = self.get_decrypted_events(room_id, session_id).await?;

        // Let's attempt to update their encryption info.
        let mut updated_events = Vec::with_capacity(events.len());

        for (event_id, mut event) in events {
            let new_encryption_info =
                room.get_encryption_info(session_id, &event.encryption_info.sender).await;

            // Only create a replacement if the encryption info actually changed.
            if let Some(new_encryption_info) = new_encryption_info
                && event.encryption_info != new_encryption_info
            {
                event.encryption_info = new_encryption_info;
                updated_events.push((event_id, event, None));
            }
        }

        self.on_resolved_utds(room_id, updated_events).await?;

        Ok(())
    }

    /// Explicitly request the redecryption of a set of events.
    ///
    /// TODO: Explain when and why this might be useful.
    pub fn request_decryption(&self, request: DecryptionRetryRequest) {
        let _ =
            self.inner.redecryption_channels.decryption_request_sender.send(request).inspect_err(
                |_| warn!("Requesting a decryption while the redecryption task has been shut down"),
            );
    }

    /// Subscribe to reports that the redecryptor generates.
    ///
    /// TODO: Explain when the redecryptor might send such reports.
    pub fn subscrube_to_decryption_reports(
        &self,
    ) -> impl Stream<Item = Result<RedecryptorReport, BroadcastStreamRecvError>> {
        BroadcastStream::new(self.inner.redecryption_channels.utd_reporter.subscribe())
    }
}

pub(crate) struct Redecryptor {
    task: JoinHandle<()>,
}

impl Drop for Redecryptor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Redecryptor {
    /// Create a new [`Redecryptor`].
    ///
    /// This creates a task that listens to various streams and attempts to
    /// redecrypt UTDs that can be found inside the [`EventCache`].
    pub(super) fn new(
        cache: Weak<EventCacheInner>,
        receiver: UnboundedReceiver<DecryptionRetryRequest>,
    ) -> Self {
        let task = spawn(async {
            let request_redecryption_stream = UnboundedReceiverStream::new(receiver);

            Self::listen_for_room_keys_task(cache, request_redecryption_stream).await;
        });

        Self { task }
    }

    /// (Re)-subscribe to the room key stream from the [`OlmMachine`].
    ///
    /// This needs to happen any time this stream returns a `None` meaning that
    /// the sending part of the stream has been dropped.
    async fn subscribe_to_room_key_stream(
        cache: &Weak<EventCacheInner>,
    ) -> Option<(
        impl Stream<Item = Result<Vec<RoomKeyInfo>, BroadcastStreamRecvError>>,
        impl Stream<Item = Vec<RoomKeyWithheldInfo>>,
    )> {
        let event_cache = cache.upgrade()?;
        let client = event_cache.client().ok()?;
        let machine = client.olm_machine().await;

        machine.as_ref().map(|m| {
            (m.store().room_keys_received_stream(), m.store().room_keys_withheld_received_stream())
        })
    }

    fn upgrade_event_cache(cache: &Weak<EventCacheInner>) -> Option<EventCache> {
        cache.upgrade().map(|inner| EventCache { inner })
    }

    async fn redecryption_loop(
        cache: &Weak<EventCacheInner>,
        decryption_request_stream: &mut Pin<&mut impl Stream<Item = DecryptionRetryRequest>>,
    ) -> bool {
        let Some((room_key_stream, withheld_stream)) =
            Self::subscribe_to_room_key_stream(cache).await
        else {
            return false;
        };

        pin_mut!(room_key_stream);
        pin_mut!(withheld_stream);

        loop {
            tokio::select! {
                // An explicit request, presumably from the timeline, has been received to decrypt
                // events that were encrypted with a certain room key.
                Some(request) = decryption_request_stream.next() => {
                        let Some(cache) = Self::upgrade_event_cache(cache) else {
                            break false;
                        };

                        trace!(?request, "Received a redecryption request");

                        for session_id in request.utd_session_ids {
                            let _ = cache
                                .retry_decryption(&request.room_id, &session_id)
                                .await
                                .inspect_err(|e| warn!("Error redecrypting after an explicit request was received {e:?}"));
                        }

                        for session_id in request.refresh_info_session_ids {
                            let _ = cache.update_encryption_info(&request.room_id, &session_id).await.inspect_err(|e|
                                warn!(
                                    room_id = %request.room_id,
                                    session_id = session_id,
                                    "Unable to update the encryption info {e:?}",
                            ));
                        }
                }
                // The room key stream from the OlmMachine. Needs to be recreated every time we
                // receive a `None` from the stream.
                room_keys = room_key_stream.next() => {
                    match room_keys {
                        Some(Ok(room_keys)) => {
                            // Alright, some room keys were received and persisted in our store,
                            // let's attempt to redecrypt events that were encrypted using these
                            // room keys.
                            let Some(cache) = Self::upgrade_event_cache(cache) else {
                                break false;
                            };

                            trace!(?room_keys, "Received new room keys");

                            for key in &room_keys {
                                let _ = cache
                                    .retry_decryption(&key.room_id, &key.session_id)
                                    .await
                                    .inspect_err(|e| warn!("Error redecrypting {e:?}"));
                            }

                            for key in room_keys {
                                let _ = cache.update_encryption_info(&key.room_id, &key.session_id).await.inspect_err(|e|
                                    warn!(
                                        room_id = %key.room_id,
                                        session_id = key.session_id,
                                        "Unable to update the encryption info {e:?}",
                                ));
                            }
                        },
                        Some(Err(_)) => {
                            // We missed some room keys, we need to report this in case a listener
                            // has and idea which UTDs we should attempt to redecrypt.
                            //
                            // This would most likely be the timeline. The timeline might attempt
                            // to redecrypt all UTDs it is showing to the user.
                            let Some(cache) = Self::upgrade_event_cache(cache) else {
                                break false;
                            };

                            let message = RedecryptorReport::Lagging;
                            let _ = cache.inner.redecryption_channels.utd_reporter.send(message);
                        },
                        // The stream got closed, this could mean that our OlmMachine got
                        // regenerated, let's return true and try to recreate the stream.
                        None => {
                            break true
                        }
                    }
                }
                withheld_info = withheld_stream.next() => {
                    match withheld_info {
                        Some(infos) => {
                            let Some(cache) = Self::upgrade_event_cache(cache) else {
                                break false;
                            };

                            trace!(?infos, "Received new withheld infos");

                            for RoomKeyWithheldInfo { room_id, session_id, .. } in &infos {
                                let _ = cache.update_encryption_info(room_id, session_id).await.inspect_err(|e|
                                    warn!(
                                        room_id = %room_id,
                                        session_id = session_id,
                                        "Unable to update the encryption info {e:?}",
                                ));
                            }
                        }
                        // The stream got closed, same as for the room key stream, we'll try to
                        // recreate the streams.
                        None => break true
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
        // We pin the decryption request stream here since that one doesn't need to be
        // recreated and we don't want to miss messages coming from the stream
        // while recreating it unnecessarily.
        pin_mut!(decryption_request_stream);

        while Self::redecryption_loop(&cache, &mut decryption_request_stream).await {
            info!("Regenerating the re-decryption streams");

            let Some(cache) = Self::upgrade_event_cache(&cache) else {
                break;
            };

            // Report that the stream got recreated so listeners can attempt to redecrypt
            // any UTDs they might be seeing.
            let message = RedecryptorReport::Lagging;
            let _ = cache.inner.redecryption_channels.utd_reporter.send(message);
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
