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
//! - [ ] backwards and forward pagination, and reconcile results with cached
//!   timelines.
//! - [ ] retry decryption upon receiving new keys (from an encryption sync
//!   service or from a key backup).
//! - [ ] expose the latest event for a given room.
//! - [ ] caching of events on-disk.

#![forbid(missing_docs)]

use std::{collections::BTreeMap, fmt::Debug, sync::Arc};

use matrix_sdk::{Client, Room};
use matrix_sdk_base::{
    deserialized_responses::{AmbiguityChange, SyncTimelineEvent},
    sync::{JoinedRoomUpdate, LeftRoomUpdate, Timeline},
};
use ruma::{
    events::{AnyRoomAccountDataEvent, AnySyncEphemeralRoomEvent},
    serde::Raw,
    OwnedEventId, OwnedRoomId, RoomId,
};
use tokio::{
    spawn,
    sync::{
        broadcast::{error::RecvError, Receiver, Sender},
        RwLock,
    },
    task::JoinHandle,
};
use tracing::{error, trace};

use self::store::{EventCacheStore, MemoryStore};

mod store;

/// An error observed in the [`EventCache`].
#[derive(thiserror::Error, Debug)]
pub enum EventCacheError {
    /// A room hasn't been found, when trying to create a view for that room.
    #[error("Room with id {0} not found")]
    RoomNotFound(OwnedRoomId),
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
/// See also the module-level comment.
pub struct EventCache {
    inner: Arc<RwLock<EventCacheInner>>,

    drop_handles: Arc<EventCacheDropHandles>,
}

impl Debug for EventCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventCache").finish_non_exhaustive()
    }
}

impl EventCache {
    /// Create a new [`EventCache`] for the given client.
    pub fn new(client: Client) -> Self {
        let mut room_updates_feed = client.subscribe_to_all_room_updates();

        let store = Arc::new(MemoryStore::new());
        let inner =
            Arc::new(RwLock::new(EventCacheInner { client, by_room: Default::default(), store }));

        // Spawn the task that will listen to all the room updates at once.
        trace!("Spawning the listen task");
        let listen_updates_task = spawn({
            let inner = inner.clone();

            async move {
                loop {
                    match room_updates_feed.recv().await {
                        Ok(updates) => {
                            // We received some room updates. Handle them.

                            // Left rooms.
                            for (room_id, left_room_update) in updates.leave {
                                let room = match inner.write().await.for_room(&room_id).await {
                                    Ok(room) => room,
                                    Err(err) => {
                                        error!("can't get left room {room_id}: {err}");
                                        continue;
                                    }
                                };

                                if let Err(err) =
                                    room.inner.handle_left_room_update(left_room_update).await
                                {
                                    error!("handling left room update: {err}");
                                }
                            }

                            // Joined rooms.
                            for (room_id, joined_room_update) in updates.join {
                                let room = match inner.write().await.for_room(&room_id).await {
                                    Ok(room) => room,
                                    Err(err) => {
                                        error!("can't get joined room {room_id}: {err}");
                                        continue;
                                    }
                                };

                                if let Err(err) =
                                    room.inner.handle_joined_room_update(joined_room_update).await
                                {
                                    error!("handling joined room update: {err}");
                                }
                            }

                            // Invited rooms.
                            // TODO: we don't anything with `updates.invite` at
                            // this point.
                        }

                        Err(RecvError::Lagged(_)) => {
                            // Forget everything we know; we could have missed events, and we have
                            // no way to reconcile at the moment!
                            // TODO: implement Smart Matching™,
                            let mut inner = inner.write().await;
                            for room_id in inner.by_room.keys() {
                                if let Err(err) = inner.store.clear_room_events(room_id).await {
                                    error!("unable to clear room after room updates lag: {err}");
                                }
                            }
                            inner.by_room.clear();
                        }

                        Err(RecvError::Closed) => {
                            // The sender has shut down, exit.
                            break;
                        }
                    }
                }
            }
        });

        Self { inner, drop_handles: Arc::new(EventCacheDropHandles { listen_updates_task }) }
    }

    /// Return a room-specific view over the [`EventCache`].
    ///
    /// It may not be found, if the room isn't known to the client.
    pub async fn for_room(
        &self,
        room_id: &RoomId,
    ) -> Result<(RoomEventCache, Arc<EventCacheDropHandles>)> {
        let room = self.inner.write().await.for_room(room_id).await?;

        Ok((room, self.drop_handles.clone()))
    }

    /// Add an initial set of events to the event cache, reloaded from a cache.
    ///
    /// TODO: temporary for API compat, as the event cache should take care of
    /// its own store.
    pub async fn add_initial_events(
        &mut self,
        room_id: &RoomId,
        events: Vec<SyncTimelineEvent>,
    ) -> Result<()> {
        let room_cache = self.inner.write().await.for_room(room_id).await?;
        room_cache.inner.append_events(events).await?;
        Ok(())
    }
}

