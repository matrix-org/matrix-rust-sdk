use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use futures_util::{pin_mut, FutureExt, StreamExt};
use imbl::vector;
use matrix_sdk_test::async_test;
use matrix_sdk_ui::{
    room_list::{
        Error, Input, RoomListEntry, State, ALL_ROOMS_LIST_NAME as ALL_ROOMS,
        VISIBLE_ROOMS_LIST_NAME as VISIBLE_ROOMS,
    },
    timeline::{TimelineItem, VirtualTimelineItem},
    RoomList,
};
use ruma::{event_id, room_id};
use serde_json::json;
use wiremock::{http::Method, Match, Mock, MockServer, Request, ResponseTemplate};

use crate::{
    logged_in_client,
    timeline::sliding_sync::{assert_timeline_stream, timeline_event},
};

async fn new_room_list() -> Result<(MockServer, RoomList), Error> {
    let (client, server) = logged_in_client().await;
    let room_list = RoomList::new(client).await?;

    Ok((server, room_list))
}

#[derive(Copy, Clone)]
struct SlidingSyncMatcher;

impl Match for SlidingSyncMatcher {
    fn matches(&self, request: &Request) -> bool {
        request.url.path() == "/_matrix/client/unstable/org.matrix.msc3575/sync"
            && request.method == Method::Post
    }
}

macro_rules! sync_then_assert_request_and_fake_response {
    (
        [$server:ident, $room_list:ident, $room_list_sync_stream:ident]
        $( states = $pre_state:pat => $post_state:pat, )?
        assert request = { $( $request_json:tt )* },
        respond with = $( ( code $code:expr ) )? { $( $response_json:tt )* }
        $(,)?
    ) => {
        sync_then_assert_request_and_fake_response! {
            [$server, $room_list, $room_list_sync_stream]
            sync matches Some(Ok(_)),
            $( states = $pre_state => $post_state, )?
            assert request = { $( $request_json )* },
            respond with = $( ( code $code ) )? { $( $response_json )* },
        }
    };

    (
        [$server:ident, $room_list:ident, $room_list_sync_stream:ident]
        sync matches $sync_result:pat,
        $( states = $pre_state:pat => $post_state:pat, )?
        assert request = { $( $request_json:tt )* },
        respond with = $( ( code $code:expr ) )? { $( $response_json:tt )* }
        $(,)?
    ) => {
        {
            let _code = 200;
            $( let _code = $code; )?

            let _mock_guard = Mock::given(SlidingSyncMatcher)
                .respond_with(ResponseTemplate::new(_code).set_body_json(
                    json!({ $( $response_json )* })
                ))
                .mount_as_scoped(&$server)
                .await;

            $(
                use State::*;

                assert_matches!($room_list.state().get(), $pre_state, "pre state");
            )?

            let next = $room_list_sync_stream.next().await;

            assert_matches!(next, $sync_result, "sync's result");

            for request in $server.received_requests().await.expect("Request recording has been disabled").iter().rev() {
                if SlidingSyncMatcher.matches(request) {
                    let json_value = serde_json::from_slice::<serde_json::Value>(&request.body).unwrap();

                    if let Err(error) = assert_json_diff::assert_json_matches_no_panic(
                        &json_value,
                        &json!({ $( $request_json )* }),
                        assert_json_diff::Config::new(assert_json_diff::CompareMode::Inclusive),
                    ) {
                        dbg!(json_value);
                        panic!("{}", error);
                    }

                    break;
                }
            }

            $( assert_matches!($room_list.state().get(), $post_state, "post state"); )?

            next
        }
    };
}

macro_rules! entries {
    ( @_ [ E $( , $( $rest:tt )* )? ] [ $( $accumulator:tt )* ] ) => {
        entries!( @_ [ $( $( $rest )* )? ] [ $( $accumulator )* RoomListEntry::Empty, ] )
    };

    ( @_ [ F( $room_id:literal ) $( , $( $rest:tt )* )? ] [ $( $accumulator:tt )* ] ) => {
        entries!( @_ [ $( $( $rest )* )? ] [ $( $accumulator )* RoomListEntry::Filled(room_id!( $room_id ).to_owned()), ] )
    };

    ( @_ [ I( $room_id:literal ) $( , $( $rest:tt )* )? ] [ $( $accumulator:tt )* ] ) => {
        entries!( @_ [ $( $( $rest )* )? ] [ $( $accumulator )* RoomListEntry::Invalidated(room_id!( $room_id ).to_owned()), ] )
    };

    ( @_ [] [ $( $accumulator:tt )* ] ) => {
        vector![ $( $accumulator )* ]
    };

    ( $( $all:tt )* ) => {
        entries!( @_ [ $( $all )* ] [] )
    };
}

