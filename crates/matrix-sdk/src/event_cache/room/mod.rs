// Copyright 2024 The Matrix.org Foundation C.I.C.
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

//! All event cache types for a single room.

use std::{collections::BTreeMap, fmt, sync::Arc};

use events::Gap;
use eyeball_im::VectorDiff;
use matrix_sdk_base::{
    deserialized_responses::{AmbiguityChange, TimelineEvent},
    sync::{JoinedRoomUpdate, LeftRoomUpdate, Timeline},
};
use ruma::{
    events::{relation::RelationType, AnyRoomAccountDataEvent, AnySyncEphemeralRoomEvent},
    serde::Raw,
    EventId, OwnedEventId, OwnedRoomId, RoomVersionId,
};
use tokio::sync::{
    broadcast::{Receiver, Sender},
    Notify, RwLock,
};
use tracing::{trace, warn};

use super::{
    paginator::{Paginator, PaginatorState},
    AllEventsCache, EventsOrigin, Result, RoomEventCacheUpdate, RoomPagination,
};
use crate::{client::WeakClient, room::WeakRoom};

pub(super) mod events;

/// A subset of an event cache, for a room.
///
/// Cloning is shallow, and thus is cheap to do.
#[derive(Clone)]
pub struct RoomEventCache {
    pub(super) inner: Arc<RoomEventCacheInner>,
}

impl fmt::Debug for RoomEventCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RoomEventCache").finish_non_exhaustive()
    }
}

impl RoomEventCache {
    /// Create a new [`RoomEventCache`] using the given room and store.
    pub(super) fn new(
        client: WeakClient,
        state: RoomEventCacheState,
        room_id: OwnedRoomId,
        room_version: RoomVersionId,
        all_events_cache: Arc<RwLock<AllEventsCache>>,
    ) -> Self {
        Self {
            inner: Arc::new(RoomEventCacheInner::new(
                client,
                state,
                room_id,
                room_version,
                all_events_cache,
            )),
        }
    }

    /// Subscribe to this room updates, after getting the initial list of
    /// events.
    pub async fn subscribe(&self) -> (Vec<TimelineEvent>, Receiver<RoomEventCacheUpdate>) {
        let state = self.inner.state.read().await;
        let events = state.events().events().map(|(_position, item)| item.clone()).collect();

        (events, self.inner.sender.subscribe())
    }

    /// Return a [`RoomPagination`] API object useful for running
    /// back-pagination queries in the current room.
    pub fn pagination(&self) -> RoomPagination {
        RoomPagination { inner: self.inner.clone() }
    }

    /// Try to find an event by id in this room.
    pub async fn event(&self, event_id: &EventId) -> Option<TimelineEvent> {
        if let Some((room_id, event)) =
            self.inner.all_events.read().await.events.get(event_id).cloned()
        {
            if room_id == self.inner.room_id {
                return Some(event);
            }
        }

        let state = self.inner.state.read().await;
        for (_pos, event) in state.events().revents() {
            if event.event_id().as_deref() == Some(event_id) {
                return Some(event.clone());
            }
        }
        None
    }

    /// Try to find an event by id in this room, along with its related events.
    ///
    /// You can filter which types of related events to retrieve using
    /// `filter`. `None` will retrieve related events of any type.
    pub async fn event_with_relations(
        &self,
        event_id: &EventId,
        filter: Option<Vec<RelationType>>,
    ) -> Option<(TimelineEvent, Vec<TimelineEvent>)> {
        let cache = self.inner.all_events.read().await;
        if let Some((_, event)) = cache.events.get(event_id) {
            let related_events = cache.collect_related_events(event_id, filter.as_deref());
            Some((event.clone(), related_events))
        } else {
            None
        }
    }

    /// Clear all the storage for this [`RoomEventCache`].
    ///
    /// This will get rid of all the events from the linked chunk and persisted
    /// storage.
    pub async fn clear(&self) -> Result<()> {
        // Clear the linked chunk and persisted storage.
        let updates_as_vector_diffs = self.inner.state.write().await.reset().await?;

        // Clear the (temporary) events mappings.
        self.inner.all_events.write().await.clear();

        // Reset the paginator.
        // TODO: properly stop any ongoing back-pagination.
        let _ = self.inner.paginator.set_idle_state(PaginatorState::Initial, None, None);

        // Notify observers about the update.
        let _ = self.inner.sender.send(RoomEventCacheUpdate::UpdateTimelineEvents {
            diffs: updates_as_vector_diffs,
            origin: EventsOrigin::Sync,
        });

        Ok(())
    }

    /// Save a single event in the event cache, for further retrieval with
    /// [`Self::event`].
    // TODO: This doesn't insert the event into the linked chunk. In the future
    // there'll be no distinction between the linked chunk and the separate
    // cache. There is a discussion in https://github.com/matrix-org/matrix-rust-sdk/issues/3886.
    pub(crate) async fn save_event(&self, event: TimelineEvent) {
        if let Some(event_id) = event.event_id() {
            let mut cache = self.inner.all_events.write().await;

            cache.append_related_event(&event);
            cache.events.insert(event_id, (self.inner.room_id.clone(), event));
        } else {
            warn!("couldn't save event without event id in the event cache");
        }
    }

    /// Save some events in the event cache, for further retrieval with
    /// [`Self::event`]. This function will save them using a single lock,
    /// as opposed to [`Self::save_event`].
    // TODO: This doesn't insert the event into the linked chunk. In the future
    // there'll be no distinction between the linked chunk and the separate
    // cache. There is a discussion in https://github.com/matrix-org/matrix-rust-sdk/issues/3886.
    pub(crate) async fn save_events(&self, events: impl IntoIterator<Item = TimelineEvent>) {
        let mut cache = self.inner.all_events.write().await;
        for event in events {
            if let Some(event_id) = event.event_id() {
                cache.append_related_event(&event);
                cache.events.insert(event_id, (self.inner.room_id.clone(), event));
            } else {
                warn!("couldn't save event without event id in the event cache");
            }
        }
    }

    /// Return a nice debug string (a vector of lines) for the linked chunk of
    /// events for this room.
    pub async fn debug_string(&self) -> Vec<String> {
        self.inner.state.read().await.events().debug_string()
    }
}

/// The (non-cloneable) details of the `RoomEventCache`.
pub(super) struct RoomEventCacheInner {
    /// The room id for this room.
    room_id: OwnedRoomId,

    /// The room version for this room.
    pub(crate) room_version: RoomVersionId,

