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

fn cmp<F>(timestamps: F, left: &RoomListItem, right: &RoomListItem) -> Ordering
where
    F: Fn(&RoomListItem, &RoomListItem) -> (Option<Rank>, Option<Rank>),
{
    match timestamps(left, right) {
        (Some(left), Some(right)) => left.cmp(&right).reverse(),

        (Some(_), None) => Ordering::Less,

        (None, Some(_)) => Ordering::Greater,

        (None, None) => Ordering::Equal,
    }
}

/// Create a new sorter that will sort two [`RoomListItem`] by recency, i.e.
/// by comparing their [`RoomInfo::new_latest_event`]'s recency (timestamp)
/// if any (i.e. if different from [`LatestEventValue::None`]), or their
/// [`RoomInfo::recency_stamp`] value. The `Room` with the newest recency stamp
/// comes first, i.e. newest < oldest.
///
/// [`RoomInfo::recency_stamp`]: matrix_sdk_base::RoomInfo::recency_stamp
/// [`RoomInfo::new_latest_event`]: matrix_sdk_base::RoomInfo::new_latest_event
/// [`LatestEventValue::None`]: matrix_sdk_base::latest_event::LatestEventValue::None
pub fn new_sorter() -> impl Sorter {
    let ranks = |left: &RoomListItem, right: &RoomListItem| extract_rank(left, right);

    move |left, right| -> Ordering { cmp(ranks, left, right) }
}

/// The term _rank_ is used here to avoid any confusion with a _timestamp_ (a
/// `u64` from the latest event), or a _recency stamp_ (a `u64` from the recency
/// stamp of the room). This type hides `u64` for the sake of semantics.
type Rank = u64;

/// Extract the recency _rank_ from the [`RoomInfo::recency_stamp`].
// TODO @hywan: We must update this method to handle the latest event's
// timestamp instead of the recency stamp.
fn extract_rank(left: &RoomListItem, right: &RoomListItem) -> (Option<Rank>, Option<Rank>) {
    (left.cached_recency_stamp.map(Into::into), right.cached_recency_stamp.map(Into::into))
}

#[cfg(test)]
mod tests {
    use matrix_sdk::{
        RoomRecencyStamp,
        latest_events::{LatestEventValue, RemoteLatestEventValue},
        test_utils::logged_in_client_with_server,
    };
    use matrix_sdk_base::RoomInfoNotableUpdateReasons;
    use matrix_sdk_test::async_test;
    use ruma::{events::room::message::RoomMessageEventContent, room_id, serde::Raw};
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

    // TODO @hywan: restore this once `extract_rank` works on latest event's value
    /*
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
    */

    fn set_latest_event_value(room: &mut RoomListItem, latest_event_value: LatestEventValue) {
        let mut room_info = room.clone_info();
        room_info.set_new_latest_event(latest_event_value);
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
    async fn test_extract_rank_with_none() {
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

    // TODO @hywan: restore this once `extract_rank` works on latest event's value
    /*
    #[async_test]
    async fn test_extract_rank_with_remote_or_local() {
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
    */

    #[async_test]
    async fn test_with_two_ranks() {
        let (client, server) = logged_in_client_with_server().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `room_a` has an older recency stamp than `room_b`.
        {
            // `room_a` is greater than `room_b`, i.e. it must come after `room_b`.
            assert_eq!(
                cmp(|_left, _right| (Some(1), Some(2)), &room_a, &room_b),
                Ordering::Greater
            );
        }

        // `room_b` has an older recency stamp than `room_a`.
        {
            // `room_a` is less than `room_b`, i.e. it must come before `room_b`.
            assert_eq!(cmp(|_left, _right| (Some(2), Some(1)), &room_a, &room_b), Ordering::Less);
        }

        // `room_a` has an equally old recency stamp than `room_b`.
        {
            assert_eq!(cmp(|_left, _right| (Some(1), Some(1)), &room_a, &room_b), Ordering::Equal);
        }
    }

    #[async_test]
    async fn test_with_one_rank() {
        let (client, server) = logged_in_client_with_server().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `room_a` has a recency stamp, `room_b` has no recency stamp.
        {
            assert_eq!(cmp(|_left, _right| (Some(1), None), &room_a, &room_b), Ordering::Less);
        }

        // `room_a` has no recency stamp, `room_b` has a recency stamp.
        {
            assert_eq!(cmp(|_left, _right| (None, Some(1)), &room_a, &room_b), Ordering::Greater);
        }
    }

    #[async_test]
    async fn test_with_zero_rank() {
        let (client, server) = logged_in_client_with_server().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `room_a` and `room_b` has no recency stamp.
        {
            assert_eq!(cmp(|_left, _right| (None, None), &room_a, &room_b), Ordering::Equal);
        }
    }
}
