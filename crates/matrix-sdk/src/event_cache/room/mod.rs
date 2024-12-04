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

use events::Gap;
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
        state: RoomEventCacheState,
        room_id: OwnedRoomId,
        all_events_cache: Arc<RwLock<AllEventsCache>>,
    ) -> Self {
        Self { inner: Arc::new(RoomEventCacheInner::new(client, state, room_id, all_events_cache)) }
    }

    /// Subscribe to room updates for this room, after getting the initial list
    /// of events. XXX: Could/should it use some kind of `Observable`
    /// instead? Or not something async, like explicit handlers as our event
    /// handlers?
    pub async fn subscribe(
        &self,
    ) -> Result<(Vec<SyncTimelineEvent>, Receiver<RoomEventCacheUpdate>)> {
        let state = self.inner.state.read().await;
        let events = state.events().events().map(|(_position, item)| item.clone()).collect();

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
        for (_pos, event) in state.events().revents() {
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
        state: RoomEventCacheState,
        room_id: OwnedRoomId,
        all_events_cache: Arc<RwLock<AllEventsCache>>,
    ) -> Self {
        let sender = Sender::new(32);
        let weak_room = WeakRoom::new(client, room_id);
        Self {
            room_id: weak_room.room_id().to_owned(),
            state: RwLock::new(state),
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

    pub(super) async fn handle_joined_room_update(
        &self,
        has_storage: bool,
        updates: JoinedRoomUpdate,
    ) -> Result<()> {
        self.handle_timeline(
            has_storage,
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
        has_storage: bool,
        timeline: Timeline,
        ephemeral_events: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    ) -> Result<()> {
        if !has_storage && timeline.limited {
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

    pub(super) async fn handle_left_room_update(
        &self,
        has_storage: bool,
        updates: LeftRoomUpdate,
    ) -> Result<()> {
        self.handle_timeline(has_storage, updates.timeline, Vec::new(), updates.ambiguity_changes)
            .await?;
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
        state.reset().await?;

        // Propagate to observers.
        let _ = self.sender.send(RoomEventCacheUpdate::Clear);

        // Push the new events.
        self.append_events_locked_impl(
            &mut state,
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
        let mut state = self.state.write().await;
        self.append_events_locked_impl(
            &mut state,
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
        state: &mut RoomEventCacheState,
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
            state
                .with_events_mut(|room_events| {
                    if let Some(prev_token) = &prev_batch {
                        room_events.push_gap(Gap { prev_token: prev_token.clone() });
                    }

                    room_events.push_events(sync_timeline_events.clone());
                })
                .await?;

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

// Use a private module to hide `events` to this parent module.
mod private {
    use std::sync::Arc;

    use matrix_sdk_base::{
        deserialized_responses::{SyncTimelineEvent, TimelineEventKind},
        event_cache::store::EventCacheStoreLock,
        linked_chunk::Update,
    };
    use once_cell::sync::OnceCell;
    use ruma::{serde::Raw, OwnedRoomId};

    use super::events::RoomEvents;
    use crate::event_cache::EventCacheError;

    /// State for a single room's event cache.
    ///
    /// This contains all the inner mutable states that ought to be updated at
    /// the same time.
    pub struct RoomEventCacheState {
        /// The room this state relates to.
        room: OwnedRoomId,

        /// Reference to the underlying backing store.
        ///
        /// Set to none if the room shouldn't read the linked chunk from
        /// storage, and shouldn't store updates to storage.
        store: Arc<OnceCell<EventCacheStoreLock>>,

        /// The events of the room.
        events: RoomEvents,

        /// Have we ever waited for a previous-batch-token to come from sync, in
        /// the context of pagination? We do this at most once per room,
        /// the first time we try to run backward pagination. We reset
        /// that upon clearing the timeline events.
        pub waited_for_initial_prev_token: bool,
    }

    impl RoomEventCacheState {
        /// Create a new state, or reload it from storage if it's been enabled.
        pub async fn new(
            room: OwnedRoomId,
            store: Arc<OnceCell<EventCacheStoreLock>>,
        ) -> Result<Self, EventCacheError> {
            let events = if let Some(store) = store.get() {
                let locked = store.lock().await?;
                let chunks = locked.reload_linked_chunk(&room).await?;
                RoomEvents::with_initial_chunks(chunks)
            } else {
                RoomEvents::default()
            };

            Ok(Self { room, store, events, waited_for_initial_prev_token: false })
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
                    *event = Raw::new(&val).ok()?.cast();
                }
                None
            };
            let _ = closure();
        }

        /// Strips the bundled relations from a collection of events.
        fn strip_relations_from_events(items: &mut [SyncTimelineEvent]) {
            for ev in items.iter_mut() {
                match &mut ev.kind {
                    TimelineEventKind::Decrypted(decrypted) => {
                        // Remove all information about encryption info for
                        // the bundled events.
                        decrypted.unsigned_encryption_info = None;

                        // Remove the `unsigned`/`m.relations` field, if needs be.
                        Self::strip_relations_if_present(&mut decrypted.event);
                    }

                    TimelineEventKind::UnableToDecrypt { event, .. }
                    | TimelineEventKind::PlainText { event } => {
                        Self::strip_relations_if_present(event);
                    }
                }
            }
        }

        /// Propagate changes to the underlying storage.
        async fn propagate_changes(&mut self) -> Result<(), EventCacheError> {
            let mut updates = self.events.updates().take();

            if !updates.is_empty() {
                if let Some(store) = self.store.get() {
                    let locked = store.lock().await?;

                    // Strip relations from the `PushItems` updates.
                    for up in updates.iter_mut() {
                        match up {
                            Update::PushItems { items, .. } => {
                                Self::strip_relations_from_events(items)
                            }
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

                    locked.handle_linked_chunk_updates(&self.room, updates).await?;
                }
            }

            Ok(())
        }

        /// Resets this data structure as if it were brand new.
        pub async fn reset(&mut self) -> Result<(), EventCacheError> {
            self.events.reset();
            self.propagate_changes().await?;
            self.waited_for_initial_prev_token = false;
            Ok(())
        }

        /// Returns a read-only reference to the underlying events.
        pub fn events(&self) -> &RoomEvents {
            &self.events
        }

        /// Gives a temporary mutable handle to the underlying in-memory events,
        /// and will propagate changes to the storage once done.
        pub async fn with_events_mut<O, F: FnOnce(&mut RoomEvents) -> O>(
            &mut self,
            func: F,
        ) -> Result<O, EventCacheError> {
            let output = func(&mut self.events);
            self.propagate_changes().await?;
            Ok(output)
        }
    }
}

pub(super) use private::RoomEventCacheState;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use assert_matches::assert_matches;
    use assert_matches2::assert_let;
    use matrix_sdk_base::{
        event_cache::{
            store::{EventCacheStore as _, MemoryStore},
            Gap,
        },
        linked_chunk::{ChunkContent, ChunkIdentifier, Position, Update},
        store::StoreConfig,
        sync::{JoinedRoomUpdate, Timeline},
    };
    use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
    use matrix_sdk_test::{async_test, event_factory::EventFactory, ALICE, BOB};
    use ruma::{
        event_id,
        events::{
            relation::RelationType, room::message::RoomMessageEventContentWithoutRelation,
            AnySyncMessageLikeEvent, AnySyncTimelineEvent,
        },
        room_id, user_id, RoomId,
    };

    use crate::test_utils::{client::MockClientBuilder, logged_in_client};

    #[async_test]
    async fn test_event_with_redaction_relation() {
        let original_id = event_id!("$original");
        let related_id = event_id!("$related");
        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        assert_relations(
            room_id,
            f.text_msg("Original event").event_id(original_id).into(),
            f.redaction(original_id).event_id(related_id).into(),
            f,
        )
        .await;
    }

    #[async_test]
    async fn test_event_with_edit_relation() {
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
    async fn test_event_with_reply_relation() {
        let original_id = event_id!("$original");
        let related_id = event_id!("$related");
        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        assert_relations(
            room_id,
            f.text_msg("Original event").event_id(original_id).into(),
            f.text_msg("A reply").reply_to(original_id).event_id(related_id).into(),
            f,
        )
        .await;
    }

    #[async_test]
    async fn test_event_with_thread_reply_relation() {
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
    async fn test_event_with_reaction_relation() {
        let original_id = event_id!("$original");
        let related_id = event_id!("$related");
        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        assert_relations(
            room_id,
            f.text_msg("Original event").event_id(original_id).into(),
            f.reaction(original_id, ":D".to_owned()).event_id(related_id).into(),
            f,
        )
        .await;
    }

    #[async_test]
    async fn test_event_with_poll_response_relation() {
        let original_id = event_id!("$original");
        let related_id = event_id!("$related");
        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        assert_relations(
            room_id,
            f.poll_start("Poll start event", "A poll question", vec!["An answer"])
                .event_id(original_id)
                .into(),
            f.poll_response("1", original_id).event_id(related_id).into(),
            f,
        )
        .await;
    }

    #[async_test]
    async fn test_event_with_poll_end_relation() {
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
    async fn test_event_with_filtered_relationships() {
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
            event_factory.redaction(related_id).event_id(associated_related_id).into();

        let client = logged_in_client(None).await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Save the original event.
        room_event_cache.save_event(original_event).await;

        // Save the related event.
        room_event_cache.save_event(related_event).await;

        // Save the associated related event, which redacts the related event.
        room_event_cache.save_event(associated_related_event).await;

        let filter = Some(vec![RelationType::Replacement]);
        let (event, related_events) =
            room_event_cache.event_with_relations(original_id, filter).await.unwrap();
        // Fetched event is the right one.
        let cached_event_id = event.event_id().unwrap();
        assert_eq!(cached_event_id, original_id);

        // There are both the related id and the associatively related id
        assert_eq!(related_events.len(), 2);

        let related_event_id = related_events[0].event_id().unwrap();
        assert_eq!(related_event_id, related_id);
        let related_event_id = related_events[1].event_id().unwrap();
        assert_eq!(related_event_id, associated_related_id);

        // Now we'll filter threads instead, there should be no related events
        let filter = Some(vec![RelationType::Thread]);
        let (event, related_events) =
            room_event_cache.event_with_relations(original_id, filter).await.unwrap();
        // Fetched event is the right one.
        let cached_event_id = event.event_id().unwrap();
        assert_eq!(cached_event_id, original_id);
        // No Thread related events found
        assert!(related_events.is_empty());
    }

    #[async_test]
    async fn test_event_with_recursive_relation() {
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
            event_factory.redaction(related_id).event_id(associated_related_id).into();

        let client = logged_in_client(None).await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Save the original event.
        room_event_cache.save_event(original_event).await;

        // Save the related event.
        room_event_cache.save_event(related_event).await;

        // Save the associated related event, which redacts the related event.
        room_event_cache.save_event(associated_related_event).await;

        let (event, related_events) =
            room_event_cache.event_with_relations(original_id, None).await.unwrap();
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

    #[cfg(not(target_arch = "wasm32"))] // This uses the cross-process lock, so needs time support.
    #[async_test]
    async fn test_write_to_storage() {
        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        let event_cache_store = Arc::new(MemoryStore::new());

        let client = MockClientBuilder::new("http://localhost".to_owned())
            .store_config(
                StoreConfig::new("hodlor".to_owned()).event_cache_store(event_cache_store.clone()),
            )
            .build()
            .await;

        let event_cache = client.event_cache();

        // Don't forget to subscribe and like^W enable storage!
        event_cache.subscribe().unwrap();
        event_cache.enable_storage().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Propagate an update for a message and a prev-batch token.
        let timeline = Timeline {
            limited: false,
            prev_batch: Some("raclette".to_owned()),
            events: vec![f.text_msg("hey yo").sender(*ALICE).into_sync()],
        };

        room_event_cache
            .inner
            .handle_joined_room_update(true, JoinedRoomUpdate { timeline, ..Default::default() })
            .await
            .unwrap();

        let linked_chunk = event_cache_store.reload_linked_chunk(room_id).await.unwrap().unwrap();

        assert_eq!(linked_chunk.chunks().count(), 3);

        let mut chunks = linked_chunk.chunks();

        // Invariant: there's always an empty items chunk at the beginning.
        assert_matches!(chunks.next().unwrap().content(), ChunkContent::Items(events) => {
            assert_eq!(events.len(), 0)
        });

        // Then we have the gap.
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

    #[cfg(not(target_arch = "wasm32"))] // This uses the cross-process lock, so needs time support.
    #[async_test]
    async fn test_write_to_storage_strips_bundled_relations() {
        use ruma::events::BundledMessageLikeRelations;

        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        let event_cache_store = Arc::new(MemoryStore::new());

        let client = MockClientBuilder::new("http://localhost".to_owned())
            .store_config(
                StoreConfig::new("hodlor".to_owned()).event_cache_store(event_cache_store.clone()),
            )
            .build()
            .await;

        let event_cache = client.event_cache();

        // Don't forget to subscribe and like^W enable storage!
        event_cache.subscribe().unwrap();
        event_cache.enable_storage().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Propagate an update for a message with bundled relations.
        let mut relations = BundledMessageLikeRelations::new();
        relations.replace =
            Some(Box::new(f.text_msg("Hello, Kind Sir").sender(*ALICE).into_raw_sync()));
        let ev = f.text_msg("hey yo").sender(*ALICE).bundled_relations(relations).into_sync();

        let timeline = Timeline { limited: false, prev_batch: None, events: vec![ev] };

        room_event_cache
            .inner
            .handle_joined_room_update(true, JoinedRoomUpdate { timeline, ..Default::default() })
            .await
            .unwrap();

        // The in-memory linked chunk keeps the bundled relation.
        {
            let (events, _) = room_event_cache.subscribe().await.unwrap();

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
        let linked_chunk = event_cache_store.reload_linked_chunk(room_id).await.unwrap().unwrap();

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

    #[cfg(not(target_arch = "wasm32"))] // This uses the cross-process lock, so needs time support.
    #[async_test]
    async fn test_load_from_storage() {
        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        let event_cache_store = Arc::new(MemoryStore::new());

        let event_id1 = event_id!("$1");
        let event_id2 = event_id!("$2");

        let ev1 = f.text_msg("hello world").sender(*ALICE).event_id(event_id1).into_sync();
        let ev2 = f.text_msg("how's it going").sender(*BOB).event_id(event_id2).into_sync();

        // Prefill the store with some data.
        event_cache_store
            .handle_linked_chunk_updates(
                room_id,
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

        let client = MockClientBuilder::new("http://localhost".to_owned())
            .store_config(
                StoreConfig::new("hodlor".to_owned()).event_cache_store(event_cache_store.clone()),
            )
            .build()
            .await;

        let event_cache = client.event_cache();

        // Don't forget to subscribe and like^W enable storage!
        event_cache.subscribe().unwrap();
        event_cache.enable_storage().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        let (items, _stream) = room_event_cache.subscribe().await.unwrap();

        // The reloaded room must contain the two events.
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].event_id().unwrap(), event_id1);
        assert_eq!(items[1].event_id().unwrap(), event_id2);

        // A new update with one of these events leads to deduplication.
        let timeline = Timeline { limited: false, prev_batch: None, events: vec![ev1] };
        room_event_cache
            .inner
            .handle_joined_room_update(true, JoinedRoomUpdate { timeline, ..Default::default() })
            .await
            .unwrap();

        // The stream doesn't report these changes *yet*. Use the items vector given
        // when subscribing, to check that the items correspond to their new
        // positions. The duplicated item is removed (so it's not the first
        // element anymore), and it's added to the back of the list.
        let (items, _stream) = room_event_cache.subscribe().await.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].event_id().unwrap(), event_id2);
        assert_eq!(items[1].event_id().unwrap(), event_id1);
    }

    async fn assert_relations(
        room_id: &RoomId,
        original_event: SyncTimelineEvent,
        related_event: SyncTimelineEvent,
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
        room_event_cache.save_event(original_event).await;

        // Save an unrelated event to check it's not in the related events list.
        let unrelated_id = event_id!("$2");
        room_event_cache
            .save_event(event_factory.text_msg("An unrelated event").event_id(unrelated_id).into())
            .await;

        // Save the related event.
        let related_id = related_event.event_id().unwrap();
        room_event_cache.save_event(related_event).await;

        let (event, related_events) =
            room_event_cache.event_with_relations(&original_event_id, None).await.unwrap();
        // Fetched event is the right one.
        let cached_event_id = event.event_id().unwrap();
        assert_eq!(cached_event_id, original_event_id);

        // There is only the actually related event in the related ones
        assert_eq!(related_events.len(), 1);
        let related_event_id = related_events[0].event_id().unwrap();
        assert_eq!(related_event_id, related_id);
    }
}
