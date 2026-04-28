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
    RoomInfoNotableUpdateReasons, apply_redaction, check_validity_of_replacement_events,
    deserialized_responses::{ThreadSummary, ThreadSummaryStatus},
    event_cache::{
        Event, Gap,
        store::{EventCacheStoreLock, EventCacheStoreLockGuard, EventCacheStoreLockState},
    },
    linked_chunk::{
        ChunkIdentifierGenerator, LinkedChunkId, OwnedLinkedChunkId, Position, Update, lazy_loader,
    },
    serde_helpers::{extract_edit_target, extract_thread_root},
    sync::Timeline,
};
use matrix_sdk_common::executor::spawn;
use ruma::{
    EventId, OwnedEventId, OwnedRoomId, OwnedUserId,
    events::{
        AnySyncEphemeralRoomEvent, AnySyncMessageLikeEvent, AnySyncTimelineEvent,
        MessageLikeEventType,
        receipt::{ReceiptEventContent, SyncReceiptEvent},
        relation::RelationType,
        room::redaction::SyncRoomRedactionEvent,
    },
    room_version_rules::RoomVersionRules,
    serde::Raw,
};
use tokio::sync::broadcast::{Receiver, Sender};
use tracing::{debug, error, instrument, trace, warn};

use super::{
    super::{
        super::{
            EventCacheError,
            automatic_pagination::AutomaticPagination,
            deduplicator::{DeduplicationOutcome, filter_duplicate_events},
            persistence::{
                find_event, find_event_relations, find_event_with_relations,
                load_linked_chunk_metadata, send_updates_to_store,
            },
        },
        EventLocation, TimelineVectorDiffs,
        event_focused::{EventFocusThreadMode, EventFocusedCache},
        event_linked_chunk::EventLinkedChunk,
        lock,
        pagination::SharedPaginationStatus,
        pinned_events::PinnedEventCache,
        read_receipts::compute_unread_counts,
    },
    EventsOrigin, PostProcessingOrigin, RoomEventCacheGenericUpdate,
    RoomEventCacheLinkedChunkUpdate, RoomEventCacheUpdate, RoomEventCacheUpdateSender,
    sort_positions_descending,
};
use crate::{Room, room::WeakRoom};

/// Key for the event-focused caches.
#[derive(Hash, PartialEq, Eq)]
struct EventFocusedCacheKey {
    /// The event ID that the cache is focused on.
    focused: OwnedEventId,
    /// The thread mode for this cache.
    thread_mode: EventFocusThreadMode,
}

pub struct RoomEventCacheState {
    /// Whether thread support has been enabled for the event cache.
    enabled_thread_support: bool,

    /// The room this state relates to.
    pub room_id: OwnedRoomId,

    /// A weak reference to the actual room.
    weak_room: WeakRoom,

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

    /// Event-focused caches for this room.
    ///
    /// Keyed by the focused event ID and thread mode. Each entry represents
    /// a timeline centered around a specific event (e.g. from a
    /// permalink).
    event_focused_caches: HashMap<EventFocusedCacheKey, EventFocusedCache>,

    /// Cache for pinned events in this room, initialized on-demand.
    pinned_event_cache: OnceLock<PinnedEventCache>,

    pagination_status: SharedObservable<SharedPaginationStatus>,

    /// A clone of [`super::RoomEventCacheInner::update_sender`].
    ///
    /// This is used only by the [`RoomEventCacheStateLock::read`] and
    /// [`RoomEventCacheStateLock::write`] when the state must be reset.
    update_sender: RoomEventCacheUpdateSender,

    /// A clone of
    /// [`super::super::EventCacheInner::linked_chunk_update_sender`].
    pub(super) linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,

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

    /// A copy of the automatic pagination API object.
    automatic_pagination: Option<AutomaticPagination>,
}

