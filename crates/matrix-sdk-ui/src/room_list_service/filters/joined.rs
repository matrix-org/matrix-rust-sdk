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

use matrix_sdk_base::RoomState;

use super::{super::RoomListItem, Filter};

fn matches<F>(state: F, room: &RoomListItem) -> bool
where
    F: Fn(&RoomListItem) -> RoomState,
{
    state(room) == RoomState::Joined
}

/// Create a new filter that will filter out rooms that are not joined (see
/// [`matrix_sdk_base::RoomState::Joined`]).
pub fn new_filter() -> impl Filter {
    let state = |room: &RoomListItem| room.cached_state;

    move |room| -> bool { matches(state, room) }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::test_utils::logged_in_client_with_server;
    use matrix_sdk_base::RoomState;
    use matrix_sdk_test::async_test;
    use ruma::room_id;

    use super::{super::new_rooms, *};

    #[async_test]
    async fn test_all_joined_kind() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        // When a room has been left, it doesn't match.
        assert!(!matches(|_| RoomState::Left, &room));

        // When a room is an invite, it doesn't match.
        assert!(!matches(|_| RoomState::Invited, &room));

        // When a room has been joined, it does match (unless it's empty).
        assert!(matches(|_| RoomState::Joined, &room));
    }
}