macro_rules! assert_entries_stream {
    // `append [$entries]`
    ( @_ [ $stream:ident ] [ append [ $( $entries:tt )+ ] ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_entries_stream!(
            @_
            [ $stream ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                assert_matches!(
                    $stream.next().now_or_never(),
                    Some(Some(VectorDiff::Append { values })) => {
                        assert_eq!(values, entries!( $( $entries )+ ));
                    }
                );
            ]
        )
    };

    // `set [$nth] [$entry]`
    ( @_ [ $stream:ident ] [ set [ $index:literal ] [ $( $entry:tt )+ ] ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_entries_stream!(
            @_
            [ $stream ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                assert_matches!(
                    $stream.next().now_or_never(),
                    Some(Some(VectorDiff::Set { index: $index, value })) => {
                        assert_eq!(value, entries!( $( $entry )+ )[0]);
                    }
                );
            ]
        )
    };

    // `remove [$nth]`
    ( @_ [ $stream:ident ] [ remove [ $index:literal ] ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_entries_stream!(
            @_
            [ $stream ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                assert_eq!(
                    $stream.next().now_or_never(),
                    Some(Some(VectorDiff::Remove { index: $index })),
                );
            ]
        )
    };

    // `insert [$nth] [ $entry ]`
    ( @_ [ $stream:ident ] [ insert [ $index:literal ] [ $( $entry:tt )+ ] ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_entries_stream!(
            @_
            [ $stream ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                assert_matches!(
                    $stream.next().now_or_never(),
                    Some(Some(VectorDiff::Insert { index: $index, value })) => {
                        assert_eq!(value, entries!( $( $entry )+ )[0]);
                    }
                );
            ]
        )
    };

    // `pending`
    ( @_ [ $stream:ident ] [ pending ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_entries_stream!(
            @_
            [ $stream ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                assert_eq!($stream.next().now_or_never(), None);
            ]
        )
    };

    ( @_ [ $stream:ident ] [] [ $( $accumulator:tt )* ] ) => {
        $( $accumulator )*
    };

    ( [ $stream:ident ] $( $all:tt )* ) => {
        assert_entries_stream!( @_ [ $stream ] [ $( $all )* ] [] )
    };
}

#[async_test]
async fn test_sync_from_init_to_enjoy() -> Result<(), Error> {
    let (server, room_list) = new_room_list().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Init => FirstRooms,
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [
                        [0, 19],
                    ],
                    "required_state": [
                        ["m.room.avatar", ""],
                        ["m.room.encryption", ""],
                    ],
                    "sort": ["by_recency", "by_name"],
                    "timeline_limit": 1,
                },
            },
            "extensions": {
                "e2ee": {
                    "enabled": true,
                },
                "to_device": {
                    "enabled": true,
                },
                "account_data": {
                    "enabled": true
                }
            },
        },
        respond with = {
            "pos": "0",
            "lists": {
                ALL_ROOMS: {
                    "count": 200,
                    "ops": [
                        // let's ignore them for now
                    ],
                },
            },
            "rooms": {
                // let's ignore them for now
            },
            "extensions": {},
        },
    };

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = FirstRooms => AllRooms,
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [
                        [0, 49],
                    ],
                    "timeline_limit": 1,
                },
                VISIBLE_ROOMS: {
                    "ranges": [],
                    "required_state": [
                        ["m.room.encryption", ""],
                    ],
                    "sort": ["by_recency", "by_name"],
                    "timeline_limit": 20,
                }
            }
        },
        respond with = {
            "pos": "1",
            "lists": {
                ALL_ROOMS: {
                    "count": 200,
                    "ops": [
                        // let's ignore them for now
                    ],
                },
                VISIBLE_ROOMS: {
                    "count": 0,
                    "ops": [],
                },
            },
            "rooms": {
                // let's ignore them for now
            },
        },
    };

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = AllRooms => CarryOn,
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 99]],
                },
                VISIBLE_ROOMS: {
                    "ranges": [],
                },
            },
        },
        respond with = {
            "pos": "2",
            "lists": {
                ALL_ROOMS: {
                    "count": 200,
                    "ops": [
                        // let's ignore them for now
                    ],
                },
                VISIBLE_ROOMS: {
                    "count": 0,
                    "ops": [],
                },
            },
            "rooms": {
                // let's ignore them for now
            },
        },
    };

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = CarryOn => CarryOn,
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 149]],
                },
                VISIBLE_ROOMS: {
                    "ranges": [],
                },
            },
        },
        respond with = {
            "pos": "2",
            "lists": {
                ALL_ROOMS: {
                    "count": 200,
                    "ops": [
                        // let's ignore them for now
                    ],
                },
                VISIBLE_ROOMS: {
                    "count": 0,
                    "ops": [],
                },
            },
            "rooms": {
                // let's ignore them for now
            },
        },
    };

    Ok(())
}
#[async_test]

