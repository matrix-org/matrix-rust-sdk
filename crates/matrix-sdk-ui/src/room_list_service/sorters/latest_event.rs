// Copyright 2025 The Matrix.org Foundation C.I.C.
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

fn cmp<F>(are_latest_events_unsent: F, left: &RoomListItem, right: &RoomListItem) -> Ordering
where
    F: Fn(&RoomListItem, &RoomListItem) -> (bool, bool),
{
    // We want latest events representing unsent events to come firsts. When
    // there is a remote latest event or no latest event, we don't want to sort
    // them.
    // NOTE: This is the same as a.cmp(b).reverse() for booleans.
    match are_latest_events_unsent(left, right) {
        // `false` and `false`, i.e.:
        // - `None` == `None`.
        // - `None` == `Remote`.
        // - `Remote` == `None`.
        // - `Remote` == `Remote`.
        (false, false) => Ordering::Equal,

        // `false` and `true`, i.e.:
        // - `None` > `Local*`.
        // - `Remote` > `Local*`.
        (false, true) => Ordering::Greater,

        // `true` and `false`, i.e.:
        // - `Local*` < `None`.
        // - `Local*` < `Remote`.
        (true, false) => Ordering::Less,

        // `true` and `true`, i.e.:
        // - `Local*` == `Local*`
        (true, true) => Ordering::Equal,
    }
}

