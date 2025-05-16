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

use std::{
    collections::BTreeMap,
    fmt,
    ops::{Deref, DerefMut},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use events::{sort_positions_descending, Gap};
use eyeball::SharedObservable;
use eyeball_im::VectorDiff;
use matrix_sdk_base::{
    deserialized_responses::{AmbiguityChange, TimelineEvent},
    linked_chunk::Position,
    sync::{JoinedRoomUpdate, LeftRoomUpdate, Timeline},
};
use ruma::{
    events::{relation::RelationType, AnyRoomAccountDataEvent, AnySyncEphemeralRoomEvent},
    serde::Raw,
    EventId, OwnedEventId, OwnedRoomId,
};
use tokio::sync::{
    broadcast::{Receiver, Sender},
    mpsc, Notify, RwLock,
};
use tracing::{instrument, trace, warn};

use super::{
    deduplicator::DeduplicationOutcome, AutoShrinkChannelPayload, EventsOrigin, Result,
    RoomEventCacheUpdate, RoomPagination, RoomPaginationStatus,
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

/// Thin wrapper for a room event cache listener, so as to trigger side-effects
/// when all listeners are gone.
#[allow(missing_debug_implementations)]
pub struct RoomEventCacheListener {
    /// Underlying receiver of the room event cache's updates.
    recv: Receiver<RoomEventCacheUpdate>,

    /// To which room are we listening?
    room_id: OwnedRoomId,

    /// Sender to the auto-shrink channel.
    auto_shrink_sender: mpsc::Sender<AutoShrinkChannelPayload>,

    /// Shared instance of the auto-shrinker.
    listener_count: Arc<AtomicUsize>,
}

impl Drop for RoomEventCacheListener {
    fn drop(&mut self) {
        let previous_listener_count = self.listener_count.fetch_sub(1, Ordering::SeqCst);

        trace!("dropping a room event cache listener; previous count: {previous_listener_count}");

        if previous_listener_count == 1 {
            // We were the last instance of the listener; let the auto-shrinker know by
            // notifying it of our room id.

            let mut room_id = self.room_id.clone();

            // Try to send without waiting for channel capacity, and restart in a spin-loop
            // if it failed (until a maximum number of attempts is reached, or
            // the send was successful). The channel shouldn't be super busy in
            // general, so this should resolve quickly enough.

            let mut num_attempts = 0;

            while let Err(err) = self.auto_shrink_sender.try_send(room_id) {
                num_attempts += 1;

                if num_attempts > 1024 {
                    // If we've tried too many times, just give up with a warning; after all, this
                    // is only an optimization.
                    warn!("couldn't send notification to the auto-shrink channel after 1024 attempts; giving up");
                    return;
                }

                match err {
                    mpsc::error::TrySendError::Full(stolen_room_id) => {
                        room_id = stolen_room_id;
                    }
                    mpsc::error::TrySendError::Closed(_) => return,
                }
            }

            trace!("sent notification to the parent channel that we were the last listener");
        }
    }
}

impl Deref for RoomEventCacheListener {
    type Target = Receiver<RoomEventCacheUpdate>;

    fn deref(&self) -> &Self::Target {
        &self.recv
    }
}

impl DerefMut for RoomEventCacheListener {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.recv
    }
}

impl RoomEventCache {
    /// Create a new [`RoomEventCache`] using the given room and store.
    pub(super) fn new(
        client: WeakClient,
        state: RoomEventCacheState,
        pagination_status: SharedObservable<RoomPaginationStatus>,
        room_id: OwnedRoomId,
        auto_shrink_sender: mpsc::Sender<AutoShrinkChannelPayload>,
    ) -> Self {
        Self {
            inner: Arc::new(RoomEventCacheInner::new(
                client,
                state,
                pagination_status,
                room_id,
                auto_shrink_sender,
            )),
        }
    }

    /// Read all current events.
    ///
    /// Use [`RoomEventCache::subscribe`] to get all current events, plus a
    /// listener/subscriber.
    pub async fn events(&self) -> Vec<TimelineEvent> {
        let state = self.inner.state.read().await;

        state.events().events().map(|(_position, item)| item.clone()).collect()
    }

    /// Subscribe to this room updates, after getting the initial list of
    /// events.
    ///
    /// Use [`RoomEventCache::events`] to get all current events without the
    /// listener/subscriber. Creating, and especially dropping, a
    /// [`RoomEventCacheListener`] isn't free.
    pub async fn subscribe(&self) -> (Vec<TimelineEvent>, RoomEventCacheListener) {
        let state = self.inner.state.read().await;
        let events = state.events().events().map(|(_position, item)| item.clone()).collect();

        let previous_listener_count = state.listener_count.fetch_add(1, Ordering::SeqCst);
        trace!("added a room event cache listener; new count: {}", previous_listener_count + 1);

        let recv = self.inner.sender.subscribe();
        let listener = RoomEventCacheListener {
            recv,
            room_id: self.inner.room_id.clone(),
            auto_shrink_sender: self.inner.auto_shrink_sender.clone(),
            listener_count: state.listener_count.clone(),
        };

        (events, listener)
    }

    /// Return a [`RoomPagination`] API object useful for running
    /// back-pagination queries in the current room.
    pub fn pagination(&self) -> RoomPagination {
        RoomPagination { inner: self.inner.clone() }
    }

    /// Try to find an event by id in this room.
    pub async fn event(&self, event_id: &EventId) -> Option<TimelineEvent> {
        self.inner
            .state
            .read()
            .await
            .find_event(event_id)
            .await
            .ok()
            .flatten()
            .map(|(_loc, event)| event)
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
        // Search in all loaded or stored events.
        self.inner
            .state
            .read()
            .await
            .find_event_with_relations(event_id, filter.clone())
            .await
            .ok()
            .flatten()
    }

    /// Clear all the storage for this [`RoomEventCache`].
    ///
    /// This will get rid of all the events from the linked chunk and persisted
    /// storage.
    pub async fn clear(&self) -> Result<()> {
        // Clear the linked chunk and persisted storage.
        let updates_as_vector_diffs = self.inner.state.write().await.reset().await?;

        // Notify observers about the update.
        let _ = self.inner.sender.send(RoomEventCacheUpdate::UpdateTimelineEvents {
            diffs: updates_as_vector_diffs,
            origin: EventsOrigin::Cache,
        });

        Ok(())
    }

    /// Save some events in the event cache, for further retrieval with
    /// [`Self::event`].
    pub(crate) async fn save_events(&self, events: impl IntoIterator<Item = TimelineEvent>) {
        if let Err(err) = self.inner.state.write().await.save_event(events).await {
            warn!("couldn't save event in the event cache: {err}");
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

    pub weak_room: WeakRoom,

    /// Sender part for subscribers to this room.
    pub sender: Sender<RoomEventCacheUpdate>,

    /// State for this room's event cache.
    pub state: RwLock<RoomEventCacheState>,

    /// A notifier that we received a new pagination token.
    pub pagination_batch_token_notifier: Notify,

    pub pagination_status: SharedObservable<RoomPaginationStatus>,

    /// Sender to the auto-shrink channel.
    ///
    /// See doc comment around [`EventCache::auto_shrink_linked_chunk_task`] for
    /// more details.
    auto_shrink_sender: mpsc::Sender<AutoShrinkChannelPayload>,
}

impl RoomEventCacheInner {
    /// Creates a new cache for a room, and subscribes to room updates, so as
    /// to handle new timeline events.
    fn new(
        client: WeakClient,
        state: RoomEventCacheState,
        pagination_status: SharedObservable<RoomPaginationStatus>,
        room_id: OwnedRoomId,
        auto_shrink_sender: mpsc::Sender<AutoShrinkChannelPayload>,
    ) -> Self {
        let sender = Sender::new(32);
        let weak_room = WeakRoom::new(client, room_id);
        Self {
            room_id: weak_room.room_id().to_owned(),
            weak_room,
            state: RwLock::new(state),
            sender,
            pagination_batch_token_notifier: Default::default(),
            auto_shrink_sender,
            pagination_status,
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

    #[instrument(skip_all, fields(room_id = %self.room_id))]
    pub(super) async fn handle_joined_room_update(&self, updates: JoinedRoomUpdate) -> Result<()> {
        self.handle_timeline(
            updates.timeline,
            updates.ephemeral.clone(),
            updates.ambiguity_changes,
        )
        .await?;

        self.handle_account_data(updates.account_data);

        Ok(())
    }

    #[instrument(skip_all, fields(room_id = %self.room_id))]
    pub(super) async fn handle_left_room_update(&self, updates: LeftRoomUpdate) -> Result<()> {
        self.handle_timeline(updates.timeline, Vec::new(), updates.ambiguity_changes).await?;
        Ok(())
    }

    async fn handle_timeline(
        &self,
        timeline: Timeline,
        ephemeral_events: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    ) -> Result<()> {
        let mut prev_batch = timeline.prev_batch;
        if timeline.events.is_empty()
            && prev_batch.is_none()
            && ephemeral_events.is_empty()
            && ambiguity_changes.is_empty()
        {
            return Ok(());
        }

        // Add all the events to the backend.
        trace!("adding new events");

        let mut state = self.state.write().await;

        // Ditch the previous-batch token if the sync isn't limited and we've seen at
        // least one event in the past.
        //
        // In this case (and only this one), we should definitely know what the head of
        // the timeline is (either we know about all the events, or we have a
        // gap somewhere), since storage is enabled by default.
        if !timeline.limited && state.events().events().next().is_some() {
            prev_batch = None;
        }

        let (
            DeduplicationOutcome {
                all_events: events,
                in_memory_duplicated_event_ids,
                in_store_duplicated_event_ids,
            },
            all_duplicates,
        ) = state.collect_valid_and_duplicated_events(timeline.events).await?;

        // During a sync, when a duplicated event is found, the old event is removed and
        // the new event is added.
        //
        // Let's remove the old events that are duplicated.
        let timeline_event_diffs = if all_duplicates {
            // No new events, thus no need to change the room events.
            vec![]
        } else {
            // Remove the old duplicated events.
            //
            // We don't have to worry the removals can change the position of the
            // existing events, because we are pushing all _new_
            // `events` at the back.
            let mut timeline_event_diffs = state
                .remove_events(in_memory_duplicated_event_ids, in_store_duplicated_event_ids)
                .await?;

            // Add the previous back-pagination token (if present), followed by the timeline
            // events themselves.
            let new_timeline_event_diffs = state
                .with_events_mut(|room_events| {
                    // If we only received duplicated events, we don't need to store the gap: if
                    // there was a gap, we'd have received an unknown event at the tail of
                    // the room's timeline (unless the server reordered sync events since the last
                    // time we sync'd).
                    if !all_duplicates {
                        if let Some(prev_token) = &prev_batch {
                            // As a tiny optimization: remove the last chunk if it's an empty event
                            // one, as it's not useful to keep it before a gap.
                            let prev_chunk_to_remove =
                                room_events.rchunks().next().and_then(|chunk| {
                                    (chunk.is_items() && chunk.num_items() == 0)
                                        .then_some(chunk.identifier())
                                });

                            room_events.push_gap(Gap { prev_token: prev_token.clone() });

                            if let Some(prev_chunk_to_remove) = prev_chunk_to_remove {
                                room_events.remove_empty_chunk_at(prev_chunk_to_remove).expect(
                                    "we just checked the chunk is there, and it's an empty item chunk",
                                );
                            }
                        }
                    }

                    room_events.push_events(events.clone());

                    events.clone()
                })
                .await?;

            timeline_event_diffs.extend(new_timeline_event_diffs);

            if timeline.limited && prev_batch.is_some() && !all_duplicates {
                // If there was a previous batch token for a limited timeline, and there's at
                // least one non-duplicated new event, unload the chunks so it
                // only contains the last one; otherwise, there might be a valid
                // gap in between, and observers may not render it (yet).
                //
                // We must do this *after* the above call to `.with_events_mut`, so the new
                // events and gaps are properly persisted to storage.
                if let Some(diffs) = state.shrink_to_last_chunk().await? {
                    // Override the diffs with the new ones, as per `shrink_to_last_chunk`'s API
                    // contract.
                    timeline_event_diffs = diffs;
                }
            }

            timeline_event_diffs
        };

        // Now that all events have been added, we can trigger the
        // `pagination_token_notifier`.
        if prev_batch.is_some() {
            self.pagination_batch_token_notifier.notify_one();
        }

        // The order of `RoomEventCacheUpdate`s is **really** important here.
        {
            if !timeline_event_diffs.is_empty() {
                let _ = self.sender.send(RoomEventCacheUpdate::UpdateTimelineEvents {
                    diffs: timeline_event_diffs,
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
/// [`RoomEventCacheState::load_more_events_backwards`].
#[derive(Debug)]
pub(super) enum LoadMoreEventsBackwardsOutcome {
    /// A gap has been inserted.
    Gap {
        /// The previous batch token to be used as the "end" parameter in the
        /// back-pagination request.
        prev_token: Option<String>,
    },

    /// The start of the timeline has been reached.
    StartOfTimeline,

    /// Events have been inserted.
    Events {
        events: Vec<TimelineEvent>,
        timeline_event_diffs: Vec<VectorDiff<TimelineEvent>>,
        reached_start: bool,
    },

    /// The caller must wait for the initial previous-batch token, and retry.
    WaitForInitialPrevToken,
}

// Use a private module to hide `events` to this parent module.
mod private {
    use std::{
        collections::HashSet,
        sync::{atomic::AtomicUsize, Arc},
    };

    use eyeball::SharedObservable;
    use eyeball_im::VectorDiff;
    use matrix_sdk_base::{
        apply_redaction,
        deserialized_responses::{TimelineEvent, TimelineEventKind},
        event_cache::{store::EventCacheStoreLock, Event, Gap},
        linked_chunk::{lazy_loader, ChunkContent, ChunkIdentifierGenerator, Position, Update},
    };
    use matrix_sdk_common::executor::spawn;
    use ruma::{
        events::{
            relation::RelationType, room::redaction::SyncRoomRedactionEvent, AnySyncTimelineEvent,
            MessageLikeEventType,
        },
        serde::Raw,
        EventId, OwnedEventId, OwnedRoomId, RoomVersionId,
    };
    use tracing::{debug, error, instrument, trace, warn};

    use super::{
        super::{
            deduplicator::{DeduplicationOutcome, Deduplicator},
            EventCacheError,
        },
        events::RoomEvents,
        sort_positions_descending, EventLocation, LoadMoreEventsBackwardsOutcome,
    };
    use crate::event_cache::RoomPaginationStatus;

    /// State for a single room's event cache.
    ///
    /// This contains all the inner mutable states that ought to be updated at
    /// the same time.
    pub struct RoomEventCacheState {
        /// The room this state relates to.
        room: OwnedRoomId,

        /// The room version for this room.
        room_version: RoomVersionId,

        /// Reference to the underlying backing store.
        store: EventCacheStoreLock,

        /// The events of the room.
        events: RoomEvents,

        /// The events deduplicator instance to help finding duplicates.
        deduplicator: Deduplicator,

        /// Have we ever waited for a previous-batch-token to come from sync, in
        /// the context of pagination? We do this at most once per room,
        /// the first time we try to run backward pagination. We reset
        /// that upon clearing the timeline events.
        pub waited_for_initial_prev_token: bool,

        pagination_status: SharedObservable<RoomPaginationStatus>,

        /// An atomic count of the current number of listeners of the
        /// [`super::RoomEventCache`].
        pub(super) listener_count: Arc<AtomicUsize>,
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
            room_version: RoomVersionId,
            store: EventCacheStoreLock,
            pagination_status: SharedObservable<RoomPaginationStatus>,
        ) -> Result<Self, EventCacheError> {
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
                    store_lock.handle_linked_chunk_updates(&room_id, vec![Update::Clear]).await?;

                    // Restart with an empty linked chunk.
                    None
                }
            };

            let events = RoomEvents::with_initial_linked_chunk(linked_chunk);
            let deduplicator = Deduplicator::new(room_id.clone(), store.clone());

            Ok(Self {
                room: room_id,
                room_version,
                store,
                events,
                deduplicator,
                waited_for_initial_prev_token: false,
                listener_count: Default::default(),
                pagination_status,
            })
        }

        /// Deduplicate `events` considering all events in `Self::events`.
        ///
        /// The returned tuple contains:
        /// - all events (duplicated or not) with an ID
        /// - all the duplicated event IDs with their position,
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
        ) -> Result<(DeduplicationOutcome, bool), EventCacheError> {
            let deduplication_outcome =
                self.deduplicator.filter_duplicate_events(events, &self.events).await?;

            let number_of_events = deduplication_outcome.all_events.len();
            let number_of_deduplicated_events =
                deduplication_outcome.in_memory_duplicated_event_ids.len()
                    + deduplication_outcome.in_store_duplicated_event_ids.len();

            let all_duplicates =
                number_of_events > 0 && number_of_events == number_of_deduplicated_events;

            Ok((deduplication_outcome, all_duplicates))
        }

        /// Given a fully-loaded linked chunk with no gaps, return the
        /// [`LoadMoreEventsBackwardsOutcome`] expected for this room's cache.
        fn conclude_load_more_for_fully_loaded_chunk(&mut self) -> LoadMoreEventsBackwardsOutcome {
            // If we never received events for this room, this means we've never
            // received a sync for that room, because every room must have at least a
            // room creation event. Otherwise, we have reached the start of the
            // timeline.
            if self.events.events().next().is_some() {
                // If there's at least one event, this means we've reached the start of the
                // timeline, since the chunk is fully loaded.
                trace!("chunk is fully loaded and non-empty: reached_start=true");
                LoadMoreEventsBackwardsOutcome::StartOfTimeline
            } else if !self.waited_for_initial_prev_token {
                // There's no events. Since we haven't yet, wait for an initial previous-token.
                LoadMoreEventsBackwardsOutcome::WaitForInitialPrevToken
            } else {
                // Otherwise, we've already waited, *and* received no previous-batch token from
                // the sync, *and* there are still no events in the fully-loaded
                // chunk: start back-pagination from the end of the room.
                LoadMoreEventsBackwardsOutcome::Gap { prev_token: None }
            }
        }

        /// Load more events backwards if the last chunk is **not** a gap.
        pub(in super::super) async fn load_more_events_backwards(
            &mut self,
        ) -> Result<LoadMoreEventsBackwardsOutcome, EventCacheError> {
            // If any in-memory chunk is a gap, don't load more events, and let the caller
            // resolve the gap.
            if let Some(prev_token) = self.events.rgap().map(|gap| gap.prev_token) {
                return Ok(LoadMoreEventsBackwardsOutcome::Gap { prev_token: Some(prev_token) });
            }

            // Because `first_chunk` is `not `Send`, get this information before the
            // `.await` point, so that this `Future` can implement `Send`.
            let first_chunk_identifier =
                self.events.chunks().next().expect("a linked chunk is never empty").identifier();

            let store = self.store.lock().await?;

            // The first chunk is not a gap, we can load its previous chunk.
            let new_first_chunk =
                match store.load_previous_chunk(&self.room, first_chunk_identifier).await {
                    Ok(Some(new_first_chunk)) => {
                        // All good, let's continue with this chunk.
                        new_first_chunk
                    }

                    Ok(None) => {
                        // There's no previous chunk. The chunk is now fully-loaded. Conclude.
                        return Ok(self.conclude_load_more_for_fully_loaded_chunk());
                    }

                    Err(err) => {
                        error!("error when loading the previous chunk of a linked chunk: {err}");

                        // Clear storage for this room.
                        store.handle_linked_chunk_updates(&self.room, vec![Update::Clear]).await?;

                        // Return the error.
                        return Err(err.into());
                    }
                };

            let chunk_content = new_first_chunk.content.clone();

            // We've reached the start on disk, if and only if, there was no chunk prior to
            // the one we just loaded.
            //
            // This value is correct, if and only if, it is used for a chunk content of kind
            // `Items`.
            let reached_start = new_first_chunk.previous.is_none();

            if let Err(err) = self.events.insert_new_chunk_as_first(new_first_chunk) {
                error!("error when inserting the previous chunk into its linked chunk: {err}");

                // Clear storage for this room.
                store.handle_linked_chunk_updates(&self.room, vec![Update::Clear]).await?;

                // Return the error.
                return Err(err.into());
            };

            // ⚠️ Let's not propagate the updates to the store! We already have these data
            // in the store! Let's drain them.
            let _ = self.events.store_updates().take();

            // However, we want to get updates as `VectorDiff`s.
            let timeline_event_diffs = self.events.updates_as_vector_diffs();

            Ok(match chunk_content {
                ChunkContent::Gap(gap) => {
                    trace!("reloaded chunk from disk (gap)");
                    LoadMoreEventsBackwardsOutcome::Gap { prev_token: Some(gap.prev_token) }
                }

                ChunkContent::Items(events) => {
                    trace!(?reached_start, "reloaded chunk from disk ({} items)", events.len());
                    LoadMoreEventsBackwardsOutcome::Events {
                        events,
                        timeline_event_diffs,
                        reached_start,
                    }
                }
            })
        }

        /// If storage is enabled, unload all the chunks, then reloads only the
        /// last one.
        ///
        /// If storage's enabled, return a diff update that starts with a clear
        /// of all events; as a result, the caller may override any
        /// pending diff updates with the result of this function.
        ///
        /// Otherwise, returns `None`.
        #[must_use = "Updates as `VectorDiff` must probably be propagated via `RoomEventCacheUpdate`"]
        pub(super) async fn shrink_to_last_chunk(
            &mut self,
        ) -> Result<Option<Vec<VectorDiff<TimelineEvent>>>, EventCacheError> {
            let store_lock = self.store.lock().await?;

            // Attempt to load the last chunk.
            let (last_chunk, chunk_identifier_generator) = match store_lock
                .load_last_chunk(&self.room)
                .await
            {
                Ok(pair) => pair,

                Err(err) => {
                    // If loading the last chunk failed, clear the entire linked chunk.
                    error!("error when reloading a linked chunk from memory: {err}");

                    // Clear storage for this room.
                    store_lock.handle_linked_chunk_updates(&self.room, vec![Update::Clear]).await?;

                    // Restart with an empty linked chunk.
                    (None, ChunkIdentifierGenerator::new_from_scratch())
                }
            };

            debug!("unloading the linked chunk, and resetting it to its last chunk");

            // Remove all the chunks from the linked chunks, except for the last one, and
            // updates the chunk identifier generator.
            if let Err(err) = self.events.replace_with(last_chunk, chunk_identifier_generator) {
                error!("error when replacing the linked chunk: {err}");
                return self.reset().await.map(Some);
            }

            // Let pagination observers know that we may have not reached the start of the
            // timeline.
            // TODO: likely need to cancel any ongoing pagination.
            self.pagination_status.set(RoomPaginationStatus::Idle { hit_timeline_start: false });

            // Don't propagate those updates to the store; this is only for the in-memory
            // representation that we're doing this. Let's drain those store updates.
            let _ = self.events.store_updates().take();

            // However, we want to get updates as `VectorDiff`s, for the external listeners.
            // Check we're respecting the contract defined in the doc comment.
            let diffs = self.events.updates_as_vector_diffs();
            assert!(matches!(diffs[0], VectorDiff::Clear));

            Ok(Some(diffs))
        }

        /// Automatically shrink the room if there are no listeners, as
        /// indicated by the atomic number of active listeners.
        #[must_use = "Updates as `VectorDiff` must probably be propagated via `RoomEventCacheUpdate`"]
        pub(crate) async fn auto_shrink_if_no_listeners(
            &mut self,
        ) -> Result<Option<Vec<VectorDiff<TimelineEvent>>>, EventCacheError> {
            let listener_count = self.listener_count.load(std::sync::atomic::Ordering::SeqCst);

            trace!(listener_count, "received request to auto-shrink");

            if listener_count == 0 {
                // If we are the last strong reference to the auto-shrinker, we can shrink the
                // events data structure to its last chunk.
                self.shrink_to_last_chunk().await
            } else {
                Ok(None)
            }
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

        /// Remove events by their position, in `RoomEvents` and in
        /// `EventCacheStore`.
        ///
        /// This method is purposely isolated because it must ensure that
        /// positions are sorted appropriately or it can be disastrous.
        #[must_use = "Updates as `VectorDiff` must probably be propagated via `RoomEventCacheUpdate`"]
        #[instrument(skip_all)]
        pub(crate) async fn remove_events(
            &mut self,
            in_memory_events: Vec<(OwnedEventId, Position)>,
            in_store_events: Vec<(OwnedEventId, Position)>,
        ) -> Result<Vec<VectorDiff<TimelineEvent>>, EventCacheError> {
            // In-store events.
            if !in_store_events.is_empty() {
                let mut positions = in_store_events
                    .into_iter()
                    .map(|(_event_id, position)| position)
                    .collect::<Vec<_>>();

                sort_positions_descending(&mut positions);

                self.send_updates_to_store(
                    positions
                        .into_iter()
                        .map(|position| Update::RemoveItem { at: position })
                        .collect(),
                )
                .await?;
            }

            // In-memory events.
            let timeline_event_diffs = if !in_memory_events.is_empty() {
                self.with_events_mut(|room_events| {
                    // `remove_events_by_position` sorts the positions by itself.
                    room_events
                        .remove_events_by_position(
                            in_memory_events
                                .into_iter()
                                .map(|(_event_id, position)| position)
                                .collect(),
                        )
                        .expect("failed to remove an event");

                    vec![]
                })
                .await?
            } else {
                Vec::new()
            };

            Ok(timeline_event_diffs)
        }

        /// Propagate changes to the underlying storage.
        async fn propagate_changes(&mut self) -> Result<(), EventCacheError> {
            let updates = self.events.store_updates().take();
            self.send_updates_to_store(updates).await
        }

        pub async fn send_updates_to_store(
            &mut self,
            mut updates: Vec<Update<TimelineEvent, Gap>>,
        ) -> Result<(), EventCacheError> {
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
            let room_id = self.room.clone();

            spawn(async move {
                let store = store.lock().await?;

                trace!(?updates, "sending linked chunk updates to the store");
                store.handle_linked_chunk_updates(&room_id, updates).await?;
                trace!("linked chunk updates applied");

                super::Result::Ok(())
            })
            .await
            .expect("joining failed")?;

            Ok(())
        }

        /// Reset this data structure as if it were brand new.
        ///
        /// Return a single diff update that is a clear of all events; as a
        /// result, the caller may override any pending diff updates
        /// with the result of this function.
        #[must_use = "Updates as `VectorDiff` must probably be propagated via `RoomEventCacheUpdate`"]
        pub async fn reset(&mut self) -> Result<Vec<VectorDiff<TimelineEvent>>, EventCacheError> {
            self.events.reset();

            self.propagate_changes().await?;

            // Reset the pagination state too: pretend we never waited for the initial
            // prev-batch token, and indicate that we're not at the start of the
            // timeline, since we don't know about that anymore.
            self.waited_for_initial_prev_token = false;
            // TODO: likely must cancel any ongoing back-paginations too
            self.pagination_status.set(RoomPaginationStatus::Idle { hit_timeline_start: false });

            let diff_updates = self.events.updates_as_vector_diffs();

            // Ensure the contract defined in the doc comment is true:
            debug_assert_eq!(diff_updates.len(), 1);
            debug_assert!(matches!(diff_updates[0], VectorDiff::Clear));

            Ok(diff_updates)
        }

        /// Returns a read-only reference to the underlying events.
        pub fn events(&self) -> &RoomEvents {
            &self.events
        }

        /// Find a single event in this room.
        ///
        /// It starts by looking into loaded events in `RoomEvents` before
        /// looking inside the storage if it is enabled.
        pub async fn find_event(
            &self,
            event_id: &EventId,
        ) -> Result<Option<(EventLocation, TimelineEvent)>, EventCacheError> {
            // There are supposedly fewer events loaded in memory than in the store. Let's
            // start by looking up in the `RoomEvents`.
            for (position, event) in self.events().revents() {
                if event.event_id().as_deref() == Some(event_id) {
                    return Ok(Some((EventLocation::Memory(position), event.clone())));
                }
            }

            let store = self.store.lock().await?;

            Ok(store
                .find_event(&self.room, event_id)
                .await?
                .map(|event| (EventLocation::Store, event)))
        }

        /// Find an event and all its relations in the persisted storage.
        ///
        /// This goes straight to the database, as a simplification; we don't
        /// expect to need to have to look up in memory events, or that
        /// all the related events are actually loaded.
        pub async fn find_event_with_relations(
            &self,
            event_id: &EventId,
            filters: Option<Vec<RelationType>>,
        ) -> Result<Option<(TimelineEvent, Vec<TimelineEvent>)>, EventCacheError> {
            let store = self.store.lock().await?;

            // First, hit storage to get the target event and its related events.
            let found = store.find_event(&self.room, event_id).await?;

            let Some(target) = found else {
                // We haven't found the event: return early.
                return Ok(None);
            };

            // Then, initialize the stack with all the related events, to find the
            // transitive closure of all the related events.
            let mut related =
                store.find_event_relations(&self.room, event_id, filters.as_deref()).await?;
            let mut stack = related.iter().filter_map(|event| event.event_id()).collect::<Vec<_>>();

            // Also keep track of already seen events, in case there's a loop in the
            // relation graph.
            let mut already_seen = HashSet::new();
            already_seen.insert(event_id.to_owned());

            let mut num_iters = 1;

            // Find the related event for each previously-related event.
            while let Some(event_id) = stack.pop() {
                if !already_seen.insert(event_id.clone()) {
                    // Skip events we've already seen.
                    continue;
                }

                let other_related =
                    store.find_event_relations(&self.room, &event_id, filters.as_deref()).await?;

                stack.extend(other_related.iter().filter_map(|event| event.event_id()));
                related.extend(other_related);

                num_iters += 1;
            }

            trace!(num_related = %related.len(), num_iters, "computed transitive closure of related events");

            Ok(Some((target, related)))
        }

        /// Gives a temporary mutable handle to the underlying in-memory events,
        /// and will propagate changes to the storage once done.
        ///
        /// Returns the updates to the linked chunk, as vector diffs, so the
        /// caller may propagate such updates, if needs be.
        ///
        /// The function `func` takes a mutable reference to `RoomEvents`. It
        /// returns a set of events that will be post-processed. At the time of
        /// writing, all these events are passed to
        /// `Self::maybe_apply_new_redaction`.
        #[must_use = "Updates as `VectorDiff` must probably be propagated via `RoomEventCacheUpdate`"]
        #[instrument(skip_all, fields(room_id = %self.room))]
        pub async fn with_events_mut<F>(
            &mut self,
            func: F,
        ) -> Result<Vec<VectorDiff<TimelineEvent>>, EventCacheError>
        where
            F: FnOnce(&mut RoomEvents) -> Vec<TimelineEvent>,
        {
            let events_to_post_process = func(&mut self.events);

            // Update the store before doing the post-processing.
            self.propagate_changes().await?;

            for event in &events_to_post_process {
                self.maybe_apply_new_redaction(event).await?;
            }

            // If we've never waited for an initial previous-batch token, and we now have at
            // least one gap in the chunk, no need to wait for a previous-batch token later.
            if !self.waited_for_initial_prev_token
                && self.events.chunks().any(|chunk| chunk.is_gap())
            {
                self.waited_for_initial_prev_token = true;
            }

            let updates_as_vector_diffs = self.events.updates_as_vector_diffs();

            Ok(updates_as_vector_diffs)
        }

        /// If the given event is a redaction, try to retrieve the
        /// to-be-redacted event in the chunk, and replace it by the
        /// redacted form.
        #[instrument(skip_all)]
        async fn maybe_apply_new_redaction(
            &mut self,
            event: &Event,
        ) -> Result<(), EventCacheError> {
            let raw_event = event.raw();

            // Do not deserialise the entire event if we aren't certain it's a
            // `m.room.redaction`. It saves a non-negligible amount of computations.
            let Ok(Some(MessageLikeEventType::RoomRedaction)) =
                raw_event.get_field::<MessageLikeEventType>("type")
            else {
                return Ok(());
            };

            // It is a `m.room.redaction`! We can deserialize it entirely.

            let Ok(AnySyncTimelineEvent::MessageLike(
                ruma::events::AnySyncMessageLikeEvent::RoomRedaction(redaction),
            )) = event.raw().deserialize()
            else {
                return Ok(());
            };

            let Some(event_id) = redaction.redacts(&self.room_version) else {
                warn!("missing target event id from the redaction event");
                return Ok(());
            };

            // Replace the redacted event by a redacted form, if we knew about it.
            if let Some((location, target_event)) = self.find_event(event_id).await? {
                // Don't redact already redacted events.
                if let Ok(deserialized) = target_event.raw().deserialize() {
                    match deserialized {
                        AnySyncTimelineEvent::MessageLike(ev) => {
                            if ev.is_redacted() {
                                return Ok(());
                            }
                        }
                        AnySyncTimelineEvent::State(ev) => {
                            if ev.is_redacted() {
                                return Ok(());
                            }
                        }
                    }
                }

                if let Some(redacted_event) = apply_redaction(
                    target_event.raw(),
                    event.raw().cast_ref::<SyncRoomRedactionEvent>(),
                    &self.room_version,
                ) {
                    let mut copy = target_event.clone();

                    // It's safe to cast `redacted_event` here:
                    // - either the event was an `AnyTimelineEvent` cast to `AnySyncTimelineEvent`
                    //   when calling .raw(), so it's still one under the hood.
                    // - or it wasn't, and it's a plain `AnySyncTimelineEvent` in this case.
                    copy.replace_raw(redacted_event.cast());

                    match location {
                        EventLocation::Memory(position) => {
                            self.events
                                .replace_event_at(position, copy)
                                .expect("should have been a valid position of an item");
                        }
                        EventLocation::Store => self.save_event([copy]).await?,
                    }
                }
            } else {
                trace!("redacted event is missing from the linked chunk");
            }

            // TODO: remove all related events too!

            Ok(())
        }

        /// Save a single event into the database, without notifying observers.
        ///
        /// Note: if the event was already saved as part of a linked chunk, and
        /// its event id may have changed, it's not safe to use this
        /// method because it may break the link between the chunk and
        /// the event. Instead, an update to the linked chunk must be used.
        pub async fn save_event(
            &self,
            events: impl IntoIterator<Item = TimelineEvent>,
        ) -> Result<(), EventCacheError> {
            let store = self.store.clone();
            let room_id = self.room.clone();
            let events = events.into_iter().collect::<Vec<_>>();

            // Spawn a task so the save is uninterrupted by task cancellation.
            spawn(async move {
                let store = store.lock().await?;
                for event in events {
                    store.save_event(&room_id, event).await?;
                }
                super::Result::Ok(())
            })
            .await
            .expect("joining failed")?;

            Ok(())
        }
    }
}

/// An enum representing where an event has been found.
pub(super) enum EventLocation {
    /// Event lives in memory (and likely in the store!).
    Memory(Position),

    /// Event lives in the store only, it has not been loaded in memory yet.
    Store,
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
            event_factory.reaction(related_id, "🤡").event_id(associated_related_id).into();

        let client = logged_in_client(None).await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Save the original event.
        room_event_cache.save_events([original_event]).await;

        // Save the related event.
        room_event_cache.save_events([related_event]).await;

        // Save the associated related event, which redacts the related event.
        room_event_cache.save_events([associated_related_event]).await;

        let filter = Some(vec![RelationType::Replacement]);
        let (event, related_events) =
            room_event_cache.event_with_relations(original_id, filter).await.unwrap();
        // Fetched event is the right one.
        let cached_event_id = event.event_id().unwrap();
        assert_eq!(cached_event_id, original_id);

        // There's only the edit event (an edit event can't have its own edit event).
        assert_eq!(related_events.len(), 1);

        let related_event_id = related_events[0].event_id().unwrap();
        assert_eq!(related_event_id, related_id);

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
            event_factory.reaction(related_id, "👍").event_id(associated_related_id).into();

        let client = logged_in_client(None).await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Save the original event.
        room_event_cache.save_events([original_event]).await;

        // Save the related event.
        room_event_cache.save_events([related_event]).await;

        // Save the associated related event, which redacts the related event.
        room_event_cache.save_events([associated_related_event]).await;

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
            .handle_joined_room_update(JoinedRoomUpdate { timeline, ..Default::default() })
            .await
            .unwrap();

        let linked_chunk =
            from_all_chunks::<3, _, _>(event_cache_store.load_all_chunks(room_id).await.unwrap())
                .unwrap()
                .unwrap();

        assert_eq!(linked_chunk.chunks().count(), 2);

        let mut chunks = linked_chunk.chunks();

        // We start with the gap.
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
            .handle_joined_room_update(JoinedRoomUpdate { timeline, ..Default::default() })
            .await
            .unwrap();

        // The in-memory linked chunk keeps the bundled relation.
        {
            let events = room_event_cache.events().await;

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

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        let (items, mut stream) = room_event_cache.subscribe().await;

        // The rooms knows about all cached events.
        {
            assert!(room_event_cache.event(event_id1).await.is_some());
            assert!(room_event_cache.event(event_id2).await.is_some());
        }

        // But only part of events are loaded from the store
        {
            // The room must contain only one event because only one chunk has been loaded.
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].event_id().unwrap(), event_id2);

            assert!(stream.is_empty());
        }

        // Let's load more chunks to load all events.
        {
            room_event_cache.pagination().run_backwards_once(20).await.unwrap();

            assert_let_timeout!(
                Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = stream.recv()
            );
            assert_eq!(diffs.len(), 1);
            assert_matches!(&diffs[0], VectorDiff::Insert { index: 0, value: event } => {
                // Here you are `event_id1`!
                assert_eq!(event.event_id().unwrap(), event_id1);
            });

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

        // Events individually are not forgotten by the event cache, after clearing a
        // room.
        assert!(room_event_cache.event(event_id1).await.is_some());

        // But their presence in a linked chunk is forgotten.
        let items = room_event_cache.events().await;
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

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        let (items, mut stream) = room_event_cache.subscribe().await;

        // The initial items contain one event because only the last chunk is loaded by
        // default.
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].event_id().unwrap(), event_id2);
        assert!(stream.is_empty());

        // The event cache knows only all events though, even if they aren't loaded.
        assert!(room_event_cache.event(event_id1).await.is_some());
        assert!(room_event_cache.event(event_id2).await.is_some());

        // Let's paginate to load more events.
        room_event_cache.pagination().run_backwards_once(20).await.unwrap();

        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = stream.recv()
        );
        assert_eq!(diffs.len(), 1);
        assert_matches!(&diffs[0], VectorDiff::Insert { index: 0, value: event } => {
            assert_eq!(event.event_id().unwrap(), event_id1);
        });

        assert!(stream.is_empty());

        // A new update with one of these events leads to deduplication.
        let timeline = Timeline { limited: false, prev_batch: None, events: vec![ev2] };
        room_event_cache
            .inner
            .handle_joined_room_update(JoinedRoomUpdate { timeline, ..Default::default() })
            .await
            .unwrap();

        // The stream doesn't report these changes *yet*. Use the items vector given
        // when subscribing, to check that the items correspond to their new
        // positions. The duplicated item is removed (so it's not the first
        // element anymore), and it's added to the back of the list.
        let items = room_event_cache.events().await;
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

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        let items = room_event_cache.events().await;

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
        use crate::event_cache::room::LoadMoreEventsBackwardsOutcome;

        let room_id = room_id!("!galette:saucisse.bzh");

        let client = MockClientBuilder::new("http://localhost".to_owned()).build().await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();
        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        let f = EventFactory::new().room(room_id).sender(*ALICE);

        // Propagate an update including a limited timeline with one message and a
        // prev-batch token.
        room_event_cache
            .inner
            .handle_joined_room_update(JoinedRoomUpdate {
                timeline: Timeline {
                    limited: true,
                    prev_batch: Some("raclette".to_owned()),
                    events: vec![f.text_msg("hey yo").into_event()],
                },
                ..Default::default()
            })
            .await
            .unwrap();

        {
            let mut state = room_event_cache.inner.state.write().await;

            let mut num_gaps = 0;
            let mut num_events = 0;

            for c in state.events().chunks() {
                match c.content() {
                    ChunkContent::Items(items) => num_events += items.len(),
                    ChunkContent::Gap(_) => num_gaps += 1,
                }
            }

            // The limited sync unloads the chunk, so it will appear as if there are only
            // the events.
            assert_eq!(num_gaps, 0);
            assert_eq!(num_events, 1);

            // But if I manually reload more of the chunk, the gap will be present.
            assert_matches!(
                state.load_more_events_backwards().await.unwrap(),
                LoadMoreEventsBackwardsOutcome::Gap { .. }
            );

            num_gaps = 0;
            num_events = 0;
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
            .handle_joined_room_update(JoinedRoomUpdate {
                timeline: Timeline {
                    limited: false,
                    prev_batch: Some("fondue".to_owned()),
                    events: vec![f.text_msg("sup").into_event()],
                },
                ..Default::default()
            })
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
        room_event_cache.save_events([original_event]).await;

        // Save an unrelated event to check it's not in the related events list.
        let unrelated_id = event_id!("$2");
        room_event_cache
            .save_events([event_factory
                .text_msg("An unrelated event")
                .event_id(unrelated_id)
                .into()])
            .await;

        // Save the related event.
        let related_id = related_event.event_id().unwrap();
        room_event_cache.save_events([related_event]).await;

        let (event, related_events) =
            room_event_cache.event_with_relations(&original_event_id, None).await.unwrap();
        // Fetched event is the right one.
        let cached_event_id = event.event_id().unwrap();
        assert_eq!(cached_event_id, original_event_id);

        // There is only the actually related event in the related ones
        let related_event_id = related_events[0].event_id().unwrap();
        assert_eq!(related_event_id, related_id);
    }

    #[cfg(not(target_arch = "wasm32"))] // This uses the cross-process lock, so needs time support.
    #[async_test]
    async fn test_shrink_to_last_chunk() {
        use eyeball_im::VectorDiff;

        use crate::{assert_let_timeout, event_cache::RoomEventCacheUpdate};

        let room_id = room_id!("!galette:saucisse.bzh");

        let client = MockClientBuilder::new("http://localhost".to_owned()).build().await;

        let f = EventFactory::new().room(room_id);

        let evid1 = event_id!("$1");
        let evid2 = event_id!("$2");

        let ev1 = f.text_msg("hello world").sender(*ALICE).event_id(evid1).into_event();
        let ev2 = f.text_msg("howdy").sender(*BOB).event_id(evid2).into_event();

        // Fill the event cache store with an initial linked chunk with 2 events chunks.
        {
            let store = client.event_cache_store();
            let store = store.lock().await.unwrap();
            store
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
                            items: vec![ev1],
                        },
                        Update::NewItemsChunk {
                            previous: Some(ChunkIdentifier::new(0)),
                            new: ChunkIdentifier::new(1),
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(1), 0),
                            items: vec![ev2],
                        },
                    ],
                )
                .await
                .unwrap();
        }

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();
        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Sanity check: lazily loaded, so only includes one item at start.
        let (events, mut stream) = room_event_cache.subscribe().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_id().as_deref(), Some(evid2));
        assert!(stream.is_empty());

        // Force loading the full linked chunk by back-paginating.
        let outcome = room_event_cache.pagination().run_backwards_once(20).await.unwrap();
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].event_id().as_deref(), Some(evid1));
        assert!(outcome.reached_start);

        // We also get an update about the loading from the store.
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = stream.recv()
        );
        assert_eq!(diffs.len(), 1);
        assert_matches!(&diffs[0], VectorDiff::Insert { index: 0, value } => {
            assert_eq!(value.event_id().as_deref(), Some(evid1));
        });

        assert!(stream.is_empty());

        // Shrink the linked chunk to the last chunk.
        let diffs = room_event_cache
            .inner
            .state
            .write()
            .await
            .shrink_to_last_chunk()
            .await
            .expect("shrinking should succeed")
            .unwrap();

        // We receive updates about the changes to the linked chunk.
        assert_eq!(diffs.len(), 2);
        assert_matches!(&diffs[0], VectorDiff::Clear);
        assert_matches!(&diffs[1], VectorDiff::Append { values} => {
            assert_eq!(values.len(), 1);
            assert_eq!(values[0].event_id().as_deref(), Some(evid2));
        });

        assert!(stream.is_empty());

        // When reading the events, we do get only the last one.
        let events = room_event_cache.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_id().as_deref(), Some(evid2));

        // But if we back-paginate, we don't need access to network to find out about
        // the previous event.
        let outcome = room_event_cache.pagination().run_backwards_once(20).await.unwrap();
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].event_id().as_deref(), Some(evid1));
        assert!(outcome.reached_start);
    }

    #[cfg(not(target_arch = "wasm32"))] // This uses the cross-process lock, so needs time support.
    #[async_test]
    async fn test_auto_shrink_after_all_subscribers_are_gone() {
        use eyeball_im::VectorDiff;
        use tokio::task::yield_now;

        use crate::{assert_let_timeout, event_cache::RoomEventCacheUpdate};

        let room_id = room_id!("!galette:saucisse.bzh");

        let client = MockClientBuilder::new("http://localhost".to_owned()).build().await;

        let f = EventFactory::new().room(room_id);

        let evid1 = event_id!("$1");
        let evid2 = event_id!("$2");

        let ev1 = f.text_msg("hello world").sender(*ALICE).event_id(evid1).into_event();
        let ev2 = f.text_msg("howdy").sender(*BOB).event_id(evid2).into_event();

        // Fill the event cache store with an initial linked chunk with 2 events chunks.
        {
            let store = client.event_cache_store();
            let store = store.lock().await.unwrap();
            store
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
                            items: vec![ev1],
                        },
                        Update::NewItemsChunk {
                            previous: Some(ChunkIdentifier::new(0)),
                            new: ChunkIdentifier::new(1),
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(1), 0),
                            items: vec![ev2],
                        },
                    ],
                )
                .await
                .unwrap();
        }

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();
        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Sanity check: lazily loaded, so only includes one item at start.
        let (events1, mut stream1) = room_event_cache.subscribe().await;
        assert_eq!(events1.len(), 1);
        assert_eq!(events1[0].event_id().as_deref(), Some(evid2));
        assert!(stream1.is_empty());

        // Force loading the full linked chunk by back-paginating.
        let outcome = room_event_cache.pagination().run_backwards_once(20).await.unwrap();
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].event_id().as_deref(), Some(evid1));
        assert!(outcome.reached_start);

        // We also get an update about the loading from the store. Ignore it, for this
        // test's sake.
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = stream1.recv()
        );
        assert_eq!(diffs.len(), 1);
        assert_matches!(&diffs[0], VectorDiff::Insert { index: 0, value } => {
            assert_eq!(value.event_id().as_deref(), Some(evid1));
        });

        assert!(stream1.is_empty());

        // Have another listener subscribe to the event cache.
        // Since it's not the first one, and the previous one loaded some more events,
        // the second listener seems them all.
        let (events2, stream2) = room_event_cache.subscribe().await;
        assert_eq!(events2.len(), 2);
        assert_eq!(events2[0].event_id().as_deref(), Some(evid1));
        assert_eq!(events2[1].event_id().as_deref(), Some(evid2));
        assert!(stream2.is_empty());

        // Drop the first stream, and wait a bit.
        drop(stream1);
        yield_now().await;

        // The second stream remains undisturbed.
        assert!(stream2.is_empty());

        // Now drop the second stream, and wait a bit.
        drop(stream2);
        yield_now().await;

        // The linked chunk must have auto-shrunk by now.

        {
            // Check the inner state: there's no more shared auto-shrinker.
            let state = room_event_cache.inner.state.read().await;
            assert_eq!(state.listener_count.load(std::sync::atomic::Ordering::SeqCst), 0);
        }

        // Getting the events will only give us the latest chunk.
        let events3 = room_event_cache.events().await;
        assert_eq!(events3.len(), 1);
        assert_eq!(events3[0].event_id().as_deref(), Some(evid2));
    }
}