async fn test_sync_resumes_from_previous_state() -> Result<(), Error> {
    let (server, room_list) = new_room_list().await?;

    // Start a sync, and drop it at the end of the block.
    {
        let sync = room_list.sync();
        pin_mut!(sync);

        sync_then_assert_request_and_fake_response! {
            [server, room_list, sync]
            states = Init => FirstRooms,
            assert request = {
                "lists": {
                    ALL_ROOMS: {
                        "ranges": [[0, 19]],
                    },
                },
            },
            respond with = {
                "pos": "0",
                "lists": {
                    ALL_ROOMS: {
                        "count": 10,
                        "ops": []
                    },
                },
                "rooms": {},
            },
        };
    }

    // Start a sync, and drop it at the end of the block.
    {
        let sync = room_list.sync();
        pin_mut!(sync);

        sync_then_assert_request_and_fake_response! {
            [server, room_list, sync]
            states = FirstRooms => AllRooms,
            assert request = {
                "lists": {
                    ALL_ROOMS: {
                        "ranges": [[0, 9]],
                    },
                    VISIBLE_ROOMS: {
                        "ranges": [],
                    },
                },
            },
            respond with = {
                "pos": "1",
                "lists": {
                    ALL_ROOMS: {
                        "count": 10,
                        "ops": [],
                    },
                    VISIBLE_ROOMS: {
                        "count": 0,
                        "ops": [],
                    },
                },
                "rooms": {},
            },
        };
    }

    // Start a sync, and drop it at the end of the block.
    {
        let sync = room_list.sync();
        pin_mut!(sync);

        sync_then_assert_request_and_fake_response! {
            [server, room_list, sync]
            states = AllRooms => CarryOn,
            assert request = {
                "lists": {
                    ALL_ROOMS: {
                        "ranges": [[0, 9]],
                    },
                    VISIBLE_ROOMS: {
                        "ranges": [],
                    },
                },
            },
            respond with = {
                "pos": "2",
                "lists": {
                    ALL_ROOMS: {
                        "count": 10,
                        "ops": [],
                    },
                    VISIBLE_ROOMS: {
                        "count": 0,
                        "ops": [],
                    },
                },
                "rooms": {},
            },
        };
    }

    Ok(())
}

