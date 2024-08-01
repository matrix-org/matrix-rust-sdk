use std::{collections::BTreeMap, fmt::Formatter, sync::Arc};

use itertools::Itertools;
use matrix_sdk::{event_cache::paginator::PaginatorError, Room, SendOutsideWasm, SyncOutsideWasm};
use matrix_sdk_base::deserialized_responses::SyncTimelineEvent;
use ruma::{EventId, MilliSecondsSinceUnixEpoch, OwnedEventId};
use thiserror::Error;
use tokio::sync::{RwLock, Semaphore};

/// Utility to load the pinned events in a room.
pub struct PinnedEventsLoader {
    room: Arc<Box<dyn PinnedEventsRoom>>,
    max_events_to_load: usize,
    max_concurrent_requests: usize,
    cache: PinnedEventCache,
}

impl PinnedEventsLoader {
    /// Creates a new `PinnedEventsLoader` instance.
    pub fn new(room: Box<dyn PinnedEventsRoom>, max_events_to_load: usize) -> Self {
        Self {
            room: Arc::new(room),
            max_events_to_load,
            max_concurrent_requests: 10,
            cache: PinnedEventCache {
                inner: Arc::new(InnerPinnedEventCache { events: Default::default() }),
            },
        }
    }

    /// Loads the pinned events in this room, using the cache first and then
    /// requesting the event from the homeserver if it couldn't be found.
    /// This method will perform as many concurrent requests for events as
    /// `max_concurrent_requests` allows, to avoid overwhelming the server.
    ///
    /// It returns a `Result` with either a
    /// chronologically sorted list of retrieved `SyncTimelineEvent`s  or a
    /// `PinnedEventsLoaderError`.
    pub async fn load_events(&self) -> Result<Vec<SyncTimelineEvent>, PinnedEventsLoaderError> {
        let pinned_event_ids: Vec<OwnedEventId> = self
            .room
            .room_pinned_event_ids()
            .into_iter()
            .rev()
            .take(self.max_events_to_load)
            .rev()
            .collect();

        let mut loaded_events = Vec::new();
        let mut event_ids_to_request = Vec::new();
        for ev_id in pinned_event_ids {
            if let Some(ev) = self.cache.get(&ev_id).await {
                loaded_events.push(ev.clone());
            } else {
                event_ids_to_request.push(ev_id);
            }
        }

        if !event_ids_to_request.is_empty() {
            let semaphore = Arc::new(Semaphore::new(self.max_concurrent_requests));
            let provider = Arc::new(self.room.clone());
            let mut handles = Vec::new();

            for id in event_ids_to_request {
                handles.push(tokio::spawn({
                    let semaphore = Arc::clone(&semaphore);
                    let provider = Arc::clone(&provider);
                    async move {
                        let permit = semaphore
                            .acquire()
                            .await
                            .map_err(|_| PinnedEventsLoaderError::SemaphoreNotAcquired)?;
                        let ret = provider
                            .room_event(&id)
                            .await
                            .map_err(|_| PinnedEventsLoaderError::EventNotFound(id.to_owned()));
                        drop(permit);
                        ret
                    }
                }));
            }

            for handle in handles {
                if let Ok(Ok(ev)) = handle.await {
                    loaded_events.push(ev)
                }
            }
        }

        self.cache.set_bulk(&loaded_events).await;

        fn timestamp(item: &SyncTimelineEvent) -> MilliSecondsSinceUnixEpoch {
            item.event
                .deserialize()
                .map(|e| e.origin_server_ts())
                .unwrap_or_else(|_| MilliSecondsSinceUnixEpoch::now())
        }

        let sorted_events = loaded_events
            .into_iter()
            .sorted_by(|e1, e2| timestamp(e1).cmp(&timestamp(e2)))
            .collect();

        Ok(sorted_events)
    }

    /// Updates the cache with the provided events if they're associated with a
    /// pinned event id, then reloads the list of pinned events if there
    /// were any changes.
    ///
    /// Returns an `Option` with either a new list of events if any of them
    /// changed in this update or `None` if they didn't.
    pub async fn update_if_needed(
        &self,
        events: Vec<impl Into<SyncTimelineEvent>>,
    ) -> Option<Vec<SyncTimelineEvent>> {
        let mut to_update = Vec::new();
        for ev in events {
            let ev = ev.into();
            if let Some(ev_id) = ev.event_id() {
                if self.room.room_is_pinned_event_id(&ev_id) {
                    to_update.push(ev);
                }
            }
        }

        if !to_update.is_empty() {
            self.cache.set_bulk(&to_update).await;
            self.load_events().await.ok()
        } else {
            None
        }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait PinnedEventsRoom: SendOutsideWasm + SyncOutsideWasm {
    /// Load a single room event.
    async fn room_event(&self, event_id: &EventId) -> Result<SyncTimelineEvent, PaginatorError>;

    /// Get the pinned event ids for a room.
    fn room_pinned_event_ids(&self) -> Vec<OwnedEventId>;

    /// Checks whether an event id is pinned in this room.
    fn room_is_pinned_event_id(&self, event_id: &EventId) -> bool;
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl PinnedEventsRoom for Room {
    async fn room_event(&self, event_id: &EventId) -> Result<SyncTimelineEvent, PaginatorError> {
        self.event(event_id)
            .await
            .map(|e| e.into())
            .map_err(|err| PaginatorError::SdkError(Box::new(err)))
    }

    fn room_pinned_event_ids(&self) -> Vec<OwnedEventId> {
        self.pinned_event_ids()
    }

    fn room_is_pinned_event_id(&self, event_id: &EventId) -> bool {
        self.is_pinned_event(event_id)
    }
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for PinnedEventsLoader {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedEventsLoader")
            .field("max_concurrent_requests", &self.max_concurrent_requests)
            .finish()
    }
}

/// Errors related to `PinnedEventsLoader` usage.
#[derive(Error, Debug)]
pub enum PinnedEventsLoaderError {
    #[error("Semaphore for requests couldn't be acquired. It was probably aborted.")]
    SemaphoreNotAcquired,

    #[error("No event found for the given event id.")]
    EventNotFound(OwnedEventId),

    #[error("Timeline focus is not pinned events.")]
    TimelineFocusNotPinnedEvents,
}

/// Cache used to store the events associated with pinned event ids.
///
/// Cloning is shallow, and thus is cheap to do.
#[derive(Clone)]
pub(crate) struct PinnedEventCache {
    inner: Arc<InnerPinnedEventCache>,
}

impl PinnedEventCache {
    /// Gets the event associated with the provided event id, if it exists in
    /// the cache.
    pub(crate) async fn get(&self, event_id: &EventId) -> Option<SyncTimelineEvent> {
        let cache = self.inner.events.read().await;
        cache.get(event_id).cloned()
    }

    /// Adds a list of pinned events to the cache in a performant way.
    pub(crate) async fn set_bulk(&self, events: &Vec<SyncTimelineEvent>) {
        let mut cache = self.inner.events.write().await;
        for ev in events {
            if let Some(ev_id) = ev.event_id() {
                cache.insert(ev_id.to_owned(), ev.clone());
            }
        }
    }
}

/// The non-cloneable implementation of the cache.
struct InnerPinnedEventCache {
    /// The pinned events of the room.
    events: RwLock<BTreeMap<OwnedEventId, SyncTimelineEvent>>,
}
