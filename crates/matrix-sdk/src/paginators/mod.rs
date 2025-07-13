// Copyright 2025 The Matrix.org Foundation C.I.C.
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

//! Stateful paginators to help with paginated APIs.

mod room;
pub mod thread;

use matrix_sdk_base::deserialized_responses::TimelineEvent;
pub use room::*;
use ruma::OwnedEventId;

/// Pagination token data, indicating in which state is the current pagination.
#[derive(Clone, Debug, PartialEq)]
pub enum PaginationToken {
    /// We never had a pagination token, so we'll start back-paginating from the
    /// end, or forward-paginating from the start.
    None,
    /// We paginated once before, and we received a prev/next batch token that
    /// we may reuse for the next query.
    HasMore(String),
    /// We've hit one end of the timeline (either the start or the actual end),
    /// so there's no need to continue paginating.
    HitEnd,
}

impl From<Option<String>> for PaginationToken {
    fn from(token: Option<String>) -> Self {
        match token {
            Some(val) => Self::HasMore(val),
            None => Self::None,
        }
    }
}

/// The result of a single event pagination.
#[derive(Debug)]
pub struct PaginationResult {
    /// Events returned during this pagination.
    ///
    /// If this is the result of a backward pagination, then the events are in
    /// reverse topological order.
    ///
    /// If this is the result of a forward pagination, then the events are in
    /// topological order.
    pub events: Vec<TimelineEvent>,

    /// Did we hit *an* end of the timeline?
    ///
    /// If this is the result of a backward pagination, this means we hit the
    /// *start* of the timeline.
    ///
    /// If this is the result of a forward pagination, this means we hit the
    /// *end* of the timeline.
    pub hit_end_of_timeline: bool,
}

/// An error that happened when using a [`Paginator`].
#[derive(Debug, thiserror::Error)]
pub enum PaginatorError {
    /// The target event could not be found.
    #[error("target event with id {0} could not be found")]
    EventNotFound(OwnedEventId),

    /// We're trying to manipulate the paginator in the wrong state.
    #[error("expected paginator state {expected:?}, observed {actual:?}")]
    InvalidPreviousState {
        /// The state we were expecting to see.
        expected: PaginatorState,
        /// The actual state when doing the check.
        actual: PaginatorState,
    },

    /// There was another SDK error while paginating.
    #[error("an error happened while paginating: {0}")]
    SdkError(#[from] Box<crate::Error>),
}
