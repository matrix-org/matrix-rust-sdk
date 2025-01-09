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

struct NonLeftRoomMatcher<F>
where
    F: Fn(&Room) -> RoomState,
{
    state: F,
}

impl<F> NonLeftRoomMatcher<F>
where
    F: Fn(&Room) -> RoomState,
{
    fn matches(&self, room: &Room) -> bool {
        match (self.state)(room) {
            RoomState::Joined | RoomState::Invited | RoomState::Knocked => true,
            RoomState::Left | RoomState::Banned => false,
        }
    }
}

/// Create a new filter that will filters out left rooms.
pub fn new_filter() -> impl Filter {
    let matcher = NonLeftRoomMatcher { state: move |room| room.state() };

    move |room| -> bool { matcher.matches(room) }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_base::RoomState;
    use matrix_sdk_test::async_test;
    use ruma::room_id;

    use super::{
        super::{client_and_server_prelude, new_rooms},
        *,
    };

    #[async_test]
    async fn test_all_non_left_kind_of_room_list_entry() {
        let (client, server, sliding_sync) = client_and_server_prelude().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server, &sliding_sync).await;

        // When a room has been left, it doesn't match.
        let matcher = NonLeftRoomMatcher { state: |_| RoomState::Left };
        assert!(!matcher.matches(&room));

        // When a room is in the banned state, it doesn't match either.
        let matcher = NonLeftRoomMatcher { state: |_| RoomState::Banned };
        assert!(!matcher.matches(&room));

        // When a room has been joined, it does match (unless it's empty).
        let matcher = NonLeftRoomMatcher { state: |_| RoomState::Joined };
        assert!(matcher.matches(&room));
    }
}