impl RoomEventCacheState {
    /// Return a read-only reference to the underlying room linked chunk.
    pub fn room_linked_chunk(&self) -> &EventLinkedChunk {
        &self.room_linked_chunk
    }
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
pub type LockedRoomEventCacheState = lock::StateLock<RoomEventCacheState>;

impl LockedRoomEventCacheState {
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
        weak_room: WeakRoom,
        room_version_rules: RoomVersionRules,
        enabled_thread_support: bool,
        update_sender: RoomEventCacheUpdateSender,
        linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,
        store: EventCacheStoreLock,
        pagination_status: SharedObservable<SharedPaginationStatus>,
        automatic_pagination: Option<AutomaticPagination>,
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
            weak_room,
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
            // Event-focused caches are created on-demand when the user navigates to a
            // permalink.
            event_focused_caches: HashMap::new(),
            pagination_status,
            update_sender,
            linked_chunk_update_sender,
            room_version_rules,
            waited_for_initial_prev_token: false,
            subscriber_count: Default::default(),
            pinned_event_cache: OnceLock::new(),
            automatic_pagination,
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

        // Reload the threads.
        for thread_event_cache in self.threads.values_mut() {
            thread_event_cache.reload().await?;
        }

        // Reload the pinned-events.
        if let Some(pinned_event_cache) = self.pinned_event_cache.get_mut() {
            pinned_event_cache.reload().await?;
        }

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
    /// Return the subscriber count.
    pub fn subscriber_count(&self) -> &Arc<AtomicUsize> {
        &self.state.subscriber_count
    }

    /// See documentation of [`find_event`].
    pub async fn find_event(
        &self,
        event_id: &EventId,
    ) -> Result<Option<(EventLocation, Event)>, EventCacheError> {
        find_event(event_id, &self.room_id, &self.room_linked_chunk, &self.store).await
    }

    /// See documentation of [`find_event_with_relations`].
    pub async fn find_event_with_relations(
        &self,
        event_id: &EventId,
        filters: Option<Vec<RelationType>>,
    ) -> Result<Option<(Event, Vec<Event>)>, EventCacheError> {
        find_event_with_relations(
            event_id,
            &self.room_id,
            filters,
            &self.room_linked_chunk,
            &self.store,
        )
        .await
    }

    /// See documentation of [`find_event_relations`].
    pub async fn find_event_relations(
        &self,
        event_id: &EventId,
        filters: Option<Vec<RelationType>>,
    ) -> Result<Vec<Event>, EventCacheError> {
        find_event_relations(event_id, &self.room_id, filters, &self.room_linked_chunk, &self.store)
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

    /// Get an event-focused cache for this event and thread mode, if it
    /// exists.
    ///
    /// Otherwise, returns `None`.
    pub fn get_event_focused_cache(
        &self,
        event_id: OwnedEventId,
        thread_mode: EventFocusThreadMode,
    ) -> Option<EventFocusedCache> {
        get_event_focused_cache(&self.state, event_id, thread_mode)
    }
}

impl<'a> RoomEventCacheStateLockWriteGuard<'a> {
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

    /// Get a reference to all the live [`event_focused_caches`].
    #[cfg(feature = "e2e-encryption")]
    pub fn event_focused_caches(&self) -> impl Iterator<Item = &EventFocusedCache> {
        self.state.event_focused_caches.values()
    }

    /// Get the `waited_for_initial_prev_token` value.
    pub fn waited_for_initial_prev_token(&self) -> bool {
        self.state.waited_for_initial_prev_token
    }

    /// Get a mutable reference to the `waited_for_initial_prev_token` value.
    pub fn waited_for_initial_prev_token_mut(&mut self) -> &mut bool {
        &mut self.state.waited_for_initial_prev_token
    }

    /// See documentation of [`find_event`].
    pub async fn find_event(
        &self,
        event_id: &EventId,
    ) -> Result<Option<(EventLocation, Event)>, EventCacheError> {
        find_event(event_id, &self.room_id, &self.room_linked_chunk, &self.store).await
    }

    /// See documentation of [`find_event_with_relations`].
    pub async fn find_event_with_relations(
        &self,
        event_id: &EventId,
        filters: Option<Vec<RelationType>>,
    ) -> Result<Option<(Event, Vec<Event>)>, EventCacheError> {
        find_event_with_relations(
            event_id,
            &self.room_id,
            filters,
            &self.room_linked_chunk,
            &self.store,
        )
        .await
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
        // timeline. This may cancel an ongoing pagination.
        self.state
            .pagination_status
            .set(SharedPaginationStatus::Idle { hit_timeline_start: false });

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
        self.propagate_changes().await?;

        // Reset the pagination state too: pretend we never waited for the initial
        // prev-batch token, and indicate that we're not at the start of the
        // timeline, since we don't know about that anymore.
        self.state.waited_for_initial_prev_token = false;

        // Note: this may cancel an ongoing pagination.
        self.state
            .pagination_status
            .set(SharedPaginationStatus::Idle { hit_timeline_start: false });

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
        ephemeral_events: &[Raw<AnySyncEphemeralRoomEvent>],
    ) -> Result<(bool, Vec<VectorDiff<Event>>), EventCacheError> {
        let timeline_prev_batch_token = timeline.prev_batch.take();
        let mut prev_batch_token = timeline_prev_batch_token.clone();

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
            prev_batch_token = None;
        }

