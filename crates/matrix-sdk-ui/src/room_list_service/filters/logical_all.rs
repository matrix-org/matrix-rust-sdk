use super::Filter;

/// Create a new filter that will run multiple filters. It returns `false` if at
/// least one of the filter returns `false`.
pub fn new_filter(filters: Vec<Box<dyn Filter>>) -> impl Filter {
    move |room_list_entry| -> bool { filters.iter().all(|filter| filter(room_list_entry)) }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use matrix_sdk::RoomListEntry;
    use ruma::room_id;

    use super::new_filter;

    #[test]
    fn test_one_filter() {
        let room_list_entry = RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned());

        {
            let filter = |_: &_| true;
            let all = new_filter(vec![Box::new(filter)]);

            assert!(all(&room_list_entry));
        }

        {
            let filter = |_: &_| false;
            let all = new_filter(vec![Box::new(filter)]);

            assert!(all(&room_list_entry).not());
        }
    }

    #[test]
    fn test_two_filters() {
        let room_list_entry = RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned());

        {
            let filter1 = |_: &_| true;
            let filter2 = |_: &_| true;
            let all = new_filter(vec![Box::new(filter1), Box::new(filter2)]);

            assert!(all(&room_list_entry));
        }

        {
            let filter1 = |_: &_| true;
            let filter2 = |_: &_| false;
            let all = new_filter(vec![Box::new(filter1), Box::new(filter2)]);

            assert!(all(&room_list_entry).not());
        }

        {
            let filter1 = |_: &_| false;
            let filter2 = |_: &_| true;
            let all = new_filter(vec![Box::new(filter1), Box::new(filter2)]);

            assert!(all(&room_list_entry).not());
        }

        {
            let filter1 = |_: &_| false;
            let filter2 = |_: &_| false;
            let all = new_filter(vec![Box::new(filter1), Box::new(filter2)]);

            assert!(all(&room_list_entry).not());
        }
    }
}
