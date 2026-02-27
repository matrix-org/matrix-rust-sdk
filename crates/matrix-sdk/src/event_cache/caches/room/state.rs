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

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use eyeball::SharedObservable;
use eyeball_im::VectorDiff;
use matrix_sdk_base::{
    apply_redaction,
    deserialized_responses::{ThreadSummary, ThreadSummaryStatus},
    event_cache::{
        Event, Gap,
        store::{EventCacheStoreLock, EventCacheStoreLockGuard, EventCacheStoreLockState},
    },
    linked_chunk::{
        ChunkContent, ChunkIdentifierGenerator, ChunkMetadata, LinkedChunkId, OwnedLinkedChunkId,
        Position, Update, lazy_loader,
    },
    serde_helpers::{extract_edit_target, extract_thread_root},
    sync::Timeline,
};
use matrix_sdk_common::executor::spawn;
use ruma::{
    EventId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId,
    events::{
        AnySyncMessageLikeEvent, AnySyncTimelineEvent, MessageLikeEventType,
        relation::RelationType, room::redaction::SyncRoomRedactionEvent,
    },
    room_version_rules::RoomVersionRules,
};
use tokio::sync::broadcast::{Receiver, Sender};
use tracing::{debug, error, instrument, trace, warn};

use super::{
    super::{
        super::{
            EventCacheError, PaginationStatus,
            deduplicator::{DeduplicationOutcome, filter_duplicate_events},
            persistence::send_updates_to_store,
        },
        TimelineVectorDiffs,
        event_linked_chunk::EventLinkedChunk,
        lock,
        pagination::LoadMoreEventsBackwardsOutcome,
        pinned_events::PinnedEventCache,
        thread::ThreadEventCache,
    },
    EventLocation, EventsOrigin, PostProcessingOrigin, RoomEventCacheGenericUpdate,
    RoomEventCacheLinkedChunkUpdate, RoomEventCacheUpdate, RoomEventCacheUpdateSender,
    sort_positions_descending,
};
use crate::Room;

pub struct RoomEventCacheState {
    /// Whether thread support has been enabled for the event cache.
    enabled_thread_support: bool,

    /// The room this state relates to.
    pub room_id: OwnedRoomId,

    /// The user's own user id.
    pub own_user_id: OwnedUserId,

    /// Reference to the underlying backing store.
    store: EventCacheStoreLock,

    /// The loaded events for the current room, that is, the in-memory
    /// linked chunk for this room.
    room_linked_chunk: EventLinkedChunk,

    /// Threads present in this room.
    ///
    /// Keyed by the thread root event ID.
    threads: HashMap<OwnedEventId, ThreadEventCache>,

    /// Cache for pinned events in this room, initialized on-demand.
    pinned_event_cache: OnceLock<PinnedEventCache>,

    pagination_status: SharedObservable<PaginationStatus>,

    /// A clone of [`super::RoomEventCacheInner::update_sender`].
    ///
    /// This is used only by the [`RoomEventCacheStateLock::read`] and
    /// [`RoomEventCacheStateLock::write`] when the state must be reset.
    update_sender: RoomEventCacheUpdateSender,

    /// A clone of
    /// [`super::super::EventCacheInner::linked_chunk_update_sender`].
    linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,

    /// The rules for the version of this room.
    room_version_rules: RoomVersionRules,

    /// Have we ever waited for a previous-batch-token to come from sync, in
    /// the context of pagination? We do this at most once per room,
    /// the first time we try to run backward pagination. We reset
    /// that upon clearing the timeline events.
    waited_for_initial_prev_token: bool,

    /// An atomic count of the current number of subscriber of the
    /// [`super::RoomEventCache`].
    subscriber_count: Arc<AtomicUsize>,
}

impl lock::Store for RoomEventCacheState {
    fn store(&self) -> &EventCacheStoreLock {
        &self.store
    }
}

