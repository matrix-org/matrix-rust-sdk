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

//! Messages search facilities and high-level helpers to perform searches across
//! one or multiple rooms, with pagination support.

use std::collections::HashSet;

use matrix_sdk_base::{RoomStateFilter, deserialized_responses::TimelineEvent};
use matrix_sdk_search::error::IndexError;
#[cfg(doc)]
use matrix_sdk_search::index::RoomIndex;
use ruma::{OwnedEventId, OwnedRoomId};

use crate::{Client, Room};

impl Room {
    /// Search this room's [`RoomIndex`] for query and return at most
    /// max_number_of_results results.
    pub async fn search(
        &self,
        query: &str,
        max_number_of_results: usize,
        pagination_offset: Option<usize>,
    ) -> Result<Vec<OwnedEventId>, IndexError> {
        let mut search_index_guard = self.client.search_index().lock().await;
        search_index_guard.search(query, max_number_of_results, pagination_offset, self.room_id())
    }
}

/// An error that can occur while searching messages, using the high-level
/// search helpers provided by this module provided by this module.
#[derive(thiserror::Error, Debug)]
pub enum SearchError {
    /// An error occurred while searching through the index for matching events.
    #[error(transparent)]
    IndexError(#[from] IndexError),
    /// An error occurred while loading the event content for a search result.
    #[error(transparent)]
    EventLoadError(#[from] crate::Error),
}

/// An async iterator for a search query in a single room.
#[derive(Debug)]
pub struct RoomSearchIterator {
    /// The room in which the search is performed.
    room: Room,

    /// The search query, directly forwarded to the search API.
    query: String,

    /// The current start offset in the search results, or `None` if we haven't
    /// called the iterator yet.
    offset: Option<usize>,

    /// Whether we have exhausted the search results.
    is_done: bool,

    /// Number of results to return (at most) per batch when calling
    /// [`Self::next()`].
    num_results_per_batch: usize,
}

impl RoomSearchIterator {
    /// Create a new search iterator for the given room and query.
    pub fn new(room: Room, query: String, num_results_per_batch: usize) -> Self {
        Self { room, query, offset: None, is_done: false, num_results_per_batch }
    }

    /// Return the next batch of event IDs matching the search query, or `None`
    /// if there are no more results.
    pub async fn next(&mut self) -> Result<Option<Vec<OwnedEventId>>, IndexError> {
        if self.is_done {
            return Ok(None);
        }

        // TODO: use the client/server API search endpoint for public rooms, as those
        // may require lots of time for indexing all events.
        let result = self.room.search(&self.query, self.num_results_per_batch, self.offset).await?;

        if result.is_empty() {
            self.is_done = true;
            Ok(None)
        } else {
            self.offset = Some(self.offset.unwrap_or(0) + result.len());
            Ok(Some(result))
        }
    }

    /// Returns [`TimelineEvent`]s instead of event IDs, by loading the events
    /// from the store or from network.
    pub async fn next_events(&mut self) -> Result<Option<Vec<TimelineEvent>>, SearchError> {
        let Some(event_ids) = self.next().await? else {
            return Ok(None);
        };
        let mut results = Vec::new();
        for event_id in event_ids {
            results.push(self.room.load_or_fetch_event(&event_id, None).await?);
        }
        Ok(Some(results))
    }
}

#[derive(Debug)]
struct GlobalSearchRoomState {
    /// The room for which we're storing state.
    room: Room,
    /// The current start offset in the search results for this room, or `None`
    /// if we haven't called the iterator for this room yet.
    offset: Option<usize>,
}

impl GlobalSearchRoomState {
    fn new(room: Room) -> Self {
        Self { room, offset: None }
    }
}

/// A builder for a [`GlobalSearchIterator`] that allows to configure the
/// initial working set of rooms to search in.
#[derive(Debug)]
pub struct GlobalSearchBuilder {
    client: Client,

    /// The search query, directly forwarded to the search API.
    query: String,

    /// Number of results to return (at most) per batch when calling
    /// [`GlobalSearchIterator::next()`].
    num_results_per_batch: usize,

    /// The working set of rooms to search in.
    room_set: Vec<Room>,
}

impl GlobalSearchBuilder {
    /// Create a new global search on all the joined rooms.
    fn new(client: Client, query: String, num_results_per_batch: usize) -> Self {
        let room_set = client.rooms_filtered(RoomStateFilter::JOINED);
        Self { client, query, room_set, num_results_per_batch }
    }

    /// Keep only the DM rooms from the initial working set.
    pub async fn only_dm_rooms(mut self) -> Result<Self, crate::Error> {
        let mut to_remove = HashSet::new();
        for room in &self.room_set {
            if !room.is_direct().await? {
                to_remove.insert(room.room_id().to_owned());
            }
        }
        self.room_set.retain(|room| !to_remove.contains(room.room_id()));
        Ok(self)
    }

    /// Keep only non-DM rooms (groups) from the initial working set.
    pub async fn no_dms(mut self) -> Result<Self, crate::Error> {
        let mut to_remove = HashSet::new();
        for room in &self.room_set {
            if room.is_direct().await? {
                to_remove.insert(room.room_id().to_owned());
            }
        }
        self.room_set.retain(|room| !to_remove.contains(room.room_id()));
        Ok(self)
    }

    /// Build the [`GlobalSearchIterator`] from this builder.
    pub fn build(self) -> GlobalSearchIterator {
        GlobalSearchIterator {
            client: self.client,
            query: self.query,
            room_state: Vec::from_iter(self.room_set.into_iter().map(GlobalSearchRoomState::new)),
            current_batch: Vec::new(),
            num_results_per_batch: self.num_results_per_batch,
        }
    }
}

/// An async iterator for a search query across multiple rooms.
#[derive(Debug)]
pub struct GlobalSearchIterator {
    client: Client,

    /// The search query, directly forwarded to the search API.
    query: String,

    /// The state for each room in the working list, that may still have
    /// results.
    ///
    /// This list is bound to shrink as we exhaust search results for each room,
    /// until it's empty and the overall iteration is done.
    room_state: Vec<GlobalSearchRoomState>,

    /// A buffer for the current batch of results across all rooms, so that we
    /// can return results in a more interleaved way instead of exhausting
    /// one room at a time.
    current_batch: Vec<(OwnedRoomId, OwnedEventId)>,

    /// Number of results to return (at most) per batch when calling
    /// [`Self::next()`].
    num_results_per_batch: usize,
}

impl GlobalSearchIterator {
    /// Create a new [`GlobalSearchBuilder`] for the given client and query, on
    /// all joined rooms by default.
    pub fn builder(
        client: Client,
        query: String,
        num_results_per_batch: usize,
    ) -> GlobalSearchBuilder {
        GlobalSearchBuilder::new(client, query, num_results_per_batch)
    }

    /// Return the next batch of event IDs matching the search query across all
    /// rooms, or `None` if there are no more results.
    pub async fn next(&mut self) -> Result<Option<Vec<(OwnedRoomId, OwnedEventId)>>, SearchError> {
        if self.room_state.is_empty() {
            return Ok(None);
        }

        // If there was enough results from a previous room iteration, return them
        // immediately.
        if self.current_batch.len() >= self.num_results_per_batch {
            return Ok(Some(self.current_batch.drain(0..self.num_results_per_batch).collect()));
        }

        let mut to_remove = HashSet::new();

        // Search across all non-done rooms for `num_results`, and accumulate them in
        // `Self::current_batch`.
        for room_state in &mut self.room_state {
            let room_results = room_state
                .room
                .search(&self.query, self.num_results_per_batch, room_state.offset)
                .await?;

            if room_results.is_empty() {
                // We've exhausted results for this room, mark it for removal.
                to_remove.insert(room_state.room.room_id().to_owned());
            } else {
                // Move the start offset for the room forward.
                room_state.offset = Some(room_state.offset.unwrap_or(0) + room_results.len());

                // Append the search results to the current batch.
                self.current_batch.extend(
                    room_results
                        .into_iter()
                        .map(|event_id| (room_state.room.room_id().to_owned(), event_id)),
                );

                if self.current_batch.len() >= self.num_results_per_batch {
                    // We have enough events to return now.
                    break;
                }
            }
        }

        // Delete rooms for which we've exhausted search results from the working list.
        for room_id in to_remove {
            self.room_state.retain(|room_state| room_state.room.room_id() != room_id);
        }

        if !self.current_batch.is_empty() {
            let high = self.num_results_per_batch.min(self.current_batch.len());
            Ok(Some(self.current_batch.drain(0..high).collect()))
        } else {
            debug_assert!(self.room_state.is_empty());
            Ok(None)
        }
    }

    /// Returns [`TimelineEvent`]s instead of event IDs, by loading the events
    /// from the store or from network.
    pub async fn next_events(
        &mut self,
    ) -> Result<Option<Vec<(OwnedRoomId, TimelineEvent)>>, SearchError> {
        let Some(event_ids) = self.next().await? else {
            return Ok(None);
        };
        let mut results = Vec::with_capacity(event_ids.len());
        for (room_id, event_id) in event_ids {
            let Some(room) = self.client.get_room(&room_id) else {
                continue;
            };
            results.push((room_id, room.load_or_fetch_event(&event_id, None).await?));
        }
        Ok(Some(results))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use matrix_sdk_test::{BOB, JoinedRoomBuilder, async_test, event_factory::EventFactory};
    use ruma::{event_id, room_id, user_id};

    use crate::{
        message_search::{GlobalSearchIterator, RoomSearchIterator},
        sleep::sleep,
        test_utils::mocks::MatrixMockServer,
    };

    #[async_test]
    async fn test_room_message_search() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let room_id = room_id!("!room_id:localhost");
        let room = server.sync_joined_room(&client, room_id).await;

        let f = EventFactory::new().room(room_id).sender(user_id!("@user_id:localhost"));

        let event_id = event_id!("$event_id:localhost");

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id)
                    .add_timeline_event(f.text_msg("hello world").event_id(event_id)),
            )
            .await;

        // Let the search indexer process the new event.
        sleep(Duration::from_millis(200)).await;

        // Search for a missing keyword.
        {
            let mut room_search =
                RoomSearchIterator::new(room.clone(), "search query".to_owned(), 5);

            // Searching for an event that's non-existing should succeed.
            let maybe_results = room_search.next().await.unwrap();
            assert!(maybe_results.is_none());

            // Calling the iterator after it's exhausted should still return `None` and not
            // error or return more results.
            let maybe_results = room_search.next().await.unwrap();
            assert!(maybe_results.is_none());
        }

        // Search for an existing keyword, by event id.
        {
            let mut room_search = RoomSearchIterator::new(room.clone(), "world".to_owned(), 5);

            // Searching for a keyword that matches an existing event should return the
            // event ID.
            let maybe_results = room_search.next().await.unwrap();
            let results = maybe_results.unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(&results[0], event_id,);

            // And no more results after that.
            let maybe_results = room_search.next().await.unwrap();
            assert!(maybe_results.is_none());
        }

        // Search for an existing keyword, by events.
        {
            let mut room_search = RoomSearchIterator::new(room.clone(), "world".to_owned(), 5);

            // Searching for a keyword that matches an existing event should return the
            // event ID.
            let maybe_results = room_search.next_events().await.unwrap();
            let results = maybe_results.unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].event_id().as_deref().unwrap(), event_id,);

