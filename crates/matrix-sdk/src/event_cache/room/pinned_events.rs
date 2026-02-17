// Copyright 2026 The Matrix.org Foundation C.I.C.
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

use std::{collections::BTreeSet, sync::Arc};

use futures_util::{StreamExt as _, stream};
#[cfg(feature = "e2e-encryption")]
use matrix_sdk_base::{deserialized_responses::TimelineEventKind, linked_chunk::Position};
use matrix_sdk_base::{
    event_cache::{Event, store::EventCacheStoreLock},
    linked_chunk::{LinkedChunkId, OwnedLinkedChunkId},
    serde_helpers::extract_relation,
    task_monitor::BackgroundTaskHandle,
};
#[cfg(feature = "e2e-encryption")]
use ruma::EventId;
use ruma::{
    MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId,
    events::{
        AnySyncMessageLikeEvent, AnySyncTimelineEvent, MessageLikeEventType, relation::RelationType,
    },
    room_version_rules::RedactionRules,
    serde::Raw,
};
use tokio::sync::broadcast::{Receiver, Sender};
use tracing::{debug, instrument, trace, warn};

#[cfg(feature = "e2e-encryption")]
use super::super::redecryptor::ResolvedUtd;
use super::super::{
    EventCacheError, EventsOrigin, Result, RoomEventCacheLinkedChunkUpdate, caches::lock,
    persistence::send_updates_to_store, room::events::EventLinkedChunk,
};
use crate::{
    Room, client::WeakClient, config::RequestConfig, event_cache::TimelineVectorDiffs,
    room::WeakRoom,
};

pub(in super::super) struct PinnedEventCacheState {
    /// The ID of the room owning this list of pinned events.
    room_id: OwnedRoomId,

    /// A sender for live events updates in this room's pinned events list.
    sender: Sender<TimelineVectorDiffs>,

    /// The linked chunk representing this room's pinned events.
    ///
    /// This linked chunk also contains related events. The events are sorted in
    /// the chronological order (oldest to newest), since it would be otherwise
    /// impossible to order them correctly, given that we fetch their
    /// relations over time.
    chunk: EventLinkedChunk,

    /// Reference to the underlying backing store.
    // TODO: can be removed?
    store: EventCacheStoreLock,

    /// A sender for the globally observable linked chunk updates that happened
    /// during a sync or a back-pagination.
    ///
    /// See also [`super::super::EventCacheInner::linked_chunk_update_sender`].
    linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,
}

impl lock::Store for PinnedEventCacheState {
    fn store(&self) -> &EventCacheStoreLock {
        &self.store
    }
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for PinnedEventCacheState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedEventCacheState")
            .field("room_id", &self.room_id)
            .field("chunk", &self.chunk)
            .finish_non_exhaustive()
    }
}

/// State for pinned events of a room's event cache.
///
/// This contains all the inner mutable states that ought to be updated at
/// the same time.
pub type PinnedEventCacheStateLock = lock::StateLock<PinnedEventCacheState>;

pub type PinnedEventCacheStateLockWriteGuard<'a> =
    lock::StateLockWriteGuard<'a, PinnedEventCacheState>;

impl<'a> lock::Reload for PinnedEventCacheStateLockWriteGuard<'a> {
    async fn reload(&mut self) -> Result<()> {
        self.reload_from_storage().await?;

        Ok(())
    }
}

impl<'a> PinnedEventCacheStateLockWriteGuard<'a> {
    /// Reload all the pinned events from storage, replacing the current linked
    /// chunk.
    async fn reload_from_storage(&mut self) -> Result<()> {
        let room_id = self.state.room_id.clone();
        let linked_chunk_id = LinkedChunkId::PinnedEvents(&room_id);

        let (last_chunk, chunk_id_gen) = self.store.load_last_chunk(linked_chunk_id).await?;

        let Some(last_chunk) = last_chunk else {
            // No pinned events stored, make sure the in-memory linked chunk is sync'd (i.e.
            // empty), and return.
            if self.state.chunk.events().next().is_some() {
                self.state.chunk.reset();

                let diffs = self.state.chunk.updates_as_vector_diffs();
                if !diffs.is_empty() {
                    let _ = self
                        .state
                        .sender
                        .send(TimelineVectorDiffs { diffs, origin: EventsOrigin::Sync });
                }
            }

            return Ok(());
        };

        let mut previous = last_chunk.previous;
        self.state.chunk.replace_with(Some(last_chunk), chunk_id_gen)?;

        // Reload the entire chunk.
        while let Some(previous_chunk_id) = previous {
            let prev = self.store.load_previous_chunk(linked_chunk_id, previous_chunk_id).await?;
            if let Some(prev_chunk) = prev {
                previous = prev_chunk.previous;
                self.state.chunk.insert_new_chunk_as_first(prev_chunk)?;
            }
        }

        // Empty store updates, since we just reloaded from storage.
        self.state.chunk.store_updates().take();

        // Let observers know about it.
        let diffs = self.state.chunk.updates_as_vector_diffs();
        if !diffs.is_empty() {
            let _ =
                self.state.sender.send(TimelineVectorDiffs { diffs, origin: EventsOrigin::Sync });
        }

        Ok(())
    }

