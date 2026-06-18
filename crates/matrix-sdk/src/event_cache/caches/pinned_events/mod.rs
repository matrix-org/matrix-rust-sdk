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

use std::{cmp::Ordering, collections::BTreeSet, fmt, sync::Arc};

use eyeball_im::VectorDiff;
use futures_util::{StreamExt as _, stream};
use matrix_sdk_base::{
    apply_redaction,
    event_cache::{Event, Gap},
    linked_chunk::{LinkedChunkId, OwnedLinkedChunkId, Position, Update},
    serde_helpers::extract_redaction_target,
    sync::{JoinedRoomUpdate, LeftRoomUpdate, Timeline},
    task_monitor::BackgroundTaskHandle,
};
use matrix_sdk_common::executor::spawn;
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedUserId,
    events::{relation::RelationType, room::redaction::SyncRoomRedactionEvent},
    room_version_rules::RoomVersionRules,
};
use tokio::sync::broadcast::{Receiver, Sender};
use tracing::{debug, instrument, trace, warn};

pub(super) use self::updates::PinnedEventsCacheUpdateSender;
#[cfg(feature = "e2e-encryption")]
use super::super::redecryptor::ResolvedUtd;
use super::{
    super::{
        EventCacheError, EventsOrigin, Result,
        deduplicator::{DeduplicationOutcome, filter_duplicate_events},
        persistence::{find_event, send_updates_to_store},
        states::{
            CacheStateLock, ReloadPreprocessing, StateLock, StateLockWriteGuard,
            selectors::PinnedEventsStateSelector,
        },
    },
    EventLocation, TimelineVectorDiffs,
    event_linked_chunk::{EventLinkedChunk, sort_positions_descending},
    room::RoomEventCacheLinkedChunkUpdate,
};
use crate::{Room, client::WeakClient, config::RequestConfig, room::WeakRoom};

pub struct PinnedEventsCacheState {
    /// The ID of the room owning this list of pinned events.
    room_id: OwnedRoomId,

    /// The user's own user id.
    own_user_id: OwnedUserId,

    /// The rules for the version of this room.
    room_version_rules: RoomVersionRules,

    /// The linked chunk representing this room's pinned events.
    ///
    /// This linked chunk also contains related events. The events are sorted in
    /// the chronological order (oldest to newest), since it would be otherwise
    /// impossible to order them correctly, given that we fetch their
    /// relations over time.
    chunk: EventLinkedChunk,

    /// Update sender for this pinned events cache.
    pub update_sender: PinnedEventsCacheUpdateSender,

    /// A sender for the globally observable linked chunk updates that happened
    /// during a sync or a back-pagination.
    ///
    /// See also [`super::super::EventCacheInner::linked_chunk_update_sender`].
    linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,
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

impl<'a> StateLockWriteGuard<'a, PinnedEventsCacheState> {
    /// Reload the pinned-events: only the last events will be reloaded,
    /// shrinking the in-memory size of the cache.
    ///
    /// If `preprocessing` is set to [`ReloadPreprocessing::ForgetAll`], all
    /// events will be erased before reloaded.
    #[must_use = "Propagate `VectorDiff` updates via `TimelineVectorDiffs`"]
    pub async fn reload(
        &mut self,
        preprocessing: ReloadPreprocessing,
    ) -> Result<Vec<VectorDiff<Event>>> {
        match preprocessing {
            ReloadPreprocessing::ForgetAll => {
                // Clear the `LinkedChunk` and broadcast the updates to the store.
                self.state.chunk.reset();
                self.propagate_changes().await?;
            }

            ReloadPreprocessing::None => {}
        }

        // The task will notice there is a desynchronisation and will reload from
        // network.
        self.reload_from_storage().await?;

        Ok(self.state.chunk.updates_as_vector_diffs())
    }

