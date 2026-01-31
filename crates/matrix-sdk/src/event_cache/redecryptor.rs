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

//! The Redecryptor (affectionately known as R2D2) is a layer and long-running
//! background task which handles redecryption of events in case we couldn't
//! decrypt them immediately.
//!
//! There are various reasons why a room key might not be available immediately
//! when the event becomes available:
//!     - The to-device message containing the room key just arrives late, i.e.
//!       after the room event.
//!     - The event is a historic event and we need to first download the room
//!       key from the backup.
//!     - The event is a historic event in a previously unjoined room, we need
//!       to receive historic room keys as defined in [MSC3061].
//!
//! R2D2 listens to the [`OlmMachine`] for received room keys and new
//! m.room_key.withheld events.
//!
//! If a new room key has been received, it attempts to find any UTDs in the
//! [`EventCache`]. If R2D2 decrypts any UTDs from the event cache, it will
//! replace the events in the cache and send out new [`RoomEventCacheUpdate`]s
//! to any of its listeners.
//!
//! If a new withheld info has been received, it attempts to find any relevant
//! events and updates the [`EncryptionInfo`] of an event.
//!
//! There's an additional gotcha: the [`OlmMachine`] might get recreated by
//! calls to [`BaseClient::regenerate_olm()`]. When this happens, we will
//! receive a `None` on the room keys stream and we need to re-listen to it.
//!
//! Another gotcha is that room keys might be received on another process if the
//! [`Client`] is operating on a Apple iOS device. A separate process is used
//! in this case to receive push notifications. In this case, the room key will
//! be received and R2D2 won't get notified about it. To work around this,
//! decryption requests can be explicitly sent to R2D2.
//!
//! The final gotcha is that a room key might be received just in between the
//! time the event was initially tried to be decrypted and the time it took to
//! persist it in the event cache. To handle this race condition, R2D2 listens
//! to the event cache and attempts to decrypt any UTDs the event cache
//! persists.
//!
//! In the graph below, the Timeline block is meant to be the `Timeline` from
//! the `matrix-sdk-ui` crate, but it could be any other listener that
//! subscribes to [`RedecryptorReport`] stream.
//!
//! ```markdown
//! 
//!      .----------------------.
//!     |                        |
//!     |      Beeb, boop!       |
//!     |                        .
//!      ----------------------._ \
//!                               -;  _____
//!                                 .`/L|__`.
//!                                / =[_]O|` \
//!                                |"+_____":|
//!                              __:='|____`-:__
//!                             ||[] ||====|| []||
//!                             ||[] ||====|| []||
//!                             |:== ||====|| ==:|
//!                             ||[] ||====|| []||
//!                             ||[] ||====|| []||
//!                            _||_  ||====||  _||_
//!                           (====) |:====:| (====)
//!                            }--{  | |  | |  }--{
//!                           (____) |_|  |_| (____)
//!
//!                              ┌─────────────┐
//!                              │             │
//!                  ┌───────────┤   Timeline  │◄────────────┐
//!                  │           │             │             │
//!                  │           └──────▲──────┘             │
//!                  │                  │                    │
//!                  │                  │                    │
//!                  │                  │                    │
//!              Decryption             │                Redecryptor
//!                request              │                  report
//!                  │        RoomEventCacheUpdates          │
//!                  │                  │                    │
//!                  │                  │                    │
//!                  │      ┌───────────┴───────────┐        │
//!                  │      │                       │        │
//!                  └──────►         R2D2          │────────┘
//!                         │                       │
//!                         └──▲─────────────────▲──┘
//!                            │                 │
//!                            │                 │
//!                            │                 │
//!                         Received        Received room
//!                          events          keys stream
//!                            │                 │
//!                            │                 │
//!                            │                 │
//!                    ┌───────┴──────┐  ┌───────┴──────┐
//!                    │              │  │              │
//!                    │  Event Cache │  │  OlmMachine  │
//!                    │              │  │              │
//!                    └──────────────┘  └──────────────┘
//! ```
//!
//! [MSC3061]: https://github.com/matrix-org/matrix-spec/pull/1655#issuecomment-2213152255

use std::{
    collections::{BTreeMap, BTreeSet},
    pin::Pin,
    sync::Weak,
};

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
    event_cache::store::EventCacheStoreLockState,
    locks::Mutex,
    timer,
};
#[cfg(doc)]
use matrix_sdk_common::deserialized_responses::EncryptionInfo;
use matrix_sdk_common::executor::{AbortOnDrop, JoinHandleExt, spawn};
use ruma::{
    OwnedEventId, OwnedRoomId, RoomId,
    events::{AnySyncTimelineEvent, room::encrypted::OriginalSyncRoomEncryptedEvent},
    push::Action,
    serde::Raw,
};
use tokio::sync::{
    broadcast::{self, Sender},
    mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
};
use tokio_stream::wrappers::{
    BroadcastStream, UnboundedReceiverStream, errors::BroadcastStreamRecvError,
};
use tracing::{info, instrument, trace, warn};

#[cfg(doc)]
use super::RoomEventCache;
use super::{EventCache, EventCacheError, EventCacheInner, EventsOrigin, RoomEventCacheUpdate};
use crate::{
    Client, Result, Room,
    encryption::backups::BackupState,
    event_cache::{RoomEventCacheGenericUpdate, RoomEventCacheLinkedChunkUpdate},
    room::PushContext,
};

