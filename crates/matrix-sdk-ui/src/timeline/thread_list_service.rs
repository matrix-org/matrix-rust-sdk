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

use std::sync::Arc;

use eyeball::{ObservableWriteGuard, SharedObservable, Subscriber};
use eyeball_im::{ObservableVector, VectorSubscriberBatchedStream};
use imbl::Vector;
use matrix_sdk::{Room, locks::Mutex, paginators::PaginationToken, room::ListThreadsOptions};
use tokio::sync::Mutex as AsyncMutex;

use crate::timeline::{threads::ThreadListItem, traits::RoomExt};

/// The pagination state of a [`ThreadListService`].
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThreadListPaginationState {
    /// The list is idle (not currently loading).
    Idle {
        /// Whether the end of the thread list has been reached (no more pages
        /// to load).
        end_reached: bool,
    },
    /// The list is currently loading the next page.
    Loading,
}

/// An error that occurred while using a [`ThreadListService`].
#[derive(Debug, thiserror::Error)]
pub enum ThreadListServiceError {
    /// An error from the underlying Matrix SDK.
    #[error(transparent)]
    Sdk(#[from] matrix_sdk::Error),
}

/// A paginated list of threads for a given room.
///
/// `ThreadListService` provides an observable, paginated list of
/// [`ThreadListItem`]s. It exposes methods to paginate forward through the
/// thread list as well as subscribe to state changes.
///
/// # Example
///
/// ```no_run
/// use matrix_sdk::Room;
/// use matrix_sdk_ui::timeline::thread_list_service::{
///     ThreadListPaginationState, ThreadListService,
/// };
///
/// # async {
/// # let room: Room = todo!();
/// let service = ThreadListService::new(room);
///
/// assert_eq!(
///     service.pagination_state(),
///     ThreadListPaginationState::Idle { end_reached: false }
/// );
///
/// service.paginate().await.unwrap();
///
/// let items = service.items();
/// # anyhow::Ok(()) };
/// ```
pub struct ThreadListService {
    /// The room whose threads are being listed.
    room: Room,

    /// The pagination token used to fetch subsequent pages.
    token: AsyncMutex<PaginationToken>,

    /// The current pagination state.
    pagination_state: SharedObservable<ThreadListPaginationState>,

    /// The current list of thread items.
    items: Arc<Mutex<ObservableVector<ThreadListItem>>>,
}

impl ThreadListService {
    /// Creates a new [`ThreadListService`] for the given room.
    pub fn new(room: Room) -> Self {
        Self {
            room,
            token: AsyncMutex::new(PaginationToken::None),
            pagination_state: SharedObservable::new(ThreadListPaginationState::Idle {
                end_reached: false,
            }),
            items: Arc::new(Mutex::new(ObservableVector::new())),
        }
    }

    /// Returns the current pagination state.
    pub fn pagination_state(&self) -> ThreadListPaginationState {
        self.pagination_state.get()
    }

    /// Subscribes to pagination state updates.
    ///
    /// The returned [`Subscriber`] will emit a new value every time the
    /// pagination state changes.
    pub fn subscribe_to_pagination_state_updates(&self) -> Subscriber<ThreadListPaginationState> {
        self.pagination_state.subscribe()
    }

    /// Returns the current list of thread items as a snapshot.
    pub fn items(&self) -> Vec<ThreadListItem> {
        self.items.lock().iter().cloned().collect()
    }

    /// Subscribes to updates of the thread item list.
    ///
    /// Returns a snapshot of the current items alongside a batched stream of
    /// [`eyeball_im::VectorDiff`]s that describe subsequent changes.
    pub fn subscribe_to_items_updates(
        &self,
    ) -> (Vector<ThreadListItem>, VectorSubscriberBatchedStream<ThreadListItem>) {
        self.items.lock().subscribe().into_values_and_batched_stream()
    }

