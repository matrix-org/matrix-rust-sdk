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

use std::{cmp::Ordering, ops::Deref};

use matrix_sdk_base::latest_event::LatestEvent;

use super::{Room, Sorter};

struct RecencyMatcher<F>
where
    F: Fn(&Room, &Room) -> (Option<LatestEvent>, Option<LatestEvent>),
{
    latest_events: F,
}

impl<F> RecencyMatcher<F>
where
    F: Fn(&Room, &Room) -> (Option<LatestEvent>, Option<LatestEvent>),
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
            // search algorithm. `left` and `right` will have the exact same `LatestEvent`,
            // so `left` and `right` will always be `Ordering::Equal`. This is wrong: if
            // `left` is compared with `right` and if they are both the same room, it means
            // that one of them (either `left`, or `right`, it's not important) has received
            // an update. The room position is very likely to change. But if they compare to
            // `Equal`, the position may not change. It actually depends of the search
            // algorithm used by [`eyeball_im_util::SortBy`].
            //
            // Since this room received an update, it is more recent than the previous one
            // we matched against, so return `Ordering::Greater`.
            return Ordering::Greater;
        }

        match (self.latest_events)(left, right) {
            (Some(left_latest_event), Some(right_latest_event)) => left_latest_event
                .event_origin_server_ts()
                .cmp(&right_latest_event.event_origin_server_ts())
                .reverse(),

            (Some(_), None) => Ordering::Less,

            (None, Some(_)) => Ordering::Greater,

            (None, None) => Ordering::Equal,
        }
    }
}

/// Create a new sorter that will sort two [`Room`] by recency, i.e. by
/// comparing their `origin_server_ts` value. The `Room` with the newest
/// `origin_server_ts` comes first, i.e. newest < oldest.
pub fn new_sorter() -> impl Sorter {
    let matcher = RecencyMatcher {
        latest_events: move |left, right| {
            (left.deref().latest_event(), right.deref().latest_event())
        },
    };

    move |left, right| -> Ordering { matcher.matches(left, right) }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::{async_test, sync_timeline_event, ALICE, BOB};
    use ruma::room_id;

    use super::{
        super::super::filters::{client_and_server_prelude, new_rooms},
        *,
    };

    #[async_test]
    async fn test_with_two_latest_events() {
        let (client, server, sliding_sync) = client_and_server_prelude().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server, &sliding_sync)
                .await;

        // `room_a` has an older latest event than `room_b`.
        {
            let matcher = RecencyMatcher {
                latest_events: |_left, _right| {
                    (
                        Some(LatestEvent::new(
                            sync_timeline_event!({
                                "content": {
                                    "body": "foo",
                                    "msgtype": "m.text",
                                },
                                "sender": &*ALICE,
                                "event_id": "$foo",
                                "origin_server_ts": 1,
                                "type": "m.room.message",
                            })
                            .into(),
                        )),
                        Some(LatestEvent::new(
                            sync_timeline_event!({
                                "content": {
                                    "body": "bar",
                                    "msgtype": "m.text",
                                },
                                "sender": &*BOB,
                                "event_id": "$bar",
                                "origin_server_ts": 2,
                                "type": "m.room.message",
                            })
                            .into(),
                        )),
                    )
                },
            };

            // `room_a` is greater than `room_b`, i.e. it must come after `room_b`.
            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Greater);
        }

        // `room_b` has an older latest event than `room_a`.
        {
            let matcher = RecencyMatcher {
                latest_events: |_left, _right| {
                    (
                        Some(LatestEvent::new(
                            sync_timeline_event!({
                                "content": {
                                    "body": "foo",
                                    "msgtype": "m.text",
                                },
                                "sender": &*ALICE,
                                "event_id": "$foo",
                                "origin_server_ts": 2,
                                "type": "m.room.message",
                            })
                            .into(),
                        )),
                        Some(LatestEvent::new(
                            sync_timeline_event!({
                                "content": {
                                    "body": "bar",
                                    "msgtype": "m.text",
                                },
                                "sender": &*BOB,
                                "event_id": "$bar",
                                "origin_server_ts": 1,
                                "type": "m.room.message",
                            })
                            .into(),
                        )),
                    )
                },
            };

            // `room_a` is less than `room_b`, i.e. it must come before `room_b`.
            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Less);
        }

        // `room_a` has an equally old latest event than `room_b`.
        {
            let matcher = RecencyMatcher {
                latest_events: |_left, _right| {
                    (
                        Some(LatestEvent::new(
                            sync_timeline_event!({
                                "content": {
                                    "body": "foo",
                                    "msgtype": "m.text",
                                },
                                "sender": &*ALICE,
                                "event_id": "$foo",
                                "origin_server_ts": 1,
                                "type": "m.room.message",
                            })
                            .into(),
                        )),
                        Some(LatestEvent::new(
                            sync_timeline_event!({
                                "content": {
                                    "body": "bar",
                                    "msgtype": "m.text",
                                },
                                "sender": &*BOB,
                                "event_id": "$bar",
                                "origin_server_ts": 1,
                                "type": "m.room.message",
                            })
                            .into(),
                        )),
                    )
                },
            };

            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Equal);
        }
    }

    #[async_test]
    async fn test_with_one_latest_event() {
        let (client, server, sliding_sync) = client_and_server_prelude().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server, &sliding_sync)
                .await;

        // `room_a` has a latest event, `room_b` has no latest event.
        {
            let matcher = RecencyMatcher {
                latest_events: |_left, _right| {
                    (
                        Some(LatestEvent::new(
                            sync_timeline_event!({
                                "content": {
                                    "body": "foo",
                                    "msgtype": "m.text",
                                },
                                "sender": &*ALICE,
                                "event_id": "$foo",
                                "origin_server_ts": 1,
                                "type": "m.room.message",
                            })
                            .into(),
                        )),
                        None,
                    )
                },
            };

            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Less);
        }

        // `room_a` has no latest event, `room_b` has a latest event.
        {
            let matcher = RecencyMatcher {
                latest_events: |_left, _right| {
                    (
                        None,
                        Some(LatestEvent::new(
                            sync_timeline_event!({
                                "content": {
                                    "body": "bar",
                                    "msgtype": "m.text",
                                },
                                "sender": &*BOB,
                                "event_id": "$bar",
                                "origin_server_ts": 1,
                                "type": "m.room.message",
                            })
                            .into(),
                        )),
                    )
                },
            };

            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Greater);
        }
    }

    #[async_test]
    async fn test_with_zero_latest_event() {
        let (client, server, sliding_sync) = client_and_server_prelude().await;
        let [room_a, room_b] =
            new_rooms([room_id!("!a:b.c"), room_id!("!d:e.f")], &client, &server, &sliding_sync)
                .await;

        // `room_a` and `room_b` has no latest event.
        {
            let matcher = RecencyMatcher { latest_events: |_left, _right| (None, None) };

            assert_eq!(matcher.matches(&room_a, &room_b), Ordering::Equal);
        }
    }
}
