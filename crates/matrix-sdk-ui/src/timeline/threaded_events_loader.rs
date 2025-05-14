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

use std::{fmt::Formatter, sync::Mutex};

use matrix_sdk::{
    event_cache::{
        paginator::{PaginationResult, PaginatorError},
        PaginationToken,
    },
    room::{IncludeRelations, RelationsOptions},
};
use ruma::{api::Direction, events::relation::RelationType, OwnedEventId, UInt};

use super::traits::RoomDataProvider;

#[derive(Debug)]
struct PaginationTokens {
    previous: PaginationToken,
    next: PaginationToken,
}

pub struct ThreadedEventsLoader<P: RoomDataProvider> {
    room: P,
    root_event_id: OwnedEventId,
    tokens: Mutex<PaginationTokens>,
}

impl<P: RoomDataProvider> ThreadedEventsLoader<P> {
    /// Create a new [`Paginator`], given a room implementation.
    pub fn new(room: P, root_event_id: OwnedEventId) -> Self {
        Self {
            room,
            root_event_id,
            tokens: Mutex::new(PaginationTokens { previous: None.into(), next: None.into() }),
        }
    }

    pub async fn paginate_backwards(
        &self,
        num_events: UInt,
    ) -> Result<PaginationResult, PaginatorError> {
        self.paginate(Direction::Backward, num_events).await
    }

    async fn paginate(
        &self,
        direction: Direction,
        num_events: UInt,
    ) -> Result<PaginationResult, PaginatorError> {
        let token = {
            let tokens = self.tokens.lock().unwrap();

            let token = match direction {
                Direction::Backward => &tokens.previous,
                Direction::Forward => &tokens.next,
            };

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
            dir: direction,
            limit: Some(num_events),
            include_relations: IncludeRelations::RelationsOfType(RelationType::Thread),
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

            let token = match direction {
                Direction::Backward => &mut tokens.previous,
                Direction::Forward => &mut tokens.next,
            };

            *token = match result.next_batch_token {
                Some(val) => PaginationToken::HasMore(val),
                None => PaginationToken::HitEnd,
            };
        }

        // Finally insert the thread root if at the end of the timeline going backwards
        if hit_end_of_timeline && direction == Direction::Backward {
            let root_event =
                self.room.load_event_with_relations(&self.root_event_id, None, None).await?.0;

            result.chunk.push(root_event);
        }

        Ok(PaginationResult { events: result.chunk, hit_end_of_timeline })
    }
}

#[cfg(not(tarpaulin_include))]
impl<P: RoomDataProvider> std::fmt::Debug for ThreadedEventsLoader<P> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadedEventsLoader").finish()
    }
}
