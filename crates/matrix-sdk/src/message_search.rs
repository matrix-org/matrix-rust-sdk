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
//! one or multiple rooms.
//!
//! These helpers expose the results as [`Stream`]s of pages, lazily fetching
//! the next page from the underlying index as the stream is polled. Use the
//! [`StreamExt`] and [`TryStreamExt`] combinators (`next`, `try_concat`,
//! `take`, …) to consume them.
//!
//! [`StreamExt`]: futures_util::StreamExt
//! [`TryStreamExt`]: futures_util::TryStreamExt
//!
//! # Examples
//!
//! ## Searching within a single room
//!
//! Use [`Room::search_messages`] to get a stream of pages of `(score,
//! event_id)` pairs, or [`Room::search_messages_events`] to load the full
//! [`TimelineEvent`]s.
//!
//! ```no_run
//! # use matrix_sdk::Room;
//! # use futures_util::StreamExt as _;
//! # async fn example(room: Room) -> anyhow::Result<()> {
//! let mut stream = Box::pin(room.search_messages("hello world".to_owned()));
//!
//! while let Some(page) = stream.next().await {
//!     for (score, event_id) in page? {
//!         println!("Found event {event_id} (score: {score})");
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! ## Searching across all joined rooms
//!
//! Use [`Client::search_messages`] to create a [`GlobalSearchBuilder`].
//! Optionally restrict the working set to DM rooms (or non-DM rooms) before
//! calling [`GlobalSearchBuilder::build`] to get a stream of pages of results,
//! sorted by relevance score across all rooms. Use
//! [`GlobalSearchBuilder::build_events`] to load full [`TimelineEvent`]s
//! instead of plain event IDs.
//!
//! ```no_run
//! # use matrix_sdk::Client;
//! # use futures_util::StreamExt as _;
//! # async fn example(client: Client) -> anyhow::Result<()> {
//! // Search only in DM rooms.
//! let mut stream = Box::pin(
//!     client
//!         .search_messages("hello world".to_owned())
//!         .only_dm_rooms()
//!         .await?
//!         .build_events(),
//! );
//!
//! while let Some(page) = stream.next().await {
//!     for (room_id, event) in page? {
//!         println!(
//!             "Found event in room {room_id} with timestamp: {:?}",
//!             event.timestamp
//!         );
//!     }
//! }
//! # Ok(())
//! # }
//! ```

use std::{collections::HashSet, pin::Pin};

use async_stream::try_stream;
use futures_util::{Stream, StreamExt as _};
use matrix_sdk_base::{RoomStateFilter, deserialized_responses::TimelineEvent};
use matrix_sdk_search::error::IndexError;
#[cfg(doc)]
use matrix_sdk_search::index::RoomIndex;
use ruma::{OwnedEventId, OwnedRoomId};

use crate::{Client, Room};

/// Number of results pulled from the index in one go while paginating through a
/// search stream.
const SEARCH_RESULTS_PAGE_SIZE: usize = 100;

/// A boxed, score-descending stream of `(score, event_id)` results for a single
/// room.
type RoomResultStream = Pin<Box<dyn Stream<Item = Result<(f32, OwnedEventId), IndexError>> + Send>>;

/// A cursor over one room's score-descending search results, used while merging
/// results across rooms.
struct RoomStreamCursor {
    /// The room these results come from.
    room_id: OwnedRoomId,

    /// The room's score-descending result stream.
    stream: RoomResultStream,

    /// The next result this room would contribute to the merge: a one-item
    /// lookahead buffered from `stream`, so we can compare every room's best
    /// remaining result without consuming it. `None` once the stream is
    /// exhausted.
    next_result: Option<(f32, OwnedEventId)>,
}

impl Room {
    /// Search this room's [`RoomIndex`] for query and return at most
    /// max_number_of_results results.
    pub async fn search(
        &self,
        query: &str,
        max_number_of_results: usize,
        pagination_offset: Option<usize>,
    ) -> Result<Vec<(f32, OwnedEventId)>, IndexError> {
        let mut search_index_guard = self.client.search_index().lock().await;
        search_index_guard.search(query, max_number_of_results, pagination_offset, self.room_id())
    }
}