type SessionId<'a> = &'a str;
type OwnedSessionId = String;

type EventIdAndUtd = (OwnedEventId, Raw<AnySyncTimelineEvent>);
type EventIdAndEvent = (OwnedEventId, DecryptedRoomEvent);
type ResolvedUtd = (OwnedEventId, DecryptedRoomEvent, Option<Vec<Action>>);

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
    /// A room key backup has become available.
    ///
    /// This means that components might want to tell R2D2 about events they
    /// care about to attempt a decryption.
    BackupAvailable,
}

pub(super) struct RedecryptorChannels {
    utd_reporter: Sender<RedecryptorReport>,
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

/// A function which can be used to filter and map [`TimelineEvent`]s into a
/// tuple of event ID and raw [`AnySyncTimelineEvent`].
///
/// The tuple can be used to attempt to redecrypt events.
fn filter_timeline_event_to_utd(
    event: TimelineEvent,
) -> Option<(OwnedEventId, Raw<AnySyncTimelineEvent>)> {
    let event_id = event.event_id();

    // Only pick out events that are UTDs, get just the Raw event as this is what
    // the OlmMachine needs.
    let event = as_variant!(event.kind, TimelineEventKind::UnableToDecrypt { event, .. } => event);
    // Zip the event ID and event together so we don't have to pick out the event ID
    // again. We need the event ID to replace the event in the cache.
    event_id.zip(event)
}

/// A function which can be used to filter an map [`TimelineEvent`]s into a
/// tuple of event ID and [`DecryptedRoomEvent`].
///
/// The tuple can be used to attempt to update the encryption info of the
/// decrypted event.
fn filter_timeline_event_to_decrypted(
    event: TimelineEvent,
) -> Option<(OwnedEventId, DecryptedRoomEvent)> {
    let event_id = event.event_id();

    let event = as_variant!(event.kind, TimelineEventKind::Decrypted(event) => event);
    // Zip the event ID and event together so we don't have to pick out the event ID
    // again. We need the event ID to replace the event in the cache.
    event_id.zip(event)
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
    ) -> Result<Vec<EventIdAndUtd>, EventCacheError> {
        let events = match self.inner.store.lock().await? {
            // If the lock is clean, no problem.
            // If the lock is dirty, it doesn't really matter as we are hitting the store
            // directly, there is no in-memory state to manage, so all good. Do not mark the lock as
            // non-dirty.
            EventCacheStoreLockState::Clean(guard) | EventCacheStoreLockState::Dirty(guard) => {
                guard.get_room_events(room_id, Some("m.room.encrypted"), Some(session_id)).await?
            }
        };

        Ok(events.into_iter().filter_map(filter_timeline_event_to_utd).collect())
    }

    /// Retrieve a set of events that we weren't able to decrypt from the memory
    /// of the event cache.
    async fn get_utds_from_memory(&self) -> BTreeMap<OwnedRoomId, Vec<EventIdAndUtd>> {
        let mut utds = BTreeMap::new();

        for (room_id, room_cache) in self.inner.by_room.read().await.iter() {
            let room_utds: Vec<_> = room_cache
                .events()
                .await
                .into_iter()
                .flatten()
                .filter_map(filter_timeline_event_to_utd)
                .collect();

            utds.insert(room_id.to_owned(), room_utds);
        }

        utds
    }

    async fn get_decrypted_events(
        &self,
        room_id: &RoomId,
        session_id: SessionId<'_>,
    ) -> Result<Vec<EventIdAndEvent>, EventCacheError> {
        let events = match self.inner.store.lock().await? {
            // If the lock is clean, no problem.
            // If the lock is dirty, it doesn't really matter as we are hitting the store
            // directly, there is no in-memory state to manage, so all good. Do not mark the lock as
            // non-dirty.
            EventCacheStoreLockState::Clean(guard) | EventCacheStoreLockState::Dirty(guard) => {
                guard.get_room_events(room_id, None, Some(session_id)).await?
            }
        };

        Ok(events.into_iter().filter_map(filter_timeline_event_to_decrypted).collect())
    }

