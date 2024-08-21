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

//! The event cache is an abstraction layer, sitting between the Rust SDK and a
//! final client, that acts as a global observer of all the rooms, gathering and
//! inferring some extra useful information about each room. In particular, this
//! doesn't require subscribing to a specific room to get access to this
//! information.
//!
//! It's intended to be fast, robust and easy to maintain.
//!
//! See the [github issue](https://github.com/matrix-org/matrix-rust-sdk/issues/3058) for more details about the historical reasons that led us to start writing this.
//!
//! Most of it is still a work-in-progress, as of 2024-01-22.
//!
//! The desired set of features it may eventually implement is the following:
//!
//! - [ ] compute proper unread room counts, and use backpagination to get
//!   missing messages/notifications/mentions, if needs be.
//! - [ ] expose that information with a new data structure similar to the
//!   `RoomInfo`, and that may update a `RoomListService`.
//! - [ ] provide read receipts for each message.
//! - [x] backwards pagination
//! - [~] forward pagination
//! - [ ] reconcile results with cached timelines.
//! - [ ] retry decryption upon receiving new keys (from an encryption sync
//!   service or from a key backup).
//! - [ ] expose the latest event for a given room.
//! - [ ] caching of events on-disk.

#![forbid(missing_docs)]

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Debug,
    sync::{Arc, OnceLock},
};

use eyeball::Subscriber;
use matrix_sdk_base::{
    deserialized_responses::{AmbiguityChange, SyncTimelineEvent, TimelineEvent},
    sync::{JoinedRoomUpdate, LeftRoomUpdate, RoomUpdates, Timeline},
};
use matrix_sdk_common::executor::{spawn, JoinHandle};
use ruma::{
    events::{
        room::{message::Relation, redaction::SyncRoomRedactionEvent},
        AnyMessageLikeEventContent, AnyRoomAccountDataEvent, AnySyncEphemeralRoomEvent,
        AnySyncMessageLikeEvent, AnySyncTimelineEvent,
    },
    serde::Raw,
    EventId, OwnedEventId, OwnedRoomId, RoomId,
};
use tokio::sync::{
    broadcast::{error::RecvError, Receiver, Sender},
    Mutex, RwLock, RwLockReadGuard, RwLockWriteGuard,
};
use tracing::{error, info_span, instrument, trace, warn, Instrument as _, Span};

use self::{
    pagination::RoomPaginationData,
    paginator::{Paginator, PaginatorError},
    store::{Gap, RoomEvents},
};
use crate::{client::WeakClient, room::WeakRoom, Client};

mod linked_chunk;
mod pagination;
mod store;

pub mod paginator;
pub use pagination::{RoomPagination, TimelineHasBeenResetWhilePaginating};

/// An error observed in the [`EventCache`].
#[derive(thiserror::Error, Debug)]
pub enum EventCacheError {
    /// The [`EventCache`] instance hasn't been initialized with
    /// [`EventCache::subscribe`]
    #[error(
        "The EventCache hasn't subscribed to sync responses yet, call `EventCache::subscribe()`"
    )]
    NotSubscribedYet,

    /// The room hasn't been found in the client.
    ///
    /// Technically, it's possible to request a [`RoomEventCache`] for a room
    /// that is not known to the client, leading to this error.
    #[error("Room {0} hasn't been found in the Client.")]
    RoomNotFound(OwnedRoomId),

    /// The given back-pagination token is unknown to the event cache.
    #[error("The given back-pagination token is unknown to the event cache.")]
    UnknownBackpaginationToken,

    /// An error has been observed while back-paginating.
    #[error("Error observed while back-paginating: {0}")]
    BackpaginationError(#[from] PaginatorError),

    /// The [`EventCache`] owns a weak reference to the [`Client`] it pertains
    /// to. It's possible this weak reference points to nothing anymore, at
    /// times where we try to use the client.
    #[error("The owning client of the event cache has been dropped.")]
    ClientDropped,
}

/// A result using the [`EventCacheError`].
pub type Result<T> = std::result::Result<T, EventCacheError>;

/// Hold handles to the tasks spawn by a [`RoomEventCache`].
pub struct EventCacheDropHandles {
    /// Task that listens to room updates.
    listen_updates_task: JoinHandle<()>,

