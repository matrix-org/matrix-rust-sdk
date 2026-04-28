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

use eyeball_im::VectorDiff;
use matrix_sdk_base::{
    apply_redaction, check_validity_of_replacement_events,
    deserialized_responses::{ThreadSummary, ThreadSummaryStatus},
    event_cache::{
        Event, Gap,
        store::{EventCacheStoreLock, EventCacheStoreLockGuard, EventCacheStoreLockState},
    },
    linked_chunk::{
        ChunkIdentifierGenerator, LinkedChunkId, OwnedLinkedChunkId, Position, Update, lazy_loader,
    },
    serde_helpers::extract_edit_target,
    sync::Timeline,
};
use matrix_sdk_common::executor::spawn;
use ruma::{
    EventId, OwnedEventId, OwnedRoomId, OwnedUserId,
    events::{
        AnySyncMessageLikeEvent, AnySyncTimelineEvent, MessageLikeEventType,
        relation::RelationType, room::redaction::SyncRoomRedactionEvent,
    },
    room_version_rules::RoomVersionRules,
};
use tokio::sync::broadcast::Sender;
use tracing::{debug, error, instrument, trace, warn};

use super::{
    super::{
        super::{
            EventCacheError, EventsOrigin, Result,
            deduplicator::{DeduplicationOutcome, filter_duplicate_events},
            persistence::{
                find_event, find_event_with_relations, load_linked_chunk_metadata,
                send_updates_to_store,
            },
        },
        EventLocation, TimelineVectorDiffs,
        event_linked_chunk::{EventLinkedChunk, sort_positions_descending},
        lock,
        room::{RoomEventCacheGenericUpdate, RoomEventCacheLinkedChunkUpdate},
    },
    ThreadEventCacheUpdateSender,
};

pub struct ThreadEventCacheState {
    /// The room owning this thread.
    pub room_id: OwnedRoomId,

    /// The ID of the thread root event, which is the first event in the thread
    /// (and eventually the first in the linked chunk).
    pub thread_id: OwnedEventId,

    /// The user's own user id.
    pub own_user_id: OwnedUserId,

    /// The rules for the version of this room.
    room_version_rules: RoomVersionRules,

    /// Reference to the underlying backing store.
    store: EventCacheStoreLock,

    /// The linked chunk for this thread.
    thread_linked_chunk: EventLinkedChunk,

    /// A clone of [`super::ThreadEventCacheInner::update_sender`].
    ///
    /// This is used only by the [`ThreadEventCacheStateLock::read`] and
    /// [`ThreadEventCacheStateLock::write`] when the state must be reset.
    update_sender: ThreadEventCacheUpdateSender,

    /// A sender for the globally observable linked chunk updates that happened
    /// during a sync or a back-pagination.
    ///
    /// See also [`super::super::EventCacheInner::linked_chunk_update_sender`].
    linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,

    /// Have we ever waited for a previous-batch-token to come from sync, in
    /// the context of pagination? We do this at most once per room/thread (?),
    /// the first time we try to run backward pagination. We reset
    /// that upon clearing the timeline events.
    waited_for_initial_prev_token: bool,
}

impl ThreadEventCacheState {
    /// If storage is enabled, unload all the chunks, then reloads only the
    /// last one.
    ///
    /// If storage's enabled, return a diff update that starts with a clear
    /// of all events; as a result, the caller may override any
    /// pending diff updates with the result of this function.
    ///
    /// Otherwise, returns `None`.
    async fn shrink_to_last_chunk(&mut self, store: &EventCacheStoreLockGuard) -> Result<()> {
        // Attempt to load the last chunk.
        let linked_chunk_id = LinkedChunkId::Thread(&self.room_id, &self.thread_id);

        let (last_chunk, chunk_identifier_generator) =
            match store.load_last_chunk(linked_chunk_id).await {
                Ok(pair) => pair,

                Err(err) => {
                    // If loading the last chunk failed, clear the entire linked chunk.
                    error!("error when reloading a linked chunk from memory: {err}");

                    // Clear storage for this room.
                    store.handle_linked_chunk_updates(linked_chunk_id, vec![Update::Clear]).await?;

                    // Restart with an empty linked chunk.
                    (None, ChunkIdentifierGenerator::new_from_scratch())
                }
            };

        debug!("unloading the linked chunk, and resetting it to its last chunk");

        // Remove all the chunks from the linked chunks, except for the last one, and
        // updates the chunk identifier generator.
        if let Err(err) =
            self.thread_linked_chunk.replace_with(last_chunk, chunk_identifier_generator)
        {
            error!("error when replacing the linked chunk: {err}");
            return self.reset_internal(store).await;
        }

        // Don't propagate those updates to the store; this is only for the in-memory
        // representation that we're doing this. Let's drain those store updates.
        let _ = self.thread_linked_chunk.store_updates().take();

        Ok(())
    }