    async fn get_decrypted_events_from_memory(
        &self,
    ) -> BTreeMap<OwnedRoomId, Vec<EventIdAndEvent>> {
        let mut decrypted_events = BTreeMap::new();

        for (room_id, room_cache) in self.inner.by_room.read().await.iter() {
            let room_utds: Vec<_> = room_cache
                .events()
                .await
                .into_iter()
                .flatten()
                .filter_map(filter_timeline_event_to_decrypted)
                .collect();

            decrypted_events.insert(room_id.to_owned(), room_utds);
        }

        decrypted_events
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
        events: Vec<ResolvedUtd>,
    ) -> Result<(), EventCacheError> {
        if events.is_empty() {
            trace!("No events were redecrypted or updated, nothing to replace");
            return Ok(());
        }

        timer!("Resolving UTDs");

        // Get the cache for this particular room and lock the state for the duration of
        // the decryption.
        let (room_cache, _drop_handles) = self.for_room(room_id).await?;
        let mut state = room_cache.inner.state.write().await?;

        let event_ids: BTreeSet<_> =
            events.iter().cloned().map(|(event_id, _, _)| event_id).collect();
        let mut new_events = Vec::with_capacity(events.len());

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
                state.replace_event_at(location, target_event.clone()).await?;
                new_events.push(target_event);
            }
        }

        state.post_process_new_events(new_events, false).await?;

        // We replaced a bunch of events, reactive updates for those replacements have
        // been queued up. We need to send them out to our subscribers now.
        let diffs = state.room_linked_chunk().updates_as_vector_diffs();

        let _ = room_cache.inner.update_sender.send(RoomEventCacheUpdate::UpdateTimelineEvents {
            diffs,
            origin: EventsOrigin::Cache,
        });

        let _ = room_cache
            .inner
            .generic_update_sender
            .send(RoomEventCacheGenericUpdate { room_id: room_id.to_owned() });

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
    #[instrument(skip_all, fields(room_id, session_id))]
    async fn retry_decryption(
        &self,
        room_id: &RoomId,
        session_id: SessionId<'_>,
    ) -> Result<(), EventCacheError> {
        // Get all the relevant UTDs.
        let events = self.get_utds(room_id, session_id).await?;
        self.retry_decryption_for_events(room_id, events).await
    }

    /// Attempt to redecrypt events that were persisted in the event cache.
    #[instrument(skip_all, fields(updates.linked_chunk_id))]
    async fn retry_decryption_for_event_cache_updates(
        &self,
        updates: RoomEventCacheLinkedChunkUpdate,
    ) -> Result<(), EventCacheError> {
        let room_id = updates.linked_chunk_id.room_id();
        let events: Vec<_> = updates
            .updates
            .into_iter()
            .flat_map(|updates| updates.into_items())
            .filter_map(filter_timeline_event_to_utd)
            .collect();

        self.retry_decryption_for_events(room_id, events).await
    }

    async fn retry_decryption_for_in_memory_events(&self) {
        let utds = self.get_utds_from_memory().await;

        for (room_id, utds) in utds.into_iter() {
            if let Err(e) = self.retry_decryption_for_events(&room_id, utds).await {
                warn!(%room_id, "Failed to redecrypt in-memory events {e:?}");
            }
        }
    }

    /// Attempt to redecrypt a chunk of UTDs.
    #[instrument(skip_all, fields(room_id, session_id))]
    async fn retry_decryption_for_events(
        &self,
        room_id: &RoomId,
        events: Vec<EventIdAndUtd>,
    ) -> Result<(), EventCacheError> {
        trace!("Retrying to decrypt");

        if events.is_empty() {
            trace!("No relevant events found.");
            return Ok(());
        }

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

        let event_ids: BTreeSet<_> =
            decrypted_events.iter().map(|(event_id, _, _)| event_id).collect();

        if !event_ids.is_empty() {
            trace!(?event_ids, "Successfully redecrypted events");
        }

        // Replace the events and notify listeners that UTDs have been replaced with
        // decrypted events.
        self.on_resolved_utds(room_id, decrypted_events).await?;

        Ok(())
    }

    /// Attempt to update the encryption info for the given list of events.
    async fn update_encryption_info_for_events(
        &self,
        room: &Room,
        events: Vec<EventIdAndEvent>,
    ) -> Result<(), EventCacheError> {
        // Let's attempt to update their encryption info.
        let mut updated_events = Vec::with_capacity(events.len());

        for (event_id, mut event) in events {
            if let Some(session_id) = event.encryption_info.session_id() {
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
        }

        let event_ids: BTreeSet<_> =
            updated_events.iter().map(|(event_id, _, _)| event_id).collect();

        if !event_ids.is_empty() {
            trace!(?event_ids, "Replacing the encryption info of some events");
        }

        self.on_resolved_utds(room.room_id(), updated_events).await
    }

    #[instrument(skip_all, fields(room_id, session_id))]
    async fn update_encryption_info(
        &self,
        room_id: &RoomId,
        session_id: SessionId<'_>,
    ) -> Result<(), EventCacheError> {
        trace!("Updating encryption info");

        let Ok(client) = self.inner.client() else {
            return Ok(());
        };

        let Some(room) = client.get_room(room_id) else {
            return Ok(());
        };

        // Get all the relevant events.
        let events = self.get_decrypted_events(room_id, session_id).await?;

        if events.is_empty() {
            trace!("No relevant events found.");
            return Ok(());
        }

        // Let's attempt to update their encryption info.
        self.update_encryption_info_for_events(&room, events).await
    }

    async fn retry_update_encryption_info_for_in_memory_events(&self) {
        let decrypted_events = self.get_decrypted_events_from_memory().await;

        for (room_id, events) in decrypted_events.into_iter() {
            let Some(room) = self.inner.client().ok().and_then(|c| c.get_room(&room_id)) else {
                continue;
            };

            if let Err(e) = self.update_encryption_info_for_events(&room, events).await {
                warn!(
                    %room_id,
                    "Failed to replace the encryption info for in-memory events {e:?}"
                );
            }
        }
    }

    /// Retry to decrypt and update the encryption info of all the events
    /// contained in the memory part of the event cache.
    ///
    /// This list of events will map one-to-one to the events components
    /// subscribed to the event cache are have received and are keeping cached.
    ///
    /// If components subscribed to the event cache are doing additional
    /// caching, they'll need to listen to [RedecryptorReport]s and
    /// explicitly request redecryption attempts using
    /// [EventCache::request_decryption].
    async fn retry_in_memory_events(&self) {
        self.retry_decryption_for_in_memory_events().await;
        self.retry_update_encryption_info_for_in_memory_events().await;
    }

    /// Explicitly request the redecryption of a set of events.
    ///
    /// The redecryption logic in the event cache might sometimes miss that a
    /// room key has become available and that a certain set of events has
    /// become decryptable.
    ///
    /// This might happen because some room keys might arrive in a separate
    /// process handling push notifications or if a room key arrives but the
    /// process shuts down before we could have decrypted the events.
    ///
    /// For this reason it is useful to tell the event cache explicitly that
    /// some events should be retried to be redecrypted.
    ///
    /// This method allows you to do so. The events that get decrypted, if any,
    /// will be advertised over the usual event cache subscription mechanism
    /// which can be accessed using the [`RoomEventCache::subscribe()`]
    /// method.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, event_cache::DecryptionRetryRequest};
    /// # use url::Url;
    /// # use ruma::owned_room_id;
    /// # use std::collections::BTreeSet;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// let event_cache = client.event_cache();
    /// let room_id = owned_room_id!("!my_room:localhost");
    ///
    /// let request = DecryptionRetryRequest {
    ///     room_id,
    ///     utd_session_ids: BTreeSet::from(["session_id".into()]),
    ///     refresh_info_session_ids: BTreeSet::new(),
    /// };
    ///
    /// event_cache.request_decryption(request);
    /// # anyhow::Ok(()) };
    /// ```
    pub fn request_decryption(&self, request: DecryptionRetryRequest) {
        let _ =
            self.inner.redecryption_channels.decryption_request_sender.send(request).inspect_err(
                |_| warn!("Requesting a decryption while the redecryption task has been shut down"),
            );
    }

    /// Subscribe to reports that the redecryptor generates.
    ///
    /// The redecryption logic in the event cache might sometimes miss that a
    /// room key has become available and that a certain set of events has
    /// become decryptable.
    ///
    /// This might happen because some room keys might arrive in a separate
    /// process handling push notifications or if room keys arrive faster than
    /// we can handle them.
    ///
    /// This stream can be used to get notified about such situations as well as
    /// a general channel where the event cache reports which events got
    /// successfully redecrypted.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, event_cache::RedecryptorReport};
    /// # use url::Url;
    /// # use tokio_stream::StreamExt;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// let event_cache = client.event_cache();
    ///
    /// let mut stream = event_cache.subscribe_to_decryption_reports();
    ///
    /// while let Some(Ok(report)) = stream.next().await {
    ///     match report {
    ///         RedecryptorReport::Lagging => {
    ///             // The event cache might have missed to redecrypt some events. We should tell
    ///             // it which events we care about, i.e. which events we're displaying to the
    ///             // user, and let it redecrypt things with an explicit request.
    ///         }
    ///         RedecryptorReport::BackupAvailable => {
    ///             // A backup has become available. We can, just like in the Lagging case, tell
    ///             // the event cache to attempt to redecrypt some events.
    ///             //
    ///             // This is only necessary with the BackupDownloadStrategy::OnDecryptionFailure
    ///             // as the decryption attempt in this case will trigger the download of the
    ///             // room key from the backup.
    ///         }
    ///         RedecryptorReport::ResolvedUtds { .. } => {
    ///             // This may be interesting for statistical reasons or in case we'd like to
    ///             // fetch and inspect these events in some manner.
    ///         }
    ///     }
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub fn subscribe_to_decryption_reports(
        &self,
    ) -> impl Stream<Item = Result<RedecryptorReport, BroadcastStreamRecvError>> {
        BroadcastStream::new(self.inner.redecryption_channels.utd_reporter.subscribe())
    }
}

