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

use std::{fmt, ops::ControlFlow, sync::Arc, time::Duration};

use matrix_sdk::event_cache::{self, BackPaginationOutcome};
use tracing::{debug, instrument, trace, warn};

use crate::timeline::inner::TimelineEnd;

impl super::Timeline {
    /// Add more events to the start of the timeline.
    #[instrument(skip_all, fields(room_id = ?self.room().room_id(), ?options))]
    pub async fn paginate_backwards(
        &self,
        mut options: PaginationOptions<'_>,
    ) -> event_cache::Result<()> {
        if self.back_pagination_status.get() == BackPaginationStatus::TimelineStartReached {
            warn!("Start of timeline reached, ignoring backwards-pagination request");
            return Ok(());
        }

        if self.back_pagination_status.set_if_not_eq(BackPaginationStatus::Paginating).is_none() {
            warn!("Another back-pagination is already running in the background");
            return Ok(());
        }

        // The first time, we allow to wait a bit for *a* back-pagination token to come
        // over via sync.
        const WAIT_FOR_TOKEN_TIMEOUT: Duration = Duration::from_secs(3);

        let mut token =
            self.event_cache.oldest_backpagination_token(Some(WAIT_FOR_TOKEN_TIMEOUT)).await?;

        let initial_options = options.clone();
        let mut outcome = PaginationOutcome::default();

        while let Some(batch_size) = options.next_event_limit(outcome) {
            loop {
                match self.event_cache.backpaginate_with_token(batch_size, token).await? {
                    BackPaginationOutcome::Success { events, reached_start } => {
                        let num_events = events.len();
                        trace!("Back-pagination succeeded with {num_events} events");

                        let handle_many_res =
                            self.inner.add_events_at(events, TimelineEnd::Front).await;

                        if reached_start {
                            self.back_pagination_status
                                .set_if_not_eq(BackPaginationStatus::TimelineStartReached);
                            return Ok(());
                        }

                        let mut update_outcome = || {
                            outcome.events_received = num_events.try_into().ok()?;
                            outcome.total_events_received = outcome
                                .total_events_received
                                .checked_add(outcome.events_received)?;

                            outcome.items_added = handle_many_res.items_added;
                            outcome.items_updated = handle_many_res.items_updated;
                            outcome.total_items_added += outcome.items_added;
                            outcome.total_items_updated += outcome.items_updated;

                            Some(())
                        };

                        if update_outcome().is_none() {
                            // Overflow: stop here and exit.
                            debug!(
                                "Hit overflow, stopping back-paginating and getting back to idle."
                            );
                            self.back_pagination_status.set_if_not_eq(BackPaginationStatus::Idle);
                            return Ok(());
                        }

                        if num_events == 0 {
                            // As an exceptional contract: if there were no events in the response,
                            // see if we had another back-pagination token, and retry the request.
                            token = self.event_cache.oldest_backpagination_token(None).await?;
                            continue;
                        }
                    }

                    BackPaginationOutcome::UnknownBackpaginationToken => {
                        // The token has been lost.
                        // It's possible the timeline has been cleared; restart the whole
                        // back-pagination.
                        outcome = Default::default();
                        options = initial_options.clone();
                    }
                }

                // Retrieve the next earliest back-pagination token.
                token = self.event_cache.oldest_backpagination_token(None).await?;

                // Exit the inner loop, and ask for another limit.
                break;
            }
        }

        self.back_pagination_status.set_if_not_eq(BackPaginationStatus::Idle);
        Ok(())
    }
}

/// Options for pagination.
#[derive(Clone)]
pub struct PaginationOptions<'a> {
    inner: PaginationOptionsInner<'a>,
    pub(super) wait_for_token: bool,
}

impl<'a> PaginationOptions<'a> {
    /// Do pagination requests until we receive some events, asking the server
    /// for the given maximum number of events.
    ///
    /// The server may choose to return fewer events, even if the start or end
    /// of the visible timeline is not yet reached.
    pub fn simple_request(event_limit: u16) -> Self {
        Self::new(PaginationOptionsInner::SingleRequest { event_limit_if_first: Some(event_limit) })
    }

    /// Continually paginate with a fixed `limit` until at least the given
    /// amount of timeline items have been added (unless the start or end of the
    /// visible timeline is reached).
    ///
    /// The `event_limit` represents the maximum number of events the server
    /// should return in one batch. It may choose to return fewer events per
    /// response.
    pub fn until_num_items(event_limit: u16, items: u16) -> Self {
        Self::new(PaginationOptionsInner::UntilNumItems { event_limit, items })
    }

    /// Paginate once with the given initial maximum number of events, then
    /// do more requests based on the user-provided strategy
    /// callback.
    ///
    /// The callback is given numbers on the events and resulting timeline
    /// items for the last request as well as summed over all
    /// requests in a `paginate_backwards` call, and can decide
    /// whether to do another request (by returning
    /// `ControlFlow::Continue(next_event_limit)`) or not (by returning
    /// `ControlFlow::Break(())`).
    pub fn custom(
        initial_event_limit: u16,
        pagination_strategy: impl Fn(PaginationOutcome) -> ControlFlow<(), u16> + Send + Sync + 'a,
    ) -> Self {
        Self::new(PaginationOptionsInner::Custom {
            event_limit_if_first: Some(initial_event_limit),
            strategy: Arc::new(pagination_strategy),
        })
    }

