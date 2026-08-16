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

//! A message-type filtered view over a room's event cache: all the room
//! messages of some `msgtype`s (typically the media and files), straight from
//! the store's index, with the room's timeline gaps interleaved.
//!
//! Unlike a filtered live timeline, which mirrors the room's in-memory linked
//! chunk and thus has to walk the whole cached history through memory to find
//! the few events it renders, this view is a *projection of the persisted*
//! linked chunk: it is seeded from the store's `msgtype` index in one query
//! (see `EventCacheStore::find_events_by_message_types`), and then kept up to
//! date by the linked chunk updates that reach the store (see
//! `LinkedChunkUpdateFanout`), whichever code path produced them: sync,
//! back-pagination, gap resolution, redecryption, redaction, clear.
//!
//! The view exposes its events newest-first in pages (see
//! [`MessageTypesEventCache::paginate_backwards`]), and reports the room's
//! gaps as [`TimelineGap`]s anchored to the next matching event, or with no
//! anchor when no matching event follows (which is how a media grid learns
//! that there may be media hidden behind a gap at its newest end).

use std::{fmt, sync::Arc};

use eyeball_im::VectorDiff;
use matrix_sdk_base::{
    event_cache::{Event, Gap},
    linked_chunk::{ChunkIdentifier, OrderTracker, OwnedLinkedChunkId, Position, Update},
    task_monitor::BackgroundTaskHandle,
};
use ruma::{OwnedEventId, OwnedRoomId, RoomId};
use tokio::sync::{
    RwLock,
    broadcast::{Receiver, Sender},
    mpsc::UnboundedReceiver,
};
use tracing::{debug, error, instrument, trace, warn};

use super::{
    super::{EventsOrigin, Result},
    room::{LinkedChunkUpdateFanout, RoomEventCache, RoomEventCacheLinkedChunkUpdate, TimelineGap},
};
use crate::Client;

/// The number of matching events exposed right away by a fresh view; the rest
/// is exposed page by page through
/// [`MessageTypesEventCache::paginate_backwards`].
const INITIAL_PAGE_SIZE: usize = 50;

/// An update of a [`MessageTypesEventCache`].
///
/// The events diffs and the gaps snapshot travel together, so that consumers
/// always reconcile the gaps against the events they were computed with.
#[derive(Clone, Debug)]
pub struct MessageTypesCacheUpdate {
    /// Diffs to apply to the exposed events (a vector, oldest first).
    pub diffs: Vec<VectorDiff<Event>>,

    /// Where the change comes from.
    pub origin: EventsOrigin,

    /// The full set of gaps to render, in timeline order, valid once the
    /// diffs are applied.
    pub gaps: Vec<TimelineGap>,
}

/// A message-type filtered view over a room's event cache. See the module
/// documentation.
///
/// This is a shallow data structure, and can be cloned cheaply.
#[derive(Clone)]
pub struct MessageTypesEventCache {
    inner: Arc<Inner>,
}

impl fmt::Debug for MessageTypesEventCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MessageTypesEventCache")
            .field("room_id", &self.inner.room_id)
            .field("msgtypes", &self.inner.msgtypes)
            .finish_non_exhaustive()
    }
}

struct Inner {
    room_id: OwnedRoomId,
    msgtypes: Vec<String>,
    room_event_cache: RoomEventCache,
    state: RwLock<State>,
    update_sender: Sender<MessageTypesCacheUpdate>,
    /// The task applying the linked chunk updates to the view; aborted with
    /// the last clone of the view.
    _update_task: std::sync::Mutex<Option<BackgroundTaskHandle>>,
}

/// One item of the projection: a matching event, or a gap of the room's
/// linked chunk, both located by their (chunk, index) position.
#[derive(Clone, Debug)]
enum Entry {
    Event { position: Position, event: Event },
    Gap { chunk_id: ChunkIdentifier, token: String },
}

impl Entry {
    fn position(&self) -> Position {
        match self {
            Entry::Event { position, .. } => *position,
            // A gap chunk holds no item; index 0 orders it against any
            // item of the surrounding chunks.
            Entry::Gap { chunk_id, .. } => Position::new(*chunk_id, 0),
        }
    }

    fn chunk_id(&self) -> ChunkIdentifier {
        self.position().chunk_identifier()
    }

