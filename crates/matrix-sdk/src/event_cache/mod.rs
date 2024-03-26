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
//! - [ ] forward pagination
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
    time::Duration,
};

use matrix_sdk_base::{
    deserialized_responses::{AmbiguityChange, SyncTimelineEvent, TimelineEvent},
    sync::{JoinedRoomUpdate, LeftRoomUpdate, RoomUpdates, Timeline},
};
use matrix_sdk_common::executor::{spawn, JoinHandle};
use ruma::{
    assign,
    events::{AnyRoomAccountDataEvent, AnySyncEphemeralRoomEvent},
    serde::Raw,
    OwnedEventId, OwnedRoomId, RoomId,
};
use tokio::{
    sync::{
        broadcast::{error::RecvError, Receiver, Sender},
        Mutex, Notify, RwLock, RwLockReadGuard, RwLockWriteGuard,
    },
    time::timeout,
};
use tracing::{error, instrument, trace, warn};

use self::{
    linked_chunk::ChunkContent,
    store::{Gap, PaginationToken, RoomEvents},
};
use crate::{client::ClientInner, room::MessagesOptions, Client, Room};

mod linked_chunk;
mod store;

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
    listen_updates_task: JoinHandle<()>,
}

impl Debug for EventCacheDropHandles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventCacheDropHandles").finish_non_exhaustive()
    }
}

impl Drop for EventCacheDropHandles {
    fn drop(&mut self) {
        self.listen_updates_task.abort();
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
            let room_updates_feed = client.subscribe_to_all_room_updates();
            let listen_updates_task =
                spawn(Self::listen_task(self.inner.clone(), room_updates_feed));

            Arc::new(EventCacheDropHandles { listen_updates_task })
        });

        Ok(())
    }

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

                Err(RecvError::Lagged(_)) => {
                    // Forget everything we know; we could have missed events, and we have
                    // no way to reconcile at the moment!
                    // TODO: implement Smart Matching™,
                    inner.by_room.write().await.clear();
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

    /// Returns the oldest back-pagination token, that is, the one closest to
    /// the beginning of the timeline as we know it.
    ///
    /// Optionally, wait at most for the given duration for a back-pagination
    /// token to be returned by a sync.
    pub async fn oldest_backpagination_token(
        &self,
        max_wait: Option<Duration>,
    ) -> Result<Option<PaginationToken>> {
        self.inner.oldest_backpagination_token(max_wait).await
    }

    /// Back-paginate with the given token, if provided.
    ///
    /// If no token has been provided, it will back-paginate from the end of the
    /// room.
    ///
    /// If a token has been provided, but it was unknown to the event cache
    /// (i.e. it's not associated to any gap in the timeline stored by the
    /// event cache), then an error result will be returned.
    pub async fn backpaginate(
        &self,
        batch_size: u16,
        token: Option<PaginationToken>,
    ) -> Result<BackPaginationOutcome> {
        self.inner.backpaginate(batch_size, token).await
    }
}

/// The (non-clonable) details of the `RoomEventCache`.
struct RoomEventCacheInner {
    /// Sender part for subscribers to this room.
    sender: Sender<RoomEventCacheUpdate>,

    /// The Client [`Room`] this event cache pertains to.
    room: Room,

    /// The events of the room.
    events: RwLock<RoomEvents>,

    /// A notifier that we received a new pagination token.
    pagination_token_notifier: Notify,

    /// A lock that ensures we don't run multiple pagination queries at the same
    /// time.
    pagination_lock: Mutex<()>,
}

impl RoomEventCacheInner {
    /// Creates a new cache for a room, and subscribes to room updates, so as
    /// to handle new timeline events.
    fn new(room: Room) -> Self {
        let sender = Sender::new(32);

        Self {
            room,
            events: RwLock::new(RoomEvents::default()),
            sender,
            pagination_lock: Default::default(),
            pagination_token_notifier: Default::default(),
        }
    }

