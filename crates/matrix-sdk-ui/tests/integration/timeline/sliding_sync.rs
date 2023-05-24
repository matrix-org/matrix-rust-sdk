use std::{pin::Pin, sync::Arc};

use anyhow::{Context, Result};
use assert_matches::assert_matches;
use eyeball_im::{Vector, VectorDiff};
use futures_util::{pin_mut, Stream, StreamExt};
use matrix_sdk::{
    SlidingSync, SlidingSyncList, SlidingSyncListBuilder, SlidingSyncMode, UpdateSummary,
};
use matrix_sdk_test::async_test;
use matrix_sdk_ui::timeline::{SlidingSyncRoomExt, TimelineItem, VirtualTimelineItem};
use ruma::{room_id, RoomId};
use serde_json::json;
use wiremock::{http::Method, Match, Mock, MockServer, Request, ResponseTemplate};

use crate::logged_in_client;

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

            let next = $sliding_sync_stream.next().await.context("`stream` trip")??;

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

macro_rules! assert_timeline_stream {
    // `--- day divider ---`
    ( @_ [ $stream:ident ] [ --- day divider --- ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_timeline_stream!(
            @_
            [ $stream ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                {
                    assert_matches!(
                        $stream.next().await,
                        Some(VectorDiff::PushBack { value }) => {
                            assert_matches!(value.as_ref(), TimelineItem::Virtual(VirtualTimelineItem::DayDivider(_)));
                        }
                    );
                }
            ]
        )
    };

    // `append "$event_id"`
    ( @_ [ $stream:ident ] [ append $event_id:literal ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_timeline_stream!(
            @_
            [ $stream ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                {
                    assert_matches!(
                        $stream.next().await,
                        Some(VectorDiff::PushBack { value }) => {
                            assert_matches!(
                                value.as_ref(),
                                TimelineItem::Event(event_timeline_item) => {
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
    ( @_ [ $stream:ident ] [ update [$index:literal] $event_id:literal ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_timeline_stream!(
            @_
            [ $stream ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                {
                    assert_matches!(
                        $stream.next().await,
                        Some(VectorDiff::Set { index: $index, value }) => {
                            assert_matches!(
                                value.as_ref(),
                                TimelineItem::Event(event_timeline_item) => {
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
    ( @_ [ $stream:ident ] [ remove [$index:literal] ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_timeline_stream!(
            @_
            [ $stream ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                {
                    assert_matches!(
                        $stream.next().await,
                        Some(VectorDiff::Remove { index: $index })
                    );
                }
            ]
        )
    };

    ( @_ [ $stream:ident ] [] [ $( $accumulator:tt )* ] ) => {
        $( $accumulator )*
    };

    ( [ $stream:ident ] $( $all:tt )* ) => {
        assert_timeline_stream!( @_ [ $stream ] [ $( $all )* ] [] )
    };
}

async fn new_sliding_sync(lists: Vec<SlidingSyncListBuilder>) -> Result<(MockServer, SlidingSync)> {
    let (client, server) = logged_in_client().await;

    let mut sliding_sync_builder = client.sliding_sync();

    for list in lists {
        sliding_sync_builder = sliding_sync_builder.add_list(list);
    }

    let sliding_sync = sliding_sync_builder.build().await?;

    Ok((server, sliding_sync))
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

    let room = sliding_sync.get_room(room_id).context("`get_room`")?;
    assert_eq!(room.name(), Some(room_name.clone()));

    Ok(())
}

async fn timeline(
    sliding_sync: &SlidingSync,
    room_id: &RoomId,
) -> Result<(Vector<Arc<TimelineItem>>, impl Stream<Item = VectorDiff<Arc<TimelineItem>>>)> {
    Ok(sliding_sync
        .get_room(room_id)
        .unwrap()
        .timeline()
        .await
        .context("`timeline`")?
        .subscribe()
        .await)
}

struct SlidingSyncMatcher;

impl Match for SlidingSyncMatcher {
    fn matches(&self, request: &Request) -> bool {
        request.url.path() == "/_matrix/client/unstable/org.matrix.msc3575/sync"
            && request.method == Method::Post
    }
}

#[async_test]
async fn test_timeline_basic() -> Result<()> {
    let (server, sliding_sync) = new_sliding_sync(vec![SlidingSyncList::builder("foo")
        .sync_mode(SlidingSyncMode::Selective)
        .set_range(0..=10)])
    .await?;

    let stream = sliding_sync.stream();
    pin_mut!(stream);

    let room_id = room_id!("!foo:bar.org");

    create_one_room(&server, &sliding_sync, &mut stream, room_id, "Room Name".to_owned()).await?;

    let (timeline_items, mut timeline_stream) = timeline(&sliding_sync, room_id).await?;
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
            --- day divider ---;
            append    "$x1:bar.org";
            update[1] "$x1:bar.org";
            append    "$x2:bar.org";
        };
    }

    Ok(())
}

#[async_test]
async fn test_timeline_duplicated_events() -> Result<()> {
    let (server, sliding_sync) = new_sliding_sync(vec![SlidingSyncList::builder("foo")
        .sync_mode(SlidingSyncMode::Selective)
        .set_range(0..=10)])
    .await?;

    let stream = sliding_sync.stream();
    pin_mut!(stream);

    let room_id = room_id!("!foo:bar.org");

    create_one_room(&server, &sliding_sync, &mut stream, room_id, "Room Name".to_owned()).await?;

    let (_, mut timeline_stream) = timeline(&sliding_sync, room_id).await?;

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
            --- day divider ---;
            append    "$x1:bar.org";
            update[1] "$x1:bar.org";
            append    "$x2:bar.org";
            update[2] "$x2:bar.org";
            append    "$x3:bar.org";
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
