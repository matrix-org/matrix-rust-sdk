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

pub mod pagination;
mod state;
mod subscriber;

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, atomic::Ordering},
};

use eyeball::SharedObservable;
use matrix_sdk_base::{
    deserialized_responses::AmbiguityChange,
    event_cache::Event,
    linked_chunk::Position,
    sync::{JoinedRoomUpdate, LeftRoomUpdate, Timeline},
};
use ruma::{
    EventId, OwnedEventId, OwnedRoomId, RoomId,
    events::{AnyRoomAccountDataEvent, AnySyncEphemeralRoomEvent, relation::RelationType},
    serde::Raw,
};
pub(in super::super) use state::RoomEventCacheStateLock;
pub use subscriber::RoomEventCacheSubscriber;
use tokio::sync::{
    Notify,
    broadcast::{Receiver, Sender},
    mpsc,
};
use tracing::{instrument, trace, warn};

use super::{
    super::{
        AutoShrinkChannelPayload, EventCacheError, EventsOrigin, PaginationStatus, Result,
        RoomEventCacheGenericUpdate, RoomEventCacheUpdate, RoomPagination,
    },
    TimelineVectorDiffs,
    event_linked_chunk::sort_positions_descending,
    thread::pagination::ThreadPagination,
};
use crate::{client::WeakClient, room::WeakRoom};

/// A subset of an event cache, for a room.
///
/// Cloning is shallow, and thus is cheap to do.
#[derive(Clone)]
pub struct RoomEventCache {
    pub(in super::super) inner: Arc<RoomEventCacheInner>,
}

impl fmt::Debug for RoomEventCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RoomEventCache").finish_non_exhaustive()
    }
}

impl RoomEventCache {
    /// Create a new [`RoomEventCache`] using the given room and store.
    pub(in super::super) fn new(
        client: WeakClient,
        state: RoomEventCacheStateLock,
        pagination_status: SharedObservable<PaginationStatus>,
        room_id: OwnedRoomId,
        auto_shrink_sender: mpsc::Sender<AutoShrinkChannelPayload>,
        update_sender: Sender<RoomEventCacheUpdate>,
        generic_update_sender: Sender<RoomEventCacheGenericUpdate>,
    ) -> Self {
        Self {
            inner: Arc::new(RoomEventCacheInner::new(
                client,
                state,
                pagination_status,
                room_id,
                auto_shrink_sender,
                update_sender,
                generic_update_sender,
            )),
        }
    }

    /// Get the room ID for this [`RoomEventCache`].
    pub fn room_id(&self) -> &RoomId {
        &self.inner.room_id
    }

    /// Read all current events.
    ///
    /// Use [`RoomEventCache::subscribe`] to get all current events, plus a
    /// subscriber.
    pub async fn events(&self) -> Result<Vec<Event>> {
        let state = self.inner.state.read().await?;

        Ok(state.room_linked_chunk().events().map(|(_position, item)| item.clone()).collect())
    }

    /// Subscribe to this room updates, after getting the initial list of
    /// events.
    ///
    /// Use [`RoomEventCache::events`] to get all current events without the
    /// subscriber. Creating, and especially dropping, a
    /// [`RoomEventCacheSubscriber`] isn't free, as it triggers side-effects.
    pub async fn subscribe(&self) -> Result<(Vec<Event>, RoomEventCacheSubscriber)> {
        let state = self.inner.state.read().await?;
        let events =
            state.room_linked_chunk().events().map(|(_position, item)| item.clone()).collect();

        let subscriber_count = state.subscriber_count();
        let previous_subscriber_count = subscriber_count.fetch_add(1, Ordering::SeqCst);
        trace!("added a room event cache subscriber; new count: {}", previous_subscriber_count + 1);

        let subscriber = RoomEventCacheSubscriber::new(
            self.inner.update_sender.subscribe(),
            self.inner.room_id.clone(),
            self.inner.auto_shrink_sender.clone(),
            subscriber_count.clone(),
        );

        Ok((events, subscriber))
    }

    /// Subscribe to thread for a given root event, and get a (maybe empty)
    /// initially known list of events for that thread.
    pub async fn subscribe_to_thread(
        &self,
        thread_root: OwnedEventId,
    ) -> Result<(Vec<Event>, Receiver<TimelineVectorDiffs>)> {
        let mut state = self.inner.state.write().await?;
        Ok(state.subscribe_to_thread(thread_root))
    }

    /// Subscribe to the pinned event cache for this room.
    ///
    /// This is a persisted view over the pinned events of a room.
    ///
    /// The pinned events will be initially reloaded from storage, and/or loaded
    /// from a network request to fetch the latest pinned events and their
    /// relations, to update it as needed. The list of pinned events will
    /// also be kept up-to-date as new events are pinned, and new
    /// related events show up from other sources.
    pub async fn subscribe_to_pinned_events(
        &self,
    ) -> Result<(Vec<Event>, Receiver<TimelineVectorDiffs>)> {
        let room = self.inner.weak_room.get().ok_or(EventCacheError::ClientDropped)?;
        let state = self.inner.state.read().await?;

        state.subscribe_to_pinned_events(room).await
    }

    /// Return a [`RoomPagination`] API object useful for running
    /// back-pagination queries in the current room.
    pub fn pagination(&self) -> RoomPagination {
        RoomPagination::new(self.inner.clone())
    }

    /// Return a `ThreadPagination` API object useful for running
    /// back-pagination queries in the `thread_id` thread.
    pub fn thread_pagination(&self, thread_id: OwnedEventId) -> ThreadPagination {
        ThreadPagination::new(self.inner.clone(), thread_id)
    }

    /// Try to find a single event in this room, starting from the most recent
    /// event.
    ///
    /// The `predicate` receives the current event as its single argument.
    ///
    /// **Warning**! It looks into the loaded events from the in-memory linked
    /// chunk **only**. It doesn't look inside the storage.
    pub async fn rfind_map_event_in_memory_by<O, P>(&self, predicate: P) -> Result<Option<O>>
    where
        P: FnMut(&Event) -> Option<O>,
    {
        Ok(self.inner.state.read().await?.rfind_map_event_in_memory_by(predicate))
    }

    /// Try to find an event by ID in this room.
    ///
    /// It starts by looking into loaded events before looking inside the
    /// storage.
    pub async fn find_event(&self, event_id: &EventId) -> Result<Option<Event>> {
        Ok(self
            .inner
            .state
            .read()
            .await?
            .find_event(event_id)
            .await
            .ok()
            .flatten()
            .map(|(_loc, event)| event))
    }

    /// Try to find an event by ID in this room, along with its related events.
    ///
    /// You can filter which types of related events to retrieve using
    /// `filter`. `None` will retrieve related events of any type.
    ///
    /// The related events are sorted like this:
    ///
    /// - events saved out-of-band (with `RoomEventCache::save_events`) will be
    ///   located at the beginning of the array.
    /// - events present in the linked chunk (be it in memory or in the storage)
    ///   will be sorted according to their ordering in the linked chunk.
    pub async fn find_event_with_relations(
        &self,
        event_id: &EventId,
        filter: Option<Vec<RelationType>>,
    ) -> Result<Option<(Event, Vec<Event>)>> {
        // Search in all loaded or stored events.
        Ok(self
            .inner
            .state
            .read()
            .await?
            .find_event_with_relations(event_id, filter.clone())
            .await
            .ok()
            .flatten())
    }

    /// Try to find the related events for an event by ID in this room.
    ///
    /// You can filter which types of related events to retrieve using
    /// `filter`. `None` will retrieve related events of any type.
    ///
    /// The related events are sorted like this:
    ///
    /// - events saved out-of-band (with `RoomEventCache::save_events`) will be
    ///   located at the beginning of the array.
    /// - events present in the linked chunk (be it in memory or in the storage)
    ///   will be sorted according to their ordering in the linked chunk.
    pub async fn find_event_relations(
        &self,
        event_id: &EventId,
        filter: Option<Vec<RelationType>>,
    ) -> Result<Vec<Event>> {
        // Search in all loaded or stored events.
        self.inner.state.read().await?.find_event_relations(event_id, filter.clone()).await
    }

    /// Clear all the storage for this [`RoomEventCache`].
    ///
    /// This will get rid of all the events from the linked chunk and persisted
    /// storage.
    pub async fn clear(&self) -> Result<()> {
        // Clear the linked chunk and persisted storage.
        let updates_as_vector_diffs = self.inner.state.write().await?.reset().await?;

        // Notify observers about the update.
        let _ = self.inner.update_sender.send(RoomEventCacheUpdate::UpdateTimelineEvents(
            TimelineVectorDiffs { diffs: updates_as_vector_diffs, origin: EventsOrigin::Cache },
        ));

        // Notify observers about the generic update.
        let _ = self
            .inner
            .generic_update_sender
            .send(RoomEventCacheGenericUpdate { room_id: self.inner.room_id.clone() });

        Ok(())
    }

