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
//! aggregation-relevant relations (reactions and edits, recursively), and the
//! cache receives live sync updates for those events, so redactions, new
//! reactions and edits are reflected over time.
//!
//! Unlike the pinned-events cache, this cache is **in-memory only** (like the
//! event-focused cache): caller-supplied sets have no natural persistent
//! identity, so nothing is written to the store.

use std::{
    cmp::Ordering,
    collections::BTreeSet,
    fmt,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
    },
};

use eyeball_im::VectorDiff;
use futures_util::{StreamExt as _, stream};
use matrix_sdk_base::{
    apply_redaction,
    event_cache::Event,
    linked_chunk::Position,
    serde_helpers::extract_redaction_target,
    sync::{JoinedRoomUpdate, LeftRoomUpdate, Timeline},
};
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId,
    events::{relation::RelationType, room::redaction::SyncRoomRedactionEvent},
    room_version_rules::RoomVersionRules,
};
use tokio::sync::broadcast::{Receiver, Sender};
use tracing::{trace, warn};

#[cfg(feature = "e2e-encryption")]
use super::super::redecryptor::{MaybeResolvedEvent, TryResolveEvents};
use super::{
    super::{
        EventCacheError, EventsOrigin, Result,
        states::{
            CacheStateLock, ReloadPreprocessing, StateLock, StateLockWriteGuard,
            selectors::SpecificEventsStateSelector,
        },
    },
    TimelineVectorDiffs,
    event_linked_chunk::EventLinkedChunk,
};
use crate::{Room, config::RequestConfig, room::WeakRoom};

/// Monotonic source of instance IDs, so several caches can coexist for the
/// same room, each with its own state.
static NEXT_INSTANCE_ID: AtomicU64 = AtomicU64::new(0);

/// State of a [`SpecificEventsCache`].
pub struct SpecificEventsCacheState {
    /// The ID of the room owning the events.
    room_id: OwnedRoomId,

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
            .field("room_id", &self.room_id)
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
}

impl<'a> StateLockWriteGuard<'a, SpecificEventsCacheState> {
    /// Reload the events and their relations from the cache or the homeserver,
    /// replacing the linked chunk's content.
    ///
    /// Since there is no persistent storage for this cache, `preprocessing` is
    /// ignored.
    #[must_use = "Propagate `VectorDiff` updates via `TimelineVectorDiffs`"]
    pub async fn reload(
        &mut self,
        _preprocessing: ReloadPreprocessing,
    ) -> Result<Vec<VectorDiff<Event>>> {
        let Some(room) = self.state.room.get() else {
            // The client is shutting down; there's nothing sensible to reload.
            return Ok(Vec::new());
        };

        let event_ids = self.state.event_ids.clone();
        let loaded_events =
            SpecificEventsCache::load_events_with_relations(room, event_ids).await?;

        Ok(self.replace_all_events(loaded_events))
    }

