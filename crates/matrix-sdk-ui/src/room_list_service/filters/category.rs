// Copyright 2024 The Matrix.org Foundation C.I.C.
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

use super::{super::RoomListItem, Filter};

/// An enum to represent whether a room is about “people” (strictly 2 users) or
/// “group” (1 or more than 2 users).
///
/// Ideally, we would only want to rely on the
/// [`matrix_sdk::BaseRoom::is_direct`] method, but the rules are a little bit
/// different for this high-level UI API.
///
/// This is implemented this way so that it's impossible to filter by “group”
/// and by “people” at the same time: these criteria are mutually
/// exclusive by design per filter.
#[derive(Copy, Clone, PartialEq)]
pub enum RoomCategory {
    Group,
    People,
}

/// Checks if the room belongs in the expected category based on whether it's a
/// DM or not.
fn matches(room: &RoomListItem, expected_kind: RoomCategory) -> bool {
    let kind = if room.is_dm() { RoomCategory::People } else { RoomCategory::Group };

    kind == expected_kind
}

/// Create a new filter that will accept all rooms that fit in the
/// `expected_category`. The category is defined by [`RoomCategory`], see this
/// type to learn more.
pub fn new_filter(expected_category: RoomCategory) -> impl Filter {
    move |room| -> bool { matches(room, expected_category) }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use matrix_sdk::{Client, test_utils::mocks::MatrixMockServer};
    use matrix_sdk_base::DmRoomDefinition;
    use matrix_sdk_test::{JoinedRoomBuilder, async_test, event_factory::EventFactory};
    use ruma::{UserId, room_id};

    use crate::room_list_service::{
        RoomListItem,
        filters::{RoomCategory, category::matches},
    };

    async fn setup_room(
        client: &Client,
        server: &MatrixMockServer,
        targets: u64,
        joined_members: u32,
    ) -> RoomListItem {
        let room_id = room_id!("!a:b.c");
        server
            .mock_sync()
            .ok_and_run(client, |builder| {
                let mut direct = EventFactory::new().direct();
                for idx in 0..targets {
                    direct = direct.add_user(
                        UserId::parse(format!("@user_{idx}:example.org"))
                            .expect("UserId is valid")
                            .into(),
                        room_id,
                    );
                }

                let mut joined_room_builder = JoinedRoomBuilder::new(room_id);
                if joined_members > 0 {
                    joined_room_builder =
                        joined_room_builder.set_joined_members_count(joined_members);
                }
                builder.add_global_account_data(direct).add_joined_room(joined_room_builder);
            })
            .await;

        client.get_room(room_id).expect("Room should exist").into()
    }

    #[async_test]
    async fn test_kind_is_group_with_matrix_spec_definition() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room = setup_room(&client, &server, 42, 10).await;

        // Expect `People`.
        {
            let expected_kind = RoomCategory::People;

            assert!(matches(&room, expected_kind).not());
        }

        // Expect `Group`.
        {
            let expected_kind = RoomCategory::Group;

            assert!(matches(&room, expected_kind));
        }
    }

    #[async_test]
    async fn test_kind_is_people_with_matrix_spec_definition() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room = setup_room(&client, &server, 1, 10).await;

        // Expect `People`.
        {
            let expected_kind = RoomCategory::People;

            assert!(matches(&room, expected_kind));
        }

        // Expect `Group`.
        {
            let expected_kind = RoomCategory::Group;

            assert!(matches(&room, expected_kind).not());
        }
    }

    #[async_test]
    async fn test_kind_is_group_with_two_people_definition() {
        let server = MatrixMockServer::new().await;
        let client = server
            .client_builder()
            .on_builder(|b| b.dm_room_definition(DmRoomDefinition::TwoMembers))
            .build()
            .await;

        // We have 3 members in the room.
        let room = setup_room(&client, &server, 1, 3).await;

        // It's Group.
        {
            let expected_kind = RoomCategory::Group;

            assert!(matches(&room, expected_kind));
        }
    }

    #[async_test]
    async fn test_kind_is_people_with_two_people_definition() {
        let server = MatrixMockServer::new().await;
        let client = server
            .client_builder()
            .on_builder(|b| b.dm_room_definition(DmRoomDefinition::TwoMembers))
            .build()
            .await;

        // The room has 2 members, but it has no direct targets, so it should not be
        // considered a `People` room.
        let room = setup_room(&client, &server, 0, 2).await;
        assert!(matches(&room, RoomCategory::People).not());

        // The room has 2 members, but it has several direct targets, so it should not
        // be considered a `People` room.
        let room = setup_room(&client, &server, 42, 2).await;
        assert!(matches(&room, RoomCategory::People).not());

        // The room has 2 members and a single target, so it should be considered a
        // `People` room.
        let room = setup_room(&client, &server, 1, 2).await;
        assert!(matches(&room, RoomCategory::People));

        // The room has 1 member, so it should be considered a `People` room.
        let room = setup_room(&client, &server, 1, 1).await;
        assert!(matches(&room, RoomCategory::People));

        // The room has no members, so it should be considered a `People` room.
        let room = setup_room(&client, &server, 1, 0).await;
        assert!(matches(&room, RoomCategory::People));
    }
}