/// State for a single room's event cache.
///
/// This contains all the inner mutable states that ought to be updated at
/// the same time.
pub type RoomEventCacheStateLock = lock::StateLock<RoomEventCacheState>;

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
        own_user_id: OwnedUserId,
        room_id: OwnedRoomId,
        room_version_rules: RoomVersionRules,
        enabled_thread_support: bool,
        update_sender: RoomEventCacheUpdateSender,
        linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,
        store: EventCacheStoreLock,
        pagination_status: SharedObservable<PaginationStatus>,
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
                    error!("error when loading a linked chunk's metadata from the store: {err}");

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
                error!("error when loading a linked chunk's latest chunk from the store: {err}");

                // Try to clear storage for this room.
                store_guard
                    .handle_linked_chunk_updates(linked_chunk_id, vec![Update::Clear])
                    .await?;

                None
            }
        };

        Ok(Self::new_inner(RoomEventCacheState {
            own_user_id,
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
            linked_chunk_update_sender,
            room_version_rules,
            waited_for_initial_prev_token: false,
            subscriber_count: Default::default(),
            pinned_event_cache: OnceLock::new(),
        }))
    }
}

/// The read-lock guard around [`RoomEventCacheState`].
///
/// See [`RoomEventCacheStateLock::read`] to acquire it.
pub type RoomEventCacheStateLockReadGuard<'a> = lock::StateLockReadGuard<'a, RoomEventCacheState>;

/// The write-lock guard around [`RoomEventCacheState`].
///
/// See [`RoomEventCacheStateLock::write`] to acquire it.
pub type RoomEventCacheStateLockWriteGuard<'a> = lock::StateLockWriteGuard<'a, RoomEventCacheState>;

impl<'a> lock::Reload for RoomEventCacheStateLockWriteGuard<'a> {
    /// Force to shrink the room, whenever there is subscribers or not.
    async fn reload(&mut self) -> Result<(), EventCacheError> {
        self.shrink_to_last_chunk().await?;

        let diffs = self.state.room_linked_chunk.updates_as_vector_diffs();

        // Notify observers about the update.
        self.state.update_sender.send(
            RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs {
                diffs,
                origin: EventsOrigin::Cache,
            }),
            Some(RoomEventCacheGenericUpdate { room_id: self.state.room_id.clone() }),
        );

        Ok(())
    }
}

impl<'a> RoomEventCacheStateLockReadGuard<'a> {
    /// Return a read-only reference to the underlying room linked chunk.
    pub fn room_linked_chunk(&self) -> &EventLinkedChunk {
        &self.state.room_linked_chunk
    }

    /// Return the subscriber count.
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
        find_event(event_id, &self.state.room_id, &self.state.room_linked_chunk, &self.store).await
    }

    /// Find an event and all its relations in the persisted storage.
    ///
    /// This goes straight to the database, as a simplification; we don't
    /// expect to need to have to look up in memory events, or that
    /// all the related events are actually loaded.
    ///
    /// The related events are sorted like this:
    /// - events saved out-of-band with [`super::RoomEventCache::save_events`]
    ///   will be located at the beginning of the array.
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
    /// - events saved out-of-band with [`super::RoomEventCache::save_events`]
    ///   will be located at the beginning of the array.
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
    /// The `predicate` receives the current event as its single argument.
    ///
    /// **Warning**! It looks into the loaded events from the in-memory
    /// linked chunk **only**. It doesn't look inside the storage,
    /// contrary to [`Self::find_event`].
    pub fn rfind_map_event_in_memory_by<O, P>(&self, mut predicate: P) -> Option<O>
    where
        P: FnMut(&Event) -> Option<O>,
    {
        self.state.room_linked_chunk.revents().find_map(|(_, event)| predicate(event))
    }

    #[cfg(test)]
    pub fn is_dirty(&self) -> bool {
        EventCacheStoreLockGuard::is_dirty(&self.store)
    }

    /// Subscribe to the lazily initialized pinned event cache for this
    /// room.
    ///
    /// This is a persisted view over the pinned events of a room. The
    /// pinned events will be initially loaded from a network
    /// request to fetch the latest pinned events will be performed,
    /// to update it as needed. The list of pinned events will also
    /// be kept up-to-date as new events are pinned, and new related
    /// events show up from sync or backpagination.
    ///
    /// This requires the room's event cache to be initialized.
    pub async fn subscribe_to_pinned_events(
        &self,
        room: Room,
    ) -> Result<(Vec<Event>, Receiver<TimelineVectorDiffs>), EventCacheError> {
        let pinned_event_cache = self.state.pinned_event_cache.get_or_init(|| {
            PinnedEventCache::new(
                room,
                self.state.linked_chunk_update_sender.clone(),
                self.state.store.clone(),
            )
        });

        pinned_event_cache.subscribe().await
    }
}