    /// Task that listens to updates to the user's ignored list.
    ignore_user_list_update_task: JoinHandle<()>,
}

impl Debug for EventCacheDropHandles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventCacheDropHandles").finish_non_exhaustive()
    }
}

impl Drop for EventCacheDropHandles {
    fn drop(&mut self) {
        self.listen_updates_task.abort();
        self.ignore_user_list_update_task.abort();
    }
}

/// An event cache, providing lots of useful functionality for clients.
///
/// Cloning is shallow, and thus is cheap to do.
///
/// See also the module-level comment.
#[derive(Clone)]
pub struct EventCache {
    /// Reference to the inner cache.
    inner: Arc<EventCacheInner>,
}

impl Debug for EventCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventCache").finish_non_exhaustive()
    }
}

impl EventCache {
    /// Create a new [`EventCache`] for the given client.
    pub(crate) fn new(client: WeakClient) -> Self {
        Self {
            inner: Arc::new(EventCacheInner {
                client,
                multiple_room_updates_lock: Default::default(),
                by_room: Default::default(),
                drop_handles: Default::default(),
                all_events: Default::default(),
            }),
        }
    }

    /// Starts subscribing the [`EventCache`] to sync responses, if not done
    /// before.
    ///
    /// Re-running this has no effect if we already subscribed before, and is
    /// cheap.
    pub fn subscribe(&self) -> Result<()> {
        let client = self.inner.client()?;

        let _ = self.inner.drop_handles.get_or_init(|| {
            // Spawn the task that will listen to all the room updates at once.
            let listen_updates_task = spawn(Self::listen_task(
                self.inner.clone(),
                client.subscribe_to_all_room_updates(),
            ));

            let ignore_user_list_update_task = spawn(Self::ignore_user_list_update_task(
                self.inner.clone(),
                client.subscribe_to_ignore_user_list_changes(),
            ));

            Arc::new(EventCacheDropHandles { listen_updates_task, ignore_user_list_update_task })
        });

        Ok(())
    }

    /// Try to find an event by its ID in all the rooms.
    // Note: replace this with a select-by-id query when this is implemented in a
    // store.
    pub async fn event(&self, event_id: &EventId) -> Option<SyncTimelineEvent> {
        self.inner
            .all_events
            .read()
            .await
            .events
            .get(event_id)
            .map(|(_room_id, event)| event.clone())
    }

    /// Clear all the events from the immutable event cache.
    ///
    /// This keeps all the rooms along with their internal events linked chunks,
    /// but it clears the side immutable cache for events.
    ///
    /// As such, it doesn't emit any [`RoomEventCacheUpdate`], and it's expected
    /// to be only useful in testing contexts.
    // Note: replace this with a remove query when this is implemented in a
    // store.
    #[cfg(any(test, feature = "testing"))]
    pub async fn empty_immutable_cache(&self) {
        self.inner.all_events.write().await.events.clear();
    }

    #[instrument(skip_all)]
    async fn ignore_user_list_update_task(
        inner: Arc<EventCacheInner>,
        mut ignore_user_list_stream: Subscriber<Vec<String>>,
    ) {
        let span = info_span!(parent: Span::none(), "ignore_user_list_update_task");
        span.follows_from(Span::current());

        async move {
            while ignore_user_list_stream.next().await.is_some() {
                inner.clear_all_rooms().await;
            }
        }
        .instrument(span)
        .await;
    }

    #[instrument(skip_all)]
    async fn listen_task(
        inner: Arc<EventCacheInner>,
        mut room_updates_feed: Receiver<RoomUpdates>,
    ) {
        trace!("Spawning the listen task");
        loop {
            match room_updates_feed.recv().await {
                Ok(updates) => {
                    if let Err(err) = inner.handle_room_updates(updates).await {
                        match err {
                            EventCacheError::ClientDropped => {
                                // The client has dropped, exit the listen task.
                                break;
                            }
                            err => {
                                error!("Error when handling room updates: {err}");
                            }
                        }
                    }
                }

                Err(RecvError::Lagged(num_skipped)) => {
                    // Forget everything we know; we could have missed events, and we have
                    // no way to reconcile at the moment!
                    // TODO: implement Smart Matching™,
                    warn!(num_skipped, "Lagged behind room updates, clearing all rooms");
                    inner.clear_all_rooms().await;
                }

                Err(RecvError::Closed) => {
                    // The sender has shut down, exit.
                    break;
                }
            }
        }
    }