    #[instrument(skip_all, fields(room_id = %self.room_id()))]
    pub(in super::super) async fn handle_left_room_update(
        &self,
        updates: LeftRoomUpdate,
    ) -> Result<()> {
        self.inner.handle_timeline(updates.timeline, Vec::new(), updates.ambiguity_changes).await?;

        Ok(())
    }

    /// Handle a single event from the `SendQueue`.
    pub(crate) async fn insert_sent_event_from_send_queue(&self, event: Event) -> Result<()> {
        self.inner
            .handle_timeline(
                Timeline { limited: false, prev_batch: None, events: vec![event] },
                Vec::new(),
                BTreeMap::new(),
            )
            .await
    }

    /// Save some events in the event cache, for further retrieval with
    /// [`Self::event`].
    pub(crate) async fn save_events(&self, events: impl IntoIterator<Item = Event>) {
        match self.inner.state.write().await {
            Ok(mut state_guard) => {
                if let Err(err) = state_guard.save_events(events).await {
                    warn!("couldn't save event in the event cache: {err}");
                }
            }

            Err(err) => {
                warn!("couldn't save event in the event cache: {err}");
            }
        }
    }

    /// Return a nice debug string (a vector of lines) for the linked chunk of
    /// events for this room.
    pub async fn debug_string(&self) -> Vec<String> {
        match self.inner.state.read().await {
            Ok(read_guard) => read_guard.room_linked_chunk().debug_string(),
            Err(err) => {
                warn!(?err, "Failed to obtain the read guard for the `RoomEventCache`");

                vec![]
            }
        }
    }
}

/// The (non-cloneable) details of the `RoomEventCache`.
pub(in super::super) struct RoomEventCacheInner {
    /// The room id for this room.
    pub(in super::super) room_id: OwnedRoomId,

    pub weak_room: WeakRoom,

    /// State for this room's event cache.
    pub state: RoomEventCacheStateLock,

    /// A notifier that we received a new pagination token.
    pub pagination_batch_token_notifier: Notify,

    pub pagination_status: SharedObservable<PaginationStatus>,

    /// Sender to the auto-shrink channel.
    ///
    /// See doc comment around [`EventCache::auto_shrink_linked_chunk_task`] for
    /// more details.
    auto_shrink_sender: mpsc::Sender<AutoShrinkChannelPayload>,

    /// Sender part for update subscribers to this room.
    pub update_sender: Sender<RoomEventCacheUpdate>,

    /// A clone of [`EventCacheInner::generic_update_sender`].
    ///
    /// Whilst `EventCacheInner` handles the generic updates from the sync, or
    /// the storage, it doesn't handle the update from pagination. Having a
    /// clone here allows to access it from [`RoomPagination`].
    pub(in super::super) generic_update_sender: Sender<RoomEventCacheGenericUpdate>,
}

impl RoomEventCacheInner {
    /// Creates a new cache for a room, and subscribes to room updates, so as
    /// to handle new timeline events.
    fn new(
        client: WeakClient,
        state: RoomEventCacheStateLock,
        pagination_status: SharedObservable<PaginationStatus>,
        room_id: OwnedRoomId,
        auto_shrink_sender: mpsc::Sender<AutoShrinkChannelPayload>,
        update_sender: Sender<RoomEventCacheUpdate>,
        generic_update_sender: Sender<RoomEventCacheGenericUpdate>,
    ) -> Self {
        let weak_room = WeakRoom::new(client, room_id);

        Self {
            room_id: weak_room.room_id().to_owned(),
            weak_room,
            state,
            update_sender,
            pagination_batch_token_notifier: Default::default(),
            auto_shrink_sender,
            pagination_status,
            generic_update_sender,
        }
    }

