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
    collections::HashMap,
    fmt,
    sync::{Arc, OnceLock, RwLock as StdRwLock, RwLockReadGuard, RwLockWriteGuard},
};

use futures_util::future::try_join_all;
use matrix_sdk_base::{
    cross_process_lock::CrossProcessLockError,
    event_cache::store::{EventCacheStoreError, EventCacheStoreLock, EventCacheStoreLockState},
    linked_chunk::lazy_loader::LazyLoaderError,
    sync::RoomUpdates,
    task_monitor::BackgroundTaskHandle,
    timer,
};
use ruma::{OwnedRoomId, RoomId};
use tokio::sync::{
    Mutex, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock,
    broadcast::{Receiver, Sender, channel},
    mpsc,
};
use tracing::{error, instrument, trace};

use crate::{
    Client,
    client::{ClientInner, WeakClient},
    paginators::PaginatorError,
};

mod automatic_pagination;
mod caches;
mod deduplicator;
mod persistence;
#[cfg(feature = "e2e-encryption")]
mod redecryptor;
mod tasks;

use caches::{Caches, room::RoomEventCacheLinkedChunkUpdate};
pub use caches::{
    TimelineVectorDiffs,
    event_focused::EventFocusThreadMode,
    pagination::{BackPaginationOutcome, PaginationStatus},
    room::{
        RoomEventCache, RoomEventCacheGenericUpdate, RoomEventCacheSubscriber,
        RoomEventCacheUpdate, pagination::RoomPagination,
    },
    thread::pagination::ThreadPagination,
};
#[cfg(feature = "e2e-encryption")]
pub use redecryptor::{DecryptionRetryRequest, RedecryptorReport};

pub use crate::event_cache::automatic_pagination::AutomaticPagination;

/// An error observed in the [`EventCache`].
#[derive(thiserror::Error, Clone, Debug)]
pub enum EventCacheError {
    /// The [`EventCache`] instance hasn't been initialized with
    /// [`EventCache::subscribe`]
    #[error(
        "The EventCache hasn't subscribed to sync responses yet, call `EventCache::subscribe()`"
    )]
    NotSubscribedYet,

    /// Room is not found.
    #[error("Room `{room_id}` is not found.")]
    RoomNotFound {
        /// The ID of the room not being found.
        room_id: OwnedRoomId,
    },

    /// An error has been observed while back- or forward- paginating.
    #[error(transparent)]
    PaginationError(Arc<crate::Error>),

    /// An error has been observed while initiating an event-focused timeline.
    #[error(transparent)]
    InitialPaginationError(#[from] PaginatorError),

    /// An error happening when interacting with storage.
    #[error(transparent)]
    Storage(#[from] EventCacheStoreError),

    /// An error happening when attempting to (cross-process) lock storage.
    #[error(transparent)]
    LockingStorage(#[from] CrossProcessLockError),

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

    /// An error happened when trying to load pinned events; none of them could
    /// be loaded, which would otherwise result in an empty pinned events
    /// list, incorrectly.
    #[error("Unable to load any of the pinned events.")]
    UnableToLoadPinnedEvents,

    /// An error happened when reading the metadata of a linked chunk, upon
    /// reload.
    #[error("the linked chunk metadata is invalid: {details}")]
    InvalidLinkedChunkMetadata {
        /// A string containing details about the error.
        details: String,
    },
}

/// A result using the [`EventCacheError`].
pub type Result<T> = std::result::Result<T, EventCacheError>;

/// Hold handles to the tasks spawn by a [`EventCache`].
pub struct EventCacheDropHandles {
    /// Task that listens to room updates.
    _listen_updates_task: BackgroundTaskHandle,

    /// Task that listens to updates to the user's ignored list.
    _ignore_user_list_update_task: BackgroundTaskHandle,

    /// The task used to automatically shrink the linked chunks.
    _auto_shrink_linked_chunk_task: BackgroundTaskHandle,

    /// A background task listening to room and send queue updates, and
    /// automatically subscribing the user to threads when needed, based on
    /// the semantics of MSC4306.
    ///
    /// One important constraint is that there is only one such task per
    /// [`EventCache`], so it does listen to *all* rooms at the same time.
    _thread_subscriber_task: BackgroundTaskHandle,

    /// A background task listening to room updates, and
    /// automatically handling search index operations add/remove/edit
    /// depending on the event type.
    ///
    /// One important constraint is that there is only one such task per
    /// [`EventCache`], so it does listen to *all* rooms at the same time.
    #[cfg(feature = "experimental-search")]
    _search_indexing_task: BackgroundTaskHandle,

    /// The task used to automatically redecrypt UTDs.
    #[cfg(feature = "e2e-encryption")]
    _redecryptor: redecryptor::Redecryptor,
}

impl fmt::Debug for EventCacheDropHandles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventCacheDropHandles").finish_non_exhaustive()
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

impl fmt::Debug for EventCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventCache").finish_non_exhaustive()
    }
}