    /// Return a room-specific view over the [`EventCache`].
    pub(crate) async fn for_room(
        &self,
        room_id: &RoomId,
    ) -> Result<(Option<RoomEventCache>, Arc<EventCacheDropHandles>)> {
        let Some(drop_handles) = self.inner.drop_handles.get().cloned() else {
            return Err(EventCacheError::NotSubscribedYet);
        };

        let room = self.inner.for_room(room_id).await?;

        Ok((room, drop_handles))
    }

    /// Add an initial set of events to the event cache, reloaded from a cache.
    ///
    /// TODO: temporary for API compat, as the event cache should take care of
    /// its own store.
    #[instrument(skip(self, events))]
    pub async fn add_initial_events(
        &self,
        room_id: &RoomId,
        events: Vec<SyncTimelineEvent>,
        prev_batch: Option<String>,
    ) -> Result<()> {
        let Some(room_cache) = self.inner.for_room(room_id).await? else {
            warn!("unknown room, skipping");
            return Ok(());
        };

        // We could have received events during a previous sync; remove them all, since
        // we can't know where to insert the "initial events" with respect to
        // them.

        room_cache
            .inner
            .replace_all_events_by(events, prev_batch, Default::default(), Default::default())
            .await?;

        Ok(())
    }
}

type AllEventsMap = BTreeMap<OwnedEventId, (OwnedRoomId, SyncTimelineEvent)>;
type RelationsMap = BTreeMap<OwnedEventId, BTreeSet<OwnedEventId>>;

/// Cache wrapper containing both copies of received events and lists of event
/// ids related to them.
#[derive(Default, Clone)]
struct AllEventsCache {
    /// A cache of received events mapped by their event id.
    events: AllEventsMap,
    /// A cache of related event ids for an event id. The key is the original
    /// event id and the value a list of event ids related to it.
    relations: RelationsMap,
}

struct EventCacheInner {
    /// A weak reference to the inner client, useful when trying to get a handle
    /// on the owning client.
    client: WeakClient,

    /// A lock used when many rooms must be updated at once.
    ///
    /// [`Mutex`] is “fair”, as it is implemented as a FIFO. It is important to
    /// ensure that multiple updates will be applied in the correct order, which
    /// is enforced by taking this lock when handling an update.
    // TODO: that's the place to add a cross-process lock!
    multiple_room_updates_lock: Mutex<()>,

    /// Lazily-filled cache of live [`RoomEventCache`], once per room.
    by_room: RwLock<BTreeMap<OwnedRoomId, RoomEventCache>>,

    /// All events, keyed by event id.
    ///
    /// Since events are immutable in Matrix, this is append-only — events can
    /// be updated, though (e.g. if it was encrypted before, and
    /// successfully decrypted later).
    ///
    /// This is shared between the [`EventCache`] singleton and all
    /// [`RoomEventCache`] instances.
    all_events: Arc<RwLock<AllEventsCache>>,

    /// Handles to keep alive the task listening to updates.
    drop_handles: OnceLock<Arc<EventCacheDropHandles>>,
}

impl EventCacheInner {
    fn client(&self) -> Result<Client> {
        self.client.get().ok_or(EventCacheError::ClientDropped)
    }

    /// Clears all the room's data.
    async fn clear_all_rooms(&self) {
        // Note: one must NOT clear the `by_room` map, because if something subscribed
        // to a room update, they would never get any new update for that room, since
        // re-creating the `RoomEventCache` would create a new unrelated sender.

        // Note 2: we don't need to clear the [`Self::events`] map, because events are
        // immutable in the Matrix protocol.

        let rooms = self.by_room.write().await;
        for room in rooms.values() {
            // Notify all the observers that we've lost track of state. (We ignore the
            // error if there aren't any.)
            let _ = room.inner.sender.send(RoomEventCacheUpdate::Clear);
            // Clear all the events in memory.
            let mut events = room.inner.events.write().await;
            room.inner.clear(&mut events).await;
        }
    }

