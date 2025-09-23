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

fn matches<F>(is_low_priority: F, room: &RoomListItem) -> bool
where
    F: Fn(&RoomListItem) -> bool,
{
    is_low_priority(room)
}

/// Create a new filter that will filter out rooms that are not marked as
/// low priority (see [`matrix_sdk_base::Room::is_low_priority`]).
pub fn new_filter() -> impl Filter {
    let is_low_priority = |room: &RoomListItem| room.is_low_priority();

    move |room| -> bool { matches(is_low_priority, room) }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use matrix_sdk::test_utils::logged_in_client_with_server;
    use matrix_sdk_test::async_test;
    use ruma::room_id;

    use super::{super::new_rooms, *};

    #[async_test]
    async fn test_is_low_priority() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        assert!(matches(|_| true, &room));
    }

    #[async_test]
    async fn test_is_not_low_priority() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        assert!(matches(|_| false, &room).not());
    }
}