impl EventCache {
    /// Create a new [`EventCache`] for the given client.
    pub(crate) fn new(client: &Arc<ClientInner>, event_cache_store: EventCacheStoreLock) -> Self {
        let (generic_update_sender, _) = channel(128);
        let (linked_chunk_update_sender, _) = channel(128);

        let weak_client = WeakClient::from_inner(client);

        let (thread_subscriber_sender, _thread_subscriber_receiver) = channel(128);

        #[cfg(feature = "e2e-encryption")]
        let redecryption_channels = redecryptor::RedecryptorChannels::new();

        Self {
            inner: Arc::new(EventCacheInner {
                client: weak_client,
                config: StdRwLock::new(EventCacheConfig::default()),
                store: event_cache_store,
                multiple_room_updates_lock: Default::default(),
                by_room: Default::default(),
                drop_handles: Default::default(),
                auto_shrink_sender: Default::default(),
                generic_update_sender,
                linked_chunk_update_sender,
                #[cfg(feature = "e2e-encryption")]
                redecryption_channels,
                automatic_pagination: OnceLock::new(),
                thread_subscriber_sender,
            }),
        }
    }

    /// Get a read-only handle to the global configuration of the
    /// [`EventCache`].
    pub fn config(&self) -> RwLockReadGuard<'_, EventCacheConfig> {
        self.inner.config.read().unwrap()
    }

    /// Get a writable handle to the global configuration of the [`EventCache`].
    pub fn config_mut(&self) -> RwLockWriteGuard<'_, EventCacheConfig> {
        self.inner.config.write().unwrap()
    }

    /// Subscribes to updates that a thread subscription has been sent.
    ///
    /// For testing purposes only.
    #[cfg(feature = "testing")]
    pub fn subscribe_thread_subscriber_updates(&self) -> Receiver<()> {
        self.inner.thread_subscriber_sender.subscribe()
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
            let task_monitor = client.task_monitor();

            // Spawn the task that will listen to all the room updates at once.
            let listen_updates_task = task_monitor.spawn_background_task("event_cache::room_updates_task", tasks::room_updates_task(
                self.inner.clone(),
                client.subscribe_to_all_room_updates(),
            )).abort_on_drop();

            let ignore_user_list_update_task = task_monitor.spawn_background_task("event_cache::ignore_user_list_update_task", tasks::ignore_user_list_update_task(
                self.inner.clone(),
                client.subscribe_to_ignore_user_list_changes(),
            )).abort_on_drop();

            let (auto_shrink_sender, auto_shrink_receiver) = mpsc::channel(32);

            // Force-initialize the sender in the [`RoomEventCacheInner`].
            self.inner.auto_shrink_sender.get_or_init(|| auto_shrink_sender);

            let auto_shrink_linked_chunk_task = task_monitor.spawn_background_task("event_cache::auto_shrink_linked_chunk_task", tasks::auto_shrink_linked_chunk_task(
                Arc::downgrade(&self.inner),
                auto_shrink_receiver,
            )).abort_on_drop();

            #[cfg(feature = "e2e-encryption")]
            let redecryptor = {
                let receiver = self
                    .inner
                    .redecryption_channels
                    .decryption_request_receiver
                    .lock()
                    .take()
                    .expect("We should have initialized the channel an subscribing should happen only once");

                redecryptor::Redecryptor::new(&client, Arc::downgrade(&self.inner), receiver, &self.inner.linked_chunk_update_sender)
            };

        let thread_subscriber_task = client
            .task_monitor()
            .spawn_background_task(
                "event_cache::thread_subscriber",
                tasks::thread_subscriber_task(
                    self.inner.client.clone(),
                    self.inner.linked_chunk_update_sender.clone(),
                    self.inner.thread_subscriber_sender.clone(),
                ),
            )
            .abort_on_drop();

        #[cfg(feature = "experimental-search")]
        let search_indexing_task = client
            .task_monitor()
            .spawn_background_task(
                "event_cache::search_indexing",
                tasks::search_indexing_task(
                    self.inner.client.clone(),
                    self.inner.linked_chunk_update_sender.clone(),
                ),
            )
            .abort_on_drop();

            if self.config().experimental_auto_backpagination {
                // Run the deferred initialization of the automatic pagination request sender, that
                // is shared with every room.
                trace!("spawning the automatic paginations API");
                self.inner.automatic_pagination.get_or_init(|| AutomaticPagination::new(Arc::downgrade(&self.inner), task_monitor));
            } else {
                trace!("automatic paginations API is disabled");
            }

            Arc::new(EventCacheDropHandles {
                _listen_updates_task: listen_updates_task,
                _ignore_user_list_update_task: ignore_user_list_update_task,
                _auto_shrink_linked_chunk_task: auto_shrink_linked_chunk_task,
                #[cfg(feature = "e2e-encryption")]
                _redecryptor: redecryptor,
                _thread_subscriber_task: thread_subscriber_task,
                #[cfg(feature = "experimental-search")]
                _search_indexing_task: search_indexing_task,
            })
        });

        Ok(())
    }

    /// For benchmarking purposes only.
    #[doc(hidden)]
    pub async fn handle_room_updates(&self, updates: RoomUpdates) -> Result<()> {
        self.inner.handle_room_updates(updates).await
    }

    /// Check whether [`EventCache::subscribe`] has been called.
    pub fn has_subscribed(&self) -> bool {
        self.inner.drop_handles.get().is_some()
    }

    /// Return a room-specific view over the [`EventCache`].
    pub(crate) async fn for_room(
        &self,
        room_id: &RoomId,
    ) -> Result<(RoomEventCache, Arc<EventCacheDropHandles>)> {
        let Some(drop_handles) = self.inner.drop_handles.get().cloned() else {
            return Err(EventCacheError::NotSubscribedYet);
        };

        let room = self.inner.all_caches_for_room(room_id).await?.room.clone();

        Ok((room, drop_handles))
    }

    /// Cleanly clear all the rooms' event caches.
    ///
    /// This will notify any live observers that the room has been cleared.
    pub async fn clear_all_rooms(&self) -> Result<()> {
        self.inner.clear_all_rooms().await
    }

    /// Subscribe to room _generic_ updates.
    ///
    /// If one wants to listen what has changed in a specific room, the
    /// [`RoomEventCache::subscribe`] is recommended. However, the
    /// [`RoomEventCacheSubscriber`] type triggers side-effects.
    ///
    /// If one wants to get a high-overview, generic, updates for rooms, and
    /// without side-effects, this method is recommended. Also, dropping the
    /// receiver of this channel will not trigger any side-effect.
    pub fn subscribe_to_room_generic_updates(&self) -> Receiver<RoomEventCacheGenericUpdate> {
        self.inner.generic_update_sender.subscribe()
    }

    /// Returns a reference to the [`AutomaticPagination`] API, if enabled at
    /// construction with the
    /// [`EventCacheConfig::experimental_auto_backpagination`] flag.
    pub fn automatic_pagination(&self) -> Option<AutomaticPagination> {
        self.inner.automatic_pagination.get().cloned()
    }
}

