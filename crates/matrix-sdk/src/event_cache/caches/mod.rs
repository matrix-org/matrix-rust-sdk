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

use std::{collections::HashMap, ops::Deref, sync::Arc};

use eyeball::SharedObservable;
use eyeball_im::VectorDiff;
use matrix_sdk_base::{
    ThreadingSupport,
    event_cache::Event,
    linked_chunk::Position,
    sync::{JoinedRoomUpdate, LeftRoomUpdate},
};
use ruma::{OwnedEventId, RoomId, room_version_rules::RoomVersionRules};
use tokio::sync::{
    OnceCell, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock, broadcast::Sender, mpsc,
};

use self::subscriber::AutoShrinkMessage;
use super::{
    EventCacheError, EventsOrigin, Result, automatic_pagination::AutomaticPagination, states,
};
use crate::{client::WeakClient, room::WeakRoom};

mod aggregator;
pub mod event_focused;
pub mod event_linked_chunk;
pub mod pagination;
pub mod pinned_events;
mod read_receipts;
pub mod room;
pub mod subscriber;
pub mod thread;

/// A type to hold all the caches for a given room.
#[derive(Debug)]
pub(super) struct Caches {
    /// The one and only [`RoomEventCache`].
    ///
    /// [`RoomEventCache`]: room::RoomEventCache
    pub room: room::RoomEventCache,

    /// All the lazily-loaded [`ThreadEventCache`].
    ///
    /// [`ThreadEventCache`]: thread::ThreadEventCache
    // An `Arc` is used to get an owned lock.
    pub threads: Arc<RwLock<HashMap<OwnedEventId, thread::ThreadEventCache>>>,

    /// The one and only [`PinnedEventsCache`].
    ///
    /// [`PinnedEventsCache`]: pinned_events::PinnedEventsCache
    pub pinned_events: OnceCell<pinned_events::PinnedEventsCache>,

    /// All the lazily-loaded [`EventFocusedCache`].
    ///
    /// [`EventFocusedCache`]: event_focused::EventFocusedCache
    // An `Arc` is used to get an owned lock.
    pub event_focused:
        Arc<RwLock<HashMap<event_focused::EventFocusedCacheKey, event_focused::EventFocusedCache>>>,

    /// Internals data, used to lazily create caches.
    internals: CachesInternals,
}

#[derive(Debug)]
struct CachesInternals {
    state: states::StateLock,
    auto_shrink_sender: mpsc::Sender<AutoShrinkMessage>,
    linked_chunk_update_sender: Sender<room::RoomEventCacheLinkedChunkUpdate>,
    room_version_rules: RoomVersionRules,
}