impl<'a> RoomEventCacheStateLockWriteGuard<'a> {
    /// Return an immutable reference to the underlying room linked chunk.
    pub fn room_linked_chunk(&self) -> &EventLinkedChunk {
        &self.state.room_linked_chunk
    }

    /// Return a mutable reference to the underlying room linked chunk.
    pub fn room_linked_chunk_mut(&mut self) -> &mut EventLinkedChunk {
        &mut self.state.room_linked_chunk
    }

    /// Get a reference to the [`pinned_event_cache`] if it has been
    /// initialized.
    #[cfg(any(feature = "e2e-encryption", test))]
    pub fn pinned_event_cache(&self) -> Option<&PinnedEventCache> {
        self.state.pinned_event_cache.get()
    }

    /// Get the `waited_for_initial_prev_token` value.
    pub fn waited_for_initial_prev_token(&mut self) -> &mut bool {
        &mut self.state.waited_for_initial_prev_token
    }

    /// Find a single event in this room.
    ///
    /// It starts by looking into loaded events in `EventLinkedChunk` before
    /// looking inside the storage.
    pub async fn find_event(
        &self,
        event_id: &EventId,
    ) -> Result<Option<(EventLocation, Event)>, EventCacheError> {
        find_event(event_id, &self.state.room_id, &self.state.room_linked_chunk, &self.store).await
    }

    /// Find an event and all its relations in the persisted storage.
    ///
    /// This goes straight to the database, as a simplification; we don't
    /// expect to need to have to look up in memory events, or that
    /// all the related events are actually loaded.
    ///
    /// The related events are sorted like this:
    /// - events saved out-of-band with [`super::RoomEventCache::save_events`]
    ///   will be located at the beginning of the array.
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
        if let Some(prev_token) = self.state.room_linked_chunk.rgap().map(|gap| gap.prev_token) {
            return Ok(LoadMoreEventsBackwardsOutcome::Gap {
                prev_token: Some(prev_token),
                waited_for_initial_prev_token: self.state.waited_for_initial_prev_token,
            });
        }

        let prev_first_chunk =
            self.state.room_linked_chunk.chunks().next().expect("a linked chunk is never empty");

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
                return Ok(LoadMoreEventsBackwardsOutcome::Gap {
                    prev_token: None,
                    waited_for_initial_prev_token: self.state.waited_for_initial_prev_token,
                });
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