#[async_test]
async fn test_sync_resumes_from_terminated() -> Result<(), Error> {
    let (server, room_list) = new_room_list().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    // Simulate an error from the `Init` state.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        sync matches Some(Err(_)),
        states = Init => Terminated { .. },
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    // The default range, in selective sync-mode.
                    "ranges": [[0, 19]],
                },
            },
        },
        respond with = (code 400) {
            "error": "foo",
            "errcode": "M_UNKNOWN",
        },
    };

    // Ensure sync is terminated.
    assert!(sync.next().await.is_none());

    // Start a new sync.
    let sync = room_list.sync();
    pin_mut!(sync);

    // Do a regular sync from the `Terminated` state.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Terminated { .. } => FirstRooms,
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    // Still the default range, in selective sync-mode.
                    "ranges": [[0, 19]],
                },
            },
        },
        respond with = {
            "pos": "1",
            "lists": {
                ALL_ROOMS: {
                    "count": 110,
                },
            },
            "rooms": {},
        },
    };

    // Simulate an error from the `FirstRooms` state.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        sync matches Some(Err(_)),
        states = FirstRooms => Terminated { .. },
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    // In `FirstRooms`, the sync-mode has changed to growing, with
                    // its initial range.
                    "ranges": [[0, 49]],
                },
                VISIBLE_ROOMS: {
                    // Hello new list.
                    "ranges": [],
                },
            },
        },
        respond with = (code 400) {
            "error": "foo",
            "errcode": "M_UNKNOWN",
        },
    };

    // Ensure sync is terminated.
    assert!(sync.next().await.is_none());

    // Start a new sync.
    let sync = room_list.sync();
    pin_mut!(sync);

    // Update the viewport, just to be sure it's not reset later.
    room_list.apply_input(Input::Viewport(vec![5..=10])).await?;

    // Do a regular sync from the `Terminated` state.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Terminated { .. } => AllRooms,
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    // In `AllRooms`, the sync-mode is still growing, but the range
                    // hasn't been modified due to previous error.
                    "ranges": [[0, 49]],
                },
                VISIBLE_ROOMS: {
                    // We have set a viewport, which reflects here.
                    "ranges": [[5, 10]],
                },
            },
        },
        respond with = {
            "pos": "2",
            "lists": {
                ALL_ROOMS: {
                    "count": 110,
                },
            },
            "rooms": {},
        },
    };

    // Simulate an error from the `AllRooms` state.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        sync matches Some(Err(_)),
        states = AllRooms => Terminated { .. },
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    // In `AllRooms`, the sync-mode is still growing, and the range
                    // has made progress.
                    "ranges": [[0, 99]],
                },
                VISIBLE_ROOMS: {
                    // Despites the error, the range is kept.
                    "ranges": [[5, 10]],
                },
            },
        },
        respond with = (code 400) {
            "error": "foo",
            "errcode": "M_UNKNOWN",
        },
    };

    // Ensure sync is terminated.
    assert!(sync.next().await.is_none());

    // Start a new sync.
    let sync = room_list.sync();
    pin_mut!(sync);

    // Do a regular sync from the `Terminated` state.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Terminated { .. } => CarryOn,
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    // Due to the error, the range is reset to its initial value.
                    "ranges": [[0, 49]],
                },
                VISIBLE_ROOMS: {
                    // Despites the error, the range is kept.
                    "ranges": [[5, 10]],
                },
            },
        },
        respond with = {
            "pos": "3",
            "lists": {
                ALL_ROOMS: {
                    "count": 110,
                },
            },
            "rooms": {},
        },
    };

    // Do a regular sync from the `CarryOn` state to update the `ALL_ROOMS` list
    // again.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = CarryOn => CarryOn,
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    // No error. The range is making progress.
                    "ranges": [[0, 99]],
                },
                VISIBLE_ROOMS: {
                    // No error. The range is still here.
                    "ranges": [[5, 10]],
                },
            },
        },
        respond with = {
            "pos": "4",
            "lists": {
                ALL_ROOMS: {
                    "count": 110,
                },
            },
            "rooms": {},
        },
    };

    // Simulate an error from the `CarryOn` state.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        sync matches Some(Err(_)),
        states = CarryOn => Terminated { .. },
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    // Range is making progress and is even reaching the maximum
                    // number of rooms.
                    "ranges": [[0, 109]],
                },
                VISIBLE_ROOMS: {
                    // The range is still here.
                    "ranges": [[5, 10]],
                },
            },
        },
        respond with = (code 400) {
            "error": "foo",
            "errcode": "M_UNKNOWN",
        },
    };

    // Ensure sync is terminated.
    assert!(sync.next().await.is_none());

    // Start a new sync.
    let sync = room_list.sync();
    pin_mut!(sync);

    // Do a regular sync from the `Terminated` state.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Terminated { .. } => CarryOn,
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    // An error was received at the previous sync iteration.
                    // The list is still in growing sync-mode, but its range has
                    // been reset.
                    "ranges": [[0, 49]],
                },
                VISIBLE_ROOMS: {
                    // The range is still here.
                    "ranges": [[5, 10]],
                },
            },
        },
        respond with = {
            "pos": "5",
            "lists": {
                ALL_ROOMS: {
                    "count": 110,
                },
            },
            "rooms": {},
        },
    };

    Ok(())
}

