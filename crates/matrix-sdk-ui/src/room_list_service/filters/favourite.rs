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

struct FavouriteRoomMatcher<F>
where
    F: Fn(&RoomListEntry) -> Option<(bool, RoomState)>,
{
    matches: F,
}

impl<F> FavouriteRoomMatcher<F>
where
    F: Fn(&RoomListEntry) -> Option<(bool, RoomState)>,
{
    fn matches(&self, room_list_entry: &RoomListEntry) -> bool {
        if !matches!(room_list_entry, RoomListEntry::Filled(_) | RoomListEntry::Invalidated(_)) {
            return false;
        }

        match (self.matches)(room_list_entry) {
            Some((true, room_state)) => room_state == RoomState::Joined,
            _ => false,
        }
    }
}

/// Create a new filter that will accept filled or invalidated entries for
/// joined rooms, but filters out rooms that are not marked as favourite (see
/// [`matrix_sdk_base::Room::is_favourite`]).
pub fn new_filter(client: &Client) -> impl Filter {
    let client = client.clone();

    let matcher = FavouriteRoomMatcher {
        matches: move |room| {
            let room_id = room.as_room_id()?;
            let room = client.get_room(room_id)?;

            Some((room.is_favourite(), room.state()))
        },
    };

    move |room_list_entry| -> bool { matcher.matches(room_list_entry) }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use matrix_sdk::RoomListEntry;
    use matrix_sdk_base::RoomState;
    use ruma::room_id;

    use super::FavouriteRoomMatcher;

    #[test]
    fn test_is_favourite() {
        let matcher = FavouriteRoomMatcher { matches: |_| Some((true, RoomState::Joined)) };

        assert!(matcher.matches(&RoomListEntry::Empty).not());
        assert!(matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())));
        assert!(matcher.matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned())));
    }

    #[test]
    fn test_is_not_favourite() {
        let matcher = FavouriteRoomMatcher { matches: |_| Some((false, RoomState::Joined)) };

        assert!(matcher.matches(&RoomListEntry::Empty).not());
        assert!(matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())).not());
        assert!(matcher
            .matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned()))
            .not());
    }

    #[test]
    fn test_favourite_state_cannot_be_found() {
        let matcher = FavouriteRoomMatcher { matches: |_| None };

        assert!(matcher.matches(&RoomListEntry::Empty).not());
        assert!(matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())).not());
        assert!(matcher
            .matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned()))
            .not());
    }

    #[test]
    fn test_does_not_match_invited() {
        let matcher = FavouriteRoomMatcher { matches: |_| Some((true, RoomState::Invited)) };

        assert!(matcher.matches(&RoomListEntry::Empty).not());
        assert!(matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())).not());
        assert!(matcher
            .matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned()))
            .not());
    }

    #[test]
    fn test_does_not_match_left() {
        let matcher = FavouriteRoomMatcher { matches: |_| Some((true, RoomState::Left)) };

        assert!(matcher.matches(&RoomListEntry::Empty).not());
        assert!(matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())).not());
        assert!(matcher
            .matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned()))
            .not());
    }
}