/// Create a new sorter that will sort two [`RoomListItem`] by their latest
/// events' state: latest events representing unsent events come firsts.
pub fn new_sorter() -> impl Sorter {
    let latest_events = |left: &RoomListItem, right: &RoomListItem| {
        // Be careful. This method is called **a lot** in the context of a
        // sorter. Using `Room::latest_event` would be dramatic as it returns a
        // clone of the `LatestEventValue`. It's better to use the more specific
        // method `Room::latest_event_is_unsent`, where the value is cached
        // in `RoomListItem`.
        (left.cached_latest_event_is_unsent, right.cached_latest_event_is_unsent)
    };

    move |left, right| -> Ordering { cmp(latest_events, left, right) }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::{
        latest_events::{LatestEventValue, LocalLatestEventValue, RemoteLatestEventValue},
        store::SerializableEventContent,
        test_utils::logged_in_client_with_server,
    };
    use matrix_sdk_test::async_test;
    use ruma::{
        MilliSecondsSinceUnixEpoch,
        events::{AnyMessageLikeEventContent, room::message::RoomMessageEventContent},
        owned_event_id, room_id,
        serde::Raw,
        uint,
    };
    use serde_json::json;

    use super::{super::super::filters::new_rooms, *};

    fn none() -> LatestEventValue {
        LatestEventValue::None
    }

    fn remote() -> LatestEventValue {
        LatestEventValue::Remote(RemoteLatestEventValue::from_plaintext(
            Raw::from_json_string(
                json!({
                    "content": RoomMessageEventContent::text_plain("raclette"),
                    "type": "m.room.message",
                    "event_id": "$ev0",
                    "room_id": "!r0",
                    "origin_server_ts": 42,
                    "sender": "@mnt_io:matrix.org",
                })
                .to_string(),
            )
            .unwrap(),
        ))
    }

    fn local_is_sending() -> LatestEventValue {
        LatestEventValue::LocalIsSending(LocalLatestEventValue {
            timestamp: MilliSecondsSinceUnixEpoch(uint!(42)),
            content: SerializableEventContent::new(&AnyMessageLikeEventContent::RoomMessage(
                RoomMessageEventContent::text_plain("raclette"),
            ))
            .unwrap(),
        })
    }

    fn local_has_been_sent() -> LatestEventValue {
        LatestEventValue::LocalHasBeenSent {
            event_id: owned_event_id!("$ev0"),
            value: LocalLatestEventValue {
                timestamp: MilliSecondsSinceUnixEpoch(uint!(42)),
                content: SerializableEventContent::new(&AnyMessageLikeEventContent::RoomMessage(
                    RoomMessageEventContent::text_plain("raclette"),
                ))
                .unwrap(),
            },
        }
    }

    fn local_cannot_be_sent() -> LatestEventValue {
        LatestEventValue::LocalCannotBeSent(LocalLatestEventValue {
            timestamp: MilliSecondsSinceUnixEpoch(uint!(42)),
            content: SerializableEventContent::new(&AnyMessageLikeEventContent::RoomMessage(
                RoomMessageEventContent::text_plain("raclette"),
            ))
            .unwrap(),
        })
    }

    fn pair(left: LatestEventValue, right: LatestEventValue) -> (bool, bool) {
        (left.is_unsent(), right.is_unsent())
    }

    #[async_test]
    async fn test_none_or_remote_and_none_or_remote() {
        let (client, server) = logged_in_client_with_server().await;

        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `None` and `None`.
        {
            assert_eq!(cmp(|_, _| pair(none(), none()), &room_a, &room_b), Ordering::Equal);
        }

        // `None` and `Remote`.
        {
            assert_eq!(cmp(|_, _| pair(none(), remote()), &room_a, &room_b), Ordering::Equal);
        }

        // `Remote` and `None`.
        {
            assert_eq!(cmp(|_, _| pair(remote(), none()), &room_a, &room_b), Ordering::Equal);
        }

        // `Remote` and `None`.
        {
            assert_eq!(cmp(|_, _| pair(remote(), remote()), &room_a, &room_b), Ordering::Equal);
        }
    }

    #[async_test]
    async fn test_none_or_remote_and_local() {
        let (client, server) = logged_in_client_with_server().await;

        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `None` and `Local*`.
        {
            assert_eq!(
                cmp(|_, _| pair(none(), local_is_sending()), &room_a, &room_b),
                Ordering::Greater
            );
            assert_eq!(
                cmp(|_, _| pair(none(), local_has_been_sent()), &room_a, &room_b),
                Ordering::Equal
            );
            assert_eq!(
                cmp(|_, _| pair(none(), local_cannot_be_sent()), &room_a, &room_b),
                Ordering::Greater
            );
        }

        // `Remote` and `Local*`.
        {
            assert_eq!(
                cmp(|_, _| pair(remote(), local_is_sending()), &room_a, &room_b),
                Ordering::Greater
            );
            assert_eq!(
                cmp(|_, _| pair(remote(), local_has_been_sent()), &room_a, &room_b),
                Ordering::Equal
            );
            assert_eq!(
                cmp(|_, _| pair(remote(), local_cannot_be_sent()), &room_a, &room_b),
                Ordering::Greater
            );
        }
    }

    #[async_test]
    async fn test_local_and_none_or_remote() {
        let (client, server) = logged_in_client_with_server().await;

        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `Local*` and `None`.
        {
            assert_eq!(
                cmp(|_, _| pair(local_is_sending(), none()), &room_a, &room_b),
                Ordering::Less
            );
            assert_eq!(
                cmp(|_, _| pair(local_has_been_sent(), none()), &room_a, &room_b),
                Ordering::Equal
            );
            assert_eq!(
                cmp(|_, _| pair(local_cannot_be_sent(), none()), &room_a, &room_b),
                Ordering::Less
            );
        }

        // `Local*` and `Remote`.
        {
            assert_eq!(
                cmp(|_, _| pair(local_is_sending(), remote()), &room_a, &room_b),
                Ordering::Less
            );
            assert_eq!(
                cmp(|_, _| pair(local_has_been_sent(), remote()), &room_a, &room_b),
                Ordering::Equal
            );
            assert_eq!(
                cmp(|_, _| pair(local_cannot_be_sent(), remote()), &room_a, &room_b),
                Ordering::Less
            );
        }
    }

    #[async_test]
    async fn test_local_and_local() {
        let (client, server) = logged_in_client_with_server().await;

        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `Local*` and `Local*`.
        {
            assert_eq!(
                cmp(|_, _| pair(local_is_sending(), local_is_sending()), &room_a, &room_b),
                Ordering::Equal
            );
            assert_eq!(
                cmp(|_, _| pair(local_is_sending(), local_has_been_sent()), &room_a, &room_b),
                Ordering::Less
            );
            assert_eq!(
                cmp(|_, _| pair(local_is_sending(), local_cannot_be_sent()), &room_a, &room_b),
                Ordering::Equal
            );

            assert_eq!(
                cmp(|_, _| pair(local_has_been_sent(), local_is_sending()), &room_a, &room_b),
                Ordering::Greater
            );
            assert_eq!(
                cmp(|_, _| pair(local_has_been_sent(), local_has_been_sent()), &room_a, &room_b),
                Ordering::Equal
            );
            assert_eq!(
                cmp(|_, _| pair(local_has_been_sent(), local_cannot_be_sent()), &room_a, &room_b),
                Ordering::Greater
            );

            assert_eq!(
                cmp(|_, _| pair(local_cannot_be_sent(), local_is_sending()), &room_a, &room_b),
                Ordering::Equal
            );
            assert_eq!(
                cmp(|_, _| pair(local_cannot_be_sent(), local_has_been_sent()), &room_a, &room_b),
                Ordering::Less
            );
            assert_eq!(
                cmp(|_, _| pair(local_cannot_be_sent(), local_cannot_be_sent()), &room_a, &room_b),
                Ordering::Equal
            );
        }
    }
}
