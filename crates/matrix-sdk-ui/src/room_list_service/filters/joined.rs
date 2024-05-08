use matrix_sdk::{Client, RoomListEntry};
use matrix_sdk_base::RoomState;

use super::Filter;

struct JoinedRoomMatcher<F>
where
    F: Fn(&RoomListEntry) -> Option<RoomState>,
{
    state: F,
}

impl<F> JoinedRoomMatcher<F>
where
    F: Fn(&RoomListEntry) -> Option<RoomState>,
{
    fn matches(&self, room: &RoomListEntry) -> bool {
        if !matches!(room, RoomListEntry::Filled(_) | RoomListEntry::Invalidated(_)) {
            return false;
        }

        if let Some(state) = (self.state)(room) {
            state == RoomState::Joined
        } else {
            false
        }
    }
}

/// Create a new filter that will accept all filled or invalidated entries, but
/// filters out rooms that are not joined (see
/// [`matrix_sdk_base::RoomState::Joined`]).
pub fn new_filter(client: &Client) -> impl Filter {
    let client = client.clone();

    let matcher = JoinedRoomMatcher {
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

    use super::JoinedRoomMatcher;

    #[test]
    fn test_all_joined_kind_of_room_list_entry() {
        // When we can't figure out the room state, nothing matches.
        let matcher = JoinedRoomMatcher { state: |_| None };
        assert!(!matcher.matches(&RoomListEntry::Empty));
        assert!(!matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())));
        assert!(!matcher.matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned())));

        // When a room has been left, it doesn't match.
        let matcher = JoinedRoomMatcher { state: |_| Some(RoomState::Left) };
        assert!(!matcher.matches(&RoomListEntry::Empty));
        assert!(!matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())));
        assert!(!matcher.matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned())));

        // When a room is an invite, it doesn't match.
        let matcher = JoinedRoomMatcher { state: |_| Some(RoomState::Invited) };
        assert!(!matcher.matches(&RoomListEntry::Empty));
        assert!(!matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())));
        assert!(!matcher.matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned())));

        // When a room has been joined, it does match (unless it's empty).
        let matcher = JoinedRoomMatcher { state: |_| Some(RoomState::Joined) };
        assert!(!matcher.matches(&RoomListEntry::Empty));
        assert!(matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())));
        assert!(matcher.matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned())));
    }
}
