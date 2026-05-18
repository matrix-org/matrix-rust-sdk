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

mod updates;

use std::{collections::BTreeSet, fmt, sync::Arc};

use eyeball_im::VectorDiff;
use futures_util::{StreamExt as _, stream};
use matrix_sdk_base::{
    event_cache::{Event, store::EventCacheStoreLock},
    linked_chunk::{LinkedChunkId, OwnedLinkedChunkId},
    sync::{JoinedRoomUpdate, LeftRoomUpdate, Timeline},
    task_monitor::BackgroundTaskHandle,
};
use ruma::{MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, events::relation::RelationType};
use tokio::sync::broadcast::{Receiver, Sender};
use tracing::{debug, instrument, trace, warn};

pub(super) use self::updates::PinnedEventsCacheUpdateSender;
#[cfg(feature = "e2e-encryption")]
use super::super::redecryptor::ResolvedUtd;
use super::{
    super::{EventCacheError, EventsOrigin, Result, persistence::send_updates_to_store},
    TimelineVectorDiffs,
    event_linked_chunk::EventLinkedChunk,
    lock,
    lock::Reload as _,
    room::RoomEventCacheLinkedChunkUpdate,
};
use crate::{Room, client::WeakClient, config::RequestConfig, room::WeakRoom};

pub(in super::super) struct PinnedEventsCacheState {
    /// The ID of the room owning this list of pinned events.
    room_id: OwnedRoomId,

    /// The linked chunk representing this room's pinned events.
    ///
    /// This linked chunk also contains related events. The events are sorted in
    /// the chronological order (oldest to newest), since it would be otherwise
    /// impossible to order them correctly, given that we fetch their
    /// relations over time.
    chunk: EventLinkedChunk,

    /// Reference to the underlying backing store.
    store: EventCacheStoreLock,

    /// A clone of [`PinnedEventsCacheInner::update_sender`].
    ///
    /// This is used only by the [`LockedPinnedEventsCacheState::read`] and
    /// [`LockedPinnedEventsCacheState::write`] when the state must be reset.
    update_sender: PinnedEventsCacheUpdateSender,

    /// A sender for the globally observable linked chunk updates that happened
    /// during a sync or a back-pagination.
    ///
    /// See also [`super::super::EventCacheInner::linked_chunk_update_sender`].
    linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,
}

impl lock::Store for PinnedEventsCacheState {
    fn store(&self) -> &EventCacheStoreLock {
        &self.store
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for PinnedEventsCacheState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinnedEventsCacheState")
            .field("room_id", &self.room_id)
            .field("chunk", &self.chunk)
            .finish_non_exhaustive()
    }
}

/// State for pinned events of a room's event cache.
///
/// This contains all the inner mutable states that ought to be updated at
/// the same time.
pub type LockedPinnedEventsCacheState = lock::StateLock<PinnedEventsCacheState>;

pub type PinnedEventsCacheStateLockWriteGuard<'a> =
    lock::StateLockWriteGuard<'a, PinnedEventsCacheState>;

impl<'a> lock::Reload for PinnedEventsCacheStateLockWriteGuard<'a> {
    async fn reload(&mut self) -> Result<()> {
        self.reload_from_storage().await?;

        Ok(())
    }
}

impl<'a> PinnedEventsCacheStateLockWriteGuard<'a> {
    /// Reset this data structure as if it were brand new.
    ///
    /// However, pinned-events are not only stored here. They are also coming at
    /// a state-event containing all the pinned-events ID list in the `Room`.
    /// This is a best-effort here: we are resetting the events stored in this
    /// cache only.
    ///
    /// Return a single diff update that is a clear of all events; as a
    /// result, the caller may override any pending diff updates
    /// with the result of this function.
    pub async fn reset(&mut self) -> Result<Vec<VectorDiff<Event>>> {
        self.state.chunk.reset();
        self.propagate_changes().await?;

        let diff_updates = self.state.chunk.updates_as_vector_diffs();

        // Ensure the contract defined in the doc comment is true:
        debug_assert_eq!(diff_updates.len(), 1);
        debug_assert!(matches!(diff_updates[0], VectorDiff::Clear));

        Ok(diff_updates)
    }

    async fn handle_sync(&mut self, timeline: Timeline) -> Result<()> {
        // We've found new relations; append them to the linked chunk.
        self.state.chunk.push_live_events(None, &timeline.events);

        self.propagate_changes().await?;
        self.notify_subscribers(EventsOrigin::Sync);

        Ok(())
    }

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
                self.notify_subscribers(EventsOrigin::Sync);
            }

            return Ok(());
        };

        {
            let mut current_chunk_identifier = last_chunk.identifier;
            self.state.chunk.replace_with(Some(last_chunk), chunk_id_gen)?;

            // Reload the entire chunk.
            while let Some(previous_chunk) =
                self.store.load_previous_chunk(linked_chunk_id, current_chunk_identifier).await?
            {
                current_chunk_identifier = previous_chunk.identifier;
                self.state.chunk.insert_new_chunk_as_first(previous_chunk)?;
            }
        }

        // Empty store updates, since we just reloaded from storage.
        self.state.chunk.store_updates().take();

        // Let observers know about it.
        self.notify_subscribers(EventsOrigin::Cache);

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
        self.notify_subscribers(EventsOrigin::Sync);

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

    /// Notify subscribers of timeline updates.
    fn notify_subscribers(&mut self, origin: EventsOrigin) {
        let diffs = self.state.chunk.updates_as_vector_diffs();

        if !diffs.is_empty() {
            let _ = self.update_sender.send(TimelineVectorDiffs { diffs, origin });
        }
    }
}