    fn handle_account_data(&self, account_data: Vec<Raw<AnyRoomAccountDataEvent>>) {
        trace!("Handling account data");
        for raw_event in account_data {
            match raw_event.deserialize() {
                Ok(AnyRoomAccountDataEvent::FullyRead(ev)) => {
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

        // Reset the events.
        room_events.reset();

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
                room_events.push_gap(Gap { prev_token: PaginationToken(prev_token.clone()) });
            }

            room_events.push_events(events.clone().into_iter());
        }

        // Now that all events have been added, we can trigger the
        // `pagination_token_notifier`.
        if prev_batch.is_some() {
            self.pagination_token_notifier.notify_one();
        }

        let _ =
            self.sender.send(RoomEventCacheUpdate::Append { events, ephemeral, ambiguity_changes });

        Ok(())
    }

    /// Run a single back-pagination `/messages` request.
    ///
    /// This will only run one request; since a backpagination may need to
    /// continue, it's preferable to use [`Self::backpaginate_until`].
    ///
    /// Returns the number of messages received in this chunk.
    #[instrument(skip(self))]
    async fn backpaginate(
        &self,
        batch_size: u16,
        token: Option<PaginationToken>,
    ) -> Result<BackPaginationOutcome> {
        // Make sure there's at most one back-pagination request.
        let _guard = self.pagination_lock.lock().await;

        // Get messages.
        let messages = self
            .room
            .messages(assign!(MessagesOptions::backward(), {
                from: token.as_ref().map(|token| token.0.clone()),
                limit: batch_size.into()
            }))
            .await
            .map_err(EventCacheError::SdkError)?;

        // Make sure the `RoomEvents` isn't updated while we are saving events from
        // backpagination.
        let mut room_events = self.events.write().await;

        // Check that the `token` exists if any.
        let gap_identifier = if let Some(token) = token.as_ref() {
            let gap_identifier = room_events.chunk_identifier(|chunk| {
                matches!(chunk.content(), ChunkContent::Gap(Gap { ref prev_token }) if prev_token == token)
            });

            // The method has been called with `token` but it doesn't exist in `RoomEvents`,
            // it's an error.
            if gap_identifier.is_none() {
                return Ok(BackPaginationOutcome::UnknownBackpaginationToken);
            }

            gap_identifier
        } else {
            None
        };

        // Would we want to backpaginate again, we'd start from the `end` token as the
        // next `from` token.

        let prev_token =
            messages.end.map(|prev_token| Gap { prev_token: PaginationToken(prev_token) });

        // If this token is missing, then we've reached the end of the timeline.
        let reached_start = prev_token.is_none();

        // Note: The chunk could be empty.
        //
        // If there's any event, they are presented in reverse order (i.e. the first one
        // should be prepended first).
        let events = messages.chunk;

        let sync_events = events
            .iter()
            // Reverse the order of the events as `/messages` has been called with `dir=b`
            // (backward). The `RoomEvents` API expects the first event to be the oldest.
            .rev()
            .cloned()
            .map(SyncTimelineEvent::from);

        // There is a `token`/gap, let's replace it by new events!
        if let Some(gap_identifier) = gap_identifier {
            let new_position = {
                // Replace the gap by new events.
                let new_chunk = room_events
                    .replace_gap_at(sync_events, gap_identifier)
                    // SAFETY: we are sure that `gap_identifier` represents a valid
                    // `ChunkIdentifier` for a `Gap` chunk.
                    .expect("The `gap_identifier` must represent a `Gap`");

                new_chunk.first_position()
            };

            // And insert a new gap if there is any `prev_token`.
            if let Some(prev_token_gap) = prev_token {
                room_events
                    .insert_gap_at(prev_token_gap, new_position)
                    // SAFETY: we are sure that `new_position` represents a valid
                    // `ChunkIdentifier` for an `Item` chunk.
                    .expect("The `new_position` must represent an `Item`");
            }

            trace!("replaced gap with new events from backpagination");

            // TODO: implement smarter reconciliation later
            //let _ = self.sender.send(RoomEventCacheUpdate::Prepend { events });

            Ok(BackPaginationOutcome::Success { events, reached_start })
        } else {
            // There is no `token`/gap identifier. Let's assume we must prepend the new
            // events.
            let first_item_position =
                room_events.events().next().map(|(item_position, _)| item_position);

            match first_item_position {
                // Is there a first item? Insert at this position.
                Some(first_item_position) => {
                    if let Some(prev_token_gap) = prev_token {
                        room_events
                            .insert_gap_at(prev_token_gap, first_item_position)
                            // SAFETY: The `first_item_position` can only be an `Item` chunk, it's
                            // an invariant of `LinkedChunk`. Also, it can only represent a valid
                            // `ChunkIdentifier` as the data structure isn't modified yet.
                            .expect("The `first_item_position` must represent a valid `Item`");
                    }

                    room_events
                        .insert_events_at(sync_events, first_item_position)
                        // SAFETY: The `first_item_position` can only be an `Item` chunk, it's
                        // an invariant of `LinkedChunk`. The chunk it points to has not been
                        // removed.
                        .expect("The `first_item_position` must represent an `Item`");
                }

                // There is no first item. Let's simply push.
                None => {
                    if let Some(prev_token_gap) = prev_token {
                        room_events.push_gap(prev_token_gap);
                    }

                    room_events.push_events(sync_events);
                }
            }

            Ok(BackPaginationOutcome::Success { events, reached_start })
        }
    }

    /// Returns the oldest back-pagination token, that is, the one closest to
    /// the start of the timeline as we know it.
    ///
    /// Optionally, wait at most for the given duration for a back-pagination
    /// token to be returned by a sync.
    async fn oldest_backpagination_token(
        &self,
        max_wait: Option<Duration>,
    ) -> Result<Option<PaginationToken>> {
        // Optimistically try to return the backpagination token immediately.
        fn get_oldest(room_events: RwLockReadGuard<'_, RoomEvents>) -> Option<PaginationToken> {
            room_events.chunks().find_map(|chunk| match chunk.content() {
                ChunkContent::Gap(gap) => Some(gap.prev_token.clone()),
                ChunkContent::Items(..) => None,
            })
        }

        if let Some(token) = get_oldest(self.events.read().await) {
            return Ok(Some(token));
        }

        let Some(max_wait) = max_wait else {
            // We had no token and no time to wait, so… no tokens.
            return Ok(None);
        };

        // Otherwise wait for a notification that we received a token.
        // Timeouts are fine, per this function's contract.
        let _ = timeout(max_wait, self.pagination_token_notifier.notified()).await;

        Ok(get_oldest(self.events.read().await))
    }
}

/// The result of a single back-pagination request.
#[derive(Debug)]
pub enum BackPaginationOutcome {
    /// The back-pagination succeeded, and new events have been found.
    Success {
        /// Did the back-pagination reach the start of the timeline?
        reached_start: bool,

        /// All the events that have been returned in the back-pagination
        /// request.
        ///
        /// Events are presented in reverse order: the first element of the vec,
        /// if present, is the most "recent" event from the chunk (or
        /// technically, the last one in the topological ordering).
        ///
        /// Note: they're not deduplicated (TODO: smart reconciliation).
        events: Vec<TimelineEvent>,
    },

