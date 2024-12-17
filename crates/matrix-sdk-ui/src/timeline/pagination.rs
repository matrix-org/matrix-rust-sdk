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

use std::ops::ControlFlow;

use async_rx::StreamExt as _;
use async_stream::stream;
use futures_core::Stream;
use futures_util::{pin_mut, StreamExt as _};
use matrix_sdk::event_cache::{
    self,
    paginator::{PaginatorError, PaginatorState},
    BackPaginationOutcome, EventCacheError, RoomPagination,
};
use tracing::{instrument, trace, warn};

use super::Error;
use crate::timeline::{controller::TimelineNewItemPosition, event_item::RemoteEventOrigin};

impl super::Timeline {
    /// Add more events to the start of the timeline.
    ///
    /// Returns whether we hit the start of the timeline.
    #[instrument(skip_all, fields(room_id = ?self.room().room_id()))]
    pub async fn paginate_backwards(&self, num_events: u16) -> Result<bool, Error> {
        if self.controller.is_live().await {
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
        Ok(self.controller.focused_paginate_forwards(num_events).await?)
    }

    /// Assuming the timeline is focused on an event, starts a backwards
    /// pagination.
    ///
    /// Returns whether we hit the start of the timeline.
    #[instrument(skip(self), fields(room_id = ?self.room().room_id()))]
    pub async fn focused_paginate_backwards(&self, num_events: u16) -> Result<bool, Error> {
        Ok(self.controller.focused_paginate_backwards(num_events).await?)
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

        let result = pagination
            .run_backwards(
                batch_size,
                |BackPaginationOutcome { events, reached_start },
                 _timeline_has_been_reset| async move {
                    let num_events = events.len();
                    trace!("Back-pagination succeeded with {num_events} events");

                    // If `TimelineSettings::vectordiffs_as_inputs` is enabled,
                    // we don't need to add events manually: everything we need
                    // is to let the `EventCache` receives the events from this
                    // pagination, and emits its updates as `VectorDiff`s, which
                    // will be handled by the `Timeline` naturally.
                    if !self.controller.settings.vectordiffs_as_inputs {
                        self.controller
                            .add_events_at(events.into_iter(), TimelineNewItemPosition::Start { origin: RemoteEventOrigin::Pagination })
                            .await;
                    }

                    if num_events == 0 && !reached_start {
                        // As an exceptional contract: if there were no events in the response,
                        // and we've not hit the start of the timeline, retry until we get
                        // some events or reach the start of the timeline.
                        return ControlFlow::Continue(());
                    }

                    ControlFlow::Break(reached_start)
                },
            )
            .await;

        match result {
            Err(EventCacheError::BackpaginationError(PaginatorError::InvalidPreviousState {
                actual: PaginatorState::Paginating,
                ..
            })) => {
                warn!("Another pagination request is already happening, returning early");
                Ok(false)
            }

            result => result,
        }
    }

    /// Subscribe to the back-pagination status of a live timeline.
    ///
    /// This will return `None` if the timeline is in the focused mode.
    ///
    /// Note: this may send multiple Paginating/Idle sequences during a single
    /// call to [`Self::paginate_backwards()`].
    pub async fn live_back_pagination_status(
        &self,
    ) -> Option<(LiveBackPaginationStatus, impl Stream<Item = LiveBackPaginationStatus>)> {
        if !self.controller.is_live().await {
            return None;
        }

        let pagination = self.event_cache.pagination();

        let mut status = pagination.status();

        let current_value =
            LiveBackPaginationStatus::from_paginator_status(&pagination, status.next_now());

        let stream = Box::pin(stream! {
            let status_stream = status.dedup();

            pin_mut!(status_stream);

            while let Some(state) = status_stream.next().await {
                yield LiveBackPaginationStatus::from_paginator_status(&pagination, state);
            }
        });

        Some((current_value, stream))
    }
}

/// Status for the back-pagination on a live timeline.
#[derive(Debug, PartialEq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum LiveBackPaginationStatus {
    /// No back-pagination is happening right now.
    Idle {
        /// Have we hit the start of the timeline, i.e. back-paginating wouldn't
        /// have any effect?
        hit_start_of_timeline: bool,
    },

    /// Back-pagination is already running in the background.
    Paginating,
}

impl LiveBackPaginationStatus {
    /// Converts from a [`PaginatorState`] into the live back-pagination status.
    ///
    /// Private method instead of `From`/`Into` impl, to avoid making it public
    /// API.
    fn from_paginator_status(pagination: &RoomPagination, state: PaginatorState) -> Self {
        match state {
            PaginatorState::Initial => Self::Idle { hit_start_of_timeline: false },
            PaginatorState::FetchingTargetEvent => {
                panic!("unexpected paginator state for a live backpagination")
            }
            PaginatorState::Idle => {
                Self::Idle { hit_start_of_timeline: pagination.hit_timeline_start() }
            }
            PaginatorState::Paginating => Self::Paginating,
        }
    }
}
