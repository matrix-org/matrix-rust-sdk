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

//! Event-focused timeline cache.
//!
//! This module provides [`EventFocusedCache`], a cache for an event-focused
//! timeline (e.g., for permalinks). It handles:
//! - Initialization from a focused event via `/context`.
//! - Thread detection (if the focused event is in a thread).
//! - Forward and backward pagination.
//! - In-memory storage, as these linked chunks are meant to be short-lived.
//!
//! Pagination tokens are stored as Gap items in the linked chunk:
//! - Backward token: Gap at the front of the chunk
//! - Forward token: Gap at the back of the chunk
//!
//! This allows pagination to resume at any point, and supports a future use
//! case where we'd want to persist these caches on disk (e.g., for permalinks
//! to work across sessions).

use std::{collections::BTreeSet, sync::Arc};

use matrix_sdk_base::{
    deserialized_responses::{TimelineEvent, TimelineEventKind},
    event_cache::{Event, Gap},
    linked_chunk::OwnedLinkedChunkId,
};
use matrix_sdk_common::{
    linked_chunk::{ChunkContent, ChunkIdentifier},
    serde_helpers::extract_thread_root,
};
use ruma::{OwnedEventId, UInt, api::Direction};
use tokio::sync::{
    RwLock,
    broadcast::{Receiver, Sender},
};
use tracing::{instrument, trace};

use super::events::EventLinkedChunk;
#[cfg(feature = "e2e-encryption")]
use crate::event_cache::redecryptor::ResolvedUtd;
use crate::{
    Room,
    event_cache::{
        EventCacheError, EventsOrigin, Result, RoomEventCacheLinkedChunkUpdate,
        caches::TimelineVectorDiffs,
    },
    paginators::{PaginationResult, Paginator, StartFromResult, thread::PaginableThread},
    room::{IncludeRelations, MessagesOptions, RelationsOptions, WeakRoom},
};

/// Options for controlling the behaviour of an `EventFocusedCache` when the
/// focused event may be part of a thread, or a thread's root.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum EventFocusThreadMode {
    /// Force the timeline into threaded mode.
    ///
    /// When the focused event is part of a thread, the timeline will be focused
    /// on that thread's root. Otherwise, the timeline will treat the target
    /// event itself as the thread root. Threaded events will never be
    /// hidden.
    ForceThread,

    /// Automatically determine if the target event is part of a thread or not.
    ///
    /// If the event is part of a thread, the timeline will be filtered to
    /// on-thread events.
    Automatic,
}

/// The mode of pagination for an event-focused timeline.
#[derive(Debug, Clone)]
pub(crate) enum EventFocusedPaginationMode {
    /// Standard room pagination (for all events as part of the unthreaded
    /// timeline).
    Room,

    /// Threaded pagination (the focused event is part of a thread).
    Thread {
        /// The root event ID of the thread.
        thread_root: OwnedEventId,
    },
}

struct EventFocusedCacheInner {
    /// The room owning this timeline.
    room: WeakRoom,

    /// The focused event ID.
    focused_event_id: OwnedEventId,

    /// The pagination mode (room or thread).
    pagination_mode: EventFocusedPaginationMode,

    /// The linked chunk for this focused timeline.
    chunk: EventLinkedChunk,

    /// A sender for timeline updates.
    sender: Sender<TimelineVectorDiffs>,

    /// A sender for globally observable linked chunk updates.
    linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,
}

