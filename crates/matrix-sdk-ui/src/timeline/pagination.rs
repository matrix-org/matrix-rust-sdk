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
use futures_util::{StreamExt as _, pin_mut};
use matrix_sdk::event_cache::{PaginationStatus, RoomEventCache};
use tracing::instrument;

use super::Error;
use crate::timeline::{
    PaginationError::NotSupported, controller::TimelineFocusKind, traits::RoomDataProvider as _,
};

impl super::Timeline {
    /// Add more events to the start of the timeline.
    ///
    /// Returns whether we hit the start of the timeline.
    #[instrument(skip_all, fields(room_id = ?self.room().room_id()))]
    pub async fn paginate_backwards(&self, mut num_events: u16) -> Result<bool, Error> {
        match self.controller.focus() {
            TimelineFocusKind::Live { event_cache, .. } => {
                match self.controller.live_lazy_paginate_backwards(num_events).await {
                    Some(needed_num_events) => {
                        num_events = needed_num_events.try_into().expect(
                            "failed to cast `needed_num_events` (`usize`) into `num_events` (`usize`)",
                        );
                    }
                    None => {
                        // We could adjust the skip count to a lower value, while passing the
                        // requested number of events. We *may* have reached the start of the
                        // timeline, but since we're fulfilling the caller's request, assume it's
                        // not the case and return false here. A subsequent call will go to the
                        // `Some()` arm of this match, and cause a call to the event cache's
                        // pagination.
                        return Ok(false);
                    }
                }

                Ok(self.live_paginate_backwards(event_cache, num_events).await?)
            }

            TimelineFocusKind::Event { event_cache, .. } => {
                Ok(event_cache.paginate_backwards(num_events).await?.hit_end_of_timeline)
            }

            TimelineFocusKind::Thread { event_cache, .. } => {
                // Same deal as `live_paginate_backwards`: in storage-only mode,
                // the cached thread is served first, gaps become gap items
                // resolved on demand with [`Self::resolve_gap`], and the
                // network is only reached once the storage is exhausted.
                let storage_only = self.controller.settings.storage_only_pagination;
                let pagination = event_cache.pagination();

                loop {
                    let outcome = if storage_only {
                        pagination.run_backwards_once_from_storage(num_events).await?
                    } else {
                        pagination.run_backwards_once(num_events).await?
                    };

                    if outcome.reached_start {
                        // The timeline start item is inserted by the thread
                        // updates task, once the fetched events (ending with
                        // the thread root) have been applied to the timeline.
                        return Ok(true);
                    }

                    if !outcome.events.is_empty() {
                        return Ok(false);
                    }

                    // As a special contract, restart a pagination that
                    // returned 0 events (a page of already-known events, a
                    // gap chunk loaded from the storage…): each run makes
                    // progress towards the start of the thread.
                }
            }

            TimelineFocusKind::PinnedEvents { .. } => Err(Error::PaginationError(NotSupported)),

            TimelineFocusKind::MessageTypes { event_cache } => {
                // Exposes older events from the store, then resolves the
                // room's gaps back to the next matching event; the diffs (and
                // the timeline start, once the room's start is reached) reach
                // the timeline through the view's updates.
                Ok(event_cache.paginate_backwards(num_events.into()).await?)
            }
        }
    }

    /// Add more events to the end of the timeline.
    ///
    /// Returns whether we hit the end of the timeline.
    #[instrument(skip_all, fields(room_id = ?self.room().room_id()))]
    pub async fn paginate_forwards(&self, num_events: u16) -> Result<bool, Error> {
        match self.controller.focus() {
            TimelineFocusKind::Live { .. } => Ok(true),

            TimelineFocusKind::Event { event_cache, .. } => {
                Ok(event_cache.paginate_forwards(num_events).await?.hit_end_of_timeline)
            }

            TimelineFocusKind::Thread { .. } | TimelineFocusKind::PinnedEvents { .. } => {
                Err(Error::PaginationError(NotSupported))
            }

            TimelineFocusKind::MessageTypes { event_cache } => {
                Ok(event_cache.paginate_forwards(num_events.into()).await?)
            }
        }
    }

