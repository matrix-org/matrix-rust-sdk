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
// See the License for that specific language governing permissions and
// limitations under the License.

use std::sync::Arc;

use eyeball::{ObservableWriteGuard, SharedObservable, Subscriber};
use eyeball_im::{ObservableVector, VectorSubscriberBatchedStream};
use futures_util::pin_mut;
use imbl::Vector;
use itertools::Itertools;
use matrix_sdk::{Client, Error, executor::AbortOnDrop, locks::Mutex, paginators::PaginationToken};
use matrix_sdk_common::executor::spawn;
use ruma::{
    OwnedRoomId, api::client::space::get_hierarchy, events::space::child::SpaceChildEventContent,
    uint,
};
use tokio::sync::Mutex as AsyncMutex;
use tracing::error;

use crate::spaces::SpaceRoom;

#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpaceRoomListPaginationState {
    Idle { end_reached: bool },
    Loading,
}

/// The `SpaceRoomList`represents a paginated list of direct rooms
/// that belong to a particular space.
///
/// It can be used to paginate through the list (and have live updates on the
/// pagination state) as well as subscribe to changes as rooms are joined or
/// left.
///
/// The `SpaceRoomList` also automatically subscribes to client room changes
/// and updates the list accordingly as rooms are joined or left.
///
/// # Examples
///
/// ```no_run
/// use futures_util::StreamExt;
/// use matrix_sdk::Client;
/// use matrix_sdk_ui::spaces::{
///     SpaceService, room_list::SpaceRoomListPaginationState,
/// };
/// use ruma::owned_room_id;
///
/// # async {
/// # let client: Client = todo!();
/// let space_service = SpaceService::new(client.clone());
///
/// // Get a list of all the rooms in a particular space
/// let room_list = space_service
///     .space_room_list(owned_room_id!("!some_space:example.org"))
///     .await;
///
/// // Start off with an empty and idle list
/// room_list.rooms().is_empty();
///
/// assert_eq!(
///     room_list.pagination_state(),
///     SpaceRoomListPaginationState::Idle { end_reached: false }
/// );
///
/// // Subscribe to pagination state updates
/// let pagination_state_stream =
///     room_list.subscribe_to_pagination_state_updates();
///
/// // And to room list updates
/// let (_, room_stream) = room_list.subscribe_to_room_updates();
///
/// // Run this in a background task so it doesn't block
/// while let Some(pagination_state) = pagination_state_stream.next().await {
///     println!("Received pagination state update: {pagination_state:?}");
/// }
///
/// // Run this in a background task so it doesn't block
/// while let Some(diffs) = room_stream.next().await {
///     println!("Received room list update: {diffs:?}");
/// }
///
/// // Ask the room to load the next page
/// room_list.paginate().await.unwrap();
///
/// // And, if successful, rooms are available
/// let rooms = room_list.rooms();
/// # anyhow::Ok(()) };
/// ```
pub struct SpaceRoomList {
    client: Client,

    space_id: OwnedRoomId,

    space: SharedObservable<Option<SpaceRoom>>,

    token: AsyncMutex<PaginationToken>,

    pagination_state: SharedObservable<SpaceRoomListPaginationState>,

    rooms: Arc<Mutex<ObservableVector<SpaceRoom>>>,

    _room_update_handle: AbortOnDrop<()>,
}