struct EventCacheInner {
    /// Reference to the client used to navigate this cache.
    client: Client,

    /// Lazily-filled cache of live [`RoomEventCache`], once per room.
    by_room: BTreeMap<OwnedRoomId, RoomEventCache>,

    /// Backend used for storage.
    store: Arc<dyn EventCacheStore>,
}

impl EventCacheInner {
    /// Return a room-specific view over the [`EventCache`].
    ///
    /// It may not be found, if the room isn't known to the client.
    async fn for_room(&mut self, room_id: &RoomId) -> Result<RoomEventCache> {
        match self.by_room.get(room_id) {
            Some(room) => Ok(room.clone()),
            None => {
                let room = self
                    .client
                    .get_room(room_id)
                    .ok_or_else(|| EventCacheError::RoomNotFound(room_id.to_owned()))?;
                let room_event_cache = RoomEventCache::new(room, self.store.clone());

                self.by_room.insert(room_id.to_owned(), room_event_cache.clone());

                Ok(room_event_cache)
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
    fn new(room: Room, store: Arc<dyn EventCacheStore>) -> Self {
        Self { inner: Arc::new(RoomEventCacheInner::new(room, store)) }
    }

    /// Subscribe to room updates for this room, after getting the initial list
    /// of events. XXX: Could/should it use some kind of `Observable`
    /// instead? Or not something async, like explicit handlers as our event
    /// handlers?
    pub async fn subscribe(
        &self,
    ) -> Result<(Vec<SyncTimelineEvent>, Receiver<RoomEventCacheUpdate>)> {
        Ok((
            self.inner.store.room_events(self.inner.room.room_id()).await?,
            self.inner.sender.subscribe(),
        ))
    }
}

struct RoomEventCacheInner {
    sender: Sender<RoomEventCacheUpdate>,
    store: Arc<dyn EventCacheStore>,
    room: Room,
}

impl RoomEventCacheInner {
    /// Creates a new cache for a room, and subscribes to room updates, so as
    /// to handle new timeline events.
    fn new(room: Room, store: Arc<dyn EventCacheStore>) -> Self {
        let sender = Sender::new(32);
        Self { room, store, sender }
    }

    async fn handle_joined_room_update(&self, updates: JoinedRoomUpdate) -> Result<()> {
        self.handle_timeline(
            updates.timeline,
            updates.ephemeral.clone(),
            updates.account_data,
            updates.ambiguity_changes,
        )
        .await?;
        Ok(())
    }

    async fn handle_timeline(
        &self,
        timeline: Timeline,
        ephemeral: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        account_data: Vec<Raw<AnyRoomAccountDataEvent>>,
        ambiguity_changes: BTreeMap<OwnedEventId, AmbiguityChange>,
    ) -> Result<()> {
        let room_id = self.room.room_id();

        if timeline.limited {
            // Ideally we'd try to reconcile existing events against those received in the
            // timeline, but we're not there yet. In the meanwhile, clear the
            // items from the room. TODO: implement Smart Matching™.
            trace!("limited timeline, clearing all previous events");
            self.store.clear_room_events(room_id).await?;
            let _ = self.sender.send(RoomEventCacheUpdate::Clear);
        }

        // Add all the events to the backend.
        trace!("adding new events");
        self.store.add_room_events(room_id, timeline.events.clone()).await?;

        // Propagate events to observers.
        let _ = self.sender.send(RoomEventCacheUpdate::Append {
            events: timeline.events,
            prev_batch: timeline.prev_batch,
            ephemeral,
            account_data,
            ambiguity_changes,
        });

        Ok(())
    }

    async fn handle_left_room_update(&self, updates: LeftRoomUpdate) -> Result<()> {
        self.handle_timeline(updates.timeline, Vec::new(), Vec::new(), updates.ambiguity_changes)
            .await?;
        Ok(())
    }

    /// Append a set of events to the room cache and storage, notifying
    /// observers.
    async fn append_events(&self, events: Vec<SyncTimelineEvent>) -> Result<()> {
        self.store.add_room_events(self.room.room_id(), events.clone()).await?;

        let _ = self.sender.send(RoomEventCacheUpdate::Append {
            events,
            prev_batch: None,
            account_data: Default::default(),
            ephemeral: Default::default(),
            ambiguity_changes: Default::default(),
        });

        Ok(())
    }
}

/// An update related to events happened in a room.
#[derive(Clone)]
pub enum RoomEventCacheUpdate {
    /// The room has been cleared from events.
    Clear,
    /// The room has new events.
    Append {
        /// All the new events that have been added to the room.
        events: Vec<SyncTimelineEvent>,
        /// XXX: this is temporary, until backpagination lives in the event
        /// cache.
        prev_batch: Option<String>,
        /// XXX: this is temporary, until account data lives in the event cache
        /// — or will it live there?
        account_data: Vec<Raw<AnyRoomAccountDataEvent>>,
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
