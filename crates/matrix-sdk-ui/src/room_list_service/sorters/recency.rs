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

use super::{RoomListItem, Sorter};

fn cmp<F>(scores: F, left: &RoomListItem, right: &RoomListItem) -> Ordering
where
    F: Fn(&RoomListItem, &RoomListItem) -> (Option<Score>, Option<Score>),
{
    let (a, b) = scores(left, right);
    cmp_impl(a, b)
}

fn cmp_impl(a: Option<Score>, b: Option<Score>) -> Ordering {
    match (a, b) {
        (Some(left), Some(right)) => left.cmp(&right).reverse(),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Create a new sorter that will sort two [`RoomListItem`] by “recency score”,
/// i.e. by comparing their [`RoomInfo::latest_event_value`]'s timestamp value,
/// or their [`RoomInfo::recency_stamp`] value. The `Room` with the newest
/// “recency score” comes first, i.e. newest < oldest.
///
/// [`RoomInfo::recency_stamp`]: matrix_sdk_base::RoomInfo::recency_stamp
/// [`RoomInfo::latest_event_value`]: matrix_sdk_base::RoomInfo::latest_event_value
pub fn new_sorter() -> impl Sorter {
    |left, right| -> Ordering { cmp(extract_scores, left, right) }
}

/// The term _score_ is used here to avoid any confusion with a _timestamp_ (a
/// `u64` from the latest event), or a _recency stamp_ (a `u64` from the recency
/// stamp of the room). This type hides `u64` for the sake of semantics.
type Score = u64;

/// Extract the recency _scores_ from either the
/// [`RoomInfo::latest_event_value`] or from [`RoomInfo::recency_stamp`].
///
/// We must be very careful to return data of the same nature: either a
/// _score_ from the [`LatestEventValue`]'s timestamp, or from the
/// [`RoomInfo::recency_stamp`], but we **must never** mix both. The
/// `RoomInfo::recency_stamp` is not a timestamp, while `LatestEventValue` uses
/// a timestamp.
///
/// [`RoomInfo::recency_stamp`]: matrix_sdk_base::RoomInfo::recency_stamp
/// [`RoomInfo::latest_event_value`]: matrix_sdk_base::RoomInfo::latest_event_value
fn extract_scores(left: &RoomListItem, right: &RoomListItem) -> (Option<Score>, Option<Score>) {
    // Warning 1.
    //
    // Be careful. This method is called **a lot** in the context of a sorter. Using
    // `Room::latest_event` would be dramatic as it returns a clone of the
    // `LatestEventValue`. It's better to use the more specific method
    // `Room::latest_event_timestamp`, where the value is cached in
    // `RoomListItem::cached_latest_event_timestamp`.

    // Warning 2.
    //
    // A `RoomListItem` must have a unique score when sorting. Its `Score`
    // must always be the same while sorting the rooms. Thus, the following
    // rules must apply:
    //
    // - Nominal case: Two rooms with a latest event can be compared together based
    //   on their latest event's timestamp,
    // - Case #1: If a room has a latest event, but the other doesn't have one, the
    //   first room has a score but the other doesn't have one,
    // - Case #2: If none of the room has a latest event, we fallback to the recency
    //   stamp for both rooms.
    //
    // The most important aspect is: if room returns its latest event's
    // timestamp or its recency stamp, _once_, it must return it every time
    // it's compared to another room, if possible, whatever the room is,
    // otherwise it must return `None`.

    // Nominal case and case #1.
    if left.cached_latest_event_timestamp.is_some() || right.cached_latest_event_timestamp.is_some()
    {
        (
            left.cached_latest_event_timestamp.map(|ts| ts.get().into()),
            right.cached_latest_event_timestamp.map(|ts| ts.get().into()),
        )
    }
    // Case #2.
    else {
        (left.cached_recency_stamp.map(Into::into), right.cached_recency_stamp.map(Into::into))
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::{
        RoomRecencyStamp,
        latest_events::{LatestEventValue, LocalLatestEventValue, RemoteLatestEventValue},
        store::SerializableEventContent,
        test_utils::logged_in_client_with_server,
    };
    use matrix_sdk_base::RoomInfoNotableUpdateReasons;
    use matrix_sdk_test::async_test;
    use proptest::prelude::*;
    use ruma::{
        MilliSecondsSinceUnixEpoch,
        events::{AnyMessageLikeEventContent, room::message::RoomMessageEventContent},
        room_id,
        serde::Raw,
    };
    use serde_json::json;

    use super::{super::super::filters::new_rooms, *};

    fn none() -> LatestEventValue {
        LatestEventValue::None
    }

    fn remote(origin_server_ts: u64) -> LatestEventValue {
        LatestEventValue::Remote(RemoteLatestEventValue::from_plaintext(
            Raw::from_json_string(
                json!({
                    "content": RoomMessageEventContent::text_plain("raclette"),
                    "type": "m.room.message",
                    "event_id": "$ev0",
                    "room_id": "!r0",
                    "origin_server_ts": origin_server_ts,
                    "sender": "@mnt_io:matrix.org",
                })
                .to_string(),
            )
            .unwrap(),
        ))
    }

    fn local_is_sending(origin_server_ts: u32) -> LatestEventValue {
        LatestEventValue::LocalIsSending(LocalLatestEventValue {
            timestamp: MilliSecondsSinceUnixEpoch(origin_server_ts.into()),
            content: SerializableEventContent::new(&AnyMessageLikeEventContent::RoomMessage(
                RoomMessageEventContent::text_plain("raclette"),
            ))
            .unwrap(),
        })
    }

    fn local_cannot_be_sent(origin_server_ts: u32) -> LatestEventValue {
        LatestEventValue::LocalCannotBeSent(LocalLatestEventValue {
            timestamp: MilliSecondsSinceUnixEpoch(origin_server_ts.into()),
            content: SerializableEventContent::new(&AnyMessageLikeEventContent::RoomMessage(
                RoomMessageEventContent::text_plain("raclette"),
            ))
            .unwrap(),
        })
    }

    fn set_latest_event_value(room: &mut RoomListItem, latest_event_value: LatestEventValue) {
        let mut room_info = room.clone_info();
        room_info.set_latest_event(latest_event_value);
        room.set_room_info(room_info, RoomInfoNotableUpdateReasons::LATEST_EVENT);
        room.refresh_cached_data();
    }

    fn set_recency_stamp(room: &mut RoomListItem, recency_stamp: RoomRecencyStamp) {
        let mut room_info = room.clone_info();
        room_info.update_recency_stamp(recency_stamp);
        room.set_room_info(room_info, RoomInfoNotableUpdateReasons::RECENCY_STAMP);
        room.refresh_cached_data();
    }

    #[async_test]
    async fn test_extract_scores_with_none() {
        let (client, server) = logged_in_client_with_server().await;
        let [mut room_a, mut room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        set_recency_stamp(&mut room_a, 1.into());
        set_recency_stamp(&mut room_b, 2.into());

        // Both rooms have a `LatestEventValue::None`.
        //
        // Because there is no latest event, the recency stamp MUST BE USED.
        {
            set_latest_event_value(&mut room_a, none());
            set_latest_event_value(&mut room_b, none());

            assert_eq!(extract_scores(&room_a, &room_b), (Some(1), Some(2)));
        }

        // `room_a` has `None`, `room_b` has something else.
        //
        // One of the room has a latest event, so the recency stamp MUST BE IGNORED.
        {
            set_latest_event_value(&mut room_a, none());
            set_latest_event_value(&mut room_b, remote(3));

            assert_eq!(extract_scores(&room_a, &room_b), (None, Some(3)));
        }

        // `room_b` has `None`, `room_a` has something else.
        //
        // One of the room has a latest event, so the recency stamp MUST BE IGNORED.
        {
            set_latest_event_value(&mut room_a, remote(3));
            set_latest_event_value(&mut room_b, none());

            assert_eq!(extract_scores(&room_a, &room_b), (Some(3), None));
        }
    }

    #[async_test]
    async fn test_extract_scores_with_remote_or_local() {
        let (client, server) = logged_in_client_with_server().await;
        let [mut room_a, mut room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        set_recency_stamp(&mut room_a, 1.into());
        set_recency_stamp(&mut room_b, 2.into());

        // `room_a` and `room_b` has either `Remote` or `Local*`.
        //
        // Both rooms have a latest event, so the recency stamp MUST BE IGNORED.
        {
            for latest_event_value_a in [remote(3), local_is_sending(3), local_cannot_be_sent(3)] {
                for latest_event_value_b in
                    [remote(4), local_is_sending(4), local_cannot_be_sent(4)]
                {
                    set_latest_event_value(&mut room_a, latest_event_value_a.clone());
                    set_latest_event_value(&mut room_b, latest_event_value_b);

                    assert_eq!(extract_scores(&room_a, &room_b), (Some(3), Some(4)));
                }
            }
        }
    }

    #[async_test]
    async fn test_with_two_scores() {
        let (client, server) = logged_in_client_with_server().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `room_a` has an older recency stamp than `room_b`.
        {
            // `room_a` has a smaller score than `room_b`, i.e. it must come
            // after `room_b`.
            assert_eq!(
                cmp(|_left, _right| (Some(1), Some(2)), &room_a, &room_b),
                Ordering::Greater
            );
        }

        // `room_b` has a smaller score than `room_a`.
        {
            // `room_a` has a greater score than `room_b`, i.e. it must come
            // before `room_b`.
            assert_eq!(cmp(|_left, _right| (Some(2), Some(1)), &room_a, &room_b), Ordering::Less);
        }

        // `room_a` has the same score than `room_b`.
        {
            assert_eq!(cmp(|_left, _right| (Some(1), Some(1)), &room_a, &room_b), Ordering::Equal);
        }
    }

    #[async_test]
    async fn test_with_one_score() {
        let (client, server) = logged_in_client_with_server().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `room_a` has a score, but `room_b` has none: `room_a` must come
        // before `room_b`.
        {
            assert_eq!(cmp(|_left, _right| (Some(1), None), &room_a, &room_b), Ordering::Less);
        }

        // `room_a` has no score, but `room_b` has one: `room_a` must come after
        // `room_b`.
        {
            assert_eq!(cmp(|_left, _right| (None, Some(1)), &room_a, &room_b), Ordering::Greater);
        }
    }

    #[async_test]
    async fn test_with_zero_score() {
        let (client, server) = logged_in_client_with_server().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `room_a` and `room_b` has no score: they are equal.
        {
            assert_eq!(cmp(|_left, _right| (None, None), &room_a, &room_b), Ordering::Equal);
        }
    }

    prop_compose! {
        fn arb_score()(score in any::<Option<u64>>()) -> Option<Score> {
            score
        }
    }

    // Property tests to ensure that our comparison implementation for the `Score`
    // is total[1]. If it wasn't so, a call to `sort_by()` with the given
    // `cmp()` method could panic.
    //
    // [1]: https://en.wikipedia.org/wiki/Total_order
    proptest! {
        // Test that the ordering is reflexive, that is, each `Score` needs to be equal to itself.
        // a <= a.
        #![proptest_config(ProptestConfig::with_cases(10_000))]
        #[test]
        fn test_cmp_reflexive(score in arb_score()) {
            let result = cmp_impl(score, score);
            assert!(result == Ordering::Less || result == Ordering::Equal);
        }

        // Test that the ordering is transitive, that is, if a <= b and b <= c then a <= c.
        #[test]
        fn test_cmp_transitive(a in arb_score(), b in arb_score(), c in arb_score()) {
            use Ordering::*;

            let a_b_comparison = cmp_impl(a, b);
            let b_c_comparison = cmp_impl(b, c);
            let a_c_comparison = cmp_impl(a, c);

            if matches!(a_b_comparison, Less | Equal) && matches!(b_c_comparison, Less | Equal) {
                assert!(a_c_comparison == Less || a_c_comparison == Equal);
            }
        }

        // Test that the ordering is antisymetric, that is, if a <= b and b <= a then a = b.
        #[test]
        fn test_cmp_antisymetric(a in arb_score(), b in arb_score()) {
            use Ordering::*;

            let a_b_comparison = cmp_impl(a, b);
            let b_a_comparison = cmp_impl(b, a);

            eprintln!("a.cmp(b) = {a_b_comparison:?}");
            eprintln!("b.cmp(a) = {b_a_comparison:?}");

            if matches!(a_b_comparison, Less | Equal) && matches!(b_a_comparison, Less | Equal) {
                assert!(a == b);
            }
        }

        // Test that the ordering is total, that is, we have either a <= b or b <= a.
        #[test]
        fn test_cmp_reverse(a in arb_score(), b in arb_score()) {
            use Ordering::*;

            let a_b_comparison = cmp_impl(a, b);
            let b_a_comparison = cmp_impl(b, a);

            if a_b_comparison == Less {
                assert_eq!(b_a_comparison, Greater);
            } else if a_b_comparison == Greater {
                assert_eq!(b_a_comparison, Less);
            } else {
                assert_eq!(a_b_comparison, Equal);
                assert_eq!(b_a_comparison, Equal);
            }
        }
    }
}
