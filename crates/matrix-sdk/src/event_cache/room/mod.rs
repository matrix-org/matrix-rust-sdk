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
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use events::sort_positions_descending;
use eyeball::SharedObservable;
use eyeball_im::VectorDiff;
use matrix_sdk_base::{
    deserialized_responses::AmbiguityChange,
    event_cache::Event,
    linked_chunk::Position,
    sync::{JoinedRoomUpdate, LeftRoomUpdate, Timeline},
};
use ruma::{
    EventId, OwnedEventId, OwnedRoomId, RoomId,
    api::Direction,
    events::{AnyRoomAccountDataEvent, AnySyncEphemeralRoomEvent, relation::RelationType},
    serde::Raw,
};
use tokio::sync::{
    Notify,
    broadcast::{Receiver, Sender},
    mpsc,
};
use tracing::{instrument, trace, warn};

use super::{
    AutoShrinkChannelPayload, EventsOrigin, Result, RoomEventCacheGenericUpdate,
    RoomEventCacheUpdate, RoomPagination, RoomPaginationStatus,
};
use crate::{
    client::WeakClient,
    event_cache::EventCacheError,
    room::{IncludeRelations, RelationsOptions, WeakRoom},
};

pub(super) mod events;
mod threads;

pub use threads::ThreadEventCacheUpdate;

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

/// Thin wrapper for a room event cache subscriber, so as to trigger
/// side-effects when all subscribers are gone.
///
/// The current side-effect is: auto-shrinking the [`RoomEventCache`] when no
/// more subscribers are active. This is an optimisation to reduce the number of
/// data held in memory by a [`RoomEventCache`]: when no more subscribers are
/// active, all data are reduced to the minimum.
///
/// The side-effect takes effect on `Drop`.
#[allow(missing_debug_implementations)]
pub struct RoomEventCacheSubscriber {
    /// Underlying receiver of the room event cache's updates.
    recv: Receiver<RoomEventCacheUpdate>,

    /// To which room are we listening?
    room_id: OwnedRoomId,

    /// Sender to the auto-shrink channel.
    auto_shrink_sender: mpsc::Sender<AutoShrinkChannelPayload>,

    /// Shared instance of the auto-shrinker.
    subscriber_count: Arc<AtomicUsize>,
}

impl Drop for RoomEventCacheSubscriber {
    fn drop(&mut self) {
        let previous_subscriber_count = self.subscriber_count.fetch_sub(1, Ordering::SeqCst);

        trace!(
            "dropping a room event cache subscriber; previous count: {previous_subscriber_count}"
        );

        if previous_subscriber_count == 1 {
            // We were the last instance of the subscriber; let the auto-shrinker know by
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
                    warn!(
                        "couldn't send notification to the auto-shrink channel \
                         after 1024 attempts; giving up"
                    );
                    return;
                }

                match err {
                    mpsc::error::TrySendError::Full(stolen_room_id) => {
                        room_id = stolen_room_id;
                    }
                    mpsc::error::TrySendError::Closed(_) => return,
                }
            }

            trace!("sent notification to the parent channel that we were the last subscriber");
        }
    }
}

impl Deref for RoomEventCacheSubscriber {
    type Target = Receiver<RoomEventCacheUpdate>;

    fn deref(&self) -> &Self::Target {
        &self.recv
    }
}

impl DerefMut for RoomEventCacheSubscriber {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.recv
    }
}

impl RoomEventCache {
    /// Create a new [`RoomEventCache`] using the given room and store.
    pub(super) fn new(
        client: WeakClient,
        state: RoomEventCacheStateLock,
        pagination_status: SharedObservable<RoomPaginationStatus>,
        room_id: OwnedRoomId,
        auto_shrink_sender: mpsc::Sender<AutoShrinkChannelPayload>,
        update_sender: Sender<RoomEventCacheUpdate>,
        generic_update_sender: Sender<RoomEventCacheGenericUpdate>,
    ) -> Self {
        Self {
            inner: Arc::new(RoomEventCacheInner::new(
                client,
                state,
                pagination_status,
                room_id,
                auto_shrink_sender,
                update_sender,
                generic_update_sender,
            )),
        }
    }

    /// Get the room ID for this [`RoomEventCache`].
    pub fn room_id(&self) -> &RoomId {
        &self.inner.room_id
    }

    /// Read all current events.
    ///
    /// Use [`RoomEventCache::subscribe`] to get all current events, plus a
    /// subscriber.
    pub async fn events(&self) -> Result<Vec<Event>> {
        let state = self.inner.state.read().await?;

        Ok(state.room_linked_chunk().events().map(|(_position, item)| item.clone()).collect())
    }

    /// Subscribe to this room updates, after getting the initial list of
    /// events.
    ///
    /// Use [`RoomEventCache::events`] to get all current events without the
    /// subscriber. Creating, and especially dropping, a
    /// [`RoomEventCacheSubscriber`] isn't free, as it triggers side-effects.
    pub async fn subscribe(&self) -> Result<(Vec<Event>, RoomEventCacheSubscriber)> {
        let state = self.inner.state.read().await?;
        let events =
            state.room_linked_chunk().events().map(|(_position, item)| item.clone()).collect();

        let subscriber_count = state.subscriber_count();
        let previous_subscriber_count = subscriber_count.fetch_add(1, Ordering::SeqCst);
        trace!("added a room event cache subscriber; new count: {}", previous_subscriber_count + 1);

        let recv = self.inner.update_sender.subscribe();
        let subscriber = RoomEventCacheSubscriber {
            recv,
            room_id: self.inner.room_id.clone(),
            auto_shrink_sender: self.inner.auto_shrink_sender.clone(),
            subscriber_count: subscriber_count.clone(),
        };

        Ok((events, subscriber))
    }

    /// Subscribe to thread for a given root event, and get a (maybe empty)
    /// initially known list of events for that thread.
    pub async fn subscribe_to_thread(
        &self,
        thread_root: OwnedEventId,
    ) -> Result<(Vec<Event>, Receiver<ThreadEventCacheUpdate>)> {
        let mut state = self.inner.state.write().await?;
        Ok(state.subscribe_to_thread(thread_root))
    }

    /// Paginate backwards in a thread, given its root event ID.
    ///
    /// Returns whether we've hit the start of the thread, in which case the
    /// root event will be prepended to the thread.
    #[instrument(skip(self), fields(room_id = %self.inner.room_id))]
    pub async fn paginate_thread_backwards(
        &self,
        thread_root: OwnedEventId,
        num_events: u16,
    ) -> Result<bool> {
        let room = self.inner.weak_room.get().ok_or(EventCacheError::ClientDropped)?;

        // Take the lock only for a short time here.
        let mut outcome =
            self.inner.state.write().await?.load_more_thread_events_backwards(thread_root.clone());

        loop {
            match outcome {
                LoadMoreEventsBackwardsOutcome::Gap { prev_token } => {
                    // Start a threaded pagination from this gap.
                    let options = RelationsOptions {
                        from: prev_token.clone(),
                        dir: Direction::Backward,
                        limit: Some(num_events.into()),
                        include_relations: IncludeRelations::AllRelations,
                        recurse: true,
                    };

                    let mut result = room
                        .relations(thread_root.clone(), options)
                        .await
                        .map_err(|err| EventCacheError::BackpaginationError(Box::new(err)))?;

                    let reached_start = result.next_batch_token.is_none();
                    trace!(num_events = result.chunk.len(), %reached_start, "received a /relations response");

                    // Because the state lock is taken again in `load_or_fetch_event`, we need
                    // to do this *before* we take the state lock again.
                    let root_event =
                        if reached_start {
                            // Prepend the thread root event to the results.
                            Some(room.load_or_fetch_event(&thread_root, None).await.map_err(
                                |err| EventCacheError::BackpaginationError(Box::new(err)),
                            )?)
                        } else {
                            None
                        };

                    let mut state = self.inner.state.write().await?;

                    // Save all the events (but the thread root) in the store.
                    state.save_events(result.chunk.iter().cloned()).await?;

                    // Note: the events are still in the reversed order at this point, so
                    // pushing will eventually make it so that the root event is the first.
                    result.chunk.extend(root_event);

                    if let Some(outcome) = state.finish_thread_network_pagination(
                        thread_root.clone(),
                        prev_token,
                        result.next_batch_token,
                        result.chunk,
                    ) {
                        return Ok(outcome.reached_start);
                    }

                    // fallthrough: restart the pagination.
                    outcome = state.load_more_thread_events_backwards(thread_root.clone());
                }

                LoadMoreEventsBackwardsOutcome::StartOfTimeline => {
                    // We're done!
                    return Ok(true);
                }

                LoadMoreEventsBackwardsOutcome::Events { .. } => {
                    // TODO: implement :)
                    unimplemented!("loading from disk for threads is not implemented yet");
                }
            }
        }
    }

    /// Return a [`RoomPagination`] API object useful for running
    /// back-pagination queries in the current room.
    pub fn pagination(&self) -> RoomPagination {
        RoomPagination { inner: self.inner.clone() }
    }

    /// Try to find a single event in this room, starting from the most recent
    /// event.
    ///
    /// The `predicate` receives two arguments: the current event, and the
    /// ID of the _previous_ (older) event.
    ///
    /// **Warning**! It looks into the loaded events from the in-memory linked
    /// chunk **only**. It doesn't look inside the storage.
    pub async fn rfind_map_event_in_memory_by<O, P>(&self, predicate: P) -> Result<Option<O>>
    where
        P: FnMut(&Event, Option<&Event>) -> Option<O>,
    {
        Ok(self.inner.state.read().await?.rfind_map_event_in_memory_by(predicate))
    }

    /// Try to find an event by ID in this room.
    ///
    /// It starts by looking into loaded events before looking inside the
    /// storage.
    pub async fn find_event(&self, event_id: &EventId) -> Result<Option<Event>> {
        Ok(self
            .inner
            .state
            .read()
            .await?
            .find_event(event_id)
            .await
            .ok()
            .flatten()
            .map(|(_loc, event)| event))
    }

    /// Try to find an event by ID in this room, along with its related events.
    ///
    /// You can filter which types of related events to retrieve using
    /// `filter`. `None` will retrieve related events of any type.
    ///
    /// The related events are sorted like this:
    ///
    /// - events saved out-of-band (with `RoomEventCache::save_events`) will be
    ///   located at the beginning of the array.
    /// - events present in the linked chunk (be it in memory or in the storage)
    ///   will be sorted according to their ordering in the linked chunk.
    pub async fn find_event_with_relations(
        &self,
        event_id: &EventId,
        filter: Option<Vec<RelationType>>,
    ) -> Result<Option<(Event, Vec<Event>)>> {
        // Search in all loaded or stored events.
        Ok(self
            .inner
            .state
            .read()
            .await?
            .find_event_with_relations(event_id, filter.clone())
            .await
            .ok()
            .flatten())
    }

    /// Try to find the related events for an event by ID in this room.
    ///
    /// You can filter which types of related events to retrieve using
    /// `filter`. `None` will retrieve related events of any type.
    ///
    /// The related events are sorted like this:
    ///
    /// - events saved out-of-band (with `RoomEventCache::save_events`) will be
    ///   located at the beginning of the array.
    /// - events present in the linked chunk (be it in memory or in the storage)
    ///   will be sorted according to their ordering in the linked chunk.
    pub async fn find_event_relations(
        &self,
        event_id: &EventId,
        filter: Option<Vec<RelationType>>,
    ) -> Result<Vec<Event>> {
        // Search in all loaded or stored events.
        self.inner.state.read().await?.find_event_relations(event_id, filter.clone()).await
    }

    /// Clear all the storage for this [`RoomEventCache`].
    ///
    /// This will get rid of all the events from the linked chunk and persisted
    /// storage.
    pub async fn clear(&self) -> Result<()> {
        // Clear the linked chunk and persisted storage.
        let updates_as_vector_diffs = self.inner.state.write().await?.reset().await?;

        // Notify observers about the update.
        let _ = self.inner.update_sender.send(RoomEventCacheUpdate::UpdateTimelineEvents {
            diffs: updates_as_vector_diffs,
            origin: EventsOrigin::Cache,
        });

        // Notify observers about the generic update.
        let _ = self
            .inner
            .generic_update_sender
            .send(RoomEventCacheGenericUpdate { room_id: self.inner.room_id.clone() });

        Ok(())
    }

    /// Handle a single event from the `SendQueue`.
    pub(crate) async fn insert_sent_event_from_send_queue(&self, event: Event) -> Result<()> {
        self.inner
            .handle_timeline(
                Timeline { limited: false, prev_batch: None, events: vec![event] },
                Vec::new(),
                BTreeMap::new(),
            )
            .await
    }

    /// Save some events in the event cache, for further retrieval with
    /// [`Self::event`].
    pub(crate) async fn save_events(&self, events: impl IntoIterator<Item = Event>) {
        match self.inner.state.write().await {
            Ok(mut state_guard) => {
                if let Err(err) = state_guard.save_events(events).await {
                    warn!("couldn't save event in the event cache: {err}");
                }
            }

            Err(err) => {
                warn!("couldn't save event in the event cache: {err}");
            }
        }
    }

    /// Return a nice debug string (a vector of lines) for the linked chunk of
    /// events for this room.
    pub async fn debug_string(&self) -> Vec<String> {
        match self.inner.state.read().await {
            Ok(read_guard) => read_guard.room_linked_chunk().debug_string(),
            Err(err) => {
                warn!(?err, "Failed to obtain the read guard for the `RoomEventCache`");

                vec![]
            }
        }
    }
}

/// The (non-cloneable) details of the `RoomEventCache`.
pub(super) struct RoomEventCacheInner {
    /// The room id for this room.
    pub(super) room_id: OwnedRoomId,

    pub weak_room: WeakRoom,

    /// State for this room's event cache.
    pub state: RoomEventCacheStateLock,

    /// A notifier that we received a new pagination token.
    pub pagination_batch_token_notifier: Notify,

    pub pagination_status: SharedObservable<RoomPaginationStatus>,

    /// Sender to the auto-shrink channel.
    ///
    /// See doc comment around [`EventCache::auto_shrink_linked_chunk_task`] for
    /// more details.
    auto_shrink_sender: mpsc::Sender<AutoShrinkChannelPayload>,

    /// Sender part for update subscribers to this room.
    pub update_sender: Sender<RoomEventCacheUpdate>,

