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

use matrix_sdk::latest_events::LatestEventValue;

use super::{Room, Sorter};

struct LatestEventMatcher<F>
where
    F: Fn(&Room, &Room) -> (LatestEventValue, LatestEventValue),
{
    latest_events: F,
}

impl<F> LatestEventMatcher<F>
where
    F: Fn(&Room, &Room) -> (LatestEventValue, LatestEventValue),
{
    fn matches(&self, left: &Room, right: &Room) -> Ordering {
        // We want local latest event to come first. When there is a remote latest event
        // or no latest event, we don't want to sort them.
        match (self.latest_events)(left, right) {
            // `None` == `None`.
            // `None` == `Remote`.
            // `Remote` == `None`.
            // `Remote` == `Remote`.
            (
                LatestEventValue::None | LatestEventValue::Remote(_),
                LatestEventValue::None | LatestEventValue::Remote(_),
            ) => Ordering::Equal,

            // `None` > `Local*`.
            // `Remote` > `Local*`.
            (
                LatestEventValue::None | LatestEventValue::Remote(_),
                LatestEventValue::LocalIsSending(_) | LatestEventValue::LocalCannotBeSent(_),
            ) => Ordering::Greater,

            // `Local*` < `None`.
            // `Local*` < `Remote`.
            (
                LatestEventValue::LocalIsSending(_) | LatestEventValue::LocalCannotBeSent(_),
                LatestEventValue::None | LatestEventValue::Remote(_),
            ) => Ordering::Less,

            // `Local*` == `Local*`
            (
                LatestEventValue::LocalIsSending(_) | LatestEventValue::LocalCannotBeSent(_),
                LatestEventValue::LocalIsSending(_) | LatestEventValue::LocalCannotBeSent(_),
            ) => Ordering::Equal,
        }
    }
}

/// Create a new sorter that will sort two [`Room`] by their latest events'
/// state: latest events representing a local event
/// ([`LatestEventValue::LocalIsSending`] or
/// [`LatestEventValue::LocalCannotBeSent`]) come first, and latest event
/// representing a remote event ([`LatestEventValue::Remote`]) come last.
pub fn new_sorter() -> impl Sorter {
    let matcher = LatestEventMatcher {
        latest_events: move |left, right| (left.new_latest_event(), right.new_latest_event()),
    };

    move |left, right| -> Ordering { matcher.matches(left, right) }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::{
        latest_events::{LocalLatestEventValue, RemoteLatestEventValue},
        store::SerializableEventContent,
        test_utils::logged_in_client_with_server,
    };
    use matrix_sdk_test::async_test;
    use ruma::{
        MilliSecondsSinceUnixEpoch,
        events::{AnyMessageLikeEventContent, room::message::RoomMessageEventContent},
        room_id,
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
            content: SerializableEventContent::from_raw(
                Raw::new(&AnyMessageLikeEventContent::RoomMessage(
                    RoomMessageEventContent::text_plain("raclette"),
                ))
                .unwrap(),
                "m.room.message".to_owned(),
            ),
        })
    }

    fn local_cannot_be_sent() -> LatestEventValue {
        LatestEventValue::LocalCannotBeSent(LocalLatestEventValue {
            timestamp: MilliSecondsSinceUnixEpoch(uint!(42)),
            content: SerializableEventContent::from_raw(
                Raw::new(&AnyMessageLikeEventContent::RoomMessage(
                    RoomMessageEventContent::text_plain("raclette"),
                ))
                .unwrap(),
                "m.room.message".to_owned(),
            ),
        })
    }

    #[async_test]
    async fn test_none_or_remote_and_none_or_remote() {
        let (client, server) = logged_in_client_with_server().await;

        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `None` and `None`.
        {
            let matcher = LatestEventMatcher { latest_events: |_, _| (none(), none()) };
            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Equal);
        }

        // `None` and `Remote`.
        {
            let matcher = LatestEventMatcher { latest_events: |_, _| (none(), remote()) };
            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Equal);
        }

        // `Remote` and `None`.
        {
            let matcher = LatestEventMatcher { latest_events: |_, _| (remote(), none()) };
            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Equal);
        }

        // `Remote` and `None`.
        {
            let matcher = LatestEventMatcher { latest_events: |_, _| (remote(), remote()) };
            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Equal);
        }
    }

    #[async_test]
    async fn test_none_or_remote_and_local() {
        let (client, server) = logged_in_client_with_server().await;

        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `None` and `Local*`.
        {
            let matcher = LatestEventMatcher { latest_events: |_, _| (none(), local_is_sending()) };
            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Greater);

            let matcher =
                LatestEventMatcher { latest_events: |_, _| (none(), local_cannot_be_sent()) };
            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Greater);
        }

        // `Remote` and `Local*`.
        {
            let matcher =
                LatestEventMatcher { latest_events: |_, _| (remote(), local_is_sending()) };
            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Greater);

            let matcher =
                LatestEventMatcher { latest_events: |_, _| (remote(), local_cannot_be_sent()) };
            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Greater);
        }
    }

    #[async_test]
    async fn test_local_and_none_or_remote() {
        let (client, server) = logged_in_client_with_server().await;

        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `Local*` and `None`.
        {
            let matcher = LatestEventMatcher { latest_events: |_, _| (local_is_sending(), none()) };
            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Less);

            let matcher =
                LatestEventMatcher { latest_events: |_, _| (local_cannot_be_sent(), none()) };
            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Less);
        }

        // `Local*` and `Remote`.
        {
            let matcher =
                LatestEventMatcher { latest_events: |_, _| (local_is_sending(), remote()) };
            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Less);

            let matcher =
                LatestEventMatcher { latest_events: |_, _| (local_cannot_be_sent(), remote()) };
            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Less);
        }
    }

    #[async_test]
    async fn test_local_and_local() {
        let (client, server) = logged_in_client_with_server().await;

        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server).await;

        // `Local*` and `Local*`.
        {
            let matcher = LatestEventMatcher {
                latest_events: |_, _| (local_is_sending(), local_is_sending()),
            };
            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Equal);

            let matcher = LatestEventMatcher {
                latest_events: |_, _| (local_is_sending(), local_cannot_be_sent()),
            };
            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Equal);

            let matcher = LatestEventMatcher {
                latest_events: |_, _| (local_cannot_be_sent(), local_is_sending()),
            };
            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Equal);

            let matcher = LatestEventMatcher {
                latest_events: |_, _| (local_cannot_be_sent(), local_cannot_be_sent()),
            };
            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Equal);
        }
    }
}