    /// Handles a single set of room updates at once.
    #[instrument(skip(self, updates))]
    async fn handle_room_updates(&self, updates: RoomUpdates) -> Result<()> {
        // First, take the lock that indicates we're processing updates, to avoid
        // handling multiple updates concurrently.
        let _lock = self.multiple_room_updates_lock.lock().await;

        // Left rooms.
        for (room_id, left_room_update) in updates.leave {
            let Some(room) = self.for_room(&room_id).await? else {
                warn!(%room_id, "missing left room");
                continue;
            };

            if let Err(err) = room.inner.handle_left_room_update(left_room_update).await {
                // Non-fatal error, try to continue to the next room.
                error!("handling left room update: {err}");
            }
        }

        // Joined rooms.
        for (room_id, joined_room_update) in updates.join {
            let Some(room) = self.for_room(&room_id).await? else {
                warn!(%room_id, "missing joined room");
                continue;
            };

            if let Err(err) = room.inner.handle_joined_room_update(joined_room_update).await {
                // Non-fatal error, try to continue to the next room.
                error!("handling joined room update: {err}");
            }
        }

        // Invited rooms.
        // TODO: we don't anything with `updates.invite` at this point.

        Ok(())
    }

    /// Return a room-specific view over the [`EventCache`].
    ///
    /// It may not be found, if the room isn't known to the client, in which
    /// case it'll return None.
    async fn for_room(&self, room_id: &RoomId) -> Result<Option<RoomEventCache>> {
        // Fast path: the entry exists; let's acquire a read lock, it's cheaper than a
        // write lock.
        let by_room_guard = self.by_room.read().await;

        match by_room_guard.get(room_id) {
            Some(room) => Ok(Some(room.clone())),

            None => {
                // Slow-path: the entry doesn't exist; let's acquire a write lock.
                drop(by_room_guard);
                let mut by_room_guard = self.by_room.write().await;

                // In the meanwhile, some other caller might have obtained write access and done
                // the same, so check for existence again.
                if let Some(room) = by_room_guard.get(room_id) {
                    return Ok(Some(room.clone()));
                }

                let room_event_cache = RoomEventCache::new(
                    self.client.clone(),
                    room_id.to_owned(),
                    self.all_events.clone(),
                );

                by_room_guard.insert(room_id.to_owned(), room_event_cache.clone());

                Ok(Some(room_event_cache))
            }
        }
    }
}

/// A subset of an event cache, for a room.
///
/// Cloning is shallow, and thus is cheap to do.
#[derive(Clone)]
pub struct RoomEventCache {
    inner: Arc<RoomEventCacheInner>,
}

impl Debug for RoomEventCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoomEventCache").finish_non_exhaustive()
    }
}

impl RoomEventCache {
    /// Create a new [`RoomEventCache`] using the given room and store.
    fn new(
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
        let events =
            self.inner.events.read().await.events().map(|(_position, item)| item.clone()).collect();

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
            self.inner.all_events_cache.read().await.events.get(event_id).cloned()
        {
            if room_id == self.inner.room_id {
                return Some(event);
            }
        }

        let events = self.inner.events.read().await;
        for (_pos, event) in events.revents() {
            if event.event_id().as_deref() == Some(event_id) {
                return Some(event.clone());
            }
        }
        None
    }

