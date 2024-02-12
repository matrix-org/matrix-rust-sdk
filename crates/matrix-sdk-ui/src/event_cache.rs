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

use matrix_sdk::{sync::RoomUpdate, Client, Room};
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
    sync::broadcast::{error::RecvError, Receiver, Sender},
    task::JoinHandle,
};
use tracing::{debug, error, trace};

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
struct RoomCacheDropHandles {
    listen_updates_task: JoinHandle<()>,
}

impl Drop for RoomCacheDropHandles {
    fn drop(&mut self) {
        self.listen_updates_task.abort();
    }
}

/// An event cache, providing lots of useful functionality for clients.
///
/// See also the module-level comment.
pub struct EventCache {
    /// Reference to the client used to navigate this cache.
    client: Client,
    /// Lazily-filled cache of live [`RoomEventCache`], once per room.
    by_room: BTreeMap<OwnedRoomId, RoomEventCache>,
    /// Backend used for storage.
    store: Arc<dyn EventCacheStore>,
}

impl Debug for EventCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EventCache").finish_non_exhaustive()
    }
}

impl EventCache {
    /// Create a new [`EventCache`] for the given client.
    pub fn new(client: Client) -> Self {
        let store = Arc::new(MemoryStore::new());
        Self { client, by_room: Default::default(), store }
    }

    /// Return a room-specific view over the [`EventCache`].
    ///
    /// It may not be found, if the room isn't known to the client.
    pub fn for_room(&mut self, room_id: &RoomId) -> Result<RoomEventCache> {
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

    /// Add an initial set of events to the event cache, reloaded from a cache.
    ///
    /// TODO: temporary for API compat, as the event cache should take care of
    /// its own store.
    pub async fn add_initial_events(
        &mut self,
        room_id: &RoomId,
        events: Vec<SyncTimelineEvent>,
    ) -> Result<()> {
        let room_cache = self.for_room(room_id)?;
        room_cache.inner.append_events(events).await?;
        Ok(())
    }
}

/// A subset of an event cache, for a room.
///
/// Cloning is shallow, and thus is cheap to do.
#[derive(Clone)]
pub struct RoomEventCache {
    inner: Arc<RoomEventCacheInner>,

    _drop_handles: Arc<RoomCacheDropHandles>,
}

impl Debug for RoomEventCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoomEventCache").finish_non_exhaustive()
    }
}

impl RoomEventCache {
    /// Create a new [`RoomEventCache`] using the given room and store.
    fn new(room: Room, store: Arc<dyn EventCacheStore>) -> Self {
        let (inner, drop_handles) = RoomEventCacheInner::new(room, store);
        Self { inner, _drop_handles: drop_handles }
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
    fn new(room: Room, store: Arc<dyn EventCacheStore>) -> (Arc<Self>, Arc<RoomCacheDropHandles>) {
        let sender = Sender::new(32);

        let room_cache = Arc::new(Self { room, store, sender });

        let listen_updates_task = spawn(Self::listen_task(room_cache.clone()));

        (room_cache, Arc::new(RoomCacheDropHandles { listen_updates_task }))
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

    async fn listen_task(this: Arc<Self>) {
        // TODO for prototyping, i'm spawning a new task to get the room updates.
        // Ideally we'd have something like the whole sync update, a generalisation of
        // the room update.
        trace!("Spawning the listen task");

        let mut update_receiver = this.room.client().subscribe_to_room_updates(this.room.room_id());

        loop {
            match update_receiver.recv().await {
                Ok(update) => {
                    trace!("Listen task received an update");

                    match update {
                        RoomUpdate::Left { updates, .. } => {
                            if let Err(err) = this.handle_left_room_update(updates).await {
                                error!("handling left room update: {err}");
                            }
                        }
                        RoomUpdate::Joined { updates, .. } => {
                            if let Err(err) = this.handle_joined_room_update(updates).await {
                                error!("handling joined room update: {err}");
                            }
                        }
                        RoomUpdate::Invited { .. } => {
                            // We don't do anything for invited rooms at this
                            // point. TODO should
                            // we?
                        }
                    }
                }

                Err(RecvError::Closed) => {
                    // The loop terminated successfully.
                    debug!("Listen task closed");
                    break;
                }

                Err(RecvError::Lagged(_)) => {
                    // Since we've lagged behind updates to this room, we might be out of
                    // sync with the events, leading to potentially lost events. Play it
                    // safe here, and clear the cache. It's fine because we can retrigger
                    // backpagination from the last event at any time, if needs be.
                    debug!("Listen task lagged, clearing room");
                    if let Err(err) = this.store.clear_room_events(this.room.room_id()).await {
                        error!("unable to clear room after room updates lag: {err}");
                    }
                }
            }
        }
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
