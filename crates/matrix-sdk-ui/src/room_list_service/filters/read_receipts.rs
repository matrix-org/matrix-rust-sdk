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

/// Filter read receipts by…
///
/// This type decides which fields to reach in [`ReadReceipts`].
///
/// [`ReadReceipts`]: matrix_sdk_base::read_receipts::ReadReceipts
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[cfg_attr(feature = "uniffi", uniffi(name = "RoomListFilterReadReceipts"))]
pub enum Category {
    /// Filter by mentions, i.e. [`ReadReceipts::num_mentions`].
    ///
    /// [`ReadReceipts::num_mentions`]: matrix_sdk_base::read_receipts::ReadReceipts::num_mentions
    Mentions,

    /// Filter by notifications, i.e. [`ReadReceipts::num_notifications`].
    ///
    /// [`ReadReceipts::num_notifications`]: matrix_sdk_base::read_receipts::ReadReceipts::num_notifications
    Notifications,

    /// Filter by messages, i.e. [`ReadReceipts::num_unread`].
    ///
    /// [`ReadReceipts::num_unread`]: matrix_sdk_base::read_receipts::ReadReceipts::num_unread
    Messages,
}

type NumUnread = u64;
type IsMarkedUnread = bool;
type NumUnreadAndIsMarkedUnread = fn(&RoomListItem) -> (NumUnread, IsMarkedUnread);

fn matches(
    num_unread_and_is_marked_unread: NumUnreadAndIsMarkedUnread,
    room: &RoomListItem,
) -> bool {
    let (num_unread, is_marked_unread) = num_unread_and_is_marked_unread(room);

    num_unread > 0 || is_marked_unread
}

fn build_matches(read_receipts_category: Category) -> NumUnreadAndIsMarkedUnread {
    match read_receipts_category {
        Category::Mentions => {
            |room: &RoomListItem| (room.num_unread_mentions(), room.is_marked_unread())
        }
        Category::Notifications => {
            |room: &RoomListItem| (room.num_unread_notifications(), room.is_marked_unread())
        }
        Category::Messages => {
            |room: &RoomListItem| (room.num_unread_messages(), room.is_marked_unread())
        }
    }
}

/// Create a new filter that will filter out rooms that have —based on the
/// `read_receipts_category`— either no unread mentions, or no unread
/// notifications or no unread messages, in addition to not being marked as
/// unread.
///
/// The formula is: `num_unread > 0 || is_marked_unread`.
pub fn new_filter(read_receipts_category: Category) -> impl Filter {
    let num_unread_and_is_marked_unread = build_matches(read_receipts_category);

    move |room_list_entry| -> bool { matches(num_unread_and_is_marked_unread, room_list_entry) }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use matrix_sdk::test_utils::mocks::MatrixMockServer;
    use matrix_sdk_test::async_test;
    use ruma::room_id;

    use super::{super::new_rooms, *};

    #[async_test]
    async fn test_build_matches_select_the_correct_category() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        room.inner()
            .update_room_info(|mut room_info| {
                room_info.set_read_receipts(matrix_sdk_base::read_receipts::ReadReceipts {
                    num_unread: 1,
                    num_notifications: 2,
                    num_mentions: 3,
                    ..Default::default()
                });

                (room_info, Default::default())
            })
            .await;

        assert_eq!((build_matches(Category::Messages))(&room).0, 1);
        assert_eq!((build_matches(Category::Notifications))(&room).0, 2);
        assert_eq!((build_matches(Category::Mentions))(&room).0, 3);
    }

    #[async_test]
    async fn test_has_unread() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        assert!(matches(|_| (42, true), &room));
        assert!(matches(|_| (42, false), &room));
    }

    #[async_test]
    async fn test_has_no_unread_and_is_not_marked_as_unread() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        assert!(matches(|_| (0, false), &room).not());
    }

    #[async_test]
    async fn test_has_no_unread_and_is_marked_as_unread() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let [room] = new_rooms([room_id!("!a:b.c")], &client, &server).await;

        assert!(matches(|_| (0, true), &room));
    }
}
