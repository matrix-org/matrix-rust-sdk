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

use std::ops::Not;

use super::{super::room_list::BoxedFilterFn, Filter};

/// Create a new filter that will negate the inner filter. It returns `false` if
/// the inner filter returns `true`, otherwise it returns `true`.
pub fn new_filter(filter: BoxedFilterFn) -> impl Filter {
    move |room_list_entry| -> bool { filter(room_list_entry).not() }
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use matrix_sdk::RoomListEntry;
    use ruma::room_id;

    use super::new_filter;

    #[test]
    fn test_true() {
        let room_list_entry = RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned());

        let filter = Box::new(|_: &_| true);
        let not = new_filter(filter);

        assert!(not(&room_list_entry).not());
    }

    #[test]
    fn test_false() {
        let room_list_entry = RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned());

        let filter = Box::new(|_: &_| false);
        let not = new_filter(filter);

        assert!(not(&room_list_entry));
    }
}
