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

struct NonLeftRoomMatcher<F>
where
    F: Fn(&RoomListEntry) -> Option<RoomState>,
{
    state: F,
}

impl<F> NonLeftRoomMatcher<F>
where
    F: Fn(&RoomListEntry) -> Option<RoomState>,
{
    fn matches(&self, room: &RoomListEntry) -> bool {
        if !matches!(room, RoomListEntry::Filled(_) | RoomListEntry::Invalidated(_)) {
            return false;
        }

        if let Some(state) = (self.state)(room) {
            state != RoomState::Left
        } else {
            false
        }
    }
}

/// Create a new filter that will accept all filled or invalidated entries, but
/// filters out left rooms.
pub fn new_filter(client: &Client) -> impl Filter {
    let client = client.clone();

    let matcher = NonLeftRoomMatcher {
        state: move |room| {
            let room_id = room.as_room_id()?;
            let room = client.get_room(room_id)?;
            Some(room.state())
        },
    };

    move |room_list_entry| -> bool { matcher.matches(room_list_entry) }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::RoomListEntry;
    use matrix_sdk_base::RoomState;
    use ruma::room_id;

    use super::NonLeftRoomMatcher;

    #[test]
    fn test_all_non_left_kind_of_room_list_entry() {
        // When we can't figure out the room state, nothing matches.
        let matcher = NonLeftRoomMatcher { state: |_| None };
        assert!(!matcher.matches(&RoomListEntry::Empty));
        assert!(!matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())));
        assert!(!matcher.matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned())));

        // When a room has been left, it doesn't match.
        let matcher = NonLeftRoomMatcher { state: |_| Some(RoomState::Left) };
        assert!(!matcher.matches(&RoomListEntry::Empty));
        assert!(!matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())));
        assert!(!matcher.matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned())));

        // When a room has been joined, it does match (unless it's empty).
        let matcher = NonLeftRoomMatcher { state: |_| Some(RoomState::Joined) };
        assert!(!matcher.matches(&RoomListEntry::Empty));
        assert!(matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())));
        assert!(matcher.matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned())));
    }
}