    fn event_id(&self) -> Option<OwnedEventId> {
        match self {
            Entry::Event { event, .. } => event.event_id().map(ToOwned::to_owned),
            Entry::Gap { .. } => None,
        }
    }
}

struct State {
    /// The order of all the chunks of the room's persisted linked chunk,
    /// including the ones not loaded in memory; the source of truth for
    /// ordering the entries.
    order: OrderTracker<Event, Gap>,

    /// All the entries, in timeline order (oldest first).
    entries: Vec<Entry>,

    /// The entries at `exposed_from..` are exposed to subscribers; the ones
    /// before are held back until
    /// [`MessageTypesEventCache::paginate_backwards`] exposes them.
    exposed_from: usize,
}

impl State {
    /// Sort key of an entry, in the current chunk order.
    fn key(&self, entry: &Entry) -> Option<(usize, usize)> {
        self.order.chunk_ordering(entry.position())
    }

    /// The exposed events, oldest first.
    fn exposed_events(&self) -> Vec<Event> {
        self.entries[self.exposed_from..]
            .iter()
            .filter_map(|entry| match entry {
                Entry::Event { event, .. } => Some(event.clone()),
                Entry::Gap { .. } => None,
            })
            .collect()
    }

    /// The index, in the vector of exposed events, of the exposed entry at
    /// `index` (or, if that entry is a gap, of the event that would follow
    /// it).
    fn event_index(&self, index: usize) -> usize {
        self.entries[self.exposed_from..index]
            .iter()
            .filter(|entry| matches!(entry, Entry::Event { .. }))
            .count()
    }

