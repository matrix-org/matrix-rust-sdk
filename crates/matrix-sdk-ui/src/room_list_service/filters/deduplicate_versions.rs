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
// See the License for the specific language governing permissions and
// limitations under the License.

use matrix_sdk_base::RoomState;

use super::{super::RoomListItem, Filter};

type SuccessorRoomState = RoomState;

fn matches<F>(state: F, room: &RoomListItem) -> bool
where
    F: Fn(&RoomListItem) -> (RoomState, Option<SuccessorRoomState>),
{
    let (room_state, successor_room_state) = state(room);

    // Check if a room is one of the active versions.
    match (room_state, successor_room_state) {
        // This room is joined, and there is no successor. It is an active version.
        (RoomState::Joined, None) => true,

        // This room is joined, and there is a successor room. This successor room is joined,
        // left or banned, so this room is **not** the active version.
        (RoomState::Joined, Some(RoomState::Joined | RoomState::Left | RoomState::Banned)) => false,

        // This room is joined, and there is a successor room. This successor room is invited or
        // knocked, so this room **is** an active version.
        (RoomState::Joined, Some(RoomState::Invited | RoomState::Knocked)) => true,

        // This room is not joined. It is either left, invited, banned or knocked. The user is
        // not part of this room, but there is a successor room. This room is **not** the active
        // version, and should be hidden.
        (
            RoomState::Left | RoomState::Invited | RoomState::Banned | RoomState::Knocked,
            Some(_),
        ) => false,

        // This room is not joined. It is either left, invited, banned or knocked. The user is
        // not part of this room, and there may not be a successor. It should not be possible to
        // know if this room is tombstoned. Consequently, this room **is** the active version,
        // and should be visible.
        (RoomState::Left | RoomState::Invited | RoomState::Banned | RoomState::Knocked, None) => {
            true
        }
    }
}

/// Create a new filter that will filter out room versions that are outdated.
/// Only the “active” versions are kept.
///
/// A room version is considered active if and only if:
///
/// * the room is joined and has no successor,
/// * the room is joined and has a successor room that is invited or knocked,
/// * the room is left, invited, banned or knocked, and has no successor.
///
/// All other rooms are filtered out.
pub fn new_filter() -> impl Filter {
    let state = |room: &RoomListItem| {
        (
            room.cached_state,
            room.successor_room()
                .and_then(|successor_room| room.client().get_room(&successor_room.room_id))
                .map(|successor_room| successor_room.state()),
        )
    };

    move |room| -> bool { matches(state, room) }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use matrix_sdk::test_utils::logged_in_client_with_server;
    use matrix_sdk_base::RoomState;
    use matrix_sdk_test::async_test;
    use ruma::room_id;

    use super::{super::new_rooms, *};

    #[async_test]
    async fn test_room_a_is_joined_and_room_b_is_none() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        assert!(matches(|_room: &RoomListItem| (RoomState::Joined, None), &room));
    }

    #[async_test]
    async fn test_room_a_is_joined_and_room_b_is_joined() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        assert!(matches(|_| (RoomState::Joined, Some(SuccessorRoomState::Joined)), &room).not());
    }

    #[async_test]
    async fn test_room_a_is_joined_and_room_b_is_left() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        assert!(matches(|_| (RoomState::Joined, Some(SuccessorRoomState::Left)), &room).not());
    }

    #[async_test]
    async fn test_room_a_is_joined_and_room_b_is_banned() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        assert!(matches(|_| (RoomState::Joined, Some(SuccessorRoomState::Banned)), &room).not());
    }

    #[async_test]
    async fn test_room_a_is_joined_and_room_b_is_invited() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        assert!(matches(|_| (RoomState::Joined, Some(SuccessorRoomState::Invited)), &room));
    }

    #[async_test]
    async fn test_room_a_is_joined_and_room_b_is_knocked() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        assert!(matches(|_| (RoomState::Joined, Some(SuccessorRoomState::Knocked)), &room));
    }

    #[async_test]
    async fn test_room_a_is_left_and_room_b_is_joined() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        assert!(matches(|_| (RoomState::Left, Some(RoomState::Joined)), &room).not());
    }

    #[async_test]
    async fn test_room_a_is_invited_and_room_b_is_joined() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        assert!(matches(|_| (RoomState::Invited, Some(RoomState::Joined)), &room).not());
    }

    #[async_test]
    async fn test_room_a_is_banned_and_room_b_is_joined() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        assert!(matches(|_| (RoomState::Banned, Some(RoomState::Joined)), &room).not());
    }

    #[async_test]
    async fn test_room_a_is_knocked_and_room_b_is_joined() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        assert!(matches(|_| (RoomState::Knocked, Some(RoomState::Joined)), &room).not());
    }

    #[async_test]
    async fn test_room_a_is_left_and_room_b_is_none() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        let state = |_: &RoomListItem| (RoomState::Left, None);
        assert!(matches(state, &room));
    }

    #[async_test]
    async fn test_room_a_is_invited_and_room_b_is_none() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        let state = |_: &RoomListItem| (RoomState::Invited, None);
        assert!(matches(state, &room));
    }

    #[async_test]
    async fn test_room_a_is_banned_and_room_b_is_none() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        let state = |_: &RoomListItem| (RoomState::Banned, None);
        assert!(matches(state, &room));
    }

    #[async_test]
    async fn test_room_a_is_knocked_and_room_b_is_none() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        let state = |_: &RoomListItem| (RoomState::Knocked, None);
        assert!(matches(state, &room));
    }
}
