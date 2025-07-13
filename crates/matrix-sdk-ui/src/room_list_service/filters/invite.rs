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

use super::{super::Room, Filter};

struct InviteRoomMatcher<F>
where
    F: Fn(&Room) -> RoomState,
{
    state: F,
}

impl<F> InviteRoomMatcher<F>
where
    F: Fn(&Room) -> RoomState,
{
    fn matches(&self, room: &Room) -> bool {
        (self.state)(room) == RoomState::Invited
    }
}

/// Create a new filter that will filter out rooms that are not invites (see
/// [`matrix_sdk_base::RoomState::Invited`]).
pub fn new_filter() -> impl Filter {
    let matcher = InviteRoomMatcher { state: move |room| room.state() };

    move |room| -> bool { matcher.matches(room) }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::test_utils::logged_in_client_with_server;
    use matrix_sdk_base::RoomState;
    use matrix_sdk_test::async_test;
    use ruma::room_id;

    use super::{super::new_rooms, *};

    #[async_test]
    async fn test_all_invite_kind() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        // When a room has been left, it doesn't match.
        let matcher = InviteRoomMatcher { state: |_| RoomState::Left };
        assert!(!matcher.matches(&room));

        // When a room has been joined, it doesn't match.
        let matcher = InviteRoomMatcher { state: |_| RoomState::Joined };
        assert!(!matcher.matches(&room));

        // When a room is an invite, it does match (unless it's empty).
        let matcher = InviteRoomMatcher { state: |_| RoomState::Invited };
        assert!(matcher.matches(&room));
    }
}