/// An error that can occur while searching messages, using the high-level
/// search helpers provided by this module.
#[derive(thiserror::Error, Debug)]
pub enum SearchError {
    /// An error occurred while searching through the index for matching events.
    #[error(transparent)]
    IndexError(#[from] IndexError),
    /// An error occurred while loading the event content for a search result.
    #[error(transparent)]
    EventLoadError(#[from] crate::Error),
}

impl Room {
    /// Search for messages in this room matching the given query, returning a
    /// stream of pages of `(score, event_id)` results sorted by descending
    /// relevance score.
    pub fn search_messages(
        &self,
        query: String,
    ) -> impl Stream<Item = Result<Vec<(f32, OwnedEventId)>, IndexError>> + use<> {
        let room = self.clone();

        // TODO: use the client/server API search endpoint for public rooms, as those
        // may require lots of time for indexing all events.
        try_stream! {
            let mut offset = 0;
            loop {
                let page = room.search(&query, SEARCH_RESULTS_PAGE_SIZE, Some(offset)).await?;
                if page.is_empty() {
                    break;
                }
                offset += page.len();
                yield page;
            }
        }
    }

    /// Same as [`Room::search_messages`], but yields pages of full
    /// [`TimelineEvent`]s instead of event IDs, by loading them from the store
    /// or from the network.
    pub fn search_messages_events(
        &self,
        query: String,
    ) -> impl Stream<Item = Result<Vec<TimelineEvent>, SearchError>> + use<> {
        let room = self.clone();

        try_stream! {
            let mut pages = Box::pin(room.search_messages(query));
            while let Some(page) = pages.next().await {
                let page = page?;
                let mut events = Vec::with_capacity(page.len());
                for (_score, event_id) in page {
                    events.push(room.load_or_fetch_event(&event_id, None).await?);
                }
                yield events;
            }
        }
    }
}

/// A builder for a global search [`Stream`] that allows configuring the initial
/// working set of rooms to search in.
#[derive(Debug)]
pub struct GlobalSearchBuilder {
    client: Client,

    /// The search query, directly forwarded to the search API.
    query: String,

    /// The working set of rooms to search in.
    room_set: Vec<Room>,
}

impl GlobalSearchBuilder {
    /// Create a new global search on all the joined rooms.
    fn new(client: Client, query: String) -> Self {
        let room_set = client.rooms_filtered(RoomStateFilter::JOINED);
        Self { client, query, room_set }
    }

    /// Keep only the DM rooms from the initial working set.
    pub async fn only_dm_rooms(mut self) -> Result<Self, crate::Error> {
        let mut to_remove = HashSet::new();
        for room in &self.room_set {
            if !room.compute_is_dm().await? {
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
            if room.compute_is_dm().await? {
                to_remove.insert(room.room_id().to_owned());
            }
        }
        self.room_set.retain(|room| !to_remove.contains(room.room_id()));
        Ok(self)
    }

    /// Build a stream over the search results across all the rooms in the
    /// working set, yielding pages of `(room_id, score, event_id)` tuples
    /// sorted by descending relevance score.
    pub fn build(
        self,
    ) -> impl Stream<Item = Result<Vec<(OwnedRoomId, f32, OwnedEventId)>, IndexError>> {
        let query = self.query;
        let rooms = self.room_set;

        try_stream! {
            // One score-descending result stream per room, each primed with its next
            // result so we can merge across rooms by score.
            let mut cursors: Vec<RoomStreamCursor> = Vec::with_capacity(rooms.len());
            for room in rooms {
                let room_id = room.room_id().to_owned();
                let stream = Box::pin(Self::flatten_pages(room.search_messages(query.clone())));
                cursors.push(RoomStreamCursor { room_id, stream, next_result: None });
            }
            for cursor in &mut cursors {
                cursor.next_result = match cursor.stream.next().await {
                    Some(result) => Some(result?),
                    None => None,
                };
            }

            let mut page = Vec::with_capacity(SEARCH_RESULTS_PAGE_SIZE);
            loop {
                // Pick the room whose next result has the highest relevance score.
                let best = cursors
                    .iter()
                    .enumerate()
                    .filter_map(|(index, cursor)| {
                        cursor.next_result.as_ref().map(|(score, _)| (index, *score))
                    })
                    .max_by(|(_, a), (_, b)| a.total_cmp(b));

                let Some((index, _)) = best else {
                    // Every room is exhausted.
                    break;
                };

                let cursor = &mut cursors[index];
                let (score, event_id) =
                    cursor.next_result.take().expect("the chosen room must have a next result");
                let room_id = cursor.room_id.clone();

                // Refill this room's lookahead for the next iteration.
                cursor.next_result = match cursor.stream.next().await {
                    Some(result) => Some(result?),
                    None => None,
                };

                page.push((room_id, score, event_id));
                if page.len() == SEARCH_RESULTS_PAGE_SIZE {
                    yield std::mem::take(&mut page);
                }
            }

            if !page.is_empty() {
                yield page;
            }
        }
    }

    /// Same as [`Self::build`], but yields pages of full [`TimelineEvent`]s
    /// instead of event IDs, by loading them from the store or from the
    /// network.
    pub fn build_events(
        self,
    ) -> impl Stream<Item = Result<Vec<(OwnedRoomId, TimelineEvent)>, SearchError>> {
        let client = self.client.clone();
        let pages = self.build();

        try_stream! {
            let mut pages = Box::pin(pages);
            while let Some(page) = pages.next().await {
                let page = page?;
                let mut events = Vec::with_capacity(page.len());
                for (room_id, _score, event_id) in page {
                    let Some(room) = client.get_room(&room_id) else {
                        continue;
                    };
                    events.push((room_id, room.load_or_fetch_event(&event_id, None).await?));
                }
                yield events;
            }
        }
    }

    /// Flatten a stream of result pages into a stream of individual results, so
    /// the cross-room merge can compare results one at a time.
    fn flatten_pages(
        pages: impl Stream<Item = Result<Vec<(f32, OwnedEventId)>, IndexError>>,
    ) -> impl Stream<Item = Result<(f32, OwnedEventId), IndexError>> {
        try_stream! {
            let mut pages = Box::pin(pages);
            while let Some(page) = pages.next().await {
                for result in page? {
                    yield result;
                }
            }
        }
    }
}

impl Client {
    /// Search across all rooms for events with the given query, returning a
    /// builder for a stream over the results.
    pub fn search_messages(&self, query: String) -> GlobalSearchBuilder {
        GlobalSearchBuilder::new(self.clone(), query)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::TryStreamExt as _;
    use matrix_sdk_test::{BOB, JoinedRoomBuilder, async_test, event_factory::EventFactory};
    use ruma::{OwnedEventId, OwnedRoomId, event_id, room_id, user_id};

    use crate::{sleep::sleep, test_utils::mocks::MatrixMockServer};

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

        // Searching for a missing keyword should succeed and yield nothing.
        {
            let results: Vec<(f32, OwnedEventId)> =
                room.search_messages("search query".to_owned()).try_concat().await.unwrap();
            assert!(results.is_empty());
        }

        // Search for an existing keyword, by event id.
        {
            let results: Vec<(f32, OwnedEventId)> =
                room.search_messages("world".to_owned()).try_concat().await.unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].1, event_id);
        }

        // Search for an existing keyword, by events.
        {
            let events: Vec<_> =
                room.search_messages_events("world".to_owned()).try_concat().await.unwrap();
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].event_id().unwrap(), event_id);
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

        // Searching for a missing keyword should succeed and yield nothing.
        {
            let results: Vec<(OwnedRoomId, f32, OwnedEventId)> = client
                .search_messages("search query".to_owned())
                .build()
                .try_concat()
                .await
                .unwrap();
            assert!(results.is_empty());
        }

        // Search for an existing keyword, by event id.
        {
            let results: Vec<(OwnedRoomId, f32, OwnedEventId)> =
                client.search_messages("world".to_owned()).build().try_concat().await.unwrap();
            assert_eq!(results.len(), 2);
            // Search results order is not guaranteed, so we check that both expected
            // results are present.
            assert!(results.iter().any(|(room_id, _, event_id)| {
                room_id == room_id1 && event_id == result_event_id1
            }));
            assert!(results.iter().any(|(room_id, _, event_id)| {
                room_id == room_id2 && event_id == result_event_id2
            }));
        }

        // Search for an existing keyword, by event.
        {
            let results: Vec<_> = client
                .search_messages("world".to_owned())
                .build_events()
                .try_concat()
                .await
                .unwrap();
            assert_eq!(results.len(), 2);
            // Search results order is not guaranteed, so we check that both expected
            // results are present.
            assert!(results.iter().any(|(room_id, event)| {
                room_id == room_id1 && event.event_id() == Some(result_event_id1)
            }));
            assert!(results.iter().any(|(room_id, event)| {
                room_id == room_id2 && event.event_id() == Some(result_event_id2)
            }));
        }
    }

