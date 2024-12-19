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
//! It's intended to be fast, robust and easy to maintain, having learned from
//! previous endeavours at implementing middle to high level features elsewhere
//! in the SDK, notably in the UI's Timeline object.
//!
//! See the [github issue](https://github.com/matrix-org/matrix-rust-sdk/issues/3058) for more
//! details about the historical reasons that led us to start writing this.

#![forbid(missing_docs)]

use std::{
    collections::BTreeMap,
    fmt::Debug,
    sync::{Arc, OnceLock},
};

use eyeball::Subscriber;
use eyeball_im::VectorDiff;
use matrix_sdk_base::{
    deserialized_responses::{AmbiguityChange, SyncTimelineEvent, TimelineEvent},
    event_cache::store::{EventCacheStoreError, EventCacheStoreLock},
    store_locks::LockStoreError,
    sync::RoomUpdates,
};
use matrix_sdk_common::executor::{spawn, JoinHandle};
use once_cell::sync::OnceCell;
use room::RoomEventCacheState;
use ruma::{
    events::{relation::RelationType, AnySyncEphemeralRoomEvent},
    serde::Raw,
    EventId, OwnedEventId, OwnedRoomId, RoomId,
};
use tokio::sync::{
    broadcast::{error::RecvError, Receiver},
    Mutex, RwLock,
};
use tracing::{error, info, info_span, instrument, trace, warn, Instrument as _, Span};

use self::paginator::PaginatorError;
use crate::{client::WeakClient, Client};

mod deduplicator;
mod pagination;
mod room;

