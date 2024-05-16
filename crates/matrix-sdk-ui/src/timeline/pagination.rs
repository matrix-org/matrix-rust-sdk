// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use async_rx::StreamExt as _;
use futures_core::Stream;
use matrix_sdk::event_cache::{
    self,
    paginator::{PaginatorError, PaginatorState},
    BackPaginationOutcome, EventCacheError,
};
use tracing::{instrument, trace, warn};

use super::Error;
use crate::timeline::{event_item::RemoteEventOrigin, inner::TimelineEnd};

impl super::Timeline {
    /// Add more events to the start of the timeline.
    ///
    /// Returns whether we hit the start of the timeline.
    #[instrument(skip_all, fields(room_id = ?self.room().room_id()))]
    pub async fn paginate_backwards(&self, num_events: u16) -> Result<bool, Error> {
        if self.inner.is_live().await {
            Ok(self.live_paginate_backwards(num_events).await?)
        } else {
            Ok(self.focused_paginate_backwards(num_events).await?)
        }
    }

    /// Assuming the timeline is focused on an event, starts a forwards
    /// pagination.
    ///
    /// Returns whether we hit the end of the timeline.
    #[instrument(skip_all)]
    pub async fn focused_paginate_forwards(&self, num_events: u16) -> Result<bool, Error> {
        Ok(self.inner.focused_paginate_forwards(num_events).await?)
    }

    /// Assuming the timeline is focused on an event, starts a backwards
    /// pagination.
    ///
    /// Returns whether we hit the start of the timeline.
    #[instrument(skip(self), fields(room_id = ?self.room().room_id()))]
    pub async fn focused_paginate_backwards(&self, num_events: u16) -> Result<bool, Error> {
        Ok(self.inner.focused_paginate_backwards(num_events).await?)
    }

    /// Paginate backwards in live mode.
    ///
    /// This can only be called when the timeline is in live mode, not focused
    /// on a specific event.
    ///
    /// Returns whether we hit the start of the timeline.
    #[instrument(skip_all, fields(room_id = ?self.room().room_id()))]
    pub async fn live_paginate_backwards(&self, batch_size: u16) -> event_cache::Result<bool> {
        let pagination = self.event_cache.pagination();

        loop {
            let result = pagination.run_backwards(batch_size).await;

            let event_cache_outcome = match result {
                Ok(outcome) => outcome,

                Err(EventCacheError::BackpaginationError(
                    PaginatorError::InvalidPreviousState {
                        actual: PaginatorState::Paginating, ..
                    },
                )) => {
                    warn!("Another pagination request is already happening, returning early");
                    return Ok(false);
                }

                Err(err) => return Err(err),
            };

            let BackPaginationOutcome { events, reached_start } = event_cache_outcome;

            let num_events = events.len();
            trace!("Back-pagination succeeded with {num_events} events");

            self.inner
                .add_events_at(events, TimelineEnd::Front, RemoteEventOrigin::Pagination)
                .await;

            if reached_start {
                return Ok(true);
            }

            if num_events == 0 {
                // As an exceptional contract: if there were no events in the response,
                // and we've not hit the start of the timeline, retry until we get
                // some events or reach the start of the timeline.
                continue;
            }

            // Exit the inner loop, and ask for another limit.
            break;
        }

        Ok(false)
    }

    /// Subscribe to the back-pagination status of the timeline.
    ///
    /// Note: this may send multiple Paginating/Idle sequences during a single
    /// call to [`Self::paginate_backwards()`].
    pub fn back_pagination_status(&self) -> (PaginatorState, impl Stream<Item = PaginatorState>) {
        let mut status = self.event_cache.pagination().status();
        (status.next_now(), status.dedup())
    }
}
