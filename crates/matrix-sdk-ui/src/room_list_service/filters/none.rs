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

use super::Filter;

/// Create a new filter that will reject all entries.
pub fn new_filter() -> impl Filter {
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