    async fn replace_all_events(&mut self, new_events: Vec<Event>) -> Result<()> {
        trace!("resetting all pinned events in linked chunk");

        let previous_pinned_event_ids = self.state.current_event_ids();

        if new_events.iter().filter_map(|e| e.event_id()).collect::<BTreeSet<_>>()
            == previous_pinned_event_ids.iter().cloned().collect()
        {
            // No change in the list of pinned events.
            return Ok(());
        }

        if self.state.chunk.events().next().is_some() {
            self.state.chunk.reset();
        }

        self.state.chunk.push_live_events(None, &new_events);
        self.propagate_changes().await?;

        let diffs = self.state.chunk.updates_as_vector_diffs();

        if !diffs.is_empty() {
            let _ =
                self.state.sender.send(TimelineVectorDiffs { diffs, origin: EventsOrigin::Sync });
        }

        Ok(())
    }

    /// Propagate the changes in this linked chunk to observers, and save the
    /// changes on disk.
    async fn propagate_changes(&mut self) -> Result<()> {
        let updates = self.state.chunk.store_updates().take();
        let linked_chunk_id = OwnedLinkedChunkId::PinnedEvents(self.state.room_id.clone());
        send_updates_to_store(
            &self.store,
            linked_chunk_id,
            &self.state.linked_chunk_update_sender,
            updates,
        )
        .await
    }
}

impl PinnedEventCacheState {
    /// Return a list of the current event IDs in this linked chunk.
    fn current_event_ids(&self) -> Vec<OwnedEventId> {
        self.chunk.events().filter_map(|(_position, event)| event.event_id()).collect()
    }

    /// Find an event in the linked chunk by its event ID, and return its
    /// location.
    ///
    /// Note: the in-memory content is always the same as the one in the store,
    /// since the store is updated synchronously with changes in the linked
    /// chunk, so we can afford to only look for the event in the memory
    /// linked chunk.
    #[cfg(feature = "e2e-encryption")]
    fn find_event(&self, event_id: &EventId) -> Option<(Position, Event)> {
        for (position, event) in self.chunk.revents() {
            if event.event_id().as_deref() == Some(event_id) {
                return Some((position, event.clone()));
            }
        }

        None
    }
}

/// All the information related to a room's pinned events cache.
pub struct PinnedEventCache {
    state: Arc<PinnedEventCacheStateLock>,

    /// The task handling the refreshing of pinned events for this specific
    /// room.
    _task: Arc<BackgroundTaskHandle>,
}

impl PinnedEventCache {
    /// Creates a new [`PinnedEventCache`] for the given room.
    pub(super) fn new(
        room: Room,
        linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,
        store: EventCacheStoreLock,
    ) -> Self {
        let sender = Sender::new(32);

        let room_id = room.room_id().to_owned();

        let chunk = EventLinkedChunk::new();

        let state =
            PinnedEventCacheState { room_id, chunk, sender, linked_chunk_update_sender, store };
        let state = Arc::new(PinnedEventCacheStateLock::new_inner(state));

        let task = Arc::new(
            room.client()
                .task_monitor()
                .spawn_background_task(
                    "pinned_event_listener_task",
                    Self::pinned_event_listener_task(room, state.clone()),
                )
                .abort_on_drop(),
        );

        Self { state, _task: task }
    }

    /// Subscribe to live events from this room's pinned events cache.
    pub async fn subscribe(&self) -> Result<(Vec<Event>, Receiver<TimelineVectorDiffs>)> {
        let guard = self.state.read().await?;
        let events = guard.state.chunk.events().map(|(_position, item)| item.clone()).collect();

        let recv = guard.state.sender.subscribe();

        Ok((events, recv))
    }

