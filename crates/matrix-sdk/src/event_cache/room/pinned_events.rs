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
use matrix_sdk_base::{
    deserialized_responses::TimelineEventKind,
    event_cache::{
        Event, Gap,
        store::{EventCacheStoreLock, EventCacheStoreLockGuard, EventCacheStoreLockState},
    },
    linked_chunk::{LinkedChunkId, OwnedLinkedChunkId, Update},
};
use ruma::{
    MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId,
    events::{
        AnySyncMessageLikeEvent, AnySyncTimelineEvent, MessageLikeEventType, relation::RelationType,
    },
    room_version_rules::RedactionRules,
    serde::Raw,
};
use serde::Deserialize;
use tokio::sync::{
    Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard,
    broadcast::{Receiver, Sender},
};
use tracing::{debug, instrument, trace, warn};

use crate::{
    Room,
    client::WeakClient,
    config::RequestConfig,
    event_cache::{
        EventCacheError, EventsOrigin, Result, RoomEventCacheLinkedChunkUpdate,
        RoomEventCacheUpdate, room::events::EventLinkedChunk,
    },
    executor::{JoinHandle, spawn},
    room::WeakRoom,
};

struct PinnedEventCacheState {
    /// The ID of the room owning this list of pinned events.
    room_id: OwnedRoomId,

    /// A sender for live events updates in this room's pinned events list.
    sender: Sender<RoomEventCacheUpdate>,

    /// The linked chunk representing this room's pinned events.
    ///
    /// Does not contain related events by default.
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

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for PinnedEventCacheState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedEventCacheState")
            .field("room_id", &self.room_id)
            .field("chunk", &self.chunk)
            .finish_non_exhaustive()
    }
}

struct PinnedEventCacheStateLock {
    /// The per-thread lock around the real state.
    locked_state: RwLock<PinnedEventCacheState>,

    /// A lock to guard against races when upgrading a read lock into a write
    /// lock, after noticing the cross-process lock has been dirtied.
    state_lock_upgrade_mutex: Mutex<()>,
}

impl PinnedEventCacheStateLock {
    /// Lock this [`PinnedEventCacheStateLock`] with per-thread shared access.
    ///
    /// This method locks the per-thread lock over the state, and then locks
    /// the cross-process lock over the store. It returns an RAII guard
    /// which will drop the read access to the state and to the store when
    /// dropped.
    ///
    /// If the cross-process lock over the store is dirty (see
    /// [`EventCacheStoreLockState`]), the state is reset to the last chunk.
    async fn read(&self) -> Result<PinnedEventCacheStateLockReadGuard<'_>> {
        // Se comment in [`RoomEventCacheStateLock::read`] for explanation.
        let _state_lock_upgrade_guard = self.state_lock_upgrade_mutex.lock().await;

        // Obtain a read lock.
        let state_guard = self.locked_state.read().await;

        match state_guard.store.lock().await? {
            EventCacheStoreLockState::Clean(store_guard) => {
                Ok(PinnedEventCacheStateLockReadGuard { state: state_guard, _store: store_guard })
            }

            EventCacheStoreLockState::Dirty(store_guard) => {
                // Drop the read lock, and take a write lock to modify the state.
                // This is safe because only one reader at a time (see
                // `Self::state_lock_upgrade_mutex`) is allowed.
                drop(state_guard);
                let state_guard = self.locked_state.write().await;

                let guard =
                    PinnedEventCacheStateLockWriteGuard { state: state_guard, store: store_guard };

                // Force to reload by shrinking to the last chunk.
                // TODO: reload the full pinned events list from the store.
                //let updates_as_vector_diffs = guard.force_shrink_to_last_chunk().await?;
                let updates_as_vector_diffs = Vec::new();

                // All good now, mark the cross-process lock as non-dirty.
                EventCacheStoreLockGuard::clear_dirty(&guard.store);

                // Downgrade the guard as soon as possible.
                let guard = guard.downgrade();

                // Now let the world know about the reload.
                if !updates_as_vector_diffs.is_empty() {
                    // Notify observers about the update.
                    let _ = guard.state.sender.send(RoomEventCacheUpdate::UpdateTimelineEvents {
                        diffs: updates_as_vector_diffs,
                        origin: EventsOrigin::Cache,
                    });
                }

                Ok(guard)
            }
        }
    }

    /// Lock this [`PinnedEventCacheStateLock`] with exclusive per-thread
    /// write access.
    ///
    /// This method locks the per-thread lock over the state, and then locks
    /// the cross-process lock over the store. It returns an RAII guard
    /// which will drop the write access to the state and to the store when
    /// dropped.
    ///
    /// If the cross-process lock over the store is dirty (see
    /// [`EventCacheStoreLockState`]), the state is reset to the last chunk.
    async fn write(&self) -> Result<PinnedEventCacheStateLockWriteGuard<'_>> {
        let state_guard = self.locked_state.write().await;

        match state_guard.store.lock().await? {
            EventCacheStoreLockState::Clean(store_guard) => {
                Ok(PinnedEventCacheStateLockWriteGuard { state: state_guard, store: store_guard })
            }

            EventCacheStoreLockState::Dirty(store_guard) => {
                let guard =
                    PinnedEventCacheStateLockWriteGuard { state: state_guard, store: store_guard };

                // TODO: reload the full pinned events list from the store.
                //let updates_as_vector_diffs = guard.force_shrink_to_last_chunk().await?;
                let updates_as_vector_diffs = Vec::new();

                // All good now, mark the cross-process lock as non-dirty.
                EventCacheStoreLockGuard::clear_dirty(&guard.store);

                // Now let the world know about the reload.
                if !updates_as_vector_diffs.is_empty() {
                    // Notify observers about the update.
                    let _ = guard.state.sender.send(RoomEventCacheUpdate::UpdateTimelineEvents {
                        diffs: updates_as_vector_diffs,
                        origin: EventsOrigin::Cache,
                    });
                }

                Ok(guard)
            }
        }
    }
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for PinnedEventCacheStateLock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedEventCacheStateLock")
            .field("locked_state", &self.locked_state)
            .finish_non_exhaustive()
    }
}

