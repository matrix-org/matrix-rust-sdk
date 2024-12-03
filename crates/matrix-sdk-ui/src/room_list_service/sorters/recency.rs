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

use std::cmp::Ordering;

use super::{Room, Sorter};

struct RecencyMatcher<F>
where
    F: Fn(&Room, &Room) -> (Option<u64>, Option<u64>),
{
    recency_stamps: F,
}

impl<F> RecencyMatcher<F>
where
    F: Fn(&Room, &Room) -> (Option<u64>, Option<u64>),
{
    fn matches(&self, left: &Room, right: &Room) -> Ordering {
        if left.id() == right.id() {
            // `left` and `right` are the same room. We are comparing the same
            // `LatestEvent`!
            //
            // The way our `Room` types are implemented makes it so they are sharing the
            // same data, because they are all built from the same store. They can be seen
            // as shallow clones of each others. In practice it's really great: a `Room` can
            // never be outdated. However, for the case of sorting rooms, it breaks the
            // search algorithm. `left` and `right` will have the exact same recency
            // stamp, so `left` and `right` will always be `Ordering::Equal`. This is
            // wrong: if `left` is compared with `right` and if they are both the same room,
            // it means that one of them (either `left`, or `right`, it's not important) has
            // received an update. The room position is very likely to change. But if they
            // compare to `Equal`, the position may not change. It actually depends of the
            // search algorithm used by [`eyeball_im_util::SortBy`].
            //
            // Since this room received an update, it is more recent than the previous one
            // we matched against, so return `Ordering::Greater`.
            return Ordering::Greater;
        }

        match (self.recency_stamps)(left, right) {
            (Some(left_stamp), Some(right_stamp)) => left_stamp.cmp(&right_stamp).reverse(),

            (Some(_), None) => Ordering::Less,

            (None, Some(_)) => Ordering::Greater,

            (None, None) => Ordering::Equal,
        }
    }
}

/// Create a new sorter that will sort two [`Room`] by recency, i.e. by
/// comparing their [`RoomInfo::recency_stamp`] value. The `Room` with the
/// newest recency stamp comes first, i.e. newest < oldest.
///
/// [`RoomInfo::recency_stamp`]: matrix_sdk_base::RoomInfo::recency_stamp
pub fn new_sorter() -> impl Sorter {
    let matcher = RecencyMatcher {
        recency_stamps: move |left, right| (left.recency_stamp(), right.recency_stamp()),
    };

    move |left, right| -> Ordering { matcher.matches(left, right) }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::test_utils::logged_in_client_with_server;
    use matrix_sdk_test::async_test;
    use ruma::room_id;

    use super::{super::super::filters::new_rooms, *};

    #[async_test]
    async fn test_with_two_recency_stamps() {
        let (client, server) = logged_in_client_with_server().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `room_a` has an older recency stamp than `room_b`.
        {
            let matcher = RecencyMatcher { recency_stamps: |_left, _right| (Some(1), Some(2)) };

            // `room_a` is greater than `room_b`, i.e. it must come after `room_b`.
            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Greater);
        }

        // `room_b` has an older recency stamp than `room_a`.
        {
            let matcher = RecencyMatcher { recency_stamps: |_left, _right| (Some(2), Some(1)) };

            // `room_a` is less than `room_b`, i.e. it must come before `room_b`.
            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Less);
        }

        // `room_a` has an equally old recency stamp than `room_b`.
        {
            let matcher = RecencyMatcher { recency_stamps: |_left, _right| (Some(1), Some(1)) };

            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Equal);
        }
    }

    #[async_test]
    async fn test_with_one_recency_stamp() {
        let (client, server) = logged_in_client_with_server().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `room_a` has a recency stamp, `room_b` has no recency stamp.
        {
            let matcher = RecencyMatcher { recency_stamps: |_left, _right| (Some(1), None) };

            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Less);
        }

        // `room_a` has no recency stamp, `room_b` has a recency stamp.
        {
            let matcher = RecencyMatcher { recency_stamps: |_left, _right| (None, Some(1)) };

            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Greater);
        }
    }

    #[async_test]
    async fn test_with_zero_recency_stamp() {
        let (client, server) = logged_in_client_with_server().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `room_a` and `room_b` has no recency stamp.
        {
            let matcher = RecencyMatcher { recency_stamps: |_left, _right| (None, None) };

            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Equal);
        }
    }
}