impl Caches {
    /// Create a new [`Caches`].
    pub async fn new(
        weak_client: &WeakClient,
        room_id: &RoomId,
        generic_update_sender: Sender<room::RoomEventCacheGenericUpdate>,
        linked_chunk_update_sender: Sender<room::RoomEventCacheLinkedChunkUpdate>,
        auto_shrink_sender: mpsc::Sender<AutoShrinkMessage>,
        state: &states::StateLock,
        automatic_pagination: Option<AutomaticPagination>,
    ) -> Result<Self> {
        let Some(client) = weak_client.get() else {
            return Err(EventCacheError::ClientDropped);
        };

        let weak_room = WeakRoom::new(weak_client.clone(), room_id.to_owned());

        let room = client
            .get_room(room_id)
            .ok_or_else(|| EventCacheError::RoomNotFound { room_id: room_id.to_owned() })?;
        let room_version_rules = room.clone_info().room_version_rules_or_default();

        let pagination_status = SharedObservable::new(pagination::SharedPaginationStatus::Idle {
            hit_timeline_start: false,
        });

        let enabled_thread_support =
            matches!(client.base_client().threading_support, ThreadingSupport::Enabled { .. });

        let update_sender = room::RoomEventCacheUpdateSender::new(generic_update_sender.clone());

        let own_user_id =
            client.user_id().expect("the user must be logged in, at this point").to_owned();

        let room_state = state
            .try_insert_once_with(
                states::selectors::RoomStateSelector::new(room_id.to_owned()),
                |store_guard| {
                    room::RoomEventCacheState::new(
                        own_user_id.clone(),
                        room_id.to_owned(),
                        weak_room.clone(),
                        room_version_rules.clone(),
                        enabled_thread_support,
                        update_sender.clone(),
                        linked_chunk_update_sender.clone(),
                        store_guard,
                        pagination_status.clone(),
                        automatic_pagination,
                    )
                },
            )
            .await?;

        let timeline_is_not_empty =
            room_state.read().await?.room_linked_chunk().revents().next().is_some();

        let room_event_cache = room::RoomEventCache::new(
            room_id.to_owned(),
            weak_room,
            own_user_id,
            room_state,
            pagination_status,
            auto_shrink_sender.clone(),
            update_sender,
        );

        // If at least one event has been loaded, it means there is a timeline. Let's
        // emit a generic update.
        if timeline_is_not_empty {
            let _ = generic_update_sender
                .send(room::RoomEventCacheGenericUpdate { room_id: room_id.to_owned() });
        }

        Ok(Self {
            room: room_event_cache,
            threads: Arc::new(RwLock::new(HashMap::new())),
            pinned_events: OnceCell::new(),
            event_focused: Arc::new(RwLock::new(HashMap::new())),
            internals: CachesInternals {
                state: state.clone(),
                auto_shrink_sender,
                linked_chunk_update_sender,
                room_version_rules,
            },
        })
    }

    /// Get the [`RoomEventCache`].
    ///
    /// [`RoomEventCache`]: room::RoomEventCache
    pub fn room(&self) -> &room::RoomEventCache {
        &self.room
    }

    /// Get or create a [`ThreadEventCache`].
    ///
    /// Note: it is impossible to know if `thread_id` represents a valid thread
    /// identifier. It means it's possible to create a [`ThreadEventCache`] for
    /// an event that is not a thread root.
    ///
    /// [`ThreadEventCache`]: thread::ThreadEventCache
    pub async fn thread(
        &self,
        thread_id: OwnedEventId,
    ) -> Result<
        OwnedRwLockReadGuard<
            HashMap<OwnedEventId, thread::ThreadEventCache>,
            thread::ThreadEventCache,
        >,
    > {
        Ok(
            match OwnedRwLockWriteGuard::try_downgrade_map(
                self.threads.clone().write_owned().await,
                |threads| threads.get(&thread_id),
            ) {
                // Thread exists.
                Ok(locked_cache) => locked_cache,
                // Thread does not exist, let's create it.
                Err(mut threads) => {
                    let room = &self.room;
                    let cache = thread::ThreadEventCache::new(
                        room.room_id().to_owned(),
                        thread_id.clone(),
                        room.own_user_id().to_owned(),
                        self.internals.room_version_rules.clone(),
                        room.weak_room().to_owned(),
                        &self.internals.state,
                        self.internals.auto_shrink_sender.clone(),
                        room.update_sender().generic_update_sender().clone(),
                        self.internals.linked_chunk_update_sender.clone(),
                    )
                    .await?;

                    threads.insert(thread_id.clone(), cache);

                    OwnedRwLockWriteGuard::downgrade_map(threads, |threads| {
                        threads.get(&thread_id).unwrap()
                    })
                }
            },
        )
    }

    /// Get or create a [`PinnedEventsCache`].
    ///
    /// [`PinnedEventsCache`]: pinned_events::PinnedEventsCache
    pub async fn pinned_events(&self) -> Result<&pinned_events::PinnedEventsCache> {
        self.pinned_events
            .get_or_try_init(|| {
                pinned_events::PinnedEventsCache::new(
                    self.room.weak_room(),
                    self.room.own_user_id().clone(),
                    self.internals.room_version_rules.clone(),
                    self.internals.linked_chunk_update_sender.clone(),
                    &self.internals.state,
                )
            })
            .await
    }