    /// Sender part for subscribers to this room.
    pub sender: Sender<RoomEventCacheUpdate>,

    /// State for this room's event cache.
    pub state: RwLock<RoomEventCacheState>,

    /// See comment of [`super::EventCacheInner::all_events`].
    ///
    /// This is shared between the [`super::EventCacheInner`] singleton and all
    /// [`RoomEventCacheInner`] instances.
    all_events: Arc<RwLock<AllEventsCache>>,

    /// A notifier that we received a new pagination token.
    pub pagination_batch_token_notifier: Notify,

    /// A paginator instance, that's configured to run back-pagination on our
    /// behalf.
    ///
    /// Note: forward-paginations are still run "out-of-band", that is,
    /// disconnected from the event cache, as we don't implement matching
    /// events received from those kinds of pagination with the cache. This
    /// paginator is only used for queries that interact with the actual event
    /// cache.
    pub paginator: Paginator<WeakRoom>,
}

impl RoomEventCacheInner {
    /// Creates a new cache for a room, and subscribes to room updates, so as
    /// to handle new timeline events.
    fn new(
        client: WeakClient,
        state: RoomEventCacheState,
        room_id: OwnedRoomId,
        room_version: RoomVersionId,
        all_events_cache: Arc<RwLock<AllEventsCache>>,
    ) -> Self {
        let sender = Sender::new(32);
        let weak_room = WeakRoom::new(client, room_id);
        Self {
            room_id: weak_room.room_id().to_owned(),
            room_version,
            state: RwLock::new(state),
            all_events: all_events_cache,
            sender,
            pagination_batch_token_notifier: Default::default(),
            paginator: Paginator::new(weak_room),
        }
    }

    fn handle_account_data(&self, account_data: Vec<Raw<AnyRoomAccountDataEvent>>) {
        if account_data.is_empty() {
            return;
        }

        let mut handled_read_marker = false;

        trace!("Handling account data");

        for raw_event in account_data {
            match raw_event.deserialize() {
                Ok(AnyRoomAccountDataEvent::FullyRead(ev)) => {
                    // If duplicated, do not forward read marker multiple times
                    // to avoid clutter the update channel.
                    if handled_read_marker {
                        continue;
                    }

                    handled_read_marker = true;

                    // Propagate to observers. (We ignore the error if there aren't any.)
                    let _ = self.sender.send(RoomEventCacheUpdate::MoveReadMarkerTo {
                        event_id: ev.content.event_id,
                    });
                }

                Ok(_) => {
                    // We're not interested in other room account data updates,
                    // at this point.
                }

                Err(e) => {
                    let event_type = raw_event.get_field::<String>("type").ok().flatten();
                    warn!(event_type, "Failed to deserialize account data: {e}");
                }
            }
        }
    }

    pub(super) async fn handle_joined_room_update(
        &self,
        has_storage: bool,
        updates: JoinedRoomUpdate,
    ) -> Result<()> {
        self.handle_timeline(
            has_storage,
            updates.timeline,
            updates.ephemeral.clone(),
            updates.ambiguity_changes,
        )
        .await?;

        self.handle_account_data(updates.account_data);

        Ok(())
    }

    async fn handle_timeline(
        &self,
        has_storage: bool,
        timeline: Timeline,
        ephemeral_events: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    ) -> Result<()> {
        if !has_storage && timeline.limited {
            // Ideally we'd try to reconcile existing events against those received in the
            // timeline, but we're not there yet. In the meanwhile, clear the
            // items from the room. TODO: implement Smart Matching™.
            trace!("limited timeline, clearing all previous events and pushing new events");

            self.replace_all_events_by(
                timeline.events,
                timeline.prev_batch,
                ephemeral_events,
                ambiguity_changes,
            )
            .await?;
        } else {
            // Add all the events to the backend.
            trace!("adding new events");

            // If we have storage, only keep the previous-batch token if we have a limited
            // timeline. Otherwise, we know about all the events, and we don't need to
            // back-paginate, so we wouldn't make use of the given previous-batch token.
            //
            // If we don't have storage, even if the timeline isn't limited, we may not have
            // saved the previous events in any cache, so we should always be
            // able to retrieve those.
            let prev_batch =
                if has_storage && !timeline.limited { None } else { timeline.prev_batch };

            let mut state = self.state.write().await;
            self.append_events_locked(
                &mut state,
                timeline.events,
                prev_batch,
                ephemeral_events,
                ambiguity_changes,
            )
            .await?;
        }

        Ok(())
    }

    pub(super) async fn handle_left_room_update(
        &self,
        has_storage: bool,
        updates: LeftRoomUpdate,
    ) -> Result<()> {
        self.handle_timeline(has_storage, updates.timeline, Vec::new(), updates.ambiguity_changes)
            .await?;
        Ok(())
    }

    /// Remove existing events, and append a set of events to the room cache and
    /// storage, notifying observers.
    pub(super) async fn replace_all_events_by(
        &self,
        sync_timeline_events: Vec<TimelineEvent>,
        prev_batch: Option<String>,
        ephemeral_events: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    ) -> Result<()> {
        // Acquire the lock.
        let mut state = self.state.write().await;

        // Reset the room's state.
        let updates_as_vector_diffs = state.reset().await?;

        // Propagate to observers.
        let _ = self.sender.send(RoomEventCacheUpdate::UpdateTimelineEvents {
            diffs: updates_as_vector_diffs,
            origin: EventsOrigin::Sync,
        });

        // Push the new events.
        self.append_events_locked(
            &mut state,
            sync_timeline_events,
            prev_batch.clone(),
            ephemeral_events,
            ambiguity_changes,
        )
        .await?;

        // Reset the paginator status to initial.
        self.paginator.set_idle_state(PaginatorState::Initial, prev_batch, None)?;

        Ok(())
    }

