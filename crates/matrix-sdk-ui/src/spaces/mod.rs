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

pub struct SpaceService {
    client: Client,
}

impl SpaceService {
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    pub fn joined_spaces(&self) -> Vec<OwnedRoomId> {
        self.client
            .joined_rooms()
            .into_iter()
            .filter_map(|room| if room.is_space() { Some(room.room_id().to_owned()) } else { None })
            .collect()
    }

    pub async fn top_level_children_for(
        &self,
        space_id: OwnedRoomId,
    ) -> Result<Vec<OwnedRoomId>, Error> {
        let request = get_hierarchy::v1::Request::new(space_id.clone());

        let result = self.client.send(request).await?;

        println!("Top level children for space {}: {:?}", space_id, result.rooms);

        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches2::assert_let;
    use matrix_sdk::{room::ParentSpace, test_utils::mocks::MatrixMockServer};
    use matrix_sdk_test::{JoinedRoomBuilder, StateTestEvent, async_test};
    use ruma::room_id;
    use tokio_stream::StreamExt;

    #[async_test]
    async fn test_joined_spaces() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let space_discovery_service = SpaceService::new(client.clone());

        server.mock_room_state_encryption().plain().mount().await;

        let parent_space_id = room_id!("!3:example.org");
        let child_space_id_1 = room_id!("!1:example.org");
        let child_space_id_2 = room_id!("!2:example.org");

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(child_space_id_1)
                    .add_state_event(StateTestEvent::CreateSpace)
                    .add_state_event(StateTestEvent::SpaceParent {
                        parent: parent_space_id.to_owned(),
                        child: child_space_id_1.to_owned(),
                    }),
            )
            .await;

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(child_space_id_2)
                    .add_state_event(StateTestEvent::CreateSpace)
                    .add_state_event(StateTestEvent::SpaceParent {
                        parent: parent_space_id.to_owned(),
                        child: child_space_id_2.to_owned(),
                    }),
            )
            .await;
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(parent_space_id)
                    .add_state_event(StateTestEvent::CreateSpace)
                    .add_state_event(StateTestEvent::SpaceChild {
                        parent: parent_space_id.to_owned(),
                        child: child_space_id_1.to_owned(),
                    })
                    .add_state_event(StateTestEvent::SpaceChild {
                        parent: parent_space_id.to_owned(),
                        child: child_space_id_2.to_owned(),
                    }),
            )
            .await;

        assert_eq!(
            space_discovery_service.joined_spaces(),
            vec![child_space_id_1, child_space_id_2, parent_space_id]
        );

        let parent_space = client.get_room(parent_space_id).unwrap();
        assert!(parent_space.is_space());

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