    #[async_test]
    async fn test_global_message_search_score_ordering() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let room_id1 = room_id!("!r1:localhost");
        let room_id2 = room_id!("!r2:localhost");

        let f = EventFactory::new().sender(user_id!("@user_id:localhost"));

        // Both rooms get two documents of identical length (padded with filler so
        // document-length normalization and the per-corpus IDF of "world" match across
        // rooms). The score then depends only on how many times "world" appears.
        //
        // Term frequencies are 4, 3, 2, 1, split so the rooms alternate by rank:
        // room1 holds the 4x and 2x events, room2 the 3x and 1x events. A correct
        // cross-room sort therefore interleaves the rooms: r1, r2, r1, r2.
        let r1_rank1 = event_id!("$r1_rank1:localhost"); // room1, "world" x4
        let r2_rank2 = event_id!("$r2_rank2:localhost"); // room2, "world" x3
        let r1_rank3 = event_id!("$r1_rank3:localhost"); // room1, "world" x2
        let r2_rank4 = event_id!("$r2_rank4:localhost"); // room2, "world" x1

        server
            .mock_sync()
            .ok_and_run(&client, |sync_builder| {
                sync_builder
                    .add_joined_room(
                        JoinedRoomBuilder::new(room_id1)
                            .add_timeline_event(
                                f.text_msg("world world world world filler filler filler filler filler filler")
                                    .room(room_id1)
                                    .event_id(r1_rank1),
                            )
                            .add_timeline_event(
                                f.text_msg("world world filler filler filler filler filler filler filler filler")
                                    .room(room_id1)
                                    .event_id(r1_rank3),
                            ),
                    )
                    .add_joined_room(
                        JoinedRoomBuilder::new(room_id2)
                            .add_timeline_event(
                                f.text_msg("world world world filler filler filler filler filler filler filler")
                                    .room(room_id2)
                                    .event_id(r2_rank2),
                            )
                            .add_timeline_event(
                                f.text_msg("world filler filler filler filler filler filler filler filler filler")
                                    .room(room_id2)
                                    .event_id(r2_rank4),
                            ),
                    );
            })
            .await;