#[inline(always)]
fn upgrade_event_cache(cache: &Weak<EventCacheInner>) -> Option<EventCache> {
    cache.upgrade().map(|inner| EventCache { inner })
}

async fn send_report_and_retry_memory_events(
    cache: &Weak<EventCacheInner>,
    report: RedecryptorReport,
) -> Result<(), ()> {
    let Some(cache) = upgrade_event_cache(cache) else {
        return Err(());
    };

    cache.retry_in_memory_events().await;
    let _ = cache.inner.redecryption_channels.utd_reporter.send(report);

    Ok(())
}

/// Struct holding on to the redecryption task.
///
/// This struct implements the bulk of the redecryption task. It listens to the
/// various streams that should trigger redecryption attempts.
///
/// For more info see the [module level docs](self).
pub(crate) struct Redecryptor {
    _task: AbortOnDrop<()>,
}

impl Redecryptor {
    /// Create a new [`Redecryptor`].
    ///
    /// This creates a task that listens to various streams and attempts to
    /// redecrypt UTDs that can be found inside the [`EventCache`].
    pub(super) fn new(
        client: &Client,
        cache: Weak<EventCacheInner>,
        receiver: UnboundedReceiver<DecryptionRetryRequest>,
        linked_chunk_update_sender: &Sender<RoomEventCacheLinkedChunkUpdate>,
    ) -> Self {
        let linked_chunk_stream = BroadcastStream::new(linked_chunk_update_sender.subscribe());
        let backup_state_stream = client.encryption().backups().state_stream();

        let task = spawn(async {
            let request_redecryption_stream = UnboundedReceiverStream::new(receiver);

            Self::listen_for_room_keys_task(
                cache,
                request_redecryption_stream,
                linked_chunk_stream,
                backup_state_stream,
            )
            .await;
        })
        .abort_on_drop();

        Self { _task: task }
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

    async fn redecryption_loop(
        cache: &Weak<EventCacheInner>,
        decryption_request_stream: &mut Pin<&mut impl Stream<Item = DecryptionRetryRequest>>,
        events_stream: &mut Pin<
            &mut impl Stream<Item = Result<RoomEventCacheLinkedChunkUpdate, BroadcastStreamRecvError>>,
        >,
        backup_state_stream: &mut Pin<
            &mut impl Stream<Item = Result<BackupState, BroadcastStreamRecvError>>,
        >,
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
                        let Some(cache) = upgrade_event_cache(cache) else {
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
                            let Some(cache) = upgrade_event_cache(cache) else {
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
                            // This would most likely be the timeline from the UI crate. The
                            // timeline might attempt to redecrypt all UTDs it is showing to the
                            // user.
                            warn!("The room key stream lagged, reporting the lag to our listeners");

                            if send_report_and_retry_memory_events(cache, RedecryptorReport::Lagging).await.is_err() {
                                break false;
                            }
                        },
                        // The stream got closed, this could mean that our OlmMachine got
                        // regenerated, let's return true and try to recreate the stream.
                        None => {
                            break true;
                        }
                    }
                }
                withheld_info = withheld_stream.next() => {
                    match withheld_info {
                        Some(infos) => {
                            let Some(cache) = upgrade_event_cache(cache) else {
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
                        None => break true,
                    }
                }
                // Events that the event cache handled. If the event cache received any UTDs, let's
                // attempt to redecrypt them in case the room key was received before the event
                // cache was able to return them using `get_utds()`.
                Some(event_updates) = events_stream.next() => {
                    match event_updates {
                        Ok(updates) => {
                            let Some(cache) = upgrade_event_cache(cache) else {
                                break false;
                            };

                            let linked_chunk_id = updates.linked_chunk_id.to_owned();

                            let _ = cache.retry_decryption_for_event_cache_updates(updates).await.inspect_err(|e|
                                warn!(
                                    %linked_chunk_id,
                                    "Unable to handle UTDs from event cache updates {e:?}",
                                )
                            );
                        }
                        Err(_) => {
                            if send_report_and_retry_memory_events(cache, RedecryptorReport::Lagging).await.is_err() {
                                break false;
                            }
                        }
                    }
                }
                Some(backup_state_update) = backup_state_stream.next() => {
                    match backup_state_update {
                        Ok(state) => {
                            match state {
                                BackupState::Unknown |
                                BackupState::Creating |
                                BackupState::Enabling |
                                BackupState::Resuming |
                                BackupState::Downloading |
                                BackupState::Disabling =>{
                                    // Those states aren't particularly interesting to components
                                    // listening to R2D2 reports.
                                }
                                BackupState::Enabled => {
                                    // Alright, the backup got enabled, we might or might not have
                                    // downloaded the room keys from the backup. In case they get
                                    // downloaded on-demand, let's try to decrypt all the events we
                                    // have cached in-memory.
                                    if send_report_and_retry_memory_events(cache, RedecryptorReport::BackupAvailable).await.is_err() {
                                        break false;
                                    }
                                }
                            }
                        }
                        Err(_) => {
                            if send_report_and_retry_memory_events(cache, RedecryptorReport::Lagging).await.is_err() {
                                break false;
                            }
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
        events_stream: BroadcastStream<RoomEventCacheLinkedChunkUpdate>,
        backup_state_stream: impl Stream<Item = Result<BackupState, BroadcastStreamRecvError>>,
    ) {
        // We pin the decryption request stream here since that one doesn't need to be
        // recreated and we don't want to miss messages coming from the stream
        // while recreating it unnecessarily.
        pin_mut!(decryption_request_stream);
        pin_mut!(events_stream);
        pin_mut!(backup_state_stream);

        while Self::redecryption_loop(
            &cache,
            &mut decryption_request_stream,
            &mut events_stream,
            &mut backup_state_stream,
        )
        .await
        {
            info!("Regenerating the re-decryption streams");

            // Report that the stream got recreated so listeners know about it, at the same
            // time retry to decrypt anything we have cached in memory.
            if send_report_and_retry_memory_events(&cache, RedecryptorReport::Lagging)
                .await
                .is_err()
            {
                break;
            }
        }

        info!("Shutting down the event cache redecryptor");
    }
}

#[cfg(not(target_family = "wasm"))]
#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use assert_matches2::assert_matches;
    use async_trait::async_trait;
    use eyeball_im::VectorDiff;
    use matrix_sdk_base::{
        cross_process_lock::CrossProcessLockGeneration,
        crypto::types::events::{ToDeviceEvent, room::encrypted::ToDeviceEncryptedEventContent},
        deserialized_responses::{TimelineEventKind, VerificationState},
        event_cache::{
            Event, Gap,
            store::{EventCacheStore, EventCacheStoreError, MemoryStore},
        },
        linked_chunk::{
            ChunkIdentifier, ChunkIdentifierGenerator, ChunkMetadata, LinkedChunkId, Position,
            RawChunk, Update,
        },
        locks::Mutex,
        sleep::sleep,
        store::StoreConfig,
    };
    use matrix_sdk_test::{
        JoinedRoomBuilder, StateTestEvent, async_test, event_factory::EventFactory,
    };
    use ruma::{
        EventId, OwnedEventId, RoomId, device_id, event_id,
        events::{AnySyncTimelineEvent, relation::RelationType},
        room_id,
        serde::Raw,
        user_id,
    };
    use serde_json::json;
    use tokio::sync::oneshot::{self, Sender};
    use tracing::{Instrument, info};

    use crate::{
        Client, assert_let_timeout,
        encryption::EncryptionSettings,
        event_cache::{DecryptionRetryRequest, RoomEventCacheGenericUpdate, RoomEventCacheUpdate},
        test_utils::mocks::MatrixMockServer,
    };

    /// A wrapper for the memory store for the event cache.
    ///
    /// Delays the persisting of events, or linked chunk updates, to allow the
    /// testing of race conditions between the event cache and R2D2.
    #[derive(Debug, Clone)]
    struct DelayingStore {
        memory_store: MemoryStore,
        delaying: Arc<AtomicBool>,
        foo: Arc<Mutex<Option<Sender<()>>>>,
    }

    impl DelayingStore {
        fn new() -> Self {
            Self {
                memory_store: MemoryStore::new(),
                delaying: AtomicBool::new(true).into(),
                foo: Arc::new(Mutex::new(None)),
            }
        }

        async fn stop_delaying(&self) {
            let (sender, receiver) = oneshot::channel();

            {
                *self.foo.lock() = Some(sender);
            }

            self.delaying.store(false, Ordering::SeqCst);

            receiver.await.expect("We should be able to receive a response")
        }
    }

    #[cfg_attr(target_family = "wasm", async_trait(?Send))]
    #[cfg_attr(not(target_family = "wasm"), async_trait)]
    impl EventCacheStore for DelayingStore {
        type Error = EventCacheStoreError;

        async fn try_take_leased_lock(
            &self,
            lease_duration_ms: u32,
            key: &str,
            holder: &str,
        ) -> Result<Option<CrossProcessLockGeneration>, Self::Error> {
            self.memory_store.try_take_leased_lock(lease_duration_ms, key, holder).await
        }

        async fn handle_linked_chunk_updates(
            &self,
            linked_chunk_id: LinkedChunkId<'_>,
            updates: Vec<Update<Event, Gap>>,
        ) -> Result<(), Self::Error> {
            // This is the key behaviour of this store - we wait to set this value until
            // someone calls `stop_delaying`.
            //
            // We use `sleep` here for simplicity. The cool way would be to use a custom
            // waker or something like that.
            while self.delaying.load(Ordering::SeqCst) {
                sleep(Duration::from_millis(10)).await;
            }

            let sender = self.foo.lock().take();
            let ret = self.memory_store.handle_linked_chunk_updates(linked_chunk_id, updates).await;

            if let Some(sender) = sender {
                sender.send(()).expect("We should be able to notify the other side that we're done with the storage operation");
            }

            ret
        }

        async fn load_all_chunks(
            &self,
            linked_chunk_id: LinkedChunkId<'_>,
        ) -> Result<Vec<RawChunk<Event, Gap>>, Self::Error> {
            self.memory_store.load_all_chunks(linked_chunk_id).await
        }

        async fn load_all_chunks_metadata(
            &self,
            linked_chunk_id: LinkedChunkId<'_>,
        ) -> Result<Vec<ChunkMetadata>, Self::Error> {
            self.memory_store.load_all_chunks_metadata(linked_chunk_id).await
        }

        async fn load_last_chunk(
            &self,
            linked_chunk_id: LinkedChunkId<'_>,
        ) -> Result<(Option<RawChunk<Event, Gap>>, ChunkIdentifierGenerator), Self::Error> {
            self.memory_store.load_last_chunk(linked_chunk_id).await
        }

        async fn load_previous_chunk(
            &self,
            linked_chunk_id: LinkedChunkId<'_>,
            before_chunk_identifier: ChunkIdentifier,
        ) -> Result<Option<RawChunk<Event, Gap>>, Self::Error> {
            self.memory_store.load_previous_chunk(linked_chunk_id, before_chunk_identifier).await
        }

        async fn clear_all_linked_chunks(&self) -> Result<(), Self::Error> {
            self.memory_store.clear_all_linked_chunks().await
        }

        async fn filter_duplicated_events(
            &self,
            linked_chunk_id: LinkedChunkId<'_>,
            events: Vec<OwnedEventId>,
        ) -> Result<Vec<(OwnedEventId, Position)>, Self::Error> {
            self.memory_store.filter_duplicated_events(linked_chunk_id, events).await
        }

        async fn find_event(
            &self,
            room_id: &RoomId,
            event_id: &EventId,
        ) -> Result<Option<Event>, Self::Error> {
            self.memory_store.find_event(room_id, event_id).await
        }

        async fn find_event_relations(
            &self,
            room_id: &RoomId,
            event_id: &EventId,
            filters: Option<&[RelationType]>,
        ) -> Result<Vec<(Event, Option<Position>)>, Self::Error> {
            self.memory_store.find_event_relations(room_id, event_id, filters).await
        }

        async fn get_room_events(
            &self,
            room_id: &RoomId,
            event_type: Option<&str>,
            session_id: Option<&str>,
        ) -> Result<Vec<Event>, Self::Error> {
            self.memory_store.get_room_events(room_id, event_type, session_id).await
        }

        async fn save_event(&self, room_id: &RoomId, event: Event) -> Result<(), Self::Error> {
            self.memory_store.save_event(room_id, event).await
        }

        async fn optimize(&self) -> Result<(), Self::Error> {
            self.memory_store.optimize().await
        }

        async fn get_size(&self) -> Result<Option<usize>, Self::Error> {
            self.memory_store.get_size().await
        }
    }

    async fn set_up_clients(
        room_id: &RoomId,
        alice_enables_cross_signing: bool,
        use_delayed_store: bool,
    ) -> (Client, Client, MatrixMockServer, Option<DelayingStore>) {
        let alice_span = tracing::info_span!("alice");
        let bob_span = tracing::info_span!("bob");

        let alice_user_id = user_id!("@alice:localhost");
        let alice_device_id = device_id!("ALICEDEVICE");
        let bob_user_id = user_id!("@bob:localhost");
        let bob_device_id = device_id!("BOBDEVICE");

        let matrix_mock_server = MatrixMockServer::new().await;
        matrix_mock_server.mock_crypto_endpoints_preset().await;

        let encryption_settings = EncryptionSettings {
            auto_enable_cross_signing: alice_enables_cross_signing,
            ..Default::default()
        };

        // Create some clients for Alice and Bob.

        let alice = matrix_mock_server
            .client_builder_for_crypto_end_to_end(alice_user_id, alice_device_id)
            .on_builder(|builder| {
                builder
                    .with_enable_share_history_on_invite(true)
                    .with_encryption_settings(encryption_settings)
            })
            .build()
            .instrument(alice_span.clone())
            .await;

        let encryption_settings =
            EncryptionSettings { auto_enable_cross_signing: true, ..Default::default() };

        let (store_config, store) = if use_delayed_store {
            let store = DelayingStore::new();

            (
                StoreConfig::new("delayed_store_event_cache_test".into())
                    .event_cache_store(store.clone()),
                Some(store),
            )
        } else {
            (StoreConfig::new("normal_store_event_cache_test".into()), None)
        };

        let bob = matrix_mock_server
            .client_builder_for_crypto_end_to_end(bob_user_id, bob_device_id)
            .on_builder(|builder| {
                builder
                    .with_enable_share_history_on_invite(true)
                    .with_encryption_settings(encryption_settings)
                    .store_config(store_config)
            })
            .build()
            .instrument(bob_span.clone())
            .await;

        bob.event_cache().subscribe().expect("Bob should be able to enable the event cache");

        // Ensure that Alice and Bob are aware of their devices and identities.
        matrix_mock_server.exchange_e2ee_identities(&alice, &bob).await;

        // Let us now create a room for them.
        let room_builder = JoinedRoomBuilder::new(room_id)
            .add_state_event(StateTestEvent::Create)
            .add_state_event(StateTestEvent::Encryption);

        matrix_mock_server
            .mock_sync()
            .ok_and_run(&alice, |builder| {
                builder.add_joined_room(room_builder.clone());
            })
            .instrument(alice_span)
            .await;

        matrix_mock_server
            .mock_sync()
            .ok_and_run(&bob, |builder| {
                builder.add_joined_room(room_builder);
            })
            .instrument(bob_span)
            .await;

        (alice, bob, matrix_mock_server, store)
    }

    async fn prepare_room(
        matrix_mock_server: &MatrixMockServer,
        event_factory: &EventFactory,
        alice: &Client,
        bob: &Client,
        room_id: &RoomId,
    ) -> (Raw<AnySyncTimelineEvent>, Raw<ToDeviceEvent<ToDeviceEncryptedEventContent>>) {
        let alice_user_id = alice.user_id().unwrap();
        let bob_user_id = bob.user_id().unwrap();

        let alice_member_event = event_factory.member(alice_user_id).into_raw();
        let bob_member_event = event_factory.member(bob_user_id).into_raw();

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

        // Let us retrieve the captured event and to-device message.
        let event = event_receiver.await.expect("Alice should have sent the event by now");
        let room_key = room_key.await;

        (event, room_key)
    }

    #[async_test]
    async fn test_redecryptor() {
        let room_id = room_id!("!test:localhost");

        let event_factory = EventFactory::new().room(room_id);
        let (alice, bob, matrix_mock_server, _) = set_up_clients(room_id, true, false).await;

        let (event, room_key) =
            prepare_room(&matrix_mock_server, &event_factory, &alice, &bob, room_id).await;

        // Let's now see what Bob's event cache does.

        let event_cache = bob.event_cache();
        let (room_cache, _) = event_cache
            .for_room(room_id)
            .await
            .expect("We should be able to get to the event cache for a specific room");

        let (_, mut subscriber) = room_cache.subscribe().await.unwrap();
        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();

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

        assert_let_timeout!(
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) = generic_stream.recv()
        );
        assert_eq!(expected_room_id, room_id);
        assert!(generic_stream.is_empty());

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

        assert_let_timeout!(
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) = generic_stream.recv()
        );
        assert_eq!(expected_room_id, room_id);
        assert!(generic_stream.is_empty());
    }

    #[async_test]
    async fn test_redecryptor_updating_encryption_info() {
        let bob_span = tracing::info_span!("bob");

        let room_id = room_id!("!test:localhost");

        let event_factory = EventFactory::new().room(room_id);
        let (alice, bob, matrix_mock_server, _) = set_up_clients(room_id, false, false).await;

        let (event, room_key) =
            prepare_room(&matrix_mock_server, &event_factory, &alice, &bob, room_id).await;

        // Let's now see what Bob's event cache does.

        let event_cache = bob.event_cache();
        let (room_cache, _) = event_cache
            .for_room(room_id)
            .instrument(bob_span.clone())
            .await
            .expect("We should be able to get to the event cache for a specific room");

        let (_, mut subscriber) = room_cache.subscribe().await.unwrap();
        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();

        // Let us forward the event to Bob.
        matrix_mock_server
            .mock_sync()
            .ok_and_run(&bob, |builder| {
                builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(event));
            })
            .instrument(bob_span.clone())
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

        assert_let_timeout!(
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) = generic_stream.recv()
        );
        assert_eq!(expected_room_id, room_id);
        assert!(generic_stream.is_empty());

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
            .instrument(bob_span.clone())
            .await;

        // Bob should receive a new update from the cache.
        assert_let_timeout!(
            Duration::from_secs(1),
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = subscriber.recv()
        );

        // It should replace the UTD with a decrypted event.
        assert_eq!(diffs.len(), 1);
        assert_matches!(&diffs[0], VectorDiff::Set { index: 0, value });
        assert_matches!(&value.kind, TimelineEventKind::Decrypted { .. });

        let encryption_info = value.encryption_info().unwrap();
        assert_matches!(&encryption_info.verification_state, VerificationState::Unverified(_));

        assert_let_timeout!(
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) = generic_stream.recv()
        );
        assert_eq!(expected_room_id, room_id);
        assert!(generic_stream.is_empty());