impl PinnedEventsCacheState {
    /// Return a list of the current event IDs in this linked chunk.
    pub(super) fn current_event_ids(&self) -> Vec<OwnedEventId> {
        self.chunk.events().filter_map(|(_position, event)| event.event_id()).collect()
    }
}

/// All the information related to room's pinned events..
///
/// Cloning is shallow, and thus is cheap to do.
#[derive(Clone)]
pub struct PinnedEventsCache {
    inner: Arc<PinnedEventsCacheInner>,
}

/// The (non-cloneable) details of the `PinnedEventsCache`.
struct PinnedEventsCacheInner {
    /// The ID of the room owning this list of pinned events.
    room_id: OwnedRoomId,

    /// State of this `PinnedEventsCache`.
    ///
    /// It is behind an `Arc` because it is shared with the task.
    state: Arc<LockedPinnedEventsCacheState>,

    /// Update sender for this pinned events cache.
    update_sender: PinnedEventsCacheUpdateSender,

    /// The task handling the refreshing of pinned events for this specific
    /// room.
    _task: BackgroundTaskHandle,
}

impl PinnedEventsCache {
    /// Creates a new [`PinnedEventsCache`] for the given room.
    pub(in super::super) fn new(
        weak_room: &WeakRoom,
        linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,
        store: EventCacheStoreLock,
    ) -> Result<Self> {
        let room = weak_room.get().ok_or(EventCacheError::ClientDropped)?;
        let room_id = room.room_id().to_owned();

        let update_sender = PinnedEventsCacheUpdateSender::new();

        let chunk = EventLinkedChunk::new();

        let state = PinnedEventsCacheState {
            room_id: room_id.clone(),
            chunk,
            update_sender: update_sender.clone(),
            linked_chunk_update_sender,
            store,
        };
        let state = Arc::new(LockedPinnedEventsCacheState::new_inner(state));

        let task = room
            .client()
            .task_monitor()
            .spawn_infinite_task(
                "pinned_event_listener_task",
                Self::pinned_event_listener_task(room, state.clone()),
            )
            .abort_on_drop();

        Ok(Self {
            inner: Arc::new(PinnedEventsCacheInner { room_id, state, update_sender, _task: task }),
        })
    }

    /// Return a reference to the state.
    pub(super) fn state(&self) -> &LockedPinnedEventsCacheState {
        &self.inner.state
    }

    /// Get a reference to the [`RoomEventCacheUpdateSender`].
    pub(in super::super) fn update_sender(&self) -> &PinnedEventsCacheUpdateSender {
        &self.inner.update_sender
    }

    /// Subscribe to live events from this room's pinned events cache.
    pub async fn subscribe(&self) -> Result<(Vec<Event>, Receiver<TimelineVectorDiffs>)> {
        let guard = self.inner.state.read().await?;
        let events = guard.state.chunk.events().map(|(_position, item)| item.clone()).collect();

        let recv = guard.state.update_sender.new_pinned_events_receiver();

        Ok((events, recv))
    }

    /// Try to locate the events in the linked chunk corresponding to the given
    /// list of decrypted events, and replace them, while alerting observers
    /// about the update.
    #[cfg(feature = "e2e-encryption")]
    pub(in crate::event_cache) async fn replace_utds(&self, events: &[ResolvedUtd]) -> Result<()> {
        let mut guard = self.inner.state.write().await?;

        if guard.state.chunk.replace_utds(events) {
            guard.propagate_changes().await?;
            guard.notify_subscribers(EventsOrigin::Cache);
        }

        Ok(())
    }

    /// Handle a [`JoinedRoomUpdate`].
    #[instrument(skip_all, fields(room_id = %self.inner.room_id))]
    pub(super) async fn handle_joined_room_update(&self, updates: JoinedRoomUpdate) -> Result<()> {
        self.handle_timeline(updates.timeline).await?;

        Ok(())
    }

    /// Handle a [`LeftRoomUpdate`].
    #[instrument(skip_all, fields(room_id = %self.inner.room_id))]
    pub(super) async fn handle_left_room_update(&self, updates: LeftRoomUpdate) -> Result<()> {
        self.handle_timeline(updates.timeline).await?;

        Ok(())
    }

    /// Handle a [`Timeline`], i.e. new events received by a sync for this
    /// thread.
    async fn handle_timeline(&self, timeline: Timeline) -> Result<()> {
        if timeline.events.is_empty() {
            return Ok(());
        }

        trace!("adding new {} events", timeline.events.len());

        self.inner.state.write().await?.handle_sync(timeline).await?;

        Ok(())
    }

    #[instrument(fields(%room_id = room.room_id()), skip(room, state))]
    async fn pinned_event_listener_task(room: Room, state: Arc<LockedPinnedEventsCacheState>) {
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
            let config = client.event_cache().config();
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

        let mut num_successful_loads = 0;

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
            // Count successful queries.
            .inspect(|result| {
                if result.is_ok() {
                    num_successful_loads += 1;
                }
            })
            // Get rid of error results.
            .flat_map(stream::iter)
            // Flatten the list of `Vec<Event>` into a list of `Event`.
            .flat_map(stream::iter)
            .collect()
            .await;

        if num_successful_loads != pinned_event_ids.len() {
            warn!(
                "only successfully loaded {} out of {} pinned events",
                num_successful_loads,
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

    /// Force to reload the pinned events.
    //
    // TODO(@hywan): Temporary fix. All the states must be in a single struct behind
    // the cross-process lock instead of being dispatched in each cache.
    pub(super) async fn reload(&self) -> Result<()> {
        self.inner.state.write().await?.reload().await
    }
}

impl fmt::Debug for PinnedEventsCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinnedEventsCache").finish_non_exhaustive()
    }
}