        sleep(Duration::from_millis(200)).await;

        let results: Vec<(OwnedRoomId, f32, OwnedEventId)> =
            client.search_messages("world".to_owned()).build().try_concat().await.unwrap();
        assert_eq!(results.len(), 4);

        // Results are interleaved across the two rooms strictly by score.
        assert_eq!((&results[0].0, &results[0].2), (&room_id1.to_owned(), &r1_rank1.to_owned()));
        assert_eq!((&results[1].0, &results[1].2), (&room_id2.to_owned(), &r2_rank2.to_owned()));
        assert_eq!((&results[2].0, &results[2].2), (&room_id1.to_owned(), &r1_rank3.to_owned()));
        assert_eq!((&results[3].0, &results[3].2), (&room_id2.to_owned(), &r2_rank4.to_owned()));
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
            let results: Vec<(OwnedRoomId, f32, OwnedEventId)> = client
                .search_messages("world".to_owned())
                .only_dm_rooms()
                .await
                .unwrap()
                .build()
                .try_concat()
                .await
                .unwrap();

            assert_eq!(results.len(), 1);
            assert_eq!(
                (&results[0].0, &results[0].2),
                (&room_id1.to_owned(), &result_event_id1.to_owned())
            );
        }

        // Search for an existing keyword, by event, only in groups.
        {
            let results: Vec<_> = client
                .search_messages("world".to_owned())
                .no_dms()
                .await
                .unwrap()
                .build_events()
                .try_concat()
                .await
                .unwrap();

            assert_eq!(results.len(), 1);
            assert_eq!(results[0].0, room_id2);
            assert_eq!(results[0].1.event_id().unwrap(), result_event_id2);
        }
    }
}