    /// Try to locate the events in the linked chunk corresponding to the given
    /// list of decrypted events, and replace them, while alerting observers
    /// about the update.
    #[cfg(feature = "e2e-encryption")]
    pub(in crate::event_cache) async fn replace_utds(&self, events: &[ResolvedUtd]) -> Result<()> {
        let mut guard = self.state.write().await?;

        let pinned_events_set =
            guard.state.current_event_ids().into_iter().collect::<BTreeSet<_>>();

        let mut replaced_some = false;

        for (event_id, decrypted, actions) in events {
            // As a performance optimization, do a lookup in the current pinned events
            // check, before looking for the event in the linked chunk.

            if !pinned_events_set.contains(event_id) {
                continue;
            }

            // The event should be in the linked chunk.
            let Some((position, mut target_event)) = guard.state.find_event(event_id) else {
                continue;
            };

            target_event.kind = TimelineEventKind::Decrypted(decrypted.clone());

            if let Some(actions) = actions {
                target_event.set_push_actions(actions.clone());
            }

            guard
                .state
                .chunk
                .replace_event_at(position, target_event.clone())
                .expect("position should be valid");

            replaced_some = true;
        }

        if replaced_some {
            guard.propagate_changes().await?;

            let diffs = guard.state.chunk.updates_as_vector_diffs();
            let _ =
                guard.state.sender.send(TimelineVectorDiffs { diffs, origin: EventsOrigin::Cache });
        }

        Ok(())
    }

    /// Given a raw event, try to extract the target event ID of a relation as
    /// defined with `m.relates_to`.
    fn extract_relation_target(raw: &Raw<AnySyncTimelineEvent>) -> Option<OwnedEventId> {
        let (rel_type, event_id) = extract_relation(raw)?;

        // Don't include thread responses in the pinned event chunk.
        match rel_type {
            RelationType::Thread => None,
            _ => Some(event_id),
        }
    }

    /// Given a raw event, try to extract the target event ID of a live
    /// redaction.
    fn extract_redaction_target(
        raw: &Raw<AnySyncTimelineEvent>,
        room_redaction_rules: &RedactionRules,
    ) -> Option<OwnedEventId> {
        // Try to find a redaction, but do not deserialize the entire event if we aren't
        // certain it's a `m.room.redaction`.
        if raw.get_field::<MessageLikeEventType>("type").ok()??
            != MessageLikeEventType::RoomRedaction
        {
            return None;
        }

        let AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomRedaction(redaction)) =
            raw.deserialize().ok()?
        else {
            return None;
        };