    async fn reset_internal(&mut self, store: &EventCacheStoreLockGuard) -> Result<()> {
        self.thread_linked_chunk.reset();

        self.propagate_changes(store).await?;

        // Reset the pagination state too: pretend we never waited for the initial
        // prev-batch token, and indicate that we're not at the start of the
        // timeline, since we don't know about that anymore.
        self.waited_for_initial_prev_token = false;

        Ok(())
    }

    pub async fn propagate_changes(&mut self, store: &EventCacheStoreLockGuard) -> Result<()> {
        let updates = self.thread_linked_chunk.store_updates().take();

        self.send_updates_to_store(updates, store).await
    }

    async fn send_updates_to_store(
        &mut self,
        updates: Vec<Update<Event, Gap>>,
        store: &EventCacheStoreLockGuard,
    ) -> Result<()> {
        let linked_chunk_id =
            OwnedLinkedChunkId::Thread(self.room_id.clone(), self.thread_id.clone());

        send_updates_to_store(&store, linked_chunk_id, &self.linked_chunk_update_sender, updates)
            .await
    }
}

impl lock::Store for ThreadEventCacheState {
    fn store(&self) -> &EventCacheStoreLock {
        &self.store
    }
}

/// State for a single thread's event cache.
///
/// This contains all the inner mutable states that ought to be updated at
/// the same time.
pub type LockedThreadEventCacheState = lock::StateLock<ThreadEventCacheState>;

impl LockedThreadEventCacheState {
    /// Create a new state, or reload it from storage if it's been enabled.
    ///
    /// Not all events are going to be loaded. Only a portion of them. The
    /// [`EventLinkedChunk`] relies on a [`LinkedChunk`] to store all
    /// events. Only the last chunk will be loaded. It means the
    /// events are loaded from the most recent to the oldest. To
    /// load more events, see [`ThreadPagination`].
    ///
    /// [`LinkedChunk`]: matrix_sdk_common::linked_chunk::LinkedChunk
    /// [`ThreadPagination`]: super::pagination::ThreadPagination
    pub async fn new(
        room_id: OwnedRoomId,
        thread_id: OwnedEventId,
        own_user_id: OwnedUserId,
        room_version_rules: RoomVersionRules,
        store: EventCacheStoreLock,
        update_sender: ThreadEventCacheUpdateSender,
        linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,
    ) -> Result<Self> {
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

        let linked_chunk_id = LinkedChunkId::Thread(&room_id, &thread_id);

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

        Ok(Self::new_inner(ThreadEventCacheState {
            room_id,
            thread_id,
            own_user_id,
            room_version_rules,
            store,
            thread_linked_chunk: EventLinkedChunk::with_initial_linked_chunk(
                linked_chunk,
                full_linked_chunk_metadata,
            ),
            update_sender,
            linked_chunk_update_sender,
            waited_for_initial_prev_token: false,
        }))
    }
}

/// The read-lock guard around [`ThreadEventCacheState`].
///
/// See [`ThreadEventCacheStateLock::read`] to acquire it.
pub type ThreadEventCacheStateLockReadGuard<'a> =
    lock::StateLockReadGuard<'a, ThreadEventCacheState>;

/// The write-lock guard around [`ThreadEventCacheState`].
///
/// See [`ThreadEventCacheStateLock::write`] to acquire it.
pub type ThreadEventCacheStateLockWriteGuard<'a> =
    lock::StateLockWriteGuard<'a, ThreadEventCacheState>;

/// The owned write-lock guard around [`ThreadEventCacheState`].
pub type OwnedThreadEventCacheStateLockWriteGuard =
    lock::OwnedStateLockWriteGuard<ThreadEventCacheState>;

impl<'a> lock::Reload for ThreadEventCacheStateLockWriteGuard<'a> {
    /// Force to shrink the room, whenever there is subscribers or not.
    async fn reload(&mut self) -> Result<()> {
        self.state.shrink_to_last_chunk(&self.store).await?;

        let diffs = self.state.thread_linked_chunk.updates_as_vector_diffs();

        if !diffs.is_empty() {
            self.state.update_sender.send(
                TimelineVectorDiffs { diffs, origin: EventsOrigin::Cache },
                Some(RoomEventCacheGenericUpdate { room_id: self.room_id.to_owned() }),
            );
        }

        Ok(())
    }
}