    /// Paginate backwards in live mode.
    ///
    /// This can only be called when the timeline is in live mode, not focused
    /// on a specific event.
    ///
    /// Returns whether we hit the start of the timeline.
    async fn live_paginate_backwards(
        &self,
        event_cache: &RoomEventCache,
        batch_size: u16,
    ) -> Result<bool, Error> {
        let event_cache_pagination = event_cache.pagination();
        // In storage-only mode, gaps encountered while walking the storage
        // are surfaced as gap timeline items instead of being resolved over
        // the network, so all the cached content is reachable even offline.
        // Gaps are then resolved on demand with [`Timeline::resolve_gap`].
        let storage_only = self.controller.settings.storage_only_pagination;

        // Storage loads are cheap and whole-chunk: for a filtered timeline
        // (e.g. media only), a chunk often yields no item at all, and having
        // the client come back for every chunk makes loading crawl. Keep
        // walking until a loaded chunk holds at least one event that passes
        // the timeline's filter, within a budget. (Checked on the loaded
        // events rather than on the items: the timeline applies the diffs
        // asynchronously.)
        const MAX_STORAGE_CHUNKS_PER_CALL: usize = 32;
        let mut storage_chunks = 0;
        let rules = self.controller.room_data_provider.room_version_rules();
        let passes_filter = |events: &[matrix_sdk::deserialized_responses::TimelineEvent]| {
            events.iter().any(|event| {
                event
                    .raw()
                    .deserialize()
                    .is_ok_and(|event| (self.controller.settings.event_filter)(&event, &rules))
            })
        };

        loop {
            let result = if storage_only {
                event_cache_pagination.run_backwards_once_from_storage(batch_size).await
            } else {
                event_cache_pagination.run_backwards_once(batch_size).await
            };

            match result {
                Ok(outcome) => {
                    if outcome.reached_start {
                        // In storage-only mode, "reached start" may only mean
                        // "exhausted the storage, up to a leading gap": pick
                        // up the gaps synchronously so the timeline start
                        // decision doesn't race with the gaps update.
                        self.controller.refresh_timeline_gaps(event_cache).await;
                        self.controller.insert_timeline_start_if_missing().await;
                        return Ok(true);
                    }

                    if !outcome.events.is_empty() {
                        if storage_only {
                            storage_chunks += 1;
                            if storage_chunks < MAX_STORAGE_CHUNKS_PER_CALL
                                && !passes_filter(&outcome.events)
                            {
                                // Everything in this chunk is filtered out of
                                // this timeline: keep walking.
                                continue;
                            }
                        }

                        return Ok(false);
                    }

                    // Fallthrough: as a special contract, restart pagination,
                    // if it returned 0 events.
                }

                // Propagate errors as such.
                Err(err) => return Err(err.into()),
            }
        }
    }

    /// Resolve the timeline gap identified by the given prev-batch token (as
    /// carried by a [`VirtualTimelineItem::Gap`] item), fetching up to
    /// `batch_size` of the missing events with a single request to the server.
    ///
    /// The fetched events replace the gap item in place; if the gap was only
    /// partially resolved, a gap item with a new token remains, and can be
    /// resolved in turn.
    ///
    /// Concurrent resolutions of the same gap are deduplicated: only the
    /// first runs, the others return `false` immediately. So it's fine to
    /// call this whenever a gap item is visible, repeatedly.
    ///
    /// Supported on live, thread and message-type filtered timelines.
    ///
    /// [`VirtualTimelineItem::Gap`]: crate::timeline::VirtualTimelineItem::Gap
    pub async fn resolve_gap(&self, prev_token: String, batch_size: u16) -> Result<bool, Error> {
        match self.controller.focus() {
            TimelineFocusKind::Live { event_cache, .. } => {
                // No synchronous gaps refresh here, unlike after a pagination:
                // it would race the event subscriber task and remove the gap
                // item in a transaction of its own, before the fetched events
                // land (a visible jump). The events update carries the new
                // gaps snapshot in the same transaction, and the trailing
                // `UpdateTimelineGaps` update settles the timeline start (see
                // `TimelineController::handle_timeline_gaps`).
                Ok(event_cache.resolve_gap(prev_token, batch_size).await.map_err(Error::from)?)
            }

            TimelineFocusKind::MessageTypes { event_cache } => {
                Ok(event_cache.resolve_gap(prev_token, batch_size).await?)
            }

            TimelineFocusKind::Thread { event_cache, .. } => {
                // Same as live: the events update carries the gaps snapshot
                // (pulled by the thread updates task) in the same transaction.
                Ok(event_cache.resolve_gap(prev_token, batch_size).await?)
            }

            TimelineFocusKind::Event { .. } | TimelineFocusKind::PinnedEvents { .. } => {
                Err(Error::PaginationError(NotSupported))
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
    ) -> Option<(PaginationStatus, impl Stream<Item = PaginationStatus> + use<>)> {
        let TimelineFocusKind::Live { event_cache, .. } = self.controller.focus() else {
            return None;
        };

        let pagination = event_cache.pagination();

        let mut status = pagination.status();

        let current_value = self.controller.map_pagination_status(status.next_now()).await;

        let controller = self.controller.clone();
        let event_cache = event_cache.clone();
        let stream = Box::pin(stream! {
            let status_stream = status.dedup();

            pin_mut!(status_stream);

            while let Some(state) = status_stream.next().await {
                let state = controller.map_pagination_status(state).await;

                match state {
                    PaginationStatus::Idle { hit_timeline_start } => {
                        if hit_timeline_start {
                            controller.refresh_timeline_gaps(&event_cache).await;
                            controller.insert_timeline_start_if_missing().await;
                        }
                    }
                    PaginationStatus::Paginating => {}
                }

                yield state;
            }
        });

        Some((current_value, stream))
    }
}