impl SpaceRoomList {
    /// Creates a new `SpaceRoomList` for the given space identifier.
    pub async fn new(client: Client, space_id: OwnedRoomId) -> Self {
        let rooms = Arc::new(Mutex::new(ObservableVector::<SpaceRoom>::new()));

        let all_room_updates_receiver = client.subscribe_to_all_room_updates();

        let handle = spawn({
            let client = client.clone();
            let rooms = rooms.clone();

            async move {
                pin_mut!(all_room_updates_receiver);

                loop {
                    match all_room_updates_receiver.recv().await {
                        Ok(updates) => {
                            if updates.is_empty() {
                                continue;
                            }

                            let mut mutable_rooms = rooms.lock();

                            updates.iter_all_room_ids().for_each(|updated_room_id| {
                                if let Some((position, room)) = mutable_rooms
                                    .clone()
                                    .iter()
                                    .find_position(|room| &room.room_id == updated_room_id)
                                    && let Some(updated_room) = client.get_room(updated_room_id)
                                {
                                    mutable_rooms.set(
                                        position,
                                        SpaceRoom::new_from_known(
                                            &updated_room,
                                            room.children_count,
                                        ),
                                    );
                                }
                            })
                        }
                        Err(err) => {
                            error!("error when listening to room updates: {err}");
                        }
                    }
                }
            }
        });

        let space = if let Some(parent) = client.get_room(&space_id) {
            let children_count = parent
                .get_state_events_static::<SpaceChildEventContent>()
                .await
                .map_or(0, |c| c.len() as u64);

            Some(SpaceRoom::new_from_known(&parent, children_count))
        } else {
            None
        };

        Self {
            client,
            space_id,
            space: SharedObservable::new(space),
            token: AsyncMutex::new(None.into()),
            pagination_state: SharedObservable::new(SpaceRoomListPaginationState::Idle {
                end_reached: false,
            }),
            rooms,
            _room_update_handle: AbortOnDrop::new(handle),
        }
    }

    /// Returns the space of the room list if known.
    pub fn space(&self) -> Option<SpaceRoom> {
        self.space.get()
    }

    /// Subscribe to space updates.
    pub fn subscribe_to_space_updates(&self) -> Subscriber<Option<SpaceRoom>> {
        self.space.subscribe()
    }

    /// Returns if the room list is currently paginating or not.
    pub fn pagination_state(&self) -> SpaceRoomListPaginationState {
        self.pagination_state.get()
    }

    /// Subscribe to pagination updates.
    pub fn subscribe_to_pagination_state_updates(
        &self,
    ) -> Subscriber<SpaceRoomListPaginationState> {
        self.pagination_state.subscribe()
    }

    /// Return the current list of rooms.
    pub fn rooms(&self) -> Vec<SpaceRoom> {
        self.rooms.lock().iter().cloned().collect_vec()
    }

    /// Subscribes to room list updates.
    pub fn subscribe_to_room_updates(
        &self,
    ) -> (Vector<SpaceRoom>, VectorSubscriberBatchedStream<SpaceRoom>) {
        self.rooms.lock().subscribe().into_values_and_batched_stream()
    }