    /// Append a set of events to the room cache and storage, notifying
    /// observers.
    ///
    /// This is a private implementation. It must not be exposed publicly.
    async fn append_events_locked(
        &self,
        state: &mut RoomEventCacheState,
        sync_timeline_events: Vec<TimelineEvent>,
        prev_batch: Option<String>,
        ephemeral_events: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    ) -> Result<()> {
        if sync_timeline_events.is_empty()
            && prev_batch.is_none()
            && ephemeral_events.is_empty()
            && ambiguity_changes.is_empty()
        {
            return Ok(());
        }

        let (events, duplicated_event_ids, all_duplicates) =
            state.collect_valid_and_duplicated_events(sync_timeline_events.clone()).await?;

        // During a sync, when a duplicated event is found, the old event is removed and
        // the new event is added. This is the opposite strategy than during a backwards
        // pagination where the old event is kept and the new event is ignored.
        //
        // Let's remove the old events that are duplicated.
        let sync_timeline_events_diffs = if all_duplicates {
            // No new events, thus no need to change the room events.
            vec![]
        } else {
            // Add the previous back-pagination token (if present), followed by the timeline
            // events themselves.
            let (_, sync_timeline_events_diffs) = state
                .with_events_mut(|room_events| {
                    if let Some(prev_token) = &prev_batch {
                        room_events.push_gap(Gap { prev_token: prev_token.clone() });
                    }

                    // Remove the old duplicated events.
                    //
                    // We don't have to worry the removals can change the position of the
                    // existing events, because we are pushing all _new_
                    // `events` at the back.
                    room_events.remove_events_by_id(duplicated_event_ids);

                    // Push the new events.
                    room_events.push_events(events.clone());

                    room_events.on_new_events(&self.room_version, events.iter());
                })
                .await?;

            {
                // Fill the AllEventsCache.
                let mut all_events = self.all_events.write().await;
                for sync_timeline_event in sync_timeline_events {
                    if let Some(event_id) = sync_timeline_event.event_id() {
                        all_events.append_related_event(&sync_timeline_event);
                        all_events.events.insert(
                            event_id.to_owned(),
                            (self.room_id.clone(), sync_timeline_event),
                        );
                    }
                }
            }

            sync_timeline_events_diffs
        };

        // Now that all events have been added, we can trigger the
        // `pagination_token_notifier`.
        if prev_batch.is_some() {
            self.pagination_batch_token_notifier.notify_one();
        }

        // The order of `RoomEventCacheUpdate`s is **really** important here.
        {
            if !sync_timeline_events_diffs.is_empty() {
                let _ = self.sender.send(RoomEventCacheUpdate::UpdateTimelineEvents {
                    diffs: sync_timeline_events_diffs,
                    origin: EventsOrigin::Sync,
                });
            }

            if !ephemeral_events.is_empty() {
                let _ = self
                    .sender
                    .send(RoomEventCacheUpdate::AddEphemeralEvents { events: ephemeral_events });
            }

            if !ambiguity_changes.is_empty() {
                let _ = self.sender.send(RoomEventCacheUpdate::UpdateMembers { ambiguity_changes });
            }
        }

        Ok(())
    }
}

/// Internal type to represent the output of
/// `RoomEventCacheState::load_more_events_backwards`.
#[derive(Debug)]
pub(super) enum LoadMoreEventsBackwardsOutcome {
    /// A gap has been inserted.
    Gap,

    /// The start of the timeline has been reached.
    StartOfTimeline,

    /// Events have been inserted.
    Events(Vec<TimelineEvent>, Vec<VectorDiff<TimelineEvent>>),
}

// Use a private module to hide `events` to this parent module.
mod private {
    use std::sync::Arc;

    use eyeball_im::VectorDiff;
    use matrix_sdk_base::{
        deserialized_responses::{TimelineEvent, TimelineEventKind},
        event_cache::{store::EventCacheStoreLock, Event},
        linked_chunk::{lazy_loader, ChunkContent, Update},
    };
    use matrix_sdk_common::executor::spawn;
    use once_cell::sync::OnceCell;
    use ruma::{serde::Raw, OwnedEventId, OwnedRoomId};
    use tracing::{error, instrument, trace};

    use super::{events::RoomEvents, LoadMoreEventsBackwardsOutcome};
    use crate::event_cache::{deduplicator::Deduplicator, EventCacheError};

    /// State for a single room's event cache.
    ///
    /// This contains all the inner mutable states that ought to be updated at
    /// the same time.
    pub struct RoomEventCacheState {
        /// The room this state relates to.
        room: OwnedRoomId,

        /// Reference to the underlying backing store.
        ///
        /// Set to none if the room shouldn't read the linked chunk from
        /// storage, and shouldn't store updates to storage.
        store: Arc<OnceCell<EventCacheStoreLock>>,

        /// The events of the room.
        events: RoomEvents,

        /// The events deduplicator instance to help finding duplicates.
        deduplicator: Deduplicator,

        /// Have we ever waited for a previous-batch-token to come from sync, in
        /// the context of pagination? We do this at most once per room,
        /// the first time we try to run backward pagination. We reset
        /// that upon clearing the timeline events.
        pub waited_for_initial_prev_token: bool,
    }

    impl RoomEventCacheState {
        /// Create a new state, or reload it from storage if it's been enabled.
        ///
        /// Not all events are going to be loaded. Only a portion of them. The
        /// [`RoomEvents`] relies on a [`LinkedChunk`] to store all events. Only
        /// the last chunk will be loaded. It means the events are loaded from
        /// the most recent to the oldest. To load more events, see
        /// [`Self::load_more_events_backwards`].
        ///
        /// [`LinkedChunk`]: matrix_sdk_common::linked_chunk::LinkedChunk
        pub async fn new(
            room_id: OwnedRoomId,
            store: Arc<OnceCell<EventCacheStoreLock>>,
        ) -> Result<Self, EventCacheError> {
            let (events, deduplicator) = if let Some(store) = store.get() {
                let store_lock = store.lock().await?;

                let linked_chunk = match store_lock
                    .load_last_chunk(&room_id)
                    .await
                    .map_err(EventCacheError::from)
                    .and_then(|(last_chunk, chunk_identifier_generator)| {
                        lazy_loader::from_last_chunk(last_chunk, chunk_identifier_generator)
                            .map_err(EventCacheError::from)
                    }) {
                    Ok(linked_chunk) => linked_chunk,

                    Err(err) => {
                        error!("error when reloading a linked chunk from memory: {err}");

                        // Clear storage for this room.
                        store_lock
                            .handle_linked_chunk_updates(&room_id, vec![Update::Clear])
                            .await?;

                        // Restart with an empty linked chunk.
                        None
                    }
                };

                (
                    RoomEvents::with_initial_linked_chunk(linked_chunk),
                    Deduplicator::new_store_based(room_id.clone(), store.clone()),
                )
            } else {
                (RoomEvents::default(), Deduplicator::new_memory_based())
            };

            Ok(Self {
                room: room_id,
                store,
                events,
                deduplicator,
                waited_for_initial_prev_token: false,
            })
        }