    /// Fetches the next page of threads, appending the results to the item
    /// list.
    ///
    /// - If the list is already loading or the end has been reached, this
    ///   method returns immediately with `Ok(())`.
    /// - On a network/SDK error the pagination state is reset to `Idle {
    ///   end_reached: false }` and the error is propagated.
    pub async fn paginate(&self) -> Result<(), ThreadListServiceError> {
        // Guard: do nothing if we are already loading or have reached the end.
        {
            let mut pagination_state = self.pagination_state.write();

            match *pagination_state {
                ThreadListPaginationState::Idle { end_reached: true }
                | ThreadListPaginationState::Loading => return Ok(()),
                _ => {}
            }

            ObservableWriteGuard::set(&mut pagination_state, ThreadListPaginationState::Loading);
        }

        let mut pagination_token = self.token.lock().await;

        // Build the options for this page, using the current token if we have one.
        let from = match &*pagination_token {
            PaginationToken::HasMore(token) => Some(token.clone()),
            _ => None,
        };

        let opts = ListThreadsOptions { from, ..Default::default() };

        match self.room.load_thread_list(opts).await {
            Ok(thread_list) => {
                // Update the pagination token based on whether there are more pages.
                *pagination_token = match &thread_list.prev_batch_token {
                    Some(token) => PaginationToken::HasMore(token.clone()),
                    None => PaginationToken::HitEnd,
                };

                let end_reached = thread_list.prev_batch_token.is_none();

                // Append new items to the observable vector.
                self.items.lock().append(thread_list.items.into());

                self.pagination_state.set(ThreadListPaginationState::Idle { end_reached });

                Ok(())
            }
            Err(err) => {
                self.pagination_state.set(ThreadListPaginationState::Idle { end_reached: false });
                Err(ThreadListServiceError::Sdk(err))
            }
        }
    }

    /// Resets the service back to its initial state.
    ///
    /// Clears all loaded items, discards the current pagination token, and
    /// sets the pagination state to `Idle { end_reached: false }`.  The next
    /// call to [`Self::paginate`] will therefore start from the beginning of
    /// the thread list.
    pub async fn reset(&self) {
        let mut pagination_token = self.token.lock().await;
        *pagination_token = PaginationToken::None;

        self.items.lock().clear();

        self.pagination_state.set(ThreadListPaginationState::Idle { end_reached: false });
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::pin_mut;
    use matrix_sdk::test_utils::mocks::MatrixMockServer;
    use matrix_sdk_test::{async_test, event_factory::EventFactory};
    use ruma::{event_id, events::AnyTimelineEvent, room_id, serde::Raw, user_id};
    use serde_json::json;
    use stream_assert::{assert_next_matches, assert_pending};
    use wiremock::ResponseTemplate;

    use super::{ThreadListPaginationState, ThreadListService};

    #[async_test]
    async fn test_initial_state() {
        let server = MatrixMockServer::new().await;
        let service = make_service(&server).await;

        assert_eq!(
            service.pagination_state(),
            ThreadListPaginationState::Idle { end_reached: false }
        );
        assert!(service.items().is_empty());
    }

    #[async_test]
    async fn test_pagination() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");
        let sender_id = user_id!("@alice:b.c");

        let f = EventFactory::new().room(room_id).sender(sender_id);

        let eid1 = event_id!("$1");
        let eid2 = event_id!("$2");

        server
            .mock_room_threads()
            .ok(
                vec![f.text_msg("Thread root 1").event_id(eid1).into_raw()],
                Some("next_page_token".to_owned()),
            )
            .mock_once()
            .mount()
            .await;

        server
            .mock_room_threads()
            .match_from("next_page_token")
            .ok(vec![f.text_msg("Thread root 2").event_id(eid2).into_raw()], None)
            .mock_once()
            .mount()
            .await;

        let room = server.sync_joined_room(&client, room_id).await;
        let service = ThreadListService::new(room);

        service.paginate().await.expect("first paginate failed");

        assert_eq!(
            service.pagination_state(),
            ThreadListPaginationState::Idle { end_reached: false }
        );
        assert_eq!(service.items().len(), 1);
        assert_eq!(service.items()[0].root_event_id, eid1);

        service.paginate().await.expect("second paginate failed");

        assert_eq!(
            service.pagination_state(),
            ThreadListPaginationState::Idle { end_reached: true }
        );
        assert_eq!(service.items().len(), 2);
        assert_eq!(service.items()[1].root_event_id, eid2);
    }

    #[async_test]
    async fn test_pagination_end_reached() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");
        let sender_id = user_id!("@alice:b.c");
        let f = EventFactory::new().room(room_id).sender(sender_id);
        let eid1 = event_id!("$1");

        server
            .mock_room_threads()
            .ok(vec![f.text_msg("Thread root").event_id(eid1).into_raw()], None)
            .mock_once()
            .mount()
            .await;

        let room = server.sync_joined_room(&client, room_id).await;
        let service = ThreadListService::new(room);

        service.paginate().await.expect("paginate failed");
        assert_eq!(
            service.pagination_state(),
            ThreadListPaginationState::Idle { end_reached: true }
        );
        assert_eq!(service.items().len(), 1);

        service.paginate().await.expect("second paginate should be a no-op");
        assert_eq!(service.items().len(), 1);
        assert_eq!(
            service.pagination_state(),
            ThreadListPaginationState::Idle { end_reached: true }
        );
    }

