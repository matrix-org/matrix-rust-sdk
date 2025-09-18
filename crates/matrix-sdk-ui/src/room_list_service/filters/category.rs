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

type DirectTargetsLength = usize;

/// _Direct targets_ mean the number of users in a direct room, except us.
/// So if it returns 1, it means there are 2 users in the direct room.
fn matches<F>(number_of_direct_targets: F, room: &RoomListItem, expected_kind: RoomCategory) -> bool
where
    F: Fn(&RoomListItem) -> Option<DirectTargetsLength>,
{
    let kind = match number_of_direct_targets(room) {
        // If 1, we are sure it's a direct room between two users. It's the strict
        // definition of the `People` category, all good.
        Some(1) => RoomCategory::People,

        // If smaller than 1, we are not sure it's a direct room, it's then a `Group`.
        // If greater than 1, we are sure it's a direct room but not between
        // two users, so it's a `Group` based on our expectation.
        Some(_) => RoomCategory::Group,

        // Don't know.
        None => return false,
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

    use matrix_sdk::test_utils::logged_in_client_with_server;
    use matrix_sdk_test::async_test;
    use ruma::room_id;

    use super::{super::new_rooms, *};

    #[async_test]
    async fn test_kind_is_group() {
        let (client, server) = logged_in_client_with_server().await;
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
    async fn test_kind_is_people() {
        let (client, server) = logged_in_client_with_server().await;
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
    async fn test_room_kind_cannot_be_found() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        let number_of_direct_targets = |_room: &RoomListItem| None;

        assert!(matches(number_of_direct_targets, &room, RoomCategory::Group).not());
    }
}