        /// Deduplicate `events` considering all events in `Self::events`.
        ///
        /// The returned tuple contains:
        /// - all events (duplicated or not) with an ID
        /// - all the duplicated event IDs
        /// - a boolean indicating all events (at least one) are duplicates.
        ///
        /// This last boolean is useful to know whether we need to store a
        /// previous-batch token (gap) we received from a server-side
        /// request (sync or back-pagination), or if we should
        /// *not* store it.
        ///
        /// Since there can be empty back-paginations with a previous-batch
        /// token (that is, they don't contain any events), we need to
        /// make sure that there is *at least* one new event that has
        /// been added. Otherwise, we might conclude something wrong
        /// because a subsequent back-pagination might
        /// return non-duplicated events.
        ///
        /// If we had already seen all the duplicated events that we're trying
        /// to add, then it would be wasteful to store a previous-batch
        /// token, or even touch the linked chunk: we would repeat
        /// back-paginations for events that we have already seen, and
        /// possibly misplace them. And we should not be missing
        /// events either: the already-known events would have their own
        /// previous-batch token (it might already be consumed).
        pub async fn collect_valid_and_duplicated_events(
            &mut self,
            events: Vec<Event>,
        ) -> Result<(Vec<Event>, Vec<OwnedEventId>, bool), EventCacheError> {
            let (events, duplicated_event_ids) =
                self.deduplicator.filter_duplicate_events(events, &self.events).await?;

            let all_duplicates = !events.is_empty() && events.len() == duplicated_event_ids.len();

            Ok((events, duplicated_event_ids, all_duplicates))
        }

        /// Load more events backwards if the last chunk is **not** a gap.
        #[must_use = "Updates as `VectorDiff` must probably be propagated via `RoomEventCacheUpdate`"]
        pub(in super::super) async fn load_more_events_backwards(
            &mut self,
        ) -> Result<LoadMoreEventsBackwardsOutcome, EventCacheError> {
            let Some(store) = self.store.get() else {
                // No store: no events to insert. Pretend the caller has to act as if a gap was
                // present.
                return Ok(LoadMoreEventsBackwardsOutcome::Gap);
            };

            // If any in-memory chunk is a gap, don't load more events, and let the caller
            // resolve the gap.
            if self.events.chunks().any(|chunk| chunk.is_gap()) {
                return Ok(LoadMoreEventsBackwardsOutcome::Gap);
            }

            // Because `first_chunk` is `not `Send`, get this information before the
            // `.await` point, so that this `Future` can implement `Send`.
            let first_chunk_identifier =
                self.events.chunks().next().expect("a linked chunk is never empty").identifier();

            let room_id = &self.room;
            let store = store.lock().await?;

            // The first chunk is not a gap, we can load its previous chunk.
            let new_first_chunk =
                match store.load_previous_chunk(room_id, first_chunk_identifier).await {
                    Ok(Some(new_first_chunk)) => {
                        // All good, let's continue with this chunk.
                        new_first_chunk
                    }
                    Ok(None) => {
                        // No previous chunk: no events to insert. Better, it means we've reached
                        // the start of the timeline!
                        return Ok(LoadMoreEventsBackwardsOutcome::StartOfTimeline);
                    }
                    Err(err) => {
                        error!("error when loading the previous chunk of a linked chunk: {err}");

                        // Clear storage for this room.
                        store.handle_linked_chunk_updates(room_id, vec![Update::Clear]).await?;

                        // Return the error.
                        return Err(err.into());
                    }
                };

            let events = match &new_first_chunk.content {
                ChunkContent::Gap(_) => None,
                ChunkContent::Items(events) => Some(events.clone()),
            };

            if let Err(err) = self.events.insert_new_chunk_as_first(new_first_chunk) {
                error!("error when inserting the previous chunk into its linked chunk: {err}");

                // Clear storage for this room.
                store.handle_linked_chunk_updates(room_id, vec![Update::Clear]).await?;

                // Return the error.
                return Err(err.into());
            };

            // ⚠️ Let's not propagate the updates to the store! We already have these data
            // in the store! Let's drain them.
            let _ = self.events.updates().take();

            // However, we want to get updates as `VectorDiff`s.
            let updates_as_vector_diffs = self.events.updates_as_vector_diffs();

            Ok(match events {
                None => LoadMoreEventsBackwardsOutcome::Gap,
                Some(events) => {
                    LoadMoreEventsBackwardsOutcome::Events(events, updates_as_vector_diffs)
                }
            })
        }
        /// Removes the bundled relations from an event, if they were present.
        ///
        /// Only replaces the present if it contained bundled relations.
        fn strip_relations_if_present<T>(event: &mut Raw<T>) {
            // We're going to get rid of the `unsigned`/`m.relations` field, if it's
            // present.
            // Use a closure that returns an option so we can quickly short-circuit.
            let mut closure = || -> Option<()> {
                let mut val: serde_json::Value = event.deserialize_as().ok()?;
                let unsigned = val.get_mut("unsigned")?;
                let unsigned_obj = unsigned.as_object_mut()?;
                if unsigned_obj.remove("m.relations").is_some() {
                    *event = Raw::new(&val).ok()?.cast();
                }
                None
            };
            let _ = closure();
        }