    /// The back-pagination token was unknown to the event cache, and the caller
    /// must retry after obtaining a new back-pagination token.
    UnknownBackpaginationToken,
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
    use matrix_sdk_common::executor::spawn;
    use matrix_sdk_test::{async_test, sync_timeline_event};
    use ruma::room_id;

    use super::{BackPaginationOutcome, EventCacheError};
    use crate::{event_cache::store::PaginationToken, test_utils::logged_in_client};

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

    // Those tests require time to work, and it does not on wasm32.
    #[cfg(not(target_arch = "wasm32"))]
    mod time_tests {
        use std::time::{Duration, Instant};

        use matrix_sdk_base::RoomState;
        use serde_json::json;
        use tokio::time::sleep;
        use wiremock::{
            matchers::{header, method, path_regex, query_param},
            Mock, ResponseTemplate,
        };

        use super::{super::store::Gap, *};
        use crate::test_utils::logged_in_client_with_server;

        #[async_test]
        async fn test_unknown_pagination_token() {
            let (client, server) = logged_in_client_with_server().await;

            let room_id = room_id!("!galette:saucisse.bzh");
            client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);

            client.event_cache().subscribe().unwrap();

            let (room_event_cache, _drop_handles) =
                client.event_cache().for_room(room_id).await.unwrap();
            let room_event_cache = room_event_cache.unwrap();

            // If I try to back-paginate with an unknown back-pagination token,
            let token_name = "unknown";
            let token = PaginationToken(token_name.to_owned());