    /// Whether to wait for a pagination token to be set before starting.
    ///
    /// This is not something you should normally do since it can lead to very
    /// long wait times, however in the specific case of using sliding sync with
    /// the current proxy and subscribing to the room in a way that you know a
    /// sync will be coming in soon, it can be useful to reduce unnecessary
    /// traffic from duplicated events and avoid ordering issues from the sync
    /// proxy returning older data than pagination.
    pub fn wait_for_token(mut self) -> Self {
        self.wait_for_token = true;
        self
    }

    pub(super) fn next_event_limit(
        &mut self,
        pagination_outcome: PaginationOutcome,
    ) -> Option<u16> {
        match &mut self.inner {
            PaginationOptionsInner::SingleRequest { event_limit_if_first } => {
                event_limit_if_first.take()
            }
            PaginationOptionsInner::UntilNumItems { items, event_limit } => {
                (pagination_outcome.total_items_added < *items as u64).then_some(*event_limit)
            }
            PaginationOptionsInner::Custom { event_limit_if_first, strategy } => {
                event_limit_if_first.take().or_else(|| match strategy(pagination_outcome) {
                    ControlFlow::Continue(event_limit) => Some(event_limit),
                    ControlFlow::Break(_) => None,
                })
            }
        }
    }

    fn new(inner: PaginationOptionsInner<'a>) -> Self {
        Self { inner, wait_for_token: false }
    }
}

#[derive(Clone)]
pub enum PaginationOptionsInner<'a> {
    SingleRequest {
        event_limit_if_first: Option<u16>,
    },
    UntilNumItems {
        event_limit: u16,
        items: u16,
    },
    Custom {
        event_limit_if_first: Option<u16>,
        strategy: Arc<dyn Fn(PaginationOutcome) -> ControlFlow<(), u16> + Send + Sync + 'a>,
    },
}

#[cfg(not(tarpaulin_include))]
impl<'a> fmt::Debug for PaginationOptions<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            PaginationOptionsInner::SingleRequest { event_limit_if_first } => f
                .debug_struct("SingleRequest")
                .field("event_limit_if_first", event_limit_if_first)
                .finish(),
            PaginationOptionsInner::UntilNumItems { items, event_limit } => f
                .debug_struct("UntilNumItems")
                .field("items", items)
                .field("event_limit", event_limit)
                .finish(),
            PaginationOptionsInner::Custom { event_limit_if_first, .. } => f
                .debug_struct("Custom")
                .field("event_limit_if_first", event_limit_if_first)
                .finish(),
        }
    }
}

/// The result of a successful pagination request.
#[derive(Clone, Copy, Debug, Default)]
#[non_exhaustive]
pub struct PaginationOutcome {
    /// The number of events received in last pagination response.
    pub events_received: u16,

    /// The number of timeline items added by the last pagination response.
    pub items_added: u64,

    /// The number of timeline items updated by the last pagination
    /// response.
    pub items_updated: u64,

    /// The number of events received by a `paginate_backwards` call so far.
    pub total_events_received: u16,

    /// The total number of items added by a `paginate_backwards` call so
    /// far.
    pub total_items_added: u64,

    /// The total number of items updated by a `paginate_backwards` call so
    /// far.
    pub total_items_updated: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum BackPaginationStatus {
    Idle,
    Paginating,
    TimelineStartReached,
}

#[cfg(test)]
mod tests {
    use std::{
        ops::ControlFlow,
        sync::atomic::{AtomicU8, Ordering},
    };

    use super::{PaginationOptions, PaginationOutcome};

    fn bump_outcome(outcome: &mut PaginationOutcome) {
        outcome.events_received = 8;
        outcome.items_added = 6;
        outcome.items_updated = 1;
        outcome.total_events_received += 8;
        outcome.total_items_added += 6;
        outcome.total_items_updated += 1;
    }

    #[test]
    fn simple_request_limits() {
        let mut opts = PaginationOptions::simple_request(10);
        let mut outcome = PaginationOutcome::default();
        assert_eq!(opts.next_event_limit(outcome), Some(10));

        bump_outcome(&mut outcome);
        assert_eq!(opts.next_event_limit(outcome), None);
    }

    #[test]
    fn until_num_items_limits() {
        let mut opts = PaginationOptions::until_num_items(10, 10);
        let mut outcome = PaginationOutcome::default();
        assert_eq!(opts.next_event_limit(outcome), Some(10));

        bump_outcome(&mut outcome);
        assert_eq!(opts.next_event_limit(outcome), Some(10));

        bump_outcome(&mut outcome);
        assert_eq!(opts.next_event_limit(outcome), None);
    }

    #[test]
    fn custom_limits() {
        let num_calls = AtomicU8::new(0);
        let mut opts = PaginationOptions::custom(8, |outcome| {
            num_calls.fetch_add(1, Ordering::AcqRel);
            if outcome.total_items_added - outcome.total_items_updated < 6 {
                ControlFlow::Continue(12)
            } else {
                ControlFlow::Break(())
            }
        });
        let mut outcome = PaginationOutcome::default();
        assert_eq!(opts.next_event_limit(outcome), Some(8));

        bump_outcome(&mut outcome);
        assert_eq!(opts.next_event_limit(outcome), Some(12));

        bump_outcome(&mut outcome);
        assert_eq!(opts.next_event_limit(outcome), None);

        assert_eq!(num_calls.load(Ordering::Acquire), 2);
    }
}
