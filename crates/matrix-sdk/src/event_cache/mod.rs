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
    collections::{BTreeMap, HashMap},
    fmt,
    sync::{Arc, OnceLock, Weak},
};

use eyeball::{SharedObservable, Subscriber};
use eyeball_im::VectorDiff;
use futures_util::future::{join_all, try_join_all};
use matrix_sdk_base::{
    ThreadingSupport,
    cross_process_lock::CrossProcessLockError,
    deserialized_responses::{AmbiguityChange, TimelineEvent},
    event_cache::{
        Gap,
        store::{EventCacheStoreError, EventCacheStoreLock, EventCacheStoreLockState},
    },
    executor::AbortOnDrop,
    linked_chunk::{self, OwnedLinkedChunkId, lazy_loader::LazyLoaderError},
    serde_helpers::extract_thread_root_from_content,
    sync::RoomUpdates,
    timer,
};
use matrix_sdk_common::executor::{JoinHandle, spawn};
use ruma::{
    OwnedEventId, OwnedRoomId, OwnedTransactionId, RoomId, events::AnySyncEphemeralRoomEvent,
    serde::Raw,
};
use tokio::{
    select,
    sync::{
        Mutex, RwLock,
        broadcast::{Receiver, Sender, channel, error::RecvError},
        mpsc,
    },
};
use tracing::{Instrument as _, Span, debug, error, info, info_span, instrument, trace, warn};

use crate::{
    Client,
    client::WeakClient,
    event_cache::room::RoomEventCacheStateLock,
    send_queue::{LocalEchoContent, RoomSendQueueUpdate, SendQueueUpdate},
};

mod deduplicator;
mod pagination;
#[cfg(feature = "e2e-encryption")]
mod redecryptor;
mod room;

pub use pagination::{RoomPagination, RoomPaginationStatus};
#[cfg(feature = "e2e-encryption")]
pub use redecryptor::{DecryptionRetryRequest, RedecryptorReport};
pub use room::{RoomEventCache, RoomEventCacheSubscriber, ThreadEventCacheUpdate};

/// An error observed in the [`EventCache`].
#[derive(thiserror::Error, Debug)]
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

    /// An error has been observed while back-paginating.
    #[error(transparent)]
    BackpaginationError(Box<crate::Error>),

    /// Back-pagination was already happening in a given room, where we tried to
    /// back-paginate again.
    #[error("We were already back-paginating.")]
    AlreadyBackpaginating,

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
    listen_updates_task: JoinHandle<()>,

    /// Task that listens to updates to the user's ignored list.
    ignore_user_list_update_task: JoinHandle<()>,

    /// The task used to automatically shrink the linked chunks.
    auto_shrink_linked_chunk_task: JoinHandle<()>,

    /// The task used to automatically redecrypt UTDs.
    #[cfg(feature = "e2e-encryption")]
    _redecryptor: redecryptor::Redecryptor,
}

impl fmt::Debug for EventCacheDropHandles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
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

impl fmt::Debug for EventCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventCache").finish_non_exhaustive()
    }
}

impl EventCache {
    /// Create a new [`EventCache`] for the given client.
    pub(crate) fn new(client: WeakClient, event_cache_store: EventCacheStoreLock) -> Self {
        let (generic_update_sender, _) = channel(128);
        let (linked_chunk_update_sender, _) = channel(128);

        let (thread_subscriber_sender, thread_subscriber_receiver) = channel(128);
        let thread_subscriber_task = AbortOnDrop::new(spawn(Self::thread_subscriber_task(
            client.clone(),
            linked_chunk_update_sender.clone(),
            thread_subscriber_sender,
        )));

        #[cfg(feature = "experimental-search")]
        let search_indexing_task = AbortOnDrop::new(spawn(Self::search_indexing_task(
            client.clone(),
            linked_chunk_update_sender.clone(),
        )));

        #[cfg(feature = "e2e-encryption")]
        let redecryption_channels = redecryptor::RedecryptorChannels::new();

        Self {
            inner: Arc::new(EventCacheInner {
                client,
                store: event_cache_store,
                multiple_room_updates_lock: Default::default(),
                by_room: Default::default(),
                drop_handles: Default::default(),
                auto_shrink_sender: Default::default(),
                generic_update_sender,
                linked_chunk_update_sender,
                _thread_subscriber_task: thread_subscriber_task,
                #[cfg(feature = "experimental-search")]
                _search_indexing_task: search_indexing_task,
                #[cfg(feature = "e2e-encryption")]
                redecryption_channels,
                thread_subscriber_receiver,
            }),
        }
    }

