use matrix_sdk::{Client, RoomListEntry};
use matrix_sdk_base::read_receipts::RoomReadReceipts;

use super::Filter;

type IsMarkedUnread = bool;

struct UnreadRoomMatcher<F>
where
    F: Fn(&RoomListEntry) -> Option<(RoomReadReceipts, IsMarkedUnread)>,
{
    read_receipts_and_unread: F,
}

impl<F> UnreadRoomMatcher<F>
where
    F: Fn(&RoomListEntry) -> Option<(RoomReadReceipts, IsMarkedUnread)>,
{
    fn matches(&self, room_list_entry: &RoomListEntry) -> bool {
        if !matches!(room_list_entry, RoomListEntry::Filled(_) | RoomListEntry::Invalidated(_)) {
            return false;
        }

        let Some((read_receipts, is_marked_unread)) =
            (self.read_receipts_and_unread)(room_list_entry)
        else {
            return false;
        };

        read_receipts.num_notifications > 0 || is_marked_unread
    }
}

/// Create a new filter that will accept all filled or invalidated entries, but
/// filters out rooms that have no unread notifications (different from unread
/// messages), or is not marked as unread.
pub fn new_filter(client: &Client) -> impl Filter {
    let client = client.clone();

    let matcher = UnreadRoomMatcher {
        read_receipts_and_unread: move |room| {
            let room_id = room.as_room_id()?;
            let room = client.get_room(room_id)?;

            Some((room.read_receipts(), room.is_marked_unread()))
        },
    };

    move |room_list_entry| -> bool { matcher.matches(room_list_entry) }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use matrix_sdk::RoomListEntry;
    use matrix_sdk_base::read_receipts::RoomReadReceipts;
    use ruma::room_id;

    use super::UnreadRoomMatcher;

    #[test]
    fn test_has_unread_notifications() {
        for is_marked_as_unread in [true, false] {
            let matcher = UnreadRoomMatcher {
                read_receipts_and_unread: |_| {
                    let mut read_receipts = RoomReadReceipts::default();
                    read_receipts.num_unread = 42;
                    read_receipts.num_notifications = 42;

                    Some((read_receipts, is_marked_as_unread))
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
            read_receipts_and_unread: |_| {
                let mut read_receipts = RoomReadReceipts::default();
                read_receipts.num_unread = 42;
                read_receipts.num_notifications = 0;

                Some((read_receipts, false))
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
            read_receipts_and_unread: |_| {
                let mut read_receipts = RoomReadReceipts::default();
                read_receipts.num_unread = 42;
                read_receipts.num_notifications = 0;

                Some((read_receipts, true))
            },
        };

        assert!(matcher.matches(&RoomListEntry::Empty).not());
        assert!(matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())));
        assert!(matcher.matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned())));
    }

    #[test]
    fn test_has_no_unread_notifications_and_is_not_marked_as_unread() {
        let matcher = UnreadRoomMatcher {
            read_receipts_and_unread: |_| Some((RoomReadReceipts::default(), false)),
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
            read_receipts_and_unread: |_| Some((RoomReadReceipts::default(), true)),
        };

        assert!(matcher.matches(&RoomListEntry::Empty).not());
        assert!(matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())));
        assert!(matcher.matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned())));
    }

    #[test]
    fn test_read_receipts_cannot_be_found() {
        let matcher = UnreadRoomMatcher { read_receipts_and_unread: |_| None };

        assert!(matcher.matches(&RoomListEntry::Empty).not());
        assert!(matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())).not());
        assert!(matcher
            .matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned()))
            .not());
    }
}
