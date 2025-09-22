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

//! Paginator facilities for a thread.
//!
//! See also the documentation for the [`ThreadedEventsLoader`] struct.

use std::{fmt::Formatter, future::Future, sync::Mutex};

use matrix_sdk_base::{SendOutsideWasm, SyncOutsideWasm, deserialized_responses::TimelineEvent};
use ruma::{EventId, OwnedEventId, UInt, api::Direction};

use crate::{
    Error, Room,
    paginators::{PaginationResult, PaginationToken, PaginationTokens, PaginatorError},
    room::{IncludeRelations, Relations, RelationsOptions},
};

/// A paginable thread interface, useful for testing purposes.
pub trait PaginableThread: SendOutsideWasm + SyncOutsideWasm {
    /// Runs a /relations query for the given thread, with the given options.
    fn relations(
        &self,
        thread_root: OwnedEventId,
        opts: RelationsOptions,
    ) -> impl Future<Output = Result<Relations, Error>> + SendOutsideWasm;

    /// Load an event, given its event ID.
    fn load_event(
        &self,
        event_id: &OwnedEventId,
    ) -> impl Future<Output = Result<TimelineEvent, Error>> + SendOutsideWasm;
}

impl PaginableThread for Room {
    async fn relations(
        &self,
        thread_root: OwnedEventId,
        opts: RelationsOptions,
    ) -> Result<Relations, Error> {
        self.relations(thread_root, opts).await
    }

    async fn load_event(&self, event_id: &OwnedEventId) -> Result<TimelineEvent, Error> {
        self.event(event_id, None).await
    }
}

/// A paginator for a thread of events.
pub struct ThreadedEventsLoader<P: PaginableThread> {
    /// Room provider for the paginated thread.
    room: P,

    /// The thread root event ID (the event that started the thread).
    root_event_id: OwnedEventId,

    /// The current pagination tokens, which are used to keep track of the
    /// pagination state.
    tokens: Mutex<PaginationTokens>,
}

impl<P: PaginableThread> ThreadedEventsLoader<P> {
    /// Create a new [`ThreadedEventsLoader`], given a room implementation.
    pub fn new(room: P, root_event_id: OwnedEventId, tokens: PaginationTokens) -> Self {
        Self { room, root_event_id, tokens: Mutex::new(tokens) }
    }

    /// Run a single pagination backwards, returning the next set of events and
    /// information whether we've reached the start of the thread.
    ///
    /// Note: when the thread start is reached, the root event *will* be
    /// included in the result.
    pub async fn paginate_backwards(
        &self,
        num_events: UInt,
    ) -> Result<PaginationResult, PaginatorError> {
        let token = {
            let token = &self.tokens.lock().unwrap().previous;

            match token {
                PaginationToken::None => None,
                PaginationToken::HasMore(token) => Some(token.clone()),
                PaginationToken::HitEnd => {
                    return Ok(PaginationResult { events: Vec::new(), hit_end_of_timeline: true });
                }
            }
        };

        let options = RelationsOptions {
            from: token,
            dir: Direction::Backward,
            limit: Some(num_events),
            include_relations: IncludeRelations::AllRelations,
            recurse: true,
        };

        let mut result = self
            .room
            .relations(self.root_event_id.to_owned(), options)
            .await
            .map_err(|error| PaginatorError::SdkError(Box::new(error)))?;

        let hit_end_of_timeline = result.next_batch_token.is_none();

        // Update the stored tokens
        {
            let mut tokens = self.tokens.lock().unwrap();

            tokens.previous = match result.next_batch_token {
                Some(val) => PaginationToken::HasMore(val),
                None => PaginationToken::HitEnd,
            };
        }

        // Finally insert the thread root if at the end of the timeline going backwards
        if hit_end_of_timeline {
            let root_event = self
                .room
                .load_event(&self.root_event_id)
                .await
                .map_err(|err| PaginatorError::SdkError(Box::new(err)))?;

            result.chunk.push(root_event);
        }

        Ok(PaginationResult { events: result.chunk, hit_end_of_timeline })
    }

    /// Run a single pagination forwards, returning the next set of events and
    /// information whether we've reached the end of the thread.
    pub async fn paginate_forwards(
        &self,
        num_events: UInt,
    ) -> Result<PaginationResult, PaginatorError> {
        let token = {
            let token = &self.tokens.lock().unwrap().next;

            match token {
                PaginationToken::None => None,
                PaginationToken::HasMore(token) => Some(token.clone()),
                PaginationToken::HitEnd => {
                    return Ok(PaginationResult { events: Vec::new(), hit_end_of_timeline: true });
                }
            }
        };

        let options = RelationsOptions {
            from: token,
            dir: Direction::Forward,
            limit: Some(num_events),
            include_relations: IncludeRelations::AllRelations,
            recurse: true,
        };

        let result = self
            .room
            .relations(self.root_event_id.to_owned(), options)
            .await
            .map_err(|error| PaginatorError::SdkError(Box::new(error)))?;

        let hit_end_of_timeline = result.next_batch_token.is_none();

        // Update the stored tokens
        {
            let mut tokens = self.tokens.lock().unwrap();

            tokens.next = match result.next_batch_token {
                Some(val) => PaginationToken::HasMore(val),
                None => PaginationToken::HitEnd,
            };
        }

        Ok(PaginationResult { events: result.chunk, hit_end_of_timeline })
    }

    /// Returns the root [`EventId`] for the thread.
    pub fn thread_root_event_id(&self) -> &EventId {
        &self.root_event_id
    }
}

#[cfg(not(tarpaulin_include))]
impl<P: PaginableThread> std::fmt::Debug for ThreadedEventsLoader<P> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadedEventsLoader").finish()
    }
}