/// The read lock guard returned by [`PinnedEventCacheStateLock::read`].
pub struct PinnedEventCacheStateLockReadGuard<'a> {
    /// The per-thread read lock guard over the
    /// [`PinnedEventCacheState`].
    state: RwLockReadGuard<'a, PinnedEventCacheState>,

    /// The cross-process lock guard over the store.
    _store: EventCacheStoreLockGuard,
}

/// The write lock guard return by [`PinnedEventCacheStateLock::write`].
struct PinnedEventCacheStateLockWriteGuard<'a> {
    /// The per-thread write lock guard over the
    /// [`PinnedEventCacheState`].
    state: RwLockWriteGuard<'a, PinnedEventCacheState>,

    /// The cross-process lock guard over the store.
    store: EventCacheStoreLockGuard,
}

impl<'a> PinnedEventCacheStateLockWriteGuard<'a> {
    /// Synchronously downgrades a write lock into a read lock.
    ///
    /// The per-thread/state lock is downgraded atomically, without allowing
    /// any writers to take exclusive access of the lock in the meantime.
    ///
    /// It returns an RAII guard which will drop the write access to the
    /// state and to the store when dropped.
    fn downgrade(self) -> PinnedEventCacheStateLockReadGuard<'a> {
        PinnedEventCacheStateLockReadGuard { state: self.state.downgrade(), _store: self.store }
    }
}

impl<'a> PinnedEventCacheStateLockWriteGuard<'a> {
    async fn replace_all_events(&mut self, new_events: Vec<Event>) -> Result<()> {
        trace!("resetting all pinned events in linked chunk");

        let previous_pinned_event_ids = self.state.current_event_ids();

        if new_events.iter().filter_map(|e| e.event_id()).collect::<BTreeSet<_>>()
            == previous_pinned_event_ids.iter().cloned().collect()
        {
            // No change in the list of pinned events.
            return Ok(());
        }

        self.state.chunk.reset();
        self.state.chunk.push_live_events(None, &new_events);

        self.propagate_changes().await?;

        let diffs = self.state.chunk.updates_as_vector_diffs();
        if !diffs.is_empty() {
            let _ = self.state.sender.send(RoomEventCacheUpdate::UpdateTimelineEvents {
                diffs,
                origin: EventsOrigin::Sync,
            });
        }

        Ok(())
    }

    /// Propagate the changes in this linked chunk to observers, and save the
    /// changes on disk.
    async fn propagate_changes(&mut self) -> Result<()> {
        let updates = self.state.chunk.store_updates().take();
        self.send_updates_to_store(updates).await
    }

    // NOTE: copy/paste
    async fn send_updates_to_store(&mut self, mut updates: Vec<Update<Event, Gap>>) -> Result<()> {
        if updates.is_empty() {
            return Ok(());
        }

        // Strip relations from updates which insert or replace items.
        for update in updates.iter_mut() {
            match update {
                Update::PushItems { items, .. } => Self::strip_relations_from_events(items),
                Update::ReplaceItem { item, .. } => Self::strip_relations_from_event(item),
                // Other update kinds don't involve adding new events.
                Update::NewItemsChunk { .. }
                | Update::NewGapChunk { .. }
                | Update::RemoveChunk(_)
                | Update::RemoveItem { .. }
                | Update::DetachLastItems { .. }
                | Update::StartReattachItems
                | Update::EndReattachItems
                | Update::Clear => {}
            }
        }

        // Spawn a task to make sure that all the changes are effectively forwarded to
        // the store, even if the call to this method gets aborted.
        //
        // The store cross-process locking involves an actual mutex, which ensures that
        // storing updates happens in the expected order.

        let store = self.store.clone();
        let room_id = self.state.room_id.clone();
        let cloned_updates = updates.clone();

        spawn(async move {
            trace!(updates = ?cloned_updates, "sending linked chunk updates to the store");
            let linked_chunk_id = LinkedChunkId::PinnedEvents(&room_id);

            store.handle_linked_chunk_updates(linked_chunk_id, cloned_updates).await?;
            trace!("linked chunk updates applied");

            Result::Ok(())
        })
        .await
        .expect("joining failed")?;

        // Forward that the store got updated to observers.
        let _ = self.state.linked_chunk_update_sender.send(RoomEventCacheLinkedChunkUpdate {
            linked_chunk_id: OwnedLinkedChunkId::PinnedEvents(self.state.room_id.clone()),
            updates,
        });

        Ok(())
    }