    /// The gaps to render: every exposed gap, anchored to the first exposed
    /// event after it (if any).
    fn timeline_gaps(&self) -> Vec<TimelineGap> {
        let exposed = &self.entries[self.exposed_from..];

        exposed
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| match entry {
                Entry::Gap { token, .. } => Some(TimelineGap {
                    prev_token: token.clone(),
                    following_event_id: exposed[index + 1..]
                        .iter()
                        .find_map(|following| following.event_id()),
                }),
                Entry::Event { .. } => None,
            })
            .collect()
    }

    /// Insert an entry at its place in the timeline order.
    ///
    /// Entries landing among the exposed ones are exposed right away, and
    /// their diff is pushed to `diffs`; entries landing before them are held
    /// back.
    fn insert(&mut self, entry: Entry, diffs: &mut Vec<VectorDiff<Event>>) {
        let Some(key) = self.key(&entry) else {
            // The chunk isn't part of the linked chunk (any more).
            trace!(?entry, "Not inserting an entry with an unknown chunk");
            return;
        };

        // The entries are sorted by key; find where this one goes. Keys can't
        // collide: positions are unique.
        let index = self
            .entries
            .partition_point(|other| self.key(other).is_some_and(|other_key| other_key < key));

        if index >= self.exposed_from {
            if let Entry::Event { event, .. } = &entry {
                diffs.push(VectorDiff::Insert {
                    index: self.event_index(index),
                    value: event.clone(),
                });
            }
        } else {
            self.exposed_from += 1;
        }

        self.entries.insert(index, entry);
    }

    /// Remove the entry at `index`.
    fn remove(&mut self, index: usize, diffs: &mut Vec<VectorDiff<Event>>) -> Entry {
        if index >= self.exposed_from {
            if let Entry::Event { .. } = &self.entries[index] {
                diffs.push(VectorDiff::Remove { index: self.event_index(index) });
            }
        } else {
            self.exposed_from -= 1;
        }

        self.entries.remove(index)
    }

    /// Remove all the entries matching a predicate.
    fn retain(&mut self, mut keep: impl FnMut(&Entry) -> bool, diffs: &mut Vec<VectorDiff<Event>>) {
        // Removing from the end keeps the earlier indices valid.
        for index in (0..self.entries.len()).rev() {
            if !keep(&self.entries[index]) {
                self.remove(index, diffs);
            }
        }
    }

    /// Find the entry at a given position, if any.
    fn find(&self, position: Position) -> Option<usize> {
        self.entries.iter().position(|entry| entry.position() == position)
    }

    /// Whether an event is one of the room messages this view is about.
    fn matches(&self, event: &Event, msgtypes: &[String]) -> bool {
        event.kind.msgtype().is_some_and(|msgtype| msgtypes.contains(&msgtype))
    }

    /// Apply one linked chunk update.
    fn apply(
        &mut self,
        update: &Update<Event, Gap>,
        msgtypes: &[String],
        diffs: &mut Vec<VectorDiff<Event>>,
    ) {
        // Keep the chunk order in sync first: inserting needs to know the
        // new chunks, removing doesn't need the removed ones.
        self.order.map_updates(std::slice::from_ref(update));

        match update {
            Update::NewItemsChunk { .. }
            | Update::StartReattachItems
            | Update::EndReattachItems => {
                // Nothing to show: an items chunk shows through its items.
            }

            Update::NewGapChunk { new, gap, .. } => {
                self.insert(Entry::Gap { chunk_id: *new, token: gap.token.clone() }, diffs);
            }

            Update::RemoveChunk(chunk_id) => {
                self.retain(|entry| entry.chunk_id() != *chunk_id, diffs);
            }

            Update::PushItems { at, items } => {
                for (offset, item) in items.iter().enumerate() {
                    if self.matches(item, msgtypes) {
                        let position = Position::new(at.chunk_identifier(), at.index() + offset);
                        self.insert(Entry::Event { position, event: item.clone() }, diffs);
                    }
                }
            }

            Update::ReplaceItem { at, item } => {
                let existing = self.find(*at);
                let matches = self.matches(item, msgtypes);

                match (existing, matches) {
                    (Some(index), true) => {
                        // Same place, new content (e.g. updated encryption
                        // info, or an edit applied).
                        if index >= self.exposed_from {
                            diffs.push(VectorDiff::Set {
                                index: self.event_index(index),
                                value: item.clone(),
                            });
                        }
                        self.entries[index] = Entry::Event { position: *at, event: item.clone() };
                    }
                    (Some(index), false) => {
                        // E.g. redacted.
                        self.remove(index, diffs);
                    }
                    (None, true) => {
                        // E.g. decrypted at last.
                        self.insert(Entry::Event { position: *at, event: item.clone() }, diffs);
                    }
                    (None, false) => {}
                }
            }

            Update::RemoveItem { at } => {
                if let Some(index) = self.find(*at) {
                    self.remove(index, diffs);
                }

                // The items after it in the same chunk shift down.
                for entry in &mut self.entries {
                    if let Entry::Event { position, .. } = entry
                        && position.chunk_identifier() == at.chunk_identifier()
                        && position.index() > at.index()
                    {
                        position.decrement_index();
                    }
                }
            }

            Update::DetachLastItems { at } => {
                self.retain(
                    |entry| {
                        !(matches!(entry, Entry::Event { .. })
                            && entry.chunk_id() == at.chunk_identifier()
                            && entry.position().index() >= at.index())
                    },
                    diffs,
                );
            }

            Update::Clear => {
                if self.entries[self.exposed_from..]
                    .iter()
                    .any(|entry| matches!(entry, Entry::Event { .. }))
                {
                    diffs.push(VectorDiff::Clear);
                }
                self.entries.clear();
                self.exposed_from = 0;
            }
        }
    }
}