    /// Handle new events from sync: replace the events we already know about
    /// (their aggregations may have changed), append the rest, and apply any
    /// redactions.
    fn handle_sync(&mut self, timeline: Timeline) -> Result<()> {
        if timeline.events.is_empty() {
            return Ok(());
        }

        let mut new_events = Vec::new();

        for event in &timeline.events {
            let known_position = event.event_id().and_then(|event_id| {
                self.state.chunk.events().find_map(|(position, known)| {
                    (known.event_id() == Some(event_id)).then_some(position)
                })
            });

            match known_position {
                Some(position) => {
                    // A refreshed version of an event we already have.
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
    /// unless nothing changed. Returns the resulting updates, which the caller
    /// is responsible for propagating to subscribers.
    fn replace_all_events(&mut self, new_events: Vec<Event>) -> Vec<VectorDiff<Event>> {
        let previous_event_ids = self.state.current_event_ids();

        if new_events
            .iter()
            .filter_map(|event| event.event_id())
            .map(ToOwned::to_owned)
            .collect::<BTreeSet<OwnedEventId>>()
            == previous_event_ids.into_iter().collect::<BTreeSet<OwnedEventId>>()
        {
            // No change in the set of loaded events.
            return Vec::new();
        }

        if self.state.chunk.events().next().is_some() {
            self.state.chunk.reset();
        }

        self.state.chunk.push_live_events(None, &new_events);

        self.drain_store_updates();
        self.state.chunk.updates_as_vector_diffs()
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
    /// Creates a new [`SpecificEventsCache`] for the given room and event IDs,
    /// and loads the events.
    pub(in super::super) async fn new(
        weak_room: WeakRoom,
        event_ids: Vec<OwnedEventId>,
        state: &StateLock,
    ) -> Result<Self> {
        let room = weak_room.get().ok_or(EventCacheError::ClientDropped)?;
        let room_id = room.room_id().to_owned();
        let room_version_rules = room.clone_info().room_version_rules_or_default();
        let instance_id = NEXT_INSTANCE_ID.fetch_add(1, AtomicOrdering::Relaxed);

        let cache_state = state
            .try_insert_once_with(
                SpecificEventsStateSelector::new(room_id.clone(), instance_id),
                |_store_guard| async {
                    Ok(SpecificEventsCacheState {
                        room_id,
                        room: weak_room.clone(),
                        room_version_rules,
                        event_ids: event_ids.clone(),
                        chunk: EventLinkedChunk::new(),
                        update_sender: Sender::new(32),
                    })
                },
            )
            .await?;

        let cache = Self { inner: Arc::new(SpecificEventsCacheInner { state: cache_state }) };

        cache.reload().await?;

        Ok(cache)
    }

    /// Reload the events, notifying subscribers of the changes.
    async fn reload(&self) -> Result<()> {
        let mut guard = self.inner.state.write().await?;

        let diffs = guard.reload(ReloadPreprocessing::None).await?;

        if !diffs.is_empty() {
            let _ = guard
                .state
                .update_sender
                .send(TimelineVectorDiffs { diffs, origin: EventsOrigin::Sync });
        }

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
    /// the caller's way of keeping the set up to date.
    pub async fn set_event_ids(&self, event_ids: Vec<OwnedEventId>) -> Result<()> {
        {
            let mut guard = self.inner.state.write().await?;

            if guard.state.event_ids.iter().collect::<BTreeSet<_>>()
                == event_ids.iter().collect::<BTreeSet<_>>()
            {
                return Ok(());
            }

            guard.state.event_ids = event_ids;
        }

        self.reload().await
    }

    /// Loads the given events, using the cache first and then requesting the
    /// events from the homeserver if they couldn't be found, along with their
    /// aggregation-relevant relations (reactions and edits, recursively).
    ///
    /// This method will perform as many concurrent requests for events as
    /// `max_pinned_events_concurrent_requests` allows, to avoid overwhelming
    /// the server.
    //
    // Note: this is the pinned-events loader with the IDs supplied by the
    // caller; it could be shared with `PinnedEventsCache` in a follow-up.
    async fn load_events_with_relations(
        room: Room,
        event_ids: Vec<OwnedEventId>,
    ) -> Result<Vec<Event>> {
        let max_concurrent_requests = {
            let client = room.client();
            let config = client.event_cache().config();
            config.max_pinned_events_concurrent_requests
        };

        if event_ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut num_successful_loads = 0;
        let num_requested = event_ids.len();

        let loaded_events: Vec<Event> = stream::iter(event_ids.into_iter().map(|event_id| {
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
        .inspect(|result| {
            if result.is_ok() {
                num_successful_loads += 1;
            }
        })
        .flat_map(stream::iter)
        .flat_map(stream::iter)
        .collect()
        .await;

        if num_successful_loads != num_requested {
            warn!(
                "only successfully loaded {} out of {} requested events",
                num_successful_loads, num_requested
            );
        }

        let mut loaded_events = loaded_events;

        // Ordering information is lost when loading through `/event` and
        // `/relations`; resort to chronological ordering (oldest -> newest).
        loaded_events.sort_by(compare_events_by_timestamp);

        Ok(loaded_events)
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
        self.inner.state.write().await?.handle_sync(updates.timeline)
    }

    /// Handle a left-room update from sync.
    pub(in super::super) async fn handle_left_room_update(
        &self,
        updates: LeftRoomUpdate,
    ) -> Result<()> {
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

fn compare_events_by_timestamp(a: &Event, b: &Event) -> Ordering {
    let a_time: Option<MilliSecondsSinceUnixEpoch> = a.timestamp_raw();
    let b_time: Option<MilliSecondsSinceUnixEpoch> = b.timestamp_raw();

    match (a_time, b_time) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Greater,
        (Some(_), None) => Ordering::Less,
        (Some(a), Some(b)) => a.cmp(&b),
    }
}
