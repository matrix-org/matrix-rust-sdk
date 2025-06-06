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
use async_stream::stream;
use futures_core::Stream;
use futures_util::{pin_mut, StreamExt as _};
use matrix_sdk::event_cache::{self, EventCacheError, RoomPaginationStatus};
use tracing::{instrument, warn};

use super::Error;

impl super::Timeline {
    /// Add more events to the start of the timeline.
    ///
    /// Returns whether we hit the start of the timeline.
    #[instrument(skip_all, fields(room_id = ?self.room().room_id()))]
    pub async fn paginate_backwards(&self, mut num_events: u16) -> Result<bool, Error> {
        if self.controller.is_live().await {
            match self.controller.live_lazy_paginate_backwards(num_events).await {
                Some(needed_num_events) => {
                    num_events = needed_num_events.try_into().expect(
                        "failed to cast `needed_num_events` (`usize`) into `num_events` (`usize`)",
                    );
                }
                None => {
                    // We could adjust the skip count to a lower value, while passing the requested
                    // number of events. We *may* have reached the start of the timeline, but since
                    // we're fulfilling the caller's request, assume it's not the case and return
                    // false here. A subsequent call will go to the `Some()` arm of this match, and
                    // cause a call to the event cache's pagination.
                    return Ok(false);
                }
            }

            Ok(self.live_paginate_backwards(num_events).await?)
        } else {
            Ok(self.controller.focused_paginate_backwards(num_events).await?)
        }
    }

    /// Add more events to the end of the timeline.
    ///
    /// Returns whether we hit the end of the timeline.
    #[instrument(skip_all, fields(room_id = ?self.room().room_id()))]
    pub async fn paginate_forwards(&self, num_events: u16) -> Result<bool, Error> {
        if self.controller.is_live().await {
            Ok(true)
        } else {
            Ok(self.controller.focused_paginate_forwards(num_events).await?)
        }
    }

    /// Paginate backwards in live mode.
    ///
    /// This can only be called when the timeline is in live mode, not focused
    /// on a specific event.
    ///
    /// Returns whether we hit the start of the timeline.
    async fn live_paginate_backwards(&self, batch_size: u16) -> event_cache::Result<bool> {
        loop {
            match self.event_cache.pagination().run_backwards_once(batch_size).await {
                Ok(outcome) => {
                    // As an exceptional contract, restart the back-pagination if we received an
                    // empty chunk.
                    if outcome.reached_start || !outcome.events.is_empty() {
                        if outcome.reached_start {
                            self.controller.insert_timeline_start_if_missing().await;
                        }
                        return Ok(outcome.reached_start);
                    }
                }

                Err(EventCacheError::AlreadyBackpaginating) => {
                    // Treat an already running pagination exceptionally, returning false so that
                    // the caller retries later.
                    warn!("Another pagination request is already happening, returning early");
                    return Ok(false);
                }

                // Propagate other errors as such.
                Err(err) => return Err(err),
            }
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
    ) -> Option<(RoomPaginationStatus, impl Stream<Item = RoomPaginationStatus>)> {
        if !self.controller.is_live().await {
            return None;
        }

        let pagination = self.event_cache.pagination();

        let mut status = pagination.status();

        let current_value = status.next_now();

        let controller = self.controller.clone();
        let stream = Box::pin(stream! {
            let status_stream = status.dedup();

            pin_mut!(status_stream);

            while let Some(state) = status_stream.next().await {
                match state {
                    RoomPaginationStatus::Idle { hit_timeline_start } => {
                        if hit_timeline_start {
                            controller.insert_timeline_start_if_missing().await;
                        }
                    }
                    RoomPaginationStatus::Paginating => {}
                }
                yield state;
            }
        });

        Some((current_value, stream))
    }
}