        if let Err(err) = self.state.room_linked_chunk.insert_new_chunk_as_first(new_first_chunk) {
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
                LoadMoreEventsBackwardsOutcome::Gap {
                    prev_token: Some(gap.prev_token),
                    waited_for_initial_prev_token: self.state.waited_for_initial_prev_token,
                }
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
        self.state.pagination_status.set(PaginationStatus::Idle { hit_timeline_start: false });

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
        updates: Vec<Update<Event, Gap>>,
    ) -> Result<(), EventCacheError> {
        let linked_chunk_id = OwnedLinkedChunkId::Room(self.state.room_id.clone());
        send_updates_to_store(
            &self.store,
            linked_chunk_id,
            &self.state.linked_chunk_update_sender,
            updates,
        )
        .await
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
        self.state.waited_for_initial_prev_token = false;

        // TODO: likely must cancel any ongoing back-paginations too
        self.state.pagination_status.set(PaginationStatus::Idle { hit_timeline_start: false });

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
            &self.state.own_user_id,
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
        if !self.state.waited_for_initial_prev_token && has_new_gap {
            self.state.waited_for_initial_prev_token = true;
        }

        // Remove the old duplicated events.
        //
        // We don't have to worry the removals can change the position of the existing
        // events, because we are pushing all _new_ `events` at the back.
        self.remove_events(in_memory_duplicated_event_ids, in_store_duplicated_event_ids).await?;

        self.state
            .room_linked_chunk
            .push_live_events(prev_batch.map(|prev_token| Gap { prev_token }), &events);

        self.post_process_new_events(events, PostProcessingOrigin::Sync).await?;

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

    /// Subscribe to thread for a given root event, and get a (maybe empty)
    /// initially known list of events for that thread.
    pub fn subscribe_to_thread(
        &mut self,
        root: OwnedEventId,
    ) -> (Vec<Event>, Receiver<TimelineVectorDiffs>) {
        self.get_or_reload_thread(root).subscribe()
    }

    // --------------------------------------------
    // utility methods
    // --------------------------------------------

    /// Post-process new events, after they have been added to the in-memory
    /// linked chunk.
    ///
    /// Flushes updates to disk first.
    pub async fn post_process_new_events(
        &mut self,
        events: Vec<Event>,
        post_processing_origin: PostProcessingOrigin,
    ) -> Result<(), EventCacheError> {
        // Update the store before doing the post-processing.
        self.propagate_changes().await?;

        // Need an explicit re-borrow to avoid a deref vs deref-mut borrowck conflict
        // below.
        let state = &mut *self.state;

        if let Some(pinned_event_cache) = state.pinned_event_cache.get_mut() {
            pinned_event_cache
                .maybe_add_live_related_events(&events, &state.room_version_rules.redaction)
                .await?;
        }

        let mut new_events_by_thread: BTreeMap<_, Vec<_>> = BTreeMap::new();

        for event in events {
            self.maybe_apply_new_redaction(&event, post_processing_origin).await?;

            if self.state.enabled_thread_support {
                // Only add the event to a thread if:
                // - thread support is enabled,
                // - and if this is a sync (we can't know where to insert backpaginated events
                //   in threads).
                if matches!(post_processing_origin, PostProcessingOrigin::Sync) {
                    if let Some(thread_root) = extract_thread_root(event.raw()) {
                        new_events_by_thread.entry(thread_root).or_default().push(event.clone());
                    } else if let Some(event_id) = event.event_id() {
                        // If we spot the root of a thread, add it to its linked chunk.
                        if self.state.threads.contains_key(&event_id) {
                            new_events_by_thread.entry(event_id).or_default().push(event.clone());
                        }
                    }
                }

                // If the post-processing origin is the redecryption, and this is part of a
                // thread, mark the thread as needing an update, potentially for its latest
                // event, that might have been redecrypted now.
                #[cfg(feature = "e2e-encryption")]
                if matches!(post_processing_origin, PostProcessingOrigin::Redecryption)
                    && let Some(thread_root) = extract_thread_root(event.raw())
                {
                    new_events_by_thread.entry(thread_root).or_default();
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
            self.update_threads(new_events_by_thread, post_processing_origin).await?;
        }

        Ok(())
    }

    pub fn get_or_reload_thread(&mut self, root_event_id: OwnedEventId) -> &mut ThreadEventCache {
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
        post_processing_origin: PostProcessingOrigin,
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

            self.maybe_update_thread_summary(thread_root, latest_event_id, post_processing_origin)
                .await?;
        }

        Ok(())
    }

    /// Update a thread summary on the given thread root, if needs be.
    async fn maybe_update_thread_summary(
        &mut self,
        thread_root: OwnedEventId,
        latest_event_id: Option<OwnedEventId>,
        _post_processing_origin: PostProcessingOrigin,
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

        // Note: in the case of redecryption, we still trigger an update even if the
        // summary has changed, so that observers can be notified that the
        // event in the summary may have been decrypted now.
        #[cfg(feature = "e2e-encryption")]
        let update_if_same_summaries =
            matches!(_post_processing_origin, PostProcessingOrigin::Redecryption);
        #[cfg(not(feature = "e2e-encryption"))]
        let update_if_same_summaries = false;

        if !update_if_same_summaries && prev_summary == new_summary.as_ref() {
            trace!(%thread_root, "thread summary is up-to-date, no need to update it");
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
    pub async fn replace_event_at(
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
        post_processing_origin: PostProcessingOrigin,
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
                self.maybe_update_thread_summary(
                    thread_root,
                    latest_event_id,
                    post_processing_origin,
                )
                .await?;
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
    let chunk_map: HashMap<_, _> = all_chunks.iter().map(|meta| (meta.identifier, meta)).collect();

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
    let mut stack = related.iter().filter_map(|(event, _pos)| event.event_id()).collect::<Vec<_>>();

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