/// Global configuration for the [`EventCache`], applied to every single room.
#[derive(Clone, Copy, Debug)]
pub struct EventCacheConfig {
    /// Maximum number of concurrent /event requests when loading pinned events.
    pub max_pinned_events_concurrent_requests: usize,

    /// Maximum number of pinned events to load, for any room.
    pub max_pinned_events_to_load: usize,

    /// Whether to automatically backpaginate a room under certain conditions.
    ///
    /// Off by default.
    pub experimental_auto_backpagination: bool,

    /// The maximum number of allowed room paginations, for a given room, that
    /// can be executed in the automatic paginations task.
    ///
    /// After that number of paginations, the task will stop executing
    /// paginations for that room *in the background* (user-requested
    /// paginations will still be executed, of course).
    ///
    /// Defaults to [`EventCacheConfig::DEFAULT_ROOM_PAGINATION_CREDITS`].
    pub room_pagination_per_room_credit: usize,

    /// The number of messages to paginate in a single batch, when executing an
    /// automatic pagination request.
    ///
    /// Defaults to [`EventCacheConfig::DEFAULT_ROOM_PAGINATION_BATCH_SIZE`].
    pub room_pagination_batch_size: u16,
}

impl EventCacheConfig {
    /// The default maximum number of pinned events to load.
    pub const DEFAULT_MAX_EVENTS_TO_LOAD: usize = 128;

