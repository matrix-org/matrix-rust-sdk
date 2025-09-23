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

use super::{super::RoomListItem, Filter};

fn matches<F>(is_space: F, room: &RoomListItem) -> bool
where
    F: Fn(&RoomListItem) -> bool,
{
    is_space(room)
}

/// Create a new filter that will filter out rooms that are spaces, i.e.
/// room with a `room_type` of `m.space` as defined in <https://spec.matrix.org/latest/client-server-api/#spaces>
pub fn new_filter() -> impl Filter {
    let is_space = |room: &RoomListItem| room.cached_is_space;

    move |room| -> bool { matches(is_space, room) }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::test_utils::logged_in_client_with_server;
    use matrix_sdk_test::async_test;
    use ruma::room_id;

    use super::{super::new_rooms, *};

    #[async_test]
    async fn test_space() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        assert!(!matches(|_| false, &room));
        assert!(matches(|_| true, &room));
    }
}
