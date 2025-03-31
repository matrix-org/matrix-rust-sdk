// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use std::{pin::Pin, sync::Arc};

use anyhow::{Context as _, Result};
use assert_matches::assert_matches;
use assert_matches2::assert_let;
use eyeball_im::{Vector, VectorDiff};
use futures_util::{pin_mut, Stream, StreamExt};
use matrix_sdk::{
    test_utils::logged_in_client_with_server, Client, SlidingSync, SlidingSyncList,
    SlidingSyncListBuilder, SlidingSyncMode, UpdateSummary,
};
use matrix_sdk_test::{async_test, mocks::mock_encryption_state};
use matrix_sdk_ui::{
    timeline::{TimelineItem, TimelineItemKind},
    Timeline,
};
use ruma::{room_id, user_id, RoomId};
use serde_json::json;
use wiremock::{http::Method, Match, Mock, MockServer, Request, ResponseTemplate};

macro_rules! receive_response {
    (
        [$server:ident, $sliding_sync_stream:ident]
        $( $json:tt )+
    ) => {
        {
            let _mock_guard = Mock::given(SlidingSyncMatcher)
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    json!( $( $json )+ )
                ))
                .mount_as_scoped(&$server)
                .await;

            let next = $sliding_sync_stream.next().await.context("`sync` trip")??;

            next
        }
    };
}

macro_rules! timeline_event {
    ($event_id:literal at $ts:literal sec) => {
        json!({
            "event_id": $event_id,
            "sender": "@alice:bar.org",
            "type": "m.room.message",
            "content": {
                "body": "foo",
                "msgtype": "m.text",
            },
            "origin_server_ts": $ts,
        })
    }
}

pub(crate) use timeline_event;