pub mod paginator;
pub use pagination::{RoomPagination, TimelineHasBeenResetWhilePaginating};
pub use room::RoomEventCache;

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

    /// An error happening when interacting with storage.
    #[error(transparent)]
    Storage(#[from] EventCacheStoreError),

    /// An error happening when attempting to (cross-process) lock storage.
    #[error(transparent)]
    LockingStorage(#[from] LockStoreError),

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
                store: Default::default(),
                multiple_room_updates_lock: Default::default(),
                by_room: Default::default(),
                drop_handles: Default::default(),
                all_events: Default::default(),
            }),
        }
    }

    /// Enable storing updates to storage, and reload events from storage.
    ///
    /// Has an effect only the first time it's called. It's safe to call it
    /// multiple times.
    pub fn enable_storage(&self) -> Result<()> {
        let _ = self.inner.store.get_or_try_init::<_, EventCacheError>(|| {
            let client = self.inner.client()?;
            Ok(client.event_cache_store().clone())
        })?;
        Ok(())
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
                info!("received an ignore user list change");
                if let Err(err) = inner.clear_all_rooms().await {
                    error!("error when clearing room storage: {err}");
                }
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
                    if let Err(err) = inner.clear_all_rooms().await {
                        error!("error when clearing storage: {err}");
                    }
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
    ) -> Result<(RoomEventCache, Arc<EventCacheDropHandles>)> {
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
        // If the event cache's storage has been enabled, do nothing.
        if self.inner.has_storage() {
            return Ok(());
        }

        let room_cache = self.inner.for_room(room_id).await?;

        // If the linked chunked already has at least one event, ignore this request, as
        // it should happen at most once per room.
        if !room_cache.inner.state.read().await.events().is_empty() {
            return Ok(());
        }

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
type RelationsMap = BTreeMap<OwnedEventId, BTreeMap<OwnedEventId, RelationType>>;

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

impl AllEventsCache {
    fn clear(&mut self) {
        self.events.clear();
        self.relations.clear();
    }
}

struct EventCacheInner {
    /// A weak reference to the inner client, useful when trying to get a handle
    /// on the owning client.
    client: WeakClient,

    /// Reference to the underlying store.
    ///
    /// Set to none if we shouldn't use storage for reading / writing linked
    /// chunks.
    store: Arc<OnceCell<EventCacheStoreLock>>,

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
    /// This is shared between the [`EventCacheInner`] singleton and all
    /// [`RoomEventCacheInner`] instances.
    all_events: Arc<RwLock<AllEventsCache>>,

    /// Handles to keep alive the task listening to updates.
    drop_handles: OnceLock<Arc<EventCacheDropHandles>>,
}

impl EventCacheInner {
    fn client(&self) -> Result<Client> {
        self.client.get().ok_or(EventCacheError::ClientDropped)
    }

    /// Has persistent storage been enabled for the event cache?
    fn has_storage(&self) -> bool {
        self.store.get().is_some()
    }

    /// Clears all the room's data.
    async fn clear_all_rooms(&self) -> Result<()> {
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
            // Clear all the room state.
            room.inner.state.write().await.reset().await?;
        }

        Ok(())
    }

    /// Handles a single set of room updates at once.
    #[instrument(skip(self, updates))]
    async fn handle_room_updates(&self, updates: RoomUpdates) -> Result<()> {
        // First, take the lock that indicates we're processing updates, to avoid
        // handling multiple updates concurrently.
        let _lock = self.multiple_room_updates_lock.lock().await;

        // Left rooms.
        for (room_id, left_room_update) in updates.leave {
            let room = self.for_room(&room_id).await?;

            if let Err(err) =
                room.inner.handle_left_room_update(self.has_storage(), left_room_update).await
            {
                // Non-fatal error, try to continue to the next room.
                error!("handling left room update: {err}");
            }
        }

        // Joined rooms.
        for (room_id, joined_room_update) in updates.join {
            let room = self.for_room(&room_id).await?;

            if let Err(err) =
                room.inner.handle_joined_room_update(self.has_storage(), joined_room_update).await
            {
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
    async fn for_room(&self, room_id: &RoomId) -> Result<RoomEventCache> {
        // Fast path: the entry exists; let's acquire a read lock, it's cheaper than a
        // write lock.
        let by_room_guard = self.by_room.read().await;

        match by_room_guard.get(room_id) {
            Some(room) => Ok(room.clone()),

            None => {
                // Slow-path: the entry doesn't exist; let's acquire a write lock.
                drop(by_room_guard);
                let mut by_room_guard = self.by_room.write().await;

                // In the meanwhile, some other caller might have obtained write access and done
                // the same, so check for existence again.
                if let Some(room) = by_room_guard.get(room_id) {
                    return Ok(room.clone());
                }

                let room_state =
                    RoomEventCacheState::new(room_id.to_owned(), self.store.clone()).await?;

                let room_event_cache = RoomEventCache::new(
                    self.client.clone(),
                    room_state,
                    room_id.to_owned(),
                    self.all_events.clone(),
                );

                by_room_guard.insert(room_id.to_owned(), room_event_cache.clone());

                Ok(room_event_cache)
            }
        }
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
    // TODO: remove once `UpdateTimelineEvents` is stabilized
    AddTimelineEvents {
        /// All the new events that have been added to the room's timeline.
        events: Vec<SyncTimelineEvent>,

        /// Where the events are coming from.
        origin: EventsOrigin,
    },

    /// The room has received updates for the timeline as _diffs_.
    UpdateTimelineEvents {
        /// Diffs to apply to the timeline.
        diffs: Vec<VectorDiff<SyncTimelineEvent>>,

        /// Where the diffs are coming from.
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
    use matrix_sdk_test::{async_test, event_factory::EventFactory};
    use ruma::{event_id, room_id, serde::Raw, user_id};
    use serde_json::json;

    use super::{EventCacheError, RoomEventCacheUpdate};
    use crate::test_utils::{assert_event_matches_msg, logged_in_client};

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
            .handle_joined_room_update(
                event_cache.inner.has_storage(),
                JoinedRoomUpdate { account_data, ..Default::default() },
            )
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
    async fn test_add_initial_events() {
        // TODO: remove this test when the event cache uses its own persistent storage.
        let client = logged_in_client(None).await;
        let room_id = room_id!("!galette:saucisse.bzh");

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));
        event_cache
            .add_initial_events(room_id, vec![f.text_msg("hey").into()], None)
            .await
            .unwrap();

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
        let (initial_events, _) = room_event_cache.subscribe().await.unwrap();
        // `add_initial_events` had an effect.
        assert_eq!(initial_events.len(), 1);
    }
}