    // NOTE: copy/paste
    fn strip_relations_from_event(ev: &mut Event) {
        match &mut ev.kind {
            TimelineEventKind::Decrypted(decrypted) => {
                // Remove all information about encryption info for
                // the bundled events.
                decrypted.unsigned_encryption_info = None;

                // Remove the `unsigned`/`m.relations` field, if needs be.
                Self::strip_relations_if_present(&mut decrypted.event);
            }

            TimelineEventKind::UnableToDecrypt { event, .. }
            | TimelineEventKind::PlainText { event } => {
                Self::strip_relations_if_present(event);
            }
        }
    }

    /// Strips the bundled relations from a collection of events.
    // NOTE: copy/paste
    fn strip_relations_from_events(items: &mut [Event]) {
        for ev in items.iter_mut() {
            Self::strip_relations_from_event(ev);
        }
    }

    /// Removes the bundled relations from an event, if they were present.
    ///
    /// Only replaces the present if it contained bundled relations.
    // NOTE: copy/paste
    fn strip_relations_if_present<T>(event: &mut Raw<T>) {
        // We're going to get rid of the `unsigned`/`m.relations` field, if it's
        // present.
        // Use a closure that returns an option so we can quickly short-circuit.
        let mut closure = || -> Option<()> {
            let mut val: serde_json::Value = event.deserialize_as().ok()?;
            let unsigned = val.get_mut("unsigned")?;
            let unsigned_obj = unsigned.as_object_mut()?;
            if unsigned_obj.remove("m.relations").is_some() {
                *event = Raw::new(&val).ok()?.cast_unchecked();
            }
            None
        };
        let _ = closure();
    }
}

impl PinnedEventCacheState {
    /// Return a list of the current event IDs in this linked chunk.
    fn current_event_ids(&self) -> Vec<OwnedEventId> {
        self.chunk.events().filter_map(|(_position, event)| event.event_id()).collect()
    }
}

/// All the information related to a room's pinned events cache.
pub struct PinnedEventCache {
    state: Arc<PinnedEventCacheStateLock>,

    /// The task handling the refreshing of pinned events for this specific
    /// room.
    // TODO(bnjbvr): use the background job handle for this, when
    // available in `main`.
    _task: Arc<JoinHandle<()>>,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for PinnedEventCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedEventCache").field("state", &self.state).finish_non_exhaustive()
    }
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
        let state = Arc::new(PinnedEventCacheStateLock {
            locked_state: RwLock::new(state),
            state_lock_upgrade_mutex: Mutex::new(()),
        });

        let task = Arc::new(spawn(Self::pinned_event_listener_task(room, state.clone())));

        Self { state, _task: task }
    }

    /// Subscribe to live events from this room's pinned events cache.
    pub async fn subscribe(&self) -> Result<(Vec<Event>, Receiver<RoomEventCacheUpdate>)> {
        let guard = self.state.read().await?;
        let events = guard.state.chunk.events().map(|(_position, item)| item.clone()).collect();

        let recv = guard.state.sender.subscribe();

        Ok((events, recv))
    }

    /// Given a raw event, try to extract the target event ID of a relation as
    /// defined with `m.relates_to`.
    fn extract_relation_target(raw: &Raw<AnySyncTimelineEvent>) -> Option<OwnedEventId> {
        #[derive(Deserialize)]
        struct SimplifiedRelation {
            event_id: OwnedEventId,
        }

        #[derive(Deserialize)]
        struct SimplifiedContent {
            #[serde(rename = "m.relates_to")]
            relates_to: Option<SimplifiedRelation>,
        }

        let SimplifiedContent { relates_to: Some(relation) } =
            raw.get_field::<SimplifiedContent>("content").ok()??
        else {
            return None;
        };

        Some(relation.event_id)
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
                let _ = guard.state.sender.send(RoomEventCacheUpdate::UpdateTimelineEvents {
                    diffs,
                    origin: EventsOrigin::Sync,
                });
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
                    warn!("error when loading initial pinned events: {err}");
                    return;
                }
            };

            // Replace the whole linked chunk with those new events, and propagate updates
            // to the observers.
            match state.write().await {
                Ok(mut guard) => {
                    guard.replace_all_events(events).await.unwrap_or_else(|err| {
                        warn!("error when replacing initial pinned events: {err}");
                    });
                }

                Err(err) => {
                    warn!(
                        "error when acquiring write lock to replace initial pinned events: {err}"
                    );
                }
            }
        };

        // TODO: reload from persisted cache!

        // Initial state: load the list of pinned events from network.
        reload_from_network(room.clone()).await;

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