macro_rules! assert_timeline_stream {
    // `--- date divider ---`
    ( @_ [ $iterator:ident ] [ --- date divider --- ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_timeline_stream!(
            @_
            [ $iterator ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                {
                    assert_matches!(
                        $iterator .next(),
                        Some(eyeball_im::VectorDiff::PushBack { value }) => {
                            assert_matches!(
                                **value,
                                matrix_sdk_ui::timeline::TimelineItemKind::Virtual(
                                    matrix_sdk_ui::timeline::VirtualTimelineItem::DateDivider(_)
                                )
                            );
                        }
                    );
                }
            ]
        )
    };

    // `append "$event_id"`
    ( @_ [ $iterator:ident ] [ append $event_id:literal ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_timeline_stream!(
            @_
            [ $iterator ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                {
                    assert_matches!(
                        $iterator .next(),
                        Some(eyeball_im::VectorDiff::PushBack { value }) => {
                            assert_matches!(
                                &**value,
                                matrix_sdk_ui::timeline::TimelineItemKind::Event(event_timeline_item) => {
                                    assert_eq!(event_timeline_item.event_id().unwrap().as_str(), $event_id);
                                }
                            );
                        }
                    );
                }
            ]
        )
    };

    // `prepend --- date divider ---`
    ( @_ [ $iterator:ident ] [ prepend --- date divider --- ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_timeline_stream!(
            @_
            [ $iterator ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                {
                    assert_matches!(
                        $iterator .next(),
                        Some(eyeball_im::VectorDiff::PushFront { value }) => {
                            assert_matches!(
                                &**value,
                                matrix_sdk_ui::timeline::TimelineItemKind::Virtual(
                                    matrix_sdk_ui::timeline::VirtualTimelineItem::DateDivider(_)
                                ) => {}
                            );
                        }
                    );
                }
            ]
        )
    };


    // `prepend --- timeline start ---`
    ( @_ [ $iterator:ident ] [ prepend --- timeline start --- ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_timeline_stream!(
            @_
            [ $iterator ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                {
                    assert_matches!(
                        $iterator .next(),
                        Some(eyeball_im::VectorDiff::PushFront { value }) => {
                            assert_matches!(
                                &**value,
                                matrix_sdk_ui::timeline::TimelineItemKind::Virtual(
                                    matrix_sdk_ui::timeline::VirtualTimelineItem::TimelineStart
                                ) => {}
                            );
                        }
                    );
                }
            ]
        )
    };

    // `prepend "$event_id"`
    ( @_ [ $iterator:ident ] [ prepend $event_id:literal ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_timeline_stream!(
            @_
            [ $iterator ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                {
                    assert_matches!(
                        $iterator .next(),
                        Some(eyeball_im::VectorDiff::PushFront { value }) => {
                            assert_matches!(
                                &**value,
                                matrix_sdk_ui::timeline::TimelineItemKind::Event(event_timeline_item) => {
                                    assert_eq!(event_timeline_item.event_id().unwrap().as_str(), $event_id);
                                }
                            );
                        }
                    );
                }
            ]
        )
    };

    // `insert [$nth] "$event_id"`
    ( @_ [ $iterator:ident ] [ insert [$index:literal] $event_id:literal ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_timeline_stream!(
            @_
            [ $iterator ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                {
                    assert_matches!(
                        $iterator .next(),
                        Some(eyeball_im::VectorDiff::Insert { index: $index, value }) => {
                            assert_matches!(
                                &**value,
                                matrix_sdk_ui::timeline::TimelineItemKind::Event(event_timeline_item) => {
                                    assert_eq!(event_timeline_item.event_id().unwrap().as_str(), $event_id);
                                }
                            );
                        }
                    );
                }
            ]
        )
    };

    // `update [$nth] "$event_id"`
    ( @_ [ $iterator:ident ] [ update [$index:literal] $event_id:literal ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_timeline_stream!(
            @_
            [ $iterator ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                {
                    assert_matches!(
                        $iterator .next(),
                        Some(eyeball_im::VectorDiff::Set { index: $index, value }) => {
                            assert_matches!(
                                &**value,
                                matrix_sdk_ui::timeline::TimelineItemKind::Event(event_timeline_item) => {
                                    assert_eq!(event_timeline_item.event_id().unwrap().as_str(), $event_id);
                                }
                            );
                        }
                    );
                }
            ]
        )
    };

    // `remove [$nth]`
    ( @_ [ $iterator:ident ] [ remove [$index:literal] ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_timeline_stream!(
            @_
            [ $iterator ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                {
                    assert_matches!(
                        $iterator .next(),
                        Some(eyeball_im::VectorDiff::Remove { index: $index })
                    );
                }
            ]
        )
    };

    // `clear`
    ( @_ [ $iterator:ident ] [ clear ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_timeline_stream!(
            @_
            [ $iterator ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                {
                    assert_matches!(
                        $iterator .next(),
                        Some(eyeball_im::VectorDiff::Clear)
                    );
                }
            ]
        )
    };

    ( @_ [ $iterator:ident ] [] [ $( $accumulator:tt )* ] ) => {
        $( $accumulator )*

        assert!($iterator .next().is_none(), concat!(stringify!($iterator), " has not been entirely read"));
    };

    ( [ $stream:ident ] $( $all:tt )* ) => {
        let mut timeline_updates = $stream
            .next()
            .await
            .expect("Failed to poll the stream")
            .into_iter();

        assert_timeline_stream!( @_ [ timeline_updates ] [ $( $all )* ] [] )
    };
}

pub(crate) use assert_timeline_stream;

async fn new_sliding_sync(
    lists: Vec<SlidingSyncListBuilder>,
) -> Result<(Client, MockServer, SlidingSync)> {
    let (client, server) = logged_in_client_with_server().await;

    let mut sliding_sync_builder = client.sliding_sync("integration-test")?;

    for list in lists {
        sliding_sync_builder = sliding_sync_builder.add_list(list);
    }

    let sliding_sync = sliding_sync_builder.build().await?;

    Ok((client, server, sliding_sync))
}

async fn create_one_room(
    server: &MockServer,
    sliding_sync: &SlidingSync,
    stream: &mut Pin<&mut impl Stream<Item = matrix_sdk::Result<UpdateSummary>>>,
    room_id: &RoomId,
    room_name: String,
) -> Result<()> {
    let update = receive_response!(
        [server, stream]
        {
            "pos": "foo",
            "lists": {},
            "rooms": {
                room_id: {
                    "name": room_name,
                    "initial": true,
                    "timeline": [],
                }
            },
            "extensions": {},
        }
    );

    assert!(update.rooms.contains(&room_id.to_owned()));

    let _room = sliding_sync.get_room(room_id).await.context("`get_room`")?;

    Ok(())
}

async fn timeline_test_helper(
    client: &Client,
    sliding_sync: &SlidingSync,
    room_id: &RoomId,
) -> Result<(Vector<Arc<TimelineItem>>, impl Stream<Item = Vec<VectorDiff<Arc<TimelineItem>>>>)> {
    let sliding_sync_room = sliding_sync.get_room(room_id).await.unwrap();

    let room_id = sliding_sync_room.room_id();
    let sdk_room = client.get_room(room_id).ok_or_else(|| {
        anyhow::anyhow!("Room {room_id} not found in client. Can't provide a timeline for it")
    })?;

    let timeline = Timeline::builder(&sdk_room).track_read_marker_and_receipts().build().await?;

    Ok(timeline.subscribe().await)
}

struct SlidingSyncMatcher;

impl Match for SlidingSyncMatcher {
    fn matches(&self, request: &Request) -> bool {
        request.url.path() == "/_matrix/client/unstable/org.matrix.simplified_msc3575/sync"
            && request.method == Method::POST
    }
}

#[async_test]
async fn test_timeline_basic() -> Result<()> {
    let (client, server, sliding_sync) = new_sliding_sync(vec![SlidingSyncList::builder("foo")
        .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10))])
    .await?;

    let stream = sliding_sync.sync();
    pin_mut!(stream);

    let room_id = room_id!("!foo:bar.org");

    create_one_room(&server, &sliding_sync, &mut stream, room_id, "Room Name".to_owned()).await?;

    mock_encryption_state(&server, false).await;

    let (timeline_items, mut timeline_stream) =
        timeline_test_helper(&client, &sliding_sync, room_id).await?;
    assert!(timeline_items.is_empty());

    // Receiving a bunch of events.
    {
        receive_response! {
            [server, stream]
            {
                "pos": "1",
                "lists": {},
                "rooms": {
                    room_id: {
                        "timeline": [
                            timeline_event!("$x1:bar.org" at 1 sec),
                            timeline_event!("$x2:bar.org" at 2 sec),
                        ]
                    }
                }
            }
        };

        assert_timeline_stream! {
            [timeline_stream]
            append    "$x1:bar.org";
            update[0] "$x1:bar.org";
            append    "$x2:bar.org";
            prepend   --- date divider ---;
        };
    }

    Ok(())
}

#[async_test]
async fn test_timeline_duplicated_events() -> Result<()> {
    let (client, server, sliding_sync) = new_sliding_sync(vec![SlidingSyncList::builder("foo")
        .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10))])
    .await?;

    let stream = sliding_sync.sync();
    pin_mut!(stream);

    let room_id = room_id!("!foo:bar.org");

    create_one_room(&server, &sliding_sync, &mut stream, room_id, "Room Name".to_owned()).await?;

    mock_encryption_state(&server, false).await;

    let (_, mut timeline_stream) = timeline_test_helper(&client, &sliding_sync, room_id).await?;

    // Receiving events.
    {
        receive_response! {
            [server, stream]
            {
                "pos": "1",
                "lists": {},
                "rooms": {
                    room_id: {
                        "timeline": [
                            timeline_event!("$x1:bar.org" at 1 sec),
                            timeline_event!("$x2:bar.org" at 2 sec),
                            timeline_event!("$x3:bar.org" at 3 sec),
                        ]
                    }
                }
            }
        };

        assert_timeline_stream! {
            [timeline_stream]
            append    "$x1:bar.org";
            update[0] "$x1:bar.org";
            append    "$x2:bar.org";
            update[1] "$x2:bar.org";
            append    "$x3:bar.org";
            prepend    --- date divider ---;
        };
    }

    // Receiving new events, where the first has already been received.
    {
        receive_response! {
            [server, stream]
            {
                "pos": "3",
                "lists": {},
                "rooms": {
                    room_id: {
                        "timeline": [
                            timeline_event!("$x1:bar.org" at 4 sec),
                            timeline_event!("$x4:bar.org" at 5 sec),
                        ]
                    }
                }
            }
        };

        assert_timeline_stream! {
            [timeline_stream]
            remove[1];
            update[2] "$x3:bar.org";
            append    "$x1:bar.org";
            update[3] "$x1:bar.org";
            append    "$x4:bar.org";
        };
    }

    Ok(())
}

#[async_test]
async fn test_timeline_read_receipts_are_updated_live() -> Result<()> {
    let (client, server, sliding_sync) = new_sliding_sync(vec![SlidingSyncList::builder("foo")
        .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10))])
    .await?;

    let stream = sliding_sync.sync();
    pin_mut!(stream);

    let room_id = room_id!("!foo:bar.org");

    create_one_room(&server, &sliding_sync, &mut stream, room_id, "Room Name".to_owned()).await?;

    mock_encryption_state(&server, false).await;

    let (timeline_items, mut timeline_stream) =
        timeline_test_helper(&client, &sliding_sync, room_id).await?;
    assert!(timeline_items.is_empty());

    // Receiving initial events.
    {
        receive_response! {
            [server, stream]
            {
                "pos": "1",
                "lists": {},
                "rooms": {
                    room_id: {
                        "timeline": [
                            timeline_event!("$x1:bar.org" at 1 sec),
                            timeline_event!("$x2:bar.org" at 2 sec),
                        ]
                    }
                }
            }
        };

        assert_timeline_stream! {
            [timeline_stream]
            append    "$x1:bar.org";
            update[0] "$x1:bar.org";
            append    "$x2:bar.org";
            prepend   --- date divider ---;
        };
    }

    // Now receiving a read receipt from another user in the room.
    {
        receive_response! {
            [server, stream]
            {
                "pos": "2",
                "lists": {},
                "rooms": {},
                "extensions":{
                    "receipts": {
                        "rooms": {
                            room_id: {
                                "room_id": "!foo:bar.org",
                                "type": "m.receipt",
                                "content": {
                                    "$x2:bar.org": {
                                        "m.read": {
                                            "@bob:bar.org": {
                                                "ts": 1436451550
                                            }
                                        }
                                    }
                                },
                            }
                        }
                    }
                }
            }
        };

        assert_let!(Some(timeline_updates) = timeline_stream.next().await);
        assert_eq!(timeline_updates.len(), 1);

        assert_let!(VectorDiff::Set { index: 2, value } = &timeline_updates[0]);

        assert_let!(TimelineItemKind::Event(event_timeline_item) = &***value);
        assert_eq!(event_timeline_item.event_id().unwrap().as_str(), "$x2:bar.org");

        let read_receipts = event_timeline_item.read_receipts();
        assert_eq!(read_receipts.len(), 2);
        // Implicit read receipt from Alice.
        assert!(read_receipts.get(user_id!("@alice:bar.org")).is_some());
        // Explicit read receipt from Bob.
        assert!(read_receipts.get(user_id!("@bob:bar.org")).is_some());
    }

    Ok(())
}
