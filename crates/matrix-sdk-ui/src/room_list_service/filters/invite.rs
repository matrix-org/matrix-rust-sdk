use matrix_sdk::{Client, RoomListEntry};
use matrix_sdk_base::RoomState;

use super::Filter;

struct InviteRoomMatcher<F>
where
    F: Fn(&RoomListEntry) -> Option<RoomState>,
{
    state: F,
}

impl<F> InviteRoomMatcher<F>
where
    F: Fn(&RoomListEntry) -> Option<RoomState>,
{
    fn matches(&self, room: &RoomListEntry) -> bool {
        if !matches!(room, RoomListEntry::Filled(_) | RoomListEntry::Invalidated(_)) {
            return false;
        }

        if let Some(state) = (self.state)(room) {
            state == RoomState::Invited
        } else {
            false
        }
    }
}

/// Create a new filter that will accept all filled or invalidated entries, but
/// filters out rooms that are not invites (see
/// [`matrix_sdk_base::RoomState::Invited`]).
pub fn new_filter(client: &Client) -> impl Filter {
    let client = client.clone();

    let matcher = InviteRoomMatcher {
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

    use super::InviteRoomMatcher;

    #[test]
    fn test_all_invite_kind_of_room_list_entry() {
        // When we can't figure out the room state, nothing matches.
        let matcher = InviteRoomMatcher { state: |_| None };
        assert!(!matcher.matches(&RoomListEntry::Empty));
        assert!(!matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())));
        assert!(!matcher.matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned())));

        // When a room has been left, it doesn't match.
        let matcher = InviteRoomMatcher { state: |_| Some(RoomState::Left) };
        assert!(!matcher.matches(&RoomListEntry::Empty));
        assert!(!matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())));
        assert!(!matcher.matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned())));

        // When a room has been joined, it doesn't match.
        let matcher = InviteRoomMatcher { state: |_| Some(RoomState::Joined) };
        assert!(!matcher.matches(&RoomListEntry::Empty));
        assert!(!matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())));
        assert!(!matcher.matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned())));

        // When a room has been joined, it does match (unless it's empty).
        let matcher = InviteRoomMatcher { state: |_| Some(RoomState::Invited) };
        assert!(!matcher.matches(&RoomListEntry::Empty));
        assert!(matcher.matches(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())));
        assert!(matcher.matches(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned())));
    }
}