    async fn handle_sync(&mut self, timeline: Timeline) -> Result<()> {
        let DeduplicationOutcome {
            all_events: events,
            in_memory_duplicated_event_ids,
            in_store_duplicated_event_ids,
            non_empty_all_duplicates: all_duplicates,
        } = filter_duplicate_events(
            &self.state.own_user_id,
            &self.store,
            LinkedChunkId::PinnedEvents(&self.state.room_id),
            &self.state.chunk,
            timeline.events,
        )
        .await?;

        if all_duplicates {
            // If all events are duplicates, we don't need to do anything; ignore
            // the new events.
            return Ok(());
        }

        // Remove the old duplicated events.
        //
        // We don't have to worry about the removals can change the position of the
        // existing events, because we are pushing all _new_ `events` at the back.
        self.remove_events(in_memory_duplicated_event_ids, in_store_duplicated_event_ids).await?;

        // We've found new relations; append them to the linked chunk.
        self.state.chunk.push_live_events(None, &events);

        self.propagate_changes().await?;
        self.notify_subscribers(EventsOrigin::Sync);

        // Do stuff for each event.
        for event in &events {
            // Handle redaction.
            self.maybe_apply_new_redaction(event).await?;
        }

        Ok(())
    }

    /// Remove events by their position, in `EventLinkedChunk`.
    ///
    /// This method is purposely isolated because it must ensure that
    /// positions are sorted appropriately or it can be disastrous.
    #[instrument(skip_all)]
    pub async fn remove_events(
        &mut self,
        in_memory_events: Vec<(OwnedEventId, Position)>,
        in_store_events: Vec<(OwnedEventId, Position)>,
    ) -> Result<()> {
        // In-store events.
        if !in_store_events.is_empty() {
            let mut positions = in_store_events
                .into_iter()
                .map(|(_event_id, position)| position)
                .collect::<Vec<_>>();

            sort_positions_descending(&mut positions);

            let updates =
                positions.into_iter().map(|pos| Update::RemoveItem { at: pos }).collect::<Vec<_>>();

            self.apply_store_only_updates(updates).await?;
        }

        // In-memory events.
        if in_memory_events.is_empty() {
            // Nothing else to do, return early.
            return Ok(());
        }

        // `remove_events_by_position` is responsible of sorting positions.
        self.state
            .chunk
            .remove_events_by_position(
                in_memory_events.into_iter().map(|(_event_id, position)| position).collect(),
            )
            .expect("failed to remove an event");

        self.propagate_changes().await
    }

    /// Apply some updates that are effective only on the store itself.
    ///
    /// This method should be used only for updates that happen *outside*
    /// the in-memory linked chunk. Such updates must be applied
    /// onto the ordering tracker as well as to the persistent
    /// storage.
    async fn apply_store_only_updates(&mut self, updates: Vec<Update<Event, Gap>>) -> Result<()> {
        self.state.chunk.order_tracker.map_updates(&updates);
        self.send_updates_to_store(updates).await
    }

    /// If the given event is a redaction, try to retrieve the
    /// to-be-redacted event in the chunk, and replace it by the
    /// redacted form.
    #[instrument(skip_all)]
    async fn maybe_apply_new_redaction(&mut self, event: &Event) -> Result<()> {
        let Some(event_id) =
            extract_redaction_target(event.raw(), &self.room_version_rules.redaction)
        else {
            return Ok(());
        };

        // Replace the redacted event by a redacted form, if we knew about it.
        let Some((location, mut target_event)) = self.find_event(&event_id).await? else {
            trace!("redacted event is missing from the linked chunk");
            return Ok(());
        };

        let target_event_raw = target_event.raw();

        // Don't redact already redacted events.
        if let Ok(deserialized) = target_event_raw.deserialize()
            && deserialized.is_redacted()
        {
            return Ok(());
        }

        if let Some(redacted_event) = apply_redaction(
            target_event_raw,
            event.raw().cast_ref_unchecked::<SyncRoomRedactionEvent>(),
            &self.room_version_rules.redaction,
        ) {
            // It's safe to cast `redacted_event` here:
            // - either the event was an `AnyTimelineEvent` cast to `AnySyncTimelineEvent`
            //   when calling .raw(), so it's still one under the hood.
            // - or it wasn't, and it's a plain `AnySyncTimelineEvent` in this case.
            target_event.replace_raw(redacted_event.cast_unchecked());

            self.replace_event_at(location, target_event.clone()).await?;
        }

        Ok(())
    }