    fn handle_account_data(&self, account_data: Vec<Raw<AnyRoomAccountDataEvent>>) {
        if account_data.is_empty() {
            return;
        }

        let mut handled_read_marker = false;

        trace!("Handling account data");

        for raw_event in account_data {
            match raw_event.deserialize() {
                Ok(AnyRoomAccountDataEvent::FullyRead(ev)) => {
                    // If duplicated, do not forward read marker multiple times
                    // to avoid clutter the update channel.
                    if handled_read_marker {
                        continue;
                    }

                    handled_read_marker = true;

                    // Propagate to observers. (We ignore the error if there aren't any.)
                    let _ = self.update_sender.send(RoomEventCacheUpdate::MoveReadMarkerTo {
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

    #[instrument(skip_all, fields(room_id = %self.room_id))]
    pub(in super::super) async fn handle_joined_room_update(
        &self,
        updates: JoinedRoomUpdate,
    ) -> Result<()> {
        self.handle_timeline(
            updates.timeline,
            updates.ephemeral.clone(),
            updates.ambiguity_changes,
        )
        .await?;
        self.handle_account_data(updates.account_data);

        Ok(())
    }

    /// Handle a [`Timeline`], i.e. new events received by a sync for this
    /// room.
    async fn handle_timeline(
        &self,
        timeline: Timeline,
        ephemeral_events: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    ) -> Result<()> {
        if timeline.events.is_empty()
            && timeline.prev_batch.is_none()
            && ephemeral_events.is_empty()
            && ambiguity_changes.is_empty()
        {
            return Ok(());
        }

        // Add all the events to the backend.
        trace!("adding new events");

        let (stored_prev_batch_token, timeline_event_diffs) =
            self.state.write().await?.handle_sync(timeline).await?;

        // Now that all events have been added, we can trigger the
        // `pagination_token_notifier`.
        if stored_prev_batch_token {
            self.pagination_batch_token_notifier.notify_one();
        }

        // The order matters here: first send the timeline event diffs, then only the
        // related events (read receipts, etc.).
        if !timeline_event_diffs.is_empty() {
            let _ = self.update_sender.send(RoomEventCacheUpdate::UpdateTimelineEvents(
                TimelineVectorDiffs { diffs: timeline_event_diffs, origin: EventsOrigin::Sync },
            ));

            let _ = self
                .generic_update_sender
                .send(RoomEventCacheGenericUpdate { room_id: self.room_id.clone() });
        }

        if !ephemeral_events.is_empty() {
            let _ = self
                .update_sender
                .send(RoomEventCacheUpdate::AddEphemeralEvents { events: ephemeral_events });
        }

        if !ambiguity_changes.is_empty() {
            let _ =
                self.update_sender.send(RoomEventCacheUpdate::UpdateMembers { ambiguity_changes });
        }

        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(in super::super) enum PostProcessingOrigin {
    Sync,
    Backpagination,
    #[cfg(feature = "e2e-encryption")]
    Redecryption,
}

/// An enum representing where an event has been found.
pub(in super::super) enum EventLocation {
    /// Event lives in memory (and likely in the store!).
    Memory(Position),

    /// Event lives in the store only, it has not been loaded in memory yet.
    Store,
}

#[cfg(test)]
mod tests {
    use matrix_sdk_base::event_cache::Event;
    use matrix_sdk_test::{async_test, event_factory::EventFactory};
    use ruma::{
        RoomId, event_id,
        events::{relation::RelationType, room::message::RoomMessageEventContentWithoutRelation},
        room_id, user_id,
    };

    use crate::test_utils::logged_in_client;

    #[async_test]
    async fn test_find_event_by_id_with_edit_relation() {
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
    async fn test_find_event_by_id_with_thread_reply_relation() {
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
    async fn test_find_event_by_id_with_reaction_relation() {
        let original_id = event_id!("$original");
        let related_id = event_id!("$related");
        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        assert_relations(
            room_id,
            f.text_msg("Original event").event_id(original_id).into(),
            f.reaction(original_id, ":D").event_id(related_id).into(),
            f,
        )
        .await;
    }

    #[async_test]
    async fn test_find_event_by_id_with_poll_response_relation() {
        let original_id = event_id!("$original");
        let related_id = event_id!("$related");
        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        assert_relations(
            room_id,
            f.poll_start("Poll start event", "A poll question", vec!["An answer"])
                .event_id(original_id)
                .into(),
            f.poll_response(vec!["1"], original_id).event_id(related_id).into(),
            f,
        )
        .await;
    }

    #[async_test]
    async fn test_find_event_by_id_with_poll_end_relation() {
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
    async fn test_find_event_by_id_with_filtered_relationships() {
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
            event_factory.reaction(related_id, "🤡").event_id(associated_related_id).into();

        let client = logged_in_client(None).await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Save the original event.
        room_event_cache.save_events([original_event]).await;

        // Save the related event.
        room_event_cache.save_events([related_event]).await;

        // Save the associated related event, which redacts the related event.
        room_event_cache.save_events([associated_related_event]).await;

        let filter = Some(vec![RelationType::Replacement]);
        let (event, related_events) = room_event_cache
            .find_event_with_relations(original_id, filter)
            .await
            .expect("Failed to find the event with relations")
            .expect("Event has no relation");
        // Fetched event is the right one.
        let cached_event_id = event.event_id().unwrap();
        assert_eq!(cached_event_id, original_id);

        // There's only the edit event (an edit event can't have its own edit event).
        assert_eq!(related_events.len(), 1);

        let related_event_id = related_events[0].event_id().unwrap();
        assert_eq!(related_event_id, related_id);

        // Now we'll filter threads instead, there should be no related events
        let filter = Some(vec![RelationType::Thread]);
        let (event, related_events) = room_event_cache
            .find_event_with_relations(original_id, filter)
            .await
            .expect("Failed to find the event with relations")
            .expect("Event has no relation");

        // Fetched event is the right one.
        let cached_event_id = event.event_id().unwrap();
        assert_eq!(cached_event_id, original_id);
        // No Thread related events found
        assert!(related_events.is_empty());
    }

    #[async_test]
    async fn test_find_event_by_id_with_recursive_relation() {
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
            event_factory.reaction(related_id, "👍").event_id(associated_related_id).into();

        let client = logged_in_client(None).await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Save the original event.
        room_event_cache.save_events([original_event]).await;

        // Save the related event.
        room_event_cache.save_events([related_event]).await;

        // Save the associated related event, which redacts the related event.
        room_event_cache.save_events([associated_related_event]).await;

        let (event, related_events) = room_event_cache
            .find_event_with_relations(original_id, None)
            .await
            .expect("Failed to find the event with relations")
            .expect("Event has no relation");
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

    async fn assert_relations(
        room_id: &RoomId,
        original_event: Event,
        related_event: Event,
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
        room_event_cache.save_events([original_event]).await;

        // Save an unrelated event to check it's not in the related events list.
        let unrelated_id = event_id!("$2");
        room_event_cache
            .save_events([event_factory
                .text_msg("An unrelated event")
                .event_id(unrelated_id)
                .into()])
            .await;

        // Save the related event.
        let related_id = related_event.event_id().unwrap();
        room_event_cache.save_events([related_event]).await;

        let (event, related_events) = room_event_cache
            .find_event_with_relations(&original_event_id, None)
            .await
            .expect("Failed to find the event with relations")
            .expect("Event has no relation");
        // Fetched event is the right one.
        let cached_event_id = event.event_id().unwrap();
        assert_eq!(cached_event_id, original_event_id);

        // There is only the actually related event in the related ones
        let related_event_id = related_events[0].event_id().unwrap();
        assert_eq!(related_event_id, related_id);
    }
}

#[cfg(all(test, not(target_family = "wasm")))] // This uses the cross-process lock, so needs time support.
mod timed_tests {
    use std::{ops::Not, sync::Arc};

    use assert_matches::assert_matches;
    use assert_matches2::assert_let;
    use eyeball_im::VectorDiff;
    use futures_util::FutureExt;
    use matrix_sdk_base::{
        event_cache::{
            Gap,
            store::{EventCacheStore as _, MemoryStore},
        },
        linked_chunk::{
            ChunkContent, ChunkIdentifier, LinkedChunkId, Position, Update,
            lazy_loader::from_all_chunks,
        },
        store::StoreConfig,
        sync::{JoinedRoomUpdate, Timeline},
    };
    use matrix_sdk_common::cross_process_lock::CrossProcessLockConfig;
    use matrix_sdk_test::{ALICE, BOB, async_test, event_factory::EventFactory};
    use ruma::{
        EventId, OwnedUserId, event_id,
        events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent},
        room_id, user_id,
    };
    use tokio::task::yield_now;

    use super::{
        super::{lock::Reload as _, pagination::LoadMoreEventsBackwardsOutcome},
        RoomEventCache, RoomEventCacheGenericUpdate, RoomEventCacheUpdate,
    };
    use crate::{
        assert_let_timeout, event_cache::TimelineVectorDiffs, test_utils::client::MockClientBuilder,
    };

    #[async_test]
    async fn test_write_to_storage() {
        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        let event_cache_store = Arc::new(MemoryStore::new());

        let client = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new(CrossProcessLockConfig::multi_process("hodor"))
                        .event_cache_store(event_cache_store.clone()),
                )
            })
            .build()
            .await;

        let event_cache = client.event_cache();

        // Don't forget to subscribe and like.
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();
        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Propagate an update for a message and a prev-batch token.
        let timeline = Timeline {
            limited: true,
            prev_batch: Some("raclette".to_owned()),
            events: vec![f.text_msg("hey yo").sender(*ALICE).into_event()],
        };

        room_event_cache
            .inner
            .handle_joined_room_update(JoinedRoomUpdate { timeline, ..Default::default() })
            .await
            .unwrap();

        // Just checking the generic update is correct.
        assert_matches!(
            generic_stream.recv().await,
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) => {
                assert_eq!(expected_room_id, room_id);
            }
        );
        assert!(generic_stream.is_empty());

        // Check the storage.
        let linked_chunk = from_all_chunks::<3, _, _>(
            event_cache_store.load_all_chunks(LinkedChunkId::Room(room_id)).await.unwrap(),
        )
        .unwrap()
        .unwrap();

        assert_eq!(linked_chunk.chunks().count(), 2);

        let mut chunks = linked_chunk.chunks();

        // We start with the gap.
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

    #[async_test]
    async fn test_write_to_storage_strips_bundled_relations() {
        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        let event_cache_store = Arc::new(MemoryStore::new());

        let client = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new(CrossProcessLockConfig::multi_process("hodor"))
                        .event_cache_store(event_cache_store.clone()),
                )
            })
            .build()
            .await;

        let event_cache = client.event_cache();

        // Don't forget to subscribe and like.
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();
        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Propagate an update for a message with bundled relations.
        let ev = f
            .text_msg("hey yo")
            .sender(*ALICE)
            .with_bundled_edit(f.text_msg("Hello, Kind Sir").sender(*ALICE))
            .into_event();

        let timeline = Timeline { limited: false, prev_batch: None, events: vec![ev] };

        room_event_cache
            .inner
            .handle_joined_room_update(JoinedRoomUpdate { timeline, ..Default::default() })
            .await
            .unwrap();

        // Just checking the generic update is correct.
        assert_matches!(
            generic_stream.recv().await,
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) => {
                assert_eq!(expected_room_id, room_id);
            }
        );
        assert!(generic_stream.is_empty());

        // The in-memory linked chunk keeps the bundled relation.
        {
            let events = room_event_cache.events().await.unwrap();

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
        let linked_chunk = from_all_chunks::<3, _, _>(
            event_cache_store.load_all_chunks(LinkedChunkId::Room(room_id)).await.unwrap(),
        )
        .unwrap()
        .unwrap();

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

    #[async_test]
    async fn test_clear() {
        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        let event_cache_store = Arc::new(MemoryStore::new());

        let event_id1 = event_id!("$1");
        let event_id2 = event_id!("$2");

        let ev1 = f.text_msg("hello world").sender(*ALICE).event_id(event_id1).into_event();
        let ev2 = f.text_msg("how's it going").sender(*BOB).event_id(event_id2).into_event();

        // Prefill the store with some data.
        event_cache_store
            .handle_linked_chunk_updates(
                LinkedChunkId::Room(room_id),
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
                        gap: Gap { prev_token: "comté".to_owned() },
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

        let client = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new(CrossProcessLockConfig::multi_process("hodor"))
                        .event_cache_store(event_cache_store.clone()),
                )
            })
            .build()
            .await;

        let event_cache = client.event_cache();

        // Don't forget to subscribe and like.
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        let (items, mut stream) = room_event_cache.subscribe().await.unwrap();
        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();

        // The rooms knows about all cached events.
        {
            assert!(room_event_cache.find_event(event_id1).await.unwrap().is_some());
            assert!(room_event_cache.find_event(event_id2).await.unwrap().is_some());
        }

        // But only part of events are loaded from the store
        {
            // The room must contain only one event because only one chunk has been loaded.
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].event_id().unwrap(), event_id2);

            assert!(stream.is_empty());
        }

        // Let's load more chunks to load all events.
        {
            room_event_cache.pagination().run_backwards_once(20).await.unwrap();

            assert_let_timeout!(
                Ok(RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { diffs, .. })) =
                    stream.recv()
            );
            assert_eq!(diffs.len(), 1);
            assert_matches!(&diffs[0], VectorDiff::Insert { index: 0, value: event } => {
                // Here you are `event_id1`!
                assert_eq!(event.event_id().unwrap(), event_id1);
            });

            assert!(stream.is_empty());

            assert_let_timeout!(
                Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) =
                    generic_stream.recv()
            );
            assert_eq!(room_id, expected_room_id);
            assert!(generic_stream.is_empty());
        }

        // After clearing,…
        room_event_cache.clear().await.unwrap();

        //… we get an update that the content has been cleared.
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { diffs, .. })) =
                stream.recv()
        );
        assert_eq!(diffs.len(), 1);
        assert_let!(VectorDiff::Clear = &diffs[0]);

        // … same with a generic update.
        assert_let_timeout!(
            Ok(RoomEventCacheGenericUpdate { room_id: received_room_id }) = generic_stream.recv()
        );
        assert_eq!(received_room_id, room_id);
        assert!(generic_stream.is_empty());

        // Events individually are not forgotten by the event cache, after clearing a
        // room.
        assert!(room_event_cache.find_event(event_id1).await.unwrap().is_some());

        // But their presence in a linked chunk is forgotten.
        let items = room_event_cache.events().await.unwrap();
        assert!(items.is_empty());

        // The event cache store too.
        let linked_chunk = from_all_chunks::<3, _, _>(
            event_cache_store.load_all_chunks(LinkedChunkId::Room(room_id)).await.unwrap(),
        )
        .unwrap()
        .unwrap();

        // Note: while the event cache store could return `None` here, clearing it will
        // reset it to its initial form, maintaining the invariant that it
        // contains a single items chunk that's empty.
        assert_eq!(linked_chunk.num_items(), 0);
    }

    #[async_test]
    async fn test_load_from_storage() {
        let room_id = room_id!("!galette:saucisse.bzh");
        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));

        let event_cache_store = Arc::new(MemoryStore::new());

        let event_id1 = event_id!("$1");
        let event_id2 = event_id!("$2");

        let ev1 = f.text_msg("hello world").sender(*ALICE).event_id(event_id1).into_event();
        let ev2 = f.text_msg("how's it going").sender(*BOB).event_id(event_id2).into_event();

        // Prefill the store with some data.
        event_cache_store
            .handle_linked_chunk_updates(
                LinkedChunkId::Room(room_id),
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

        let client = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new(CrossProcessLockConfig::multi_process("hodor"))
                        .event_cache_store(event_cache_store.clone()),
                )
            })
            .build()
            .await;

        let event_cache = client.event_cache();

        // Don't forget to subscribe and like.
        event_cache.subscribe().unwrap();

        // Let's check whether the generic updates are received for the initialisation.
        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // The room event cache has been loaded. A generic update must have been
        // triggered.
        assert_matches!(
            generic_stream.recv().await,
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) => {
                assert_eq!(room_id, expected_room_id);
            }
        );
        assert!(generic_stream.is_empty());

        let (items, mut stream) = room_event_cache.subscribe().await.unwrap();

        // The initial items contain one event because only the last chunk is loaded by
        // default.
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].event_id().unwrap(), event_id2);
        assert!(stream.is_empty());

        // The event cache knows only all events though, even if they aren't loaded.
        assert!(room_event_cache.find_event(event_id1).await.unwrap().is_some());
        assert!(room_event_cache.find_event(event_id2).await.unwrap().is_some());

        // Let's paginate to load more events.
        room_event_cache.pagination().run_backwards_once(20).await.unwrap();

        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { diffs, .. })) =
                stream.recv()
        );
        assert_eq!(diffs.len(), 1);
        assert_matches!(&diffs[0], VectorDiff::Insert { index: 0, value: event } => {
            assert_eq!(event.event_id().unwrap(), event_id1);
        });

        assert!(stream.is_empty());

        // A generic update is triggered too.
        assert_matches!(
            generic_stream.recv().await,
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) => {
                assert_eq!(expected_room_id, room_id);
            }
        );
        assert!(generic_stream.is_empty());

        // A new update with one of these events leads to deduplication.
        let timeline = Timeline { limited: false, prev_batch: None, events: vec![ev2] };

        room_event_cache
            .inner
            .handle_joined_room_update(JoinedRoomUpdate { timeline, ..Default::default() })
            .await
            .unwrap();

        // Just checking the generic update is correct. There is a duplicate event, so
        // no generic changes whatsoever!
        assert!(generic_stream.recv().now_or_never().is_none());

        // The stream doesn't report these changes *yet*. Use the items vector given
        // when subscribing, to check that the items correspond to their new
        // positions. The duplicated item is removed (so it's not the first
        // element anymore), and it's added to the back of the list.
        let items = room_event_cache.events().await.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].event_id().unwrap(), event_id1);
        assert_eq!(items[1].event_id().unwrap(), event_id2);
    }

    #[async_test]
    async fn test_load_from_storage_resilient_to_failure() {
        let room_id = room_id!("!fondue:patate.ch");
        let event_cache_store = Arc::new(MemoryStore::new());

        let event = EventFactory::new()
            .room(room_id)
            .sender(user_id!("@ben:saucisse.bzh"))
            .text_msg("foo")
            .event_id(event_id!("$42"))
            .into_event();

        // Prefill the store with invalid data: two chunks that form a cycle.
        event_cache_store
            .handle_linked_chunk_updates(
                LinkedChunkId::Room(room_id),
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(0),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(0), 0),
                        items: vec![event],
                    },
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(0)),
                        new: ChunkIdentifier::new(1),
                        next: Some(ChunkIdentifier::new(0)),
                    },
                ],
            )
            .await
            .unwrap();

        let client = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new(CrossProcessLockConfig::multi_process("holder"))
                        .event_cache_store(event_cache_store.clone()),
                )
            })
            .build()
            .await;

        let event_cache = client.event_cache();

        // Don't forget to subscribe and like.
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        let items = room_event_cache.events().await.unwrap();

        // Because the persisted content was invalid, the room store is reset: there are
        // no events in the cache.
        assert!(items.is_empty());

        // Storage doesn't contain anything. It would also be valid that it contains a
        // single initial empty items chunk.
        let raw_chunks =
            event_cache_store.load_all_chunks(LinkedChunkId::Room(room_id)).await.unwrap();
        assert!(raw_chunks.is_empty());
    }

    #[async_test]
    async fn test_no_useless_gaps() {
        let room_id = room_id!("!galette:saucisse.bzh");

        let client = MockClientBuilder::new(None).build().await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();
        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();

        let f = EventFactory::new().room(room_id).sender(*ALICE);

        // Propagate an update including a limited timeline with one message and a
        // prev-batch token.
        room_event_cache
            .inner
            .handle_joined_room_update(JoinedRoomUpdate {
                timeline: Timeline {
                    limited: true,
                    prev_batch: Some("raclette".to_owned()),
                    events: vec![f.text_msg("hey yo").into_event()],
                },
                ..Default::default()
            })
            .await
            .unwrap();

        // Just checking the generic update is correct.
        assert_matches!(
            generic_stream.recv().await,
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) => {
                assert_eq!(expected_room_id, room_id);
            }
        );
        assert!(generic_stream.is_empty());

        {
            let mut state = room_event_cache.inner.state.write().await.unwrap();

            let mut num_gaps = 0;
            let mut num_events = 0;

            for c in state.room_linked_chunk().chunks() {
                match c.content() {
                    ChunkContent::Items(items) => num_events += items.len(),
                    ChunkContent::Gap(_) => num_gaps += 1,
                }
            }

            // The limited sync unloads the chunk, so it will appear as if there are only
            // the events.
            assert_eq!(num_gaps, 0);
            assert_eq!(num_events, 1);

            // But if I manually reload more of the chunk, the gap will be present.
            assert_matches!(
                state.load_more_events_backwards().await.unwrap(),
                LoadMoreEventsBackwardsOutcome::Gap { .. }
            );

            num_gaps = 0;
            num_events = 0;
            for c in state.room_linked_chunk().chunks() {
                match c.content() {
                    ChunkContent::Items(items) => num_events += items.len(),
                    ChunkContent::Gap(_) => num_gaps += 1,
                }
            }

            // The gap must have been stored.
            assert_eq!(num_gaps, 1);
            assert_eq!(num_events, 1);
        }

        // Now, propagate an update for another message, but the timeline isn't limited
        // this time.
        room_event_cache
            .inner
            .handle_joined_room_update(JoinedRoomUpdate {
                timeline: Timeline {
                    limited: false,
                    prev_batch: Some("fondue".to_owned()),
                    events: vec![f.text_msg("sup").into_event()],
                },
                ..Default::default()
            })
            .await
            .unwrap();

        // Just checking the generic update is correct.
        assert_matches!(
            generic_stream.recv().await,
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) => {
                assert_eq!(expected_room_id, room_id);
            }
        );
        assert!(generic_stream.is_empty());

        {
            let state = room_event_cache.inner.state.read().await.unwrap();

            let mut num_gaps = 0;
            let mut num_events = 0;

            for c in state.room_linked_chunk().chunks() {
                match c.content() {
                    ChunkContent::Items(items) => num_events += items.len(),
                    ChunkContent::Gap(gap) => {
                        assert_eq!(gap.prev_token, "raclette");
                        num_gaps += 1;
                    }
                }
            }

            // There's only the previous gap, no new ones.
            assert_eq!(num_gaps, 1);
            assert_eq!(num_events, 2);
        }
    }

    #[async_test]
    async fn test_shrink_to_last_chunk() {
        let room_id = room_id!("!galette:saucisse.bzh");

        let client = MockClientBuilder::new(None).build().await;

        let f = EventFactory::new().room(room_id);

        let evid1 = event_id!("$1");
        let evid2 = event_id!("$2");

        let ev1 = f.text_msg("hello world").sender(*ALICE).event_id(evid1).into_event();
        let ev2 = f.text_msg("howdy").sender(*BOB).event_id(evid2).into_event();

        // Fill the event cache store with an initial linked chunk with 2 events chunks.
        {
            client
                .event_cache_store()
                .lock()
                .await
                .expect("Could not acquire the event cache lock")
                .as_clean()
                .expect("Could not acquire a clean event cache lock")
                .handle_linked_chunk_updates(
                    LinkedChunkId::Room(room_id),
                    vec![
                        Update::NewItemsChunk {
                            previous: None,
                            new: ChunkIdentifier::new(0),
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(0), 0),
                            items: vec![ev1],
                        },
                        Update::NewItemsChunk {
                            previous: Some(ChunkIdentifier::new(0)),
                            new: ChunkIdentifier::new(1),
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(1), 0),
                            items: vec![ev2],
                        },
                    ],
                )
                .await
                .unwrap();
        }

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();
        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Sanity check: lazily loaded, so only includes one item at start.
        let (events, mut stream) = room_event_cache.subscribe().await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_id().as_deref(), Some(evid2));
        assert!(stream.is_empty());

        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();

        // Force loading the full linked chunk by back-paginating.
        let outcome = room_event_cache.pagination().run_backwards_once(20).await.unwrap();
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].event_id().as_deref(), Some(evid1));
        assert!(outcome.reached_start);

        // We also get an update about the loading from the store.
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { diffs, .. })) =
                stream.recv()
        );
        assert_eq!(diffs.len(), 1);
        assert_matches!(&diffs[0], VectorDiff::Insert { index: 0, value } => {
            assert_eq!(value.event_id().as_deref(), Some(evid1));
        });

        assert!(stream.is_empty());

        // Same for the generic update.
        assert_let_timeout!(
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) = generic_stream.recv()
        );
        assert_eq!(expected_room_id, room_id);
        assert!(generic_stream.is_empty());

        // Shrink the linked chunk to the last chunk.
        room_event_cache
            .inner
            .state
            .write()
            .await
            .unwrap()
            .reload()
            .await
            .expect("shrinking should succeed");

        // We receive updates about the changes to the linked chunk.
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { diffs, .. })) =
                stream.recv()
        );
        assert_eq!(diffs.len(), 2);
        assert_matches!(&diffs[0], VectorDiff::Clear);
        assert_matches!(&diffs[1], VectorDiff::Append { values} => {
            assert_eq!(values.len(), 1);
            assert_eq!(values[0].event_id().as_deref(), Some(evid2));
        });

        assert!(stream.is_empty());

        // A generic update has been received.
        assert_let_timeout!(Ok(RoomEventCacheGenericUpdate { .. }) = generic_stream.recv());
        assert!(generic_stream.is_empty());

        // When reading the events, we do get only the last one.
        let events = room_event_cache.events().await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_id().as_deref(), Some(evid2));

        // But if we back-paginate, we don't need access to network to find out about
        // the previous event.
        let outcome = room_event_cache.pagination().run_backwards_once(20).await.unwrap();
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].event_id().as_deref(), Some(evid1));
        assert!(outcome.reached_start);
    }

    #[async_test]
    async fn test_room_ordering() {
        let room_id = room_id!("!galette:saucisse.bzh");

        let client = MockClientBuilder::new(None).build().await;

        let f = EventFactory::new().room(room_id).sender(*ALICE);

        let evid1 = event_id!("$1");
        let evid2 = event_id!("$2");
        let evid3 = event_id!("$3");

        let ev1 = f.text_msg("hello world").event_id(evid1).into_event();
        let ev2 = f.text_msg("howdy").sender(*BOB).event_id(evid2).into_event();
        let ev3 = f.text_msg("yo").event_id(evid3).into_event();

        // Fill the event cache store with an initial linked chunk with 2 events chunks.
        {
            client
                .event_cache_store()
                .lock()
                .await
                .expect("Could not acquire the event cache lock")
                .as_clean()
                .expect("Could not acquire a clean event cache lock")
                .handle_linked_chunk_updates(
                    LinkedChunkId::Room(room_id),
                    vec![
                        Update::NewItemsChunk {
                            previous: None,
                            new: ChunkIdentifier::new(0),
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(0), 0),
                            items: vec![ev1, ev2],
                        },
                        Update::NewItemsChunk {
                            previous: Some(ChunkIdentifier::new(0)),
                            new: ChunkIdentifier::new(1),
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(1), 0),
                            items: vec![ev3.clone()],
                        },
                    ],
                )
                .await
                .unwrap();
        }

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();
        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Initially, the linked chunk only contains the last chunk, so only ev3 is
        // loaded.
        {
            let state = room_event_cache.inner.state.read().await.unwrap();
            let room_linked_chunk = state.room_linked_chunk();

            // But we can get the order of ev1.
            assert_eq!(
                room_linked_chunk.event_order(Position::new(ChunkIdentifier::new(0), 0)),
                Some(0)
            );

            // And that of ev2 as well.
            assert_eq!(
                room_linked_chunk.event_order(Position::new(ChunkIdentifier::new(0), 1)),
                Some(1)
            );

            // ev3, which is loaded, also has a known ordering.
            let mut events = room_linked_chunk.events();
            let (pos, ev) = events.next().unwrap();
            assert_eq!(pos, Position::new(ChunkIdentifier::new(1), 0));
            assert_eq!(ev.event_id().as_deref(), Some(evid3));
            assert_eq!(room_linked_chunk.event_order(pos), Some(2));

            // No other loaded events.
            assert!(events.next().is_none());
        }

        // Force loading the full linked chunk by back-paginating.
        let outcome = room_event_cache.pagination().run_backwards_once(20).await.unwrap();
        assert!(outcome.reached_start);

        // All events are now loaded, so their order is precisely their enumerated index
        // in a linear iteration.
        {
            let state = room_event_cache.inner.state.read().await.unwrap();
            let room_linked_chunk = state.room_linked_chunk();

            for (i, (pos, _)) in room_linked_chunk.events().enumerate() {
                assert_eq!(room_linked_chunk.event_order(pos), Some(i));
            }
        }

        // Handle a gappy sync with two events (including one duplicate, so
        // deduplication kicks in), so that the linked chunk is shrunk to the
        // last chunk, and that the linked chunk only contains the last two
        // events.
        let evid4 = event_id!("$4");
        room_event_cache
            .inner
            .handle_joined_room_update(JoinedRoomUpdate {
                timeline: Timeline {
                    limited: true,
                    prev_batch: Some("fondue".to_owned()),
                    events: vec![ev3, f.text_msg("sup").event_id(evid4).into_event()],
                },
                ..Default::default()
            })
            .await
            .unwrap();

        {
            let state = room_event_cache.inner.state.read().await.unwrap();
            let room_linked_chunk = state.room_linked_chunk();

            // After the shrink, only evid3 and evid4 are loaded.
            let mut events = room_linked_chunk.events();

            let (pos, ev) = events.next().unwrap();
            assert_eq!(ev.event_id().as_deref(), Some(evid3));
            assert_eq!(room_linked_chunk.event_order(pos), Some(2));

            let (pos, ev) = events.next().unwrap();
            assert_eq!(ev.event_id().as_deref(), Some(evid4));
            assert_eq!(room_linked_chunk.event_order(pos), Some(3));

            // No other loaded events.
            assert!(events.next().is_none());

            // But we can still get the order of previous events.
            assert_eq!(
                room_linked_chunk.event_order(Position::new(ChunkIdentifier::new(0), 0)),
                Some(0)
            );
            assert_eq!(
                room_linked_chunk.event_order(Position::new(ChunkIdentifier::new(0), 1)),
                Some(1)
            );

            // ev3 doesn't have an order with its previous position, since it's been
            // deduplicated.
            assert_eq!(
                room_linked_chunk.event_order(Position::new(ChunkIdentifier::new(1), 0)),
                None
            );
        }
    }

    #[async_test]
    async fn test_auto_shrink_after_all_subscribers_are_gone() {
        let room_id = room_id!("!galette:saucisse.bzh");

        let client = MockClientBuilder::new(None).build().await;

        let f = EventFactory::new().room(room_id);

        let evid1 = event_id!("$1");
        let evid2 = event_id!("$2");

        let ev1 = f.text_msg("hello world").sender(*ALICE).event_id(evid1).into_event();
        let ev2 = f.text_msg("howdy").sender(*BOB).event_id(evid2).into_event();

        // Fill the event cache store with an initial linked chunk with 2 events chunks.
        {
            client
                .event_cache_store()
                .lock()
                .await
                .expect("Could not acquire the event cache lock")
                .as_clean()
                .expect("Could not acquire a clean event cache lock")
                .handle_linked_chunk_updates(
                    LinkedChunkId::Room(room_id),
                    vec![
                        Update::NewItemsChunk {
                            previous: None,
                            new: ChunkIdentifier::new(0),
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(0), 0),
                            items: vec![ev1],
                        },
                        Update::NewItemsChunk {
                            previous: Some(ChunkIdentifier::new(0)),
                            new: ChunkIdentifier::new(1),
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(1), 0),
                            items: vec![ev2],
                        },
                    ],
                )
                .await
                .unwrap();
        }

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();
        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Sanity check: lazily loaded, so only includes one item at start.
        let (events1, mut stream1) = room_event_cache.subscribe().await.unwrap();
        assert_eq!(events1.len(), 1);
        assert_eq!(events1[0].event_id().as_deref(), Some(evid2));
        assert!(stream1.is_empty());

        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();

        // Force loading the full linked chunk by back-paginating.
        let outcome = room_event_cache.pagination().run_backwards_once(20).await.unwrap();
        assert_eq!(outcome.events.len(), 1);
        assert_eq!(outcome.events[0].event_id().as_deref(), Some(evid1));
        assert!(outcome.reached_start);

        // We also get an update about the loading from the store. Ignore it, for this
        // test's sake.
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { diffs, .. })) =
                stream1.recv()
        );
        assert_eq!(diffs.len(), 1);
        assert_matches!(&diffs[0], VectorDiff::Insert { index: 0, value } => {
            assert_eq!(value.event_id().as_deref(), Some(evid1));
        });

        assert!(stream1.is_empty());

        assert_let_timeout!(
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) = generic_stream.recv()
        );
        assert_eq!(expected_room_id, room_id);
        assert!(generic_stream.is_empty());

        // Have another subscriber.
        // Since it's not the first one, and the previous one loaded some more events,
        // the second subscribers sees them all.
        let (events2, stream2) = room_event_cache.subscribe().await.unwrap();
        assert_eq!(events2.len(), 2);
        assert_eq!(events2[0].event_id().as_deref(), Some(evid1));
        assert_eq!(events2[1].event_id().as_deref(), Some(evid2));
        assert!(stream2.is_empty());

        // Drop the first stream, and wait a bit.
        drop(stream1);
        yield_now().await;

        // The second stream remains undisturbed.
        assert!(stream2.is_empty());

        // Now drop the second stream, and wait a bit.
        drop(stream2);
        yield_now().await;

        // The linked chunk must have auto-shrunk by now.

        {
            // Check the inner state: there's no more shared auto-shrinker.
            let state = room_event_cache.inner.state.read().await.unwrap();
            assert_eq!(state.subscriber_count().load(std::sync::atomic::Ordering::SeqCst), 0);
        }

        // Getting the events will only give us the latest chunk.
        let events3 = room_event_cache.events().await.unwrap();
        assert_eq!(events3.len(), 1);
        assert_eq!(events3[0].event_id().as_deref(), Some(evid2));
    }

    #[async_test]
    async fn test_rfind_map_event_in_memory_by() {
        let user_id = user_id!("@mnt_io:matrix.org");
        let room_id = room_id!("!raclette:patate.ch");
        let client = MockClientBuilder::new(None).build().await;

        let event_factory = EventFactory::new().room(room_id);

        let event_id_0 = event_id!("$ev0");
        let event_id_1 = event_id!("$ev1");
        let event_id_2 = event_id!("$ev2");
        let event_id_3 = event_id!("$ev3");

        let event_0 =
            event_factory.text_msg("hello").sender(*BOB).event_id(event_id_0).into_event();
        let event_1 =
            event_factory.text_msg("world").sender(*ALICE).event_id(event_id_1).into_event();
        let event_2 = event_factory.text_msg("!").sender(*ALICE).event_id(event_id_2).into_event();
        let event_3 =
            event_factory.text_msg("eh!").sender(user_id).event_id(event_id_3).into_event();

        // Fill the event cache store with an initial linked chunk of 2 chunks, and 4
        // events.
        {
            client
                .event_cache_store()
                .lock()
                .await
                .expect("Could not acquire the event cache lock")
                .as_clean()
                .expect("Could not acquire a clean event cache lock")
                .handle_linked_chunk_updates(
                    LinkedChunkId::Room(room_id),
                    vec![
                        Update::NewItemsChunk {
                            previous: None,
                            new: ChunkIdentifier::new(0),
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(0), 0),
                            items: vec![event_3],
                        },
                        Update::NewItemsChunk {
                            previous: Some(ChunkIdentifier::new(0)),
                            new: ChunkIdentifier::new(1),
                            next: None,
                        },
                        Update::PushItems {
                            at: Position::new(ChunkIdentifier::new(1), 0),
                            items: vec![event_0, event_1, event_2],
                        },
                    ],
                )
                .await
                .unwrap();
        }

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();
        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Look for an event from `BOB`: it must be `event_0`.
        assert_matches!(
            room_event_cache
                .rfind_map_event_in_memory_by(|event| {
                    (event.raw().get_field::<OwnedUserId>("sender").unwrap().as_deref() == Some(*BOB)).then(|| event.event_id())
                })
                .await,
            Ok(Some(event_id)) => {
                assert_eq!(event_id.as_deref(), Some(event_id_0));
            }
        );

        // Look for an event from `ALICE`: it must be `event_2`, right before `event_1`
        // because events are looked for in reverse order.
        assert_matches!(
            room_event_cache
                .rfind_map_event_in_memory_by(|event| {
                    (event.raw().get_field::<OwnedUserId>("sender").unwrap().as_deref() == Some(*ALICE)).then(|| event.event_id())
                })
                .await,
            Ok(Some(event_id)) => {
                assert_eq!(event_id.as_deref(), Some(event_id_2));
            }
        );

        // Look for an event that is inside the storage, but not loaded.
        assert!(
            room_event_cache
                .rfind_map_event_in_memory_by(|event| {
                    (event.raw().get_field::<OwnedUserId>("sender").unwrap().as_deref()
                        == Some(user_id))
                    .then(|| event.event_id())
                })
                .await
                .unwrap()
                .is_none()
        );

        // Look for an event that doesn't exist.
        assert!(
            room_event_cache.rfind_map_event_in_memory_by(|_| None::<()>).await.unwrap().is_none()
        );
    }

    #[async_test]
    async fn test_reload_when_dirty() {
        let user_id = user_id!("@mnt_io:matrix.org");
        let room_id = room_id!("!raclette:patate.ch");

        // The storage shared by the two clients.
        let event_cache_store = MemoryStore::new();

        // Client for the process 0.
        let client_p0 = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new(CrossProcessLockConfig::multi_process("process #0"))
                        .event_cache_store(event_cache_store.clone()),
                )
            })
            .build()
            .await;

        // Client for the process 1.
        let client_p1 = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new(CrossProcessLockConfig::multi_process("process #1"))
                        .event_cache_store(event_cache_store),
                )
            })
            .build()
            .await;

        let event_factory = EventFactory::new().room(room_id).sender(user_id);

        let ev_id_0 = event_id!("$ev_0");
        let ev_id_1 = event_id!("$ev_1");

        let ev_0 = event_factory.text_msg("comté").event_id(ev_id_0).into_event();
        let ev_1 = event_factory.text_msg("morbier").event_id(ev_id_1).into_event();

        // Add events to the storage (shared by the two clients!).
        client_p0
            .event_cache_store()
            .lock()
            .await
            .expect("[p0] Could not acquire the event cache lock")
            .as_clean()
            .expect("[p0] Could not acquire a clean event cache lock")
            .handle_linked_chunk_updates(
                LinkedChunkId::Room(room_id),
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(0),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(0), 0),
                        items: vec![ev_0],
                    },
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(0)),
                        new: ChunkIdentifier::new(1),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(1), 0),
                        items: vec![ev_1],
                    },
                ],
            )
            .await
            .unwrap();

        // Subscribe the event caches, and create the room.
        let (room_event_cache_p0, room_event_cache_p1) = {
            let event_cache_p0 = client_p0.event_cache();
            event_cache_p0.subscribe().unwrap();

            let event_cache_p1 = client_p1.event_cache();
            event_cache_p1.subscribe().unwrap();

            client_p0.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
            client_p1.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);

            let (room_event_cache_p0, _drop_handles) =
                client_p0.get_room(room_id).unwrap().event_cache().await.unwrap();
            let (room_event_cache_p1, _drop_handles) =
                client_p1.get_room(room_id).unwrap().event_cache().await.unwrap();

            (room_event_cache_p0, room_event_cache_p1)
        };

        // Okay. We are ready for the test!
        //
        // First off, let's check `room_event_cache_p0` has access to the first event
        // loaded in-memory, then do a pagination, and see more events.
        let mut updates_stream_p0 = {
            let room_event_cache = &room_event_cache_p0;

            let (initial_updates, mut updates_stream) =
                room_event_cache_p0.subscribe().await.unwrap();

            // Initial updates contain `ev_id_1` only.
            assert_eq!(initial_updates.len(), 1);
            assert_eq!(initial_updates[0].event_id().as_deref(), Some(ev_id_1));
            assert!(updates_stream.is_empty());

            // `ev_id_1` must be loaded in memory.
            assert!(event_loaded(room_event_cache, ev_id_1).await);

            // `ev_id_0` must NOT be loaded in memory.
            assert!(event_loaded(room_event_cache, ev_id_0).await.not());

            // Load one more event with a backpagination.
            room_event_cache.pagination().run_backwards_once(1).await.unwrap();

            // A new update for `ev_id_0` must be present.
            assert_matches!(
                updates_stream.recv().await.unwrap(),
                RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { diffs, .. }) => {
                    assert_eq!(diffs.len(), 1, "{diffs:#?}");
                    assert_matches!(
                        &diffs[0],
                        VectorDiff::Insert { index: 0, value: event } => {
                            assert_eq!(event.event_id().as_deref(), Some(ev_id_0));
                        }
                    );
                }
            );

            // `ev_id_0` must now be loaded in memory.
            assert!(event_loaded(room_event_cache, ev_id_0).await);

            updates_stream
        };

        // Second, let's check `room_event_cache_p1` has the same accesses.
        let mut updates_stream_p1 = {
            let room_event_cache = &room_event_cache_p1;
            let (initial_updates, mut updates_stream) =
                room_event_cache_p1.subscribe().await.unwrap();

            // Initial updates contain `ev_id_1` only.
            assert_eq!(initial_updates.len(), 1);
            assert_eq!(initial_updates[0].event_id().as_deref(), Some(ev_id_1));
            assert!(updates_stream.is_empty());

            // `ev_id_1` must be loaded in memory.
            assert!(event_loaded(room_event_cache, ev_id_1).await);

            // `ev_id_0` must NOT be loaded in memory.
            assert!(event_loaded(room_event_cache, ev_id_0).await.not());

            // Load one more event with a backpagination.
            room_event_cache.pagination().run_backwards_once(1).await.unwrap();

            // A new update for `ev_id_0` must be present.
            assert_matches!(
                updates_stream.recv().await.unwrap(),
                RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { diffs, .. }) => {
                    assert_eq!(diffs.len(), 1, "{diffs:#?}");
                    assert_matches!(
                        &diffs[0],
                        VectorDiff::Insert { index: 0, value: event } => {
                            assert_eq!(event.event_id().as_deref(), Some(ev_id_0));
                        }
                    );
                }
            );

            // `ev_id_0` must now be loaded in memory.
            assert!(event_loaded(room_event_cache, ev_id_0).await);

            updates_stream
        };

        // Do this a couple times, for the fun.
        for _ in 0..3 {
            // Third, because `room_event_cache_p1` has locked the store, the lock
            // is dirty for `room_event_cache_p0`, so it will shrink to its last
            // chunk!
            {
                let room_event_cache = &room_event_cache_p0;
                let updates_stream = &mut updates_stream_p0;

                // `ev_id_1` must be loaded in memory, just like before.
                assert!(event_loaded(room_event_cache, ev_id_1).await);

                // However, `ev_id_0` must NOT be loaded in memory. It WAS loaded, but the
                // state has been reloaded to its last chunk.
                assert!(event_loaded(room_event_cache, ev_id_0).await.not());

                // The reload can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { diffs, .. }) => {
                        assert_eq!(diffs.len(), 2, "{diffs:#?}");
                        assert_matches!(&diffs[0], VectorDiff::Clear);
                        assert_matches!(
                            &diffs[1],
                            VectorDiff::Append { values: events } => {
                                assert_eq!(events.len(), 1);
                                assert_eq!(events[0].event_id().as_deref(), Some(ev_id_1));
                            }
                        );
                    }
                );

                // Load one more event with a backpagination.
                room_event_cache.pagination().run_backwards_once(1).await.unwrap();

                // `ev_id_0` must now be loaded in memory.
                assert!(event_loaded(room_event_cache, ev_id_0).await);

                // The pagination can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { diffs, .. }) => {
                        assert_eq!(diffs.len(), 1, "{diffs:#?}");
                        assert_matches!(
                            &diffs[0],
                            VectorDiff::Insert { index: 0, value: event } => {
                                assert_eq!(event.event_id().as_deref(), Some(ev_id_0));
                            }
                        );
                    }
                );
            }

            // Fourth, because `room_event_cache_p0` has locked the store again, the lock
            // is dirty for `room_event_cache_p1` too!, so it will shrink to its last
            // chunk!
            {
                let room_event_cache = &room_event_cache_p1;
                let updates_stream = &mut updates_stream_p1;

                // `ev_id_1` must be loaded in memory, just like before.
                assert!(event_loaded(room_event_cache, ev_id_1).await);

                // However, `ev_id_0` must NOT be loaded in memory. It WAS loaded, but the
                // state has shrunk to its last chunk.
                assert!(event_loaded(room_event_cache, ev_id_0).await.not());

                // The reload can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { diffs, .. }) => {
                        assert_eq!(diffs.len(), 2, "{diffs:#?}");
                        assert_matches!(&diffs[0], VectorDiff::Clear);
                        assert_matches!(
                            &diffs[1],
                            VectorDiff::Append { values: events } => {
                                assert_eq!(events.len(), 1);
                                assert_eq!(events[0].event_id().as_deref(), Some(ev_id_1));
                            }
                        );
                    }
                );

                // Load one more event with a backpagination.
                room_event_cache.pagination().run_backwards_once(1).await.unwrap();

                // `ev_id_0` must now be loaded in memory.
                assert!(event_loaded(room_event_cache, ev_id_0).await);

                // The pagination can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { diffs, .. }) => {
                        assert_eq!(diffs.len(), 1, "{diffs:#?}");
                        assert_matches!(
                            &diffs[0],
                            VectorDiff::Insert { index: 0, value: event } => {
                                assert_eq!(event.event_id().as_deref(), Some(ev_id_0));
                            }
                        );
                    }
                );
            }
        }

        // Repeat that with an explicit read lock (so that we don't rely on
        // `event_loaded` to trigger the dirty detection).
        for _ in 0..3 {
            {
                let room_event_cache = &room_event_cache_p0;
                let updates_stream = &mut updates_stream_p0;

                let guard = room_event_cache.inner.state.read().await.unwrap();

                // Guard is kept alive, to ensure we can have multiple read guards alive with a
                // shared access.
                // See `RoomEventCacheStateLock::read` to learn more.

                // The lock is no longer marked as dirty, it's been cleaned.
                assert!(guard.is_dirty().not());

                // The reload can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { diffs, .. }) => {
                        assert_eq!(diffs.len(), 2, "{diffs:#?}");
                        assert_matches!(&diffs[0], VectorDiff::Clear);
                        assert_matches!(
                            &diffs[1],
                            VectorDiff::Append { values: events } => {
                                assert_eq!(events.len(), 1);
                                assert_eq!(events[0].event_id().as_deref(), Some(ev_id_1));
                            }
                        );
                    }
                );

                assert!(event_loaded(room_event_cache, ev_id_1).await);
                assert!(event_loaded(room_event_cache, ev_id_0).await.not());

                // Ensure `guard` is alive up to this point (in case this test is refactored, I
                // want to make this super explicit).
                //
                // We drop need to drop it before the pagination because the pagination needs to
                // obtain a write lock.
                drop(guard);

                room_event_cache.pagination().run_backwards_once(1).await.unwrap();
                assert!(event_loaded(room_event_cache, ev_id_0).await);

                // The pagination can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { diffs, .. }) => {
                        assert_eq!(diffs.len(), 1, "{diffs:#?}");
                        assert_matches!(
                            &diffs[0],
                            VectorDiff::Insert { index: 0, value: event } => {
                                assert_eq!(event.event_id().as_deref(), Some(ev_id_0));
                            }
                        );
                    }
                );
            }

            {
                let room_event_cache = &room_event_cache_p1;
                let updates_stream = &mut updates_stream_p1;

                let guard = room_event_cache.inner.state.read().await.unwrap();

                // Guard is kept alive, to ensure we can have multiple read guards alive with a
                // shared access.

                // The lock is no longer marked as dirty, it's been cleaned.
                assert!(guard.is_dirty().not());

                // The reload can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { diffs, .. }) => {
                        assert_eq!(diffs.len(), 2, "{diffs:#?}");
                        assert_matches!(&diffs[0], VectorDiff::Clear);
                        assert_matches!(
                            &diffs[1],
                            VectorDiff::Append { values: events } => {
                                assert_eq!(events.len(), 1);
                                assert_eq!(events[0].event_id().as_deref(), Some(ev_id_1));
                            }
                        );
                    }
                );

                assert!(event_loaded(room_event_cache, ev_id_1).await);
                assert!(event_loaded(room_event_cache, ev_id_0).await.not());

                // Ensure `guard` is alive up to this point (in case this test is refactored, I
                // want to make this super explicit).
                //
                // We drop need to drop it before the pagination because the pagination needs to
                // obtain a write lock.
                drop(guard);

                room_event_cache.pagination().run_backwards_once(1).await.unwrap();
                assert!(event_loaded(room_event_cache, ev_id_0).await);

                // The pagination can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { diffs, .. }) => {
                        assert_eq!(diffs.len(), 1, "{diffs:#?}");
                        assert_matches!(
                            &diffs[0],
                            VectorDiff::Insert { index: 0, value: event } => {
                                assert_eq!(event.event_id().as_deref(), Some(ev_id_0));
                            }
                        );
                    }
                );
            }
        }

        // Repeat that with an explicit write lock.
        for _ in 0..3 {
            {
                let room_event_cache = &room_event_cache_p0;
                let updates_stream = &mut updates_stream_p0;

                let guard = room_event_cache.inner.state.write().await.unwrap();

                // The lock is no longer marked as dirty, it's been cleaned.
                assert!(guard.is_dirty().not());

                // The reload can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { diffs, .. }) => {
                        assert_eq!(diffs.len(), 2, "{diffs:#?}");
                        assert_matches!(&diffs[0], VectorDiff::Clear);
                        assert_matches!(
                            &diffs[1],
                            VectorDiff::Append { values: events } => {
                                assert_eq!(events.len(), 1);
                                assert_eq!(events[0].event_id().as_deref(), Some(ev_id_1));
                            }
                        );
                    }
                );

                // Guard isn't kept alive, otherwise `event_loaded` couldn't run because it
                // needs to obtain a read lock.
                drop(guard);

                assert!(event_loaded(room_event_cache, ev_id_1).await);
                assert!(event_loaded(room_event_cache, ev_id_0).await.not());

                room_event_cache.pagination().run_backwards_once(1).await.unwrap();
                assert!(event_loaded(room_event_cache, ev_id_0).await);

                // The pagination can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { diffs, .. }) => {
                        assert_eq!(diffs.len(), 1, "{diffs:#?}");
                        assert_matches!(
                            &diffs[0],
                            VectorDiff::Insert { index: 0, value: event } => {
                                assert_eq!(event.event_id().as_deref(), Some(ev_id_0));
                            }
                        );
                    }
                );
            }

            {
                let room_event_cache = &room_event_cache_p1;
                let updates_stream = &mut updates_stream_p1;

                let guard = room_event_cache.inner.state.write().await.unwrap();

                // The lock is no longer marked as dirty, it's been cleaned.
                assert!(guard.is_dirty().not());

                // The reload can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { diffs, .. }) => {
                        assert_eq!(diffs.len(), 2, "{diffs:#?}");
                        assert_matches!(&diffs[0], VectorDiff::Clear);
                        assert_matches!(
                            &diffs[1],
                            VectorDiff::Append { values: events } => {
                                assert_eq!(events.len(), 1);
                                assert_eq!(events[0].event_id().as_deref(), Some(ev_id_1));
                            }
                        );
                    }
                );

                // Guard isn't kept alive, otherwise `event_loaded` couldn't run because it
                // needs to obtain a read lock.
                drop(guard);

                assert!(event_loaded(room_event_cache, ev_id_1).await);
                assert!(event_loaded(room_event_cache, ev_id_0).await.not());

                room_event_cache.pagination().run_backwards_once(1).await.unwrap();
                assert!(event_loaded(room_event_cache, ev_id_0).await);

                // The pagination can be observed via the updates too.
                assert_matches!(
                    updates_stream.recv().await.unwrap(),
                    RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs { diffs, .. }) => {
                        assert_eq!(diffs.len(), 1, "{diffs:#?}");
                        assert_matches!(
                            &diffs[0],
                            VectorDiff::Insert { index: 0, value: event } => {
                                assert_eq!(event.event_id().as_deref(), Some(ev_id_0));
                            }
                        );
                    }
                );
            }
        }
    }

    #[async_test]
    async fn test_load_when_dirty() {
        let room_id_0 = room_id!("!raclette:patate.ch");
        let room_id_1 = room_id!("!morbiflette:patate.ch");

        // The storage shared by the two clients.
        let event_cache_store = MemoryStore::new();

        // Client for the process 0.
        let client_p0 = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new(CrossProcessLockConfig::multi_process("process #0"))
                        .event_cache_store(event_cache_store.clone()),
                )
            })
            .build()
            .await;

        // Client for the process 1.
        let client_p1 = MockClientBuilder::new(None)
            .on_builder(|builder| {
                builder.store_config(
                    StoreConfig::new(CrossProcessLockConfig::multi_process("process #1"))
                        .event_cache_store(event_cache_store),
                )
            })
            .build()
            .await;

        // Subscribe the event caches, and create the room.
        let (room_event_cache_0_p0, room_event_cache_0_p1) = {
            let event_cache_p0 = client_p0.event_cache();
            event_cache_p0.subscribe().unwrap();

            let event_cache_p1 = client_p1.event_cache();
            event_cache_p1.subscribe().unwrap();

            client_p0
                .base_client()
                .get_or_create_room(room_id_0, matrix_sdk_base::RoomState::Joined);
            client_p0
                .base_client()
                .get_or_create_room(room_id_1, matrix_sdk_base::RoomState::Joined);

            client_p1
                .base_client()
                .get_or_create_room(room_id_0, matrix_sdk_base::RoomState::Joined);
            client_p1
                .base_client()
                .get_or_create_room(room_id_1, matrix_sdk_base::RoomState::Joined);

            let (room_event_cache_0_p0, _drop_handles) =
                client_p0.get_room(room_id_0).unwrap().event_cache().await.unwrap();
            let (room_event_cache_0_p1, _drop_handles) =
                client_p1.get_room(room_id_0).unwrap().event_cache().await.unwrap();

            (room_event_cache_0_p0, room_event_cache_0_p1)
        };

        // Let's make the cross-process lock over the store dirty.
        {
            drop(room_event_cache_0_p0.inner.state.read().await.unwrap());
            drop(room_event_cache_0_p1.inner.state.read().await.unwrap());
        }

        // Create the `RoomEventCache` for `room_id_1`. During its creation, the
        // cross-process lock over the store MUST be dirty, which makes no difference as
        // a clean one: the state is just loaded, not reloaded.
        let (room_event_cache_1_p0, _) =
            client_p0.get_room(room_id_1).unwrap().event_cache().await.unwrap();

        // Check the lock isn't dirty because it's been cleared.
        {
            let guard = room_event_cache_1_p0.inner.state.read().await.unwrap();
            assert!(guard.is_dirty().not());
        }

        // The only way to test this behaviour is to see that the dirty block in
        // `RoomEventCacheStateLock` is covered by this test.
    }

    async fn event_loaded(room_event_cache: &RoomEventCache, event_id: &EventId) -> bool {
        room_event_cache
            .rfind_map_event_in_memory_by(|event| {
                (event.event_id().as_deref() == Some(event_id)).then_some(())
            })
            .await
            .unwrap()
            .is_some()
    }
}
