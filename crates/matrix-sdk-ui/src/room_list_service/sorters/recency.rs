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

use matrix_sdk::latest_events::LatestEventValue;

use super::{Room, Sorter};

struct RecencySorter<F>
where
    F: Fn(&Room, &Room) -> (Option<Rank>, Option<Rank>),
{
    ranks: F,
}

impl<F> RecencySorter<F>
where
    F: Fn(&Room, &Room) -> (Option<Rank>, Option<Rank>),
{
    fn cmp(&self, left: &Room, right: &Room) -> Ordering {
        if left.room_id() == right.room_id() {
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

        match (self.ranks)(left, right) {
            (Some(left_rank), Some(right_rank)) => left_rank.cmp(&right_rank).reverse(),

            (Some(_), None) => Ordering::Less,

            (None, Some(_)) => Ordering::Greater,

            (None, None) => Ordering::Equal,
        }
    }
}

/// Create a new sorter that will sort two [`Room`] by recency, i.e. by
/// comparing their [`RoomInfo::new_latest_event`]'s recency (timestamp)
/// if any (i.e. if different from [`LatestEventValue::None`]), or their
/// [`RoomInfo::recency_stamp`] value. The `Room` with the newest recency stamp
/// comes first, i.e. newest < oldest.
///
/// [`RoomInfo::recency_stamp`]: matrix_sdk_base::RoomInfo::recency_stamp
/// [`RoomInfo::new_latest_event`]: matrix_sdk_base::RoomInfo::new_latest_event
pub fn new_sorter() -> impl Sorter {
    let sorter = RecencySorter { ranks: move |left, right| extract_rank(left, right) };

    move |left, right| -> Ordering { sorter.cmp(left, right) }
}

/// The term _rank_ is used here to avoid any confusion with a _timestamp_ (a
/// `u64` from the latest event), or a _recency stamp_ (a `u64` from the recency
/// stamp of the room). This type hides `u64` for the sake of semantics.
type Rank = u64;

/// Extract the recency _rank_ from either the [`RoomInfo::new_latest_event`] or
/// from [`RoomInfo::recency_stamp`].
///
/// We must be very careful to return data of the same nature: either a
/// _rank_ from the [`LatestEventValue`]'s timestamp, or from the
/// [`RoomInfo::recency_stamp`], but we **must never** mix both. The
/// `RoomInfo::recency_stamp` is not a timestamp, while `LatestEventValue` uses
/// a timestamp.
fn extract_rank(left: &Room, right: &Room) -> (Option<Rank>, Option<Rank>) {
    match (left.new_latest_event(), right.new_latest_event()) {
        // None of both rooms, or only one of both rooms, have a latest event value. Let's fallback
        // to the recency stamp from the `RoomInfo` for both room.
        (LatestEventValue::None, LatestEventValue::None)
        | (LatestEventValue::None, _)
        | (_, LatestEventValue::None) => {
            (left.recency_stamp().map(Into::into), right.recency_stamp().map(Into::into))
        }

        // Both rooms have a non-`None` latest event. We can use their timestamps as a rank.
        (
            left @ LatestEventValue::Remote(_)
            | left @ LatestEventValue::LocalIsSending(_)
            | left @ LatestEventValue::LocalCannotBeSent(_),
            right @ LatestEventValue::Remote(_)
            | right @ LatestEventValue::LocalIsSending(_)
            | right @ LatestEventValue::LocalCannotBeSent(_),
        ) => (left.timestamp(), right.timestamp()),
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::{
        RoomRecencyStamp,
        latest_events::{LocalLatestEventValue, RemoteLatestEventValue},
        store::SerializableEventContent,
        test_utils::logged_in_client_with_server,
    };
    use matrix_sdk_base::RoomInfoNotableUpdateReasons;
    use matrix_sdk_test::async_test;
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
            content: SerializableEventContent::from_raw(
                Raw::new(&AnyMessageLikeEventContent::RoomMessage(
                    RoomMessageEventContent::text_plain("raclette"),
                ))
                .unwrap(),
                "m.room.message".to_owned(),
            ),
        })
    }

    fn local_cannot_be_sent(origin_server_ts: u32) -> LatestEventValue {
        LatestEventValue::LocalCannotBeSent(LocalLatestEventValue {
            timestamp: MilliSecondsSinceUnixEpoch(origin_server_ts.into()),
            content: SerializableEventContent::from_raw(
                Raw::new(&AnyMessageLikeEventContent::RoomMessage(
                    RoomMessageEventContent::text_plain("raclette"),
                ))
                .unwrap(),
                "m.room.message".to_owned(),
            ),
        })
    }

    fn set_latest_event_value(room: &mut Room, latest_event_value: LatestEventValue) {
        let mut room_info = room.clone_info();
        room_info.set_new_latest_event(latest_event_value);
        room.set_room_info(room_info, RoomInfoNotableUpdateReasons::LATEST_EVENT);
    }

    fn set_recency_stamp(room: &mut Room, recency_stamp: RoomRecencyStamp) {
        let mut room_info = room.clone_info();
        room_info.update_recency_stamp(recency_stamp);
        room.set_room_info(room_info, RoomInfoNotableUpdateReasons::RECENCY_STAMP);
    }

    #[async_test]
    async fn test_extract_recency_stamp_with_none() {
        let (client, server) = logged_in_client_with_server().await;
        let [mut room_a, mut room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        set_recency_stamp(&mut room_a, 1.into());
        set_recency_stamp(&mut room_b, 2.into());

        // Both rooms have a `LatestEventValue::None`.
        {
            set_latest_event_value(&mut room_a, none());
            set_latest_event_value(&mut room_b, none());

            assert_eq!(extract_rank(&room_a, &room_b), (Some(1), Some(2)));
        }

        // `room_a` has `None`, `room_b` has something else.
        {
            set_latest_event_value(&mut room_a, none());
            set_latest_event_value(&mut room_b, remote(3));

            assert_eq!(extract_rank(&room_a, &room_b), (Some(1), Some(2)));
        }

        // `room_b` has `None`, `room_a` has something else.
        {
            set_latest_event_value(&mut room_a, remote(3));
            set_latest_event_value(&mut room_b, none());

            assert_eq!(extract_rank(&room_a, &room_b), (Some(1), Some(2)));
        }
    }

    #[async_test]
    async fn test_extract_recency_stamp_with_remote_or_local() {
        let (client, server) = logged_in_client_with_server().await;
        let [mut room_a, mut room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        set_recency_stamp(&mut room_a, 1.into());
        set_recency_stamp(&mut room_b, 2.into());

        // `room_a` and `room_b` has either `Remote` or `Local*`.
        {
            for latest_event_value_a in [remote(3), local_is_sending(3), local_cannot_be_sent(3)] {
                for latest_event_value_b in
                    [remote(4), local_is_sending(4), local_cannot_be_sent(4)]
                {
                    set_latest_event_value(&mut room_a, latest_event_value_a.clone());
                    set_latest_event_value(&mut room_b, latest_event_value_b);

                    assert_eq!(extract_rank(&room_a, &room_b), (Some(3), Some(4)));
                }
            }
        }
    }

    #[async_test]
    async fn test_with_two_recency_stamps() {
        let (client, server) = logged_in_client_with_server().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `room_a` has an older recency stamp than `room_b`.
        {
            let sorter = RecencySorter { ranks: |_left, _right| (Some(1), Some(2)) };

            // `room_a` is greater than `room_b`, i.e. it must come after `room_b`.
            assert_eq!(sorter.cmp(&room_a, &room_b), Ordering::Greater);
        }

        // `room_b` has an older recency stamp than `room_a`.
        {
            let sorter = RecencySorter { ranks: |_left, _right| (Some(2), Some(1)) };

            // `room_a` is less than `room_b`, i.e. it must come before `room_b`.
            assert_eq!(sorter.cmp(&room_a, &room_b), Ordering::Less);
        }

        // `room_a` has an equally old recency stamp than `room_b`.
        {
            let sorter = RecencySorter { ranks: |_left, _right| (Some(1), Some(1)) };

            assert_eq!(sorter.cmp(&room_a, &room_b), Ordering::Equal);
        }
    }

    #[async_test]
    async fn test_with_one_recency_stamp() {
        let (client, server) = logged_in_client_with_server().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `room_a` has a recency stamp, `room_b` has no recency stamp.
        {
            let sorter = RecencySorter { ranks: |_left, _right| (Some(1), None) };

            assert_eq!(sorter.cmp(&room_a, &room_b), Ordering::Less);
        }

        // `room_a` has no recency stamp, `room_b` has a recency stamp.
        {
            let sorter = RecencySorter { ranks: |_left, _right| (None, Some(1)) };

            assert_eq!(sorter.cmp(&room_a, &room_b), Ordering::Greater);
        }
    }

    #[async_test]
    async fn test_with_zero_recency_stamp() {
        let (client, server) = logged_in_client_with_server().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `room_a` and `room_b` has no recency stamp.
        {
            let sorter = RecencySorter { ranks: |_left, _right| (None, None) };

            assert_eq!(sorter.cmp(&room_a, &room_b), Ordering::Equal);
        }
    }
}