    /// A clone of [`EventCacheInner::generic_update_sender`].
    ///
    /// Whilst `EventCacheInner` handles the generic updates from the sync, or
    /// the storage, it doesn't handle the update from pagination. Having a
    /// clone here allows to access it from [`RoomPagination`].
    pub(super) generic_update_sender: Sender<RoomEventCacheGenericUpdate>,
}

impl RoomEventCacheInner {
    /// Creates a new cache for a room, and subscribes to room updates, so as
    /// to handle new timeline events.
    fn new(
        client: WeakClient,
        state: RoomEventCacheStateLock,
        pagination_status: SharedObservable<RoomPaginationStatus>,
        room_id: OwnedRoomId,
        auto_shrink_sender: mpsc::Sender<AutoShrinkChannelPayload>,
        update_sender: Sender<RoomEventCacheUpdate>,
        generic_update_sender: Sender<RoomEventCacheGenericUpdate>,
    ) -> Self {
        let weak_room = WeakRoom::new(client, room_id);

        Self {
            room_id: weak_room.room_id().to_owned(),
            weak_room,
            state,
            update_sender,
            pagination_batch_token_notifier: Default::default(),
            auto_shrink_sender,
            pagination_status,
            generic_update_sender,
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
                    let _ = self.update_sender.send(RoomEventCacheUpdate::MoveReadMarkerTo {
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

    /// Handle a [`Timeline`], i.e. new events received by a sync for this
    /// room.
    async fn handle_timeline(
        &self,
        timeline: Timeline,
        ephemeral_events: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    ) -> Result<()> {
        if timeline.events.is_empty()
            && timeline.prev_batch.is_none()
            && ephemeral_events.is_empty()
            && ambiguity_changes.is_empty()
        {
            return Ok(());
        }

        // Add all the events to the backend.
        trace!("adding new events");

        let (stored_prev_batch_token, timeline_event_diffs) =
            self.state.write().await?.handle_sync(timeline).await?;

        // Now that all events have been added, we can trigger the
        // `pagination_token_notifier`.
        if stored_prev_batch_token {
            self.pagination_batch_token_notifier.notify_one();
        }

        // The order matters here: first send the timeline event diffs, then only the
        // related events (read receipts, etc.).
        if !timeline_event_diffs.is_empty() {
            let _ = self.update_sender.send(RoomEventCacheUpdate::UpdateTimelineEvents {
                diffs: timeline_event_diffs,
                origin: EventsOrigin::Sync,
            });

            let _ = self
                .generic_update_sender
                .send(RoomEventCacheGenericUpdate { room_id: self.room_id.clone() });
        }

        if !ephemeral_events.is_empty() {
            let _ = self
                .update_sender
                .send(RoomEventCacheUpdate::AddEphemeralEvents { events: ephemeral_events });
        }

        if !ambiguity_changes.is_empty() {
            let _ =
                self.update_sender.send(RoomEventCacheUpdate::UpdateMembers { ambiguity_changes });
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
    Events { events: Vec<Event>, timeline_event_diffs: Vec<VectorDiff<Event>>, reached_start: bool },
}

// Use a private module to hide `events` to this parent module.
mod private {
    use std::{
        collections::{BTreeMap, HashMap, HashSet},
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
    };

    use eyeball::SharedObservable;
    use eyeball_im::VectorDiff;
    use itertools::Itertools;
    use matrix_sdk_base::{
        apply_redaction,
        deserialized_responses::{ThreadSummary, ThreadSummaryStatus, TimelineEventKind},
        event_cache::{
            Event, Gap,
            store::{EventCacheStoreLock, EventCacheStoreLockGuard, EventCacheStoreLockState},
        },
        linked_chunk::{
            ChunkContent, ChunkIdentifierGenerator, ChunkMetadata, LinkedChunkId,
            OwnedLinkedChunkId, Position, Update, lazy_loader,
        },
        serde_helpers::{extract_edit_target, extract_thread_root},
        sync::Timeline,
    };
    use matrix_sdk_common::executor::spawn;
    use ruma::{
        EventId, OwnedEventId, OwnedRoomId, RoomId,
        events::{
            AnySyncMessageLikeEvent, AnySyncTimelineEvent, MessageLikeEventType,
            relation::RelationType, room::redaction::SyncRoomRedactionEvent,
        },
        room_version_rules::RoomVersionRules,
        serde::Raw,
    };
    use tokio::sync::{
        Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard,
        broadcast::{Receiver, Sender},
    };
    use tracing::{debug, error, instrument, trace, warn};

    use super::{
        super::{
            BackPaginationOutcome, EventCacheError, RoomEventCacheLinkedChunkUpdate,
            RoomPaginationStatus, ThreadEventCacheUpdate,
            deduplicator::{DeduplicationOutcome, filter_duplicate_events},
            room::threads::ThreadEventCache,
        },
        EventLocation, EventsOrigin, LoadMoreEventsBackwardsOutcome, RoomEventCacheGenericUpdate,
        RoomEventCacheUpdate,
        events::EventLinkedChunk,
        sort_positions_descending,
    };

    /// State for a single room's event cache.
    ///
    /// This contains all the inner mutable states that ought to be updated at
    /// the same time.
    pub struct RoomEventCacheStateLock {
        /// The per-thread lock around the real state.
        locked_state: RwLock<RoomEventCacheStateLockInner>,

        /// Please see inline comment of [`Self::read`] to understand why it
        /// exists.
        read_lock_acquisition: Mutex<()>,
    }

    struct RoomEventCacheStateLockInner {
        /// Whether thread support has been enabled for the event cache.
        enabled_thread_support: bool,

        /// The room this state relates to.
        room_id: OwnedRoomId,

        /// Reference to the underlying backing store.
        store: EventCacheStoreLock,

        /// The loaded events for the current room, that is, the in-memory
        /// linked chunk for this room.
        room_linked_chunk: EventLinkedChunk,

        /// Threads present in this room.
        ///
        /// Keyed by the thread root event ID.
        threads: HashMap<OwnedEventId, ThreadEventCache>,

        pagination_status: SharedObservable<RoomPaginationStatus>,

        /// A clone of [`super::RoomEventCacheInner::update_sender`].
        ///
        /// This is used only by the [`RoomEventCacheStateLock::read`] and
        /// [`RoomEventCacheStateLock::write`] when the state must be reset.
        update_sender: Sender<RoomEventCacheUpdate>,

        /// A clone of [`super::super::EventCacheInner::generic_update_sender`].
        ///
        /// This is used only by the [`RoomEventCacheStateLock::read`] and
        /// [`RoomEventCacheStateLock::write`] when the state must be reset.
        generic_update_sender: Sender<RoomEventCacheGenericUpdate>,

        /// A clone of
        /// [`super::super::EventCacheInner::linked_chunk_update_sender`].
        linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,

        /// The rules for the version of this room.
        room_version_rules: RoomVersionRules,

        /// Have we ever waited for a previous-batch-token to come from sync, in
        /// the context of pagination? We do this at most once per room,
        /// the first time we try to run backward pagination. We reset
        /// that upon clearing the timeline events.
        waited_for_initial_prev_token: Arc<AtomicBool>,

        /// An atomic count of the current number of subscriber of the
        /// [`super::RoomEventCache`].
        subscriber_count: Arc<AtomicUsize>,
    }

    impl RoomEventCacheStateLock {
        /// Create a new state, or reload it from storage if it's been enabled.
        ///
        /// Not all events are going to be loaded. Only a portion of them. The
        /// [`EventLinkedChunk`] relies on a [`LinkedChunk`] to store all
        /// events. Only the last chunk will be loaded. It means the
        /// events are loaded from the most recent to the oldest. To
        /// load more events, see [`RoomPagination`].
        ///
        /// [`LinkedChunk`]: matrix_sdk_common::linked_chunk::LinkedChunk
        /// [`RoomPagination`]: super::RoomPagination
        #[allow(clippy::too_many_arguments)]
        pub async fn new(
            room_id: OwnedRoomId,
            room_version_rules: RoomVersionRules,
            enabled_thread_support: bool,
            update_sender: Sender<RoomEventCacheUpdate>,
            generic_update_sender: Sender<RoomEventCacheGenericUpdate>,
            linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,
            store: EventCacheStoreLock,
            pagination_status: SharedObservable<RoomPaginationStatus>,
        ) -> Result<Self, EventCacheError> {
            let store_guard = match store.lock().await? {
                // Lock is clean: all good!
                EventCacheStoreLockState::Clean(guard) => guard,

                // Lock is dirty, not a problem, it's the first time we are creating this state, no
                // need to refresh.
                EventCacheStoreLockState::Dirty(guard) => {
                    EventCacheStoreLockGuard::clear_dirty(&guard);

                    guard
                }
            };

            let linked_chunk_id = LinkedChunkId::Room(&room_id);

            // Load the full linked chunk's metadata, so as to feed the order tracker.
            //
            // If loading the full linked chunk failed, we'll clear the event cache, as it
            // indicates that at some point, there's some malformed data.
            let full_linked_chunk_metadata =
                match load_linked_chunk_metadata(&store_guard, linked_chunk_id).await {
                    Ok(metas) => metas,
                    Err(err) => {
                        error!(
                            "error when loading a linked chunk's metadata from the store: {err}"
                        );

                        // Try to clear storage for this room.
                        store_guard
                            .handle_linked_chunk_updates(linked_chunk_id, vec![Update::Clear])
                            .await?;

                        // Restart with an empty linked chunk.
                        None
                    }
                };

            let linked_chunk = match store_guard
                .load_last_chunk(linked_chunk_id)
                .await
                .map_err(EventCacheError::from)
                .and_then(|(last_chunk, chunk_identifier_generator)| {
                    lazy_loader::from_last_chunk(last_chunk, chunk_identifier_generator)
                        .map_err(EventCacheError::from)
                }) {
                Ok(linked_chunk) => linked_chunk,
                Err(err) => {
                    error!(
                        "error when loading a linked chunk's latest chunk from the store: {err}"
                    );

                    // Try to clear storage for this room.
                    store_guard
                        .handle_linked_chunk_updates(linked_chunk_id, vec![Update::Clear])
                        .await?;

                    None
                }
            };

            let waited_for_initial_prev_token = Arc::new(AtomicBool::new(false));

            Ok(Self {
                locked_state: RwLock::new(RoomEventCacheStateLockInner {
                    enabled_thread_support,
                    room_id,
                    store,
                    room_linked_chunk: EventLinkedChunk::with_initial_linked_chunk(
                        linked_chunk,
                        full_linked_chunk_metadata,
                    ),
                    // The threads mapping is intentionally empty at start, since we're going to
                    // reload threads lazily, as soon as we need to (based on external
                    // subscribers) or when we get new information about those (from
                    // sync).
                    threads: HashMap::new(),
                    pagination_status,
                    update_sender,
                    generic_update_sender,
                    linked_chunk_update_sender,
                    room_version_rules,
                    waited_for_initial_prev_token,
                    subscriber_count: Default::default(),
                }),
                read_lock_acquisition: Mutex::new(()),
            })
        }

        /// Lock this [`RoomEventCacheStateLock`] with per-thread shared access.
        ///
        /// This method locks the per-thread lock over the state, and then locks
        /// the cross-process lock over the store. It returns an RAII guard
        /// which will drop the read access to the state and to the store when
        /// dropped.
        ///
        /// If the cross-process lock over the store is dirty (see
        /// [`EventCacheStoreLockState`]), the state is reset to the last chunk.
        pub async fn read(&self) -> Result<RoomEventCacheStateLockReadGuard<'_>, EventCacheError> {
            // Only one call at a time to `read` is allowed.
            //
            // Why? Because in case the cross-process lock over the store is dirty, we need
            // to upgrade the read lock over the state to a write lock.
            //
            // ## Upgradable read lock
            //
            // One may argue that this upgrades can be done with an _upgradable read lock_
            // [^1] [^2]. We don't want to use this solution: an upgradable read lock is
            // basically a mutex because we are losing the shared access property, i.e.
            // having multiple read locks at the same time. This is an important property to
            // hold for performance concerns.
            //
            // ## Downgradable write lock
            //
            // One may also argue we could first obtain a write lock over the state from the
            // beginning, thus removing the need to upgrade the read lock to a write lock.
            // The write lock is then downgraded to a read lock once the dirty is cleaned
            // up. It can potentially create a deadlock in the following situation:
            //
            // - `read` is called once, it takes a write lock, then downgrades it to a read
            //   lock: the guard is kept alive somewhere,
            // - `read` is called again, and waits to obtain the write lock, which is
            //   impossible as long as the guard from the previous call is not dropped.
            //
            // ## “Atomic” read and write
            //
            // One may finally argue to first obtain a read lock over the state, then drop
            // it if the cross-process lock over the store is dirty, and immediately obtain
            // a write lock (which can later be downgraded to a read lock). The problem is
            // that this write lock is async: anything can happen between the drop and the
            // new lock acquisition, and it's not possible to pause the runtime in the
            // meantime.
            //
            // ## Semaphore with 1 permit, aka a Mutex
            //
            // The chosen idea is to allow only one execution at a time of this method: it
            // becomes a critical section. That way we are free to “upgrade” the read lock
            // by dropping it and obtaining a new write lock. All callers to this method are
            // waiting, so nothing can happen in the meantime.
            //
            // Note that it doesn't conflict with the `write` method because this later
            // immediately obtains a write lock, which avoids any conflict with this method.
            //
            // [^1]: https://docs.rs/lock_api/0.4.14/lock_api/struct.RwLock.html#method.upgradable_read
            // [^2]: https://docs.rs/async-lock/3.4.1/async_lock/struct.RwLock.html#method.upgradable_read
            let _one_reader_guard = self.read_lock_acquisition.lock().await;

            // Obtain a read lock.
            let state_guard = self.locked_state.read().await;

            match state_guard.store.lock().await? {
                EventCacheStoreLockState::Clean(store_guard) => {
                    Ok(RoomEventCacheStateLockReadGuard { state: state_guard, store: store_guard })
                }
                EventCacheStoreLockState::Dirty(store_guard) => {
                    // Drop the read lock, and take a write lock to modify the state.
                    // This is safe because only one reader at a time (see
                    // `Self::read_lock_acquisition`) is allowed.
                    drop(state_guard);
                    let state_guard = self.locked_state.write().await;

                    let mut guard = RoomEventCacheStateLockWriteGuard {
                        state: state_guard,
                        store: store_guard,
                    };

                    // Force to reload by shrinking to the last chunk.
                    let updates_as_vector_diffs = guard.force_shrink_to_last_chunk().await?;

                    // All good now, mark the cross-process lock as non-dirty.
                    EventCacheStoreLockGuard::clear_dirty(&guard.store);

                    // Downgrade the guard as soon as possible.
                    let guard = guard.downgrade();

                    // Now let the world know about the reload.
                    if !updates_as_vector_diffs.is_empty() {
                        // Notify observers about the update.
                        let _ = guard.state.update_sender.send(
                            RoomEventCacheUpdate::UpdateTimelineEvents {
                                diffs: updates_as_vector_diffs,
                                origin: EventsOrigin::Cache,
                            },
                        );

                        // Notify observers about the generic update.
                        let _ =
                            guard.state.generic_update_sender.send(RoomEventCacheGenericUpdate {
                                room_id: guard.state.room_id.clone(),
                            });
                    }

                    Ok(guard)
                }
            }
        }

        /// Lock this [`RoomEventCacheStateLock`] with exclusive per-thread
        /// write access.
        ///
        /// This method locks the per-thread lock over the state, and then locks
        /// the cross-process lock over the store. It returns an RAII guard
        /// which will drop the write access to the state and to the store when
        /// dropped.
        ///
        /// If the cross-process lock over the store is dirty (see
        /// [`EventCacheStoreLockState`]), the state is reset to the last chunk.
        pub async fn write(
            &self,
        ) -> Result<RoomEventCacheStateLockWriteGuard<'_>, EventCacheError> {
            let state_guard = self.locked_state.write().await;

            match state_guard.store.lock().await? {
                EventCacheStoreLockState::Clean(store_guard) => {
                    Ok(RoomEventCacheStateLockWriteGuard { state: state_guard, store: store_guard })
                }
                EventCacheStoreLockState::Dirty(store_guard) => {
                    let mut guard = RoomEventCacheStateLockWriteGuard {
                        state: state_guard,
                        store: store_guard,
                    };

                    // Force to reload by shrinking to the last chunk.
                    let updates_as_vector_diffs = guard.force_shrink_to_last_chunk().await?;

                    // All good now, mark the cross-process lock as non-dirty.
                    EventCacheStoreLockGuard::clear_dirty(&guard.store);

                    // Now let the world know about the reload.
                    if !updates_as_vector_diffs.is_empty() {
                        // Notify observers about the update.
                        let _ = guard.state.update_sender.send(
                            RoomEventCacheUpdate::UpdateTimelineEvents {
                                diffs: updates_as_vector_diffs,
                                origin: EventsOrigin::Cache,
                            },
                        );

                        // Notify observers about the generic update.
                        let _ =
                            guard.state.generic_update_sender.send(RoomEventCacheGenericUpdate {
                                room_id: guard.state.room_id.clone(),
                            });
                    }

                    Ok(guard)
                }
            }
        }
    }

    /// The read lock guard returned by [`RoomEventCacheStateLock::read`].
    pub struct RoomEventCacheStateLockReadGuard<'a> {
        /// The per-thread read lock guard over the
        /// [`RoomEventCacheStateLockInner`].
        state: RwLockReadGuard<'a, RoomEventCacheStateLockInner>,

        /// The cross-process lock guard over the store.
        store: EventCacheStoreLockGuard,
    }

    /// The write lock guard return by [`RoomEventCacheStateLock::write`].
    pub struct RoomEventCacheStateLockWriteGuard<'a> {
        /// The per-thread write lock guard over the
        /// [`RoomEventCacheStateLockInner`].
        state: RwLockWriteGuard<'a, RoomEventCacheStateLockInner>,

        /// The cross-process lock guard over the store.
        store: EventCacheStoreLockGuard,
    }

    impl<'a> RoomEventCacheStateLockWriteGuard<'a> {
        /// Synchronously downgrades a write lock into a read lock.
        ///
        /// The per-thread/state lock is downgraded atomically, without allowing
        /// any writers to take exclusive access of the lock in the meantime.
        ///
        /// It returns an RAII guard which will drop the write access to the
        /// state and to the store when dropped.
        fn downgrade(self) -> RoomEventCacheStateLockReadGuard<'a> {
            RoomEventCacheStateLockReadGuard { state: self.state.downgrade(), store: self.store }
        }
    }

    impl<'a> RoomEventCacheStateLockReadGuard<'a> {
        /// Returns a read-only reference to the underlying room linked chunk.
        pub fn room_linked_chunk(&self) -> &EventLinkedChunk {
            &self.state.room_linked_chunk
        }

        pub fn subscriber_count(&self) -> &Arc<AtomicUsize> {
            &self.state.subscriber_count
        }

        /// Find a single event in this room.
        ///
        /// It starts by looking into loaded events in `EventLinkedChunk` before
        /// looking inside the storage.
        pub async fn find_event(
            &self,
            event_id: &EventId,
        ) -> Result<Option<(EventLocation, Event)>, EventCacheError> {
            find_event(event_id, &self.state.room_id, &self.state.room_linked_chunk, &self.store)
                .await
        }

        /// Find an event and all its relations in the persisted storage.
        ///
        /// This goes straight to the database, as a simplification; we don't
        /// expect to need to have to look up in memory events, or that
        /// all the related events are actually loaded.
        ///
        /// The related events are sorted like this:
        /// - events saved out-of-band with
        ///   [`super::RoomEventCache::save_events`] will be located at the
        ///   beginning of the array.
        /// - events present in the linked chunk (be it in memory or in the
        ///   database) will be sorted according to their ordering in the linked
        ///   chunk.
        pub async fn find_event_with_relations(
            &self,
            event_id: &EventId,
            filters: Option<Vec<RelationType>>,
        ) -> Result<Option<(Event, Vec<Event>)>, EventCacheError> {
            find_event_with_relations(
                event_id,
                &self.state.room_id,
                filters,
                &self.state.room_linked_chunk,
                &self.store,
            )
            .await
        }

        /// Find all relations for an event in the persisted storage.
        ///
        /// This goes straight to the database, as a simplification; we don't
        /// expect to need to have to look up in memory events, or that
        /// all the related events are actually loaded.
        ///
        /// The related events are sorted like this:
        /// - events saved out-of-band with
        ///   [`super::RoomEventCache::save_events`] will be located at the
        ///   beginning of the array.
        /// - events present in the linked chunk (be it in memory or in the
        ///   database) will be sorted according to their ordering in the linked
        ///   chunk.
        pub async fn find_event_relations(
            &self,
            event_id: &EventId,
            filters: Option<Vec<RelationType>>,
        ) -> Result<Vec<Event>, EventCacheError> {
            find_event_relations(
                event_id,
                &self.state.room_id,
                filters,
                &self.state.room_linked_chunk,
                &self.store,
            )
            .await
        }

        //// Find a single event in this room, starting from the most recent event.
        ///
        /// The `predicate` receives two arguments: the current event, and the
        /// ID of the _previous_ (older) event.
        ///
        /// **Warning**! It looks into the loaded events from the in-memory
        /// linked chunk **only**. It doesn't look inside the storage,
        /// contrary to [`Self::find_event`].
        pub fn rfind_map_event_in_memory_by<'i, O, P>(&'i self, mut predicate: P) -> Option<O>
        where
            P: FnMut(&'i Event, Option<&'i Event>) -> Option<O>,
        {
            self.state
                .room_linked_chunk
                .revents()
                .peekable()
                .batching(|iter| {
                    iter.next().map(|(_position, event)| {
                        (event, iter.peek().map(|(_next_position, next_event)| *next_event))
                    })
                })
                .find_map(|(event, next_event)| predicate(event, next_event))
        }

        #[cfg(test)]
        pub fn is_dirty(&self) -> bool {
            EventCacheStoreLockGuard::is_dirty(&self.store)
        }
    }

