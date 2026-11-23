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

use super::Sorter;

/// Create a new sorter that will sort two [`RoomListItem`] by recency, i.e. by
/// comparing their [`RoomInfo::latest_event_value`]'s timestamp, then their
/// [`RoomInfo::recency_stamp`]. The most recent `Room` comes first, i.e.
/// newest < oldest. A room without a latest event comes after every room
/// having one.
///
/// [`RoomListItem`]: super::RoomListItem
/// [`RoomInfo::recency_stamp`]: matrix_sdk_base::RoomInfo::recency_stamp
/// [`RoomInfo::latest_event_value`]: matrix_sdk_base::RoomInfo::latest_event_value
pub fn new_sorter() -> impl Sorter {
    |left, right| {
        (left.cached_latest_event_timestamp, left.cached_recency_stamp)
            .cmp(&(right.cached_latest_event_timestamp, right.cached_recency_stamp))
            .reverse()
    }
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use matrix_sdk::test_utils::mocks::MatrixMockServer;
    use matrix_sdk_test::async_test;
    use ruma::{MilliSecondsSinceUnixEpoch, room_id};

    use super::{super::super::filters::new_rooms, *};

    #[async_test]
    async fn test_recency() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let [mut room_a, mut room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        room_a.cached_recency_stamp = 1.into();
        room_b.cached_recency_stamp = 2.into();

        // No timestamps: the most recent recency stamp comes first.
        assert_eq!(new_sorter()(&room_a, &room_b), Ordering::Greater);
        assert_eq!(new_sorter()(&room_b, &room_a), Ordering::Less);

        // Only `room_a` has a timestamp: it comes first.
        room_a.cached_latest_event_timestamp = Some(MilliSecondsSinceUnixEpoch(3u32.into()));

        assert_eq!(new_sorter()(&room_a, &room_b), Ordering::Less);
        assert_eq!(new_sorter()(&room_b, &room_a), Ordering::Greater);

        // The most recent timestamp comes first.
        room_a.cached_latest_event_timestamp = Some(MilliSecondsSinceUnixEpoch(4u32.into()));
        room_b.cached_latest_event_timestamp = Some(MilliSecondsSinceUnixEpoch(3u32.into()));

        assert_eq!(new_sorter()(&room_a, &room_b), Ordering::Less);
        assert_eq!(new_sorter()(&room_b, &room_a), Ordering::Greater);

        // Same timestamp: the most recent recency stamp comes first.
        room_a.cached_latest_event_timestamp = room_b.cached_latest_event_timestamp;

        assert_eq!(new_sorter()(&room_a, &room_b), Ordering::Greater);
        assert_eq!(new_sorter()(&room_b, &room_a), Ordering::Less);

        // Same timestamp and recency stamp: equal.
        room_a.cached_recency_stamp = 2.into();

        assert_eq!(new_sorter()(&room_a, &room_b), Ordering::Equal);
    }
}