#[async_test]
async fn test_entries_stream() -> Result<(), Error> {
    let (server, room_list) = new_room_list().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    let (previous_entries, entries_stream) = room_list.entries().await?;
    pin_mut!(entries_stream);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Init => FirstRooms,
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 19]],
                },
            },
        },
        respond with = {
            "pos": "0",
            "lists": {
                ALL_ROOMS: {
                    "count": 10,
                    "ops": [
                        {
                            "op": "SYNC",
                            "range": [0, 2],
                            "room_ids": [
                                "!r0:bar.org",
                                "!r1:bar.org",
                                "!r2:bar.org",
                            ],
                        },
                    ],
                },
            },
            "rooms": {
                "!r0:bar.org": {
                    "name": "Room #0",
                    "initial": true,
                    "timeline": [],
                },
                "!r1:bar.org": {
                    "name": "Room #1",
                    "initial": true,
                    "timeline": [],
                },
                "!r2:bar.org": {
                    "name": "Room #2",
                    "initial": true,
                    "timeline": [],
                }
            },
        },
    };

    assert!(previous_entries.is_empty());
    assert_entries_stream! {
        [entries_stream]
        append [ E, E, E, E, E, E, E, E, E, E ];
        set[0] [ F("!r0:bar.org") ];
        set[1] [ F("!r1:bar.org") ];
        set[2] [ F("!r2:bar.org") ];
        pending;
    }

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = FirstRooms => AllRooms,
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [
                        [0, 9],
                    ],
                },
                VISIBLE_ROOMS: {
                    "ranges": [],
                }
            }
        },
        respond with = {
            "pos": "1",
            "lists": {
                ALL_ROOMS: {
                    "count": 9,
                    "ops": [
                        {
                            "op": "DELETE",
                            "index": 1,
                        },
                        {
                            "op": "DELETE",
                            "index": 0,
                        },
                        {
                            "op": "INSERT",
                            "index": 0,
                            "room_id": "!r3:bar.org"
                        },
                    ],
                },
                VISIBLE_ROOMS: {
                    "count": 0,
                    "ops": [
                        // let's ignore them for now
                    ],
                },
            },
            "rooms": {
                "!r3:bar.org": {
                    "name": "Room #3",
                    "initial": true,
                    "timeline": [],
                },
            },
        },
    };

    assert_entries_stream! {
        [entries_stream]
        remove[1];
        remove[0];
        insert[0] [ F("!r3:bar.org") ];
        pending;
    }

    Ok(())
}