    /// Get or create a [`EventFocusedCache`].
    ///
    /// [`EventFocusedCache`]: event_focused::EventFocusedCache
    pub async fn event_focused(
        &self,
        event_id: OwnedEventId,
        thread_mode: event_focused::EventFocusThreadMode,
        number_of_initial_events: u16,
    ) -> Result<
        OwnedRwLockReadGuard<
            HashMap<event_focused::EventFocusedCacheKey, event_focused::EventFocusedCache>,
            event_focused::EventFocusedCache,
        >,
    > {
        let key = event_focused::EventFocusedCacheKey { focused_event_id: event_id, thread_mode };

        Ok(
            match OwnedRwLockWriteGuard::try_downgrade_map(
                self.event_focused.clone().write_owned().await,
                |event_focused_caches| event_focused_caches.get(&key),
            ) {
                // Event-focused cache exists.
                Ok(locked_cache) => locked_cache,
                // Event-focused cache does not exist, let's create it.
                Err(mut event_focused_caches) => {
                    let cache = event_focused::EventFocusedCache::new(
                        self.room.weak_room().clone(),
                        key.clone(),
                        &self.internals.state,
                        self.internals.linked_chunk_update_sender.clone(),
                    )
                    .await?;
                    cache.start_from(number_of_initial_events, thread_mode).await?;

                    event_focused_caches.insert(key.clone(), cache);

                    OwnedRwLockWriteGuard::downgrade_map(
                        event_focused_caches,
                        |event_focused_caches| event_focused_caches.get(&key).unwrap(),
                    )
                }
            },
        )
    }

    /// Update all the event caches with a [`JoinedRoomUpdate`].
    pub(super) async fn handle_joined_room_update(&self, updates: JoinedRoomUpdate) -> Result<()> {
        let Self { room, threads, pinned_events, event_focused, internals } = &self;

        // Room.
        {
            let mut updates = updates.clone();
            updates.timeline = aggregator::aggregate_timeline_for_room(updates.timeline);

            room.handle_joined_room_update(updates).await?;
        }

        // Threads.
        {
            let mut updates = updates.clone();
            updates.account_data.clear();
            updates.ambiguity_changes.clear();

            let timeline_for_threads = aggregator::aggregate_timeline_for_threads(
                &updates.timeline,
                threads.read().await.deref(),
                room.state().read().await?,
                &internals.room_version_rules.redaction,
            )
            .await?;

            for (thread_id, timeline) in timeline_for_threads {
                let mut updates = updates.clone();
                updates.timeline = timeline;

                let thread = self.thread(thread_id).await?;
                thread.handle_joined_room_update(updates).await?;

                let new_thread_summary =
                    thread.state().read().await?.compute_thread_summary().await?;

                room.update_thread_summary(thread.thread_id(), new_thread_summary).await?;
            }
        }

        // Pinned-events.
        if let Some(pinned_events) = pinned_events.get() {
            let mut updates = updates.clone();
            updates.timeline = aggregator::aggregate_timeline_for_pinned_events(
                &updates.timeline,
                &pinned_events.state().read().await?.current_event_ids(),
                &internals.room_version_rules.redaction,
            );

            pinned_events.handle_joined_room_update(updates).await?;
        }

        // Event-focused.
        {
            // An event-focused cache isn't listening to live update. Consequently, it is
            // not interested by this kind of update.
            let _ = event_focused;
        }

        Ok(())
    }