    impl<'a> RoomEventCacheStateLockWriteGuard<'a> {
        /// Returns a write reference to the underlying room linked chunk.
        #[cfg(any(feature = "e2e-encryption", test))]
        pub fn room_linked_chunk(&mut self) -> &mut EventLinkedChunk {
            &mut self.state.room_linked_chunk
        }

        /// Get a reference to the `waited_for_initial_prev_token` atomic bool.
        pub fn waited_for_initial_prev_token(&self) -> &Arc<AtomicBool> {
            &self.state.waited_for_initial_prev_token
        }

        /// Find a single event in this room.
        ///
        /// It starts by looking into loaded events in `EventLinkedChunk` before
        /// looking inside the storage.
        pub async fn find_event(
            &self,
            event_id: &EventId,
        ) -> Result<Option<(EventLocation, Event)>, EventCacheError> {
            find_event(event_id, &self.state.room_id, &self.state.room_linked_chunk, &self.store)
                .await
        }

        /// Find an event and all its relations in the persisted storage.
        ///
        /// This goes straight to the database, as a simplification; we don't
        /// expect to need to have to look up in memory events, or that
        /// all the related events are actually loaded.
        ///
        /// The related events are sorted like this:
        /// - events saved out-of-band with
        ///   [`super::RoomEventCache::save_events`] will be located at the
        ///   beginning of the array.
        /// - events present in the linked chunk (be it in memory or in the
        ///   database) will be sorted according to their ordering in the linked
        ///   chunk.
        pub async fn find_event_with_relations(
            &self,
            event_id: &EventId,
            filters: Option<Vec<RelationType>>,
        ) -> Result<Option<(Event, Vec<Event>)>, EventCacheError> {
            find_event_with_relations(
                event_id,
                &self.state.room_id,
                filters,
                &self.state.room_linked_chunk,
                &self.store,
            )
            .await
        }