    /// Try to find an event by id in this room, along with all relations.
    pub async fn event_with_relations(
        &self,
        event_id: &EventId,
    ) -> Option<(SyncTimelineEvent, Vec<SyncTimelineEvent>)> {
        let mut relation_events = Vec::new();

        let cache = self.inner.all_events_cache.read().await;
        if let Some((_, event)) = cache.events.get(event_id) {
            Self::collect_related_events(&cache, event_id, &mut relation_events);
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
        results: &mut Vec<SyncTimelineEvent>,
    ) {
        if let Some(related_event_ids) = cache.relations.get(event_id) {
            for id in related_event_ids {
                // If the event was already added to the related ones, skip it.
                if results.iter().any(|e| {
                    e.event_id().is_some_and(|added_related_event_id| added_related_event_id == *id)
                }) {
                    continue;
                }
                if let Some((_, ev)) = cache.events.get(id) {
                    results.push(ev.clone());
                    Self::collect_related_events(cache, id, results);
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
            let mut cache = self.inner.all_events_cache.write().await;

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
        let mut cache = self.inner.all_events_cache.write().await;
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

/// The (non-clonable) details of the `RoomEventCache`.
struct RoomEventCacheInner {
    /// The room id for this room.
    room_id: OwnedRoomId,

    /// Sender part for subscribers to this room.
    sender: Sender<RoomEventCacheUpdate>,

    /// The events of the room.
    events: RwLock<RoomEvents>,

    /// See comment of [`EventCacheInner::events`].
    all_events_cache: Arc<RwLock<AllEventsCache>>,

    /// A paginator instance, that's configured to run back-pagination on our
    /// behalf.
    ///
    /// Note: forward-paginations are still run "out-of-band", that is,
    /// disconnected from the event cache, as we don't implement matching
    /// events received from those kinds of pagination with the cache. This
    /// paginator is only used for queries that interact with the actual event
    /// cache.
    ///
    /// It's protected behind a lock to avoid multiple accesses to the paginator
    /// at the same time.
    pagination: RoomPaginationData<WeakRoom>,
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
            events: RwLock::new(RoomEvents::default()),
            all_events_cache,
            sender,
            pagination: RoomPaginationData {
                paginator: Paginator::new(weak_room),
                waited_for_initial_prev_token: Mutex::new(false),
                token_notifier: Default::default(),
            },
        }
    }

    async fn clear(&self, room_events: &mut RwLockWriteGuard<'_, RoomEvents>) {
        room_events.reset();

        // Reset the back-pagination state to the initial too.
        *self.pagination.waited_for_initial_prev_token.lock().await = false;
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

    async fn handle_joined_room_update(&self, updates: JoinedRoomUpdate) -> Result<()> {
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

    async fn handle_left_room_update(&self, updates: LeftRoomUpdate) -> Result<()> {
        self.handle_timeline(updates.timeline, Vec::new(), updates.ambiguity_changes).await?;
        Ok(())
    }

    /// Remove existing events, and append a set of events to the room cache and
    /// storage, notifying observers.
    async fn replace_all_events_by(
        &self,
        sync_timeline_events: Vec<SyncTimelineEvent>,
        prev_batch: Option<String>,
        ephemeral_events: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    ) -> Result<()> {
        // Acquire the lock.
        let mut room_events = self.events.write().await;

        // Reset the room's state.
        self.clear(&mut room_events).await;

        // Propagate to observers.
        let _ = self.sender.send(RoomEventCacheUpdate::Clear);

        // Push the new events.
        self.append_events_locked_impl(
            room_events,
            sync_timeline_events,
            prev_batch,
            ephemeral_events,
            ambiguity_changes,
        )
        .await
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
            self.events.write().await,
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
        if let Ok(AnySyncTimelineEvent::MessageLike(ev)) = event.event.deserialize() {
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
                        .insert(ev.event_id.to_owned());
                }
            } else {
                let original_event_id = match ev.original_content() {
                    Some(AnyMessageLikeEventContent::RoomMessage(c)) => {
                        if let Some(relation) = c.relates_to {
                            match relation {
                                Relation::Replacement(replacement) => Some(replacement.event_id),
                                Relation::Reply { in_reply_to } => Some(in_reply_to.event_id),
                                Relation::Thread(thread) => Some(thread.event_id),
                                // Do nothing for custom
                                _ => None,
                            }
                        } else {
                            None
                        }
                    }
                    Some(AnyMessageLikeEventContent::PollResponse(c)) => {
                        Some(c.relates_to.event_id)
                    }
                    Some(AnyMessageLikeEventContent::PollEnd(c)) => Some(c.relates_to.event_id),
                    Some(AnyMessageLikeEventContent::UnstablePollResponse(c)) => {
                        Some(c.relates_to.event_id)
                    }
                    Some(AnyMessageLikeEventContent::UnstablePollEnd(c)) => {
                        Some(c.relates_to.event_id)
                    }
                    Some(AnyMessageLikeEventContent::Reaction(c)) => Some(c.relates_to.event_id),
                    _ => None,
                };

                if let Some(event_id) = original_event_id {
                    cache.relations.entry(event_id).or_default().insert(ev.event_id().to_owned());
                }
            }
        }
    }

    /// Append a set of events, with an attached lock.
    ///
    /// If the lock `room_events` is `None`, one will be created.
    ///
    /// This is a private implementation. It must not be exposed publicly.
    async fn append_events_locked_impl(
        &self,
        mut room_events: RwLockWriteGuard<'_, RoomEvents>,
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

            let mut cache = self.all_events_cache.write().await;
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
            self.pagination.token_notifier.notify_one();
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

/// The result of a single back-pagination request.
#[derive(Debug)]
pub struct BackPaginationOutcome {
    /// Did the back-pagination reach the start of the timeline?
    pub reached_start: bool,

    /// All the events that have been returned in the back-pagination
    /// request.
    ///
    /// Events are presented in reverse order: the first element of the vec,
    /// if present, is the most "recent" event from the chunk (or
    /// technically, the last one in the topological ordering).
    ///
    /// Note: they're not deduplicated (TODO: smart reconciliation).
    pub events: Vec<TimelineEvent>,
}

/// An update related to events happened in a room.
#[derive(Debug, Clone)]
pub enum RoomEventCacheUpdate {
    /// The room has been cleared from events.
    Clear,

    /// The fully read marker has moved to a different event.
    MoveReadMarkerTo {
        /// Event at which the read marker is now pointing.
        event_id: OwnedEventId,
    },

    /// The members have changed.
    UpdateMembers {
        /// Collection of ambiguity changes that room member events trigger.
        ///
        /// This is a map of event ID of the `m.room.member` event to the
        /// details of the ambiguity change.
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    },

    /// The room has received new timeline events.
    AddTimelineEvents {
        /// All the new events that have been added to the room's timeline.
        events: Vec<SyncTimelineEvent>,

        /// Where the events are coming from.
        origin: EventsOrigin,
    },

    /// The room has received new ephemeral events.
    AddEphemeralEvents {
        /// XXX: this is temporary, until read receipts are handled in the event
        /// cache
        events: Vec<Raw<AnySyncEphemeralRoomEvent>>,
    },
}

/// Indicate where events are coming from.
#[derive(Debug, Clone)]
pub enum EventsOrigin {
    /// Events are coming from a sync.
    Sync,
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use futures_util::FutureExt as _;
    use matrix_sdk_base::sync::{JoinedRoomUpdate, RoomUpdates, Timeline};
    use matrix_sdk_common::deserialized_responses::SyncTimelineEvent;
    use matrix_sdk_test::async_test;
    use ruma::{
        event_id, events::room::message::RoomMessageEventContentWithoutRelation, room_id,
        serde::Raw, user_id, RoomId,
    };
    use serde_json::json;

    use super::{EventCacheError, RoomEventCacheUpdate};
    use crate::test_utils::{assert_event_matches_msg, events::EventFactory, logged_in_client};

    #[async_test]
    async fn test_must_explicitly_subscribe() {
        let client = logged_in_client(None).await;

        let event_cache = client.event_cache();

        // If I create a room event subscriber for a room before subscribing the event
        // cache,
        let room_id = room_id!("!omelette:fromage.fr");
        let result = event_cache.for_room(room_id).await;

        // Then it fails, because one must explicitly call `.subscribe()` on the event
        // cache.
        assert_matches!(result, Err(EventCacheError::NotSubscribedYet));
    }

    #[async_test]
    async fn test_uniq_read_marker() {
        let client = logged_in_client(None).await;
        let room_id = room_id!("!galette:saucisse.bzh");
        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);

        let event_cache = client.event_cache();

        event_cache.subscribe().unwrap();

        let (room_event_cache, _drop_handles) = event_cache.for_room(room_id).await.unwrap();
        let room_event_cache = room_event_cache.unwrap();

        let (events, mut stream) = room_event_cache.subscribe().await.unwrap();

        assert!(events.is_empty());

        // When sending multiple times the same read marker event,…
        let read_marker_event = Raw::from_json_string(
            json!({
                "content": {
                    "event_id": "$crepe:saucisse.bzh"
                },
                "room_id": "!galette:saucisse.bzh",
                "type": "m.fully_read"
            })
            .to_string(),
        )
        .unwrap();
        let account_data = vec![read_marker_event; 100];

        room_event_cache
            .inner
            .handle_joined_room_update(JoinedRoomUpdate { account_data, ..Default::default() })
            .await
            .unwrap();

        // … there's only one read marker update.
        assert_matches!(
            stream.recv().await.unwrap(),
            RoomEventCacheUpdate::MoveReadMarkerTo { .. }
        );

        assert!(stream.recv().now_or_never().is_none());
    }

    #[async_test]
    async fn test_get_event_by_id() {
        let client = logged_in_client(None).await;
        let room_id1 = room_id!("!galette:saucisse.bzh");
        let room_id2 = room_id!("!crepe:saucisse.bzh");

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        // Insert two rooms with a few events.
        let f = EventFactory::new().room(room_id1).sender(user_id!("@ben:saucisse.bzh"));

        let eid1 = event_id!("$1");
        let eid2 = event_id!("$2");
        let eid3 = event_id!("$3");

        let joined_room_update1 = JoinedRoomUpdate {
            timeline: Timeline {
                events: vec![
                    f.text_msg("hey").event_id(eid1).into(),
                    f.text_msg("you").event_id(eid2).into(),
                ],
                ..Default::default()
            },
            ..Default::default()
        };

        let joined_room_update2 = JoinedRoomUpdate {
            timeline: Timeline {
                events: vec![f.text_msg("bjr").event_id(eid3).into()],
                ..Default::default()
            },
            ..Default::default()
        };

        let mut updates = RoomUpdates::default();
        updates.join.insert(room_id1.to_owned(), joined_room_update1);
        updates.join.insert(room_id2.to_owned(), joined_room_update2);

        // Have the event cache handle them.
        event_cache.inner.handle_room_updates(updates).await.unwrap();

        // Now retrieve all the events one by one.
        let found1 = event_cache.event(eid1).await.unwrap();
        assert_event_matches_msg(&found1, "hey");

        let found2 = event_cache.event(eid2).await.unwrap();
        assert_event_matches_msg(&found2, "you");

        let found3 = event_cache.event(eid3).await.unwrap();
        assert_event_matches_msg(&found3, "bjr");

        // An unknown event won't be found.
        assert!(event_cache.event(event_id!("$unknown")).await.is_none());

        // Can also find events in a single room.
        client.base_client().get_or_create_room(room_id1, matrix_sdk_base::RoomState::Joined);
        let room1 = client.get_room(room_id1).unwrap();

        let (room_event_cache, _drop_handles) = room1.event_cache().await.unwrap();

        let found1 = room_event_cache.event(eid1).await.unwrap();
        assert_event_matches_msg(&found1, "hey");

        let found2 = room_event_cache.event(eid2).await.unwrap();
        assert_event_matches_msg(&found2, "you");

        // Retrieving the event with id3 from the room which doesn't contain it will
        // fail…
        assert!(room_event_cache.event(eid3).await.is_none());
        // …but it doesn't fail at the client-wide level.
        assert!(event_cache.event(eid3).await.is_some());
    }

    #[async_test]
    async fn test_save_event_and_clear() {
        let client = logged_in_client(None).await;
        let room_id = room_id!("!galette:saucisse.bzh");

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));
        let event_id = event_id!("$1");

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
        room_event_cache.save_event(f.text_msg("hey there").event_id(event_id).into()).await;

        // Retrieving the event at the room-wide cache works.
        assert!(room_event_cache.event(event_id).await.is_some());
        // Also at the client level.
        assert!(event_cache.event(event_id).await.is_some());

        event_cache.empty_immutable_cache().await;

        // After clearing, both fail to find the event.
        assert!(room_event_cache.event(event_id).await.is_none());
        assert!(event_cache.event(event_id).await.is_none());
    }

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
            room_event_cache.event_with_relations(original_id).await.unwrap();
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
            room_event_cache.event_with_relations(&original_event_id).await.unwrap();
        // Fetched event is the right one.
        let cached_event_id = event.event_id().unwrap();
        assert_eq!(cached_event_id, original_event_id);

        // There is only the actually related event in the related ones
        assert_eq!(related_events.len(), 1);
        let related_event_id = related_events[0].event_id().unwrap();
        assert_eq!(related_event_id, related_id);
    }
}