    /// Update all the event caches with a [`LeftRoomUpdate`].
    pub(super) async fn handle_left_room_update(&self, updates: LeftRoomUpdate) -> Result<()> {
        let Self { room, threads, pinned_events, event_focused, internals } = &self;

        // Room.
        {
            let mut updates = updates.clone();
            updates.timeline = aggregator::aggregate_timeline_for_room(updates.timeline);

            room.handle_left_room_update(updates).await?;
        }

        // Threads.
        {
            let mut updates = updates.clone();
            updates.account_data.clear();
            updates.ambiguity_changes.clear();

            let timeline_for_threads = aggregator::aggregate_timeline_for_threads(
                &updates.timeline,
                threads.read().await.deref(),
                room.state().read().await?,
                &internals.room_version_rules.redaction,
            )
            .await?;

            for (thread_id, timeline) in timeline_for_threads {
                let mut updates = updates.clone();
                updates.timeline = timeline;

                let thread = self.thread(thread_id).await?;
                thread.handle_left_room_update(updates).await?;

                let new_thread_summary =
                    thread.state().read().await?.compute_thread_summary().await?;

                room.update_thread_summary(thread.thread_id(), new_thread_summary).await?;
            }
        }

        // Pinned-events.
        if let Some(pinned_events) = pinned_events.get() {
            let mut updates = updates.clone();
            updates.timeline = aggregator::aggregate_timeline_for_pinned_events(
                &updates.timeline,
                &pinned_events.state().read().await?.current_event_ids(),
                &internals.room_version_rules.redaction,
            );

            pinned_events.handle_left_room_update(updates).await?;
        }

        // Event-focused.
        {
            // An event-focused cache isn't listening to live update. Consequently, it is
            // not interested by this kind of update.
            let _ = event_focused;
        }

        Ok(())
    }

    /// Get all in-memory events from all the event caches managed by this
    /// [`Caches`].
    ///
    /// Events can be duplicated if present in different event caches.
    #[cfg(feature = "e2e-encryption")]
    pub async fn all_in_memory_events(&self) -> Result<impl Iterator<Item = Event>> {
        // We have to fetch events from all the caches.
        //
        // The room cache contains all the room events + the thread events + the
        // pinned-events.
        let mut events = self.room.events().await?;

        // The last cache is the events from the event-focused cache.
        {
            let event_focused = self.event_focused.read().await;

            for event_focused in event_focused.values() {
                events.extend(event_focused.events().await?);
            }
        }

        Ok(events.into_iter())
    }

    /// Get all encrypted events from all the event caches managed by this
    /// [`Caches`].
    ///
    /// The `event_type` represents the type of the event to filter by.
    /// The `session_id` represents the unique ID of the room key that was used
    /// to encrypt the event
    ///
    /// Events can be duplicated if present in different event caches.
    #[cfg(feature = "e2e-encryption")]
    pub async fn all_events_of_type(
        &self,
        event_type: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<impl Iterator<Item = Event>> {
        // All caches store their events in the store except one. Let's start by looking
        // inside the store.
        let mut events = {
            let state = self.internals.state.read().await?;

            state.store.get_room_events(self.room.room_id(), event_type, session_id).await?
        };

        // The only cache to not store its events is the event-focused cache. Its events
        // only live in memory.
        {
            let event_focused = self.event_focused.read().await;

            for event_focused in event_focused.values() {
                events.extend(
                    event_focused
                        .events()
                        .await?
                        .into_iter()
                        .filter(|event| event_type == event.kind.event_type().as_deref())
                        .filter(|event| session_id == event.kind.session_id()),
                );
            }
        }

        Ok(events.into_iter())
    }
}

/// A diff update for an event cache timeline represented as a vector.
#[derive(Clone, Debug)]
pub struct TimelineVectorDiffs {
    /// New vector diff for the thread timeline.
    pub diffs: Vec<VectorDiff<Event>>,
    /// The origin that triggered this update.
    pub origin: EventsOrigin,
}

/// An enum representing where an event has been found.
#[derive(Debug)]
pub(super) enum EventLocation {
    /// Event lives in memory (and likely in the store!).
    Memory(Position),

    /// Event lives in the store only, it has not been loaded in memory yet.
    Store,
}
