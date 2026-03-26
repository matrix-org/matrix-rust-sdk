// Copyright 2026 The Matrix.org Foundation C.I.C.
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

use std::collections::HashMap;

use matrix_sdk::{Client, Room, deserialized_responses::TimelineEvent};
use matrix_sdk_search::error::IndexError;
use ruma::{OwnedEventId, OwnedRoomId};

#[derive(thiserror::Error, Debug)]
pub enum SearchError {
    #[error(transparent)]
    IndexError(#[from] IndexError),
    #[error(transparent)]
    EventLoadError(#[from] matrix_sdk::Error),
}

const RESULTS_PER_PAGE: usize = 10;

pub struct RoomSearch {
    room: Room,
    query: String,
    current_offset: Option<usize>,
    is_done: bool,
}

impl RoomSearch {
    pub fn new(room: Room, query: String) -> Self {
        Self { room, query, current_offset: None, is_done: false }
    }

    /// Return the next batch of event IDs matching the search query, or `None`
    /// if there are no more results.
    pub async fn next(&mut self) -> Result<Option<Vec<OwnedEventId>>, IndexError> {
        if self.is_done {
            return Ok(None);
        }

        let result = self.room.search(&self.query, RESULTS_PER_PAGE, self.current_offset).await?;

        if result.is_empty() {
            self.is_done = true;
            Ok(None)
        } else {
            self.current_offset = Some(self.current_offset.unwrap_or(0) + result.len());
            Ok(Some(result))
        }
    }

    /// Returns [`TimelineEvent`]s instead of event IDs, by loading the events
    /// from the store or from network.
    pub async fn next_events(&mut self) -> Result<Option<Vec<TimelineEvent>>, SearchError> {
        let event_ids = match self.next().await? {
            Some(ids) => ids,
            None => return Ok(None),
        };
        let mut results = Vec::new();
        for event_id in event_ids {
            results.push(self.room.load_or_fetch_event(&event_id, None).await?);
        }
        Ok(Some(results))
    }
}

#[derive(Default)]
struct GlobalSearchRoomState {
    offset: Option<usize>,
    is_done: bool,
}

pub struct GlobalSearch {
    client: Client,
    query: String,
    offset_per_room: HashMap<OwnedRoomId, GlobalSearchRoomState>,
    is_done: bool,

    current_batch: Vec<(OwnedRoomId, OwnedEventId)>,
}

impl GlobalSearch {
    pub fn new(client: Client, query: String) -> Self {
        let offset_per_room = HashMap::from_iter(
            client
                // TODO allow filtering on room state (joined, left, etc.).
                // TODO allow filtering on DMs vs non-DMS to reduce the initial set.
                .joined_rooms()
                .into_iter()
                .map(|room| (room.room_id().to_owned(), GlobalSearchRoomState::default())),
        );

        Self { client, query, offset_per_room, is_done: false, current_batch: Vec::new() }
    }

    pub async fn next(&mut self) -> Result<Option<Vec<(OwnedRoomId, OwnedEventId)>>, SearchError> {
        if self.is_done {
            return Ok(None);
        }

        if !self.current_batch.is_empty() {
            let hi = RESULTS_PER_PAGE.min(self.current_batch.len());
            return Ok(Some(self.current_batch.drain(0..hi).collect()));
        }

        for (room_id, room_state) in &mut self.offset_per_room {
            if room_state.is_done {
                continue;
            }

            let Some(room) = self.client.get_room(room_id) else {
                continue;
            };

            // TODO: optimize, by only having a single async call.
            let room_results =
                room.search(&self.query, RESULTS_PER_PAGE, room_state.offset).await?;

            if room_results.is_empty() {
                room_state.is_done = true;
            } else {
                room_state.offset = Some(room_state.offset.unwrap_or(0) + room_results.len());
                self.current_batch
                    .extend(room_results.into_iter().map(|event_id| (room_id.clone(), event_id)));

                if self.current_batch.len() >= RESULTS_PER_PAGE {
                    // We have enough events to return now.
                    break;
                }
            }
        }

        if !self.current_batch.is_empty() {
            let hi = RESULTS_PER_PAGE.min(self.current_batch.len());
            Ok(Some(self.current_batch.drain(0..hi).collect()))
        } else {
            self.is_done = true;
            Ok(None)
        }
    }

    /// Returns [`TimelineEvent`]s instead of event IDs, by loading the events
    /// from the store or from network.
    pub async fn next_events(
        &mut self,
    ) -> Result<Option<Vec<(OwnedRoomId, TimelineEvent)>>, SearchError> {
        let event_ids = match self.next().await? {
            Some(ids) => ids,
            None => return Ok(None),
        };
        let mut results = Vec::new();
        for (room_id, event_id) in event_ids {
            let Some(room) = self.client.get_room(&room_id) else {
                continue;
            };
            results.push((room_id, room.load_or_fetch_event(&event_id, None).await?));
        }
        Ok(Some(results))
    }
}
