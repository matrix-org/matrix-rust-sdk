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

use std::{fmt::Formatter, num::NonZeroUsize, sync::Arc};

use futures_util::future::join_all;
use matrix_sdk::{
    config::RequestConfig, event_cache::paginator::PaginatorError,
    pinned_events_cache::PinnedEventCache, Room, SendOutsideWasm, SyncOutsideWasm,
};
use matrix_sdk_base::deserialized_responses::SyncTimelineEvent;
use ruma::{EventId, MilliSecondsSinceUnixEpoch, OwnedEventId};
use thiserror::Error;
use tracing::{debug, warn};

const MAX_CONCURRENT_REQUESTS: usize = 10;

/// Utility to load the pinned events in a room.
pub struct PinnedEventsLoader {
    /// Backend to load pinned events.
    room: Arc<dyn PinnedEventsRoom>,
    /// Maximum number of pinned events to load (either from network or the
    /// cache).
    max_events_to_load: usize,
}

impl PinnedEventsLoader {
    /// Creates a new `PinnedEventsLoader` instance.
    pub fn new(room: Arc<dyn PinnedEventsRoom>, max_events_to_load: usize) -> Self {
        Self { room, max_events_to_load }
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
                debug!("Loading pinned event {ev_id} from cache");
                loaded_events.push(ev.clone());
            } else {
                debug!("Loading pinned event {ev_id} from HS");
                event_ids_to_request.push(ev_id);
            }
        }

        if !event_ids_to_request.is_empty() {
            let provider = Arc::new(self.room.clone());

            let request_config = Some(
                RequestConfig::default()
                    .retry_limit(3)
                    .max_concurrent_requests(NonZeroUsize::new(MAX_CONCURRENT_REQUESTS)),
            );

            let new_events = join_all(event_ids_to_request.into_iter().map(|event_id| {
                let provider = Arc::clone(&provider);
                async move {
                    match provider.event_with_config(&event_id, request_config).await {
                        Ok(event) => Some(event),
                        Err(err) => {
                            warn!("error when loading pinned event: {err}");
                            None
                        }
                    }
                }
            }))
            .await;

            loaded_events.extend(new_events.into_iter().flatten());
        }

        if has_pinned_event_ids && loaded_events.is_empty() {
            return Err(PinnedEventsLoaderError::TimelineReloadFailed);
        }

        debug!("Saving {} pinned events to the cache", loaded_events.len());
        cache.set_bulk(loaded_events.clone()).await;

        // Sort using chronological ordering (oldest -> newest)
        loaded_events.sort_by_key(|item| {
            item.event
                .deserialize()
                .map(|e| e.origin_server_ts())
                .unwrap_or_else(|_| MilliSecondsSinceUnixEpoch::now())
        });

        Ok(loaded_events)
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
            cache.set_bulk(to_update).await;
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
        self.event(event_id, request_config)
            .await
            .map(|e| e.into())
            .map_err(|err| PaginatorError::SdkError(Box::new(err)))
    }

    fn pinned_event_ids(&self) -> Vec<OwnedEventId> {
        self.clone_info().pinned_event_ids()
    }

    fn is_pinned_event(&self, event_id: &EventId) -> bool {
        self.clone_info().is_pinned_event(event_id)
    }
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for PinnedEventsLoader {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinnedEventsLoader")
            .field("max_events_to_load", &self.max_events_to_load)
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