    /// Two concurrent calls to [`ThreadListService::paginate`] must not result
    /// in two concurrent HTTP requests. The second call should detect that a
    /// pagination is already in progress (state is `Loading`) and return
    /// immediately without making another network request.
    #[async_test]
    async fn test_concurrent_pagination_is_not_possible() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");
        let sender_id = user_id!("@alice:b.c");
        let f = EventFactory::new().room(room_id).sender(sender_id);
        let eid1 = event_id!("$1");

        // Set up a slow mock response so both `paginate()` calls overlap in
        // flight. Using `expect(1)` means the test will panic during server
        // teardown if the endpoint is hit more than once.
        let chunk: Vec<Raw<AnyTimelineEvent>> =
            vec![f.text_msg("Thread root").event_id(eid1).into_raw()];
        server
            .mock_room_threads()
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "chunk": chunk, "next_batch": null }))
                    .set_delay(Duration::from_millis(100)),
            )
            .expect(1)
            .mount()
            .await;

        let room = server.sync_joined_room(&client, room_id).await;
        let service = ThreadListService::new(room);

        // Run two paginations concurrently.
        let (first, second) = tokio::join!(service.paginate(), service.paginate());

        first.expect("first paginate should succeed");
        second.expect("second (concurrent) paginate should succeed as a no-op");

        // Only one HTTP request was made, so we have exactly one item.
        assert_eq!(service.items().len(), 1);
        assert_eq!(service.items()[0].root_event_id, eid1);
        assert_eq!(
            service.pagination_state(),
            ThreadListPaginationState::Idle { end_reached: true }
        );
    }

    /// When the server returns an error, [`ThreadListService::paginate`] must
    /// propagate the error *and* reset the pagination state back to
    /// `Idle { end_reached: false }` so that the caller can retry.
    #[async_test]
    async fn test_pagination_error() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");

        server.mock_room_threads().error500().mock_once().mount().await;

        let room = server.sync_joined_room(&client, room_id).await;
        let service = ThreadListService::new(room);

        // Pagination must surface the server error.
        service.paginate().await.expect_err("paginate should fail on a 500 response");

        // The state must be reset so the caller can retry; it must *not* be
        // stuck in `Loading`.
        assert_eq!(
            service.pagination_state(),
            ThreadListPaginationState::Idle { end_reached: false }
        );

        // No items should have been added.
        assert!(service.items().is_empty());
    }

    #[async_test]
    async fn test_reset() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");
        let sender_id = user_id!("@alice:b.c");
        let f = EventFactory::new().room(room_id).sender(sender_id);
        let eid1 = event_id!("$1");

        server
            .mock_room_threads()
            .ok(vec![f.text_msg("Thread root").event_id(eid1).into_raw()], None)
            .expect(2)
            .mount()
            .await;

        let room = server.sync_joined_room(&client, room_id).await;
        let service = ThreadListService::new(room);

        service.paginate().await.expect("first paginate failed");
        assert_eq!(service.items().len(), 1);
        assert_eq!(
            service.pagination_state(),
            ThreadListPaginationState::Idle { end_reached: true }
        );

        service.reset().await;
        assert!(service.items().is_empty());
        assert_eq!(
            service.pagination_state(),
            ThreadListPaginationState::Idle { end_reached: false }
        );

        service.paginate().await.expect("paginate after reset failed");
        assert_eq!(service.items().len(), 1);
    }

    #[async_test]
    async fn test_pagination_state_subscriber() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");
        let sender_id = user_id!("@alice:b.c");
        let f = EventFactory::new().room(room_id).sender(sender_id);
        let eid1 = event_id!("$1");

        server
            .mock_room_threads()
            .ok(
                vec![f.text_msg("Thread root").event_id(eid1).into_raw()],
                Some("next_token".to_owned()),
            )
            .mock_once()
            .mount()
            .await;

        let room = server.sync_joined_room(&client, room_id).await;
        let service = ThreadListService::new(room);

        let subscriber = service.subscribe_to_pagination_state_updates();
        pin_mut!(subscriber);

        assert_pending!(subscriber);

        service.paginate().await.expect("paginate failed");

        assert_next_matches!(subscriber, ThreadListPaginationState::Idle { end_reached: false });
    }

    /// Builds a [`ThreadListService`] and makes the room known to the client
    /// by performing a sync.
    async fn make_service(server: &MatrixMockServer) -> ThreadListService {
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");
        let room = server.sync_joined_room(&client, room_id).await;
        ThreadListService::new(room)
    }
}
