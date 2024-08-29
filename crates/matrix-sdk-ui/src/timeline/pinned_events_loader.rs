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

use futures_util::{future::join_all, FutureExt as _};
use matrix_sdk::{
    config::RequestConfig, event_cache::paginator::PaginatorError, BoxFuture, Room,
    SendOutsideWasm, SyncOutsideWasm,
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
    pub async fn load_events(&self) -> Result<Vec<SyncTimelineEvent>, PinnedEventsLoaderError> {
        let pinned_event_ids: Vec<OwnedEventId> = self
            .room
            .pinned_event_ids()
            .into_iter()
            .rev()
            .take(self.max_events_to_load)
            .rev()
            .collect();

        if pinned_event_ids.is_empty() {
            return Ok(Vec::new());
        }

        let request_config = Some(
            RequestConfig::default()
                .retry_limit(3)
                .max_concurrent_requests(NonZeroUsize::new(MAX_CONCURRENT_REQUESTS)),
        );

        let new_events = join_all(pinned_event_ids.into_iter().map(|event_id| {
            let provider = self.room.clone();
            async move {
                match provider.load_event_with_relations(&event_id, request_config).await {
                    Ok((event, related_events)) => {
                        let mut events = vec![event];
                        events.extend(related_events);
                        Some(events)
                    }
                    Err(err) => {
                        warn!("error when loading pinned event: {err}");
                        None
                    }
                }
            }
        }))
        .await;

        let mut loaded_events = new_events
            .into_iter()
            // Get only the `Some<Vec<_>>` results
            .flatten()
            // Flatten the `Vec`s into a single one containing all their items
            .flatten()
            .collect::<Vec<SyncTimelineEvent>>();
        if loaded_events.is_empty() {
            return Err(PinnedEventsLoaderError::TimelineReloadFailed);
        }

        // Sort using chronological ordering (oldest -> newest)
        loaded_events.sort_by_key(|item| {
            item.event
                .deserialize()
                .map(|e| e.origin_server_ts())
                .unwrap_or_else(|_| MilliSecondsSinceUnixEpoch::now())
        });

        Ok(loaded_events)
    }
}

pub trait PinnedEventsRoom: SendOutsideWasm + SyncOutsideWasm {
    /// Load a single room event using the cache or network and any events
    /// related to it, if they are cached.
    fn load_event_with_relations<'a>(
        &'a self,
        event_id: &'a EventId,
        request_config: Option<RequestConfig>,
    ) -> BoxFuture<'a, Result<(SyncTimelineEvent, Vec<SyncTimelineEvent>), PaginatorError>>;

    /// Get the pinned event ids for a room.
    fn pinned_event_ids(&self) -> Vec<OwnedEventId>;

    /// Checks whether an event id is pinned in this room.
    ///
    /// It avoids having to clone the whole list of event ids to check a single
    /// value.
    fn is_pinned_event(&self, event_id: &EventId) -> bool;
}

impl PinnedEventsRoom for Room {
    fn load_event_with_relations<'a>(
        &'a self,
        event_id: &'a EventId,
        request_config: Option<RequestConfig>,
    ) -> BoxFuture<'a, Result<(SyncTimelineEvent, Vec<SyncTimelineEvent>), PaginatorError>> {
        async move {
            if let Ok((cache, _handles)) = self.event_cache().await {
                if let Some(ret) = cache.event_with_relations(event_id).await {
                    debug!("Loaded pinned event {event_id} and related events from cache");
                    return Ok(ret);
                }
            }

            debug!("Loading pinned event {event_id} from HS");
            self.event(event_id, request_config)
                .await
                .map(|e| (e.into(), Vec::new()))
                .map_err(|err| PaginatorError::SdkError(Box::new(err)))
        }
        .boxed()
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