        /// Load more events backwards if the last chunk is **not** a gap.
        pub async fn load_more_events_backwards(
            &mut self,
        ) -> Result<LoadMoreEventsBackwardsOutcome, EventCacheError> {
            // If any in-memory chunk is a gap, don't load more events, and let the caller
            // resolve the gap.
            if let Some(prev_token) = self.state.room_linked_chunk.rgap().map(|gap| gap.prev_token)
            {
                return Ok(LoadMoreEventsBackwardsOutcome::Gap { prev_token: Some(prev_token) });
            }

            let prev_first_chunk = self
                .state
                .room_linked_chunk
                .chunks()
                .next()
                .expect("a linked chunk is never empty");

            // The first chunk is not a gap, we can load its previous chunk.
            let linked_chunk_id = LinkedChunkId::Room(&self.state.room_id);
            let new_first_chunk = match self
                .store
                .load_previous_chunk(linked_chunk_id, prev_first_chunk.identifier())
                .await
            {
                Ok(Some(new_first_chunk)) => {
                    // All good, let's continue with this chunk.
                    new_first_chunk
                }

                Ok(None) => {
                    // If we never received events for this room, this means we've never received a
                    // sync for that room, because every room must have *at least* a room creation
                    // event. Otherwise, we have reached the start of the timeline.

                    if self.state.room_linked_chunk.events().next().is_some() {
                        // If there's at least one event, this means we've reached the start of the
                        // timeline, since the chunk is fully loaded.
                        trace!("chunk is fully loaded and non-empty: reached_start=true");
                        return Ok(LoadMoreEventsBackwardsOutcome::StartOfTimeline);
                    }

                    // Otherwise, start back-pagination from the end of the room.
                    return Ok(LoadMoreEventsBackwardsOutcome::Gap { prev_token: None });
                }

                Err(err) => {
                    error!("error when loading the previous chunk of a linked chunk: {err}");

                    // Clear storage for this room.
                    self.store
                        .handle_linked_chunk_updates(linked_chunk_id, vec![Update::Clear])
                        .await?;

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

            if let Err(err) =
                self.state.room_linked_chunk.insert_new_chunk_as_first(new_first_chunk)
            {
                error!("error when inserting the previous chunk into its linked chunk: {err}");

                // Clear storage for this room.
                self.store
                    .handle_linked_chunk_updates(
                        LinkedChunkId::Room(&self.state.room_id),
                        vec![Update::Clear],
                    )
                    .await?;

                // Return the error.
                return Err(err.into());
            }

            // ⚠️ Let's not propagate the updates to the store! We already have these data
            // in the store! Let's drain them.
            let _ = self.state.room_linked_chunk.store_updates().take();

            // However, we want to get updates as `VectorDiff`s.
            let timeline_event_diffs = self.state.room_linked_chunk.updates_as_vector_diffs();

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
        pub async fn shrink_to_last_chunk(&mut self) -> Result<(), EventCacheError> {
            // Attempt to load the last chunk.
            let linked_chunk_id = LinkedChunkId::Room(&self.state.room_id);
            let (last_chunk, chunk_identifier_generator) =
                match self.store.load_last_chunk(linked_chunk_id).await {
                    Ok(pair) => pair,

                    Err(err) => {
                        // If loading the last chunk failed, clear the entire linked chunk.
                        error!("error when reloading a linked chunk from memory: {err}");

                        // Clear storage for this room.
                        self.store
                            .handle_linked_chunk_updates(linked_chunk_id, vec![Update::Clear])
                            .await?;

                        // Restart with an empty linked chunk.
                        (None, ChunkIdentifierGenerator::new_from_scratch())
                    }
                };

            debug!("unloading the linked chunk, and resetting it to its last chunk");

            // Remove all the chunks from the linked chunks, except for the last one, and
            // updates the chunk identifier generator.
            if let Err(err) =
                self.state.room_linked_chunk.replace_with(last_chunk, chunk_identifier_generator)
            {
                error!("error when replacing the linked chunk: {err}");
                return self.reset_internal().await;
            }

            // Let pagination observers know that we may have not reached the start of the
            // timeline.
            // TODO: likely need to cancel any ongoing pagination.
            self.state
                .pagination_status
                .set(RoomPaginationStatus::Idle { hit_timeline_start: false });

            // Don't propagate those updates to the store; this is only for the in-memory
            // representation that we're doing this. Let's drain those store updates.
            let _ = self.state.room_linked_chunk.store_updates().take();

            Ok(())
        }

        /// Automatically shrink the room if there are no more subscribers, as
        /// indicated by the atomic number of active subscribers.
        #[must_use = "Propagate `VectorDiff` updates via `RoomEventCacheUpdate`"]
        pub async fn auto_shrink_if_no_subscribers(
            &mut self,
        ) -> Result<Option<Vec<VectorDiff<Event>>>, EventCacheError> {
            let subscriber_count = self.state.subscriber_count.load(Ordering::SeqCst);

            trace!(subscriber_count, "received request to auto-shrink");

            if subscriber_count == 0 {
                // If we are the last strong reference to the auto-shrinker, we can shrink the
                // events data structure to its last chunk.
                self.shrink_to_last_chunk().await?;

                Ok(Some(self.state.room_linked_chunk.updates_as_vector_diffs()))
            } else {
                Ok(None)
            }
        }

        /// Force to shrink the room, whenever there is subscribers or not.
        #[must_use = "Propagate `VectorDiff` updates via `RoomEventCacheUpdate`"]
        pub async fn force_shrink_to_last_chunk(
            &mut self,
        ) -> Result<Vec<VectorDiff<Event>>, EventCacheError> {
            self.shrink_to_last_chunk().await?;

            Ok(self.state.room_linked_chunk.updates_as_vector_diffs())
        }

        /// Remove events by their position, in `EventLinkedChunk` and in
        /// `EventCacheStore`.
        ///
        /// This method is purposely isolated because it must ensure that
        /// positions are sorted appropriately or it can be disastrous.
        #[instrument(skip_all)]
        pub async fn remove_events(
            &mut self,
            in_memory_events: Vec<(OwnedEventId, Position)>,
            in_store_events: Vec<(OwnedEventId, Position)>,
        ) -> Result<(), EventCacheError> {
            // In-store events.
            if !in_store_events.is_empty() {
                let mut positions = in_store_events
                    .into_iter()
                    .map(|(_event_id, position)| position)
                    .collect::<Vec<_>>();

                sort_positions_descending(&mut positions);

                let updates = positions
                    .into_iter()
                    .map(|pos| Update::RemoveItem { at: pos })
                    .collect::<Vec<_>>();

                self.apply_store_only_updates(updates).await?;
            }

            // In-memory events.
            if in_memory_events.is_empty() {
                // Nothing else to do, return early.
                return Ok(());
            }

            // `remove_events_by_position` is responsible of sorting positions.
            self.state
                .room_linked_chunk
                .remove_events_by_position(
                    in_memory_events.into_iter().map(|(_event_id, position)| position).collect(),
                )
                .expect("failed to remove an event");

            self.propagate_changes().await
        }

        async fn propagate_changes(&mut self) -> Result<(), EventCacheError> {
            let updates = self.state.room_linked_chunk.store_updates().take();
            self.send_updates_to_store(updates).await
        }

        /// Apply some updates that are effective only on the store itself.
        ///
        /// This method should be used only for updates that happen *outside*
        /// the in-memory linked chunk. Such updates must be applied
        /// onto the ordering tracker as well as to the persistent
        /// storage.
        async fn apply_store_only_updates(
            &mut self,
            updates: Vec<Update<Event, Gap>>,
        ) -> Result<(), EventCacheError> {
            self.state.room_linked_chunk.order_tracker.map_updates(&updates);
            self.send_updates_to_store(updates).await
        }

        async fn send_updates_to_store(
            &mut self,
            mut updates: Vec<Update<Event, Gap>>,
        ) -> Result<(), EventCacheError> {
            if updates.is_empty() {
                return Ok(());
            }

            // Strip relations from updates which insert or replace items.
            for update in updates.iter_mut() {
                match update {
                    Update::PushItems { items, .. } => strip_relations_from_events(items),
                    Update::ReplaceItem { item, .. } => strip_relations_from_event(item),
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
                let linked_chunk_id = LinkedChunkId::Room(&room_id);
                store.handle_linked_chunk_updates(linked_chunk_id, cloned_updates).await?;
                trace!("linked chunk updates applied");

                super::Result::Ok(())
            })
            .await
            .expect("joining failed")?;

            // Forward that the store got updated to observers.
            let _ = self.state.linked_chunk_update_sender.send(RoomEventCacheLinkedChunkUpdate {
                linked_chunk_id: OwnedLinkedChunkId::Room(self.state.room_id.clone()),
                updates,
            });

            Ok(())
        }

        /// Reset this data structure as if it were brand new.
        ///
        /// Return a single diff update that is a clear of all events; as a
        /// result, the caller may override any pending diff updates
        /// with the result of this function.
        pub async fn reset(&mut self) -> Result<Vec<VectorDiff<Event>>, EventCacheError> {
            self.reset_internal().await?;

            let diff_updates = self.state.room_linked_chunk.updates_as_vector_diffs();

            // Ensure the contract defined in the doc comment is true:
            debug_assert_eq!(diff_updates.len(), 1);
            debug_assert!(matches!(diff_updates[0], VectorDiff::Clear));

            Ok(diff_updates)
        }

        async fn reset_internal(&mut self) -> Result<(), EventCacheError> {
            self.state.room_linked_chunk.reset();

            // No need to update the thread summaries: the room events are
            // gone because of the reset of `room_linked_chunk`.
            //
            // Clear the threads.
            for thread in self.state.threads.values_mut() {
                thread.clear();
            }

            self.propagate_changes().await?;

            // Reset the pagination state too: pretend we never waited for the initial
            // prev-batch token, and indicate that we're not at the start of the
            // timeline, since we don't know about that anymore.
            self.state.waited_for_initial_prev_token.store(false, Ordering::SeqCst);
            // TODO: likely must cancel any ongoing back-paginations too
            self.state
                .pagination_status
                .set(RoomPaginationStatus::Idle { hit_timeline_start: false });

            Ok(())
        }

        /// Handle the result of a sync.
        ///
        /// It may send room event cache updates to the given sender, if it
        /// generated any of those.
        ///
        /// Returns `true` for the first part of the tuple if a new gap
        /// (previous-batch token) has been inserted, `false` otherwise.
        #[must_use = "Propagate `VectorDiff` updates via `RoomEventCacheUpdate`"]
        pub async fn handle_sync(
            &mut self,
            mut timeline: Timeline,
        ) -> Result<(bool, Vec<VectorDiff<Event>>), EventCacheError> {
            let mut prev_batch = timeline.prev_batch.take();

            let DeduplicationOutcome {
                all_events: events,
                in_memory_duplicated_event_ids,
                in_store_duplicated_event_ids,
                non_empty_all_duplicates: all_duplicates,
            } = filter_duplicate_events(
                &self.store,
                LinkedChunkId::Room(&self.state.room_id),
                &self.state.room_linked_chunk,
                timeline.events,
            )
            .await?;

            // If the timeline isn't limited, and we already knew about some past events,
            // then this definitely knows what the timeline head is (either we know
            // about all the events persisted in storage, or we have a gap
            // somewhere). In this case, we can ditch the previous-batch
            // token, which is an optimization to avoid unnecessary future back-pagination
            // requests.
            //
            // We can also ditch it if we knew about all the events that came from sync,
            // namely, they were all deduplicated. In this case, using the
            // previous-batch token would only result in fetching other events we
            // knew about. This is slightly incorrect in the presence of
            // network splits, but this has shown to be Good Enough™.
            if !timeline.limited && self.state.room_linked_chunk.events().next().is_some()
                || all_duplicates
            {
                prev_batch = None;
            }

            if prev_batch.is_some() {
                // Sad time: there's a gap, somewhere, in the timeline, and there's at least one
                // non-duplicated event. We don't know which threads might have gappy, so we
                // must invalidate them all :(
                // TODO(bnjbvr): figure out a better catchup mechanism for threads.
                let mut summaries_to_update = Vec::new();

                for (thread_root, thread) in self.state.threads.iter_mut() {
                    // Empty the thread's linked chunk.
                    thread.clear();

                    summaries_to_update.push(thread_root.clone());
                }

                // Now, update the summaries to indicate that we're not sure what the latest
                // thread event is. The thread count can remain as is, as it might still be
                // valid, and there's no good value to reset it to, anyways.
                for thread_root in summaries_to_update {
                    let Some((location, mut target_event)) = self.find_event(&thread_root).await?
                    else {
                        trace!(%thread_root, "thread root event is unknown, when updating thread summary after a gappy sync");
                        continue;
                    };

                    if let Some(mut prev_summary) = target_event.thread_summary.summary().cloned() {
                        prev_summary.latest_reply = None;

                        target_event.thread_summary = ThreadSummaryStatus::Some(prev_summary);

                        self.replace_event_at(location, target_event).await?;
                    }
                }
            }

            if all_duplicates {
                // No new events and no gap (per the previous check), thus no need to change the
                // room state. We're done!
                return Ok((false, Vec::new()));
            }

            let has_new_gap = prev_batch.is_some();

            // If we've never waited for an initial previous-batch token, and we've now
            // inserted a gap, no need to wait for a previous-batch token later.
            if !self.state.waited_for_initial_prev_token.load(Ordering::SeqCst) && has_new_gap {
                self.state.waited_for_initial_prev_token.store(true, Ordering::SeqCst);
            }

            // Remove the old duplicated events.
            //
            // We don't have to worry the removals can change the position of the existing
            // events, because we are pushing all _new_ `events` at the back.
            self.remove_events(in_memory_duplicated_event_ids, in_store_duplicated_event_ids)
                .await?;

            self.state
                .room_linked_chunk
                .push_live_events(prev_batch.map(|prev_token| Gap { prev_token }), &events);

            self.post_process_new_events(events, true).await?;

            if timeline.limited && has_new_gap {
                // If there was a previous batch token for a limited timeline, unload the chunks
                // so it only contains the last one; otherwise, there might be a
                // valid gap in between, and observers may not render it (yet).
                //
                // We must do this *after* persisting these events to storage (in
                // `post_process_new_events`).
                self.shrink_to_last_chunk().await?;
            }

            let timeline_event_diffs = self.state.room_linked_chunk.updates_as_vector_diffs();

            Ok((has_new_gap, timeline_event_diffs))
        }

        /// Handle the result of a single back-pagination request.
        ///
        /// If the `prev_token` is set, then this function will check that the
        /// corresponding gap is present in the in-memory linked chunk.
        /// If it's not the case, `Ok(None)` will be returned, and the
        /// caller may decide to do something based on that (e.g. restart a
        /// pagination).
        #[must_use = "Propagate `VectorDiff` updates via `RoomEventCacheUpdate`"]
        pub async fn handle_backpagination(
            &mut self,
            events: Vec<Event>,
            mut new_token: Option<String>,
            prev_token: Option<String>,
        ) -> Result<Option<(BackPaginationOutcome, Vec<VectorDiff<Event>>)>, EventCacheError>
        {
            // Check that the previous token still exists; otherwise it's a sign that the
            // room's timeline has been cleared.
            let prev_gap_id = if let Some(token) = prev_token {
                // Find the corresponding gap in the in-memory linked chunk.
                let gap_chunk_id = self.state.room_linked_chunk.chunk_identifier(|chunk| {
                    matches!(chunk.content(), ChunkContent::Gap(Gap { prev_token }) if *prev_token == token)
                });

                if gap_chunk_id.is_none() {
                    // We got a previous-batch token from the linked chunk *before* running the
                    // request, but it is missing *after* completing the request.
                    //
                    // It may be a sign the linked chunk has been reset, but it's fine, per this
                    // function's contract.
                    return Ok(None);
                }

                gap_chunk_id
            } else {
                None
            };

            let DeduplicationOutcome {
                all_events: mut events,
                in_memory_duplicated_event_ids,
                in_store_duplicated_event_ids,
                non_empty_all_duplicates: all_duplicates,
            } = filter_duplicate_events(
                &self.store,
                LinkedChunkId::Room(&self.state.room_id),
                &self.state.room_linked_chunk,
                events,
            )
            .await?;

            // If not all the events have been back-paginated, we need to remove the
            // previous ones, otherwise we can end up with misordered events.
            //
            // Consider the following scenario:
            // - sync returns [D, E, F]
            // - then sync returns [] with a previous batch token PB1, so the internal
            //   linked chunk state is [D, E, F, PB1].
            // - back-paginating with PB1 may return [A, B, C, D, E, F].
            //
            // Only inserting the new events when replacing PB1 would result in a timeline
            // ordering of [D, E, F, A, B, C], which is incorrect. So we do have to remove
            // all the events, in case this happens (see also #4746).

            if !all_duplicates {
                // Let's forget all the previous events.
                self.remove_events(in_memory_duplicated_event_ids, in_store_duplicated_event_ids)
                    .await?;
            } else {
                // All new events are duplicated, they can all be ignored.
                events.clear();
                // The gap can be ditched too, as it won't be useful to backpaginate any
                // further.
                new_token = None;
            }

            // `/messages` has been called with `dir=b` (backwards), so the events are in
            // the inverted order; reorder them.
            let topo_ordered_events = events.iter().rev().cloned().collect::<Vec<_>>();

            let new_gap = new_token.map(|prev_token| Gap { prev_token });
            let reached_start = self.state.room_linked_chunk.finish_back_pagination(
                prev_gap_id,
                new_gap,
                &topo_ordered_events,
            );

            // Note: this flushes updates to the store.
            self.post_process_new_events(topo_ordered_events, false).await?;

            let event_diffs = self.state.room_linked_chunk.updates_as_vector_diffs();

            Ok(Some((BackPaginationOutcome { events, reached_start }, event_diffs)))
        }

        /// Subscribe to thread for a given root event, and get a (maybe empty)
        /// initially known list of events for that thread.
        pub fn subscribe_to_thread(
            &mut self,
            root: OwnedEventId,
        ) -> (Vec<Event>, Receiver<ThreadEventCacheUpdate>) {
            self.get_or_reload_thread(root).subscribe()
        }

        /// Back paginate in the given thread.
        ///
        /// Will always start from the end, unless we previously paginated.
        pub fn finish_thread_network_pagination(
            &mut self,
            root: OwnedEventId,
            prev_token: Option<String>,
            new_token: Option<String>,
            events: Vec<Event>,
        ) -> Option<BackPaginationOutcome> {
            self.get_or_reload_thread(root).finish_network_pagination(prev_token, new_token, events)
        }

        pub fn load_more_thread_events_backwards(
            &mut self,
            root: OwnedEventId,
        ) -> LoadMoreEventsBackwardsOutcome {
            self.get_or_reload_thread(root).load_more_events_backwards()
        }

        // --------------------------------------------
        // utility methods
        // --------------------------------------------

        /// Post-process new events, after they have been added to the in-memory
        /// linked chunk.
        ///
        /// Flushes updates to disk first.
        pub(in super::super) async fn post_process_new_events(
            &mut self,
            events: Vec<Event>,
            is_sync: bool,
        ) -> Result<(), EventCacheError> {
            // Update the store before doing the post-processing.
            self.propagate_changes().await?;

            let mut new_events_by_thread: BTreeMap<_, Vec<_>> = BTreeMap::new();

            for event in events {
                self.maybe_apply_new_redaction(&event).await?;

                if self.state.enabled_thread_support {
                    // Only add the event to a thread if:
                    // - thread support is enabled,
                    // - and if this is a sync (we can't know where to insert backpaginated events
                    //   in threads).
                    if is_sync {
                        if let Some(thread_root) = extract_thread_root(event.raw()) {
                            new_events_by_thread
                                .entry(thread_root)
                                .or_default()
                                .push(event.clone());
                        } else if let Some(event_id) = event.event_id() {
                            // If we spot the root of a thread, add it to its linked chunk.
                            if self.state.threads.contains_key(&event_id) {
                                new_events_by_thread
                                    .entry(event_id)
                                    .or_default()
                                    .push(event.clone());
                            }
                        }
                    }

                    // Look for edits that may apply to a thread; we'll process them later.
                    if let Some(edit_target) = extract_edit_target(event.raw()) {
                        // If the edited event is known, and part of a thread,
                        if let Some((_location, edit_target_event)) =
                            self.find_event(&edit_target).await?
                            && let Some(thread_root) = extract_thread_root(edit_target_event.raw())
                        {
                            // Mark the thread for processing, unless it was already marked as
                            // such.
                            new_events_by_thread.entry(thread_root).or_default();
                        }
                    }
                }

                // Save a bundled thread event, if there was one.
                if let Some(bundled_thread) = event.bundled_latest_thread_event {
                    self.save_events([*bundled_thread]).await?;
                }
            }

            if self.state.enabled_thread_support {
                self.update_threads(new_events_by_thread).await?;
            }

            Ok(())
        }

        fn get_or_reload_thread(&mut self, root_event_id: OwnedEventId) -> &mut ThreadEventCache {
            // TODO: when there's persistent storage, try to lazily reload from disk, if
            // missing from memory.
            let room_id = self.state.room_id.clone();
            let linked_chunk_update_sender = self.state.linked_chunk_update_sender.clone();

            self.state.threads.entry(root_event_id.clone()).or_insert_with(|| {
                ThreadEventCache::new(room_id, root_event_id, linked_chunk_update_sender)
            })
        }

        #[instrument(skip_all)]
        async fn update_threads(
            &mut self,
            new_events_by_thread: BTreeMap<OwnedEventId, Vec<Event>>,
        ) -> Result<(), EventCacheError> {
            for (thread_root, new_events) in new_events_by_thread {
                let thread_cache = self.get_or_reload_thread(thread_root.clone());

                thread_cache.add_live_events(new_events);

                let mut latest_event_id = thread_cache.latest_event_id();

                // If there's an edit to the latest event in the thread, use the latest edit
                // event id as the latest event id for the thread summary.
                if let Some(event_id) = latest_event_id.as_ref()
                    && let Some((_, edits)) = self
                        .find_event_with_relations(event_id, Some(vec![RelationType::Replacement]))
                        .await?
                    && let Some(latest_edit) = edits.last()
                {
                    latest_event_id = latest_edit.event_id();
                }

                self.maybe_update_thread_summary(thread_root, latest_event_id).await?;
            }

            Ok(())
        }

        /// Update a thread summary on the given thread root, if needs be.
        async fn maybe_update_thread_summary(
            &mut self,
            thread_root: OwnedEventId,
            latest_event_id: Option<OwnedEventId>,
        ) -> Result<(), EventCacheError> {
            // Add a thread summary to the (room) event which has the thread root, if we
            // knew about it.

            let Some((location, mut target_event)) = self.find_event(&thread_root).await? else {
                trace!(%thread_root, "thread root event is missing from the room linked chunk");
                return Ok(());
            };

            let prev_summary = target_event.thread_summary.summary();

            // Recompute the thread summary, if needs be.

            // Read the latest number of thread replies from the store.
            //
            // Implementation note: since this is based on the `m.relates_to` field, and
            // that field can only be present on room messages, we don't have to
            // worry about filtering out aggregation events (like
            // reactions/edits/etc.). Pretty neat, huh?
            let num_replies = {
                let thread_replies = self
                    .store
                    .find_event_relations(
                        &self.state.room_id,
                        &thread_root,
                        Some(&[RelationType::Thread]),
                    )
                    .await?;
                thread_replies.len().try_into().unwrap_or(u32::MAX)
            };

            let new_summary = if num_replies > 0 {
                Some(ThreadSummary { num_replies, latest_reply: latest_event_id })
            } else {
                None
            };

            if prev_summary == new_summary.as_ref() {
                trace!(%thread_root, "thread summary is already up-to-date");
                return Ok(());
            }

            // Trigger an update to observers.
            trace!(%thread_root, "updating thread summary: {new_summary:?}");
            target_event.thread_summary = ThreadSummaryStatus::from_opt(new_summary);
            self.replace_event_at(location, target_event).await
        }

        /// Replaces a single event, be it saved in memory or in the store.
        ///
        /// If it was saved in memory, this will emit a notification to
        /// observers that a single item has been replaced. Otherwise,
        /// such a notification is not emitted, because observers are
        /// unlikely to observe the store updates directly.
        pub(crate) async fn replace_event_at(
            &mut self,
            location: EventLocation,
            event: Event,
        ) -> Result<(), EventCacheError> {
            match location {
                EventLocation::Memory(position) => {
                    self.state
                        .room_linked_chunk
                        .replace_event_at(position, event)
                        .expect("should have been a valid position of an item");
                    // We just changed the in-memory representation; synchronize this with
                    // the store.
                    self.propagate_changes().await?;
                }
                EventLocation::Store => {
                    self.save_events([event]).await?;
                }
            }

            Ok(())
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

            let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomRedaction(
                redaction,
            ))) = raw_event.deserialize()
            else {
                return Ok(());
            };

            let Some(event_id) = redaction.redacts(&self.state.room_version_rules.redaction) else {
                warn!("missing target event id from the redaction event");
                return Ok(());
            };

            // Replace the redacted event by a redacted form, if we knew about it.
            let Some((location, mut target_event)) = self.find_event(event_id).await? else {
                trace!("redacted event is missing from the linked chunk");
                return Ok(());
            };

            // Don't redact already redacted events.
            let thread_root = if let Ok(deserialized) = target_event.raw().deserialize() {
                // TODO: replace with `deserialized.is_redacted()` when
                // https://github.com/ruma/ruma/pull/2254 has been merged.
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

                // If the event is part of a thread, update the thread linked chunk and the
                // summary.
                extract_thread_root(target_event.raw())
            } else {
                warn!("failed to deserialize the event to redact");
                None
            };

            if let Some(redacted_event) = apply_redaction(
                target_event.raw(),
                event.raw().cast_ref_unchecked::<SyncRoomRedactionEvent>(),
                &self.state.room_version_rules.redaction,
            ) {
                // It's safe to cast `redacted_event` here:
                // - either the event was an `AnyTimelineEvent` cast to `AnySyncTimelineEvent`
                //   when calling .raw(), so it's still one under the hood.
                // - or it wasn't, and it's a plain `AnySyncTimelineEvent` in this case.
                target_event.replace_raw(redacted_event.cast_unchecked());

                self.replace_event_at(location, target_event).await?;

                // If the redacted event was part of a thread, remove it in the thread linked
                // chunk too, and make sure to update the thread root's summary
                // as well.
                //
                // Note: there is an ordering issue here: the above `replace_event_at` must
                // happen BEFORE we recompute the summary, otherwise the set of
                // replies may include the to-be-redacted event.
                if let Some(thread_root) = thread_root
                    && let Some(thread_cache) = self.state.threads.get_mut(&thread_root)
                {
                    thread_cache.remove_if_present(event_id);

                    // The number of replies may have changed, so update the thread summary if
                    // needs be.
                    let latest_event_id = thread_cache.latest_event_id();
                    self.maybe_update_thread_summary(thread_root, latest_event_id).await?;
                }
            }

            Ok(())
        }

        /// Save events into the database, without notifying observers.
        pub async fn save_events(
            &mut self,
            events: impl IntoIterator<Item = Event>,
        ) -> Result<(), EventCacheError> {
            let store = self.store.clone();
            let room_id = self.state.room_id.clone();
            let events = events.into_iter().collect::<Vec<_>>();

            // Spawn a task so the save is uninterrupted by task cancellation.
            spawn(async move {
                for event in events {
                    store.save_event(&room_id, event).await?;
                }
                super::Result::Ok(())
            })
            .await
            .expect("joining failed")?;

            Ok(())
        }

        #[cfg(test)]
        pub fn is_dirty(&self) -> bool {
            EventCacheStoreLockGuard::is_dirty(&self.store)
        }
    }

