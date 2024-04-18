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

use matrix_sdk::{Client, RoomListEntry};
use matrix_sdk_base::{read_receipts::RoomReadReceipts, RoomState};

use super::Filter;

type IsMarkedUnread = bool;

struct UnreadRoomMatcher<F>
where
    F: Fn(&RoomListEntry) -> Option<(RoomReadReceipts, IsMarkedUnread, RoomState)>,
{
    matches: F,
}

impl<F> UnreadRoomMatcher<F>
where
    F: Fn(&RoomListEntry) -> Option<(RoomReadReceipts, IsMarkedUnread, RoomState)>,
{
    fn matches(&self, room_list_entry: &RoomListEntry) -> bool {
        if !matches!(room_list_entry, RoomListEntry::Filled(_) | RoomListEntry::Invalidated(_)) {
            return false;
        }

        let Some((read_receipts, is_marked_unread, room_state)) = (self.matches)(room_list_entry)
        else {
            return false;
        };

        if !matches!(room_state, RoomState::Joined) {
            return false;
        }

        read_receipts.num_notifications > 0 || is_marked_unread
    }
}

/// Create a new filter that will accept filled or invalidated entries for
/// joined rooms, but filters out rooms that have no unread notifications
/// (different from unread messages), or is not marked as unread.
pub fn new_filter(client: &Client) -> impl Filter {
    let client = client.clone();

    let matcher = UnreadRoomMatcher {
        matches: move |room| {
            let room_id = room.as_room_id()?;
            let room = client.get_room(room_id)?;

            Some((room.read_receipts(), room.is_marked_unread(), room.state()))
        },
    };

    move |room_list_entry| -> bool { matcher.matches(room_list_entry) }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use matrix_sdk::RoomListEntry;
    use matrix_sdk_base::{read_receipts::RoomReadReceipts, RoomState};
    use ruma::room_id;

    use super::UnreadRoomMatcher;

    #[test]
    fn test_has_unread_notifications() {
        for is_marked_as_unread in [true, false] {
            let matcher = UnreadRoomMatcher {
                matches: |_| {
                    let mut read_receipts = RoomReadReceipts::default();
                    read_receipts.num_unread = 42;
                    read_receipts.num_notifications = 42;

                    Some((read_receipts, is_marked_as_unread, RoomState::Joined))
                },
            };

            assert!(matcher.matches(&RoomListEntry::Empty).not());
            assert!(matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())));
            assert!(
                matcher.matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned()))
            );
        }
    }

    #[test]
    fn test_has_unread_messages_but_no_unread_notifications_and_is_not_marked_as_unread() {
        let matcher = UnreadRoomMatcher {
            matches: |_| {
                let mut read_receipts = RoomReadReceipts::default();
                read_receipts.num_unread = 42;
                read_receipts.num_notifications = 0;

                Some((read_receipts, false, RoomState::Joined))
            },
        };

        assert!(matcher.matches(&RoomListEntry::Empty).not());
        assert!(matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())).not());
        assert!(matcher
            .matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned()))
            .not());
    }

    #[test]
    fn test_has_unread_messages_but_no_unread_notifications_and_is_marked_as_unread() {
        let matcher = UnreadRoomMatcher {
            matches: |_| {
                let mut read_receipts = RoomReadReceipts::default();
                read_receipts.num_unread = 42;
                read_receipts.num_notifications = 0;

                Some((read_receipts, true, RoomState::Joined))
            },
        };

        assert!(matcher.matches(&RoomListEntry::Empty).not());
        assert!(matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())));
        assert!(matcher.matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned())));
    }

    #[test]
    fn test_has_no_unread_notifications_and_is_not_marked_as_unread() {
        let matcher = UnreadRoomMatcher {
            matches: |_| Some((RoomReadReceipts::default(), false, RoomState::Joined)),
        };

        assert!(matcher.matches(&RoomListEntry::Empty).not());
        assert!(matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())).not());
        assert!(matcher
            .matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned()))
            .not());
    }

    #[test]
    fn test_has_no_unread_notifications_and_is_marked_as_unread() {
        let matcher = UnreadRoomMatcher {
            matches: |_| Some((RoomReadReceipts::default(), true, RoomState::Joined)),
        };

        assert!(matcher.matches(&RoomListEntry::Empty).not());
        assert!(matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())));
        assert!(matcher.matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned())));
    }

    #[test]
    fn test_read_receipts_cannot_be_found() {
        let matcher = UnreadRoomMatcher { matches: |_| None };

        assert!(matcher.matches(&RoomListEntry::Empty).not());
        assert!(matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())).not());
        assert!(matcher
            .matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned()))
            .not());
    }

    #[test]
    fn test_does_not_match_invited() {
        let matcher = UnreadRoomMatcher {
            matches: |_| Some((RoomReadReceipts::default(), false, RoomState::Invited)),
        };

        assert!(matcher.matches(&RoomListEntry::Empty).not());
        assert!(matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())).not());
        assert!(matcher
            .matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned()))
            .not());
    }

    #[test]
    fn test_does_not_match_left() {
        let matcher = UnreadRoomMatcher {
            matches: |_| Some((RoomReadReceipts::default(), false, RoomState::Left)),
        };

        assert!(matcher.matches(&RoomListEntry::Empty).not());
        assert!(matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())).not());
        assert!(matcher
            .matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned()))
            .not());
    }
}
