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

use std::{collections::BTreeSet, fmt};

use matrix_sdk_base::{
    apply_redaction, event_cache::Event, linked_chunk::Position,
    serde_helpers::extract_redaction_target, sync::Timeline,
};
use ruma::{
    EventId, OwnedEventId, events::room::redaction::SyncRoomRedactionEvent,
    room_version_rules::RoomVersionRules,
};
use tokio::sync::broadcast::Sender;
use tracing::trace;

use super::{
    super::{EventsOrigin, Result, states::StateLockWriteGuard},
    TimelineVectorDiffs,
    event_linked_chunk::EventLinkedChunk,
};
use crate::room::WeakRoom;

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