impl lock::Reload for OwnedThreadEventCacheStateLockWriteGuard {
    /// Force to shrink the room, whenever there is subscribers or not.
    async fn reload(&mut self) -> Result<()> {
        self.state.shrink_to_last_chunk(&self.store).await?;

        let diffs = self.state.thread_linked_chunk.updates_as_vector_diffs();

        if !diffs.is_empty() {
            self.state.update_sender.send(
                TimelineVectorDiffs { diffs, origin: EventsOrigin::Cache },
                Some(RoomEventCacheGenericUpdate { room_id: self.room_id.to_owned() }),
            );
        }

        Ok(())
    }
}

impl<'a> ThreadEventCacheStateLockReadGuard<'a> {
    /// Return a read-only reference to the underlying thread linked chunk.
    pub fn thread_linked_chunk(&self) -> &EventLinkedChunk {
        &self.state.thread_linked_chunk
    }

    /// Compute and return the [`ThreadSummary`] for this thread.
    pub async fn compute_thread_summary(&self) -> Result<Option<ThreadSummary>> {
        // Find the latest event ID.
        //
        // TODO(@hywan): This is inefficient. Ultimately, we want to rely on
        // `LatestEvent` to compute the `ThreadSummary` correctly.
        let latest_event_id = {
            // Find the last non-edit event.
            let mut latest_event_id = self
                .thread_linked_chunk()
                .revents()
                .filter(|(_position, event)| extract_edit_target(event.raw()).is_none())
                .next()
                .and_then(|(_position, event)| event.event_id());

            // If there's an edit to the latest event in the thread, use the latest edit
            // event ID as the latest event ID for the thread summary.
            if let Some(event_id) = latest_event_id.as_ref()
                && let Some((original_event, edits)) = self
                    .find_event_with_relations(event_id, Some(vec![RelationType::Replacement]))
                    .await?
            {
                let latest_valid_edit = edits.into_iter().rfind(|edit| {
                    let original_json = original_event.raw();
                    let original_encryption_info = original_event.encryption_info();
                    let replacement_json = edit.raw();
                    let replacement_encryption_info = edit.encryption_info();

                    check_validity_of_replacement_events(
                        original_json,
                        original_encryption_info.map(|v| &**v),
                        replacement_json,
                        replacement_encryption_info.map(|v| &**v),
                    )
                    .is_ok()
                });

                if let Some(latest_valid_edit) = latest_valid_edit {
                    latest_event_id = latest_valid_edit.event_id();
                }
            }

            latest_event_id
        };

        // Compute the thread summary.

        // Read the latest number of thread replies from the store.
        //
        // Implementation note: since this is based on the `m.relates_to` field, and
        // that field can only be present on room messages, we don't have to
        // worry about filtering out aggregation events (like reactions/edits/etc.).
        // Pretty neat, huh?
        let num_replies = {
            let thread_replies = self
                .store
                .find_event_relations(&self.room_id, &self.thread_id, Some(&[RelationType::Thread]))
                .await?;
            thread_replies.len().try_into().unwrap_or(u32::MAX)
        };

        let summary = if num_replies > 0 {
            Some(ThreadSummary { num_replies, latest_reply: latest_event_id })
        } else {
            None
        };

        Ok(summary)
    }

    /// See documentation of [`find_event_with_relations`].
    async fn find_event_with_relations(
        &self,
        event_id: &EventId,
        filters: Option<Vec<RelationType>>,
    ) -> Result<Option<(Event, Vec<Event>)>> {
        find_event_with_relations(
            event_id,
            &self.room_id,
            filters,
            &self.thread_linked_chunk,
            &self.store,
        )
        .await
    }
}

impl<'a> ThreadEventCacheStateLockWriteGuard<'a> {
    /// Return a read-only reference to the underlying thread linked chunk.
    pub fn thread_linked_chunk(&self) -> &EventLinkedChunk {
        &self.state.thread_linked_chunk
    }

    /// Return a mutable reference to the underlying thread linked chunk.
    pub fn thread_linked_chunk_mut(&mut self) -> &mut EventLinkedChunk {
        &mut self.state.thread_linked_chunk
    }

    /// Get the `waited_for_initial_prev_token` value.
    pub fn waited_for_initial_prev_token(&self) -> bool {
        self.state.waited_for_initial_prev_token
    }

    /// Get the `waited_for_initial_prev_token` value.
    pub fn waited_for_initial_prev_token_mut(&mut self) -> &mut bool {
        &mut self.state.waited_for_initial_prev_token
    }

