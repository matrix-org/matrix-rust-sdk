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
use matrix_sdk_base::RoomState;

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
    F: Fn(&RoomListEntry) -> Option<(DirectTargetsLength, RoomState)>,
{
    /// _Direct targets_ mean the number of users in a direct room, except us.
    /// So if it returns 1, it means there are 2 users in the direct room.
    matches: F,
}

impl<F> CategoryRoomMatcher<F>
where
    F: Fn(&RoomListEntry) -> Option<(DirectTargetsLength, RoomState)>,
{
    fn matches(&self, room_list_entry: &RoomListEntry, expected_kind: RoomCategory) -> bool {
        if !matches!(room_list_entry, RoomListEntry::Filled(_) | RoomListEntry::Invalidated(_)) {
            return false;
        }

        let (kind, room_state) = match (self.matches)(room_list_entry) {
            // If 1, we are sure it's a direct room between two users. It's the strict
            // definition of the `People` category, all good.
            Some((1, room_state)) => (RoomCategory::People, room_state),

            // If smaller than 1, we are not sure it's a direct room, it's then a `Group`.
            // If greater than 1, we are sure it's a direct room but not between
            // two users, so it's a `Group` based on our expectation.
            Some((_, room_state)) => (RoomCategory::Group, room_state),

            // Don't know.
            None => return false,
        };

        kind == expected_kind && room_state == RoomState::Joined
    }
}

/// Create a new filter that will accept filled or invalidated entries for
/// joined rooms, and if the associated rooms fit in the `expected_category`.
/// The category is defined by [`RoomCategory`], see this type to learn more.
pub fn new_filter(client: &Client, expected_category: RoomCategory) -> impl Filter {
    let client = client.clone();

    let matcher = CategoryRoomMatcher {
        matches: move |room| {
            let room_id = room.as_room_id()?;
            let room = client.get_room(room_id)?;

            Some((room.direct_targets_length(), room.state()))
        },
    };

    move |room_list_entry| -> bool { matcher.matches(room_list_entry, expected_category) }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use matrix_sdk::RoomListEntry;
    use matrix_sdk_base::RoomState;
    use ruma::room_id;

    use super::{CategoryRoomMatcher, RoomCategory};

    #[test]
    fn test_kind_is_group() {
        let matcher = CategoryRoomMatcher { matches: |_| Some((42, RoomState::Joined)) };

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
        let matcher = CategoryRoomMatcher { matches: |_| Some((1, RoomState::Joined)) };

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
        let matcher = CategoryRoomMatcher { matches: |_| None };

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

    #[test]
    fn test_does_not_match_invited() {
        let matcher = CategoryRoomMatcher { matches: |_| Some((0, RoomState::Invited)) };

        assert!(matcher.matches(&RoomListEntry::Empty, RoomCategory::Group).not());
        assert!(matcher.matches(&RoomListEntry::Empty, RoomCategory::People).not());

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

        assert!(matcher
            .matches(
                &RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned()),
                RoomCategory::People
            )
            .not());
        assert!(matcher
            .matches(
                &RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned()),
                RoomCategory::People
            )
            .not());
    }

    #[test]
    fn test_does_not_match_left() {
        let matcher = CategoryRoomMatcher { matches: |_| Some((0, RoomState::Left)) };

        assert!(matcher.matches(&RoomListEntry::Empty, RoomCategory::Group).not());
        assert!(matcher.matches(&RoomListEntry::Empty, RoomCategory::People).not());

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

        assert!(matcher
            .matches(
                &RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned()),
                RoomCategory::People
            )
            .not());
        assert!(matcher
            .matches(
                &RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned()),
                RoomCategory::People
            )
            .not());
    }
}