    /// The default maximum number of concurrent requests to perform when
    /// loading the pinned events.
    pub const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 8;

    /// The default number of credits to give to a room for automatic
    /// paginations (see also
    /// [`EventCacheConfig::room_pagination_per_room_credit`]).
    pub const DEFAULT_ROOM_PAGINATION_CREDITS: usize = 20;

    /// The default number of messages to paginate in a single batch, when
    /// executing an automatic pagination request (see also
    /// [`EventCacheConfig::room_pagination_batch_size`]).
    pub const DEFAULT_ROOM_PAGINATION_BATCH_SIZE: u16 = 30;
}

impl Default for EventCacheConfig {
    fn default() -> Self {
        Self {
            max_pinned_events_concurrent_requests: Self::DEFAULT_MAX_CONCURRENT_REQUESTS,
            max_pinned_events_to_load: Self::DEFAULT_MAX_EVENTS_TO_LOAD,
            room_pagination_per_room_credit: Self::DEFAULT_ROOM_PAGINATION_CREDITS,
            room_pagination_batch_size: Self::DEFAULT_ROOM_PAGINATION_BATCH_SIZE,
            experimental_auto_backpagination: false,
        }
    }
}

type CachesByRoom = HashMap<OwnedRoomId, Caches>;

struct EventCacheInner {
    /// A weak reference to the inner client, useful when trying to get a handle
    /// on the owning client.
    client: WeakClient,

    /// Global configuration for the event cache.
    config: StdRwLock<EventCacheConfig>,

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
    //
    // It's behind an `Arc` to get owned locks.
    by_room: Arc<RwLock<CachesByRoom>>,

    /// Handles to keep alive the task listening to updates.
    drop_handles: OnceLock<Arc<EventCacheDropHandles>>,

    /// A sender for notifications that a room *may* need to be auto-shrunk.
    ///
    /// Needs to live here, so it may be passed to each [`RoomEventCache`]
    /// instance.
    ///
    /// It's a `OnceLock` because its initialization is deferred to
    /// [`EventCache::subscribe`].
    ///
    /// See doc comment of [`EventCache::auto_shrink_linked_chunk_task`].
    auto_shrink_sender: OnceLock<mpsc::Sender<AutoShrinkChannelPayload>>,

    /// A sender for room generic update.
    ///
    /// See doc comment of [`RoomEventCacheGenericUpdate`] and
    /// [`EventCache::subscribe_to_room_generic_updates`].
    generic_update_sender: Sender<RoomEventCacheGenericUpdate>,

    /// A sender for a persisted linked chunk update.
    ///
    /// This is used to notify that some linked chunk has persisted some updates
    /// to a store, during sync or a back-pagination of *any* linked chunk.
    /// This can be used by observers to look for new events.
    ///
    /// See doc comment of [`RoomEventCacheLinkedChunkUpdate`].
    linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,

    /// A test helper receiver that will be emitted every time the thread
    /// subscriber task subscribed to a new thread.
    ///
    /// This is helpful for tests to coordinate that a new thread subscription
    /// has been sent or not.
    thread_subscriber_sender: Sender<()>,

    #[cfg(feature = "e2e-encryption")]
    redecryption_channels: redecryptor::RedecryptorChannels,

    /// State for the automatic pagination mechanism.
    ///
    /// Depends on the [`EventCacheConfig::experimental_auto_backpagination`]
    /// flag to be set at subscription time.
    automatic_pagination: OnceLock<AutomaticPagination>,
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
        //
        // 1. the lock for `by_room` is usually held only for a short while, and
        //    independently of the other two kinds.
        // 2. the state may acquire the store cross-process lock internally, but only
        //    while the state's methods are called (so it's always transient). As a
        //    result, as soon as we've acquired the state locks, the store lock ought to
        //    be free.
        // 3. The store lock is held explicitly only in a small scoped area below.
        // 4. Then the store lock will be held internally when calling `reset_all()`,
        //    but at this point it's only held for a short while each time, so rooms
        //    will take turn to acquire it.

        let mut all_caches = self.by_room.write().await;