impl MessageTypesEventCache {
    /// Create a new view over the given room's event cache, for room messages
    /// of the given `msgtype`s.
    ///
    /// The view is seeded from the store, and kept up to date from there on.
    #[instrument(skip(client, room_event_cache, linked_chunk_update_sender), fields(room_id = %room_event_cache.room_id()))]
    pub(in super::super) async fn new(
        client: &Client,
        room_event_cache: RoomEventCache,
        linked_chunk_update_sender: &LinkedChunkUpdateFanout,
        msgtypes: Vec<String>,
    ) -> Result<Self> {
        let room_id = room_event_cache.room_id().to_owned();
        let msgtypes_refs = msgtypes.iter().map(String::as_str).collect::<Vec<_>>();

        // The store may have to backfill its `msgtype` index the first time a
        // room is queried (rows predating the index): do that outside of the
        // room's state lock, held below, so it doesn't stall the room's sync
        // handling meanwhile. The result is discarded: it's re-queried below,
        // index-only this time, consistently with the update subscription.
        {
            let room_state = room_event_cache.state().read().await?;
            let store = room_state.store.clone();
            drop(room_state);
            store.find_events_by_message_types(&room_id, &msgtypes_refs).await?;
        }

        // Seed under the room's state read lock: linked chunk updates are
        // sent to the store (and to the fanout) under the state write lock,
        // so nothing can slip between the subscription and the snapshot.
        let (state, updates) = {
            let room_state = room_event_cache.state().read().await?;

            let updates = linked_chunk_update_sender.subscribe();

            let linked_chunk_id = OwnedLinkedChunkId::Room(room_id.clone());
            let metadata =
                room_state.store.load_all_chunks_metadata(linked_chunk_id.as_ref()).await?;
            let gaps = room_state.store.load_all_gaps(linked_chunk_id.as_ref()).await?;
            let events =
                room_state.store.find_events_by_message_types(&room_id, &msgtypes_refs).await?;

            let metadata = order_chunk_metadata(metadata);
            let order = OrderTracker::from_metadata(metadata);

            let mut state = State { order, entries: Vec::new(), exposed_from: 0 };

            let mut entries = gaps
                .into_iter()
                .map(|(chunk_id, gap)| Entry::Gap { chunk_id, token: gap.token })
                .chain(events.into_iter().map(|(event, position)| Entry::Event { position, event }))
                .filter_map(|entry| state.key(&entry).map(|key| (key, entry)))
                .collect::<Vec<_>>();
            entries.sort_by_key(|(key, _)| *key);
            state.entries = entries.into_iter().map(|(_, entry)| entry).collect();

            // Expose the newest page.
            state.exposed_from = state.entries.len();
            state.expose_backwards(INITIAL_PAGE_SIZE, &mut Vec::new());

            (state, updates)
        };

        debug!(
            num_entries = state.entries.len(),
            exposed_from = state.exposed_from,
            "Seeded the message-type filtered view"
        );

        let inner = Arc::new(Inner {
            room_id,
            msgtypes,
            room_event_cache,
            state: RwLock::new(state),
            update_sender: Sender::new(32),
            _update_task: std::sync::Mutex::new(None),
        });

        let task = client.task_monitor().spawn_infinite_task(
            "event_cache::message_types_updates",
            update_task(Arc::downgrade(&inner), updates),
        );
        *inner._update_task.lock().unwrap() = Some(task.abort_on_drop());

        Ok(Self { inner })
    }

    /// The room this view is over.
    pub fn room_id(&self) -> &RoomId {
        &self.inner.room_id
    }

    /// The exposed events (oldest first) and the gaps to render, plus a
    /// receiver for subsequent updates.
    pub async fn subscribe(
        &self,
    ) -> (Vec<Event>, Vec<TimelineGap>, Receiver<MessageTypesCacheUpdate>) {
        let state = self.inner.state.read().await;
        (state.exposed_events(), state.timeline_gaps(), self.inner.update_sender.subscribe())
    }

    /// Whether everything the store knows for this room is exposed (there is
    /// nothing older to expose with [`Self::paginate_backwards`], gaps
    /// notwithstanding).
    pub async fn hit_start(&self) -> bool {
        self.inner.state.read().await.exposed_from == 0
    }

    /// The exposed events (oldest first) and the gaps to render.
    pub async fn events_and_gaps(&self) -> (Vec<Event>, Vec<TimelineGap>) {
        let state = self.inner.state.read().await;
        (state.exposed_events(), state.timeline_gaps())
    }

    /// Expose up to `num_events` more (older) events, sent to subscribers as
    /// an update.
    ///
    /// Returns whether everything the store knows for this room is exposed
    /// now (there is nothing older to expose, gaps notwithstanding).
    pub async fn paginate_backwards(&self, num_events: usize) -> Result<bool> {
        let mut state = self.inner.state.write().await;

        let mut diffs = Vec::new();
        state.expose_backwards(num_events, &mut diffs);
        let hit_start = state.exposed_from == 0;

        if !diffs.is_empty() {
            let _ = self.inner.update_sender.send(MessageTypesCacheUpdate {
                diffs,
                origin: EventsOrigin::Cache,
                gaps: state.timeline_gaps(),
            });
        }

        Ok(hit_start)
    }

