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

//! A global, reactive search service.
//!
//! [`SearchService`] aggregates results of different kinds into a single
//! reactive, paginated list of typed [`ResultType`]s. Call
//! [`SearchService::set_query`] to start (or restart) a search, then drive it
//! page by page with [`SearchService::paginate`], observing the results and the
//! [`PaginationState`] as they change.
//!
//! Today the only source is the SDK's per-room message search; people, rooms
//! and other kinds are expected to be added as further [`ResultType`]
//! variants, and a `matrix-sdk` source can be swapped for a server-side one
//! without changing this interface.

use std::pin::Pin;

use eyeball::{ObservableWriteGuard, SharedObservable, Subscriber};
use eyeball_im::{ObservableVector, Vector, VectorSubscriberBatchedStream};
use futures_util::{Stream, StreamExt as _};
use matrix_sdk::{
    Client, deserialized_responses::TimelineEvent, message_search::SearchError, room::Room,
};
use ruma::{MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedUserId};
use tokio::sync::Mutex as AsyncMutex;

use crate::timeline::{Profile, TimelineDetails, TimelineItemContent};

/// A boxed stream of pages of resolved search hits, as produced by the SDK's
/// global message search.
type ResultsStream =
    Pin<Box<dyn Stream<Item = Result<Vec<(OwnedRoomId, TimelineEvent)>, SearchError>> + Send>>;

/// Whether the search service is currently loading a page of results.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PaginationState {
    /// Not currently paginating. `end_reached` is `true` once every source has
    /// been exhausted for the current query.
    Idle { end_reached: bool },
    /// A page of results is currently being loaded.
    Loading,
}

/// A single search result, tagged by the kind of entity it represents.
///
/// New result kinds will be added as additional variants.
#[derive(Debug, Clone)]
pub enum ResultType {
    /// A message (room timeline event) matching the query.
    Message(MessageResult),
}

/// A message matching a search query, with its content and sender resolved.
#[derive(Debug, Clone)]
pub struct MessageResult {
    /// The room the message belongs to.
    pub room_id: OwnedRoomId,
    /// The event ID of the message.
    pub event_id: OwnedEventId,
    /// The sender of the message.
    pub sender: OwnedUserId,
    /// The sender's profile, as far as it could be resolved.
    pub sender_profile: TimelineDetails<Profile>,
    /// The rendered content of the message.
    pub content: TimelineItemContent,
    /// The origin server timestamp of the message.
    pub timestamp: MilliSecondsSinceUnixEpoch,
}

impl MessageResult {
    /// Resolve a search hit into a full result by loading its content and
    /// sender profile, returning `None` if the event can't be rendered.
    async fn from_event(room: &Room, event: TimelineEvent) -> Option<Self> {
        let sender = event.sender()?;
        let event_id = event.event_id()?.to_owned();
        let timestamp = event.timestamp().unwrap_or_else(MilliSecondsSinceUnixEpoch::now);

        let content = TimelineItemContent::from_event(room, event).await?;
        let sender_profile =
            TimelineDetails::from_initial_value(Profile::load(room, &sender).await);

        Some(Self {
            room_id: room.room_id().to_owned(),
            event_id,
            sender,
            sender_profile,
            content,
            timestamp,
        })
    }
}

/// A global, reactive, paginated search across all the user's data.
pub struct SearchService {
    /// The client used to run searches and resolve their results.
    client: Client,

    /// The current query's result stream, set by [`Self::set_query`] and pulled
    /// one page at a time by [`Self::paginate`]. `None` until a query is set.
    stream: AsyncMutex<Option<ResultsStream>>,

    /// The current pagination state, observable via
    /// [`Self::subscribe_to_pagination_state_updates`].
    pagination_state: SharedObservable<PaginationState>,

    /// The accumulated results across the pages loaded so far, observable via
    /// [`Self::subscribe_to_results`].
    results: AsyncMutex<ObservableVector<ResultType>>,
}

impl SearchService {
    /// Create a new [`SearchService`] for the given client.
    pub fn new(client: Client) -> Self {
        Self {
            client,
            stream: AsyncMutex::new(None),
            pagination_state: SharedObservable::new(PaginationState::Idle { end_reached: false }),
            results: AsyncMutex::new(ObservableVector::new()),
        }
    }

    /// Set (or update) the search query.
    /// Clears the current results, restarts pagination from scratch and loads
    /// the first page. Call [`Self::paginate`] to load any further pages.
    pub async fn set_query(&self, query: String) -> Result<(), SearchError> {
        let stream = self.client.search_messages(query).build_events();
        *self.stream.lock().await = Some(Box::pin(stream));
        self.results.lock().await.clear();
        self.pagination_state.set(PaginationState::Idle { end_reached: false });

        self.paginate().await
    }

    /// Returns the current pagination state.
    pub fn pagination_state(&self) -> PaginationState {
        self.pagination_state.get()
    }

    /// Subscribe to pagination state updates.
    pub fn subscribe_to_pagination_state_updates(&self) -> Subscriber<PaginationState> {
        self.pagination_state.subscribe()
    }

    /// Return the current list of results.
    pub async fn results(&self) -> Vec<ResultType> {
        self.results.lock().await.iter().cloned().collect()
    }

    /// Subscribe to result list updates.
    pub async fn subscribe_to_results(
        &self,
    ) -> (Vector<ResultType>, VectorSubscriberBatchedStream<ResultType>) {
        self.results.lock().await.subscribe().into_values_and_batched_stream()
    }