        let session_id = encryption_info.session_id().unwrap().to_owned();
        let alice_user_id = alice.user_id().unwrap();

        // Alice now creates the identity.
        alice
            .encryption()
            .bootstrap_cross_signing(None)
            .await
            .expect("Alice should be able to create the cross-signing keys");

        bob.update_tracked_users_for_testing([alice_user_id]).instrument(bob_span.clone()).await;
        matrix_mock_server
            .mock_sync()
            .ok_and_run(&bob, |builder| {
                builder.add_change_device(alice_user_id);
            })
            .instrument(bob_span.clone())
            .await;

        bob.event_cache().request_decryption(DecryptionRetryRequest {
            room_id: room_id.into(),
            utd_session_ids: BTreeSet::new(),
            refresh_info_session_ids: BTreeSet::from([session_id]),
        });

        // Bob should again receive a new update from the cache, this time updating the
        // encryption info.
        assert_let_timeout!(
            Duration::from_secs(1),
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = subscriber.recv()
        );

        assert_eq!(diffs.len(), 1);
        assert_matches!(&diffs[0], VectorDiff::Set { index: 0, value });
        assert_matches!(&value.kind, TimelineEventKind::Decrypted { .. });
        let encryption_info = value.encryption_info().unwrap();

        assert_matches!(
            &encryption_info.verification_state,
            VerificationState::Unverified(_),
            "The event should now know about the identity but still be unverified"
        );

