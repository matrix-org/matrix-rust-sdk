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

//! An event cache for a caller-supplied set of event IDs.
//!
//! This is a generalisation of the pinned-events cache: instead of the event
//! IDs coming from the room's `m.room.pinned_events` state, they are supplied
//! (and updated) by the caller. Each event is loaded together with its
//! reactions and edits, and the cache receives the sync events related to what
//! it holds (any relation type, redactions included), so the events are kept
//! up to date over time.
//!
//! Unlike the pinned-events cache, this cache is **in-memory only** (like the
//! event-focused cache): caller-supplied sets have no natural persistent
//! identity, so nothing is written to the store.

use std::{
    collections::BTreeSet,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
    },
};

use matrix_sdk_base::{
    apply_redaction,
    event_cache::Event,
    linked_chunk::Position,
    serde_helpers::extract_redaction_target,
    sync::{JoinedRoomUpdate, LeftRoomUpdate, Timeline},
};
use ruma::{
    EventId, OwnedEventId, events::room::redaction::SyncRoomRedactionEvent,
    room_version_rules::RoomVersionRules,
};
use tokio::sync::broadcast::{Receiver, Sender};
use tracing::trace;

#[cfg(feature = "e2e-encryption")]
use super::super::redecryptor::{MaybeResolvedEvent, TryResolveEvents};
use super::{
    super::{
        EventCacheError, EventsOrigin, Result,
        states::{
            CacheStateLock, StateLock, StateLockWriteGuard, selectors::SpecificEventsStateSelector,
        },
    },
    TimelineVectorDiffs,
    event_linked_chunk::EventLinkedChunk,
    pinned_events::load_events_with_relations,
};
use crate::room::WeakRoom;

/// Monotonic source of instance IDs, so several caches can coexist for the
/// same room, each with its own state.
static NEXT_INSTANCE_ID: AtomicU64 = AtomicU64::new(0);

/// State of a [`SpecificEventsCache`].
pub struct SpecificEventsCacheState {
    /// The room owning the events, used to (re)load them.
    room: WeakRoom,

    /// The rules for the version of this room.
    room_version_rules: RoomVersionRules,

    /// The caller-supplied set of event IDs this cache is about.
    event_ids: Vec<OwnedEventId>,

    /// The linked chunk holding the events and their related events, sorted
    /// chronologically (oldest to newest), as ordering information is lost
    /// when loading events through `/event` and `/relations`.
    chunk: EventLinkedChunk,

    /// Update sender for this cache.
    pub update_sender: Sender<TimelineVectorDiffs>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for SpecificEventsCacheState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpecificEventsCacheState")
            .field("room_id", &self.room.room_id())
            .field("event_ids", &self.event_ids)
            .finish_non_exhaustive()
    }
}

impl SpecificEventsCacheState {
    /// Return a list of the current event IDs in this linked chunk (the
    /// caller-supplied events and their loaded relations).
    pub(super) fn current_event_ids(&self) -> Vec<OwnedEventId> {
        self.chunk
            .events()
            .filter_map(|(_position, event)| event.event_id().map(ToOwned::to_owned))
            .collect()
    }

    fn has_event_ids(&self, event_ids: &[OwnedEventId]) -> bool {
        self.event_ids.iter().collect::<BTreeSet<_>>() == event_ids.iter().collect::<BTreeSet<_>>()
    }
}

impl<'a> StateLockWriteGuard<'a, SpecificEventsCacheState> {
    /// Handle new events from sync: events already loaded (e.g. through
    /// `/relations`) keep their position, the rest are appended, then any
    /// redaction is applied.
    fn handle_sync(&mut self, timeline: Timeline) -> Result<()> {
        let mut new_events = Vec::new();

        for event in &timeline.events {
            let known_position = event.event_id().and_then(|event_id| {
                self.state.chunk.events().find_map(|(position, known)| {
                    (known.event_id() == Some(event_id)).then_some(position)
                })
            });

            match known_position {
                Some(position) => {
                    self.state
                        .chunk
                        .replace_event_at(position, event.clone())
                        .expect("should have been a valid position of an item");
                }
                None => new_events.push(event.clone()),
            }
        }

        if !new_events.is_empty() {
            self.state.chunk.push_live_events(None, &new_events);
        }

        self.drain_store_updates();
        self.notify_subscribers(EventsOrigin::Sync);

        for event in &timeline.events {
            self.maybe_apply_new_redaction(event)?;
        }

        Ok(())
    }