    /// Load the next page of results, appending them to the list.
    pub async fn paginate(&self) -> Result<(), SearchError> {
        {
            let mut pagination_state = self.pagination_state.write();

            match *pagination_state {
                PaginationState::Idle { end_reached } if end_reached => return Ok(()),
                PaginationState::Loading => return Ok(()),
                _ => {}
            }

            ObservableWriteGuard::set(&mut pagination_state, PaginationState::Loading);
        }

        let mut stream = self.stream.lock().await;
        let Some(stream) = stream.as_mut() else {
            self.pagination_state.set(PaginationState::Idle { end_reached: true });
            return Ok(());
        };

        match stream.next().await {
            None => {
                self.pagination_state.set(PaginationState::Idle { end_reached: true });
            }
            Some(Err(err)) => {
                self.pagination_state.set(PaginationState::Idle { end_reached: false });
                return Err(err);
            }
            Some(Ok(page)) => {
                let mut resolved = Vector::new();
                for (room_id, event) in page {
                    let Some(room) = self.client.get_room(&room_id) else {
                        continue;
                    };
                    let Some(result) = MessageResult::from_event(&room, event).await else {
                        continue;
                    };
                    resolved.push_back(ResultType::Message(result));
                }

                if !resolved.is_empty() {
                    self.results.lock().await.append(resolved);
                }

                self.pagination_state.set(PaginationState::Idle { end_reached: false });
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use assert_matches2::assert_let;
    use eyeball_im::VectorDiff;
    use futures_util::pin_mut;
    use matrix_sdk::test_utils::mocks::MatrixMockServer;
    use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
    use ruma::{event_id, room_id, user_id};
    use stream_assert::{assert_next_matches, assert_pending};
    use tokio::time::sleep;

    use super::{PaginationState, ResultType, SearchService};

    #[async_test]
    async fn test_search_pagination() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let room_id = room_id!("!room:localhost");
        let event_id = event_id!("$event:localhost");
        let f = EventFactory::new().sender(user_id!("@user:localhost"));

        server
            .mock_sync()
            .ok_and_run(&client, |builder| {
                builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
                    f.text_msg("hello world").room(room_id).event_id(event_id),
                ));
            })
            .await;

        // Let the indexer process the event.
        sleep(Duration::from_millis(300)).await;

        let search = SearchService::new(client);

        // Starts idle and empty.
        assert_eq!(search.pagination_state(), PaginationState::Idle { end_reached: false });
        assert!(search.results().await.is_empty());

        // Setting the query loads the first page automatically.
        search.set_query("world".to_owned()).await.unwrap();

        assert_eq!(search.pagination_state(), PaginationState::Idle { end_reached: false });
        let results = search.results().await;
        assert_eq!(results.len(), 1);
        assert_let!(ResultType::Message(message) = &results[0]);
        assert_eq!(message.event_id, event_id);

        // Subscribing now yields the loaded results as the current state.
        let (initial, results_stream) = search.subscribe_to_results().await;
        assert_eq!(initial.len(), 1);
        pin_mut!(results_stream);
        assert_pending!(results_stream);

        // The next page is empty, so the end is reached and nothing more is emitted.
        search.paginate().await.unwrap();

        assert_pending!(results_stream);
        assert_eq!(search.pagination_state(), PaginationState::Idle { end_reached: true });
        assert_eq!(search.results().await.len(), 1);
    }

    #[async_test]
    async fn test_search_resets_on_query_change() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        let room_id = room_id!("!room:localhost");
        let apple_event = event_id!("$apple:localhost");
        let banana_event = event_id!("$banana:localhost");
        let f = EventFactory::new().sender(user_id!("@user:localhost"));

        server
            .mock_sync()
            .ok_and_run(&client, |builder| {
                builder.add_joined_room(
                    JoinedRoomBuilder::new(room_id)
                        .add_timeline_event(
                            f.text_msg("apple pie").room(room_id).event_id(apple_event),
                        )
                        .add_timeline_event(
                            f.text_msg("banana split").room(room_id).event_id(banana_event),
                        ),
                );
            })
            .await;

        sleep(Duration::from_millis(300)).await;

        let search = SearchService::new(client);

        // The first query loads the apple result automatically.
        search.set_query("apple".to_owned()).await.unwrap();

        let (initial, results_stream) = search.subscribe_to_results().await;
        assert_eq!(initial.len(), 1);
        assert_let!(ResultType::Message(message) = &initial[0]);
        assert_eq!(message.event_id, apple_event);
        pin_mut!(results_stream);
        assert_pending!(results_stream);

        // Changing the query clears the previous results and loads the new ones.
        search.set_query("banana".to_owned()).await.unwrap();

        // The subscriber observes the clear followed by the new page in one batch.
        assert_next_matches!(results_stream, diffs => {
            assert_let!([VectorDiff::Clear, VectorDiff::Append { values }] = diffs.as_slice());
            assert_eq!(values.len(), 1);
            assert_let!(ResultType::Message(message) = &values[0]);
            assert_eq!(message.event_id, banana_event);
        });

        let results = search.results().await;
        assert_eq!(results.len(), 1);
        assert_let!(ResultType::Message(message) = &results[0]);
        assert_eq!(message.event_id, banana_event);
    }
}