            // Then I run into an error.
            Mock::given(method("GET"))
                .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
                .and(header("authorization", "Bearer 1234"))
                .and(query_param("from", token_name))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "start": token_name,
                    "chunk": [],
                })))
                .expect(1)
                .mount(&server)
                .await;

            let res = room_event_cache.backpaginate(20, Some(token)).await;
            assert_matches!(res, Ok(BackPaginationOutcome::UnknownBackpaginationToken));

            server.verify().await
        }

        #[async_test]
        async fn test_wait_no_pagination_token() {
            let client = logged_in_client(None).await;
            let room_id = room_id!("!galette:saucisse.bzh");
            client.base_client().get_or_create_room(room_id, RoomState::Joined);

            let event_cache = client.event_cache();

            event_cache.subscribe().unwrap();

            let (room_event_cache, _drop_handlers) = event_cache.for_room(room_id).await.unwrap();
            let room_event_cache = room_event_cache.unwrap();

            // When I only have events in a room,
            {
                let mut room_events = room_event_cache.inner.events.write().await;
                room_events.push_events([sync_timeline_event!({
                    "sender": "b@z.h",
                    "type": "m.room.message",
                    "event_id": "$ida",
                    "origin_server_ts": 12344446,
                    "content": { "body":"yolo", "msgtype": "m.text" },
                })
                .into()]);
            }

            // If I don't wait for the backpagination token,
            let found = room_event_cache.oldest_backpagination_token(None).await.unwrap();
            // Then I don't find it.
            assert!(found.is_none());

            // If I wait for a back-pagination token for 0 seconds,
            let before = Instant::now();
            let found = room_event_cache
                .oldest_backpagination_token(Some(Duration::default()))
                .await
                .unwrap();
            let waited = before.elapsed();
            // then I don't get any,
            assert!(found.is_none());
            // and I haven't waited long.
            assert!(waited.as_secs() < 1);

            // If I wait for a back-pagination token for 1 second,
            let before = Instant::now();
            let found = room_event_cache
                .oldest_backpagination_token(Some(Duration::from_secs(1)))
                .await
                .unwrap();
            let waited = before.elapsed();
            // then I still don't get any.
            assert!(found.is_none());
            // and I've waited a bit.
            assert!(waited.as_secs() < 2);
            assert!(waited.as_secs() >= 1);
        }

        #[async_test]
        async fn test_wait_for_pagination_token_already_present() {
            let client = logged_in_client(None).await;
            let room_id = room_id!("!galette:saucisse.bzh");
            client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);

            let event_cache = client.event_cache();

            event_cache.subscribe().unwrap();

            let (room_event_cache, _drop_handlers) = event_cache.for_room(room_id).await.unwrap();
            let room_event_cache = room_event_cache.unwrap();

            let expected_token = PaginationToken("old".to_owned());

            // When I have events and multiple gaps, in a room,
            {
                let mut room_events = room_event_cache.inner.events.write().await;
                room_events.push_gap(Gap { prev_token: expected_token.clone() });
                room_events.push_events([sync_timeline_event!({
                    "sender": "b@z.h",
                    "type": "m.room.message",
                    "event_id": "$ida",
                    "origin_server_ts": 12344446,
                    "content": { "body":"yolo", "msgtype": "m.text" },
                })
                .into()]);
            }

            // If I don't wait for a back-pagination token,
            let found = room_event_cache.oldest_backpagination_token(None).await.unwrap();
            // Then I get it.
            assert_eq!(found.as_ref(), Some(&expected_token));

            // If I wait for a back-pagination token for 0 seconds,
            let before = Instant::now();
            let found = room_event_cache
                .oldest_backpagination_token(Some(Duration::default()))
                .await
                .unwrap();
            let waited = before.elapsed();
            // then I do get one.
            assert_eq!(found.as_ref(), Some(&expected_token));
            // and I haven't waited long.
            assert!(waited.as_millis() < 100);

            // If I wait for a back-pagination token for 1 second,
            let before = Instant::now();
            let found = room_event_cache
                .oldest_backpagination_token(Some(Duration::from_secs(1)))
                .await
                .unwrap();
            let waited = before.elapsed();
            // then I do get one.
            assert_eq!(found.as_ref(), Some(&expected_token));
            // and I haven't waited long.
            assert!(waited.as_millis() < 100);
        }

        #[async_test]
        async fn test_wait_for_late_pagination_token() {
            let client = logged_in_client(None).await;
            let room_id = room_id!("!galette:saucisse.bzh");
            client.base_client().get_or_create_room(room_id, matrix_sdk_base::RoomState::Joined);

            let event_cache = client.event_cache();

            event_cache.subscribe().unwrap();

            let (room_event_cache, _drop_handles) = event_cache.for_room(room_id).await.unwrap();
            let room_event_cache = room_event_cache.unwrap();

            let expected_token = PaginationToken("old".to_owned());

            let before = Instant::now();
            let cloned_expected_token = expected_token.clone();
            let cloned_room_event_cache = room_event_cache.clone();
            let insert_token_task = spawn(async move {
                // If a backpagination token is inserted after 400 milliseconds,
                sleep(Duration::from_millis(400)).await;

                {
                    let mut room_events = cloned_room_event_cache.inner.events.write().await;
                    room_events.push_gap(Gap { prev_token: cloned_expected_token });
                }
            });

            // Then first I don't get it (if I'm not waiting,)
            let found = room_event_cache.oldest_backpagination_token(None).await.unwrap();
            assert!(found.is_none());

            // And if I wait for the back-pagination token for 600ms,
            let found = room_event_cache
                .oldest_backpagination_token(Some(Duration::from_millis(600)))
                .await
                .unwrap();
            let waited = before.elapsed();

            // then I do get one eventually.
            assert_eq!(found.as_ref(), Some(&expected_token));
            // and I have waited between ~400 and ~1000 milliseconds.
            assert!(waited.as_secs() < 1);
            assert!(waited.as_millis() >= 400);

            // The task succeeded.
            insert_token_task.await.unwrap();
        }
    }
}
