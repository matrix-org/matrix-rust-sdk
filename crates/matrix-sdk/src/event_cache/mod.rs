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
    collections::BTreeMap,
    fmt::Debug,
    sync::{Arc, OnceLock, Weak},
};

use eyeball::Subscriber;
use matrix_sdk_base::{
    deserialized_responses::{AmbiguityChange, SyncTimelineEvent, TimelineEvent},
    sync::{JoinedRoomUpdate, LeftRoomUpdate, RoomUpdates, Timeline},
};
use matrix_sdk_common::executor::{spawn, JoinHandle};
use ruma::{
    events::{AnyRoomAccountDataEvent, AnySyncEphemeralRoomEvent},
    serde::Raw,
    OwnedEventId, OwnedRoomId, RoomId,
};
use tokio::sync::{
    broadcast::{error::RecvError, Receiver, Sender},
    Mutex, RwLock, RwLockWriteGuard,
};
use tracing::{error, info_span, instrument, trace, warn, Instrument as _, Span};

use self::{
    pagination::RoomPaginationData,
    paginator::{Paginator, PaginatorError},
    store::{Gap, RoomEvents},
};
use crate::{client::ClientInner, Client, Room};

mod linked_chunk;
mod pagination;
mod store;

pub mod paginator;
pub use pagination::RoomPagination;

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
    /// Technically, it's possible to request a `RoomEventCache` for a room that
    /// is not known to the client, leading to this error.
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

    /// Another error caused by the SDK happened somewhere, and we report it to
    /// the caller.
    #[error("SDK error: {0}")]
    SdkError(#[source] crate::Error),
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
    pub(crate) fn new(client: &Arc<ClientInner>) -> Self {
        Self {
            inner: Arc::new(EventCacheInner {
                client: Arc::downgrade(client),
                multiple_room_updates_lock: Default::default(),
                by_room: Default::default(),
                drop_handles: Default::default(),
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

struct EventCacheInner {
    /// A weak reference to the inner client, useful when trying to get a handle
    /// on the owning client.
    client: Weak<ClientInner>,

    /// A lock used when many rooms must be updated at once.
    ///
    /// [`Mutex`] is “fair”, as it is implemented as a FIFO. It is important to
    /// ensure that multiple updates will be applied in the correct order, which
    /// is enforced by taking this lock when handling an update.
    // TODO: that's the place to add a cross-process lock!
    multiple_room_updates_lock: Mutex<()>,

    /// Lazily-filled cache of live [`RoomEventCache`], once per room.
    by_room: RwLock<BTreeMap<OwnedRoomId, RoomEventCache>>,

    /// Handles to keep alive the task listening to updates.
    drop_handles: OnceLock<Arc<EventCacheDropHandles>>,
}

impl EventCacheInner {
    fn client(&self) -> Result<Client> {
        Ok(Client { inner: self.client.upgrade().ok_or(EventCacheError::ClientDropped)? })
    }

    /// Clears all the room's data.
    async fn clear_all_rooms(&self) {
        // Note: one must NOT clear the `by_room` map, because if something subscribed
        // to a room update, they would never get any new update for that room, since
        // re-creating the `RoomEventCache` would create a new unrelated sender.

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

                let Some(room) = self.client()?.get_room(room_id) else {
                    return Ok(None);
                };

                let room_event_cache = RoomEventCache::new(room);

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
    fn new(room: Room) -> Self {
        Self { inner: Arc::new(RoomEventCacheInner::new(room)) }
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
}

/// The (non-clonable) details of the `RoomEventCache`.
struct RoomEventCacheInner {
    /// Sender part for subscribers to this room.
    sender: Sender<RoomEventCacheUpdate>,

    /// The events of the room.
    events: RwLock<RoomEvents>,

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
    pagination: RoomPaginationData,
}

impl RoomEventCacheInner {
    /// Creates a new cache for a room, and subscribes to room updates, so as
    /// to handle new timeline events.
    fn new(room: Room) -> Self {
        let sender = Sender::new(32);

        Self {
            events: RwLock::new(RoomEvents::default()),
            sender,
            pagination: RoomPaginationData {
                paginator: Paginator::new(Box::new(room)),
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
                    let _ = self.sender.send(RoomEventCacheUpdate::UpdateReadMarker {
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
        ephemeral: Vec<Raw<AnySyncEphemeralRoomEvent>>,
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
                ephemeral,
                ambiguity_changes,
            )
            .await?;
        } else {
            // Add all the events to the backend.
            trace!("adding new events");

            self.append_new_events(
                timeline.events,
                timeline.prev_batch,
                ephemeral,
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
        events: Vec<SyncTimelineEvent>,
        prev_batch: Option<String>,
        ephemeral: Vec<Raw<AnySyncEphemeralRoomEvent>>,
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
            events,
            prev_batch,
            ephemeral,
            ambiguity_changes,
        )
        .await
    }

    /// Append a set of events to the room cache and storage, notifying
    /// observers.
    async fn append_new_events(
        &self,
        events: Vec<SyncTimelineEvent>,
        prev_batch: Option<String>,
        ephemeral: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    ) -> Result<()> {
        self.append_events_locked_impl(
            self.events.write().await,
            events,
            prev_batch,
            ephemeral,
            ambiguity_changes,
        )
        .await
    }

    /// Append a set of events, with an attached lock.
    ///
    /// If the lock `room_events` is `None`, one will be created.
    ///
    /// This is a private implementation. It must not be exposed publicly.
    async fn append_events_locked_impl(
        &self,
        mut room_events: RwLockWriteGuard<'_, RoomEvents>,
        events: Vec<SyncTimelineEvent>,
        prev_batch: Option<String>,
        ephemeral: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    ) -> Result<()> {
        if events.is_empty()
            && prev_batch.is_none()
            && ephemeral.is_empty()
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

            room_events.push_events(events.clone().into_iter());
        }

        // Now that all events have been added, we can trigger the
        // `pagination_token_notifier`.
        if prev_batch.is_some() {
            self.pagination.token_notifier.notify_one();
        }

        let _ =
            self.sender.send(RoomEventCacheUpdate::Append { events, ephemeral, ambiguity_changes });

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
    UpdateReadMarker {
        /// Event at which the read marker is now pointing.
        event_id: OwnedEventId,
    },

    /// The room has new events.
    Append {
        /// All the new events that have been added to the room's timeline.
        events: Vec<SyncTimelineEvent>,
        /// XXX: this is temporary, until read receipts are handled in the event
        /// cache
        ephemeral: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        /// Collection of ambiguity changes that room member events trigger.
        ///
        /// This is a map of event ID of the `m.room.member` event to the
        /// details of the ambiguity change.
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    },
}

#[cfg(test)]
mod tests {
    use assert_matches2::assert_matches;
    use futures_util::FutureExt as _;
    use matrix_sdk_base::sync::JoinedRoomUpdate;
    use matrix_sdk_test::async_test;
    use ruma::{room_id, serde::Raw};
    use serde_json::json;

    use super::{EventCacheError, RoomEventCacheUpdate};
    use crate::test_utils::logged_in_client;

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
            RoomEventCacheUpdate::UpdateReadMarker { .. }
        );

        assert!(stream.recv().now_or_never().is_none());
    }
}
