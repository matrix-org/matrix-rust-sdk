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

use std::{fmt, ops::ControlFlow, pin::pin, sync::Arc, time::Duration};

use matrix_sdk::{room::MessagesOptions, Result};
use matrix_sdk_base::timeout::timeout;
use ruma::assign;
use tracing::{debug, error, info, instrument, trace, warn};

use super::{inner::HandleBackPaginatedEventsError, Timeline};

impl Timeline {
    /// Run back-pagination.
    ///
    /// Returns `Ok(ControlFlow::Continue(()))` if back-pagination should be
    /// retried because the timeline was reset while a pagination request was
    /// in-flight.
    ///
    /// Returns `Ok(ControlFlow::Break(status))` if back-pagination succeeded,
    /// where `status` is the resulting back-pagination status, either
    /// [`Idle`][BackPaginationStatus::Idle] or
    /// [`TimelineStartReached`][BackPaginationStatus::TimelineStartReached].
    ///
    /// Returns `Err(_)` if the a pagination request failed. This doesn't mean
    /// that no events were added to the timeline though, it is possible that
    /// one or more pagination requests succeeded before the failure.
    pub(super) async fn paginate_backwards_impl(
        &self,
        mut options: PaginationOptions<'_>,
    ) -> Result<ControlFlow<BackPaginationStatus>> {
        // How long to wait for the back-pagination token to be set if the
        // `wait_for_token` option is set
        const WAIT_FOR_TOKEN_TIMEOUT: Duration = Duration::from_secs(3);

        let mut from = match self.inner.back_pagination_token().await {
            None if options.wait_for_token => {
                trace!("Waiting for back-pagination token from sync...");

                let wait_for_token = pin!(async {
                    loop {
                        self.sync_response_notify.notified().await;
                        match self.inner.back_pagination_token().await {
                            Some(token) => break token,
                            None => {
                                warn!(
                                    "Sync response without prev_batch received, \
                                     continuing to wait"
                                );
                                // fall through and continue the loop
                            }
                        }
                    }
                });

                match timeout(wait_for_token, WAIT_FOR_TOKEN_TIMEOUT).await {
                    Ok(token) => Some(token),
                    Err(_) => {
                        debug!("Waiting for prev_batch token timed out after 3s");
                        None
                    }
                }
            }
            token => token,
        };

        self.back_pagination_status.set_if_not_eq(BackPaginationStatus::Paginating);

        let mut outcome = PaginationOutcome::new();
        while let Some(limit) = options.next_event_limit(outcome) {
            match self.paginate_backwards_once(limit, from, &mut outcome).await? {
                PaginateBackwardsOnceResult::Success { from: None } => {
                    trace!("Start of timeline was reached");
                    return Ok(ControlFlow::Break(BackPaginationStatus::TimelineStartReached));
                }
                PaginateBackwardsOnceResult::Success { from: Some(f) } => {
                    from = Some(f);
                    // fall through and continue the loop
                }
                PaginateBackwardsOnceResult::TokenMismatch => {
                    info!("Head of timeline was altered since pagination was started, resetting");
                    return Ok(ControlFlow::Continue(()));
                }
                PaginateBackwardsOnceResult::ResultOverflow => {
                    error!("Received an excessive number of events, ending pagination");
                    break;
                }
            };
        }

        Ok(ControlFlow::Break(BackPaginationStatus::Idle))
    }