        fn strip_relations_from_event(ev: &mut TimelineEvent) {
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
        fn strip_relations_from_events(items: &mut [TimelineEvent]) {
            for ev in items.iter_mut() {
                Self::strip_relations_from_event(ev);
            }
        }

        /// Propagate changes to the underlying storage.
        #[instrument(skip_all)]
        async fn propagate_changes(&mut self) -> Result<(), EventCacheError> {
            let mut updates = self.events.updates().take();

            if updates.is_empty() {
                return Ok(());
            }

            let Some(store) = self.store.get() else {
                return Ok(());
            };

            trace!("propagating {} updates", updates.len());

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

            let store = store.clone();
            let room_id = self.room.clone();

            spawn(async move {
                let store = store.lock().await?;

                if let Err(err) = store.handle_linked_chunk_updates(&room_id, updates).await {
                    error!("unable to handle linked chunk updates: {err}");
                }

                super::Result::Ok(())
            })
            .await
            .expect("joining failed")?;

            trace!("done propagating store changes");

            Ok(())
        }

        /// Resets this data structure as if it were brand new.
        #[must_use = "Updates as `VectorDiff` must probably be propagated via `RoomEventCacheUpdate`"]
        pub async fn reset(&mut self) -> Result<Vec<VectorDiff<TimelineEvent>>, EventCacheError> {
            self.events.reset();
            self.propagate_changes().await?;
            self.waited_for_initial_prev_token = false;

            Ok(self.events.updates_as_vector_diffs())
        }

        /// Returns a read-only reference to the underlying events.
        pub fn events(&self) -> &RoomEvents {
            &self.events
        }

        /// Gives a temporary mutable handle to the underlying in-memory events,
        /// and will propagate changes to the storage once done.
        ///
        /// Returns the output of the given callback, as well as updates to the
        /// linked chunk, as vector diff, so the caller may propagate
        /// such updates, if needs be.
        #[must_use = "Updates as `VectorDiff` must probably be propagated via `RoomEventCacheUpdate`"]
        pub async fn with_events_mut<O, F: FnOnce(&mut RoomEvents) -> O>(
            &mut self,
            func: F,
        ) -> Result<(O, Vec<VectorDiff<TimelineEvent>>), EventCacheError> {
            let output = func(&mut self.events);
            self.propagate_changes().await?;
            let updates_as_vector_diffs = self.events.updates_as_vector_diffs();
            Ok((output, updates_as_vector_diffs))
        }
    }
}

pub(super) use private::RoomEventCacheState;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use assert_matches::assert_matches;
    use assert_matches2::assert_let;
    use matrix_sdk_base::{
        event_cache::{
            store::{EventCacheStore as _, MemoryStore},
            Gap,
        },
        linked_chunk::{ChunkContent, ChunkIdentifier, Position, Update},
        store::StoreConfig,
        sync::{JoinedRoomUpdate, Timeline},
    };
    use matrix_sdk_common::deserialized_responses::TimelineEvent;
    use matrix_sdk_test::{async_test, event_factory::EventFactory, ALICE, BOB};
    use ruma::{
        event_id,
        events::{
            relation::RelationType, room::message::RoomMessageEventContentWithoutRelation,
            AnySyncMessageLikeEvent, AnySyncTimelineEvent,
        },
        room_id, user_id, RoomId,
    };

    use crate::test_utils::{client::MockClientBuilder, logged_in_client};

    #[async_test]
    async fn test_event_with_redaction_relation() {
        let original_id = event_id!("$original");
        let related_id = event_id!("$related");
        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        assert_relations(
            room_id,
            f.text_msg("Original event").event_id(original_id).into(),
            f.redaction(original_id).event_id(related_id).into(),
            f,
        )
        .await;
    }

    #[async_test]
    async fn test_event_with_edit_relation() {
        let original_id = event_id!("$original");
        let related_id = event_id!("$related");
        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        assert_relations(
            room_id,
            f.text_msg("Original event").event_id(original_id).into(),
            f.text_msg("* An edited event")
                .edit(
                    original_id,
                    RoomMessageEventContentWithoutRelation::text_plain("And edited event"),
                )
                .event_id(related_id)
                .into(),
            f,
        )
        .await;
    }

    #[async_test]
    async fn test_event_with_reply_relation() {
        let original_id = event_id!("$original");
        let related_id = event_id!("$related");
        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        assert_relations(
            room_id,
            f.text_msg("Original event").event_id(original_id).into(),
            f.text_msg("A reply").reply_to(original_id).event_id(related_id).into(),
            f,
        )
        .await;
    }

    #[async_test]
    async fn test_event_with_thread_reply_relation() {
        let original_id = event_id!("$original");
        let related_id = event_id!("$related");
        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        assert_relations(
            room_id,
            f.text_msg("Original event").event_id(original_id).into(),
            f.text_msg("A reply").in_thread(original_id, related_id).event_id(related_id).into(),
            f,
        )
        .await;
    }

    #[async_test]
    async fn test_event_with_reaction_relation() {
        let original_id = event_id!("$original");
        let related_id = event_id!("$related");
        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        assert_relations(
            room_id,
            f.text_msg("Original event").event_id(original_id).into(),
            f.reaction(original_id, ":D").event_id(related_id).into(),
            f,
        )
        .await;
    }

    #[async_test]
    async fn test_event_with_poll_response_relation() {
        let original_id = event_id!("$original");
        let related_id = event_id!("$related");
        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        assert_relations(
            room_id,
            f.poll_start("Poll start event", "A poll question", vec!["An answer"])
                .event_id(original_id)
                .into(),
            f.poll_response(vec!["1"], original_id).event_id(related_id).into(),
            f,
        )
        .await;
    }

    #[async_test]
    async fn test_event_with_poll_end_relation() {
        let original_id = event_id!("$original");
        let related_id = event_id!("$related");
        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        assert_relations(
            room_id,
            f.poll_start("Poll start event", "A poll question", vec!["An answer"])
                .event_id(original_id)
                .into(),
            f.poll_end("Poll ended", original_id).event_id(related_id).into(),
            f,
        )
        .await;
    }

    #[async_test]
    async fn test_event_with_filtered_relationships() {
        let original_id = event_id!("$original");
        let related_id = event_id!("$related");
        let associated_related_id = event_id!("$recursive_related");
        let room_id = room_id!("!galette:saucisse.bzh");
        let event_factory = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        let original_event = event_factory.text_msg("Original event").event_id(original_id).into();
        let related_event = event_factory
            .text_msg("* Edited event")
            .edit(original_id, RoomMessageEventContentWithoutRelation::text_plain("Edited event"))
            .event_id(related_id)
            .into();
        let associated_related_event =
            event_factory.redaction(related_id).event_id(associated_related_id).into();

        let client = logged_in_client(None).await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Save the original event.
        room_event_cache.save_event(original_event).await;

        // Save the related event.
        room_event_cache.save_event(related_event).await;

        // Save the associated related event, which redacts the related event.
        room_event_cache.save_event(associated_related_event).await;

        let filter = Some(vec![RelationType::Replacement]);
        let (event, related_events) =
            room_event_cache.event_with_relations(original_id, filter).await.unwrap();
        // Fetched event is the right one.
        let cached_event_id = event.event_id().unwrap();
        assert_eq!(cached_event_id, original_id);

        // There are both the related id and the associatively related id
        assert_eq!(related_events.len(), 2);

        let related_event_id = related_events[0].event_id().unwrap();
        assert_eq!(related_event_id, related_id);
        let related_event_id = related_events[1].event_id().unwrap();
        assert_eq!(related_event_id, associated_related_id);

        // Now we'll filter threads instead, there should be no related events
        let filter = Some(vec![RelationType::Thread]);
        let (event, related_events) =
            room_event_cache.event_with_relations(original_id, filter).await.unwrap();
        // Fetched event is the right one.
        let cached_event_id = event.event_id().unwrap();
        assert_eq!(cached_event_id, original_id);
        // No Thread related events found
        assert!(related_events.is_empty());
    }

