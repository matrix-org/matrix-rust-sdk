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

use matrix_sdk_base::read_receipts::RoomReadReceipts;

use super::{super::Room, Filter};

type IsMarkedUnread = bool;

struct UnreadRoomMatcher<F>
where
    F: Fn(&Room) -> (RoomReadReceipts, IsMarkedUnread),
{
    read_receipts_and_unread: F,
}

impl<F> UnreadRoomMatcher<F>
where
    F: Fn(&Room) -> (RoomReadReceipts, IsMarkedUnread),
{
    fn matches(&self, room: &Room) -> bool {
        let (read_receipts, is_marked_unread) = (self.read_receipts_and_unread)(room);

        read_receipts.num_notifications > 0 || is_marked_unread
    }
}

/// Create a new filter that will filters out rooms that have no unread
/// notifications (different from unread messages), or is not marked as unread.
pub fn new_filter() -> impl Filter {
    let matcher = UnreadRoomMatcher {
        read_receipts_and_unread: move |room| (room.read_receipts(), room.is_marked_unread()),
    };

    move |room_list_entry| -> bool { matcher.matches(room_list_entry) }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use matrix_sdk::test_utils::logged_in_client_with_server;
    use matrix_sdk_base::read_receipts::RoomReadReceipts;
    use matrix_sdk_test::async_test;
    use ruma::room_id;

    use super::{super::new_rooms, *};

    #[async_test]
    async fn test_has_unread_notifications() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        for is_marked_as_unread in [true, false] {
            let matcher = UnreadRoomMatcher {
                read_receipts_and_unread: |_| {
                    let mut read_receipts = RoomReadReceipts::default();
                    read_receipts.num_unread = 42;
                    read_receipts.num_notifications = 42;

                    (read_receipts, is_marked_as_unread)
                },
            };

            assert!(matcher.matches(&room));
        }
    }

    #[async_test]
    async fn test_has_unread_messages_but_no_unread_notifications_and_is_not_marked_as_unread() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        let matcher = UnreadRoomMatcher {
            read_receipts_and_unread: |_| {
                let mut read_receipts = RoomReadReceipts::default();
                read_receipts.num_unread = 42;
                read_receipts.num_notifications = 0;

                (read_receipts, false)
            },
        };

        assert!(matcher.matches(&room).not());
    }

    #[async_test]
    async fn test_has_unread_messages_but_no_unread_notifications_and_is_marked_as_unread() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        let matcher = UnreadRoomMatcher {
            read_receipts_and_unread: |_| {
                let mut read_receipts = RoomReadReceipts::default();
                read_receipts.num_unread = 42;
                read_receipts.num_notifications = 0;

                (read_receipts, true)
            },
        };

        assert!(matcher.matches(&room));
    }

    #[async_test]
    async fn test_has_no_unread_notifications_and_is_not_marked_as_unread() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        let matcher = UnreadRoomMatcher {
            read_receipts_and_unread: |_| (RoomReadReceipts::default(), false),
        };

        assert!(matcher.matches(&room).not());
    }

    #[async_test]
    async fn test_has_no_unread_notifications_and_is_marked_as_unread() {
        let (client, server) = logged_in_client_with_server().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        let matcher =
            UnreadRoomMatcher { read_receipts_and_unread: |_| (RoomReadReceipts::default(), true) };

        assert!(matcher.matches(&room));
    }
}