            // And no more results after that.
            let maybe_results = room_search.next_events().await.unwrap();
            assert!(maybe_results.is_none());
        }
    }

    #[async_test]
    async fn test_global_message_search() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let room_id1 = room_id!("!r1:localhost");
        let room_id2 = room_id!("!r2:localhost");

        let f = EventFactory::new().sender(user_id!("@user_id:localhost"));

        let result_event_id1 = event_id!("$result1:localhost");
        let result_event_id2 = event_id!("$result2:localhost");

        server
            .mock_sync()
            .ok_and_run(&client, |sync_builder| {
                sync_builder
                    .add_joined_room(
                        JoinedRoomBuilder::new(room_id1)
                            .add_timeline_event(
                                f.text_msg("hello world").room(room_id1).event_id(result_event_id1),
                            )
                            .add_timeline_event(f.text_msg("hello back").room(room_id1)),
                    )
                    .add_joined_room(JoinedRoomBuilder::new(room_id2).add_timeline_event(
                        f.text_msg("it's a mad world").room(room_id2).event_id(result_event_id2),
                    ));
            })
            .await;

        // Let the search indexer process the new event.
        sleep(Duration::from_millis(200)).await;

        // Search for a missing keyword.
        {
            let mut search =
                GlobalSearchIterator::builder(client.clone(), "search query".to_owned(), 5).build();

            // Searching for an event that's non-existing should succeed.
            let maybe_results = search.next().await.unwrap();
            assert!(maybe_results.is_none());

            // Calling the iterator after it's exhausted should still return `None` and not
            // error or return more results.
            let maybe_results = search.next().await.unwrap();
            assert!(maybe_results.is_none());
        }

        // Search for an existing keyword, by event id.
        {
            let mut search =
                GlobalSearchIterator::builder(client.clone(), "world".to_owned(), 5).build();

            // Searching for a keyword that matches an existing event should return the
            // event ID.
            let maybe_results = search.next().await.unwrap();
            let results = maybe_results.unwrap();
            assert_eq!(results.len(), 2);
            // Search results order is not guaranteed, so we check that both expected
            // results are present in the returned batch.
            assert!(results.contains(&(room_id1.to_owned(), result_event_id1.to_owned())));
            assert!(results.contains(&(room_id2.to_owned(), result_event_id2.to_owned())));

            // And no more results after that.
            let maybe_results = search.next().await.unwrap();
            assert!(maybe_results.is_none());
        }

        // Search for an existing keyword, by event.
        {
            let mut search =
                GlobalSearchIterator::builder(client.clone(), "world".to_owned(), 5).build();

            // Searching for a keyword that matches an existing event should return the
            // event ID.
            let maybe_results = search.next_events().await.unwrap();
            let results = maybe_results.unwrap();
            assert_eq!(results.len(), 2);
            // Search results order is not guaranteed, so we check that both expected
            // results are present in the returned batch.
            assert!(results.iter().any(|(room_id, event)| {
                room_id == room_id1 && event.event_id().as_deref() == Some(result_event_id1)
            }));
            assert!(results.iter().any(|(room_id, event)| {
                room_id == room_id2 && event.event_id().as_deref() == Some(result_event_id2)
            }));

            // And no more results after that.
            let maybe_results = search.next_events().await.unwrap();
            assert!(maybe_results.is_none());
        }
    }

    #[async_test]
    async fn test_global_message_search_dm_or_groups() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        // This time, room_id1 is a DM room,
        let room_id1 = room_id!("!r1:localhost");
        // While room_id2 isn't.
        let room_id2 = room_id!("!r2:localhost");

        let f = EventFactory::new().sender(user_id!("@user_id:localhost"));

        let result_event_id1 = event_id!("$result1:localhost");
        let result_event_id2 = event_id!("$result2:localhost");

        server
            .mock_sync()
            .ok_and_run(&client, |sync_builder| {
                sync_builder
                    .add_joined_room(
                        JoinedRoomBuilder::new(room_id1)
                            .add_timeline_event(
                                f.text_msg("hello world").room(room_id1).event_id(result_event_id1),
                            )
                            .add_timeline_event(f.text_msg("hello back").room(room_id1)),
                    )
                    .add_joined_room(JoinedRoomBuilder::new(room_id2).add_timeline_event(
                        f.text_msg("it's a mad world").room(room_id2).event_id(result_event_id2),
                    ))
                    // Note: adding a DM room for room_id1 here.
                    .add_global_account_data(
                        f.direct().add_user((*BOB).to_owned().into(), room_id1),
                    );
            })
            .await;

        // Let the search indexer process the new event.
        sleep(Duration::from_millis(200)).await;

        // Search for an existing keyword, by event id, only in DMs.
        {
            let mut search = GlobalSearchIterator::builder(client.clone(), "world".to_owned(), 5)
                .only_dm_rooms()
                .await
                .unwrap()
                .build();

            let maybe_results = search.next().await.unwrap();
            let results = maybe_results.unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(&results[0], &(room_id1.to_owned(), result_event_id1.to_owned()));

            // And no more results after that.
            let maybe_results = search.next().await.unwrap();
            assert!(maybe_results.is_none());
        }

        // Search for an existing keyword, by event, only in groups.
        {
            let mut search = GlobalSearchIterator::builder(client.clone(), "world".to_owned(), 5)
                .no_dms()
                .await
                .unwrap()
                .build();

            let maybe_results = search.next_events().await.unwrap();
            let results = maybe_results.unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].0, room_id2);
            assert_eq!(results[0].1.event_id().as_deref().unwrap(), result_event_id2);

            // And no more results after that.
            let maybe_results = search.next().await.unwrap();
            assert!(maybe_results.is_none());
        }
    }
}
