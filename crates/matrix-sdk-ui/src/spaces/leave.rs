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

use matrix_sdk::{Client, RoomState, room::RoomMemberRole};
use ruma::{Int, OwnedRoomId};

use crate::spaces::{Error, SpaceRoom};

/// Space leaving specific room that groups normal [`SpaceRoom`] details with
/// information about the leaving user's role.
#[derive(Debug, Clone)]
pub struct LeaveSpaceRoom {
    /// The underlying [`SpaceRoom`]
    pub space_room: SpaceRoom,
    /// Whether the user is the last admin in the room. This helps clients
    /// better inform the user about the consequences of leaving the room.
    pub is_last_admin: bool,
}

/// The `LeaveSpaceHandle` processes rooms to be left in the order they were
/// provided by the [`crate::spaces::SpaceService`] and annotates them with
/// extra data to inform the leave process e.g. if the current user is the last
/// room admin.
///
/// Once the upstream client decides what rooms should actually be left, the
/// handle provides a method to execute that too.
pub struct LeaveSpaceHandle {
    client: Client,
    rooms: Vec<LeaveSpaceRoom>,
}

impl LeaveSpaceHandle {
    pub(crate) async fn new(client: Client, room_ids: Vec<OwnedRoomId>) -> Self {
        let mut rooms = Vec::new();

        for room_id in &room_ids {
            let Some(room) = client.get_room(room_id) else {
                continue;
            };

            if room.state() != RoomState::Joined {
                continue;
            }

            let users_to_power_levels = room.users_with_power_levels().await;

            let is_last_admin = users_to_power_levels
                .iter()
                .filter(|(_, power_level)| {
                    let Some(power_level) = Int::new(**power_level) else {
                        return false;
                    };

                    RoomMemberRole::suggested_role_for_power_level(power_level.into())
                        == RoomMemberRole::Administrator
                })
                .map(|p| p.0)
                .collect::<Vec<_>>()
                == vec![room.own_user_id()];

            rooms.push(LeaveSpaceRoom {
                space_room: SpaceRoom::new_from_known(&room, 0),
                is_last_admin,
            });
        }

        Self { client, rooms }
    }

    /// A list of rooms to be left which next to normal [`SpaceRoom`] data also
    /// include leave specific information.
    pub fn rooms(&self) -> &Vec<LeaveSpaceRoom> {
        &self.rooms
    }

    /// Bulk leave the given rooms. Stops when encountering an error.
    pub async fn leave(&self, filter: impl FnMut(&LeaveSpaceRoom) -> bool) -> Result<(), Error> {
        for room in self.rooms.clone().into_iter().filter(filter) {
            if let Some(room) = self.client.get_room(&room.space_room.room_id) {
                room.leave().await.map_err(Error::LeaveSpace)?;
            } else {
                return Err(Error::RoomNotFound(room.space_room.room_id));
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use matrix_sdk::test_utils::mocks::MatrixMockServer;
    use matrix_sdk_test::{
        InvitedRoomBuilder, JoinedRoomBuilder, LeftRoomBuilder, async_test,
        event_factory::EventFactory,
    };
    use ruma::{RoomVersionId, owned_user_id, room_id};

    use crate::spaces::SpaceService;

    #[async_test]
    async fn test_leaving() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let user_id = client.user_id().unwrap();
        let space_service = SpaceService::new(client.clone());
        let factory = EventFactory::new().sender(user_id);

        server.mock_room_state_encryption().plain().mount().await;

        // Given one parent space with 2 children spaces

        let parent_space_id = room_id!("!parent_space:example.org");
        let child_space_id_1 = room_id!("!child_space_1:example.org");
        let child_space_id_2 = room_id!("!child_space_2:example.org");
        let left_room_id = room_id!("!left_room:example.org");
        let invited_room_id = room_id!("!invited_room:example.org");

        let mut power_levels = BTreeMap::from([
            (user_id.to_owned(), 100.into()),
            (owned_user_id!("@some_non_admin:a.b"), 50.into()),
        ]);

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(child_space_id_1)
                    .add_state_event(factory.create(user_id, RoomVersionId::V1).with_space_type())
                    .add_state_event(
                        factory
                            .space_parent(parent_space_id.to_owned(), child_space_id_1.to_owned()),
                    )
                    .add_state_event(
                        factory.power_levels(&mut power_levels).state_key("").sender(user_id),
                    ),
            )
            .await;

        power_levels.insert(owned_user_id!("@some_other_admin:a.b"), 100.into());

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(child_space_id_2)
                    .add_state_event(factory.create(user_id, RoomVersionId::V1).with_space_type())
                    .add_state_event(
                        factory
                            .space_parent(parent_space_id.to_owned(), child_space_id_2.to_owned()),
                    )
                    .add_state_event(
                        factory.power_levels(&mut power_levels).state_key("").sender(user_id),
                    ),
            )
            .await;

        server.sync_room(&client, LeftRoomBuilder::new(invited_room_id)).await;
        server.sync_room(&client, InvitedRoomBuilder::new(invited_room_id)).await;

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(parent_space_id)
                    .add_state_event(factory.create(user_id, RoomVersionId::V1).with_space_type())
                    .add_state_event(
                        factory
                            .space_child(parent_space_id.to_owned(), child_space_id_1.to_owned()),
                    )
                    .add_state_event(
                        factory
                            .space_child(parent_space_id.to_owned(), child_space_id_2.to_owned()),
                    )
                    .add_state_event(
                        factory.space_child(parent_space_id.to_owned(), left_room_id.to_owned()),
                    )
                    .add_state_event(
                        factory.space_child(parent_space_id.to_owned(), invited_room_id.to_owned()),
                    ),
            )
            .await;

        server.mock_room_leave().ok(room_id!("!does_not_matter:a:b")).mount().await;

        assert!(!space_service.joined_spaces().await.is_empty());

        let handle = space_service.leave_space(parent_space_id).await.unwrap();

        let rooms = handle.rooms();

        let child_room_1 = &rooms[0];
        assert!(child_room_1.is_last_admin);

        let child_room_2 = &rooms[1];
        assert!(!child_room_2.is_last_admin);

        let room_ids = rooms.iter().map(|r| r.space_room.room_id.clone()).collect::<Vec<_>>();
        assert_eq!(room_ids, vec![child_space_id_1, child_space_id_2, parent_space_id]);

        handle.leave(|room| room_ids.contains(&room.space_room.room_id)).await.unwrap();

        assert!(space_service.joined_spaces().await.is_empty());
    }
}