    /// If the given event is a redaction, try to retrieve the to-be-redacted
    /// event in the chunk, and replace it by the redacted form.
    fn maybe_apply_new_redaction(&mut self, event: &Event) -> Result<()> {
        let Some(event_id) =
            extract_redaction_target(event.raw(), &self.state.room_version_rules.redaction)
        else {
            return Ok(());
        };

        let Some((position, mut target_event)) = self.find_event_in_memory(&event_id) else {
            trace!("redacted event is missing from the linked chunk");
            return Ok(());
        };

        // Don't redact already redacted events.
        if let Ok(deserialized) = target_event.raw().deserialize()
            && deserialized.is_redacted()
        {
            return Ok(());
        }

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

            self.state
                .chunk
                .replace_event_at(position, target_event)
                .expect("should have been a valid position of an item");

            self.drain_store_updates();
            self.notify_subscribers(EventsOrigin::Sync);
        }

        Ok(())
    }

    /// Find an event in the linked chunk, by ID.
    fn find_event_in_memory(&self, event_id: &EventId) -> Option<(Position, Event)> {
        self.state
            .chunk
            .events()
            .find(|(_position, event)| event.event_id() == Some(event_id))
            .map(|(position, event)| (position, event.clone()))
    }

    /// Replace the entire content of the linked chunk with the given events,
    /// unless nothing changed, and notify subscribers.
    fn replace_all_events(&mut self, new_events: Vec<Event>) {
        let previous_event_ids = self.state.current_event_ids();

        if new_events
            .iter()
            .filter_map(|event| event.event_id())
            .map(ToOwned::to_owned)
            .collect::<BTreeSet<OwnedEventId>>()
            == previous_event_ids.into_iter().collect::<BTreeSet<OwnedEventId>>()
        {
            return;
        }

        if self.state.chunk.events().next().is_some() {
            self.state.chunk.reset();
        }

        self.state.chunk.push_live_events(None, &new_events);

        self.drain_store_updates();
        self.notify_subscribers(EventsOrigin::Sync);
    }

    /// This cache is in-memory only: drain the accumulated store updates so
    /// they don't grow unbounded.
    //
    // TODO: decide whether these updates should be broadcast on the
    // `linked_chunk_update_sender` too, which would require a non-persisted
    // `LinkedChunkId` variant for caller-supplied sets.
    fn drain_store_updates(&mut self) {
        let _ = self.state.chunk.store_updates().take();
    }

    /// Notify subscribers of timeline updates.
    fn notify_subscribers(&mut self, origin: EventsOrigin) {
        let diffs = self.state.chunk.updates_as_vector_diffs();

        if !diffs.is_empty() {
            let _ = self.state.update_sender.send(TimelineVectorDiffs { diffs, origin });
        }
    }
}

/// An event cache for a caller-supplied set of event IDs.
///
/// Cloning is shallow, and thus is cheap to do.
#[derive(Clone)]
pub struct SpecificEventsCache {
    inner: Arc<SpecificEventsCacheInner>,
}

/// The (non-cloneable) details of the `SpecificEventsCache`.
struct SpecificEventsCacheInner {
    /// State of this `SpecificEventsCache`.
    state: CacheStateLock<SpecificEventsStateSelector>,
}

impl SpecificEventsCache {
    /// Creates a new, empty [`SpecificEventsCache`] for the given room and
    /// event IDs; call [`Self::reload`] to load the events.
    pub(in super::super) async fn new(
        weak_room: WeakRoom,
        mut event_ids: Vec<OwnedEventId>,
        state: &StateLock,
    ) -> Result<Self> {
        let room = weak_room.get().ok_or(EventCacheError::ClientDropped)?;
        let room_id = room.room_id().to_owned();
        let room_version_rules = room.clone_info().room_version_rules_or_default();
        let instance_id = NEXT_INSTANCE_ID.fetch_add(1, AtomicOrdering::Relaxed);

        dedup_event_ids(&mut event_ids);

        let cache_state = state
            .try_insert_once_with(
                SpecificEventsStateSelector::new(room_id, instance_id),
                |_store_guard| async {
                    Ok(SpecificEventsCacheState {
                        room: weak_room.clone(),
                        room_version_rules,
                        event_ids: event_ids.clone(),
                        chunk: EventLinkedChunk::new(),
                        update_sender: Sender::new(32),
                    })
                },
            )
            .await?;

        Ok(Self { inner: Arc::new(SpecificEventsCacheInner { state: cache_state }) })
    }

