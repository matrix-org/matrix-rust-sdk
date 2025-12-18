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

use ruma::OwnedRoomId;

use super::Filter;

/// Create a new filter that will filter out rooms that are not part of the
/// given identifiers array
pub fn new_filter(identifiers: Vec<OwnedRoomId>) -> impl Filter {
    move |room| -> bool { identifiers.contains(&room.room_id().to_owned()) }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::test_utils::logged_in_client_with_server;
    use matrix_sdk_test::async_test;
    use ruma::{owned_room_id, room_id};

    use super::{super::new_rooms, *};

    #[async_test]
    async fn test_space() {
        let (client, server) = logged_in_client_with_server().await;
        let rooms = new_rooms(
            [room_id!("!alpha:b.c"), room_id!("!beta:b.c"), room_id!("!gamma:b.c")],
            &client,
            &server,
        )
        .await;

        let filter = new_filter(vec![owned_room_id!("!beta:b.c")]);

        assert!(!filter(&rooms[0]));
        assert!(filter(&rooms[1]));
        assert!(!filter(&rooms[2]));
    }
}
