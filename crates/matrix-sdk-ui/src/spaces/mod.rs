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
//! See [`SpaceDiscoveryService`] for details.

use matrix_sdk::{Client, Error};
use ruma::OwnedRoomId;
use ruma::api::client::space::get_hierarchy;

pub use crate::spaces::room::SpaceServiceRoom;

pub mod room;

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
            .map(|room| SpaceServiceRoom::new_from_known(room))
            .collect::<Vec<_>>()
    }

    pub async fn top_level_children_for(
        &self,
        space_id: OwnedRoomId,
    ) -> Result<Vec<SpaceServiceRoom>, Error> {
        let request = get_hierarchy::v1::Request::new(space_id.clone());

        let result = self.client.send(request).await?;

        Ok(result
            .rooms
            .iter()
            .map(|room| (&room.summary, self.client.get_room(&room.summary.room_id)))
            .map(|(summary, room)| SpaceServiceRoom::new_from_summary(summary, room))
            .collect::<Vec<_>>())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches2::assert_let;
    use matrix_sdk::{room::ParentSpace, test_utils::mocks::MatrixMockServer};
    use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
    use ruma::{RoomVersionId, room_id};
    use tokio_stream::StreamExt;

    #[async_test]
    async fn test_spaces_hierarchy() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let space_service = SpaceService::new(client.clone());
        let factory = EventFactory::new();

        server.mock_room_state_encryption().plain().mount().await;

        // Given one parent space with 2 children spaces

        let parent_space_id = room_id!("!3:example.org");
        let child_space_id_1 = room_id!("!1:example.org");
        let child_space_id_2 = room_id!("!2:example.org");

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