    /// Load a linked chunk's full metadata, making sure the chunks are
    /// according to their their links.
    ///
    /// Returns `None` if there's no such linked chunk in the store, or an
    /// error if the linked chunk is malformed.
    async fn load_linked_chunk_metadata(
        store_guard: &EventCacheStoreLockGuard,
        linked_chunk_id: LinkedChunkId<'_>,
    ) -> Result<Option<Vec<ChunkMetadata>>, EventCacheError> {
        let mut all_chunks = store_guard
            .load_all_chunks_metadata(linked_chunk_id)
            .await
            .map_err(EventCacheError::from)?;

        if all_chunks.is_empty() {
            // There are no chunks, so there's nothing to do.
            return Ok(None);
        }

        // Transform the vector into a hashmap, for quick lookup of the predecessors.
        let chunk_map: HashMap<_, _> =
            all_chunks.iter().map(|meta| (meta.identifier, meta)).collect();

        // Find a last chunk.
        let mut iter = all_chunks.iter().filter(|meta| meta.next.is_none());
        let Some(last) = iter.next() else {
            return Err(EventCacheError::InvalidLinkedChunkMetadata {
                details: "no last chunk found".to_owned(),
            });
        };

        // There must at most one last chunk.
        if let Some(other_last) = iter.next() {
            return Err(EventCacheError::InvalidLinkedChunkMetadata {
                details: format!(
                    "chunks {} and {} both claim to be last chunks",
                    last.identifier.index(),
                    other_last.identifier.index()
                ),
            });
        }

        // Rewind the chain back to the first chunk, and do some checks at the same
        // time.
        let mut seen = HashSet::new();
        let mut current = last;
        loop {
            // If we've already seen this chunk, there's a cycle somewhere.
            if !seen.insert(current.identifier) {
                return Err(EventCacheError::InvalidLinkedChunkMetadata {
                    details: format!(
                        "cycle detected in linked chunk at {}",
                        current.identifier.index()
                    ),
                });
            }

            let Some(prev_id) = current.previous else {
                // If there's no previous chunk, we're done.
                if seen.len() != all_chunks.len() {
                    return Err(EventCacheError::InvalidLinkedChunkMetadata {
                        details: format!(
                            "linked chunk likely has multiple components: {} chunks seen through the chain of predecessors, but {} expected",
                            seen.len(),
                            all_chunks.len()
                        ),
                    });
                }
                break;
            };

            // If the previous chunk is not in the map, then it's unknown
            // and missing.
            let Some(pred_meta) = chunk_map.get(&prev_id) else {
                return Err(EventCacheError::InvalidLinkedChunkMetadata {
                    details: format!(
                        "missing predecessor {} chunk for {}",
                        prev_id.index(),
                        current.identifier.index()
                    ),
                });
            };

            // If the previous chunk isn't connected to the next, then the link is invalid.
            if pred_meta.next != Some(current.identifier) {
                return Err(EventCacheError::InvalidLinkedChunkMetadata {
                    details: format!(
                        "chunk {}'s next ({:?}) doesn't match the current chunk ({})",
                        pred_meta.identifier.index(),
                        pred_meta.next.map(|chunk_id| chunk_id.index()),
                        current.identifier.index()
                    ),
                });
            }

            current = *pred_meta;
        }

        // At this point, `current` is the identifier of the first chunk.
        //
        // Reorder the resulting vector, by going through the chain of `next` links, and
        // swapping items into their final position.
        //
        // Invariant in this loop: all items in [0..i[ are in their final, correct
        // position.
        let mut current = current.identifier;
        for i in 0..all_chunks.len() {
            // Find the target metadata.
            let j = all_chunks
                .iter()
                .rev()
                .position(|meta| meta.identifier == current)
                .map(|j| all_chunks.len() - 1 - j)
                .expect("the target chunk must be present in the metadata");
            if i != j {
                all_chunks.swap(i, j);
            }
            if let Some(next) = all_chunks[i].next {
                current = next;
            }
        }

        Ok(Some(all_chunks))
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
                *event = Raw::new(&val).ok()?.cast_unchecked();
            }
            None
        };
        let _ = closure();
    }

    fn strip_relations_from_event(ev: &mut Event) {
        match &mut ev.kind {
            TimelineEventKind::Decrypted(decrypted) => {
                // Remove all information about encryption info for
                // the bundled events.
                decrypted.unsigned_encryption_info = None;

                // Remove the `unsigned`/`m.relations` field, if needs be.
                strip_relations_if_present(&mut decrypted.event);
            }

            TimelineEventKind::UnableToDecrypt { event, .. }
            | TimelineEventKind::PlainText { event } => {
                strip_relations_if_present(event);
            }
        }
    }

    /// Strips the bundled relations from a collection of events.
    fn strip_relations_from_events(items: &mut [Event]) {
        for ev in items.iter_mut() {
            strip_relations_from_event(ev);
        }
    }

    /// Implementation of [`RoomEventCacheStateLockReadGuard::find_event`] and
    /// [`RoomEventCacheStateLockWriteGuard::find_event`].
    async fn find_event(
        event_id: &EventId,
        room_id: &RoomId,
        room_linked_chunk: &EventLinkedChunk,
        store: &EventCacheStoreLockGuard,
    ) -> Result<Option<(EventLocation, Event)>, EventCacheError> {
        // There are supposedly fewer events loaded in memory than in the store. Let's
        // start by looking up in the `EventLinkedChunk`.
        for (position, event) in room_linked_chunk.revents() {
            if event.event_id().as_deref() == Some(event_id) {
                return Ok(Some((EventLocation::Memory(position), event.clone())));
            }
        }

        Ok(store.find_event(room_id, event_id).await?.map(|event| (EventLocation::Store, event)))
    }

    /// Implementation of
    /// [`RoomEventCacheStateLockReadGuard::find_event_with_relations`] and
    /// [`RoomEventCacheStateLockWriteGuard::find_event_with_relations`].
    async fn find_event_with_relations(
        event_id: &EventId,
        room_id: &RoomId,
        filters: Option<Vec<RelationType>>,
        room_linked_chunk: &EventLinkedChunk,
        store: &EventCacheStoreLockGuard,
    ) -> Result<Option<(Event, Vec<Event>)>, EventCacheError> {
        // First, hit storage to get the target event and its related events.
        let found = store.find_event(room_id, event_id).await?;

        let Some(target) = found else {
            // We haven't found the event: return early.
            return Ok(None);
        };

        // Then, find the transitive closure of all the related events.
        let related =
            find_event_relations(event_id, room_id, filters, room_linked_chunk, store).await?;

        Ok(Some((target, related)))
    }

    /// Implementation of
    /// [`RoomEventCacheStateLockReadGuard::find_event_relations`].
    async fn find_event_relations(
        event_id: &EventId,
        room_id: &RoomId,
        filters: Option<Vec<RelationType>>,
        room_linked_chunk: &EventLinkedChunk,
        store: &EventCacheStoreLockGuard,
    ) -> Result<Vec<Event>, EventCacheError> {
        // Initialize the stack with all the related events, to find the
        // transitive closure of all the related events.
        let mut related = store.find_event_relations(room_id, event_id, filters.as_deref()).await?;
        let mut stack =
            related.iter().filter_map(|(event, _pos)| event.event_id()).collect::<Vec<_>>();

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
                store.find_event_relations(room_id, &event_id, filters.as_deref()).await?;

            stack.extend(other_related.iter().filter_map(|(event, _pos)| event.event_id()));
            related.extend(other_related);

            num_iters += 1;
        }

        trace!(num_related = %related.len(), num_iters, "computed transitive closure of related events");

        // Sort the results by their positions in the linked chunk, if available.
        //
        // If an event doesn't have a known position, it goes to the start of the array.
        related.sort_by(|(_, lhs), (_, rhs)| {
            use std::cmp::Ordering;

            match (lhs, rhs) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Less,
                (Some(_), None) => Ordering::Greater,
                (Some(lhs), Some(rhs)) => {
                    let lhs = room_linked_chunk.event_order(*lhs);
                    let rhs = room_linked_chunk.event_order(*rhs);

                    // The events should have a definite position, but in the case they don't,
                    // still consider that not having a position means you'll end at the start
                    // of the array.
                    match (lhs, rhs) {
                        (None, None) => Ordering::Equal,
                        (None, Some(_)) => Ordering::Less,
                        (Some(_), None) => Ordering::Greater,
                        (Some(lhs), Some(rhs)) => lhs.cmp(&rhs),
                    }
                }
            }
        });

        // Keep only the events, not their positions.
        let related = related.into_iter().map(|(event, _pos)| event).collect();

        Ok(related)
    }
}

