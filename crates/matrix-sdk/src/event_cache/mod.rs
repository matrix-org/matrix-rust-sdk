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

use eyeball::{SharedObservable, Subscriber};
use eyeball_im::VectorDiff;
use futures_util::future::{join_all, try_join_all};
use matrix_sdk_base::{
    deserialized_responses::{AmbiguityChange, TimelineEvent},
    event_cache::store::{EventCacheStoreError, EventCacheStoreLock},
    linked_chunk::lazy_loader::LazyLoaderError,
    store_locks::LockStoreError,
    sync::RoomUpdates,
};
use matrix_sdk_common::executor::{spawn, JoinHandle};
use room::RoomEventCacheState;
use ruma::{
    events::AnySyncEphemeralRoomEvent, serde::Raw, OwnedEventId, OwnedRoomId, RoomId, RoomVersionId,
};
use tokio::sync::{
    broadcast::{error::RecvError, Receiver},
    mpsc, Mutex, RwLock,
};
use tracing::{debug, error, info, info_span, instrument, trace, warn, Instrument as _, Span};

use self::paginator::PaginatorError;
use crate::{client::WeakClient, Client};

mod deduplicator;
mod pagination;
mod room;

pub mod paginator;
pub use pagination::{PaginationToken, RoomPagination, RoomPaginationStatus};
pub use room::{RoomEventCache, RoomEventCacheListener};

