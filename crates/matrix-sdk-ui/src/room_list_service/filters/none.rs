use matrix_sdk::RoomListEntry;

/// Create a new filter that will reject all entries.
pub fn new_filter() -> impl Fn(&RoomListEntry) -> bool {
    |_room_list_entry| -> bool { false }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use matrix_sdk::RoomListEntry;
    use ruma::room_id;

    use super::new_filter;

    #[test]
    fn test_all_kind_of_room_list_entry() {
        let none = new_filter();

        assert!(none(&RoomListEntry::Empty).not());
        assert!(none(&RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned())).not());
        assert!(none(&RoomListEntry::Invalidated(room_id!("!r0:bar.org").to_owned())).not());
    }
}