/// An enum representing where an event has been found.
pub(super) enum EventLocation {
    /// Event lives in memory (and likely in the store!).
    Memory(Position),

    /// Event lives in the store only, it has not been loaded in memory yet.
    Store,
}

pub(super) use private::RoomEventCacheStateLock;

#[cfg(test)]
mod tests {
    use matrix_sdk_base::event_cache::Event;
    use matrix_sdk_test::{async_test, event_factory::EventFactory};
    use ruma::{
        RoomId, event_id,
        events::{relation::RelationType, room::message::RoomMessageEventContentWithoutRelation},
        room_id, user_id,
    };

    use crate::test_utils::logged_in_client;

    #[async_test]
    async fn test_find_event_by_id_with_edit_relation() {
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
    async fn test_find_event_by_id_with_thread_reply_relation() {
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
    async fn test_find_event_by_id_with_reaction_relation() {
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
    async fn test_find_event_by_id_with_poll_response_relation() {
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
    async fn test_find_event_by_id_with_poll_end_relation() {
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
    async fn test_find_event_by_id_with_filtered_relationships() {
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
        let (event, related_events) = room_event_cache
            .find_event_with_relations(original_id, filter)
            .await
            .expect("Failed to find the event with relations")
            .expect("Event has no relation");
        // Fetched event is the right one.
        let cached_event_id = event.event_id().unwrap();
        assert_eq!(cached_event_id, original_id);

        // There's only the edit event (an edit event can't have its own edit event).
        assert_eq!(related_events.len(), 1);

        let related_event_id = related_events[0].event_id().unwrap();
        assert_eq!(related_event_id, related_id);

        // Now we'll filter threads instead, there should be no related events
        let filter = Some(vec![RelationType::Thread]);
        let (event, related_events) = room_event_cache
            .find_event_with_relations(original_id, filter)
            .await
            .expect("Failed to find the event with relations")
            .expect("Event has no relation");

        // Fetched event is the right one.
        let cached_event_id = event.event_id().unwrap();
        assert_eq!(cached_event_id, original_id);
        // No Thread related events found
        assert!(related_events.is_empty());
    }

    #[async_test]
    async fn test_find_event_by_id_with_recursive_relation() {
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

        let (event, related_events) = room_event_cache
            .find_event_with_relations(original_id, None)
            .await
            .expect("Failed to find the event with relations")
            .expect("Event has no relation");
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

    async fn assert_relations(
        room_id: &RoomId,
        original_event: Event,
        related_event: Event,
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

        let (event, related_events) = room_event_cache
            .find_event_with_relations(&original_event_id, None)
            .await
            .expect("Failed to find the event with relations")
            .expect("Event has no relation");
        // Fetched event is the right one.
        let cached_event_id = event.event_id().unwrap();
        assert_eq!(cached_event_id, original_event_id);

        // There is only the actually related event in the related ones
        let related_event_id = related_events[0].event_id().unwrap();
        assert_eq!(related_event_id, related_id);
    }
}

#[cfg(all(test, not(target_family = "wasm")))] // This uses the cross-process lock, so needs time support.
mod timed_tests {
    use std::{ops::Not, sync::Arc};

    use assert_matches::assert_matches;
    use assert_matches2::assert_let;
    use eyeball_im::VectorDiff;
    use futures_util::FutureExt;
    use matrix_sdk_base::{
        event_cache::{
            Gap,
            store::{EventCacheStore as _, MemoryStore},
        },
        linked_chunk::{
            ChunkContent, ChunkIdentifier, LinkedChunkId, Position, Update,
            lazy_loader::from_all_chunks,
        },
        store::StoreConfig,
        sync::{JoinedRoomUpdate, Timeline},
    };
    use matrix_sdk_test::{ALICE, BOB, async_test, event_factory::EventFactory};
    use ruma::{
        EventId, OwnedUserId, event_id,
        events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent},
        room_id, user_id,
    };
    use tokio::task::yield_now;

    use super::RoomEventCacheGenericUpdate;
    use crate::{
        assert_let_timeout,
        event_cache::{RoomEventCache, RoomEventCacheUpdate, room::LoadMoreEventsBackwardsOutcome},
        test_utils::client::MockClientBuilder,
    };

    #[async_test]
    async fn test_write_to_storage() {
        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        let event_cache_store = Arc::new(MemoryStore::new());

        let client = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new("hodlor".to_owned())
                        .event_cache_store(event_cache_store.clone()),
                )
            })
            .build()
            .await;

        let event_cache = client.event_cache();

        // Don't forget to subscribe and like.
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();
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

        // Just checking the generic update is correct.
        assert_matches!(
            generic_stream.recv().await,
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) => {
                assert_eq!(expected_room_id, room_id);
            }
        );
        assert!(generic_stream.is_empty());

        // Check the storage.
        let linked_chunk = from_all_chunks::<3, _, _>(
            event_cache_store.load_all_chunks(LinkedChunkId::Room(room_id)).await.unwrap(),
        )
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

    #[async_test]
    async fn test_write_to_storage_strips_bundled_relations() {
        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        let event_cache_store = Arc::new(MemoryStore::new());

        let client = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new("hodlor".to_owned())
                        .event_cache_store(event_cache_store.clone()),
                )
            })
            .build()
            .await;

        let event_cache = client.event_cache();

        // Don't forget to subscribe and like.
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();
        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Propagate an update for a message with bundled relations.
        let ev = f
            .text_msg("hey yo")
            .sender(*ALICE)
            .with_bundled_edit(f.text_msg("Hello, Kind Sir").sender(*ALICE))
            .into_event();

        let timeline = Timeline { limited: false, prev_batch: None, events: vec![ev] };

        room_event_cache
            .inner
            .handle_joined_room_update(JoinedRoomUpdate { timeline, ..Default::default() })
            .await
            .unwrap();

