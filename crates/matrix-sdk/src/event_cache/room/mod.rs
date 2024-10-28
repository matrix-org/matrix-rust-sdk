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

use std::{collections::BTreeMap, fmt, sync::Arc};

use events::{Gap, RoomEvents};
use matrix_sdk_base::{
    deserialized_responses::{AmbiguityChange, SyncTimelineEvent},
    sync::{JoinedRoomUpdate, LeftRoomUpdate, Timeline},
};
use ruma::{
    events::{
        relation::RelationType,
        room::{message::Relation, redaction::SyncRoomRedactionEvent},
        AnyMessageLikeEventContent, AnyRoomAccountDataEvent, AnySyncEphemeralRoomEvent,
        AnySyncMessageLikeEvent, AnySyncTimelineEvent,
    },
    serde::Raw,
    EventId, OwnedEventId, OwnedRoomId,
};
use tokio::sync::{
    broadcast::{Receiver, Sender},
    Notify, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
use tracing::{trace, warn};

use super::{
    paginator::{Paginator, PaginatorState},
    AllEventsCache, EventsOrigin, Result, RoomEventCacheUpdate, RoomPagination,
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

impl RoomEventCache {
    /// Create a new [`RoomEventCache`] using the given room and store.
    pub(super) fn new(
        client: WeakClient,
        room_id: OwnedRoomId,
        all_events_cache: Arc<RwLock<AllEventsCache>>,
    ) -> Self {
        Self { inner: Arc::new(RoomEventCacheInner::new(client, room_id, all_events_cache)) }
    }

    /// Subscribe to room updates for this room, after getting the initial list
    /// of events. XXX: Could/should it use some kind of `Observable`
    /// instead? Or not something async, like explicit handlers as our event
    /// handlers?
    pub async fn subscribe(
        &self,
    ) -> Result<(Vec<SyncTimelineEvent>, Receiver<RoomEventCacheUpdate>)> {
        let state = self.inner.state.read().await;
        let events = state.events.events().map(|(_position, item)| item.clone()).collect();

        Ok((events, self.inner.sender.subscribe()))
    }

    /// Return a [`RoomPagination`] API object useful for running
    /// back-pagination queries in the current room.
    pub fn pagination(&self) -> RoomPagination {
        RoomPagination { inner: self.inner.clone() }
    }

    /// Try to find an event by id in this room.
    pub async fn event(&self, event_id: &EventId) -> Option<SyncTimelineEvent> {
        if let Some((room_id, event)) =
            self.inner.all_events.read().await.events.get(event_id).cloned()
        {
            if room_id == self.inner.room_id {
                return Some(event);
            }
        }

        let state = self.inner.state.read().await;
        for (_pos, event) in state.events.revents() {
            if event.event_id().as_deref() == Some(event_id) {
                return Some(event.clone());
            }
        }
        None
    }

    /// Try to find an event by id in this room, along with its related events.
    ///
    /// You can filter which types of related events to retrieve using
    /// `filter`. `None` will retrieve related events of any type.
    pub async fn event_with_relations(
        &self,
        event_id: &EventId,
        filter: Option<Vec<RelationType>>,
    ) -> Option<(SyncTimelineEvent, Vec<SyncTimelineEvent>)> {
        let mut relation_events = Vec::new();

        let cache = self.inner.all_events.read().await;
        if let Some((_, event)) = cache.events.get(event_id) {
            Self::collect_related_events(&cache, event_id, &filter, &mut relation_events);
            Some((event.clone(), relation_events))
        } else {
            None
        }
    }

    /// Looks for related event ids for the passed event id, and appends them to
    /// the `results` parameter. Then it'll recursively get the related
    /// event ids for those too.
    fn collect_related_events(
        cache: &RwLockReadGuard<'_, AllEventsCache>,
        event_id: &EventId,
        filter: &Option<Vec<RelationType>>,
        results: &mut Vec<SyncTimelineEvent>,
    ) {
        if let Some(related_event_ids) = cache.relations.get(event_id) {
            for (related_event_id, relation_type) in related_event_ids {
                if let Some(filter) = filter {
                    if !filter.contains(relation_type) {
                        continue;
                    }
                }

                // If the event was already added to the related ones, skip it.
                if results.iter().any(|e| {
                    e.event_id().is_some_and(|added_related_event_id| {
                        added_related_event_id == *related_event_id
                    })
                }) {
                    continue;
                }
                if let Some((_, ev)) = cache.events.get(related_event_id) {
                    results.push(ev.clone());
                    Self::collect_related_events(cache, related_event_id, filter, results);
                }
            }
        }
    }

    /// Save a single event in the event cache, for further retrieval with
    /// [`Self::event`].
    // TODO: This doesn't insert the event into the linked chunk. In the future
    // there'll be no distinction between the linked chunk and the separate
    // cache. There is a discussion in https://github.com/matrix-org/matrix-rust-sdk/issues/3886.
    pub(crate) async fn save_event(&self, event: SyncTimelineEvent) {
        if let Some(event_id) = event.event_id() {
            let mut cache = self.inner.all_events.write().await;

            self.inner.append_related_event(&mut cache, &event);
            cache.events.insert(event_id, (self.inner.room_id.clone(), event));
        } else {
            warn!("couldn't save event without event id in the event cache");
        }
    }

    /// Save some events in the event cache, for further retrieval with
    /// [`Self::event`]. This function will save them using a single lock,
    /// as opposed to [`Self::save_event`].
    // TODO: This doesn't insert the event into the linked chunk. In the future
    // there'll be no distinction between the linked chunk and the separate
    // cache. There is a discussion in https://github.com/matrix-org/matrix-rust-sdk/issues/3886.
    pub(crate) async fn save_events(&self, events: impl IntoIterator<Item = SyncTimelineEvent>) {
        let mut cache = self.inner.all_events.write().await;
        for event in events {
            if let Some(event_id) = event.event_id() {
                self.inner.append_related_event(&mut cache, &event);
                cache.events.insert(event_id, (self.inner.room_id.clone(), event));
            } else {
                warn!("couldn't save event without event id in the event cache");
            }
        }
    }
}

/// The (non-cloneable) details of the `RoomEventCache`.
pub(super) struct RoomEventCacheInner {
    /// The room id for this room.
    room_id: OwnedRoomId,

    /// Sender part for subscribers to this room.
    pub sender: Sender<RoomEventCacheUpdate>,

    /// State for this room's event cache.
    pub state: RwLock<RoomEventCacheState>,

    /// See comment of [`EventCacheInner::all_events`].
    ///
    /// This is shared between the [`EventCacheInner`] singleton and all
    /// [`RoomEventCacheInner`] instances.
    all_events: Arc<RwLock<AllEventsCache>>,

    /// A notifier that we received a new pagination token.
    pub pagination_batch_token_notifier: Notify,

    /// A paginator instance, that's configured to run back-pagination on our
    /// behalf.
    ///
    /// Note: forward-paginations are still run "out-of-band", that is,
    /// disconnected from the event cache, as we don't implement matching
    /// events received from those kinds of pagination with the cache. This
    /// paginator is only used for queries that interact with the actual event
    /// cache.
    pub paginator: Paginator<WeakRoom>,
}

impl RoomEventCacheInner {
    /// Creates a new cache for a room, and subscribes to room updates, so as
    /// to handle new timeline events.
    fn new(
        client: WeakClient,
        room_id: OwnedRoomId,
        all_events_cache: Arc<RwLock<AllEventsCache>>,
    ) -> Self {
        let sender = Sender::new(32);

        let weak_room = WeakRoom::new(client, room_id);

        Self {
            room_id: weak_room.room_id().to_owned(),
            state: RwLock::new(RoomEventCacheState {
                events: RoomEvents::default(),
                waited_for_initial_prev_token: false,
            }),
            all_events: all_events_cache,
            sender,
            pagination_batch_token_notifier: Default::default(),
            paginator: Paginator::new(weak_room),
        }
    }

    fn handle_account_data(&self, account_data: Vec<Raw<AnyRoomAccountDataEvent>>) {
        let mut handled_read_marker = false;

        trace!("Handling account data");

        for raw_event in account_data {
            match raw_event.deserialize() {
                Ok(AnyRoomAccountDataEvent::FullyRead(ev)) => {
                    // Sometimes the sliding sync proxy sends many duplicates of the read marker
                    // event. Don't forward it multiple times to avoid clutter
                    // the update channel.
                    //
                    // NOTE: SS proxy workaround.
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

    async fn handle_timeline(
        &self,
        timeline: Timeline,
        ephemeral_events: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    ) -> Result<()> {
        if timeline.limited {
            // Ideally we'd try to reconcile existing events against those received in the
            // timeline, but we're not there yet. In the meanwhile, clear the
            // items from the room. TODO: implement Smart Matching™.
            trace!("limited timeline, clearing all previous events and pushing new events");

            self.replace_all_events_by(
                timeline.events,
                timeline.prev_batch,
                ephemeral_events,
                ambiguity_changes,
            )
            .await?;
        } else {
            // Add all the events to the backend.
            trace!("adding new events");

            self.append_new_events(
                timeline.events,
                timeline.prev_batch,
                ephemeral_events,
                ambiguity_changes,
            )
            .await?;
        }

        Ok(())
    }

    pub(super) async fn handle_left_room_update(&self, updates: LeftRoomUpdate) -> Result<()> {
        self.handle_timeline(updates.timeline, Vec::new(), updates.ambiguity_changes).await?;
        Ok(())
    }

    /// Remove existing events, and append a set of events to the room cache and
    /// storage, notifying observers.
    pub(super) async fn replace_all_events_by(
        &self,
        sync_timeline_events: Vec<SyncTimelineEvent>,
        prev_batch: Option<String>,
        ephemeral_events: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    ) -> Result<()> {
        // Acquire the lock.
        let mut state = self.state.write().await;

        // Reset the room's state.
        state.reset();

        // Propagate to observers.
        let _ = self.sender.send(RoomEventCacheUpdate::Clear);

        // Push the new events.
        self.append_events_locked_impl(
            &mut state.events,
            sync_timeline_events,
            prev_batch.clone(),
            ephemeral_events,
            ambiguity_changes,
        )
        .await?;

        // Reset the paginator status to initial.
        self.paginator.set_idle_state(PaginatorState::Initial, prev_batch, None)?;

        Ok(())
    }

    /// Append a set of events to the room cache and storage, notifying
    /// observers.
    async fn append_new_events(
        &self,
        sync_timeline_events: Vec<SyncTimelineEvent>,
        prev_batch: Option<String>,
        ephemeral_events: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    ) -> Result<()> {
        self.append_events_locked_impl(
            &mut self.state.write().await.events,
            sync_timeline_events,
            prev_batch,
            ephemeral_events,
            ambiguity_changes,
        )
        .await
    }

    /// If the event is related to another one, its id is added to the
    /// relations map.
    fn append_related_event(
        &self,
        cache: &mut RwLockWriteGuard<'_, AllEventsCache>,
        event: &SyncTimelineEvent,
    ) {
        // Handle and cache events and relations.
        if let Ok(AnySyncTimelineEvent::MessageLike(ev)) = event.raw().deserialize() {
            // Handle redactions separately, as their logic is slightly different.
            if let AnySyncMessageLikeEvent::RoomRedaction(SyncRoomRedactionEvent::Original(ev)) =
                &ev
            {
                if let Some(redacted_event_id) = ev.content.redacts.as_ref().or(ev.redacts.as_ref())
                {
                    cache
                        .relations
                        .entry(redacted_event_id.to_owned())
                        .or_default()
                        .insert(ev.event_id.to_owned(), RelationType::Replacement);
                }
            } else {
                let relationship = match ev.original_content() {
                    Some(AnyMessageLikeEventContent::RoomMessage(c)) => {
                        if let Some(relation) = c.relates_to {
                            match relation {
                                Relation::Replacement(replacement) => {
                                    Some((replacement.event_id, RelationType::Replacement))
                                }
                                Relation::Reply { in_reply_to } => {
                                    Some((in_reply_to.event_id, RelationType::Reference))
                                }
                                Relation::Thread(thread) => {
                                    Some((thread.event_id, RelationType::Thread))
                                }
                                // Do nothing for custom
                                _ => None,
                            }
                        } else {
                            None
                        }
                    }
                    Some(AnyMessageLikeEventContent::PollResponse(c)) => {
                        Some((c.relates_to.event_id, RelationType::Reference))
                    }
                    Some(AnyMessageLikeEventContent::PollEnd(c)) => {
                        Some((c.relates_to.event_id, RelationType::Reference))
                    }
                    Some(AnyMessageLikeEventContent::UnstablePollResponse(c)) => {
                        Some((c.relates_to.event_id, RelationType::Reference))
                    }
                    Some(AnyMessageLikeEventContent::UnstablePollEnd(c)) => {
                        Some((c.relates_to.event_id, RelationType::Reference))
                    }
                    Some(AnyMessageLikeEventContent::Reaction(c)) => {
                        Some((c.relates_to.event_id, RelationType::Annotation))
                    }
                    _ => None,
                };

                if let Some(relationship) = relationship {
                    cache
                        .relations
                        .entry(relationship.0)
                        .or_default()
                        .insert(ev.event_id().to_owned(), relationship.1);
                }
            }
        }
    }

    /// Append a set of events and associated room data.
    ///
    /// This is a private implementation. It must not be exposed publicly.
    async fn append_events_locked_impl(
        &self,
        room_events: &mut RoomEvents,
        sync_timeline_events: Vec<SyncTimelineEvent>,
        prev_batch: Option<String>,
        ephemeral_events: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    ) -> Result<()> {
        if sync_timeline_events.is_empty()
            && prev_batch.is_none()
            && ephemeral_events.is_empty()
            && ambiguity_changes.is_empty()
        {
            return Ok(());
        }

        // Add the previous back-pagination token (if present), followed by the timeline
        // events themselves.
        {
            if let Some(prev_token) = &prev_batch {
                room_events.push_gap(Gap { prev_token: prev_token.clone() });
            }

            room_events.push_events(sync_timeline_events.clone());

            let mut cache = self.all_events.write().await;
            for ev in &sync_timeline_events {
                if let Some(event_id) = ev.event_id() {
                    self.append_related_event(&mut cache, ev);
                    cache.events.insert(event_id.to_owned(), (self.room_id.clone(), ev.clone()));
                }
            }
        }

        // Now that all events have been added, we can trigger the
        // `pagination_token_notifier`.
        if prev_batch.is_some() {
            self.pagination_batch_token_notifier.notify_one();
        }

        // The order of `RoomEventCacheUpdate`s is **really** important here.
        {
            if !sync_timeline_events.is_empty() {
                let _ = self.sender.send(RoomEventCacheUpdate::AddTimelineEvents {
                    events: sync_timeline_events,
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

/// State for a single room's event cache.
///
/// This contains all inner mutable state that ought to be updated at the same
/// time.
pub(super) struct RoomEventCacheState {
    /// The events of the room.
    pub events: RoomEvents,

    /// Have we ever waited for a previous-batch-token to come from sync, in the
    /// context of pagination? We do this at most once per room, the first
    /// time we try to run backward pagination. We reset that upon clearing
    /// the timeline events.
    pub waited_for_initial_prev_token: bool,
}

impl RoomEventCacheState {
    /// Resets this data structure as if it were brand new.
    pub(super) fn reset(&mut self) {
        self.events.reset();
        self.waited_for_initial_prev_token = false;
    }
}
