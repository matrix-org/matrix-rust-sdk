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

use std::{fmt::Formatter, sync::Mutex};

use matrix_sdk::{
    event_cache::{
        paginator::{PaginationResult, PaginatorError},
        PaginationToken,
    },
    room::{IncludeRelations, RelationsOptions},
};
use ruma::{api::Direction, OwnedEventId, UInt};

use super::traits::RoomDataProvider;

pub struct ThreadedEventsLoader<P: RoomDataProvider> {
    room: P,
    root_event_id: OwnedEventId,
    token: Mutex<PaginationToken>,
}

impl<P: RoomDataProvider> ThreadedEventsLoader<P> {
    /// Create a new [`Paginator`], given a room implementation.
    pub fn new(room: P, root_event_id: OwnedEventId) -> Self {
        Self { room, root_event_id, token: Mutex::new(None.into()) }
    }

    pub async fn paginate_backwards(
        &self,
        num_events: UInt,
    ) -> Result<PaginationResult, PaginatorError> {
        let token = {
            let token = self.token.lock().unwrap();

            match &*token {
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
            let mut token = self.token.lock().unwrap();

            *token = match result.next_batch_token {
                Some(val) => PaginationToken::HasMore(val),
                None => PaginationToken::HitEnd,
            };
        }

        // Finally insert the thread root if at the end of the timeline going backwards
        if hit_end_of_timeline {
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
