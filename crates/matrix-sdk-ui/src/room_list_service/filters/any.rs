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

use super::{super::room_list::BoxedFilterFn, Filter};

/// Create a new filter that will run multiple filters. It returns `true` if at
/// least one of the filter returns `true`.
pub fn new_filter(filters: Vec<BoxedFilterFn>) -> impl Filter {
    move |room_list_entry| -> bool { filters.iter().any(|filter| filter(room_list_entry)) }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use matrix_sdk::RoomListEntry;
    use ruma::room_id;

    use super::new_filter;

    #[test]
    fn test_one_filter_is_true() {
        let room_list_entry = RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned());

        let filter = |_: &_| true;
        let any = new_filter(vec![Box::new(filter)]);

        assert!(any(&room_list_entry));
    }

    #[test]
    fn test_one_filter_is_false() {
        let room_list_entry = RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned());

        let filter = |_: &_| false;
        let any = new_filter(vec![Box::new(filter)]);

        assert!(any(&room_list_entry).not());
    }

    #[test]
    fn test_two_filters_with_true_true() {
        let room_list_entry = RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned());

        let filter1 = |_: &_| true;
        let filter2 = |_: &_| true;
        let any = new_filter(vec![Box::new(filter1), Box::new(filter2)]);

        assert!(any(&room_list_entry));
    }

    #[test]
    fn test_two_filters_with_true_false() {
        let room_list_entry = RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned());

        let filter1 = |_: &_| true;
        let filter2 = |_: &_| false;
        let any = new_filter(vec![Box::new(filter1), Box::new(filter2)]);

        assert!(any(&room_list_entry));
    }

    #[test]
    fn test_two_filters_with_false_true() {
        let room_list_entry = RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned());

        let filter1 = |_: &_| false;
        let filter2 = |_: &_| true;
        let any = new_filter(vec![Box::new(filter1), Box::new(filter2)]);

        assert!(any(&room_list_entry));
    }

    #[test]
    fn test_two_filters_with_false_false() {
        let room_list_entry = RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned());

        let filter1 = |_: &_| false;
        let filter2 = |_: &_| false;
        let any = new_filter(vec![Box::new(filter1), Box::new(filter2)]);

        assert!(any(&room_list_entry).not());
    }
}
