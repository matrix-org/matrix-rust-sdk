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

use matrix_sdk_base::DmRoomDefinition;

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

type DirectTargetsLength = usize;

/// _Direct targets_ mean the number of users in a direct room, except us.
/// So if it returns 1, it means there are 2 users in the direct room.
fn matches<F>(number_of_direct_targets: F, room: &RoomListItem, expected_kind: RoomCategory) -> bool
where
    F: Fn(&RoomListItem) -> Option<DirectTargetsLength>,
{
    let client = room.client();
    let dm_room_definition = client.dm_room_definition();
    let Some(targets) = number_of_direct_targets(room) else {
        // Don't know.
        return false;
    };
    let kind = match dm_room_definition {
        DmRoomDefinition::MatrixSpec => {
            if targets == 1 {
                // If there is a single target, it's a direct room.
                RoomCategory::People
            } else {
                // If smaller than 1, we are not sure it's a direct room, it's then a `Group`.
                // If greater than 1, we are sure it's a direct room but not between
                // two users, so it's a `Group` based on our expectation.
                RoomCategory::Group
            }
        }
        DmRoomDefinition::TwoMembers => {
            // We can't fully rely on the `service_members` value, as it only contains the
            // user ids that should be considered service members in the room,
            // not the service members that are active in the room. We can only
            // make a good guess and assume those service members are in the room.
            // Note this can return false positives if some service members aren't in the
            // room, but they were replaced by other members.
            let service_member_count =
                room.service_members().map(|members| members.len()).unwrap_or_default() as u64;
            let active_non_service_members =
                room.active_members_count().saturating_sub(service_member_count);
            if targets == 1 && active_non_service_members <= 2 {
                // If lower than or equal to 2, we are sure it's a direct room between two
                // users.
                RoomCategory::People
            } else {
                // If smaller than 1, we are not sure it's a direct room, it's then a `Group`.
                // If greater than 1, we are sure it's a direct room but not between
                // two users, so it's a `Group` based on our expectation.
                RoomCategory::Group
            }
        }
    };

    kind == expected_kind
}

/// Create a new filter that will accept all rooms that fit in the
/// `expected_category`. The category is defined by [`RoomCategory`], see this
/// type to learn more.
pub fn new_filter(expected_category: RoomCategory) -> impl Filter {
    let number_of_direct_targets = move |room: &RoomListItem| Some(room.direct_targets_length());

    move |room| -> bool { matches(number_of_direct_targets, room, expected_category) }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use matrix_sdk::test_utils::mocks::{AnyRoomBuilder, MatrixMockServer};
    use matrix_sdk_test::{JoinedRoomBuilder, async_test};
    use ruma::room_id;

    use super::{super::new_rooms, *};

    #[async_test]
    async fn test_kind_is_group_with_matrix_spec_definition() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        let number_of_direct_targets = |_room: &RoomListItem| Some(42);

        // Expect `People`.
        {
            let expected_kind = RoomCategory::People;

            assert!(matches(number_of_direct_targets, &room, expected_kind).not());
        }

        // Expect `Group`.
        {
            let expected_kind = RoomCategory::Group;

            assert!(matches(number_of_direct_targets, &room, expected_kind));
        }
    }

    #[async_test]
    async fn test_kind_is_people_with_matrix_spec_definition() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        let number_of_direct_targets = |_room: &RoomListItem| Some(1);

        // Expect `People`.
        {
            let expected_kind = RoomCategory::People;

            assert!(matches(number_of_direct_targets, &room, expected_kind));
        }

        // Expect `Group`.
        {
            let expected_kind = RoomCategory::Group;

            assert!(matches(number_of_direct_targets, &room, expected_kind).not());
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
        let room_id = room_id!("!a:b.c");

        // We have 3 members in the room.
        let room = server
            .sync_room(
                &client,
                AnyRoomBuilder::Joined(JoinedRoomBuilder::new(room_id).set_joined_members_count(3)),
            )
            .await;

        let number_of_direct_targets = |_room: &RoomListItem| Some(1);

        // It's Group.
        {
            let expected_kind = RoomCategory::Group;

            assert!(matches(number_of_direct_targets, &room.into(), expected_kind));
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
        let room_id = room_id!("!a:b.c");

        // The room has 2 members, but it has no direct targets, so it should not be
        // considered a `People` room.
        let room = server
            .sync_room(
                &client,
                AnyRoomBuilder::Joined(JoinedRoomBuilder::new(room_id).set_joined_members_count(2)),
            )
            .await;

        let number_of_direct_targets = |_room: &RoomListItem| Some(0);

        // It's not People.
        {
            let expected_kind = RoomCategory::People;

            assert!(matches(number_of_direct_targets, &room.into(), expected_kind).not());
        }

        // The room has 2 members, but it has several direct targets, so it should not
        // be considered a `People` room.
        let room = server
            .sync_room(
                &client,
                AnyRoomBuilder::Joined(JoinedRoomBuilder::new(room_id).set_joined_members_count(2)),
            )
            .await;

        let number_of_direct_targets = |_room: &RoomListItem| Some(43);

        // It's not People.
        {
            let expected_kind = RoomCategory::People;

            assert!(matches(number_of_direct_targets, &room.into(), expected_kind).not());
        }

        // The room has 2 members and a single target, so it should be considered a
        // `People` room.
        let room = server
            .sync_room(
                &client,
                AnyRoomBuilder::Joined(JoinedRoomBuilder::new(room_id).set_joined_members_count(2)),
            )
            .await;

        let number_of_direct_targets = |_room: &RoomListItem| Some(1);

        // It's People.
        {
            let expected_kind = RoomCategory::People;

            assert!(matches(number_of_direct_targets, &room.into(), expected_kind));
        }

        // The room has 1 member, so it should be considered a `People` room.
        let room = server
            .sync_room(
                &client,
                AnyRoomBuilder::Joined(JoinedRoomBuilder::new(room_id).set_joined_members_count(1)),
            )
            .await;

        let number_of_direct_targets = |_room: &RoomListItem| Some(1);

        // It's People.
        {
            let expected_kind = RoomCategory::People;

            assert!(matches(number_of_direct_targets, &room.into(), expected_kind));
        }

        // The room has no members, so it should be considered a `People` room.
        let room = server
            .sync_room(
                &client,
                AnyRoomBuilder::Joined(JoinedRoomBuilder::new(room_id).set_joined_members_count(0)),
            )
            .await;

        let number_of_direct_targets = |_room: &RoomListItem| Some(1);

        // It's People.
        {
            let expected_kind = RoomCategory::People;

            assert!(matches(number_of_direct_targets, &room.into(), expected_kind));
        }
    }

    #[async_test]
    async fn test_room_kind_cannot_be_found() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        let number_of_direct_targets = |_room: &RoomListItem| None;

        assert!(matches(number_of_direct_targets, &room, RoomCategory::Group).not());
    }
}