impl EventFocusedCacheInner {
    /// Initialize the cache from a focused event.
    ///
    /// This uses `/context` to fetch the event with surrounding context.
    ///
    /// This detects if the event is part of a thread and sets up the
    /// appropriate pagination mode.
    ///
    /// Pagination tokens are stored as gaps in the linked chunk:
    /// - Backward token (start): Gap at the front
    /// - Forward token (end): Gap at the back
    #[instrument(skip(self, room), fields(room_id = %self.room.room_id(), event_id = %self.focused_event_id))]
    async fn start_from(
        &mut self,
        room: Room,
        num_context_events: u16,
        thread_mode: EventFocusThreadMode,
    ) -> Result<StartFromResult> {
        trace!(num_context_events, "fetching event with context via /context");

        let paginator = Paginator::new(room);

        let result =
            paginator.start_from(&self.focused_event_id, UInt::from(num_context_events)).await?;

        // Detect if the focused event is part of a thread.
        let thread_root = match thread_mode {
            EventFocusThreadMode::ForceThread => {
                // Try to extract a thread root from the focused event, first.
                let focused_event = result
                    .events
                    .iter()
                    .find(|event| event.event_id().as_deref() == Some(&*self.focused_event_id));

                // If the focused event has a thread root, use it.
                let mut thread_root =
                    focused_event.and_then(|event| extract_thread_root(event.raw()));

                // If there's no thread root, consider that the focused event itself is the
                // thread root.
                if thread_root.is_none() {
                    thread_root = Some(self.focused_event_id.clone());
                }

                trace!("force thread mode enabled, treating focused event as thread root");
                thread_root
            }

            EventFocusThreadMode::Automatic => {
                trace!(
                    "automatic thread mode enabled, checking if focused event is part of a thread"
                );
                result
                    .events
                    .iter()
                    .find(|event| event.event_id().as_deref() == Some(&*self.focused_event_id))
                    .and_then(|event| extract_thread_root(event.raw()))
            }
        };

        // Get pagination tokens from the paginator.
        let tokens = paginator.tokens();

        if let Some(root_id) = thread_root {
            trace!(thread_root = %root_id, "focused event is part of a thread, setting up thread pagination");

            // Check if the thread root is included in the response. Start from the
            // beginning, since it's more likely to be around there, in that
            // case.
            let includes_root =
                result.events.iter().any(|event| event.event_id().as_deref() == Some(&*root_id));

            self.pagination_mode =
                EventFocusedPaginationMode::Thread { thread_root: root_id.clone() };

            // Filter events to only include those in the thread.
            let thread_events = result
                .events
                .iter()
                .filter(|event| {
                    extract_thread_root(event.raw()).as_ref() == Some(&root_id)
                        || event.event_id().as_deref() == Some(&*root_id)
                })
                .cloned()
                .collect();

            // Determine backward token (only if we don't have the thread root).
            let backward_token = if includes_root {
                // We have the root, no need for backward pagination.
                None
            } else {
                tokens.previous.into_token()
            };

            // Forward token.
            let forward_token = tokens.next.into_token();

            self.add_initial_events_with_gaps(thread_events, backward_token, forward_token);
        } else {
            trace!("focused event is not part of a thread, setting up room pagination");
            self.pagination_mode = EventFocusedPaginationMode::Room;

            let backward_token = tokens.previous.into_token();
            let forward_token = tokens.next.into_token();

            self.add_initial_events_with_gaps(result.events.clone(), backward_token, forward_token);
        }

        self.propagate_changes();

        // Empty the updates_as_vector_diffs(), since it's impossible for an observer to
        // have subscribed to this cache yet, since it was being created here. Such
        // initial updates would be duplicated, since the subscriber will get
        // the full initial list of events on subscription.
        let _ = self.chunk.updates_as_vector_diffs();

        Ok(result)
    }

    /// Add initial events to the chunk, with gaps for pagination tokens.
    fn add_initial_events_with_gaps(
        &mut self,
        events: Vec<TimelineEvent>,
        prev_gap_token: Option<String>,
        next_gap_token: Option<String>,
    ) {
        let events: Vec<Event> = events.into_iter().collect();

        // Insert backward gap at front if we have a token.
        if let Some(prev_token) = prev_gap_token {
            trace!("inserting backward pagination gap at front");
            self.chunk.push_live_events(Some(Gap { prev_token }), &[]);
        }

        // Add the events.
        if !events.is_empty() {
            self.chunk.push_live_events(None, &events);
        }

        // Insert forward gap at back if we have a token.
        if let Some(next_token) = next_gap_token {
            trace!("inserting forward pagination gap at back");
            self.chunk.push_live_events(Some(Gap { prev_token: next_token }), &[]);
        }
    }