    /// See documentation of [`find_event`].
    pub(super) async fn find_event(
        &self,
        event_id: &EventId,
    ) -> Result<Option<(EventLocation, Event)>> {
        find_event(event_id, &self.room_id, &self.chunk, &self.store).await
    }

    /// Replaces a single event, be it saved in memory or in the store.
    ///
    /// If it was saved in memory, this will emit a notification to
    /// observers that a single item has been replaced. Otherwise,
    /// such a notification is not emitted, because observers are
    /// unlikely to observe the store updates directly.
    pub async fn replace_event_at(
        &mut self,
        location: EventLocation,
        new_event: Event,
    ) -> Result<()> {
        match location {
            EventLocation::Memory(position) => {
                self.state
                    .chunk
                    .replace_event_at(position, new_event)
                    .expect("should have been a valid position of an item");
                // We just changed the in-memory representation; synchronize this with
                // the store.
                self.propagate_changes().await?;
            }
            EventLocation::Store => {
                self.save_events([new_event]).await?;
            }
        }

        Ok(())
    }

    /// Save events into the database, without notifying observers.
    pub async fn save_events(&mut self, events: impl IntoIterator<Item = Event>) -> Result<()> {
        let store = self.store.clone();
        let room_id = self.state.room_id.clone();
        let events = events.into_iter().collect::<Vec<_>>();

        // Spawn a task so the save is uninterrupted by task cancellation.
        spawn(async move {
            for event in events {
                store.save_event(&room_id, event).await?;
            }

            Result::Ok(())
        })
        .await
        .expect("joining failed")?;

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

        if new_events
            .iter()
            .filter_map(|e| e.event_id())
            .map(ToOwned::to_owned)
            .collect::<BTreeSet<_>>()
            == previous_pinned_event_ids.into_iter().collect()
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
    pub async fn propagate_changes(&mut self) -> Result<()> {
        let updates = self.state.chunk.store_updates().take();

        self.send_updates_to_store(updates).await
    }

    async fn send_updates_to_store(&mut self, updates: Vec<Update<Event, Gap>>) -> Result<()> {
        let linked_chunk_id = OwnedLinkedChunkId::PinnedEvents(self.room_id.clone());

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
            self.update_sender.send(TimelineVectorDiffs { diffs, origin });
        }
    }
}

impl PinnedEventsCacheState {
    /// Return a list of the current event IDs in this linked chunk.
    pub(super) fn current_event_ids(&self) -> Vec<OwnedEventId> {
        self.chunk
            .events()
            .filter_map(|(_position, event)| event.event_id().map(ToOwned::to_owned))
            .collect()
    }
}

/// All the information related to room's pinned events..
///
/// Cloning is shallow, and thus is cheap to do.
#[derive(Clone)]
pub struct PinnedEventsCache {
    inner: Arc<PinnedEventsCacheInner>,

    /// The task handling the refreshing of pinned events for this specific
    /// room.
    _task: Arc<BackgroundTaskHandle>,
}

/// The (non-cloneable) details of the `PinnedEventsCache`.
struct PinnedEventsCacheInner {
    /// The ID of the room owning this list of pinned events.
    room_id: OwnedRoomId,

    /// State of this `PinnedEventsCache`.
    ///
    /// It is behind an `Arc` because it is shared with the task.
    state: CacheStateLock<PinnedEventsStateSelector>,
}

impl PinnedEventsCache {
    /// Creates a new [`PinnedEventsCache`] for the given room.
    pub(in super::super) async fn new(
        weak_room: &WeakRoom,
        own_user_id: OwnedUserId,
        room_version_rules: RoomVersionRules,
        linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,
        state: &StateLock,
    ) -> Result<Self> {
        let room = weak_room.get().ok_or(EventCacheError::ClientDropped)?;
        let room_id = room.room_id().to_owned();

        let cache_state = state
            .try_insert_once_with(
                PinnedEventsStateSelector::new(room_id.clone()),
                |_store_guard| async {
                    Ok(PinnedEventsCacheState {
                        room_id: room_id.clone(),
                        own_user_id,
                        room_version_rules,
                        chunk: EventLinkedChunk::new(),
                        update_sender: PinnedEventsCacheUpdateSender::new(),
                        linked_chunk_update_sender,
                    })
                },
            )
            .await?;

        let inner = Arc::new(PinnedEventsCacheInner { room_id, state: cache_state });

        let task = room
            .client()
            .task_monitor()
            .spawn_infinite_task(
                "pinned_event_listener_task",
                Self::pinned_event_listener_task(room, inner.clone()),
            )
            .abort_on_drop();

        Ok(Self { inner, _task: Arc::new(task) })
    }

