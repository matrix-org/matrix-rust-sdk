use matrix_sdk::{Client, RoomListEntry};

use super::Filter;

/// An enum to represent whether a room is about “people” (strictly 2 users) or
/// “group” (1 or more than 2 users).
///
/// Ideally, we would only want to rely on the
/// [`matrix_sdk::BaseRoom::is_direct`] method, but the rules are a little bit
/// different for this high-level UI API.
///
/// This is implemented this way so that it's impossible to filter by “group”
/// and by “people” at the same time: these criteria are mutually
/// exclusive by design per filter.
#[derive(Copy, Clone, PartialEq)]
pub enum RoomCategory {
    Group,
    People,
}

type DirectTargetsLength = usize;

struct CategoryRoomMatcher<F>
where
    F: Fn(&RoomListEntry) -> Option<DirectTargetsLength>,
{
    /// _Direct targets_ mean the number of users in a direct room, except us.
    /// So if it returns 1, it means there are 2 users in the direct room.
    number_of_direct_targets: F,
}

impl<F> CategoryRoomMatcher<F>
where
    F: Fn(&RoomListEntry) -> Option<DirectTargetsLength>,
{
    fn matches(&self, room_list_entry: &RoomListEntry, expected_kind: RoomCategory) -> bool {
        if !matches!(room_list_entry, RoomListEntry::Filled(_) | RoomListEntry::Invalidated(_)) {
            return false;
        }

        let kind = match (self.number_of_direct_targets)(room_list_entry) {
            // If 1, we are sure it's a direct room between two users. It's the strict
            // definition of the `People` category, all good.
            Some(1) => RoomCategory::People,

            // If smaller than 1, we are not sure it's a direct room, it's then a `Group`.
            // If greater than 1, we are sure it's a direct room but not between
            // two users, so it's a `Group` based on our expectation.
            Some(_) => RoomCategory::Group,

            // Don't know.
            None => return false,
        };

        kind == expected_kind
    }
}

/// Create a new filter that will accept all filled or invalidated entries, and
/// if the associated rooms fit in the `expected_category`. The category is
/// defined by [`RoomCategory`], see this type to learn more.
pub fn new_filter(client: &Client, expected_category: RoomCategory) -> impl Filter {
    let client = client.clone();

    let matcher = CategoryRoomMatcher {
        number_of_direct_targets: move |room| {
            let room_id = room.as_room_id()?;
            let room = client.get_room(room_id)?;

            Some(room.direct_targets_length())
        },
    };

    move |room_list_entry| -> bool { matcher.matches(room_list_entry, expected_category) }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use matrix_sdk::RoomListEntry;
    use ruma::room_id;

    use super::{CategoryRoomMatcher, RoomCategory};

    #[test]
    fn test_kind_is_group() {
        let matcher = CategoryRoomMatcher { number_of_direct_targets: |_| Some(42) };

        // Expect `People`.
        {
            let expected_kind = RoomCategory::People;

            assert!(matcher.matches(&RoomListEntry::Empty, expected_kind).not());
            assert!(
                matcher
                    .matches(
                        &RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned(),),
                        expected_kind,
                    )
                    .not()
            );
            assert!(matcher
                .matches(
                    &RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned()),
                    expected_kind
                )
                .not());
        }

        // Expect `Group`.
        {
            let expected_kind = RoomCategory::Group;

            assert!(matcher.matches(&RoomListEntry::Empty, expected_kind).not());
            assert!(matcher.matches(
                &RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned(),),
                expected_kind,
            ));
            assert!(matcher.matches(
                &RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned()),
                expected_kind,
            ));
        }
    }

    #[test]
    fn test_kind_is_people() {
        let matcher = CategoryRoomMatcher { number_of_direct_targets: |_| Some(1) };

        // Expect `People`.
        {
            let expected_kind = RoomCategory::People;

            assert!(matcher.matches(&RoomListEntry::Empty, expected_kind).not());
            assert!(matcher.matches(
                &RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned()),
                expected_kind,
            ));
            assert!(matcher.matches(
                &RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned()),
                expected_kind
            ));
        }

        // Expect `Group`.
        {
            let expected_kind = RoomCategory::Group;

            assert!(matcher.matches(&RoomListEntry::Empty, expected_kind).not());
            assert!(
                matcher
                    .matches(
                        &RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned(),),
                        expected_kind,
                    )
                    .not()
            );
            assert!(matcher
                .matches(
                    &RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned()),
                    expected_kind,
                )
                .not());
        }
    }

    #[test]
    fn test_room_kind_cannot_be_found() {
        let matcher = CategoryRoomMatcher { number_of_direct_targets: |_| None };

        assert!(matcher.matches(&RoomListEntry::Empty, RoomCategory::Group).not());
        assert!(matcher
            .matches(
                &RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned()),
                RoomCategory::Group
            )
            .not());
        assert!(matcher
            .matches(
                &RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned()),
                RoomCategory::Group
            )
            .not());
    }
}