        // Prepare to reset all the caches: it ensures nobody is accessing or mutating
        // them.
        let resets =
            try_join_all(all_caches.values_mut().map(|caches| caches.prepare_to_reset())).await?;

        // Clear the storage for all the rooms, using the storage facility.
        let store_guard = match self.store.lock().await? {
            EventCacheStoreLockState::Clean(store_guard) => store_guard,
            EventCacheStoreLockState::Dirty(store_guard) => store_guard,
        };
        store_guard.clear_all_linked_chunks().await?;

        // At this point, all the in-memory linked chunks are desynchronized from their
        // storages. Resynchronize them manually by resetting them.
        try_join_all(resets.into_iter().map(|reset_cache| reset_cache.reset_all())).await?;

        Ok(())
    }

    /// Handles a single set of room updates at once.
    #[instrument(skip(self, updates))]
    async fn handle_room_updates(&self, updates: RoomUpdates) -> Result<()> {
        // First, take the lock that indicates we're processing updates, to avoid
        // handling multiple updates concurrently.
        let _lock = {
            let _timer = timer!("Taking the `multiple_room_updates_lock`");
            self.multiple_room_updates_lock.lock().await
        };

        // NOTE: We tried to make this concurrent at some point, but it turned out to be
        // a performance regression, even for large sync updates. Lacking time
        // to investigate, this code remains sequential for now. See also
        // https://github.com/matrix-org/matrix-rust-sdk/pull/5426.

        // Left rooms.
        for (room_id, left_room_update) in updates.left {
            let Ok(caches) = self.all_caches_for_room(&room_id).await else {
                error!(?room_id, "Room must exist");
                continue;
            };

            if let Err(err) = caches.handle_left_room_update(left_room_update).await {
                // Non-fatal error, try to continue to the next room.
                error!("handling left room update: {err}");
            }
        }

        // Joined rooms.
        for (room_id, joined_room_update) in updates.joined {
            trace!(?room_id, "Handling a `JoinedRoomUpdate`");

            let Ok(caches) = self.all_caches_for_room(&room_id).await else {
                error!(?room_id, "Room must exist");
                continue;
            };

            if let Err(err) = caches.handle_joined_room_update(joined_room_update).await {
                // Non-fatal error, try to continue to the next room.
                error!(%room_id, "handling joined room update: {err}");
            }
        }

        // Invited rooms.
        // TODO: we don't anything with `updates.invite` at this point.

        Ok(())
    }

    /// Return all the event caches associated to a specific room.
    async fn all_caches_for_room(
        &self,
        room_id: &RoomId,
    ) -> Result<OwnedRwLockReadGuard<CachesByRoom, Caches>> {
        // Fast path: the entry exists; let's acquire a read lock, it's cheaper than a
        // write lock.
        match OwnedRwLockReadGuard::try_map(self.by_room.clone().read_owned().await, |by_room| {
            by_room.get(room_id)
        }) {
            Ok(caches) => Ok(caches),

            Err(by_room_guard) => {
                // Slow-path: the entry doesn't exist; let's acquire a write lock.
                drop(by_room_guard);
                let by_room_guard = self.by_room.clone().write_owned().await;

                // In the meanwhile, some other caller might have obtained write access and done
                // the same, so check for existence again.
                let mut by_room_guard =
                    match OwnedRwLockWriteGuard::try_downgrade_map(by_room_guard, |by_room| {
                        by_room.get(room_id)
                    }) {
                        Ok(caches) => return Ok(caches),
                        Err(by_room_guard) => by_room_guard,
                    };

                let caches = Caches::new(
                    &self.client,
                    room_id,
                    self.generic_update_sender.clone(),
                    self.linked_chunk_update_sender.clone(),
                    // SAFETY: we must have subscribed before reaching this code, otherwise
                    // something is very wrong.
                    self.auto_shrink_sender.get().cloned().expect(
                        "we must have called `EventCache::subscribe()` before calling here.",
                    ),
                    self.store.clone(),
                    self.automatic_pagination.get().cloned(),
                )
                .await?;

                by_room_guard.insert(room_id.to_owned(), caches);

                Ok(OwnedRwLockWriteGuard::try_downgrade_map(by_room_guard, |by_room| {
                    by_room.get(room_id)
                })
                .expect("`Caches` has just been inserted"))
            }
        }
    }
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
    use std::{ops::Not, sync::Arc, time::Duration};

    use assert_matches::assert_matches;
    use futures_util::FutureExt as _;
    use matrix_sdk_base::{
        RoomState,
        linked_chunk::{ChunkIdentifier, LinkedChunkId, Position, Update},
        sync::{JoinedRoomUpdate, RoomUpdates, Timeline},
    };
    use matrix_sdk_test::{
        JoinedRoomBuilder, SyncResponseBuilder, async_test, event_factory::EventFactory,
    };
    use ruma::{event_id, room_id, user_id};
    use tokio::time::sleep;

    use super::{EventCacheError, RoomEventCacheGenericUpdate};
    use crate::test_utils::{
        assert_event_matches_msg, client::MockClientBuilder, logged_in_client,
    };

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
    async fn test_get_event_by_id() {
        let client = logged_in_client(None).await;
        let room_id1 = room_id!("!galette:saucisse.bzh");
        let room_id2 = room_id!("!crepe:saucisse.bzh");

        client.base_client().get_or_create_room(room_id1, RoomState::Joined);
        client.base_client().get_or_create_room(room_id2, RoomState::Joined);

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
        let room1 = client.get_room(room_id1).unwrap();

        let (room_event_cache, _drop_handles) = room1.event_cache().await.unwrap();

        let found1 = room_event_cache.find_event(eid1).await.unwrap().unwrap();
        assert_event_matches_msg(&found1, "hey");

        let found2 = room_event_cache.find_event(eid2).await.unwrap().unwrap();
        assert_event_matches_msg(&found2, "you");

        // Retrieving the event with id3 from the room which doesn't contain it will
        // fail…
        assert!(room_event_cache.find_event(eid3).await.unwrap().is_none());
    }

    #[async_test]
    async fn test_save_event() {
        let client = logged_in_client(None).await;
        let room_id = room_id!("!galette:saucisse.bzh");

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let f = EventFactory::new().room(room_id).sender(user_id!("@ben:saucisse.bzh"));
        let event_id = event_id!("$1");

        client.base_client().get_or_create_room(room_id, RoomState::Joined);
        let room = client.get_room(room_id).unwrap();

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();
        room_event_cache.save_events([f.text_msg("hey there").event_id(event_id).into()]).await;

        // Retrieving the event at the room-wide cache works.
        assert!(room_event_cache.find_event(event_id).await.unwrap().is_some());
    }

    #[async_test]
    async fn test_generic_update_when_loading_rooms() {
        // Create 2 rooms. One of them has data in the event cache storage.
        let user = user_id!("@mnt_io:matrix.org");
        let client = logged_in_client(None).await;
        let room_id_0 = room_id!("!raclette:patate.ch");
        let room_id_1 = room_id!("!fondue:patate.ch");

        let event_factory = EventFactory::new().room(room_id_0).sender(user);

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id_0, RoomState::Joined);
        client.base_client().get_or_create_room(room_id_1, RoomState::Joined);

        client
            .event_cache_store()
            .lock()
            .await
            .expect("Could not acquire the event cache lock")
            .as_clean()
            .expect("Could not acquire a clean event cache lock")
            .handle_linked_chunk_updates(
                LinkedChunkId::Room(room_id_0),
                vec![
                    // Non-empty items chunk.
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(0),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(0), 0),
                        items: vec![
                            event_factory
                                .text_msg("hello")
                                .sender(user)
                                .event_id(event_id!("$ev0"))
                                .into_event(),
                        ],
                    },
                ],
            )
            .await
            .unwrap();

        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();

        // Room 0 has initial data, so it must trigger a generic update.
        {
            let _room_event_cache = event_cache.for_room(room_id_0).await.unwrap();

            assert_matches!(
                generic_stream.recv().await,
                Ok(RoomEventCacheGenericUpdate { room_id }) => {
                    assert_eq!(room_id, room_id_0);
                }
            );
        }

        // Room 1 has NO initial data, so nothing should happen.
        {
            let _room_event_cache = event_cache.for_room(room_id_1).await.unwrap();

            assert!(generic_stream.recv().now_or_never().is_none());
        }
    }

    #[async_test]
    async fn test_generic_update_when_paginating_room() {
        // Create 1 room, with 4 chunks in the event cache storage.
        let user = user_id!("@mnt_io:matrix.org");
        let client = logged_in_client(None).await;
        let room_id = room_id!("!raclette:patate.ch");

        let event_factory = EventFactory::new().room(room_id).sender(user);

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        client.base_client().get_or_create_room(room_id, RoomState::Joined);

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
                    // Empty chunk.
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(0),
                        next: None,
                    },
                    // Empty chunk.
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(0)),
                        new: ChunkIdentifier::new(1),
                        next: None,
                    },
                    // Non-empty items chunk.
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(1)),
                        new: ChunkIdentifier::new(2),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(2), 0),
                        items: vec![
                            event_factory
                                .text_msg("hello")
                                .sender(user)
                                .event_id(event_id!("$ev0"))
                                .into_event(),
                        ],
                    },
                    // Non-empty items chunk.
                    Update::NewItemsChunk {
                        previous: Some(ChunkIdentifier::new(2)),
                        new: ChunkIdentifier::new(3),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(3), 0),
                        items: vec![
                            event_factory
                                .text_msg("world")
                                .sender(user)
                                .event_id(event_id!("$ev1"))
                                .into_event(),
                        ],
                    },
                ],
            )
            .await
            .unwrap();

        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();

        // Room is initialised, it gets one event in the timeline.
        let (room_event_cache, _) = event_cache.for_room(room_id).await.unwrap();

        assert_matches!(
            generic_stream.recv().await,
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) => {
                assert_eq!(room_id, expected_room_id);
            }
        );

        let pagination = room_event_cache.pagination();

        // Paginate, it gets one new event in the timeline.
        let pagination_outcome = pagination.run_backwards_once(1).await.unwrap();

        assert_eq!(pagination_outcome.events.len(), 1);
        assert!(pagination_outcome.reached_start.not());
        assert_matches!(
            generic_stream.recv().await,
            Ok(RoomEventCacheGenericUpdate { room_id: expected_room_id }) => {
                assert_eq!(room_id, expected_room_id);
            }
        );

        // Paginate, it gets zero new event in the timeline.
        let pagination_outcome = pagination.run_backwards_once(1).await.unwrap();

        assert!(pagination_outcome.events.is_empty());
        assert!(pagination_outcome.reached_start.not());
        assert!(generic_stream.recv().now_or_never().is_none());

        // Paginate once more. Just checking our scenario is correct.
        let pagination_outcome = pagination.run_backwards_once(1).await.unwrap();

        assert!(pagination_outcome.reached_start);
        assert!(generic_stream.recv().now_or_never().is_none());
    }

    #[async_test]
    async fn test_for_room_when_room_is_not_found() {
        let client = logged_in_client(None).await;
        let room_id = room_id!("!raclette:patate.ch");

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        // Room doesn't exist. It returns an error.
        assert_matches!(
            event_cache.for_room(room_id).await,
            Err(EventCacheError::RoomNotFound { room_id: not_found_room_id }) => {
                assert_eq!(room_id, not_found_room_id);
            }
        );

        // Now create the room.
        client.base_client().get_or_create_room(room_id, RoomState::Joined);

        // Room exists. Everything fine.
        assert!(event_cache.for_room(room_id).await.is_ok());
    }

    /// Test that the event cache does not create reference cycles or tasks that
    /// retain its reference indefinitely, preventing it from being deallocated.
    #[cfg(not(target_family = "wasm"))]
    #[async_test]
    async fn test_no_refcycle_event_cache_tasks() {
        let client = MockClientBuilder::new(None).build().await;

        // Wait for the init tasks to die.
        sleep(Duration::from_secs(1)).await;

        let event_cache_weak = Arc::downgrade(&client.event_cache().inner);
        assert_eq!(event_cache_weak.strong_count(), 1);

        {
            let room_id = room_id!("!room:example.org");

            // Have the client know the room.
            let response = SyncResponseBuilder::default()
                .add_joined_room(JoinedRoomBuilder::new(room_id))
                .build_sync_response();
            client.inner.base_client.receive_sync_response(response).await.unwrap();

            client.event_cache().subscribe().unwrap();

            let (_room_event_cache, _drop_handles) =
                client.get_room(room_id).unwrap().event_cache().await.unwrap();
        }

        drop(client);

        // Give a bit of time for background tasks to die.
        sleep(Duration::from_secs(1)).await;

        // No strong counts should exist now that the Client has been dropped.
        assert_eq!(
            event_cache_weak.strong_count(),
            0,
            "Too many strong references to the event cache {}",
            event_cache_weak.strong_count()
        );
    }
}