    /// Return a reference to the state.
    pub(super) fn state(&self) -> &CacheStateLock<PinnedEventsStateSelector> {
        &self.inner.state
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
    pub(in super::super) async fn replace_utds(&self, events: &[ResolvedUtd]) -> Result<()> {
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

    #[instrument(fields(%room_id = room.room_id()), skip(room, inner))]
    async fn pinned_event_listener_task(room: Room, inner: Arc<PinnedEventsCacheInner>) {
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
            match inner.state.write().await {
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
        match inner.state.write().await {
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

            let guard = match inner.state.read().await {
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
        loaded_events.sort_by(compare_pinned_items);

        Ok(Some(loaded_events))
    }
}

impl fmt::Debug for PinnedEventsCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PinnedEventsCache").finish_non_exhaustive()
    }
}

fn compare_pinned_items(a: &Event, b: &Event) -> Ordering {
    let a_time: Option<MilliSecondsSinceUnixEpoch> = a.timestamp_raw();
    let b_time: Option<MilliSecondsSinceUnixEpoch> = b.timestamp_raw();

    compare_by_optional_timestamp(a_time, b_time)
}

fn compare_by_optional_timestamp(
    a: Option<MilliSecondsSinceUnixEpoch>,
    b: Option<MilliSecondsSinceUnixEpoch>,
) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => a.cmp(&b),
    }
}

#[cfg(not(target_family = "wasm"))]
#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use ruma::UInt;

    use super::*;

    fn any_timestamp() -> impl Strategy<Value = Option<MilliSecondsSinceUnixEpoch>> {
        prop::option::of(
            any::<u32>().prop_map(|value| MilliSecondsSinceUnixEpoch(UInt::from(value))),
        )
    }

    #[test]
    fn sort_pinned_events_never_panics_only_nones() {
        let mut vec = vec![None; 100_000];
        vec.sort_by(|a, b| compare_by_optional_timestamp(*a, *b))
    }

    proptest! {
    #[test]
    fn sort_pinned_events_never_panics(mut v in prop::collection::vec(any_timestamp(), 0..1000)) {
        v.sort_by(
            |a, b| compare_by_optional_timestamp(*a, *b))
    }

    #[test]
    fn compare_pinned_events_reflexive(a in any_timestamp()) {
        prop_assert_eq!(compare_by_optional_timestamp(a, a), Ordering::Equal);
    }

    #[test]
    fn compare_pinned_events_antisymmetric(a in any_timestamp(), b in any_timestamp()) {
        let ab = compare_by_optional_timestamp(a, b);
        let ba = compare_by_optional_timestamp(b, a);

        prop_assert_eq!(ab, ba.reverse());
    }

    #[test]
    fn compare_pinned_events_transitive(
        a in any_timestamp(),
        b in any_timestamp(),
        c in any_timestamp()
    ) {
        let ab = compare_by_optional_timestamp(a, b);
        let bc = compare_by_optional_timestamp(b, c);
        let ac = compare_by_optional_timestamp(a, c);

        if ab == Ordering::Less && bc == Ordering::Less {
            prop_assert_eq!(ac, Ordering::Less);
        }

        if ab == Ordering::Equal && bc == Ordering::Equal {
            prop_assert_eq!(ac, Ordering::Equal);
        }

        if ab == Ordering::Greater && bc == Ordering::Greater {
            prop_assert_eq!(ac, Ordering::Greater);
        }
    }
    }
}
