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

//! A collection of room filters.
//!
//! The room list can provide an access to the rooms per list, like with
//! [`super::RoomList::entries_with_dynamic_adapters`]. The provided collection
//! of rooms can be filtered with these filters. A classical usage would be the
//! following:
//!
//! ```rust
//! use matrix_sdk::Client;
//! use matrix_sdk_ui::room_list_service::{
//!     filters, RoomListDynamicEntriesController,
//! };
//!
//! fn configure_room_list(
//!     client: &Client,
//!     entries_controller: &RoomListDynamicEntriesController,
//! ) {
//!     // _All_ non-left rooms
//!     // _and_ that fall in the “People” category,
//!     // _and_ that are marked as favourite,
//!     // _and_ that are _not_ unread.
//!     entries_controller.set_filter(Box::new(
//!         // All
//!         filters::new_filter_all(vec![
//!             // Non-left
//!             Box::new(filters::new_filter_non_left(&client)),
//!             // People
//!             Box::new(filters::new_filter_category(
//!                 client,
//!                 filters::RoomCategory::People,
//!             )),
//!             // Favourite
//!             Box::new(filters::new_filter_favourite(client)),
//!             // Not Unread
//!             Box::new(filters::new_filter_not(Box::new(
//!                 filters::new_filter_unread(client),
//!             ))),
//!         ]),
//!     ));
//! }
//! ```

mod all;
mod any;
mod category;
mod favourite;
mod fuzzy_match_room_name;
mod invite;
mod joined;
mod non_left;
mod none;
mod normalized_match_room_name;
mod not;
mod unread;

pub use all::new_filter as new_filter_all;
pub use any::new_filter as new_filter_any;
pub use category::{new_filter as new_filter_category, RoomCategory};
pub use favourite::new_filter as new_filter_favourite;
pub use fuzzy_match_room_name::new_filter as new_filter_fuzzy_match_room_name;
pub use invite::new_filter as new_filter_invite;
pub use joined::new_filter as new_filter_joined;
use matrix_sdk::RoomListEntry;
pub use non_left::new_filter as new_filter_non_left;
pub use none::new_filter as new_filter_none;
pub use normalized_match_room_name::new_filter as new_filter_normalized_match_room_name;
pub use not::new_filter as new_filter_not;
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};
pub use unread::new_filter as new_filter_unread;

/// A trait “alias” that represents a _filter_.
///
/// A filter is simply a function that receives a `&RoomListEntry` and returns a
/// `bool`.
pub trait Filter: Fn(&RoomListEntry) -> bool {}

impl<F> Filter for F where F: Fn(&RoomListEntry) -> bool {}

/// Normalize a string, i.e. decompose it into NFD (Normalization Form D, i.e. a
/// canonical decomposition, see http://www.unicode.org/reports/tr15/) and
/// filter out the combining marks.
fn normalize_string(str: &str) -> String {
    str.nfd().filter(|c| !is_combining_mark(*c)).collect::<String>()
}

#[cfg(test)]
mod tests {
    use super::normalize_string;

    #[test]
    fn test_normalize_string() {
        assert_eq!(&normalize_string("abc"), "abc");
        assert_eq!(&normalize_string("Ștefan Été"), "Stefan Ete");
        assert_eq!(&normalize_string("Ç ṩ ḋ Å"), "C s d A");
        assert_eq!(&normalize_string("هند"), "هند");
    }
}