    #[async_test]
    async fn test_event_with_recursive_relation() {
        let original_id = event_id!("$original");
        let related_id = event_id!("$related");
        let associated_related_id = event_id!("$recursive_related");
        let room_id = room_id!("!galette:saucisse.bzh");
        let event_factory = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        let original_event = event_factory.text_msg("Original event").event_id(original_id).into();
        let related_event = event_factory
            .text_msg("* Edited event")
            .edit(original_id, RoomMessageEventContentWithoutRelation::text_plain("Edited event"))
            .event_id(related_id)
            .into();
        let associated_related_event =
            event_factory.redaction(related_id).event_id(associated_related_id).into();

        let client = logged_in_client(None).await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Save the original event.
        room_event_cache.save_event(original_event).await;

        // Save the related event.
        room_event_cache.save_event(related_event).await;

        // Save the associated related event, which redacts the related event.
        room_event_cache.save_event(associated_related_event).await;

        let (event, related_events) =
            room_event_cache.event_with_relations(original_id, None).await.unwrap();
        // Fetched event is the right one.
        let cached_event_id = event.event_id().unwrap();
        assert_eq!(cached_event_id, original_id);

        // There are both the related id and the associatively related id
        assert_eq!(related_events.len(), 2);

        let related_event_id = related_events[0].event_id().unwrap();
        assert_eq!(related_event_id, related_id);
        let related_event_id = related_events[1].event_id().unwrap();
        assert_eq!(related_event_id, associated_related_id);
    }

    #[cfg(not(target_arch = "wasm32"))] // This uses the cross-process lock, so needs time support.
    #[async_test]
    async fn test_write_to_storage() {
        use matrix_sdk_base::linked_chunk::lazy_loader::from_all_chunks;

        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        let event_cache_store = Arc::new(MemoryStore::new());

        let client = MockClientBuilder::new("http://localhost".to_owned())
            .store_config(
                StoreConfig::new("hodlor".to_owned()).event_cache_store(event_cache_store.clone()),
            )
            .build()
            .await;

        let event_cache = client.event_cache();

        // Don't forget to subscribe and like^W enable storage!
        event_cache.subscribe().unwrap();
        event_cache.enable_storage().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Propagate an update for a message and a prev-batch token.
        let timeline = Timeline {
            limited: true,
            prev_batch: Some("raclette".to_owned()),
            events: vec![f.text_msg("hey yo").sender(*ALICE).into_event()],
        };

        room_event_cache
            .inner
            .handle_joined_room_update(true, JoinedRoomUpdate { timeline, ..Default::default() })
            .await
            .unwrap();

        let linked_chunk =
            from_all_chunks::<3, _, _>(event_cache_store.load_all_chunks(room_id).await.unwrap())
                .unwrap()
                .unwrap();

        assert_eq!(linked_chunk.chunks().count(), 3);

        let mut chunks = linked_chunk.chunks();

        // Invariant: there's always an empty items chunk at the beginning.
        assert_matches!(chunks.next().unwrap().content(), ChunkContent::Items(events) => {
            assert_eq!(events.len(), 0)
        });

        // Then we have the gap.
        assert_matches!(chunks.next().unwrap().content(), ChunkContent::Gap(gap) => {
            assert_eq!(gap.prev_token, "raclette");
        });

        // Then we have the stored event.
        assert_matches!(chunks.next().unwrap().content(), ChunkContent::Items(events) => {
            assert_eq!(events.len(), 1);
            let deserialized = events[0].raw().deserialize().unwrap();
            assert_let!(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(msg)) = deserialized);
            assert_eq!(msg.as_original().unwrap().content.body(), "hey yo");
        });

