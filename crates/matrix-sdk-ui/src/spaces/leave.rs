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

use matrix_sdk::{Client, ROOM_VERSION_RULES_FALLBACK, RoomState, room::RoomMemberRole};
use ruma::{Int, OwnedRoomId, events::room::member::MembershipState};
use tracing::info;

use crate::spaces::{Error, SpaceRoom};

/// Space leaving specific room that groups normal [`SpaceRoom`] details with
/// information about the leaving user's role.
#[derive(Debug, Clone)]
pub struct LeaveSpaceRoom {
    /// The underlying [`SpaceRoom`]
    pub space_room: SpaceRoom,
    /// Whether the user is the last owner in the room. This helps clients
    /// better inform the user about the consequences of leaving the room.
    pub is_last_owner: bool,
    /// If the room creators have infinite PL.
    pub are_creators_privileged: bool,
}

/// The `LeaveSpaceHandle` processes rooms to be left in the order they were
/// provided by the [`crate::spaces::SpaceService`] and annotates them with
/// extra data to inform the leave process e.g. if the current user is the last
/// room owner.
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

            if !room.are_members_synced() {
                info!("Syncing members for room {} to check if can leave", room.room_id());
                _ = room.sync_members().await.ok();
            }

            let mut privileged_creator_ids = Vec::new();
            let mut are_creators_privileged = false;
            if let Some(mut create) = room.create_content() {
                let rules = create.room_version.rules().unwrap_or(ROOM_VERSION_RULES_FALLBACK);
                if rules.authorization.explicitly_privilege_room_creators {
                    are_creators_privileged = true;
                    privileged_creator_ids.push(create.creator);
                    privileged_creator_ids.append(&mut create.additional_creators);
                }
            }

            let owner_ids = room
                .users_with_power_levels()
                .await
                .into_iter()
                .filter(|(_, power_level)| {
                    let Some(power_level) = Int::new(*power_level) else {
                        return false;
                    };

                    if are_creators_privileged {
                        power_level >= ruma::int!(150)
                    } else {
                        RoomMemberRole::suggested_role_for_power_level(power_level.into())
                            == RoomMemberRole::Administrator
                    }
                })
                .map(|p: (ruma::OwnedUserId, i64)| p.0)
                .chain(privileged_creator_ids.into_iter());

            let mut joined_owner_ids = Vec::new();
            for owner_id in owner_ids {
                if let Ok(Some(member)) = room.get_member_no_sync(&owner_id).await
                    && *member.membership() == MembershipState::Join
                {
                    joined_owner_ids.push(owner_id);
                }
            }
            let is_last_owner = joined_owner_ids == [room.own_user_id()];

            rooms.push(LeaveSpaceRoom {
                space_room: SpaceRoom::new_from_known(&room, 0),
                is_last_owner,
                are_creators_privileged,
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
        let space_service = SpaceService::new(client.clone()).await;
        let factory = EventFactory::new().sender(user_id);

        server.mock_room_state_encryption().plain().mount().await;

        // Given one parent space with 2 children spaces

        let parent_space_id = room_id!("!parent_space:example.org");
        let child_space_id_1 = room_id!("!child_space_1:example.org");
        let child_space_id_2 = room_id!("!child_space_2:example.org");
        let child_space_v12_id_1 = room_id!("!child_space_v12_1:example.org");
        let child_space_v12_id_2 = room_id!("!child_space_v12_2:example.org");
        let left_room_id = room_id!("!left_room:example.org");
        let invited_room_id = room_id!("!invited_room:example.org");

        let some_non_admin_id = owned_user_id!("@some_non_admin:a.b");
        let mut power_levels = BTreeMap::from([
            (user_id.to_owned(), 100.into()),
            (some_non_admin_id.clone(), 50.into()),
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
                    )
                    .add_state_event(factory.member(user_id).state_key(user_id.to_string()))
                    .add_state_event(
                        factory.member(&some_non_admin_id).state_key(some_non_admin_id.to_string()),
                    )
                    .set_joined_members_count(2),
            )
            .await;

        let other_admin_user_id = owned_user_id!("@some_other_admin:a.b");
        power_levels.insert(other_admin_user_id.clone(), 100.into());

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
                    )
                    .add_state_event(factory.member(user_id).state_key(user_id.to_string()))
                    .add_state_event(
                        factory.member(&some_non_admin_id).state_key(some_non_admin_id.to_string()),
                    )
                    .add_state_event(
                        factory
                            .member(&other_admin_user_id)
                            .state_key(other_admin_user_id.to_string()),
                    )
                    .set_joined_members_count(3),
            )
            .await;

        // The creator is not supposed to be in the power levels on v12
        let mut power_levels = BTreeMap::from([
            (some_non_admin_id.clone(), 50.into()),
            (other_admin_user_id.clone(), 100.into()),
        ]);
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(child_space_v12_id_1)
                    .add_state_event(factory.create(user_id, RoomVersionId::V12).with_space_type())
                    .add_state_event(
                        factory.space_parent(
                            parent_space_id.to_owned(),
                            child_space_v12_id_1.to_owned(),
                        ),
                    )
                    .add_state_event(factory.member(user_id).state_key(user_id.to_string()))
                    .add_state_event(
                        factory.member(&some_non_admin_id).state_key(some_non_admin_id.to_string()),
                    )
                    .add_state_event(
                        factory
                            .member(&other_admin_user_id)
                            .state_key(other_admin_user_id.to_string()),
                    )
                    .add_state_event(
                        factory.power_levels(&mut power_levels).state_key("").sender(user_id),
                    )
                    .set_joined_members_count(3),
            )
            .await;

        let other_owner_user_id = owned_user_id!("@some_other_owner:a.b");
        power_levels.insert(other_owner_user_id.clone(), 150.into());
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(child_space_v12_id_2)
                    .add_state_event(factory.create(user_id, RoomVersionId::V12).with_space_type())
                    .add_state_event(
                        factory.space_parent(
                            parent_space_id.to_owned(),
                            child_space_v12_id_2.to_owned(),
                        ),
                    )
                    .add_state_event(factory.member(user_id).state_key(user_id.to_string()))
                    .add_state_event(
                        factory.member(&some_non_admin_id).state_key(some_non_admin_id.to_string()),
                    )
                    .add_state_event(
                        factory
                            .member(&other_admin_user_id)
                            .state_key(other_admin_user_id.to_string()),
                    )
                    .add_state_event(
                        factory
                            .member(&other_owner_user_id)
                            .state_key(other_owner_user_id.to_string()),
                    )
                    .add_state_event(
                        factory.power_levels(&mut power_levels).state_key("").sender(user_id),
                    )
                    .set_joined_members_count(4),
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
                        factory.space_child(
                            parent_space_id.to_owned(),
                            child_space_v12_id_1.to_owned(),
                        ),
                    )
                    .add_state_event(
                        factory.space_child(
                            parent_space_id.to_owned(),
                            child_space_v12_id_2.to_owned(),
                        ),
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

        assert!(!space_service.top_level_joined_spaces().await.is_empty());

        let handle = space_service.leave_space(parent_space_id).await.unwrap();

        let rooms = handle.rooms();

        let child_room_1 = &rooms[0];
        assert!(child_room_1.is_last_owner);
        assert_eq!(child_room_1.space_room.num_joined_members, 2);
        assert!(!child_room_1.are_creators_privileged);

        let child_room_2 = &rooms[1];
        assert!(!child_room_2.is_last_owner);
        assert_eq!(child_room_2.space_room.num_joined_members, 3);
        assert!(!child_room_2.are_creators_privileged);

        let child_room_3 = &rooms[2];
        assert!(child_room_3.is_last_owner);
        assert_eq!(child_room_3.space_room.num_joined_members, 3);
        assert!(child_room_3.are_creators_privileged);

        let child_room_4 = &rooms[3];
        assert!(!child_room_4.is_last_owner);
        assert_eq!(child_room_4.space_room.num_joined_members, 4);
        assert!(child_room_4.are_creators_privileged);

        let room_ids = rooms.iter().map(|r| r.space_room.room_id.clone()).collect::<Vec<_>>();
        assert_eq!(
            room_ids,
            vec![
                child_space_id_1,
                child_space_id_2,
                child_space_v12_id_1,
                child_space_v12_id_2,
                parent_space_id
            ]
        );

        handle.leave(|room| room_ids.contains(&room.space_room.room_id)).await.unwrap();

        assert!(space_service.top_level_joined_spaces().await.is_empty());
    }
}