        redaction.redacts(room_redaction_rules).map(ToOwned::to_owned).or_else(|| {
            warn!("missing target event id from the redaction event");
            None
        })
    }

    /// Check if any of the given events relate to an event in the pinned events
    /// linked chunk, and append it, in this case.
    pub(super) async fn maybe_add_live_related_events(
        &mut self,
        events: &[Event],
        room_redaction_rules: &RedactionRules,
    ) -> Result<()> {
        trace!("checking live events for relations to pinned events");
        let mut guard = self.state.write().await?;

        let pinned_event_ids: BTreeSet<OwnedEventId> =
            guard.state.current_event_ids().into_iter().collect();

        if pinned_event_ids.is_empty() {
            return Ok(());
        }

        let mut new_relations = Vec::new();

        // For all events that relate to an event in the pinned events chunk, push this
        // event to the linked chunk, and propagate changes to observers.
        for ev in events {
            // Try to find a regular relation in ev.
            if let Some(relation_target) = Self::extract_relation_target(ev.raw())
                && pinned_event_ids.contains(&relation_target)
            {
                new_relations.push(ev.clone());
                continue;
            }

            // Try to find a redaction in ev.
            if let Some(redaction_target) =
                Self::extract_redaction_target(ev.raw(), room_redaction_rules)
                && pinned_event_ids.contains(&redaction_target)
            {
                new_relations.push(ev.clone());
                continue;
            }
        }

        if !new_relations.is_empty() {
            trace!("found {} new related events to pinned events", new_relations.len());

            // We've found new relations; append them to the linked chunk.
            guard.state.chunk.push_live_events(None, &new_relations);

            guard.propagate_changes().await?;

            let diffs = guard.state.chunk.updates_as_vector_diffs();
            if !diffs.is_empty() {
                let _ = guard
                    .state
                    .sender
                    .send(TimelineVectorDiffs { diffs, origin: EventsOrigin::Sync });
            }
        }

        Ok(())
    }

    #[instrument(fields(%room_id = room.room_id()), skip(room, state))]
    async fn pinned_event_listener_task(room: Room, state: Arc<PinnedEventCacheStateLock>) {
        debug!("pinned events listener task started");

        let reload_from_network = async |room: Room| {
            let events = match Self::reload_pinned_events(room).await {
                Ok(Some(events)) => events,
                Ok(None) => Vec::new(),
                Err(err) => {
                    warn!("error when loading pinned events: {err}");
                    return;
                }
            };

            // Replace the whole linked chunk with those new events, and propagate updates
            // to the observers.
            match state.write().await {
                Ok(mut guard) => {
                    guard.replace_all_events(events).await.unwrap_or_else(|err| {
                        warn!("error when replacing pinned events: {err}");
                    });
                }

                Err(err) => {
                    warn!("error when acquiring write lock to replace pinned events: {err}");
                }
            }
        };

        // Reload the pinned events from the storage first.
        match state.write().await {
            Ok(mut guard) => {
                // On startup, reload the pinned events from storage.
                guard.reload_from_storage().await.unwrap_or_else(|err| {
                    warn!("error when reloading pinned events from storage, at start: {err}");
                });

                // Compare the initial list of pinned events to the one in the linked chunk.
                let actual_pinned_events = room.pinned_event_ids().unwrap_or_default();
                let reloaded_set =
                    guard.state.current_event_ids().into_iter().collect::<BTreeSet<_>>();

                if actual_pinned_events.len() != reloaded_set.len()
                    || actual_pinned_events.iter().any(|event_id| !reloaded_set.contains(event_id))
                {
                    // Reload the list of pinned events from network.
                    drop(guard);
                    reload_from_network(room.clone()).await;
                }
            }

            Err(err) => {
                warn!("error when acquiring write lock to initialize pinned events: {err}");
            }
        }

        let weak_room =
            WeakRoom::new(WeakClient::from_client(&room.client()), room.room_id().to_owned());

        let mut stream = room.pinned_event_ids_stream();

        drop(room);

        // Whenever the list of pinned events changes, reload it.
        while let Some(new_list) = stream.next().await {
            trace!("handling update");

            let guard = match state.read().await {
                Ok(guard) => guard,
                Err(err) => {
                    warn!("error when acquiring read lock to handle pinned events update: {err}");
                    break;
                }
            };

            // Compare to the current linked chunk.
            let current_set = guard.state.current_event_ids().into_iter().collect::<BTreeSet<_>>();

            if !new_list.is_empty()
                && new_list.iter().all(|event_id| current_set.contains(event_id))
            {
                // All the events in the pinned list are the same, don't reload.
                continue;
            }

            let Some(room) = weak_room.get() else {
                debug!("room has been dropped, ending pinned events listener task");
                break;
            };

            drop(guard);

            // Event IDs differ, so reload all the pinned events.
            reload_from_network(room).await;
        }

        debug!("pinned events listener task ended");
    }

    /// Loads the pinned events in this room, using the cache first and then
    /// requesting the event from the homeserver if it couldn't be found.
    /// This method will perform as many concurrent requests for events as
    /// `max_concurrent_requests` allows, to avoid overwhelming the server.
    ///
    /// Returns `None` if the list of pinned events hasn't changed since the
    /// previous time we loaded them. May return an error if there was an
    /// issue fetching the full events.
    async fn reload_pinned_events(room: Room) -> Result<Option<Vec<Event>>> {
        let (max_events_to_load, max_concurrent_requests) = {
            let client = room.client();
            let config = client.event_cache().config().await;
            (config.max_pinned_events_to_load, config.max_pinned_events_concurrent_requests)
        };

        let pinned_event_ids: Vec<OwnedEventId> = room
            .pinned_event_ids()
            .unwrap_or_default()
            .into_iter()
            .rev()
            .take(max_events_to_load)
            .rev()
            .collect();

        if pinned_event_ids.is_empty() {
            return Ok(Some(Vec::new()));
        }

        let mut loaded_events: Vec<Event> =
            stream::iter(pinned_event_ids.clone().into_iter().map(|event_id| {
                let room = room.clone();
                let filter = vec![RelationType::Annotation, RelationType::Replacement];
                let request_config = RequestConfig::default().retry_limit(3);

                async move {
                    let (target, mut relations) = room
                        .load_or_fetch_event_with_relations(
                            &event_id,
                            Some(filter),
                            Some(request_config),
                        )
                        .await?;

                    relations.insert(0, target);
                    Ok::<_, crate::Error>(relations)
                }
            }))
            .buffer_unordered(max_concurrent_requests)
            // Flatten all the vectors.
            .flat_map(stream::iter)
            .flat_map(stream::iter)
            .collect()
            .await;

        if loaded_events.len() != pinned_event_ids.len() {
            warn!(
                "only successfully loaded {} out of {} pinned events",
                loaded_events.len(),
                pinned_event_ids.len()
            );
        }

        if loaded_events.is_empty() {
            // If the list of loaded events is empty, we ran into an error to load *all* the
            // pinned events, which needs to be reported to the caller.
            return Err(EventCacheError::UnableToLoadPinnedEvents);
        }

        // Since we have all the events and their related events, we can't nicely sort
        // them, since we've lost all ordering information from using /event or
        // /relations. Resort to sorting using chronological ordering (oldest ->
        // newest).
        loaded_events.sort_by_key(|item| {
            item.raw()
                .deserialize()
                .map(|e| e.origin_server_ts())
                .unwrap_or_else(|_| MilliSecondsSinceUnixEpoch::now())
        });

        Ok(Some(loaded_events))
    }
}
