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
//! use matrix_sdk_ui::room_list_service::{
//!     filters, RoomListDynamicEntriesController,
//! };
//!
//! fn configure_room_list(
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
//!             Box::new(filters::new_filter_non_left()),
//!             // People
//!             Box::new(filters::new_filter_category(
//!                 filters::RoomCategory::People,
//!             )),
//!             // Favourite
//!             Box::new(filters::new_filter_favourite()),
//!             // Not Unread
//!             Box::new(filters::new_filter_not(Box::new(
//!                 filters::new_filter_unread(),
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

#[cfg(test)]
use std::sync::Arc;

pub use all::new_filter as new_filter_all;
pub use any::new_filter as new_filter_any;
pub use category::{new_filter as new_filter_category, RoomCategory};
pub use favourite::new_filter as new_filter_favourite;
pub use fuzzy_match_room_name::new_filter as new_filter_fuzzy_match_room_name;
pub use invite::new_filter as new_filter_invite;
pub use joined::new_filter as new_filter_joined;
#[cfg(test)]
use matrix_sdk::{test_utils::logged_in_client_with_server, Client, SlidingSync};
#[cfg(test)]
use matrix_sdk_test::{JoinedRoomBuilder, SyncResponseBuilder};
pub use non_left::new_filter as new_filter_non_left;
pub use none::new_filter as new_filter_none;
pub use normalized_match_room_name::new_filter as new_filter_normalized_match_room_name;
pub use not::new_filter as new_filter_not;
#[cfg(test)]
use ruma::RoomId;
use unicode_normalization::{char::is_combining_mark, UnicodeNormalization};
pub use unread::new_filter as new_filter_unread;
#[cfg(test)]
use wiremock::{
    matchers::{header, method, path},
    Mock, MockServer, ResponseTemplate,
};

use super::Room;

/// A trait “alias” that represents a _filter_.
///
/// A filter is simply a function that receives a `&Room` and returns a `bool`.
pub trait Filter: Fn(&Room) -> bool {}

impl<F> Filter for F where F: Fn(&Room) -> bool {}

/// Type alias for a boxed filter function.
#[cfg(not(target_arch = "wasm32"))]
pub type BoxedFilterFn = Box<dyn Filter + Send + Sync>;
#[cfg(target_arch = "wasm32")]
pub type BoxedFilterFn = Box<dyn Filter>;

/// Normalize a string, i.e. decompose it into NFD (Normalization Form D, i.e. a
/// canonical decomposition, see http://www.unicode.org/reports/tr15/) and
/// filter out the combining marks.
fn normalize_string(str: &str) -> String {
    str.nfd().filter(|c| !is_combining_mark(*c)).collect::<String>()
}

#[cfg(test)]
pub(super) async fn new_rooms<const N: usize>(
    room_ids: [&RoomId; N],
    client: &Client,
    server: &MockServer,
    sliding_sync: &Arc<SlidingSync>,
) -> [Room; N] {
    let mut response_builder = SyncResponseBuilder::default();

    for room_id in room_ids {
        response_builder.add_joined_room(JoinedRoomBuilder::new(room_id));
    }

    let json_response = response_builder.build_json_sync_response();

    let _scope = Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/sync"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json_response))
        .mount_as_scoped(server)
        .await;

    let _response = client.sync_once(Default::default()).await.unwrap();

    room_ids.map(|room_id| Room::new(client.get_room(room_id).unwrap(), sliding_sync))
}

#[cfg(test)]
pub(super) async fn client_and_server_prelude() -> (Client, MockServer, Arc<SlidingSync>) {
    let (client, server) = logged_in_client_with_server().await;
    let sliding_sync = Arc::new(client.sliding_sync("foo").unwrap().build().await.unwrap());

    (client, server, sliding_sync)
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