    #[must_use = "Propagate `VectorDiff` updates via `TimelineVectorDiffs`"]
    pub async fn handle_sync(
        &mut self,
        timeline: Timeline,
    ) -> Result<(bool, Vec<VectorDiff<Event>>)> {
        let prev_batch_token = &timeline.prev_batch;

        let DeduplicationOutcome {
            all_events: events,
            in_memory_duplicated_event_ids,
            in_store_duplicated_event_ids,
            non_empty_all_duplicates: all_duplicates,
        } = filter_duplicate_events(
            &self.state.own_user_id,
            &self.store,
            LinkedChunkId::Thread(&self.state.room_id, &self.state.thread_id),
            &self.state.thread_linked_chunk,
            timeline.events,
        )
        .await?;

        if all_duplicates {
            // If all events are duplicates, we don't need to do anything; ignore
            // the new events.
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
        // We don't have to worry about the removals can change the position of the
        // existing events, because we are pushing all _new_ `events` at the back.
        self.remove_events(in_memory_duplicated_event_ids, in_store_duplicated_event_ids).await?;

        self.state.thread_linked_chunk.push_live_events(
            prev_batch_token.as_ref().map(|prev_token| Gap { token: prev_token.clone() }),
            &events,
        );

        self.state.propagate_changes(&self.store).await?;

        if timeline.limited && has_new_gap {
            // If there was a previous batch token for a limited timeline, unload the chunks
            // so it only contains the last one; otherwise, there might be a
            // valid gap in between, and observers may not render it (yet).
            //
            // We must do this *after* persisting these events to storage.
            self.state.shrink_to_last_chunk(&self.store).await?;
        }

        // Do stuff for each event.
        for event in events {
            // Handle redaction.
            self.maybe_apply_new_redaction(&event).await?;

            // Save a bundled thread event, if there was one.
            if let Some(bundled_thread) = event.bundled_latest_thread_event {
                self.save_events([*bundled_thread]).await?;
            }
        }

        let timeline_event_diffs = self.state.thread_linked_chunk.updates_as_vector_diffs();

        Ok((has_new_gap, timeline_event_diffs))
    }

    /// If the given event is a redaction, try to retrieve the
    /// to-be-redacted event in the chunk, and replace it by the
    /// redacted form.
    #[instrument(skip_all)]
    async fn maybe_apply_new_redaction(&mut self, event: &Event) -> Result<()> {
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

        let Some(event_id) = redaction.redacts(&self.room_version_rules.redaction) else {
            warn!("missing target event id from the redaction event");
            return Ok(());
        };

        // Replace the redacted event by a redacted form, if we knew about it.
        let Some((location, mut target_event)) = self.find_event(event_id).await? else {
            trace!("redacted event is missing from the linked chunk");
            return Ok(());
        };

        let target_event_raw = target_event.raw();

        // Don't redact already redacted events.
        if let Ok(deserialized) = target_event_raw.deserialize()
            && deserialized.is_redacted()
        {
            return Ok(());
        };

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
        find_event(event_id, &self.room_id, &self.thread_linked_chunk, &self.store).await
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
                    .thread_linked_chunk
                    .replace_event_at(position, new_event)
                    .expect("should have been a valid position of an item");
                // We just changed the in-memory representation; synchronize this with
                // the store.
                self.state.propagate_changes(&self.store).await?;
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
            .thread_linked_chunk
            .remove_events_by_position(
                in_memory_events.into_iter().map(|(_event_id, position)| position).collect(),
            )
            .expect("failed to remove an event");

        self.state.propagate_changes(&self.store).await
    }

    /// Apply some updates that are effective only on the store itself.
    ///
    /// This method should be used only for updates that happen *outside*
    /// the in-memory linked chunk. Such updates must be applied
    /// onto the ordering tracker as well as to the persistent
    /// storage.
    async fn apply_store_only_updates(&mut self, updates: Vec<Update<Event, Gap>>) -> Result<()> {
        self.state.thread_linked_chunk.order_tracker.map_updates(&updates);
        self.state.send_updates_to_store(updates, &self.store).await
    }
}

impl OwnedThreadEventCacheStateLockWriteGuard {
    /// Reset this data structure as if it were brand new.
    ///
    /// Return a single diff update that is a clear of all events; as a
    /// result, the caller may override any pending diff updates
    /// with the result of this function.
    pub async fn reset(&mut self) -> Result<Vec<VectorDiff<Event>>> {
        self.state.reset_internal(&self.store).await?;

        let diff_updates = self.state.thread_linked_chunk.updates_as_vector_diffs();

        // Ensure the contract defined in the doc comment is true:
        debug_assert_eq!(diff_updates.len(), 1);
        debug_assert!(matches!(diff_updates[0], VectorDiff::Clear));

        Ok(diff_updates)
    }
}
