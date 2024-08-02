use std::{fmt::Formatter, sync::Arc};

use itertools::Itertools;
use matrix_sdk::{
    event_cache::paginator::PaginatorError, pinned_events_cache::PinnedEventCache, Room,
    SendOutsideWasm, SyncOutsideWasm,
};
use matrix_sdk_base::deserialized_responses::SyncTimelineEvent;
use ruma::{EventId, MilliSecondsSinceUnixEpoch, OwnedEventId};
use thiserror::Error;
use tokio::sync::Semaphore;
use tracing::info;

/// Utility to load the pinned events in a room.
pub struct PinnedEventsLoader {
    room: Arc<Box<dyn PinnedEventsRoom>>,
    max_events_to_load: usize,
    max_concurrent_requests: usize,
}

impl PinnedEventsLoader {
    /// Creates a new `PinnedEventsLoader` instance.
    pub fn new(room: Box<dyn PinnedEventsRoom>, max_events_to_load: usize) -> Self {
        Self { room: Arc::new(room), max_events_to_load, max_concurrent_requests: 10 }
    }

    /// Loads the pinned events in this room, using the cache first and then
    /// requesting the event from the homeserver if it couldn't be found.
    /// This method will perform as many concurrent requests for events as
    /// `max_concurrent_requests` allows, to avoid overwhelming the server.
    ///
    /// It returns a `Result` with either a
    /// chronologically sorted list of retrieved `SyncTimelineEvent`s  or a
    /// `PinnedEventsLoaderError`.
    pub async fn load_events(
        &self,
        cache: &PinnedEventCache,
    ) -> Result<Vec<SyncTimelineEvent>, PinnedEventsLoaderError> {
        let pinned_event_ids: Vec<OwnedEventId> = self
            .room
            .pinned_event_ids()
            .into_iter()
            .rev()
            .take(self.max_events_to_load)
            .rev()
            .collect();

        let mut loaded_events = Vec::new();
        let mut event_ids_to_request = Vec::new();
        for ev_id in pinned_event_ids {
            if let Some(ev) = cache.get(&ev_id).await {
                info!("Loading pinned event {ev_id} from cache");
                loaded_events.push(ev.clone());
            } else {
                info!("Loading pinned event {ev_id} from HS");
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
                            .event(&id)
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

        info!("Saving {} pinned events to the cache", loaded_events.len());
        cache.set_bulk(&loaded_events).await;

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
        cache: &PinnedEventCache,
    ) -> Option<Vec<SyncTimelineEvent>> {
        let mut to_update = Vec::new();
        for ev in events {
            let ev = ev.into();
            if let Some(ev_id) = ev.event_id() {
                if self.room.is_pinned_event(&ev_id) {
                    to_update.push(ev);
                }
            }
        }

        if !to_update.is_empty() {
            cache.set_bulk(&to_update).await;
            self.load_events(cache).await.ok()
        } else {
            None
        }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait PinnedEventsRoom: SendOutsideWasm + SyncOutsideWasm {
    /// Load a single room event.
    async fn event(&self, event_id: &EventId) -> Result<SyncTimelineEvent, PaginatorError>;

    /// Get the pinned event ids for a room.
    fn pinned_event_ids(&self) -> Vec<OwnedEventId>;

    /// Checks whether an event id is pinned in this room.
    ///
    /// It avoids having to clone the whole list of event ids to check a single
    /// value.
    fn is_pinned_event(&self, event_id: &EventId) -> bool;
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl PinnedEventsRoom for Room {
    async fn event(&self, event_id: &EventId) -> Result<SyncTimelineEvent, PaginatorError> {
        self.event(event_id)
            .await
            .map(|e| e.into())
            .map_err(|err| PaginatorError::SdkError(Box::new(err)))
    }

    fn pinned_event_ids(&self) -> Vec<OwnedEventId> {
        self.clone_info().pinned_event_ids()
    }

    fn is_pinned_event(&self, event_id: &EventId) -> bool {
        self.subscribe_info().read().is_pinned_event(event_id)
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
