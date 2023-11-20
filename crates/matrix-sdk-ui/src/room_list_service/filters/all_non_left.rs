use matrix_sdk::{Client, RoomListEntry};
use matrix_sdk_base::RoomState;

struct NonLeftRoomMatcher<F: Fn(&RoomListEntry) -> Option<RoomState>> {
    get_state: F,
}

impl<F: Fn(&RoomListEntry) -> Option<RoomState>> NonLeftRoomMatcher<F> {
    fn matches(&self, room: &RoomListEntry) -> bool {
        if !matches!(room, RoomListEntry::Filled(_) | RoomListEntry::Invalidated(_)) {
            return false;
        }

        if let Some(state) = (self.get_state)(room) {
            state != RoomState::Left
        } else {
            false
        }
    }
}

/// Create a new filter that will accept all filled or invalidated entries, but
/// filters out left rooms.
pub fn new_filter(client: &Client) -> impl Fn(&RoomListEntry) -> bool {
    let client = client.clone();

    let matcher = NonLeftRoomMatcher {
        get_state: move |room| {
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

    use crate::room_list_service::filters::all_non_left::NonLeftRoomMatcher;

    #[test]
    fn test_all_non_left_kind_of_room_list_entry() {
        // When we can't figure out the room state, nothing matches.
        let matcher = NonLeftRoomMatcher { get_state: |_| None };
        assert!(!matcher.matches(&RoomListEntry::Empty));
        assert!(!matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())));
        assert!(!matcher.matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned())));

        // When a room has been left, it doesn't match.
        let matcher = NonLeftRoomMatcher { get_state: |_| Some(RoomState::Left) };
        assert!(!matcher.matches(&RoomListEntry::Empty));
        assert!(!matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())));
        assert!(!matcher.matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned())));

        // When a room has been joined, it does match (unless it's empty).
        let matcher = NonLeftRoomMatcher { get_state: |_| Some(RoomState::Joined) };
        assert!(!matcher.matches(&RoomListEntry::Empty));
        assert!(matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())));
        assert!(matcher.matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned())));
    }
}