    /// Ask the list to retrieve the next page if the end hasn't been reached
    /// yet. Otherwise it no-ops.
    pub async fn paginate(&self) -> Result<(), Error> {
        {
            let mut pagination_state = self.pagination_state.write();

            match *pagination_state {
                SpaceRoomListPaginationState::Idle { end_reached } if end_reached => {
                    return Ok(());
                }
                SpaceRoomListPaginationState::Loading => {
                    return Ok(());
                }
                _ => {}
            }

            ObservableWriteGuard::set(&mut pagination_state, SpaceRoomListPaginationState::Loading);
        }

        let mut request = get_hierarchy::v1::Request::new(self.space_id.clone());
        request.max_depth = Some(uint!(1)); // We only want the immediate children of the space

        let mut pagination_token = self.token.lock().await;

        if let PaginationToken::HasMore(ref token) = *pagination_token {
            request.from = Some(token.clone());
        }

        match self.client.send(request).await {
            Ok(result) => {
                *pagination_token = match &result.next_batch {
                    Some(val) => PaginationToken::HasMore(val.clone()),
                    None => PaginationToken::HitEnd,
                };

                let mut rooms = self.rooms.lock();
                result
                    .rooms
                    .iter()
                    .filter_map(|room| {
                        let space_room = Some(SpaceRoom::new_from_summary(
                            &room.summary,
                            self.client.get_room(&room.summary.room_id),
                            room.children_state.len() as u64,
                        ));

                        if room.summary.room_id == self.space_id {
                            let mut space = self.space.write();
                            if space.is_none() {
                                ObservableWriteGuard::set(&mut space, space_room);
                            }

                            return None;
                        }

                        space_room
                    })
                    .for_each(|room| rooms.push_back(room));

                self.pagination_state.set(SpaceRoomListPaginationState::Idle {
                    end_reached: result.next_batch.is_none(),
                });

                Ok(())
            }
            Err(err) => {
                self.pagination_state
                    .set(SpaceRoomListPaginationState::Idle { end_reached: false });
                Err(err.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use assert_matches2::{assert_let, assert_matches};
    use eyeball_im::VectorDiff;
    use futures_util::pin_mut;
    use matrix_sdk::{RoomState, test_utils::mocks::MatrixMockServer};
    use matrix_sdk_test::{
        JoinedRoomBuilder, LeftRoomBuilder, async_test, event_factory::EventFactory,
    };
    use ruma::{
        room::{JoinRuleSummary, RoomSummary},
        room_id, uint,
    };
    use stream_assert::{assert_next_eq, assert_next_matches, assert_pending, assert_ready};

    use crate::spaces::{SpaceRoom, SpaceService, room_list::SpaceRoomListPaginationState};

    #[async_test]
    async fn test_room_list_pagination() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let space_service = SpaceService::new(client.clone());
        let factory = EventFactory::new();

        server.mock_room_state_encryption().plain().mount().await;

        let parent_space_id = room_id!("!parent_space:example.org");
        let child_space_id_1 = room_id!("!1:example.org");
        let child_space_id_2 = room_id!("!2:example.org");

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(parent_space_id)
                    .add_state_event(
                        factory
                            .space_child(parent_space_id.to_owned(), child_space_id_1.to_owned())
                            .sender(user_id),
                    )
                    .add_state_event(
                        factory
                            .space_child(parent_space_id.to_owned(), child_space_id_2.to_owned())
                            .sender(user_id),
                    ),
            )
            .await;

        let room_list = space_service.space_room_list(parent_space_id.to_owned()).await;

        // The space parent is known to the client and should be populated accordingly
        assert_let!(Some(parent_space) = room_list.space());
        assert_eq!(parent_space.children_count, 2);

        // Start off idle
        assert_matches!(
            room_list.pagination_state(),
            SpaceRoomListPaginationState::Idle { end_reached: false }
        );

        // without any rooms
        assert_eq!(room_list.rooms(), vec![]);

        // and with pending subscribers

        let pagination_state_subscriber = room_list.subscribe_to_pagination_state_updates();
        pin_mut!(pagination_state_subscriber);
        assert_pending!(pagination_state_subscriber);

        let (_, rooms_subscriber) = room_list.subscribe_to_room_updates();
        pin_mut!(rooms_subscriber);
        assert_pending!(rooms_subscriber);

        // Paginating the room list
        server
            .mock_get_hierarchy()
            .ok_with_room_ids_and_children_state(
                vec![child_space_id_1, child_space_id_2],
                vec![room_id!("!child:example.org")],
            )
            .mount()
            .await;

        room_list.paginate().await.unwrap();

        // informs that the pagination reached the end
        assert_next_matches!(
            pagination_state_subscriber,
            SpaceRoomListPaginationState::Idle { end_reached: true }
        );

        // and yields results
        assert_next_eq!(
            rooms_subscriber,
            vec![
                VectorDiff::PushBack {
                    value: SpaceRoom::new_from_summary(
                        &RoomSummary::new(
                            child_space_id_1.to_owned(),
                            JoinRuleSummary::Public,
                            false,
                            uint!(1),
                            false,
                        ),
                        None,
                        1
                    )
                },
                VectorDiff::PushBack {
                    value: SpaceRoom::new_from_summary(
                        &RoomSummary::new(
                            child_space_id_2.to_owned(),
                            JoinRuleSummary::Public,
                            false,
                            uint!(1),
                            false,
                        ),
                        None,
                        1
                    ),
                }
            ]
        );
    }

    #[async_test]
    async fn test_room_state_updates() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let space_service = SpaceService::new(client.clone());

        let parent_space_id = room_id!("!parent_space:example.org");
        let child_room_id_1 = room_id!("!1:example.org");
        let child_room_id_2 = room_id!("!2:example.org");

        server
            .mock_get_hierarchy()
            .ok_with_room_ids(vec![child_room_id_1, child_room_id_2])
            .mount()
            .await;

        let room_list = space_service.space_room_list(parent_space_id.to_owned()).await;

        room_list.paginate().await.unwrap();

        // This space contains 2 rooms
        assert_eq!(room_list.rooms().first().unwrap().room_id, child_room_id_1);
        assert_eq!(room_list.rooms().last().unwrap().room_id, child_room_id_2);

        // and we don't know about either of them
        assert_eq!(room_list.rooms().first().unwrap().state, None);
        assert_eq!(room_list.rooms().last().unwrap().state, None);

        let (_, rooms_subscriber) = room_list.subscribe_to_room_updates();
        pin_mut!(rooms_subscriber);
        assert_pending!(rooms_subscriber);

        // Joining one of them though
        server.sync_room(&client, JoinedRoomBuilder::new(child_room_id_1)).await;

        // Results in an update being pushed through
        assert_ready!(rooms_subscriber);
        assert_eq!(room_list.rooms().first().unwrap().state, Some(RoomState::Joined));
        assert_eq!(room_list.rooms().last().unwrap().state, None);

        // Same for the second one
        server.sync_room(&client, JoinedRoomBuilder::new(child_room_id_2)).await;
        assert_ready!(rooms_subscriber);
        assert_eq!(room_list.rooms().first().unwrap().state, Some(RoomState::Joined));
        assert_eq!(room_list.rooms().last().unwrap().state, Some(RoomState::Joined));

        // And when leaving them
        server.sync_room(&client, LeftRoomBuilder::new(child_room_id_1)).await;
        server.sync_room(&client, LeftRoomBuilder::new(child_room_id_2)).await;
        assert_ready!(rooms_subscriber);
        assert_eq!(room_list.rooms().first().unwrap().state, Some(RoomState::Left));
        assert_eq!(room_list.rooms().last().unwrap().state, Some(RoomState::Left));
    }

    #[async_test]
    async fn test_parent_space_updates() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let space_service = SpaceService::new(client.clone());
        let factory = EventFactory::new();

        server.mock_room_state_encryption().plain().mount().await;

        let parent_space_id = room_id!("!parent_space:example.org");
        let child_space_id_1 = room_id!("!1:example.org");
        let child_space_id_2 = room_id!("!2:example.org");

        // Parent space is unknown to the client and thus not populated yet
        let room_list = space_service.space_room_list(parent_space_id.to_owned()).await;
        assert!(room_list.space().is_none());

        let parent_space_subscriber = room_list.subscribe_to_space_updates();
        pin_mut!(parent_space_subscriber);
        assert_pending!(parent_space_subscriber);

        server
            .mock_get_hierarchy()
            .ok_with_room_ids(vec![parent_space_id, child_space_id_1, child_space_id_2])
            .mount()
            .await;

        // Pagination will however fetch and populate it from /hierarchy
        room_list.paginate().await.unwrap();
        assert_let!(Some(parent_space) = room_list.space());
        assert_eq!(parent_space.room_id, parent_space_id);

        // And the subscription is informed about the change
        assert_next_eq!(parent_space_subscriber, Some(parent_space));

        // If the room is already known to the client then the space parent
        // is populated directly on creation
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(parent_space_id)
                    .add_state_event(
                        factory
                            .space_child(parent_space_id.to_owned(), child_space_id_1.to_owned())
                            .sender(user_id),
                    )
                    .add_state_event(
                        factory
                            .space_child(parent_space_id.to_owned(), child_space_id_2.to_owned())
                            .sender(user_id),
                    ),
            )
            .await;

        let room_list = space_service.space_room_list(parent_space_id.to_owned()).await;

        // The parent space is known to the client and should be populated accordingly
        assert_let!(Some(parent_space) = room_list.space());
        assert_eq!(parent_space.children_count, 2);
    }
}
