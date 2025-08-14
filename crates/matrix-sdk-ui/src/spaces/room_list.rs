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

use eyeball::{SharedObservable, Subscriber};
use eyeball_im::{ObservableVector, VectorSubscriberBatchedStream};
use futures_util::pin_mut;
use imbl::Vector;
use itertools::Itertools;
use matrix_sdk::{Client, Error, locks::Mutex, paginators::PaginationToken};
use matrix_sdk_common::executor::{JoinHandle, spawn};
use ruma::{OwnedRoomId, api::client::space::get_hierarchy, uint};
use tracing::error;

use crate::spaces::SpaceRoom;

#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpaceRoomListPaginationState {
    Idle { end_reached: bool },
    Loading,
}

pub struct SpaceRoomList {
    client: Client,

    parent_space_id: OwnedRoomId,

    token: Mutex<PaginationToken>,

    pagination_state: SharedObservable<SpaceRoomListPaginationState>,

    rooms: Arc<Mutex<ObservableVector<SpaceRoom>>>,

    room_update_handle: JoinHandle<()>,
}

impl Drop for SpaceRoomList {
    fn drop(&mut self) {
        self.room_update_handle.abort();
    }
}
impl SpaceRoomList {
    pub fn new(client: Client, parent_space_id: OwnedRoomId) -> Self {
        let rooms = Arc::new(Mutex::new(ObservableVector::<SpaceRoom>::new()));

        let client_clone = client.clone();
        let rooms_clone = rooms.clone();
        let all_room_updates_receiver = client.subscribe_to_all_room_updates();

        let handle = spawn(async move {
            pin_mut!(all_room_updates_receiver);

            loop {
                match all_room_updates_receiver.recv().await {
                    Ok(updates) => {
                        let mut mutable_rooms = rooms_clone.lock();

                        updates.iter_all_room_ids().for_each(|updated_room_id| {
                            if let Some((position, room)) = mutable_rooms
                                .clone()
                                .iter()
                                .find_position(|room| &room.room_id == updated_room_id)
                                && let Some(update_room) = client_clone.get_room(updated_room_id)
                            {
                                mutable_rooms.set(
                                    position,
                                    SpaceRoom::new_from_known(update_room, room.children_count),
                                );
                            }
                        })
                    }
                    Err(err) => {
                        error!("error when listening to room updates: {err}");
                    }
                }
            }
        });

        Self {
            client,
            parent_space_id,
            token: Mutex::new(None.into()),
            pagination_state: SharedObservable::new(SpaceRoomListPaginationState::Idle {
                end_reached: false,
            }),
            rooms,
            room_update_handle: handle,
        }
    }

    pub fn pagination_state(&self) -> SpaceRoomListPaginationState {
        self.pagination_state.get()
    }

    pub fn subscribe_to_pagination_state_updates(
        &self,
    ) -> Subscriber<SpaceRoomListPaginationState> {
        self.pagination_state.subscribe()
    }

    pub fn rooms(&self) -> Vec<SpaceRoom> {
        self.rooms.lock().iter().cloned().collect_vec()
    }

    pub fn subscribe_to_room_updates(
        &self,
    ) -> (Vector<SpaceRoom>, VectorSubscriberBatchedStream<SpaceRoom>) {
        self.rooms.lock().subscribe().into_values_and_batched_stream()
    }

    pub async fn paginate(&self) -> Result<(), Error> {
        match *self.pagination_state.read() {
            SpaceRoomListPaginationState::Idle { end_reached } if end_reached => {
                return Ok(());
            }
            SpaceRoomListPaginationState::Loading => {
                return Ok(());
            }
            _ => {}
        }

        self.pagination_state.set(SpaceRoomListPaginationState::Loading);

        let mut request = get_hierarchy::v1::Request::new(self.parent_space_id.clone());
        request.max_depth = Some(uint!(1)); // We only want the immediate children of the space

        if let PaginationToken::HasMore(ref token) = *self.token.lock() {
            request.from = Some(token.clone());
        }

        match self.client.send(request).await {
            Ok(result) => {
                let mut token = self.token.lock();
                *token = match &result.next_batch {
                    Some(val) => PaginationToken::HasMore(val.clone()),
                    None => PaginationToken::HitEnd,
                };

                let mut rooms = self.rooms.lock();
                result
                    .rooms
                    .iter()
                    .flat_map(|room| {
                        if room.summary.room_id == self.parent_space_id {
                            None
                        } else {
                            Some(SpaceRoom::new_from_summary(
                                &room.summary,
                                self.client.get_room(&room.summary.room_id),
                                room.children_state.len() as u64,
                            ))
                        }
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
    use assert_matches2::assert_matches;
    use eyeball_im::VectorDiff;
    use futures_util::pin_mut;
    use matrix_sdk::{RoomState, test_utils::mocks::MatrixMockServer};
    use matrix_sdk_test::{JoinedRoomBuilder, LeftRoomBuilder, async_test};
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
        let space_service = SpaceService::new(client.clone());

        server.mock_room_state_encryption().plain().mount().await;

        let parent_space_id = room_id!("!parent_space:example.org");

        let room_list = space_service.space_room_list(parent_space_id.to_owned());

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

        let child_space_id_1 = room_id!("!1:example.org");
        let child_space_id_2 = room_id!("!2:example.org");

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

        let room_list = space_service.space_room_list(parent_space_id.to_owned());

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
}