#[async_test]
async fn test_entries_stream_with_updated_filter() -> Result<(), Error> {
    let (server, room_list) = new_room_list().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    let (previous_entries, entries_stream) = room_list.entries().await?;
    pin_mut!(entries_stream);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Init => FirstRooms,
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 19]],
                },
            },
        },
        respond with = {
            "pos": "0",
            "lists": {
                ALL_ROOMS: {
                    "count": 10,
                    "ops": [
                        {
                            "op": "SYNC",
                            "range": [0, 0],
                            "room_ids": [
                                "!r0:bar.org",
                            ],
                        },
                    ],
                },
            },
            "rooms": {
                "!r0:bar.org": {
                    "name": "Room #0",
                    "initial": true,
                    "timeline": [],
                },
            },
        },
    };

    assert!(previous_entries.is_empty());
    assert_entries_stream! {
        [entries_stream]
        append [ E, E, E, E, E, E, E, E, E, E ];
        set[0] [ F("!r0:bar.org") ];
        pending;
    };

    let (previous_entries, entries_stream) = room_list
        .entries_filtered(|room_list_entry| {
            matches!(
                room_list_entry.as_room_id(),
                Some(room_id) if room_id.server_name() == "bar.org"
            )
        })
        .await?;
    pin_mut!(entries_stream);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = FirstRooms => AllRooms,
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [
                        [0, 9],
                    ],
                },
                VISIBLE_ROOMS: {
                    "ranges": [],
                }
            }
        },
        respond with = {
            "pos": "1",
            "lists": {
                ALL_ROOMS: {
                    "count": 10,
                    "ops": [
                        {
                            "op": "SYNC",
                            "range": [1, 4],
                            "room_ids": [
                                "!r1:bar.org",
                                "!r2:qux.org",
                                "!r3:qux.org",
                                "!r4:bar.org",
                            ],
                        },
                    ],
                },
                VISIBLE_ROOMS: {
                    "count": 0,
                    "ops": [
                        // let's ignore them for now
                    ],
                },
            },
            "rooms": {
                "!r1:bar.org": {
                    "name": "Room #1",
                    "initial": true,
                    "timeline": [],
                },
                "!r2:qux.org": {
                    "name": "Room #2",
                    "initial": true,
                    "timeline": [],
                },
                "!r3:qux.org": {
                    "name": "Room #3",
                    "initial": true,
                    "timeline": [],
                },
                "!r4:bar.org": {
                    "name": "Room #4",
                    "initial": true,
                    "timeline": [],
                },
            },
        },
    };

    assert_eq!(previous_entries, entries![F("!r0:bar.org")]);
    assert_entries_stream! {
        [entries_stream]
        insert[1] [ F("!r1:bar.org") ];
        insert[2] [ F("!r4:bar.org") ];
        pending;
    };

    Ok(())
}

#[async_test]
async fn test_room() -> Result<(), Error> {
    let (server, room_list) = new_room_list().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    let room_id_0 = room_id!("!r0:bar.org");
    let room_id_1 = room_id!("!r1:bar.org");

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        assert request = {},
        respond with = {
            "pos": "0",
            "lists": {
                ALL_ROOMS: {
                    "count": 2,
                    "ops": [
                        {
                            "op": "SYNC",
                            "range": [0, 1],
                            "room_ids": [
                                room_id_0,
                                room_id_1,
                            ],
                        },
                    ],
                },
            },
            "rooms": {
                room_id_0: {
                    "name": "Room #0",
                    "initial": true,
                },
                room_id_1: {
                    "initial": true,
                },
            },
        },
    };

    // Room has received a name from sliding sync.
    let room0 = room_list.room(room_id_0).await?;
    assert_eq!(room0.name().await, Some("Room #0".to_owned()));

    // Room has not received a name from sliding sync, then it's calculated.
    let room1 = room_list.room(room_id_1).await?;
    assert_eq!(room1.name().await, Some("Empty Room".to_owned()));

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        assert request = {},
        respond with = {
            "pos": "1",
            "lists": {
                ALL_ROOMS: {
                    "count": 2,
                    "ops": [
                        {
                            "op": "SYNC",
                            "range": [1, 1],
                            "room_ids": [
                                room_id_1,
                            ],
                        },
                    ],
                },
            },
            "rooms": {
                room_id_1: {
                    "name": "Room #1",
                },
            },
        },
    };

    // Room has _now_ received a name from sliding sync!
    assert_eq!(room1.name().await, Some("Room #1".to_owned()));

    Ok(())
}

#[async_test]
async fn test_room_not_found() -> Result<(), Error> {
    let (_server, room_list) = new_room_list().await?;

    let room_id = room_id!("!foo:bar.org");

    assert_matches!(
        room_list.room(room_id).await,
        Err(Error::RoomNotFound(error_room_id)) => {
            assert_eq!(error_room_id, room_id.to_owned());
        }
    );

    Ok(())
}