    /// Propagate changes to the linked chunk update sender.
    fn propagate_changes(&mut self) {
        let updates = self.chunk.store_updates().take();
        if !updates.is_empty() {
            let _ = self.linked_chunk_update_sender.send(RoomEventCacheLinkedChunkUpdate {
                updates,
                linked_chunk_id: OwnedLinkedChunkId::EventFocused(
                    self.room.room_id().to_owned(),
                    self.focused_event_id.clone(),
                ),
            });
        }
    }

    /// Notify subscribers of timeline updates.
    fn notify_subscribers(&mut self, origin: EventsOrigin) {
        let diffs = self.chunk.updates_as_vector_diffs();
        if !diffs.is_empty() {
            let _ = self.sender.send(TimelineVectorDiffs { diffs, origin });
        }
    }

    /// Return the first chunk as a gap, if it's one.
    ///
    /// This stores the backward pagination token, in this case.
    fn first_chunk_as_gap(&self) -> Option<(ChunkIdentifier, Gap)> {
        self.chunk.chunks().next().and_then(|chunk| {
            if let ChunkContent::Gap(gap) = chunk.content() {
                Some((chunk.identifier(), gap.clone()))
            } else {
                None
            }
        })
    }

    /// Return the last chunk as a gap, if it's one.
    ///
    /// This stores the forward pagination token, in this case.
    fn last_chunk_as_gap(&self) -> Option<(ChunkIdentifier, Gap)> {
        self.chunk.rchunks().next().and_then(|chunk| {
            if let ChunkContent::Gap(gap) = chunk.content() {
                Some((chunk.identifier(), gap.clone()))
            } else {
                None
            }
        })
    }

    /// Paginate backwards in this event-focused timeline.
    ///
    /// This finds the gap at the front of the linked chunk, fetches older
    /// events, replaces the gap with the events, and inserts a new gap if
    /// there are more events to fetch.
    #[instrument(skip(self), fields(room_id = %self.room.room_id()))]
    async fn paginate_backwards(&mut self, num_events: u16) -> Result<PaginationResult> {
        let room = self.room.get().ok_or(EventCacheError::ClientDropped)?;

        // Find the gap at the front (backward pagination token).
        let Some((gap_id, gap)) = self.first_chunk_as_gap() else {
            // No gap at front means we've already hit the start of the timeline.
            trace!("no front gap found, already at timeline start");
            return Ok(PaginationResult { events: Vec::new(), hit_end_of_timeline: true });
        };

        let token = gap.prev_token;
        trace!(?token, "paginating backwards with token from front gap");

        // Fetch events based on pagination mode.
        let (mut events, new_token) = match &self.pagination_mode {
            EventFocusedPaginationMode::Room => {
                Self::fetch_room_backwards(&room, num_events, &token).await?
            }
            EventFocusedPaginationMode::Thread { thread_root } => {
                Self::fetch_thread_backwards(&room, num_events, &token, thread_root.clone()).await?
            }
        };

        // Events are in the reverse order, per the API contracts defined in the two
        // fetch methods.
        events.reverse();

        let hit_end = new_token.is_none();
        let new_gap = new_token.map(|t| Gap { prev_token: t });

        // Replace the gap and insert the new events.
        self.chunk.finish_back_pagination(Some(gap_id), new_gap, &events);

        self.propagate_changes();
        self.notify_subscribers(EventsOrigin::Pagination);

        Ok(PaginationResult { events, hit_end_of_timeline: hit_end })
    }

    /// Fetch events for backward room pagination (returns events and optional
    /// next token).
    ///
    /// Returns the events in the same ordering as the one received by the
    /// server, i.e., newest to oldest.
    async fn fetch_room_backwards(
        room: &Room,
        num_events: u16,
        token: &str,
    ) -> Result<(Vec<Event>, Option<String>)> {
        let mut options = MessagesOptions::backward().from(token);
        options.limit = UInt::from(num_events);

        let messages = room
            .messages(options)
            .await
            .map_err(|err| EventCacheError::BackpaginationError(Box::new(err)))?;

        Ok((messages.chunk, messages.end))
    }