        assert_let_timeout!(
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) = generic_stream.recv()
        );
        assert_eq!(expected_room_id, room_id);
        assert!(generic_stream.is_empty());
    }

    #[async_test]
    async fn test_event_is_redecrypted_even_if_key_arrives_while_event_processing() {
        let room_id = room_id!("!test:localhost");

        let event_factory = EventFactory::new().room(room_id);
        let (alice, bob, matrix_mock_server, delayed_store) =
            set_up_clients(room_id, true, true).await;

        let delayed_store = delayed_store.unwrap();

        let (event, room_key) =
            prepare_room(&matrix_mock_server, &event_factory, &alice, &bob, room_id).await;

        let event_cache = bob.event_cache();

        // Let's now see what Bob's event cache does.
        let (room_cache, _) = event_cache
            .for_room(room_id)
            .await
            .expect("We should be able to get to the event cache for a specific room");

        let (_, mut subscriber) = room_cache.subscribe().await.unwrap();
        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();

        // Let us forward the event to Bob.
        matrix_mock_server
            .mock_sync()
            .ok_and_run(&bob, |builder| {
                builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(event));
            })
            .await;

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

        info!("Stopping the delay");
        delayed_store.stop_delaying().await;

        // Now that the first decryption attempt has failed since the sync with the
        // event did not contain the room key, and the decryptor has received
        // the room key but the event was not persisted in the cache as of yet,
        // let's the event cache process the event.

        // Alright, Bob has received an update from the cache.
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = subscriber.recv()
        );

        // There should be a single new event, and it should be a UTD as we did not
        // receive the room key yet.
        assert_eq!(diffs.len(), 1);
        assert_matches!(&diffs[0], VectorDiff::Append { values });
        assert_matches!(&values[0].kind, TimelineEventKind::UnableToDecrypt { .. });

        assert_let_timeout!(
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) = generic_stream.recv()
        );
        assert_eq!(expected_room_id, room_id);

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

        assert_let_timeout!(
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) = generic_stream.recv()
        );
        assert_eq!(expected_room_id, room_id);
        assert!(generic_stream.is_empty());
    }
}