#[async_test]
async fn test_room_timeline() -> Result<(), Error> {
    let (server, room_list) = new_room_list().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    let room_id = room_id!("!r0:bar.org");

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        assert request = {},
        respond with = {
            "pos": "0",
            "lists": {
                ALL_ROOMS: {
                    "count": 2,
                    "ops": [
                        {
                            "op": "SYNC",
                            "range": [0, 0],
                            "room_ids": [room_id],
                        },
                    ],
                },
            },
            "rooms": {
                room_id: {
                    "name": "Room #0",
                    "initial": true,
                    "timeline": [
                        timeline_event!("$x0:bar.org" at 0 sec),
                    ],
                },
            },
        },
    };

    let room = room_list.room(room_id).await?;
    let timeline = room.timeline().await;

    let (previous_timeline_items, mut timeline_items_stream) = timeline.subscribe().await;

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        assert request = {},
        respond with = {
            "pos": "0",
            "lists": {},
            "rooms": {
                room_id: {
                    "timeline": [
                        timeline_event!("$x1:bar.org" at 1 sec),
                        timeline_event!("$x2:bar.org" at 2 sec),
                    ],
                },
            },
        },
    };

    // Previous timeline items.
    assert_matches!(
        previous_timeline_items[0].as_ref(),
        TimelineItem::Virtual(VirtualTimelineItem::DayDivider(_))
    );
    assert_matches!(
        previous_timeline_items[1].as_ref(),
        TimelineItem::Event(item) => {
            assert_eq!(item.event_id().unwrap().as_str(), "$x0:bar.org");
        }
    );

    // Timeline items stream.
    assert_timeline_stream! {
        [timeline_items_stream]
        update[1] "$x0:bar.org";
        append    "$x1:bar.org";
        update[2] "$x1:bar.org";
        append    "$x2:bar.org";
    };

    Ok(())
}

#[async_test]
async fn test_room_latest_event() -> Result<(), Error> {
    let (server, room_list) = new_room_list().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    let room_id = room_id!("!r0:bar.org");

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        assert request = {},
        respond with = {
            "pos": "0",
            "lists": {
                ALL_ROOMS: {
                    "count": 2,
                    "ops": [
                        {
                            "op": "SYNC",
                            "range": [0, 0],
                            "room_ids": [room_id],
                        },
                    ],
                },
            },
            "rooms": {
                room_id: {
                    "name": "Room #0",
                    "initial": true,
                },
            },
        },
    };

    let room = room_list.room(room_id).await?;

    // The latest event does not exist.
    assert!(room.latest_event().await.is_none());

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        assert request = {},
        respond with = {
            "pos": "0",
            "lists": {},
            "rooms": {
                room_id: {
                    "timeline": [
                        timeline_event!("$x0:bar.org" at 0 sec),
                    ],
                },
            },
        },
    };

    // The latest event exists.
    assert_matches!(
        room.latest_event().await,
        Some(event) => {
            assert_eq!(event.event_id(), Some(event_id!("$x0:bar.org")));
        }
    );

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        assert request = {},
        respond with = {
            "pos": "0",
            "lists": {},
            "rooms": {
                room_id: {
                    "timeline": [
                        timeline_event!("$x1:bar.org" at 1 sec),
                    ],
                },
            },
        },
    };

    // The latest event has been updated.
    assert_matches!(
        room.latest_event().await,
        Some(event) => {
            assert_eq!(event.event_id(), Some(event_id!("$x1:bar.org")));
        }
    );

    Ok(())
}

#[async_test]
async fn test_input_viewport() -> Result<(), Error> {
    let (server, room_list) = new_room_list().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    // The input cannot be applied because the `VISIBLE_ROOMS_LIST_NAME` list isn't
    // present.
    assert_matches!(
        room_list.apply_input(Input::Viewport(vec![10..=15])).await,
        Err(Error::InputHasNotBeenApplied(_))
    );

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Init => FirstRooms,
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 19]],
                },
            },
        },
        respond with = {
            "pos": "0",
            "lists": {},
            "rooms": {},
        },
    };

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = FirstRooms => AllRooms,
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 49]],
                },
                VISIBLE_ROOMS: {
                    "ranges": [],
                    "timeline_limit": 20,
                }
            }
        },
        respond with = {
            "pos": "1",
            "lists": {},
            "rooms": {},
        },
    };

    assert!(room_list.apply_input(Input::Viewport(vec![10..=15, 20..=25])).await.is_ok());

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = AllRooms => CarryOn,
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 49]],
                },
                VISIBLE_ROOMS: {
                    "ranges": [[10, 15], [20, 25]],
                    "timeline_limit": 20,
                }
            }
        },
        respond with = {
            "pos": "1",
            "lists": {},
            "rooms": {},
        },
    };

    Ok(())
}
