// Copyright 2026 The Matrix.org Foundation C.I.C.
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

use matrix_sdk_base::read_receipts::RoomReadReceipts;

use super::{super::RoomListItem, Filter};

fn matches<F>(read_receipts: F, room: &RoomListItem) -> bool
where
    F: Fn(&RoomListItem) -> RoomReadReceipts,
{
    read_receipts(room).num_mentions > 0
}

/// Create a new filter that will filter out rooms that have no unread mentions
/// (as computed client-side and exposed via
/// [`matrix_sdk_base::Room::num_unread_mentions`]).
pub fn new_filter() -> impl Filter {
    let read_receipts = |room: &RoomListItem| room.read_receipts();

    move |room_list_entry| -> bool { matches(read_receipts, room_list_entry) }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use matrix_sdk::test_utils::mocks::MatrixMockServer;
    use matrix_sdk_base::read_receipts::RoomReadReceipts;
    use matrix_sdk_test::async_test;
    use ruma::room_id;

    use super::{super::new_rooms, *};

    #[async_test]
    async fn test_has_unread_mentions() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        let read_receipts = |_: &RoomListItem| RoomReadReceipts {
            num_unread: 42,
            num_notifications: 42,
            num_mentions: 42,
            ..Default::default()
        };

        assert!(matches(read_receipts, &room));
    }

    #[async_test]
    async fn test_has_unread_notifications_but_no_unread_mentions() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        let read_receipts = |_: &RoomListItem| RoomReadReceipts {
            num_unread: 42,
            num_notifications: 42,
            num_mentions: 0,
            ..Default::default()
        };

        assert!(matches(read_receipts, &room).not());
    }

    #[async_test]
    async fn test_has_no_unread_mentions() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        let read_receipts = |_: &RoomListItem| RoomReadReceipts::default();

        assert!(matches(read_receipts, &room).not());
    }
}