    /// Fetch events for backward thread pagination.
    ///
    /// Returns the events in the same ordering as the one received by the
    /// server, i.e., newest to oldest.
    async fn fetch_thread_backwards(
        room: &Room,
        num_events: u16,
        token: &str,
        thread_root: OwnedEventId,
    ) -> Result<(Vec<Event>, Option<String>)> {
        let options = RelationsOptions {
            from: Some(token.to_owned()),
            dir: Direction::Backward,
            limit: Some(UInt::from(num_events)),
            include_relations: IncludeRelations::AllRelations,
            recurse: true,
        };

        let mut result = room
            .relations(thread_root.clone(), options)
            .await
            .map_err(|err| EventCacheError::BackpaginationError(Box::new(err)))?;

        // If we hit the end (no more token), load the thread root event.
        if result.next_batch_token.is_none() {
            let root_event = room
                .load_event(&thread_root)
                .await
                .map_err(|err| EventCacheError::BackpaginationError(Box::new(err)))?;
            result.chunk.push(root_event);
        }

        Ok((result.chunk, result.next_batch_token))
    }

    /// Paginate forwards in this event-focused timeline.
    ///
    /// This finds the gap at the back of the linked chunk, fetches newer
    /// events, replaces the gap with the events, and inserts a new gap if
    /// there are more events to fetch.
    #[instrument(skip(self), fields(room_id = %self.room.room_id()))]
    async fn paginate_forwards(&mut self, num_events: u16) -> Result<PaginationResult> {
        let room = self.room.get().ok_or(EventCacheError::ClientDropped)?;

        // Find the gap at the back (forward pagination token).
        let Some((gap_id, gap)) = self.last_chunk_as_gap() else {
            // No gap at back means we've already hit the end of the timeline.
            trace!("no back gap found, already at timeline end");
            return Ok(PaginationResult { events: Vec::new(), hit_end_of_timeline: true });
        };

        let token = gap.prev_token;
        trace!(?token, "paginating forwards with token from back gap");

        // Fetch events based on pagination mode.
        let (events, new_token) = match &self.pagination_mode {
            EventFocusedPaginationMode::Room => {
                Self::fetch_room_forwards(&room, num_events, &token).await?
            }
            EventFocusedPaginationMode::Thread { thread_root } => {
                Self::fetch_thread_forwards(&room, num_events, &token, thread_root.clone()).await?
            }
        };

        let hit_end = new_token.is_none();
        let new_gap = new_token.map(|t| Gap { prev_token: t });

        // Replace the gap and insert new events.
        self.chunk.finish_forward_pagination(Some(gap_id), new_gap, &events);

        self.propagate_changes();
        self.notify_subscribers(EventsOrigin::Pagination);

        Ok(PaginationResult { events, hit_end_of_timeline: hit_end })
    }

    /// Fetch events for forward room pagination.
    async fn fetch_room_forwards(
        room: &Room,
        num_events: u16,
        token: &str,
    ) -> Result<(Vec<Event>, Option<String>)> {
        let mut options = MessagesOptions::new(Direction::Forward);
        options = options.from(Some(token));
        options.limit = UInt::from(num_events);

        let messages = room.messages(options).await.map_err(|err|
            // TODO(bnjbvr): should be a more generic "pagination" error now!
            EventCacheError::BackpaginationError(Box::new(err)))?;

        Ok((messages.chunk, messages.end))
    }

    /// Fetch events for forward thread pagination.
    async fn fetch_thread_forwards(
        room: &Room,
        num_events: u16,
        token: &str,
        thread_root: OwnedEventId,
    ) -> Result<(Vec<Event>, Option<String>)> {
        let options = RelationsOptions {
            from: Some(token.to_owned()),
            dir: Direction::Forward,
            limit: Some(UInt::from(num_events)),
            include_relations: IncludeRelations::AllRelations,
            recurse: true,
        };

        let result = room
            .relations(thread_root, options)
            .await
            .map_err(|err| EventCacheError::BackpaginationError(Box::new(err)))?;

        Ok((result.chunk, result.next_batch_token))
    }
}

/// A cache for an event-focused timeline.
///
/// This represents a timeline centered around a specific event (e.g., from a
/// permalink), supporting both forward and backward pagination. The focused
/// event may be part of a thread, in which case pagination will use the
/// `/relations` API instead of `/messages`.
///
/// Pagination tokens are stored as Gap items in the linked chunk itself:
/// - A gap at the **front** (first position) contains the backward pagination
///   token.
/// - A gap at the **back** (last position) contains the forward pagination
///   token.
///
/// This is a shallow data structure, and can be cloned cheaply.
#[derive(Clone)]
pub struct EventFocusedCache {
    inner: Arc<RwLock<EventFocusedCacheInner>>,
}