        // Just checking the generic update is correct.
        assert_matches!(
            generic_stream.recv().await,
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) => {
                assert_eq!(expected_room_id, room_id);
            }
        );
        assert!(generic_stream.is_empty());

        // The in-memory linked chunk keeps the bundled relation.
        {
            let events = room_event_cache.events().await.unwrap();

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
        let linked_chunk = from_all_chunks::<3, _, _>(
            event_cache_store.load_all_chunks(LinkedChunkId::Room(room_id)).await.unwrap(),
        )
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

    #[async_test]
    async fn test_clear() {
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
                LinkedChunkId::Room(room_id),
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

        let client = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new("hodlor".to_owned())
                        .event_cache_store(event_cache_store.clone()),
                )
            })
            .build()
            .await;

        let event_cache = client.event_cache();

        // Don't forget to subscribe and like.
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        let (items, mut stream) = room_event_cache.subscribe().await.unwrap();
        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();

        // The rooms knows about all cached events.
        {
            assert!(room_event_cache.find_event(event_id1).await.unwrap().is_some());
            assert!(room_event_cache.find_event(event_id2).await.unwrap().is_some());
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

            assert_let_timeout!(
                Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) =
                    generic_stream.recv()
            );
            assert_eq!(room_id, expected_room_id);
            assert!(generic_stream.is_empty());
        }

        // After clearing,…
        room_event_cache.clear().await.unwrap();

        //… we get an update that the content has been cleared.
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. }) = stream.recv()
        );
        assert_eq!(diffs.len(), 1);
        assert_let!(VectorDiff::Clear = &diffs[0]);

        // … same with a generic update.
        assert_let_timeout!(
            Ok(RoomEventCacheGenericUpdate { room_id: received_room_id }) = generic_stream.recv()
        );
        assert_eq!(received_room_id, room_id);
        assert!(generic_stream.is_empty());

        // Events individually are not forgotten by the event cache, after clearing a
        // room.
        assert!(room_event_cache.find_event(event_id1).await.unwrap().is_some());

        // But their presence in a linked chunk is forgotten.
        let items = room_event_cache.events().await.unwrap();
        assert!(items.is_empty());

        // The event cache store too.
        let linked_chunk = from_all_chunks::<3, _, _>(
            event_cache_store.load_all_chunks(LinkedChunkId::Room(room_id)).await.unwrap(),
        )
        .unwrap()
        .unwrap();

        // Note: while the event cache store could return `None` here, clearing it will
        // reset it to its initial form, maintaining the invariant that it
        // contains a single items chunk that's empty.
        assert_eq!(linked_chunk.num_items(), 0);
    }

    #[async_test]
    async fn test_load_from_storage() {
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
                LinkedChunkId::Room(room_id),
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

        let client = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new("hodlor".to_owned())
                        .event_cache_store(event_cache_store.clone()),
                )
            })
            .build()
            .await;

        let event_cache = client.event_cache();

        // Don't forget to subscribe and like.
        event_cache.subscribe().unwrap();

        // Let's check whether the generic updates are received for the initialisation.
        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // The room event cache has been loaded. A generic update must have been
        // triggered.
        assert_matches!(
            generic_stream.recv().await,
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) => {
                assert_eq!(room_id, expected_room_id);
            }
        );
        assert!(generic_stream.is_empty());

        let (items, mut stream) = room_event_cache.subscribe().await.unwrap();

        // The initial items contain one event because only the last chunk is loaded by
        // default.
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].event_id().unwrap(), event_id2);
        assert!(stream.is_empty());

        // The event cache knows only all events though, even if they aren't loaded.
        assert!(room_event_cache.find_event(event_id1).await.unwrap().is_some());
        assert!(room_event_cache.find_event(event_id2).await.unwrap().is_some());

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

        // A generic update is triggered too.
        assert_matches!(
            generic_stream.recv().await,
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) => {
                assert_eq!(expected_room_id, room_id);
            }
        );
        assert!(generic_stream.is_empty());

        // A new update with one of these events leads to deduplication.
        let timeline = Timeline { limited: false, prev_batch: None, events: vec![ev2] };

        room_event_cache
            .inner
            .handle_joined_room_update(JoinedRoomUpdate { timeline, ..Default::default() })
            .await
            .unwrap();

        // Just checking the generic update is correct. There is a duplicate event, so
        // no generic changes whatsoever!
        assert!(generic_stream.recv().now_or_never().is_none());

        // The stream doesn't report these changes *yet*. Use the items vector given
        // when subscribing, to check that the items correspond to their new
        // positions. The duplicated item is removed (so it's not the first
        // element anymore), and it's added to the back of the list.
        let items = room_event_cache.events().await.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].event_id().unwrap(), event_id1);
        assert_eq!(items[1].event_id().unwrap(), event_id2);
    }

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
                LinkedChunkId::Room(room_id),
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

        let client = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new("holder".to_owned())
                        .event_cache_store(event_cache_store.clone()),
                )
            })
            .build()
            .await;

        let event_cache = client.event_cache();

        // Don't forget to subscribe and like.
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        let items = room_event_cache.events().await.unwrap();

        // Because the persisted content was invalid, the room store is reset: there are
        // no events in the cache.
        assert!(items.is_empty());

        // Storage doesn't contain anything. It would also be valid that it contains a
        // single initial empty items chunk.
        let raw_chunks =
            event_cache_store.load_all_chunks(LinkedChunkId::Room(room_id)).await.unwrap();
        assert!(raw_chunks.is_empty());
    }

    #[async_test]
    async fn test_no_useless_gaps() {
        let room_id = room_id!("!galette:saucisse.bzh");

        let client = MockClientBuilder::new(None).build().await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();
        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();

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

        // Just checking the generic update is correct.
        assert_matches!(
            generic_stream.recv().await,
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) => {
                assert_eq!(expected_room_id, room_id);
            }
        );
        assert!(generic_stream.is_empty());

        {
            let mut state = room_event_cache.inner.state.write().await.unwrap();

            let mut num_gaps = 0;
            let mut num_events = 0;

            for c in state.room_linked_chunk().chunks() {
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
            for c in state.room_linked_chunk().chunks() {
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

        // Just checking the generic update is correct.
        assert_matches!(
            generic_stream.recv().await,
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) => {
                assert_eq!(expected_room_id, room_id);
            }
        );
        assert!(generic_stream.is_empty());

        {
            let state = room_event_cache.inner.state.read().await.unwrap();

            let mut num_gaps = 0;
            let mut num_events = 0;

            for c in state.room_linked_chunk().chunks() {
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

    #[async_test]
    async fn test_shrink_to_last_chunk() {
        let room_id = room_id!("!galette:saucisse.bzh");

        let client = MockClientBuilder::new(None).build().await;

        let f = EventFactory::new().room(room_id);

        let evid1 = event_id!("$1");
        let evid2 = event_id!("$2");

        let ev1 = f.text_msg("hello world").sender(*ALICE).event_id(evid1).into_event();
        let ev2 = f.text_msg("howdy").sender(*BOB).event_id(evid2).into_event();

        // Fill the event cache store with an initial linked chunk with 2 events chunks.
        {
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
        let (events, mut stream) = room_event_cache.subscribe().await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_id().as_deref(), Some(evid2));
        assert!(stream.is_empty());

        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();

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

        // Same for the generic update.
        assert_let_timeout!(
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) = generic_stream.recv()
        );
        assert_eq!(expected_room_id, room_id);
        assert!(generic_stream.is_empty());

        // Shrink the linked chunk to the last chunk.
        let diffs = room_event_cache
            .inner
            .state
            .write()
            .await
            .unwrap()
            .force_shrink_to_last_chunk()
            .await
            .expect("shrinking should succeed");

        // We receive updates about the changes to the linked chunk.
        assert_eq!(diffs.len(), 2);
        assert_matches!(&diffs[0], VectorDiff::Clear);
        assert_matches!(&diffs[1], VectorDiff::Append { values} => {
            assert_eq!(values.len(), 1);
            assert_eq!(values[0].event_id().as_deref(), Some(evid2));
        });

        assert!(stream.is_empty());

        // No generic update is sent in this case.
        assert!(generic_stream.is_empty());

        // When reading the events, we do get only the last one.
        let events = room_event_cache.events().await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_id().as_deref(), Some(evid2));

        // But if we back-paginate, we don't need access to network to find out about
        // the previous event.
        let outcome = room_event_cache.pagination().run_backwards_once(20).await.unwrap();
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].event_id().as_deref(), Some(evid1));
        assert!(outcome.reached_start);
    }

    #[async_test]
    async fn test_room_ordering() {
        let room_id = room_id!("!galette:saucisse.bzh");

        let client = MockClientBuilder::new(None).build().await;

        let f = EventFactory::new().room(room_id).sender(*ALICE);

        let evid1 = event_id!("$1");
        let evid2 = event_id!("$2");
        let evid3 = event_id!("$3");

        let ev1 = f.text_msg("hello world").event_id(evid1).into_event();
        let ev2 = f.text_msg("howdy").sender(*BOB).event_id(evid2).into_event();
        let ev3 = f.text_msg("yo").event_id(evid3).into_event();

        // Fill the event cache store with an initial linked chunk with 2 events chunks.
        {
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
                            items: vec![ev1, ev2],
                        },
                        Update::NewItemsChunk {
                            previous: Some(ChunkIdentifier::new(0)),
                            new: ChunkIdentifier::new(1),
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(1), 0),
                            items: vec![ev3.clone()],
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

        // Initially, the linked chunk only contains the last chunk, so only ev3 is
        // loaded.
        {
            let state = room_event_cache.inner.state.read().await.unwrap();
            let room_linked_chunk = state.room_linked_chunk();

            // But we can get the order of ev1.
            assert_eq!(
                room_linked_chunk.event_order(Position::new(ChunkIdentifier::new(0), 0)),
                Some(0)
            );

            // And that of ev2 as well.
            assert_eq!(
                room_linked_chunk.event_order(Position::new(ChunkIdentifier::new(0), 1)),
                Some(1)
            );

            // ev3, which is loaded, also has a known ordering.
            let mut events = room_linked_chunk.events();
            let (pos, ev) = events.next().unwrap();
            assert_eq!(pos, Position::new(ChunkIdentifier::new(1), 0));
            assert_eq!(ev.event_id().as_deref(), Some(evid3));
            assert_eq!(room_linked_chunk.event_order(pos), Some(2));

            // No other loaded events.
            assert!(events.next().is_none());
        }

        // Force loading the full linked chunk by back-paginating.
        let outcome = room_event_cache.pagination().run_backwards_once(20).await.unwrap();
        assert!(outcome.reached_start);

        // All events are now loaded, so their order is precisely their enumerated index
        // in a linear iteration.
        {
            let state = room_event_cache.inner.state.read().await.unwrap();
            let room_linked_chunk = state.room_linked_chunk();

            for (i, (pos, _)) in room_linked_chunk.events().enumerate() {
                assert_eq!(room_linked_chunk.event_order(pos), Some(i));
            }
        }

        // Handle a gappy sync with two events (including one duplicate, so
        // deduplication kicks in), so that the linked chunk is shrunk to the
        // last chunk, and that the linked chunk only contains the last two
        // events.
        let evid4 = event_id!("$4");
        room_event_cache
            .inner
            .handle_joined_room_update(JoinedRoomUpdate {
                timeline: Timeline {
                    limited: true,
                    prev_batch: Some("fondue".to_owned()),
                    events: vec![ev3, f.text_msg("sup").event_id(evid4).into_event()],
                },
                ..Default::default()
            })
            .await
            .unwrap();

        {
            let state = room_event_cache.inner.state.read().await.unwrap();
            let room_linked_chunk = state.room_linked_chunk();

            // After the shrink, only evid3 and evid4 are loaded.
            let mut events = room_linked_chunk.events();

            let (pos, ev) = events.next().unwrap();
            assert_eq!(ev.event_id().as_deref(), Some(evid3));
            assert_eq!(room_linked_chunk.event_order(pos), Some(2));

            let (pos, ev) = events.next().unwrap();
            assert_eq!(ev.event_id().as_deref(), Some(evid4));
            assert_eq!(room_linked_chunk.event_order(pos), Some(3));

            // No other loaded events.
            assert!(events.next().is_none());

            // But we can still get the order of previous events.
            assert_eq!(
                room_linked_chunk.event_order(Position::new(ChunkIdentifier::new(0), 0)),
                Some(0)
            );
            assert_eq!(
                room_linked_chunk.event_order(Position::new(ChunkIdentifier::new(0), 1)),
                Some(1)
            );

            // ev3 doesn't have an order with its previous position, since it's been
            // deduplicated.
            assert_eq!(
                room_linked_chunk.event_order(Position::new(ChunkIdentifier::new(1), 0)),
                None
            );
        }
    }

    #[async_test]
    async fn test_auto_shrink_after_all_subscribers_are_gone() {
        let room_id = room_id!("!galette:saucisse.bzh");

        let client = MockClientBuilder::new(None).build().await;

        let f = EventFactory::new().room(room_id);

        let evid1 = event_id!("$1");
        let evid2 = event_id!("$2");

        let ev1 = f.text_msg("hello world").sender(*ALICE).event_id(evid1).into_event();
        let ev2 = f.text_msg("howdy").sender(*BOB).event_id(evid2).into_event();

        // Fill the event cache store with an initial linked chunk with 2 events chunks.
        {
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
        let (events1, mut stream1) = room_event_cache.subscribe().await.unwrap();
        assert_eq!(events1.len(), 1);
        assert_eq!(events1[0].event_id().as_deref(), Some(evid2));
        assert!(stream1.is_empty());

        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();

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

        assert_let_timeout!(
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) = generic_stream.recv()
        );
        assert_eq!(expected_room_id, room_id);
        assert!(generic_stream.is_empty());

        // Have another subscriber.
        // Since it's not the first one, and the previous one loaded some more events,
        // the second subscribers sees them all.
        let (events2, stream2) = room_event_cache.subscribe().await.unwrap();
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
            let state = room_event_cache.inner.state.read().await.unwrap();
            assert_eq!(state.subscriber_count().load(std::sync::atomic::Ordering::SeqCst), 0);
        }

        // Getting the events will only give us the latest chunk.
        let events3 = room_event_cache.events().await.unwrap();
        assert_eq!(events3.len(), 1);
        assert_eq!(events3[0].event_id().as_deref(), Some(evid2));
    }

    #[async_test]
    async fn test_rfind_map_event_in_memory_by() {
        let user_id = user_id!("@mnt_io:matrix.org");
        let room_id = room_id!("!raclette:patate.ch");
        let client = MockClientBuilder::new(None).build().await;

        let event_factory = EventFactory::new().room(room_id);

        let event_id_0 = event_id!("$ev0");
        let event_id_1 = event_id!("$ev1");
        let event_id_2 = event_id!("$ev2");
        let event_id_3 = event_id!("$ev3");

        let event_0 =
            event_factory.text_msg("hello").sender(*BOB).event_id(event_id_0).into_event();
        let event_1 =
            event_factory.text_msg("world").sender(*ALICE).event_id(event_id_1).into_event();
        let event_2 = event_factory.text_msg("!").sender(*ALICE).event_id(event_id_2).into_event();
        let event_3 =
            event_factory.text_msg("eh!").sender(user_id).event_id(event_id_3).into_event();

        // Fill the event cache store with an initial linked chunk of 2 chunks, and 4
        // events.
        {
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
                            items: vec![event_3],
                        },
                        Update::NewItemsChunk {
                            previous: Some(ChunkIdentifier::new(0)),
                            new: ChunkIdentifier::new(1),
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(1), 0),
                            items: vec![event_0, event_1, event_2],
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

        // Look for an event from `BOB`: it must be `event_0`.
        assert_matches!(
            room_event_cache
                .rfind_map_event_in_memory_by(|event, previous_event| {
                    (event.raw().get_field::<OwnedUserId>("sender").unwrap().as_deref() == Some(*BOB)).then(|| (event.event_id(), previous_event.and_then(|event| event.event_id())))
                })
                .await,
            Ok(Some((event_id, previous_event_id))) => {
                assert_eq!(event_id.as_deref(), Some(event_id_0));
                assert!(previous_event_id.is_none());
            }
        );

        // Look for an event from `ALICE`: it must be `event_2`, right before `event_1`
        // because events are looked for in reverse order.
        assert_matches!(
            room_event_cache
                .rfind_map_event_in_memory_by(|event, previous_event| {
                    (event.raw().get_field::<OwnedUserId>("sender").unwrap().as_deref() == Some(*ALICE)).then(|| (event.event_id(), previous_event.and_then(|event| event.event_id())))
                })
                .await,
            Ok(Some((event_id, previous_event_id))) => {
                assert_eq!(event_id.as_deref(), Some(event_id_2));
                assert_eq!(previous_event_id.as_deref(), Some(event_id_1));
            }
        );

        // Look for an event that is inside the storage, but not loaded.
        assert!(
            room_event_cache
                .rfind_map_event_in_memory_by(|event, _| {
                    (event.raw().get_field::<OwnedUserId>("sender").unwrap().as_deref()
                        == Some(user_id))
                    .then(|| event.event_id())
                })
                .await
                .unwrap()
                .is_none()
        );

        // Look for an event that doesn't exist.
        assert!(
            room_event_cache
                .rfind_map_event_in_memory_by(|_, _| None::<()>)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[async_test]
    async fn test_reload_when_dirty() {
        let user_id = user_id!("@mnt_io:matrix.org");
        let room_id = room_id!("!raclette:patate.ch");

        // The storage shared by the two clients.
        let event_cache_store = MemoryStore::new();

        // Client for the process 0.
        let client_p0 = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new("process #0".to_owned())
                        .event_cache_store(event_cache_store.clone()),
                )
            })
            .build()
            .await;

        // Client for the process 1.
        let client_p1 = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new("process #1".to_owned()).event_cache_store(event_cache_store),
                )
            })
            .build()
            .await;

        let event_factory = EventFactory::new().room(room_id).sender(user_id);

        let ev_id_0 = event_id!("$ev_0");
        let ev_id_1 = event_id!("$ev_1");

        let ev_0 = event_factory.text_msg("comté").event_id(ev_id_0).into_event();
        let ev_1 = event_factory.text_msg("morbier").event_id(ev_id_1).into_event();

        // Add events to the storage (shared by the two clients!).
        client_p0
            .event_cache_store()
            .lock()
            .await
            .expect("[p0] Could not acquire the event cache lock")
            .as_clean()
            .expect("[p0] Could not acquire a clean event cache lock")
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
                        items: vec![ev_0],
                    },
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(0)),
                        new: ChunkIdentifier::new(1),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(1), 0),
                        items: vec![ev_1],
                    },
                ],
            )
            .await
            .unwrap();

        // Subscribe the event caches, and create the room.
        let (room_event_cache_p0, room_event_cache_p1) = {
            let event_cache_p0 = client_p0.event_cache();
            event_cache_p0.subscribe().unwrap();

            let event_cache_p1 = client_p1.event_cache();
            event_cache_p1.subscribe().unwrap();

            client_p0.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
            client_p1.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);

            let (room_event_cache_p0, _drop_handles) =
                client_p0.get_room(room_id).unwrap().event_cache().await.unwrap();
            let (room_event_cache_p1, _drop_handles) =
                client_p1.get_room(room_id).unwrap().event_cache().await.unwrap();

            (room_event_cache_p0, room_event_cache_p1)
        };

        // Okay. We are ready for the test!
        //
        // First off, let's check `room_event_cache_p0` has access to the first event
        // loaded in-memory, then do a pagination, and see more events.
        let mut updates_stream_p0 = {
            let room_event_cache = &room_event_cache_p0;

            let (initial_updates, mut updates_stream) =
                room_event_cache_p0.subscribe().await.unwrap();

            // Initial updates contain `ev_id_1` only.
            assert_eq!(initial_updates.len(), 1);
            assert_eq!(initial_updates[0].event_id().as_deref(), Some(ev_id_1));
            assert!(updates_stream.is_empty());

            // `ev_id_1` must be loaded in memory.
            assert!(event_loaded(room_event_cache, ev_id_1).await);

            // `ev_id_0` must NOT be loaded in memory.
            assert!(event_loaded(room_event_cache, ev_id_0).await.not());

            // Load one more event with a backpagination.
            room_event_cache.pagination().run_backwards_once(1).await.unwrap();

            // A new update for `ev_id_0` must be present.
            assert_matches!(
                updates_stream.recv().await.unwrap(),
                RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
                    assert_eq!(diffs.len(), 1, "{diffs:#?}");
                    assert_matches!(
                        &diffs[0],
                        VectorDiff::Insert { index: 0, value: event } => {
                            assert_eq!(event.event_id().as_deref(), Some(ev_id_0));
                        }
                    );
                }
            );

            // `ev_id_0` must now be loaded in memory.
            assert!(event_loaded(room_event_cache, ev_id_0).await);

            updates_stream
        };

        // Second, let's check `room_event_cache_p1` has the same accesses.
        let mut updates_stream_p1 = {
            let room_event_cache = &room_event_cache_p1;
            let (initial_updates, mut updates_stream) =
                room_event_cache_p1.subscribe().await.unwrap();

            // Initial updates contain `ev_id_1` only.
            assert_eq!(initial_updates.len(), 1);
            assert_eq!(initial_updates[0].event_id().as_deref(), Some(ev_id_1));
            assert!(updates_stream.is_empty());

            // `ev_id_1` must be loaded in memory.
            assert!(event_loaded(room_event_cache, ev_id_1).await);

            // `ev_id_0` must NOT be loaded in memory.
            assert!(event_loaded(room_event_cache, ev_id_0).await.not());

            // Load one more event with a backpagination.
            room_event_cache.pagination().run_backwards_once(1).await.unwrap();

            // A new update for `ev_id_0` must be present.
            assert_matches!(
                updates_stream.recv().await.unwrap(),
                RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
                    assert_eq!(diffs.len(), 1, "{diffs:#?}");
                    assert_matches!(
                        &diffs[0],
                        VectorDiff::Insert { index: 0, value: event } => {
                            assert_eq!(event.event_id().as_deref(), Some(ev_id_0));
                        }
                    );
                }
            );

            // `ev_id_0` must now be loaded in memory.
            assert!(event_loaded(room_event_cache, ev_id_0).await);

            updates_stream
        };

        // Do this a couple times, for the fun.
        for _ in 0..3 {
            // Third, because `room_event_cache_p1` has locked the store, the lock
            // is dirty for `room_event_cache_p0`, so it will shrink to its last
            // chunk!
            {
                let room_event_cache = &room_event_cache_p0;
                let updates_stream = &mut updates_stream_p0;

                // `ev_id_1` must be loaded in memory, just like before.
                assert!(event_loaded(room_event_cache, ev_id_1).await);

                // However, `ev_id_0` must NOT be loaded in memory. It WAS loaded, but the
                // state has been reloaded to its last chunk.
                assert!(event_loaded(room_event_cache, ev_id_0).await.not());

                // The reload can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
                        assert_eq!(diffs.len(), 2, "{diffs:#?}");
                        assert_matches!(&diffs[0], VectorDiff::Clear);
                        assert_matches!(
                            &diffs[1],
                            VectorDiff::Append { values: events } => {
                                assert_eq!(events.len(), 1);
                                assert_eq!(events[0].event_id().as_deref(), Some(ev_id_1));
                            }
                        );
                    }
                );

                // Load one more event with a backpagination.
                room_event_cache.pagination().run_backwards_once(1).await.unwrap();

                // `ev_id_0` must now be loaded in memory.
                assert!(event_loaded(room_event_cache, ev_id_0).await);

                // The pagination can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
                        assert_eq!(diffs.len(), 1, "{diffs:#?}");
                        assert_matches!(
                            &diffs[0],
                            VectorDiff::Insert { index: 0, value: event } => {
                                assert_eq!(event.event_id().as_deref(), Some(ev_id_0));
                            }
                        );
                    }
                );
            }

            // Fourth, because `room_event_cache_p0` has locked the store again, the lock
            // is dirty for `room_event_cache_p1` too!, so it will shrink to its last
            // chunk!
            {
                let room_event_cache = &room_event_cache_p1;
                let updates_stream = &mut updates_stream_p1;

                // `ev_id_1` must be loaded in memory, just like before.
                assert!(event_loaded(room_event_cache, ev_id_1).await);

                // However, `ev_id_0` must NOT be loaded in memory. It WAS loaded, but the
                // state has shrunk to its last chunk.
                assert!(event_loaded(room_event_cache, ev_id_0).await.not());

                // The reload can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
                        assert_eq!(diffs.len(), 2, "{diffs:#?}");
                        assert_matches!(&diffs[0], VectorDiff::Clear);
                        assert_matches!(
                            &diffs[1],
                            VectorDiff::Append { values: events } => {
                                assert_eq!(events.len(), 1);
                                assert_eq!(events[0].event_id().as_deref(), Some(ev_id_1));
                            }
                        );
                    }
                );

                // Load one more event with a backpagination.
                room_event_cache.pagination().run_backwards_once(1).await.unwrap();

                // `ev_id_0` must now be loaded in memory.
                assert!(event_loaded(room_event_cache, ev_id_0).await);

                // The pagination can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
                        assert_eq!(diffs.len(), 1, "{diffs:#?}");
                        assert_matches!(
                            &diffs[0],
                            VectorDiff::Insert { index: 0, value: event } => {
                                assert_eq!(event.event_id().as_deref(), Some(ev_id_0));
                            }
                        );
                    }
                );
            }
        }

        // Repeat that with an explicit read lock (so that we don't rely on
        // `event_loaded` to trigger the dirty detection).
        for _ in 0..3 {
            {
                let room_event_cache = &room_event_cache_p0;
                let updates_stream = &mut updates_stream_p0;

                let guard = room_event_cache.inner.state.read().await.unwrap();

                // Guard is kept alive, to ensure we can have multiple read guards alive with a
                // shared access.
                // See `RoomEventCacheStateLock::read` to learn more.

                // The lock is no longer marked as dirty, it's been cleaned.
                assert!(guard.is_dirty().not());

                // The reload can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
                        assert_eq!(diffs.len(), 2, "{diffs:#?}");
                        assert_matches!(&diffs[0], VectorDiff::Clear);
                        assert_matches!(
                            &diffs[1],
                            VectorDiff::Append { values: events } => {
                                assert_eq!(events.len(), 1);
                                assert_eq!(events[0].event_id().as_deref(), Some(ev_id_1));
                            }
                        );
                    }
                );

                assert!(event_loaded(room_event_cache, ev_id_1).await);
                assert!(event_loaded(room_event_cache, ev_id_0).await.not());

                // Ensure `guard` is alive up to this point (in case this test is refactored, I
                // want to make this super explicit).
                //
                // We drop need to drop it before the pagination because the pagination needs to
                // obtain a write lock.
                drop(guard);

                room_event_cache.pagination().run_backwards_once(1).await.unwrap();
                assert!(event_loaded(room_event_cache, ev_id_0).await);

                // The pagination can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
                        assert_eq!(diffs.len(), 1, "{diffs:#?}");
                        assert_matches!(
                            &diffs[0],
                            VectorDiff::Insert { index: 0, value: event } => {
                                assert_eq!(event.event_id().as_deref(), Some(ev_id_0));
                            }
                        );
                    }
                );
            }

            {
                let room_event_cache = &room_event_cache_p1;
                let updates_stream = &mut updates_stream_p1;

                let guard = room_event_cache.inner.state.read().await.unwrap();

                // Guard is kept alive, to ensure we can have multiple read guards alive with a
                // shared access.

                // The lock is no longer marked as dirty, it's been cleaned.
                assert!(guard.is_dirty().not());

                // The reload can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
                        assert_eq!(diffs.len(), 2, "{diffs:#?}");
                        assert_matches!(&diffs[0], VectorDiff::Clear);
                        assert_matches!(
                            &diffs[1],
                            VectorDiff::Append { values: events } => {
                                assert_eq!(events.len(), 1);
                                assert_eq!(events[0].event_id().as_deref(), Some(ev_id_1));
                            }
                        );
                    }
                );

                assert!(event_loaded(room_event_cache, ev_id_1).await);
                assert!(event_loaded(room_event_cache, ev_id_0).await.not());

                // Ensure `guard` is alive up to this point (in case this test is refactored, I
                // want to make this super explicit).
                //
                // We drop need to drop it before the pagination because the pagination needs to
                // obtain a write lock.
                drop(guard);

                room_event_cache.pagination().run_backwards_once(1).await.unwrap();
                assert!(event_loaded(room_event_cache, ev_id_0).await);

                // The pagination can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
                        assert_eq!(diffs.len(), 1, "{diffs:#?}");
                        assert_matches!(
                            &diffs[0],
                            VectorDiff::Insert { index: 0, value: event } => {
                                assert_eq!(event.event_id().as_deref(), Some(ev_id_0));
                            }
                        );
                    }
                );
            }
        }

        // Repeat that with an explicit write lock.
        for _ in 0..3 {
            {
                let room_event_cache = &room_event_cache_p0;
                let updates_stream = &mut updates_stream_p0;

                let guard = room_event_cache.inner.state.write().await.unwrap();

                // The lock is no longer marked as dirty, it's been cleaned.
                assert!(guard.is_dirty().not());

                // The reload can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
                        assert_eq!(diffs.len(), 2, "{diffs:#?}");
                        assert_matches!(&diffs[0], VectorDiff::Clear);
                        assert_matches!(
                            &diffs[1],
                            VectorDiff::Append { values: events } => {
                                assert_eq!(events.len(), 1);
                                assert_eq!(events[0].event_id().as_deref(), Some(ev_id_1));
                            }
                        );
                    }
                );

                // Guard isn't kept alive, otherwise `event_loaded` couldn't run because it
                // needs to obtain a read lock.
                drop(guard);

                assert!(event_loaded(room_event_cache, ev_id_1).await);
                assert!(event_loaded(room_event_cache, ev_id_0).await.not());

                room_event_cache.pagination().run_backwards_once(1).await.unwrap();
                assert!(event_loaded(room_event_cache, ev_id_0).await);

                // The pagination can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
                        assert_eq!(diffs.len(), 1, "{diffs:#?}");
                        assert_matches!(
                            &diffs[0],
                            VectorDiff::Insert { index: 0, value: event } => {
                                assert_eq!(event.event_id().as_deref(), Some(ev_id_0));
                            }
                        );
                    }
                );
            }

            {
                let room_event_cache = &room_event_cache_p1;
                let updates_stream = &mut updates_stream_p1;

                let guard = room_event_cache.inner.state.write().await.unwrap();

                // The lock is no longer marked as dirty, it's been cleaned.
                assert!(guard.is_dirty().not());

                // The reload can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
                        assert_eq!(diffs.len(), 2, "{diffs:#?}");
                        assert_matches!(&diffs[0], VectorDiff::Clear);
                        assert_matches!(
                            &diffs[1],
                            VectorDiff::Append { values: events } => {
                                assert_eq!(events.len(), 1);
                                assert_eq!(events[0].event_id().as_deref(), Some(ev_id_1));
                            }
                        );
                    }
                );

                // Guard isn't kept alive, otherwise `event_loaded` couldn't run because it
                // needs to obtain a read lock.
                drop(guard);

                assert!(event_loaded(room_event_cache, ev_id_1).await);
                assert!(event_loaded(room_event_cache, ev_id_0).await.not());

                room_event_cache.pagination().run_backwards_once(1).await.unwrap();
                assert!(event_loaded(room_event_cache, ev_id_0).await);

                // The pagination can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents { diffs, .. } => {
                        assert_eq!(diffs.len(), 1, "{diffs:#?}");
                        assert_matches!(
                            &diffs[0],
                            VectorDiff::Insert { index: 0, value: event } => {
                                assert_eq!(event.event_id().as_deref(), Some(ev_id_0));
                            }
                        );
                    }
                );
            }
        }
    }

    #[async_test]
    async fn test_load_when_dirty() {
        let room_id_0 = room_id!("!raclette:patate.ch");
        let room_id_1 = room_id!("!morbiflette:patate.ch");

        // The storage shared by the two clients.
        let event_cache_store = MemoryStore::new();

        // Client for the process 0.
        let client_p0 = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new("process #0".to_owned())
                        .event_cache_store(event_cache_store.clone()),
                )
            })
            .build()
            .await;

        // Client for the process 1.
        let client_p1 = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new("process #1".to_owned()).event_cache_store(event_cache_store),
                )
            })
            .build()
            .await;

        // Subscribe the event caches, and create the room.
        let (room_event_cache_0_p0, room_event_cache_0_p1) = {
            let event_cache_p0 = client_p0.event_cache();
            event_cache_p0.subscribe().unwrap();

            let event_cache_p1 = client_p1.event_cache();
            event_cache_p1.subscribe().unwrap();

            client_p0
                .base_client()
                .get_or_create_room(room_id_0, matrix_sdk_base::RoomState::Joined);
            client_p0
                .base_client()
                .get_or_create_room(room_id_1, matrix_sdk_base::RoomState::Joined);

            client_p1
                .base_client()
                .get_or_create_room(room_id_0, matrix_sdk_base::RoomState::Joined);
            client_p1
                .base_client()
                .get_or_create_room(room_id_1, matrix_sdk_base::RoomState::Joined);

            let (room_event_cache_0_p0, _drop_handles) =
                client_p0.get_room(room_id_0).unwrap().event_cache().await.unwrap();
            let (room_event_cache_0_p1, _drop_handles) =
                client_p1.get_room(room_id_0).unwrap().event_cache().await.unwrap();

            (room_event_cache_0_p0, room_event_cache_0_p1)
        };

        // Let's make the cross-process lock over the store dirty.
        {
            drop(room_event_cache_0_p0.inner.state.read().await.unwrap());
            drop(room_event_cache_0_p1.inner.state.read().await.unwrap());
        }

        // Create the `RoomEventCache` for `room_id_1`. During its creation, the
        // cross-process lock over the store MUST be dirty, which makes no difference as
        // a clean one: the state is just loaded, not reloaded.
        let (room_event_cache_1_p0, _) =
            client_p0.get_room(room_id_1).unwrap().event_cache().await.unwrap();

        // Check the lock isn't dirty because it's been cleared.
        {
            let guard = room_event_cache_1_p0.inner.state.read().await.unwrap();
            assert!(guard.is_dirty().not());
        }

        // The only way to test this behaviour is to see that the dirty block in
        // `RoomEventCacheStateLock` is covered by this test.
    }

    async fn event_loaded(room_event_cache: &RoomEventCache, event_id: &EventId) -> bool {
        room_event_cache
            .rfind_map_event_in_memory_by(|event, _previous_event_id| {
                (event.event_id().as_deref() == Some(event_id)).then_some(())
            })
            .await
            .unwrap()
            .is_some()
    }
}
