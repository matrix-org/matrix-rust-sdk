use matrix_sdk::RoomListEntry;

/// Create a new filter that will accept all filled or invalidated entries.
pub fn new_filter() -> impl Fn(&RoomListEntry) -> bool + Send + Sync + 'static {
    |room_list_entry| -> bool {
        matches!(room_list_entry, RoomListEntry::Filled(_) | RoomListEntry::Invalidated(_))
    }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use ruma::room_id;

    use super::*;

    #[test]
    fn test_all_kind_of_room_list_entry() {
        let all = new_filter();

        assert!(all(&RoomListEntry::Empty).not());
        assert!(all(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())));
        assert!(all(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned())));
    }
}