    /// Do a single back-pagination request.
    ///
    /// Returns `Ok(ControlFlow::Continue(true))` if back-pagination should be
    /// retried because the timeline was reset while a pagination request
    /// was in-flight.
    ///
    /// Returns `Ok(ControlFlow::Break(status))` if back-pagination succeeded,
    /// where `status` is the resulting back-pagination status, either
    /// [`Idle`][BackPaginationStatus::Idle] or
    /// [`TimelineStartReached`][BackPaginationStatus::TimelineStartReached].
    ///
    /// Returns `Err(_)` if the a pagination request failed. This doesn't mean
    /// that no events were added to the timeline though, it is possible that
    /// one or more pagination requests succeeded before the failure.
    #[instrument(skip(self, outcome))]
    async fn paginate_backwards_once(
        &self,
        limit: u16,
        from: Option<String>,
        outcome: &mut PaginationOutcome,
    ) -> Result<PaginateBackwardsOnceResult> {
        trace!("Requesting messages");

        let messages = self
            .room()
            .messages(assign!(MessagesOptions::backward(), {
                from: from.clone(),
                limit: limit.into(),
            }))
            .await?;
        let chunk_len = messages.chunk.len();

        let tokens = PaginationTokens { from, to: messages.end.clone() };
        let res = match self.inner.handle_back_paginated_events(messages.chunk, tokens).await {
            Ok(result) => result,
            Err(HandleBackPaginatedEventsError::TokenMismatch) => {
                return Ok(PaginateBackwardsOnceResult::TokenMismatch);
            }
            Err(HandleBackPaginatedEventsError::ResultOverflow) => {
                return Ok(PaginateBackwardsOnceResult::ResultOverflow);
            }
        };

        // FIXME: Change to try block once stable
        let mut update_outcome = || {
            outcome.events_received = chunk_len.try_into().ok()?;
            outcome.total_events_received =
                outcome.total_events_received.checked_add(outcome.events_received)?;

            outcome.items_added = res.items_added;
            outcome.items_updated = res.items_updated;
            outcome.total_items_added =
                outcome.total_items_added.checked_add(outcome.items_added)?;
            outcome.total_items_updated =
                outcome.total_items_updated.checked_add(outcome.items_updated)?;

            Some(())
        };

        Ok(match update_outcome() {
            Some(()) => PaginateBackwardsOnceResult::Success { from: messages.end },
            None => PaginateBackwardsOnceResult::ResultOverflow,
        })
    }
}

/// Options for pagination.
#[derive(Clone)]
pub struct PaginationOptions<'a> {
    inner: PaginationOptionsInner<'a>,
    pub(super) wait_for_token: bool,
}

impl<'a> PaginationOptions<'a> {
    /// Do a single pagination request, asking the server for the given
    /// maximum number of events.
    ///
    /// The server may choose to return fewer events, even if the start or end
    /// of the visible timeline is not yet reached.
    pub fn single_request(event_limit: u16) -> Self {
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
        pagination_strategy: impl Fn(PaginationOutcome) -> ControlFlow<(), u16> + Send + 'a,
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
                (pagination_outcome.total_items_added < *items).then_some(*event_limit)
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
        strategy: Arc<dyn Fn(PaginationOutcome) -> ControlFlow<(), u16> + Send + 'a>,
    },
}

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
    pub items_added: u16,

    /// The number of timeline items updated by the last pagination
    /// response.
    pub items_updated: u16,

    /// The number of events received by a `paginate_backwards` call so far.
    pub total_events_received: u16,

    /// The total number of items added by a `paginate_backwards` call so
    /// far.
    pub total_items_added: u16,

    /// The total number of items updated by a `paginate_backwards` call so
    /// far.
    pub total_items_updated: u16,
}

impl PaginationOutcome {
    pub(super) fn new() -> Self {
        Self {
            events_received: 0,
            items_added: 0,
            items_updated: 0,
            total_events_received: 0,
            total_items_added: 0,
            total_items_updated: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackPaginationStatus {
    Idle,
    Paginating,
    TimelineStartReached,
}

#[derive(Default)]
pub(super) struct PaginationTokens {
    /// The `from` parameter of the pagination request.
    pub from: Option<String>,
    /// The `end` parameter of the pagination response.
    pub to: Option<String>,
}

/// The `Ok` result of `paginate_backwards_once`.
enum PaginateBackwardsOnceResult {
    /// Success, the items from the response were prepended.
    Success { from: Option<String> },
    /// The `from` token is not equal to the first event item's back-pagination
    /// token.
    ///
    /// This means that prepending the events from the back-pagination response
    /// would result in a gap in the timeline. Back-pagination must be retried
    /// with the current back-pagination token.
    TokenMismatch,
    /// Overflow in reporting the number of events / items processed.
    ResultOverflow,
}
