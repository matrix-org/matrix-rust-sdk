use std::{fmt::Formatter, num::NonZeroUsize, sync::Arc};

use itertools::Itertools;
use matrix_sdk::{
    config::RequestConfig, event_cache::paginator::PaginatorError,
    pinned_events_cache::PinnedEventCache, Room, SendOutsideWasm, SyncOutsideWasm,
};
use matrix_sdk_base::deserialized_responses::SyncTimelineEvent;
use ruma::{EventId, MilliSecondsSinceUnixEpoch, OwnedEventId};
use thiserror::Error;
use tracing::info;

const MAX_CONCURRENT_REQUESTS: usize = 10;

/// Utility to load the pinned events in a room.
pub struct PinnedEventsLoader {
    room: Arc<Box<dyn PinnedEventsRoom>>,
    max_events_to_load: usize,
    max_concurrent_requests: usize,
}

impl PinnedEventsLoader {
    /// Creates a new `PinnedEventsLoader` instance.
    pub fn new(room: Box<dyn PinnedEventsRoom>, max_events_to_load: usize) -> Self {
        Self {
            room: Arc::new(room),
            max_events_to_load,
            max_concurrent_requests: MAX_CONCURRENT_REQUESTS,
        }
    }

    /// Loads the pinned events in this room, using the cache first and then
    /// requesting the event from the homeserver if it couldn't be found.
    /// This method will perform as many concurrent requests for events as
    /// `max_concurrent_requests` allows, to avoid overwhelming the server.
    ///
    /// It returns a `Result` with either a
    /// chronologically sorted list of retrieved `SyncTimelineEvent`s
    /// or a `PinnedEventsLoaderError`.
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

        let has_pinned_event_ids = !pinned_event_ids.is_empty();
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
            let provider = Arc::new(self.room.clone());
            let mut handles = Vec::new();

            let config = Some(
                RequestConfig::default()
                    .retry_limit(3)
                    .max_concurrent_requests(NonZeroUsize::new(self.max_concurrent_requests)),
            );

            for id in event_ids_to_request {
                handles.push(tokio::spawn({
                    let provider = Arc::clone(&provider);
                    async move {
                        provider
                            .event_with_config(&id, config)
                            .await
                            .map_err(|_| PinnedEventsLoaderError::EventNotFound(id.to_owned()))
                    }
                }));
            }

            for handle in handles {
                if let Ok(Ok(ev)) = handle.await {
                    loaded_events.push(ev);
                }
            }
        }

        if has_pinned_event_ids && loaded_events.is_empty() {
            return Err(PinnedEventsLoaderError::TimelineReloadFailed);
        }

        info!("Saving {} pinned events to the cache", loaded_events.len());
        cache.set_bulk(&loaded_events).await;

        fn timestamp(item: &SyncTimelineEvent) -> MilliSecondsSinceUnixEpoch {
            item.event
                .deserialize()
                .map(|e| e.origin_server_ts())
                .unwrap_or_else(|_| MilliSecondsSinceUnixEpoch::now())
        }

        // Sort using chronological ordering (oldest -> newest)
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
    ) -> Option<Result<Vec<SyncTimelineEvent>, PinnedEventsLoaderError>> {
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
            Some(self.load_events(cache).await)
        } else {
            None
        }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait PinnedEventsRoom: SendOutsideWasm + SyncOutsideWasm {
    /// Load a single room event.
    async fn event_with_config(
        &self,
        event_id: &EventId,
        request_config: Option<RequestConfig>,
    ) -> Result<SyncTimelineEvent, PaginatorError>;

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
    async fn event_with_config(
        &self,
        event_id: &EventId,
        request_config: Option<RequestConfig>,
    ) -> Result<SyncTimelineEvent, PaginatorError> {
        self.event_with_config(event_id, request_config)
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
    #[error("No event found for the given event id.")]
    EventNotFound(OwnedEventId),

    #[error("Timeline focus is not pinned events.")]
    TimelineFocusNotPinnedEvents,

    #[error("Could not load pinned events.")]
    TimelineReloadFailed,
}