        // That's all, folks!
        assert!(chunks.next().is_none());
    }

    #[cfg(not(target_arch = "wasm32"))] // This uses the cross-process lock, so needs time support.
    #[async_test]
    async fn test_write_to_storage_strips_bundled_relations() {
        use matrix_sdk_base::linked_chunk::lazy_loader::from_all_chunks;
        use ruma::events::BundledMessageLikeRelations;

        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        let event_cache_store = Arc::new(MemoryStore::new());

        let client = MockClientBuilder::new("http://localhost".to_owned())
            .store_config(
                StoreConfig::new("hodlor".to_owned()).event_cache_store(event_cache_store.clone()),
            )
            .build()
            .await;

        let event_cache = client.event_cache();

        // Don't forget to subscribe and like^W enable storage!
        event_cache.subscribe().unwrap();
        event_cache.enable_storage().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Propagate an update for a message with bundled relations.
        let mut relations = BundledMessageLikeRelations::new();
        relations.replace =
            Some(Box::new(f.text_msg("Hello, Kind Sir").sender(*ALICE).into_raw_sync()));
        let ev = f.text_msg("hey yo").sender(*ALICE).bundled_relations(relations).into_event();

        let timeline = Timeline { limited: false, prev_batch: None, events: vec![ev] };

        room_event_cache
            .inner
            .handle_joined_room_update(true, JoinedRoomUpdate { timeline, ..Default::default() })
            .await
            .unwrap();

        // The in-memory linked chunk keeps the bundled relation.
        {
            let (events, _) = room_event_cache.subscribe().await;

            assert_eq!(events.len(), 1);

            let ev = events[0].raw().deserialize().unwrap();
            assert_let!(
                AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(msg)) = ev
            );

            let original = msg.as_original().unwrap();
            assert_eq!(original.content.body(), "hey yo");
            assert!(original.unsigned.relations.replace.is_some());
        }

        // The one in storage does not.
        let linked_chunk =
            from_all_chunks::<3, _, _>(event_cache_store.load_all_chunks(room_id).await.unwrap())
                .unwrap()
                .unwrap();

        assert_eq!(linked_chunk.chunks().count(), 1);

        let mut chunks = linked_chunk.chunks();
        assert_matches!(chunks.next().unwrap().content(), ChunkContent::Items(events) => {
            assert_eq!(events.len(), 1);

            let ev = events[0].raw().deserialize().unwrap();
            assert_let!(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(msg)) = ev);

            let original = msg.as_original().unwrap();
            assert_eq!(original.content.body(), "hey yo");
            assert!(original.unsigned.relations.replace.is_none());
        });

        // That's all, folks!
        assert!(chunks.next().is_none());
    }

    #[cfg(not(target_arch = "wasm32"))] // This uses the cross-process lock, so needs time support.
    #[async_test]
    async fn test_clear() {
        use std::ops::ControlFlow;

        use eyeball_im::VectorDiff;
        use matrix_sdk_base::linked_chunk::lazy_loader::from_all_chunks;

        use crate::{assert_let_timeout, event_cache::RoomEventCacheUpdate};

        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        let event_cache_store = Arc::new(MemoryStore::new());

        let event_id1 = event_id!("$1");
        let event_id2 = event_id!("$2");

        let ev1 = f.text_msg("hello world").sender(*ALICE).event_id(event_id1).into_event();
        let ev2 = f.text_msg("how's it going").sender(*BOB).event_id(event_id2).into_event();

        // Prefill the store with some data.
        event_cache_store
            .handle_linked_chunk_updates(
                room_id,
                vec![
                    // An empty items chunk.
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(0),
                        next: None,
                    },
                    // A gap chunk.
                    Update::NewGapChunk {
                        previous: Some(ChunkIdentifier::new(0)),
                        // Chunk IDs aren't supposed to be ordered, so use a random value here.
                        new: ChunkIdentifier::new(42),
                        next: None,
                        gap: Gap { prev_token: "comté".to_owned() },
                    },
                    // Another items chunk, non-empty this time.
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(42)),
                        new: ChunkIdentifier::new(1),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(1), 0),
                        items: vec![ev1.clone()],
                    },
                    // And another items chunk, non-empty again.
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(1)),
                        new: ChunkIdentifier::new(2),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(2), 0),
                        items: vec![ev2.clone()],
                    },
                ],
            )
            .await
            .unwrap();

        let client = MockClientBuilder::new("http://localhost".to_owned())
            .store_config(
                StoreConfig::new("hodlor".to_owned()).event_cache_store(event_cache_store.clone()),
            )
            .build()
            .await;

        let event_cache = client.event_cache();

        // Don't forget to subscribe and like^W enable storage!
        event_cache.subscribe().unwrap();
        event_cache.enable_storage().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        let (items, mut stream) = room_event_cache.subscribe().await;

        // The rooms knows about some cached events.
        {
            // The chunk containing this event is not loaded yet
            assert!(room_event_cache.event(event_id1).await.is_none());
            // The chunk containing this event **is** loaded.
            assert!(room_event_cache.event(event_id2).await.is_some());

            // The reloaded room must contain only one event.
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].event_id().unwrap(), event_id2);

            assert!(stream.is_empty());
        }

        // Let's load more chunks to get all events.
        {
            room_event_cache
                .pagination()
                .run_backwards(20, |outcome, _| async move { ControlFlow::Break(outcome) })
                .await
                .unwrap();

            assert_let_timeout!(
                Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = stream.recv()
            );
            assert_eq!(diffs.len(), 1);
            assert_matches!(&diffs[0], VectorDiff::Insert { index: 0, value: _ });

            // The rooms knows about more cached events.
            assert!(room_event_cache.event(event_id1).await.is_some());
            assert!(room_event_cache.event(event_id2).await.is_some());

            assert!(stream.is_empty());
        }

        // After clearing,…
        room_event_cache.clear().await.unwrap();

        //… we get an update that the content has been cleared.
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = stream.recv()
        );
        assert_eq!(diffs.len(), 1);
        assert_let!(VectorDiff::Clear = &diffs[0]);

        // The room event cache has forgotten about the events.
        assert!(room_event_cache.event(event_id1).await.is_none());

        let (items, _) = room_event_cache.subscribe().await;
        assert!(items.is_empty());

        // The event cache store too.
        let linked_chunk =
            from_all_chunks::<3, _, _>(event_cache_store.load_all_chunks(room_id).await.unwrap())
                .unwrap()
                .unwrap();

        // Note: while the event cache store could return `None` here, clearing it will
        // reset it to its initial form, maintaining the invariant that it
        // contains a single items chunk that's empty.
        assert_eq!(linked_chunk.num_items(), 0);
    }

    #[cfg(not(target_arch = "wasm32"))] // This uses the cross-process lock, so needs time support.
    #[async_test]
    async fn test_load_from_storage() {
        use std::ops::ControlFlow;

        use eyeball_im::VectorDiff;

        use super::RoomEventCacheUpdate;
        use crate::assert_let_timeout;

        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        let event_cache_store = Arc::new(MemoryStore::new());

        let event_id1 = event_id!("$1");
        let event_id2 = event_id!("$2");

        let ev1 = f.text_msg("hello world").sender(*ALICE).event_id(event_id1).into_event();
        let ev2 = f.text_msg("how's it going").sender(*BOB).event_id(event_id2).into_event();

        // Prefill the store with some data.
        event_cache_store
            .handle_linked_chunk_updates(
                room_id,
                vec![
                    // An empty items chunk.
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(0),
                        next: None,
                    },
                    // A gap chunk.
                    Update::NewGapChunk {
                        previous: Some(ChunkIdentifier::new(0)),
                        // Chunk IDs aren't supposed to be ordered, so use a random value here.
                        new: ChunkIdentifier::new(42),
                        next: None,
                        gap: Gap { prev_token: "cheddar".to_owned() },
                    },
                    // Another items chunk, non-empty this time.
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(42)),
                        new: ChunkIdentifier::new(1),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(1), 0),
                        items: vec![ev1.clone()],
                    },
                    // And another items chunk, non-empty again.
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(1)),
                        new: ChunkIdentifier::new(2),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(2), 0),
                        items: vec![ev2.clone()],
                    },
                ],
            )
            .await
            .unwrap();

        let client = MockClientBuilder::new("http://localhost".to_owned())
            .store_config(
                StoreConfig::new("hodlor".to_owned()).event_cache_store(event_cache_store.clone()),
            )
            .build()
            .await;

        let event_cache = client.event_cache();

        // Don't forget to subscribe and like^W enable storage!
        event_cache.subscribe().unwrap();
        event_cache.enable_storage().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        let (items, mut stream) = room_event_cache.subscribe().await;

        // The initial items contain one event because only the last chunk is loaded by
        // default.
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].event_id().unwrap(), event_id2);
        assert!(stream.is_empty());

        // The event cache knows only one event.
        assert!(room_event_cache.event(event_id1).await.is_none());
        assert!(room_event_cache.event(event_id2).await.is_some());

        // Let's paginate to load more events.
        room_event_cache
            .pagination()
            .run_backwards(20, |outcome, _| async move { ControlFlow::Break(outcome) })
            .await
            .unwrap();

        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = stream.recv()
        );
        assert_eq!(diffs.len(), 1);
        assert_matches!(&diffs[0], VectorDiff::Insert { index: 0, value: _ });

        // The event cache knows about the two events now!
        assert!(room_event_cache.event(event_id1).await.is_some());
        assert!(room_event_cache.event(event_id2).await.is_some());

        assert!(stream.is_empty());

        // A new update with one of these events leads to deduplication.
        let timeline = Timeline { limited: false, prev_batch: None, events: vec![ev2] };
        room_event_cache
            .inner
            .handle_joined_room_update(true, JoinedRoomUpdate { timeline, ..Default::default() })
            .await
            .unwrap();

        // The stream doesn't report these changes *yet*. Use the items vector given
        // when subscribing, to check that the items correspond to their new
        // positions. The duplicated item is removed (so it's not the first
        // element anymore), and it's added to the back of the list.
        let (items, _stream) = room_event_cache.subscribe().await;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].event_id().unwrap(), event_id1);
        assert_eq!(items[1].event_id().unwrap(), event_id2);
    }

    #[cfg(not(target_arch = "wasm32"))] // This uses the cross-process lock, so needs time support.
    #[async_test]
    async fn test_load_from_storage_resilient_to_failure() {
        let room_id = room_id!("!fondue:patate.ch");
        let event_cache_store = Arc::new(MemoryStore::new());

        let event = EventFactory::new()
            .room(room_id)
            .sender(user_id!("@ben:saucisse.bzh"))
            .text_msg("foo")
            .event_id(event_id!("$42"))
            .into_event();

        // Prefill the store with invalid data: two chunks that form a cycle.
        event_cache_store
            .handle_linked_chunk_updates(
                room_id,
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(0),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(0), 0),
                        items: vec![event],
                    },
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(0)),
                        new: ChunkIdentifier::new(1),
                        next: Some(ChunkIdentifier::new(0)),
                    },
                ],
            )
            .await
            .unwrap();

        let client = MockClientBuilder::new("http://localhost".to_owned())
            .store_config(
                StoreConfig::new("holder".to_owned()).event_cache_store(event_cache_store.clone()),
            )
            .build()
            .await;

        let event_cache = client.event_cache();

        // Don't forget to subscribe and like^W enable storage!
        event_cache.subscribe().unwrap();
        event_cache.enable_storage().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        let (items, _stream) = room_event_cache.subscribe().await;

        // Because the persisted content was invalid, the room store is reset: there are
        // no events in the cache.
        assert!(items.is_empty());

        // Storage doesn't contain anything. It would also be valid that it contains a
        // single initial empty items chunk.
        let raw_chunks = event_cache_store.load_all_chunks(room_id).await.unwrap();
        assert!(raw_chunks.is_empty());
    }

    #[cfg(not(target_arch = "wasm32"))] // This uses the cross-process lock, so needs time support.
    #[async_test]
    async fn test_no_useless_gaps() {
        let room_id = room_id!("!galette:saucisse.bzh");

        let client = MockClientBuilder::new("http://localhost".to_owned()).build().await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let has_storage = true; // for testing purposes only
        event_cache.enable_storage().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();
        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        let f = EventFactory::new().room(room_id).sender(*ALICE);

        // Propagate an update including a limited timeline with one message and a
        // prev-batch token.
        room_event_cache
            .inner
            .handle_joined_room_update(
                has_storage,
                JoinedRoomUpdate {
                    timeline: Timeline {
                        limited: true,
                        prev_batch: Some("raclette".to_owned()),
                        events: vec![f.text_msg("hey yo").into_event()],
                    },
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        {
            let state = room_event_cache.inner.state.read().await;

            let mut num_gaps = 0;
            let mut num_events = 0;

            for c in state.events().chunks() {
                match c.content() {
                    ChunkContent::Items(items) => num_events += items.len(),
                    ChunkContent::Gap(_) => num_gaps += 1,
                }
            }

            // The gap must have been stored.
            assert_eq!(num_gaps, 1);
            assert_eq!(num_events, 1);
        }

        // Now, propagate an update for another message, but the timeline isn't limited
        // this time.
        room_event_cache
            .inner
            .handle_joined_room_update(
                has_storage,
                JoinedRoomUpdate {
                    timeline: Timeline {
                        limited: false,
                        prev_batch: Some("fondue".to_owned()),
                        events: vec![f.text_msg("sup").into_event()],
                    },
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        {
            let state = room_event_cache.inner.state.read().await;

            let mut num_gaps = 0;
            let mut num_events = 0;

            for c in state.events().chunks() {
                match c.content() {
                    ChunkContent::Items(items) => num_events += items.len(),
                    ChunkContent::Gap(gap) => {
                        assert_eq!(gap.prev_token, "raclette");
                        num_gaps += 1;
                    }
                }
            }

            // There's only the previous gap, no new ones.
            assert_eq!(num_gaps, 1);
            assert_eq!(num_events, 2);
        }
    }

    async fn assert_relations(
        room_id: &RoomId,
        original_event: TimelineEvent,
        related_event: TimelineEvent,
        event_factory: EventFactory,
    ) {
        let client = logged_in_client(None).await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Save the original event.
        let original_event_id = original_event.event_id().unwrap();
        room_event_cache.save_event(original_event).await;

        // Save an unrelated event to check it's not in the related events list.
        let unrelated_id = event_id!("$2");
        room_event_cache
            .save_event(event_factory.text_msg("An unrelated event").event_id(unrelated_id).into())
            .await;

        // Save the related event.
        let related_id = related_event.event_id().unwrap();
        room_event_cache.save_event(related_event).await;

        let (event, related_events) =
            room_event_cache.event_with_relations(&original_event_id, None).await.unwrap();
        // Fetched event is the right one.
        let cached_event_id = event.event_id().unwrap();
        assert_eq!(cached_event_id, original_event_id);

        // There is only the actually related event in the related ones
        assert_eq!(related_events.len(), 1);
        let related_event_id = related_events[0].event_id().unwrap();
        assert_eq!(related_event_id, related_id);
    }
}
