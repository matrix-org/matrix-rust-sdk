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

use std::sync::Mutex;

use eyeball::{SharedObservable, Subscriber};
use matrix_sdk::{Client, Error, paginators::PaginationToken};
use ruma::{OwnedRoomId, api::client::space::get_hierarchy};

use crate::spaces::SpaceServiceRoom;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpaceServiceRoomListPaginationState {
    Idle { end_reached: bool },
    Loading,
}

pub struct SpaceServiceRoomList {
    client: Client,

    parent_space_id: OwnedRoomId,

    token: Mutex<PaginationToken>,

    pagination_state: SharedObservable<SpaceServiceRoomListPaginationState>,

    rooms: SharedObservable<Vec<SpaceServiceRoom>>,
}

impl SpaceServiceRoomList {
    pub fn new(client: Client, parent_space_id: OwnedRoomId) -> Self {
        Self {
            client,
            parent_space_id,
            token: Mutex::new(None.into()),
            pagination_state: SharedObservable::new(SpaceServiceRoomListPaginationState::Idle {
                end_reached: false,
            }),
            rooms: SharedObservable::new(Vec::new()),
        }
    }

    pub fn pagination_state(&self) -> SpaceServiceRoomListPaginationState {
        self.pagination_state.get()
    }

    pub fn subscribe_to_pagination_state_updates(
        &self,
    ) -> Subscriber<SpaceServiceRoomListPaginationState> {
        self.pagination_state.subscribe()
    }

    pub fn rooms(&self) -> Vec<SpaceServiceRoom> {
        self.rooms.get()
    }

    pub fn subscribe_to_room_updates(&self) -> Subscriber<Vec<SpaceServiceRoom>> {
        self.rooms.subscribe()
    }

    pub async fn paginate(&self) -> Result<(), Error> {
        match *self.pagination_state.read() {
            SpaceServiceRoomListPaginationState::Idle { end_reached } if end_reached => {
                return Ok(());
            }
            SpaceServiceRoomListPaginationState::Loading => {
                return Ok(());
            }
            _ => {}
        }

        self.pagination_state.set(SpaceServiceRoomListPaginationState::Loading);

        let mut request = get_hierarchy::v1::Request::new(self.parent_space_id.clone());

        if let PaginationToken::HasMore(ref token) = *self.token.lock().unwrap() {
            request.from = Some(token.clone());
        }

        match self.client.send(request).await {
            Ok(result) => {
                let mut token = self.token.lock().unwrap();
                *token = match &result.next_batch {
                    Some(val) => PaginationToken::HasMore(val.clone()),
                    None => PaginationToken::HitEnd,
                };

                let mut current_rooms = Vec::new();

                (self.rooms.read()).iter().for_each(|room| {
                    current_rooms.push(room.clone());
                });

                current_rooms.extend(
                    result
                        .rooms
                        .iter()
                        .map(|room| (&room.summary, self.client.get_room(&room.summary.room_id)))
                        .map(|(summary, room)| SpaceServiceRoom::new_from_summary(summary, room))
                        .collect::<Vec<_>>(),
                );

                self.rooms.set(current_rooms.clone());

                self.pagination_state.set(SpaceServiceRoomListPaginationState::Idle {
                    end_reached: result.next_batch.is_none(),
                });

                Ok(())
            }
            Err(err) => {
                self.pagination_state
                    .set(SpaceServiceRoomListPaginationState::Idle { end_reached: false });
                Err(err.into())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use assert_matches2::assert_matches;
    use futures_util::pin_mut;
    use matrix_sdk::test_utils::mocks::MatrixMockServer;
    use matrix_sdk_test::async_test;
    use ruma::{
        room::{JoinRuleSummary, RoomSummary},
        room_id, uint,
    };
    use stream_assert::{assert_next_eq, assert_next_matches, assert_pending};

    use crate::spaces::{
        SpaceService, SpaceServiceRoom, room_list::SpaceServiceRoomListPaginationState,
    };

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
            SpaceServiceRoomListPaginationState::Idle { end_reached: false }
        );

        // without any rooms
        assert_eq!(room_list.rooms(), vec![]);

        // and with pending subscribers

        let pagination_state_subscriber = room_list.subscribe_to_pagination_state_updates();
        pin_mut!(pagination_state_subscriber);
        assert_pending!(pagination_state_subscriber);

        let rooms_subscriber = room_list.subscribe_to_room_updates();
        pin_mut!(rooms_subscriber);
        assert_pending!(rooms_subscriber);

        let child_space_id_1 = room_id!("!1:example.org");
        let child_space_id_2 = room_id!("!2:example.org");

        // Paginating the room list
        server
            .mock_get_hierarchy()
            .ok_with_room_ids(vec![child_space_id_1, child_space_id_2])
            .mount()
            .await;

        room_list.paginate().await.unwrap();

        // informs that the pagination reached the end
        assert_next_matches!(
            pagination_state_subscriber,
            SpaceServiceRoomListPaginationState::Idle { end_reached: true }
        );

        // yields results
        assert_next_eq!(
            rooms_subscriber,
            vec![
                SpaceServiceRoom::new_from_summary(
                    &RoomSummary::new(
                        child_space_id_1.to_owned(),
                        JoinRuleSummary::Public,
                        false,
                        uint!(1),
                        false,
                    ),
                    None,
                ),
                SpaceServiceRoom::new_from_summary(
                    &RoomSummary::new(
                        child_space_id_2.to_owned(),
                        JoinRuleSummary::Public,
                        false,
                        uint!(1),
                        false,
                    ),
                    None,
                ),
            ]
        );
    }
}