    /// Subscribes to updates that a thread subscription has been sent.
    ///
    /// For testing purposes only.
    #[doc(hidden)]
    pub fn subscribe_thread_subscriber_updates(&self) -> Receiver<()> {
        self.inner.thread_subscriber_receiver.resubscribe()
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

            let (auto_shrink_sender, auto_shrink_receiver) = mpsc::channel(32);

            // Force-initialize the sender in the [`RoomEventCacheInner`].
            self.inner.auto_shrink_sender.get_or_init(|| auto_shrink_sender);

            let auto_shrink_linked_chunk_task = spawn(Self::auto_shrink_linked_chunk_task(
                Arc::downgrade(&self.inner),
                auto_shrink_receiver,
            ));

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


            Arc::new(EventCacheDropHandles {
                listen_updates_task,
                ignore_user_list_update_task,
                auto_shrink_linked_chunk_task,
                #[cfg(feature = "e2e-encryption")]
                _redecryptor: redecryptor,
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

    /// For benchmarking purposes only.
    #[doc(hidden)]
    pub async fn handle_room_updates(&self, updates: RoomUpdates) -> Result<()> {
        self.inner.handle_room_updates(updates).await
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
                    trace!("Receiving `RoomUpdates`");

                    if let Err(err) = inner.handle_room_updates(updates).await {
                        match err {
                            EventCacheError::ClientDropped => {
                                // The client has dropped, exit the listen task.
                                info!(
                                    "Closing the event cache global listen task because client dropped"
                                );
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
    ///   increment the active number of subscribers to that room, aka
    ///   [`RoomEventCacheState::subscriber_count`].
    /// - When that subscriber is dropped, it will decrement that count; and
    ///   notify the task below if it reached 0.
    /// - The task spawned here, owned by the [`EventCacheInner`], will listen
    ///   to such notifications that a room may be shrunk. It will attempt an
    ///   auto-shrink, by letting the inner state decide whether this is a good
    ///   time to do so (new subscribers might have spawned in the meanwhile).
    #[instrument(skip_all)]
    async fn auto_shrink_linked_chunk_task(
        inner: Weak<EventCacheInner>,
        mut rx: mpsc::Receiver<AutoShrinkChannelPayload>,
    ) {
        while let Some(room_id) = rx.recv().await {
            trace!(for_room = %room_id, "received notification to shrink");

            let Some(inner) = inner.upgrade() else {
                return;
            };

            let room = match inner.for_room(&room_id).await {
                Ok(room) => room,
                Err(err) => {
                    warn!(for_room = %room_id, "Failed to get the `RoomEventCache`: {err}");
                    continue;
                }
            };

            trace!("waiting for state lock…");
            let mut state = match room.inner.state.write().await {
                Ok(state) => state,
                Err(err) => {
                    warn!(for_room = %room_id, "Failed to get the `RoomEventCacheStateLock`: {err}");
                    continue;
                }
            };

            match state.auto_shrink_if_no_subscribers().await {
                Ok(diffs) => {
                    if let Some(diffs) = diffs {
                        // Hey, fun stuff: we shrunk the linked chunk, so there shouldn't be any
                        // subscribers, right? RIGHT? Especially because the state is guarded behind
                        // a lock.
                        //
                        // However, better safe than sorry, and it's cheap to send an update here,
                        // so let's do it!
                        if !diffs.is_empty() {
                            let _ = room.inner.update_sender.send(
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

        info!("Auto-shrink linked chunk task has been closed, exiting");
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

        let room = self.inner.for_room(room_id).await?;

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

    /// React to a given linked chunk update by subscribing the user to a
    /// thread, if needs be (when the user got mentioned in a thread reply, for
    /// a thread they were not subscribed to).
    ///
    /// Returns a boolean indicating whether the task should keep on running or
    /// not.
    #[instrument(skip(client, thread_subscriber_sender))]
    async fn handle_thread_subscriber_linked_chunk_update(
        client: &WeakClient,
        thread_subscriber_sender: &Sender<()>,
        up: RoomEventCacheLinkedChunkUpdate,
    ) -> bool {
        let Some(client) = client.get() else {
            // Client shutting down.
            debug!("Client is shutting down, exiting thread subscriber task");
            return false;
        };

        let OwnedLinkedChunkId::Thread(room_id, thread_root) = &up.linked_chunk_id else {
            trace!("received an update for a non-thread linked chunk, ignoring");
            return true;
        };

        let Some(room) = client.get_room(room_id) else {
            warn!(%room_id, "unknown room");
            return true;
        };

        let thread_root = thread_root.clone();

        let mut new_events = up.events().peekable();

        if new_events.peek().is_none() {
            // No new events, nothing to do.
            return true;
        }

        // This `PushContext` is going to be used to compute whether an in-thread event
        // would trigger a mention.
        //
        // Of course, we're not interested in an in-thread event causing a mention,
        // because it's part of a thread we've subscribed to. So the
        // `PushContext` must not include the check for thread subscriptions (otherwise
        // it would be impossible to subscribe to new threads).

        let with_thread_subscriptions = false;

        let Some(push_context) = room
            .push_context_internal(with_thread_subscriptions)
            .await
            .inspect_err(|err| {
                warn!("Failed to get push context for threads: {err}");
            })
            .ok()
            .flatten()
        else {
            warn!("Missing push context for thread subscriptions.");
            return true;
        };

        let mut subscribe_up_to = None;

        // Find if there's an event that would trigger a mention for the current
        // user, iterating from the end of the new events towards the oldest, so we can
        // find the most recent event to subscribe to.
        for ev in new_events.rev() {
            if push_context
                .for_event(ev.raw())
                .await
                .into_iter()
                .any(|action| action.should_notify())
            {
                let Some(event_id) = ev.event_id() else {
                    // Shouldn't happen.
                    continue;
                };
                subscribe_up_to = Some(event_id);
                break;
            }
        }

        // And if we've found such a mention, subscribe to the thread up to this
        // event.
        if let Some(event_id) = subscribe_up_to {
            trace!(thread = %thread_root, up_to = %event_id, "found a new thread to subscribe to");
            if let Err(err) = room.subscribe_thread_if_needed(&thread_root, Some(event_id)).await {
                warn!(%err, "Failed to subscribe to thread");
            } else {
                let _ = thread_subscriber_sender.send(());
            }
        }

        true
    }

    /// React to a given send queue update by subscribing the user to a
    /// thread, if needs be (when the user sent an event in a thread they were
    /// not subscribed to).
    ///
    /// Returns a boolean indicating whether the task should keep on running or
    /// not.
    #[instrument(skip(client, thread_subscriber_sender))]
    async fn handle_thread_subscriber_send_queue_update(
        client: &WeakClient,
        thread_subscriber_sender: &Sender<()>,
        events_being_sent: &mut HashMap<OwnedTransactionId, OwnedEventId>,
        up: SendQueueUpdate,
    ) -> bool {
        let Some(client) = client.get() else {
            // Client shutting down.
            debug!("Client is shutting down, exiting thread subscriber task");
            return false;
        };

        let room_id = up.room_id;
        let Some(room) = client.get_room(&room_id) else {
            warn!(%room_id, "unknown room");
            return true;
        };

        let (thread_root, subscribe_up_to) = match up.update {
            RoomSendQueueUpdate::NewLocalEvent(local_echo) => {
                match local_echo.content {
                    LocalEchoContent::Event { serialized_event, .. } => {
                        if let Some(thread_root) =
                            extract_thread_root_from_content(serialized_event.into_raw().0)
                        {
                            events_being_sent.insert(local_echo.transaction_id, thread_root);
                        }
                    }
                    LocalEchoContent::React { .. } => {
                        // Nothing to do, reactions don't count as a thread
                        // subscription.
                    }
                }
                return true;
            }

            RoomSendQueueUpdate::CancelledLocalEvent { transaction_id } => {
                events_being_sent.remove(&transaction_id);
                return true;
            }

            RoomSendQueueUpdate::ReplacedLocalEvent { transaction_id, new_content } => {
                if let Some(thread_root) =
                    extract_thread_root_from_content(new_content.into_raw().0)
                {
                    events_being_sent.insert(transaction_id, thread_root);
                } else {
                    // It could be that the event isn't part of a thread anymore; handle that by
                    // removing the pending transaction id.
                    events_being_sent.remove(&transaction_id);
                }
                return true;
            }

            RoomSendQueueUpdate::SentEvent { transaction_id, event_id } => {
                if let Some(thread_root) = events_being_sent.remove(&transaction_id) {
                    (thread_root, event_id)
                } else {
                    // We don't know about the event that has been sent, so ignore it.
                    trace!(%transaction_id, "received a sent event that we didn't know about, ignoring");
                    return true;
                }
            }

            RoomSendQueueUpdate::SendError { .. }
            | RoomSendQueueUpdate::RetryEvent { .. }
            | RoomSendQueueUpdate::MediaUpload { .. } => {
                // Nothing to do for these bad boys.
                return true;
            }
        };

        // And if we've found such a mention, subscribe to the thread up to this event.
        trace!(thread = %thread_root, up_to = %subscribe_up_to, "found a new thread to subscribe to");
        if let Err(err) = room.subscribe_thread_if_needed(&thread_root, Some(subscribe_up_to)).await
        {
            warn!(%err, "Failed to subscribe to thread");
        } else {
            let _ = thread_subscriber_sender.send(());
        }

        true
    }

    #[instrument(skip_all)]
    async fn thread_subscriber_task(
        client: WeakClient,
        linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,
        thread_subscriber_sender: Sender<()>,
    ) {
        let mut send_q_rx = if let Some(client) = client.get() {
            if !client.enabled_thread_subscriptions() {
                trace!("Thread subscriptions are not enabled, not spawning thread subscriber task");
                return;
            }

            client.send_queue().subscribe()
        } else {
            trace!("Client is shutting down, not spawning thread subscriber task");
            return;
        };

        let mut linked_chunk_rx = linked_chunk_update_sender.subscribe();

        // A mapping of local echoes (events being sent), to their thread root, if
        // they're in an in-thread reply.
        //
        // Entirely managed by `handle_thread_subscriber_send_queue_update`.
        let mut events_being_sent = HashMap::new();

        loop {
            select! {
                res = send_q_rx.recv() => {
                    match res {
                        Ok(up) => {
                            if !Self::handle_thread_subscriber_send_queue_update(&client, &thread_subscriber_sender, &mut events_being_sent, up).await {
                                break;
                            }
                        }
                        Err(RecvError::Closed) => {
                            debug!("Linked chunk update channel has been closed, exiting thread subscriber task");
                            break;
                        }
                        Err(RecvError::Lagged(num_skipped)) => {
                            warn!(num_skipped, "Lagged behind linked chunk updates");
                        }
                    }
                }

                res = linked_chunk_rx.recv() => {
                    match res {
                        Ok(up) => {
                            if !Self::handle_thread_subscriber_linked_chunk_update(&client, &thread_subscriber_sender, up).await {
                                break;
                            }
                        }
                        Err(RecvError::Closed) => {
                            debug!("Linked chunk update channel has been closed, exiting thread subscriber task");
                            break;
                        }
                        Err(RecvError::Lagged(num_skipped)) => {
                            warn!(num_skipped, "Lagged behind linked chunk updates");
                        }
                    }
                }
            }
        }
    }

    /// Takes a [`TimelineEvent`] and passes it to the [`RoomIndex`] of the
    /// given room which will add/remove/edit an event in the index based on
    /// the event type.
    #[cfg(feature = "experimental-search")]
    #[instrument(skip_all)]
    async fn search_indexing_task(
        client: WeakClient,
        linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,
    ) {
        let mut linked_chunk_update_receiver = linked_chunk_update_sender.subscribe();

        loop {
            match linked_chunk_update_receiver.recv().await {
                Ok(room_ec_lc_update) => {
                    let OwnedLinkedChunkId::Room(room_id) =
                        room_ec_lc_update.linked_chunk_id.clone()
                    else {
                        trace!("Received non-room updates, ignoring.");
                        continue;
                    };

                    let mut timeline_events = room_ec_lc_update.events().peekable();

                    if timeline_events.peek().is_none() {
                        continue;
                    }

                    let Some(client) = client.get() else {
                        trace!("Client is shutting down, not spawning thread subscriber task");
                        return;
                    };

                    let maybe_room_cache = client.event_cache().for_room(&room_id).await;
                    let Ok((room_cache, _drop_handles)) = maybe_room_cache else {
                        warn!(for_room = %room_id, "Failed to get RoomEventCache: {maybe_room_cache:?}");
                        continue;
                    };

                    let maybe_room = client.get_room(&room_id);
                    let Some(room) = maybe_room else {
                        warn!(get_room = %room_id, "Failed to get room while indexing: {maybe_room:?}");
                        continue;
                    };
                    let redaction_rules =
                        room.clone_info().room_version_rules_or_default().redaction;

                    let mut search_index_guard = client.search_index().lock().await;

                    if let Err(err) = search_index_guard
                        .bulk_handle_timeline_event(
                            timeline_events,
                            &room_cache,
                            &room_id,
                            &redaction_rules,
                        )
                        .await
                    {
                        error!("Failed to handle events for indexing: {err}")
                    }
                }
                Err(RecvError::Closed) => {
                    debug!(
                        "Linked chunk update channel has been closed, exiting thread subscriber task"
                    );
                    break;
                }
                Err(RecvError::Lagged(num_skipped)) => {
                    warn!(num_skipped, "Lagged behind linked chunk updates");
                }
            }
        }
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
    by_room: RwLock<HashMap<OwnedRoomId, RoomEventCache>>,

    /// Handles to keep alive the task listening to updates.
    drop_handles: OnceLock<Arc<EventCacheDropHandles>>,

    /// A sender for notifications that a room *may* need to be auto-shrunk.
    ///
    /// Needs to live here, so it may be passed to each [`RoomEventCache`]
    /// instance.
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

    /// A background task listening to room and send queue updates, and
    /// automatically subscribing the user to threads when needed, based on
    /// the semantics of MSC4306.
    ///
    /// One important constraint is that there is only one such task per
    /// [`EventCache`], so it does listen to *all* rooms at the same time.
    _thread_subscriber_task: AbortOnDrop<()>,

    /// A background task listening to room updates, and
    /// automatically handling search index operations add/remove/edit
    /// depending on the event type.
    ///
    /// One important constraint is that there is only one such task per
    /// [`EventCache`], so it does listen to *all* rooms at the same time.
    #[cfg(feature = "experimental-search")]
    _search_indexing_task: AbortOnDrop<()>,

    /// A test helper receiver that will be emitted every time the thread
    /// subscriber task subscribed to a new thread.
    ///
    /// This is helpful for tests to coordinate that a new thread subscription
    /// has been sent or not.
    thread_subscriber_receiver: Receiver<()>,

    #[cfg(feature = "e2e-encryption")]
    redecryption_channels: redecryptor::RedecryptorChannels,
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
        let store_guard = match self.store.lock().await? {
            EventCacheStoreLockState::Clean(store_guard) => store_guard,
            EventCacheStoreLockState::Dirty(store_guard) => store_guard,
        };
        store_guard.clear_all_linked_chunks().await?;

        // At this point, all the in-memory linked chunks are desynchronized from the
        // storage. Resynchronize them manually by calling reset(), and
        // propagate updates to observers.
        try_join_all(room_locks.into_iter().map(|(room, state_guard)| async move {
            let mut state_guard = state_guard?;
            let updates_as_vector_diffs = state_guard.reset().await?;

            let _ = room.inner.update_sender.send(RoomEventCacheUpdate::UpdateTimelineEvents {
                diffs: updates_as_vector_diffs,
                origin: EventsOrigin::Cache,
            });

            let _ = room
                .inner
                .generic_update_sender
                .send(RoomEventCacheGenericUpdate { room_id: room.inner.room_id.clone() });

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
        let _lock = {
            let _timer = timer!("Taking the `multiple_room_updates_lock`");
            self.multiple_room_updates_lock.lock().await
        };

        // NOTE: bnjbvr tried to make this concurrent at some point, but it turned out
        // to be a performance regression, even for large sync updates. Lacking
        // time to investigate, this code remains sequential for now. See also
        // https://github.com/matrix-org/matrix-rust-sdk/pull/5426.

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
            trace!(?room_id, "Handling a `JoinedRoomUpdate`");

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

                let Some(client) = self.client.get() else {
                    return Err(EventCacheError::ClientDropped);
                };

                let room = client
                    .get_room(room_id)
                    .ok_or_else(|| EventCacheError::RoomNotFound { room_id: room_id.to_owned() })?;
                let room_version_rules = room.clone_info().room_version_rules_or_default();

                let enabled_thread_support = matches!(
                    client.base_client().threading_support,
                    ThreadingSupport::Enabled { .. }
                );

                let update_sender = Sender::new(32);

                let room_state = RoomEventCacheStateLock::new(
                    room_id.to_owned(),
                    room_version_rules,
                    enabled_thread_support,
                    update_sender.clone(),
                    self.generic_update_sender.clone(),
                    self.linked_chunk_update_sender.clone(),
                    self.store.clone(),
                    pagination_status.clone(),
                )
                .await?;

                let timeline_is_not_empty =
                    room_state.read().await?.room_linked_chunk().revents().next().is_some();

                // SAFETY: we must have subscribed before reaching this code, otherwise
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
                    update_sender,
                    self.generic_update_sender.clone(),
                );

                by_room_guard.insert(room_id.to_owned(), room_event_cache.clone());

                // If at least one event has been loaded, it means there is a timeline. Let's
                // emit a generic update.
                if timeline_is_not_empty {
                    let _ = self
                        .generic_update_sender
                        .send(RoomEventCacheGenericUpdate { room_id: room_id.to_owned() });
                }

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

/// Represents a timeline update of a room. It hides the details of
/// [`RoomEventCacheUpdate`] by being more generic.
///
/// This is used by [`EventCache::subscribe_to_room_generic_updates`]. Please
/// read it to learn more about the motivation behind this type.
#[derive(Clone, Debug)]
pub struct RoomEventCacheGenericUpdate {
    /// The room ID owning the timeline.
    pub room_id: OwnedRoomId,
}

/// An update being triggered when events change in the persisted event cache
/// for any room.
#[derive(Clone, Debug)]
struct RoomEventCacheLinkedChunkUpdate {
    /// The linked chunk affected by the update.
    linked_chunk_id: OwnedLinkedChunkId,

    /// A vector of all the linked chunk updates that happened during this event
    /// cache update.
    updates: Vec<linked_chunk::Update<TimelineEvent, Gap>>,
}

impl RoomEventCacheLinkedChunkUpdate {
    /// Return all the new events propagated by this update, in topological
    /// order.
    pub fn events(self) -> impl DoubleEndedIterator<Item = TimelineEvent> {
        use itertools::Either;
        self.updates.into_iter().flat_map(|update| match update {
            linked_chunk::Update::PushItems { items, .. } => {
                Either::Left(Either::Left(items.into_iter()))
            }
            linked_chunk::Update::ReplaceItem { item, .. } => {
                Either::Left(Either::Right(std::iter::once(item)))
            }
            linked_chunk::Update::RemoveItem { .. }
            | linked_chunk::Update::DetachLastItems { .. }
            | linked_chunk::Update::StartReattachItems
            | linked_chunk::Update::EndReattachItems
            | linked_chunk::Update::NewItemsChunk { .. }
            | linked_chunk::Update::NewGapChunk { .. }
            | linked_chunk::Update::RemoveChunk(..)
            | linked_chunk::Update::Clear => {
                // All these updates don't contain any new event.
                Either::Right(std::iter::empty())
            }
        })
    }
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
    use ruma::{event_id, room_id, serde::Raw, user_id};
    use serde_json::json;
    use tokio::time::sleep;

    use super::{EventCacheError, RoomEventCacheGenericUpdate, RoomEventCacheUpdate};
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
    async fn test_uniq_read_marker() {
        let client = logged_in_client(None).await;
        let room_id = room_id!("!galette:saucisse.bzh");
        client.base_client().get_or_create_room(room_id, RoomState::Joined);

        let event_cache = client.event_cache();

        event_cache.subscribe().unwrap();

        let mut generic_stream = event_cache.subscribe_to_room_generic_updates();
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
            .handle_joined_room_update(JoinedRoomUpdate { account_data, ..Default::default() })
            .await
            .unwrap();

        // … there's only one read marker update.
        assert_matches!(
            stream.recv().await.unwrap(),
            RoomEventCacheUpdate::MoveReadMarkerTo { .. }
        );

        assert!(stream.recv().now_or_never().is_none());

        // None, because an account data doesn't trigger a generic update.
        assert!(generic_stream.recv().now_or_never().is_none());
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