        if all_duplicates {
            // No new events and no gap (per the previous check), thus no need to change the
            // room state. We're done!

            // We might have a new read receipt, though! If that's the case, handle it for
            // unread counts tracking.
            if let Some(new_receipt) = extract_read_receipt(ephemeral_events) {
                self.update_read_receipts(Some(&new_receipt)).await?;
            }

            return Ok((false, Vec::new()));
        }

        let has_new_gap = prev_batch_token.is_some();

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

        self.state.room_linked_chunk.push_live_events(
            prev_batch_token.map(|prev_token| Gap { token: prev_token }),
            &events,
        );

        // Extract a new read receipt, if available.
        let new_receipt = extract_read_receipt(ephemeral_events);
        self.post_process_new_events(
            events,
            timeline_prev_batch_token,
            PostProcessingOrigin::Sync,
            new_receipt,
        )
        .await?;

        if timeline.limited && has_new_gap {
            // If there was a previous batch token for a limited timeline, unload the chunks
            // so it only contains the last one; otherwise, there might be a
            // valid gap in between, and observers may not render it (yet).
            //
            // We must do this *after* persisting these events to storage (in
            // `post_process_new_events`).
            self.shrink_to_last_chunk().await?;
        }

        let timeline_event_diffs = self.room_linked_chunk.updates_as_vector_diffs();

        Ok((has_new_gap, timeline_event_diffs))
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
        prev_batch_token: Option<String>,
        post_processing_origin: PostProcessingOrigin,
        receipt_event: Option<ReceiptEventContent>,
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

        for event in events {
            self.maybe_apply_new_redaction(&event, post_processing_origin).await?;
        }

        self.update_read_receipts(receipt_event.as_ref()).await?;

        Ok(())
    }

    /// Update read receipts for all events in the room, based on the current
    /// state of the in-memory linked chunk.
    pub async fn update_read_receipts(
        &mut self,
        receipt_event: Option<&ReceiptEventContent>,
    ) -> Result<(), EventCacheError> {
        let Some(room) = self.state.weak_room.get() else {
            debug!("can't update read receipts: client's closing");
            return Ok(());
        };

        let user_id = &self.state.own_user_id;
        let room_id = &self.state.room_id;

        let prev_read_receipts = room.read_receipts().clone();
        let mut read_receipts = prev_read_receipts.clone();

        compute_unread_counts(
            user_id,
            room_id,
            receipt_event,
            &self.state.room_linked_chunk,
            &mut read_receipts,
            self.state.enabled_thread_support,
            self.state.automatic_pagination.as_ref(),
            room.client().state_store(),
        )
        .await;

        if prev_read_receipts != read_receipts {
            // The read receipt has changed! Do a little dance to update the `RoomInfo` in
            // the state store, and then in the room itself, so that observers
            // can be notified of the change.
            let result = room
                .update_and_save_room_info(|mut room_info| {
                    room_info.set_read_receipts(read_receipts);
                    (room_info, RoomInfoNotableUpdateReasons::READ_RECEIPT)
                })
                .await;
            if let Err(error) = result {
                error!(room_id = ?room.room_id(), ?error, "Failed to save the changes");
            }
        }

        Ok(())
    }

    /*
    pub(super) async fn get_or_reload_thread(
        &mut self,
        root_event_id: OwnedEventId,
    ) -> Result<&mut ThreadEventCache, EventCacheError> {
        let RoomEventCacheState {
            room_id,
            weak_room,
            own_user_id,
            store,
            update_sender,
            linked_chunk_update_sender,
            threads,
            ..
        } = self.state.deref_mut();

        match threads.entry(root_event_id.clone()) {
            Entry::Vacant(entry) => {
                let thread_event_cache = ThreadEventCache::new(
                    room_id.clone(),
                    root_event_id,
                    own_user_id.clone(),
                    weak_room.clone(),
                    store.clone(),
                    update_sender.generic_update_sender().clone(),
                    linked_chunk_update_sender.clone(),
                )
                .await?;

                Ok(entry.insert(thread_event_cache))
            }

            Entry::Occupied(entry) => Ok(entry.into_mut()),
        }
    }
    */