impl EventFocusedCache {
    /// Create a new empty event-focused cache.
    pub(super) fn new(
        room: WeakRoom,
        focused_event_id: OwnedEventId,
        linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,
    ) -> Self {
        Self {
            inner: Arc::new(RwLock::new(EventFocusedCacheInner {
                room,
                focused_event_id,
                pagination_mode: EventFocusedPaginationMode::Room,
                chunk: EventLinkedChunk::new(),
                sender: Sender::new(32),
                linked_chunk_update_sender,
            })),
        }
    }

    /// Subscribe to updates from this event-focused timeline.
    pub async fn subscribe(&self) -> (Vec<Event>, Receiver<TimelineVectorDiffs>) {
        let inner = self.inner.read().await;
        let events = inner.chunk.events().map(|(_position, item)| item.clone()).collect();
        let recv = inner.sender.subscribe();
        (events, recv)
    }

    /// Check if we've hit the start of the timeline (no more backward
    /// pagination possible).
    pub async fn hit_timeline_start(&self) -> bool {
        self.inner.read().await.first_chunk_as_gap().is_none()
    }

    /// Check if we've hit the end of the timeline (no more forward pagination
    /// possible).
    pub async fn hit_timeline_end(&self) -> bool {
        self.inner.read().await.last_chunk_as_gap().is_none()
    }

    /// Start the event-focused timeline from the focused event, fetching
    /// context events and detecting thread membership.
    pub(super) async fn start_from(
        &self,
        room: Room,
        num_context_events: u16,
        thread_mode: EventFocusThreadMode,
    ) -> Result<StartFromResult> {
        self.inner.write().await.start_from(room, num_context_events, thread_mode).await
    }

    /// Paginate backwards in this event-focused timeline, be it room or thread
    /// pagination depending on the mode.
    pub async fn paginate_backwards(&self, num_events: u16) -> Result<PaginationResult> {
        self.inner.write().await.paginate_backwards(num_events).await
    }

    /// Paginate forwards in this event-focused timeline, be it room or thread
    /// pagination depending on the mode.
    pub async fn paginate_forwards(&self, num_events: u16) -> Result<PaginationResult> {
        self.inner.write().await.paginate_forwards(num_events).await
    }

    /// Get the thread root event ID if this linked chunk is in thread mode.
    pub async fn thread_root(&self) -> Option<OwnedEventId> {
        match &self.inner.read().await.pagination_mode {
            EventFocusedPaginationMode::Thread { thread_root } => Some(thread_root.clone()),
            _ => None,
        }
    }

    /// Try to locate the events in the linked chunk corresponding to the given
    /// list of decrypted events, and replace them, while alerting observers
    /// about the update.
    // TODO(bnjbvr): common out
    pub async fn replace_utds(&self, events: &[ResolvedUtd]) {
        let mut guard = self.inner.write().await;

        let event_set = guard
            .chunk
            .events()
            .filter_map(|(_pos, ev)| ev.event_id())
            .into_iter()
            .collect::<BTreeSet<_>>();

        let mut replaced_some = false;

        for (event_id, decrypted, actions) in events {
            // As a performance optimization, do a lookup in the current pinned events
            // check, before looking for the event in the linked chunk.

            if !event_set.contains(event_id) {
                continue;
            }

            // The event should be in the linked chunk.
            let Some((position, mut target_event)) = guard.chunk.find_event(event_id) else {
                continue;
            };

            target_event.kind = TimelineEventKind::Decrypted(decrypted.clone());

            if let Some(actions) = actions {
                target_event.set_push_actions(actions.clone());
            }

            guard
                .chunk
                .replace_event_at(position, target_event.clone())
                .expect("position should be valid");

            replaced_some = true;
        }

        if replaced_some {
            guard.propagate_changes();
            guard.notify_subscribers(EventsOrigin::Cache);
        }
    }
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for EventFocusedCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventFocusedCache").finish_non_exhaustive()
    }
}