/// An error observed in the [`EventCache`].
#[derive(thiserror::Error, Debug)]
pub enum EventCacheError {
    /// The [`EventCache`] instance hasn't been initialized with
    /// [`EventCache::subscribe`]
    #[error(
        "The EventCache hasn't subscribed to sync responses yet, call `EventCache::subscribe()`"
    )]
    NotSubscribedYet,

    /// An error has been observed while back-paginating.
    #[error(transparent)]
    BackpaginationError(#[from] PaginatorError),

    /// Back-pagination was already happening in a given room, where we tried to
    /// back-paginate again.
    #[error("We were already back-paginating.")]
    AlreadyBackpaginating,

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

    /// An error happening when interacting with the [`LinkedChunk`]'s lazy
    /// loader.
    ///
    /// [`LinkedChunk`]: matrix_sdk_common::linked_chunk::LinkedChunk
    #[error(transparent)]
    LinkedChunkLoader(#[from] LazyLoaderError),
}

/// A result using the [`EventCacheError`].
pub type Result<T> = std::result::Result<T, EventCacheError>;

/// Hold handles to the tasks spawn by a [`RoomEventCache`].
pub struct EventCacheDropHandles {
    /// Task that listens to room updates.
    listen_updates_task: JoinHandle<()>,

    /// Task that listens to updates to the user's ignored list.
    ignore_user_list_update_task: JoinHandle<()>,

    /// The task used to automatically shrink the linked chunks.
    auto_shrink_linked_chunk_task: JoinHandle<()>,
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
        self.auto_shrink_linked_chunk_task.abort();
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
    pub(crate) fn new(client: WeakClient, event_cache_store: EventCacheStoreLock) -> Self {
        Self {
            inner: Arc::new(EventCacheInner {
                client,
                store: event_cache_store,
                multiple_room_updates_lock: Default::default(),
                by_room: Default::default(),
                drop_handles: Default::default(),
                auto_shrink_sender: Default::default(),
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

        // Initialize the drop handles.
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

            let (tx, rx) = mpsc::channel(32);

            // Force-initialize the sender in the [`RoomEventCacheInner`].
            self.inner.auto_shrink_sender.get_or_init(|| tx);

            let auto_shrink_linked_chunk_tasks =
                spawn(Self::auto_shrink_linked_chunk_task(self.inner.clone(), rx));

            Arc::new(EventCacheDropHandles {
                listen_updates_task,
                ignore_user_list_update_task,
                auto_shrink_linked_chunk_task: auto_shrink_linked_chunk_tasks,
            })
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
                info!("Received an ignore user list change");
                if let Err(err) = inner.clear_all_rooms().await {
                    error!("when clearing room storage after ignore user list change: {err}");
                }
            }
            info!("Ignore user list stream has closed");
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
                                info!("Closing the event cache global listen task because client dropped");
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
                        error!("when clearing storage after lag in listen_task: {err}");
                    }
                }

                Err(RecvError::Closed) => {
                    // The sender has shut down, exit.
                    info!("Closing the event cache global listen task because receiver closed");
                    break;
                }
            }
        }
    }

    /// Spawns the task that will listen to auto-shrink notifications.
    ///
    /// The auto-shrink mechanism works this way:
    ///
    /// - Each time there's a new subscriber to a [`RoomEventCache`], it will
    ///   increment the active number of listeners to that room, aka
    ///   [`RoomEventCacheState::listener_count`].
    /// - When that subscriber is dropped, it will decrement that count; and
    ///   notify the task below if it reached 0.
    /// - The task spawned here, owned by the [`EventCacheInner`], will listen
    ///   to such notifications that a room may be shrunk. It will attempt an
    ///   auto-shrink, by letting the inner state decide whether this is a good
    ///   time to do so (new listeners might have spawned in the meanwhile).
    #[instrument(skip_all)]
    async fn auto_shrink_linked_chunk_task(
        inner: Arc<EventCacheInner>,
        mut rx: mpsc::Receiver<AutoShrinkChannelPayload>,
    ) {
        while let Some(room_id) = rx.recv().await {
            trace!(for_room = %room_id, "received notification to shrink");

            let room = match inner.for_room(&room_id).await {
                Ok(room) => room,
                Err(err) => {
                    warn!(for_room = %room_id, "error when getting a RoomEventCache: {err}");
                    continue;
                }
            };

            trace!("waiting for state lock…");
            let mut state = room.inner.state.write().await;

            match state.auto_shrink_if_no_listeners().await {
                Ok(diffs) => {
                    if let Some(diffs) = diffs {
                        // Hey, fun stuff: we shrunk the linked chunk, so there shouldn't be any
                        // listeners, right? RIGHT? Especially because the state is guarded behind
                        // a lock.
                        //
                        // However, better safe than sorry, and it's cheap to send an update here,
                        // so let's do it!
                        if !diffs.is_empty() {
                            let _ = room.inner.sender.send(
                                RoomEventCacheUpdate::UpdateTimelineEvents {
                                    diffs,
                                    origin: EventsOrigin::Cache,
                                },
                            );
                        }
                    } else {
                        debug!("auto-shrinking didn't happen");
                    }
                }

                Err(err) => {
                    // There's not much we can do here, unfortunately.
                    warn!(for_room = %room_id, "error when attempting to shrink linked chunk: {err}");
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

    /// Cleanly clear all the rooms' event caches.
    ///
    /// This will notify any live observers that the room has been cleared.
    pub async fn clear_all_rooms(&self) -> Result<()> {
        self.inner.clear_all_rooms().await
    }
}

struct EventCacheInner {
    /// A weak reference to the inner client, useful when trying to get a handle
    /// on the owning client.
    client: WeakClient,

    /// Reference to the underlying store.
    store: EventCacheStoreLock,

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

    /// A sender for notifications that a room *may* need to be auto-shrunk.
    ///
    /// Needs to live here, so it may be passed to each [`RoomEventCache`]
    /// instance.
    ///
    /// See doc comment of [`EventCache::auto_shrink_linked_chunk_task`].
    auto_shrink_sender: OnceLock<mpsc::Sender<AutoShrinkChannelPayload>>,
}

type AutoShrinkChannelPayload = OwnedRoomId;

impl EventCacheInner {
    fn client(&self) -> Result<Client> {
        self.client.get().ok_or(EventCacheError::ClientDropped)
    }

    /// Clears all the room's data.
    async fn clear_all_rooms(&self) -> Result<()> {
        // Okay, here's where things get complicated.
        //
        // On the one hand, `by_room` may include storage for *some* rooms that we know
        // about, but not *all* of them. Any room that hasn't been loaded in the
        // client, or touched by a sync, will remain unloaded in memory, so it
        // will be missing from `self.by_room`. As a result, we need to make
        // sure that we're hitting the storage backend to *really* clear all the
        // rooms, including those that haven't been loaded yet.
        //
        // On the other hand, one must NOT clear the `by_room` map, because if someone
        // subscribed to a room update, they would never get any new update for
        // that room, since re-creating the `RoomEventCache` would create a new,
        // unrelated sender.
        //
        // So we need to *keep* the rooms in `by_room` alive, while clearing them in the
        // store backend.
        //
        // As a result, for a short while, the in-memory linked chunks
        // will be desynchronized from the storage. We need to be careful then. During
        // that short while, we don't want *anyone* to touch the linked chunk
        // (be it in memory or in the storage).
        //
        // And since that requirement applies to *any* room in `by_room` at the same
        // time, we'll have to take the locks for *all* the live rooms, so as to
        // properly clear the underlying storage.
        //
        // At this point, you might be scared about the potential for deadlocking. I am
        // as well, but I'm convinced we're fine:
        // 1. the lock for `by_room` is usually held only for a short while, and
        //    independently of the other two kinds.
        // 2. the state may acquire the store cross-process lock internally, but only
        //    while the state's methods are called (so it's always transient). As a
        //    result, as soon as we've acquired the state locks, the store lock ought to
        //    be free.
        // 3. The store lock is held explicitly only in a small scoped area below.
        // 4. Then the store lock will be held internally when calling `reset()`, but at
        //    this point it's only held for a short while each time, so rooms will take
        //    turn to acquire it.

        let rooms = self.by_room.write().await;

        // Collect all the rooms' state locks, first: we can clear the storage only when
        // nobody will touch it at the same time.
        let room_locks = join_all(
            rooms.values().map(|room| async move { (room, room.inner.state.write().await) }),
        )
        .await;

        // Clear the storage for all the rooms, using the storage facility.
        self.store.lock().await?.clear_all_linked_chunks().await?;

        // At this point, all the in-memory linked chunks are desynchronized from the
        // storage. Resynchronize them manually by calling reset(), and
        // propagate updates to observers.
        try_join_all(room_locks.into_iter().map(|(room, mut state_guard)| async move {
            let updates_as_vector_diffs = state_guard.reset().await?;
            let _ = room.inner.sender.send(RoomEventCacheUpdate::UpdateTimelineEvents {
                diffs: updates_as_vector_diffs,
                origin: EventsOrigin::Cache,
            });
            Ok::<_, EventCacheError>(())
        }))
        .await?;

        Ok(())
    }

    /// Handles a single set of room updates at once.
    #[instrument(skip(self, updates))]
    async fn handle_room_updates(&self, updates: RoomUpdates) -> Result<()> {
        // First, take the lock that indicates we're processing updates, to avoid
        // handling multiple updates concurrently.
        let _lock = self.multiple_room_updates_lock.lock().await;

        // Left rooms.
        for (room_id, left_room_update) in updates.left {
            let room = self.for_room(&room_id).await?;

            if let Err(err) = room.inner.handle_left_room_update(left_room_update).await {
                // Non-fatal error, try to continue to the next room.
                error!("handling left room update: {err}");
            }
        }

        // Joined rooms.
        for (room_id, joined_room_update) in updates.joined {
            let room = self.for_room(&room_id).await?;

            if let Err(err) = room.inner.handle_joined_room_update(joined_room_update).await {
                // Non-fatal error, try to continue to the next room.
                error!(%room_id, "handling joined room update: {err}");
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

                let pagination_status =
                    SharedObservable::new(RoomPaginationStatus::Idle { hit_timeline_start: false });

                let room_version = self
                    .client
                    .get()
                    .and_then(|client| client.get_room(room_id))
                    .as_ref()
                    .map(|room| room.clone_info().room_version_or_default())
                    .unwrap_or_else(|| {
                        warn!("unknown room version for {room_id}, using default V1");
                        RoomVersionId::V1
                    });

                let room_state = RoomEventCacheState::new(
                    room_id.to_owned(),
                    room_version,
                    self.store.clone(),
                    pagination_status.clone(),
                )
                .await?;

                // SAFETY: we must have subscribed before reaching this coed, otherwise
                // something is very wrong.
                let auto_shrink_sender =
                    self.auto_shrink_sender.get().cloned().expect(
                        "we must have called `EventCache::subscribe()` before calling here.",
                    );

                let room_event_cache = RoomEventCache::new(
                    self.client.clone(),
                    room_state,
                    pagination_status,
                    room_id.to_owned(),
                    auto_shrink_sender,
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
    pub events: Vec<TimelineEvent>,
}

/// An update related to events happened in a room.
#[derive(Debug, Clone)]
pub enum RoomEventCacheUpdate {
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

    /// The room has received updates for the timeline as _diffs_.
    UpdateTimelineEvents {
        /// Diffs to apply to the timeline.
        diffs: Vec<VectorDiff<TimelineEvent>>,

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

    /// Events are coming from pagination.
    Pagination,

    /// The cause of the change is purely internal to the cache.
    Cache,
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

        let (events, mut stream) = room_event_cache.subscribe().await;

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
        updates.joined.insert(room_id1.to_owned(), joined_room_update1);
        updates.joined.insert(room_id2.to_owned(), joined_room_update2);

        // Have the event cache handle them.
        event_cache.inner.handle_room_updates(updates).await.unwrap();

        // We can find the events in a single room.
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
    }

    #[async_test]
    async fn test_save_event() {
        let client = logged_in_client(None).await;
        let room_id = room_id!("!galette:saucisse.bzh");

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));
        let event_id = event_id!("$1");

        client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
        room_event_cache.save_events([f.text_msg("hey there").event_id(event_id).into()]).await;

        // Retrieving the event at the room-wide cache works.
        assert!(room_event_cache.event(event_id).await.is_some());
    }
}
