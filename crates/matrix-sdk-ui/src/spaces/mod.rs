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

//! High level interfaces for working with Spaces
//!
//! See [`SpaceService`] for details.

use matrix_sdk::Client;
use ruma::OwnedRoomId;

pub use crate::spaces::{room::SpaceServiceRoom, room_list::SpaceServiceRoomList};

pub mod room;
pub mod room_list;

pub struct SpaceService {
    client: Client,
}

impl SpaceService {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    pub fn joined_spaces(&self) -> Vec<SpaceServiceRoom> {
        self.client
            .joined_rooms()
            .into_iter()
            .filter_map(|room| if room.is_space() { Some(room) } else { None })
            .map(SpaceServiceRoom::new_from_known)
            .collect::<Vec<_>>()
    }

    pub fn space_room_list(&self, space_id: OwnedRoomId) -> SpaceServiceRoomList {
        SpaceServiceRoomList::new(self.client.clone(), space_id)
    }
}

#[cfg(test)]
mod tests {
    use assert_matches2::assert_let;
    use matrix_sdk::{room::ParentSpace, test_utils::mocks::MatrixMockServer};
    use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
    use ruma::{RoomVersionId, room_id};
    use tokio_stream::StreamExt;

    use super::*;

    #[async_test]
    async fn test_spaces_hierarchy() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let space_service = SpaceService::new(client.clone());
        let factory = EventFactory::new();

        server.mock_room_state_encryption().plain().mount().await;

        // Given one parent space with 2 children spaces

        let parent_space_id = room_id!("!parent_space:example.org");
        let child_space_id_1 = room_id!("!child_space_1:example.org");
        let child_space_id_2 = room_id!("!child_space_2:example.org");

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(child_space_id_1)
                    .add_state_event(factory.create(user_id, RoomVersionId::V1).with_space_type())
                    .add_state_event(
                        factory
                            .space_parent(parent_space_id.to_owned(), child_space_id_1.to_owned())
                            .sender(user_id),
                    ),
            )
            .await;

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(child_space_id_2)
                    .add_state_event(factory.create(user_id, RoomVersionId::V1).with_space_type())
                    .add_state_event(
                        factory
                            .space_parent(parent_space_id.to_owned(), child_space_id_2.to_owned())
                            .sender(user_id),
                    ),
            )
            .await;
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(parent_space_id)
                    .add_state_event(factory.create(user_id, RoomVersionId::V1).with_space_type())
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

        // All joined
        assert_eq!(
            space_service.joined_spaces().iter().map(|s| s.room_id.to_owned()).collect::<Vec<_>>(),
            vec![child_space_id_1, child_space_id_2, parent_space_id]
        );

        let parent_space = client.get_room(parent_space_id).unwrap();
        assert!(parent_space.is_space());

        // Then the parent space and the two child spaces are linked

        let spaces: Vec<ParentSpace> = client
            .get_room(child_space_id_1)
            .unwrap()
            .parent_spaces()
            .await
            .unwrap()
            .map(Result::unwrap)
            .collect()
            .await;

        assert_let!(ParentSpace::Reciprocal(parent) = spaces.first().unwrap());
        assert_eq!(parent.room_id(), parent_space.room_id());

        let spaces: Vec<ParentSpace> = client
            .get_room(child_space_id_2)
            .unwrap()
            .parent_spaces()
            .await
            .unwrap()
            .map(Result::unwrap)
            .collect()
            .await;

        assert_let!(ParentSpace::Reciprocal(parent) = spaces.last().unwrap());
        assert_eq!(parent.room_id(), parent_space.room_id());
    }
}