    #[instrument(skip_all)]
    async fn update_threads(
        &mut self,
        new_events_by_thread: BTreeMap<OwnedEventId, Vec<Event>>,
        prev_batch_token: Option<String>,
        post_processing_origin: PostProcessingOrigin,
    ) -> Result<(), EventCacheError> {
        for (thread_root, new_events) in new_events_by_thread {
            let thread_cache = self.get_or_reload_thread(thread_root.clone()).await?;

            thread_cache.add_live_events(new_events, &prev_batch_token).await?;
        }

        Ok(())
    }

    /// Update a thread summary on the given thread root, if needs be.
    #[must_use = "Propagate `VectorDiff` updates via `RoomEventCacheUpdate`"]
    pub async fn update_thread_summary(
        &mut self,
        thread_id: &EventId,
        new_thread_summary: ThreadSummary,
    ) -> Result<Vec<VectorDiff<Event>>, EventCacheError> {
        let Some((location, mut thread_root_event)) = self.find_event(&thread_id).await? else {
            trace!(%thread_id, "thread root event is missing from the room linked chunk");
            return Ok(Vec::new());
        };

        // Trigger an update to observers.
        trace!(%thread_id, "updating thread summary: {new_thread_summary:?}");
        thread_root_event.thread_summary = ThreadSummaryStatus::from_opt(Some(new_thread_summary));
        self.replace_event_at(location, thread_root_event).await?;

        Ok(self.room_linked_chunk.updates_as_vector_diffs())
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
            if deserialized.is_redacted() {
                return Ok(());
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

            self.replace_event_at(location, target_event.clone()).await?;

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
                thread_cache.replace_event_if_present(event_id, target_event).await?;

                // The number of replies may have changed, so update the thread summary if
                // needs be.
                let latest_event_id = thread_cache.latest_event_id().await?;

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

    /// Insert an initialized event-focused cache for the given event id.
    pub fn insert_event_focused_cache(
        &mut self,
        event_id: OwnedEventId,
        thread_mode: EventFocusThreadMode,
        cache: EventFocusedCache,
    ) {
        let key = EventFocusedCacheKey { focused: event_id, thread_mode };
        self.state.event_focused_caches.insert(key, cache);
    }

    /// Get an event-focused cache for this event and thread mode, if it
    /// exists.
    ///
    /// Otherwise, returns `None`.
    pub fn get_event_focused_cache(
        &self,
        event_id: OwnedEventId,
        thread_mode: EventFocusThreadMode,
    ) -> Option<EventFocusedCache> {
        get_event_focused_cache(&self.state, event_id, thread_mode)
    }
}

/// Extract a valid read receipt event from the ephemeral events, if
/// available.
fn extract_read_receipt(
    ephemeral_events: &[Raw<AnySyncEphemeralRoomEvent>],
) -> Option<ReceiptEventContent> {
    let mut receipt_event = None;

    for raw_ephemeral in ephemeral_events {
        match raw_ephemeral.deserialize() {
            Ok(AnySyncEphemeralRoomEvent::Receipt(SyncReceiptEvent { content, .. })) => {
                receipt_event = Some(content);
                break;
            }

            Ok(_) => {}

            Err(err) => {
                error!("error when deserializing an ephemeral event from sync: {err}");
            }
        }
    }

    receipt_event
}

/// Get an event-focused cache for this event and thread mode, if it exists.
///
/// Otherwise, returns `None`.
///
/// Extracted as a separate function to avoid duplicating the implementation for
/// both the read and write guards.
fn get_event_focused_cache(
    state: &RoomEventCacheState,
    event_id: OwnedEventId,
    thread_mode: EventFocusThreadMode,
) -> Option<EventFocusedCache> {
    let key = EventFocusedCacheKey { focused: event_id, thread_mode };
    state.event_focused_caches.get(&key).cloned()
}