    /// Resolve one of the room's gaps, fetching up to `batch_size` of the
    /// missing events with a single request to the server; the outcome
    /// reaches this view (and every other one) through the store updates.
    ///
    /// Gaps are resolved by the room event cache, on its in-memory linked
    /// chunk; a gap sitting behind chunks not loaded in memory yet is reached
    /// by loading them first.
    ///
    /// Returns whether the gap was resolved (see
    /// [`RoomEventCache::resolve_gap`]).
    pub async fn resolve_gap(&self, prev_token: String, batch_size: u16) -> Result<bool> {
        self.inner.room_event_cache.load_from_storage_until_gap(&prev_token).await?;
        self.inner.room_event_cache.resolve_gap(prev_token, batch_size).await
    }
}

impl State {
    /// Expose up to `num_events` more events from the held-back prefix; when
    /// no held-back event remains, expose the rest (gaps) too.
    fn expose_backwards(&mut self, num_events: usize, diffs: &mut Vec<VectorDiff<Event>>) {
        let mut exposed = 0;

        while self.exposed_from > 0 && exposed < num_events {
            self.exposed_from -= 1;

            if let Entry::Event { event, .. } = &self.entries[self.exposed_from] {
                exposed += 1;
                diffs.push(VectorDiff::Insert { index: 0, value: event.clone() });
            }
        }

        // Only gaps left before: expose them along, there is nothing to page.
        if self.entries[..self.exposed_from].iter().all(|entry| matches!(entry, Entry::Gap { .. }))
        {
            self.exposed_from = 0;
        }
    }
}

/// Order the chunks' metadata by their links, first chunk first, as
/// [`OrderTracker::from_metadata`] requires (the store returns them
/// unordered).
fn order_chunk_metadata(
    metadata: Vec<matrix_sdk_base::linked_chunk::ChunkMetadata>,
) -> Vec<matrix_sdk_base::linked_chunk::ChunkMetadata> {
    use std::collections::HashMap;

    let mut by_id: HashMap<ChunkIdentifier, matrix_sdk_base::linked_chunk::ChunkMetadata> =
        metadata.into_iter().map(|meta| (meta.identifier, meta)).collect();

    let mut ordered = Vec::with_capacity(by_id.len());

    let mut current =
        by_id.values().find(|meta| meta.previous.is_none()).map(|meta| meta.identifier);

    while let Some(chunk_id) = current {
        let Some(meta) = by_id.remove(&chunk_id) else {
            // A cycle, or a dangling link: stop here rather than loop.
            error!(?chunk_id, "Broken chunk links while ordering the chunk metadata");
            break;
        };
        current = meta.next;
        ordered.push(meta);
    }

    if !by_id.is_empty() {
        error!(
            num_unreachable = by_id.len(),
            "Some chunks are unreachable from the first chunk; ignoring them"
        );
    }

    ordered
}

/// The task applying the room's persisted linked chunk updates to the view.
async fn update_task(
    inner: std::sync::Weak<Inner>,
    mut updates: UnboundedReceiver<RoomEventCacheLinkedChunkUpdate>,
) {
    while let Some(update) = updates.recv().await {
        let Some(inner) = inner.upgrade() else {
            // The view is gone.
            break;
        };

        let OwnedLinkedChunkId::Room(room_id) = &update.linked_chunk_id else {
            continue;
        };
        if *room_id != inner.room_id {
            continue;
        }

        let mut state = inner.state.write().await;

        // Whether the update touches the newest chunk (i.e. comes from
        // sync, as opposed to a back-pagination or a gap resolution): the
        // timeline treats freshly synced events differently from paginated
        // ones.
        let is_at_end = |position: &Position| {
            state.entries.last().is_none_or(|last| {
                state
                    .key(last)
                    .zip(state.order.chunk_ordering(*position))
                    .is_none_or(|(last_key, key)| key >= last_key)
            })
        };
        let origin = if update.updates.iter().any(|update| match update {
            Update::PushItems { at, .. } => is_at_end(at),
            _ => false,
        }) {
            EventsOrigin::Sync
        } else {
            EventsOrigin::Pagination
        };

        let mut diffs = Vec::new();
        for update in &update.updates {
            state.apply(update, &inner.msgtypes, &mut diffs);
        }

        // Even without diffs, the gaps may have changed (a gap resolved into
        // non-matching events only): always let subscribers reconcile.
        let _ = inner.update_sender.send(MessageTypesCacheUpdate {
            diffs,
            origin,
            gaps: state.timeline_gaps(),
        });
    }

    warn!("The linked chunk updates stream ended; the message-type filtered view is frozen");
}

#[cfg(test)]
mod tests;