    /// Load the events for the current set of IDs, notifying subscribers of
    /// the changes.
    ///
    /// The events are fetched with no lock held: loading goes through the
    /// event cache, which needs the state lock too.
    pub(in super::super) async fn reload(&self) -> Result<()> {
        let (room, event_ids) = {
            let guard = self.inner.state.read().await?;
            (guard.state.room.get(), guard.state.event_ids.clone())
        };

        let Some(room) = room else {
            // The client is shutting down; there's nothing sensible to reload.
            return Ok(());
        };

        let max_concurrent_requests =
            room.client().event_cache().config().max_pinned_events_concurrent_requests;

        let events = load_events_with_relations(&room, event_ids.clone(), max_concurrent_requests)
            .await
            .ok_or(EventCacheError::UnableToLoadSpecificEvents)?;

        let mut guard = self.inner.state.write().await?;

        // A newer set is being loaded by someone else: let it win.
        if !guard.state.has_event_ids(&event_ids) {
            return Ok(());
        }

        guard.replace_all_events(events);

        Ok(())
    }

    /// Return a reference to the state.
    pub(super) fn state(&self) -> &CacheStateLock<SpecificEventsStateSelector> {
        &self.inner.state
    }

    /// Read all current events (the caller-supplied events and their loaded
    /// relations).
    pub async fn events(&self) -> Result<Vec<Event>> {
        let guard = self.inner.state.read().await?;

        Ok(guard.state.chunk.events().map(|(_position, event)| event.clone()).collect())
    }

    /// Subscribe to live updates from this cache.
    pub async fn subscribe(&self) -> Result<(Vec<Event>, Receiver<TimelineVectorDiffs>)> {
        let guard = self.inner.state.read().await?;
        let events = guard.state.chunk.events().map(|(_position, event)| event.clone()).collect();
        let recv = guard.state.update_sender.subscribe();

        Ok((events, recv))
    }

    /// Replace the set of event IDs this cache is about, reloading the events
    /// if the set changed.
    ///
    /// There is no state event to observe for caller-supplied sets, so this is
    /// the caller's way of keeping the set up to date. If the events can't be
    /// loaded, the previous set is kept.
    pub async fn set_event_ids(&self, mut event_ids: Vec<OwnedEventId>) -> Result<()> {
        dedup_event_ids(&mut event_ids);

        let previous_event_ids = {
            let mut guard = self.inner.state.write().await?;

            if guard.state.has_event_ids(&event_ids) {
                return Ok(());
            }

            std::mem::replace(&mut guard.state.event_ids, event_ids.clone())
        };

        if let Err(err) = self.reload().await {
            let mut guard = self.inner.state.write().await?;

            if guard.state.has_event_ids(&event_ids) {
                guard.state.event_ids = previous_event_ids;
            }

            return Err(err);
        }

        Ok(())
    }

    /// Handle a joined-room update from sync.
    ///
    /// The caller is expected to have filtered the timeline down to events
    /// related to this cache's events already (see
    /// [`super::aggregator::aggregate_timeline_for_pinned_events`]).
    pub(in super::super) async fn handle_joined_room_update(
        &self,
        updates: JoinedRoomUpdate,
    ) -> Result<()> {
        if updates.timeline.events.is_empty() {
            return Ok(());
        }

        self.inner.state.write().await?.handle_sync(updates.timeline)
    }

    /// Handle a left-room update from sync.
    pub(in super::super) async fn handle_left_room_update(
        &self,
        updates: LeftRoomUpdate,
    ) -> Result<()> {
        if updates.timeline.events.is_empty() {
            return Ok(());
        }

        self.inner.state.write().await?.handle_sync(updates.timeline)
    }

    /// Try to locate the events in the linked chunk corresponding to the given
    /// list of resolved events, and replace them, while alerting observers
    /// about the update.
    #[cfg(feature = "e2e-encryption")]
    pub(in super::super) async fn replace_in_memory_utds(
        &self,
        resolved_events: &[MaybeResolvedEvent],
    ) -> Result<()> {
        let mut guard = self.inner.state.write().await?;

        // This cache doesn't persist anything in the store, so try to resolve
        // events against the in-memory linked chunk.
        let resolved_events = resolved_events.try_resolve_events(&guard.state.chunk);

        if guard.state.chunk.replace_utds(&resolved_events) {
            guard.drain_store_updates();
            guard.notify_subscribers(EventsOrigin::Cache);
        }

        Ok(())
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for SpecificEventsCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpecificEventsCache").finish_non_exhaustive()
    }
}

/// Remove duplicate IDs, keeping the first occurrence.
fn dedup_event_ids(event_ids: &mut Vec<OwnedEventId>) {
    let mut seen = BTreeSet::new();
    event_ids.retain(|event_id| seen.insert(event_id.clone()));
}
