use std::{
    ops::Not,
    time::{Duration, Instant},
};

use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use futures_util::{pin_mut, FutureExt, StreamExt};
use matrix_sdk::{test_utils::logged_in_client_with_server, Client};
use matrix_sdk_base::{
    sliding_sync::http::request::RoomSubscription, sync::UnreadNotificationsCount,
};
use matrix_sdk_test::async_test;
use matrix_sdk_ui::{
    room_list_service::{
        filters::{new_filter_fuzzy_match_room_name, new_filter_non_left, new_filter_none},
        Error, RoomListLoadingState, State, SyncIndicator, ALL_ROOMS_LIST_NAME as ALL_ROOMS,
    },
    timeline::{TimelineItemKind, VirtualTimelineItem},
    RoomListService,
};
use ruma::{
    assign, event_id,
    events::{room::message::RoomMessageEventContent, StateEventType},
    mxc_uri, room_id, uint,
};
use serde_json::json;
use stream_assert::{assert_next_matches, assert_pending};
use tokio::{spawn, sync::mpsc::channel, task::yield_now};
use wiremock::MockServer;

use crate::{
    mock_encryption_state,
    timeline::sliding_sync::{assert_timeline_stream, timeline_event},
};

async fn new_room_list_service() -> Result<(Client, MockServer, RoomListService), Error> {
    let (client, server) = logged_in_client_with_server().await;
    let room_list = RoomListService::new(client.clone()).await?;

    Ok((client, server, room_list))
}

// Same macro as in the main, with additional checking that the state
// before/after the sync loop match those we expect.
macro_rules! sync_then_assert_request_and_fake_response {
    (
        [$server:ident, $room_list:ident, $stream:ident]
        $( states = $pre_state:pat => $post_state:pat, )?
        assert request $assert_request:tt { $( $request_json:tt )* },
        respond with = $( ( code $code:expr ) )? { $( $response_json:tt )* }
        $( , after delay = $response_delay:expr )?
        $(,)?
    ) => {
        sync_then_assert_request_and_fake_response! {
            [$server, $room_list, $stream]
            sync matches Some(Ok(_)),
            $( states = $pre_state => $post_state, )?
            assert request $assert_request { $( $request_json )* },
            respond with = $( ( code $code ) )? { $( $response_json )* },
            $( after delay = $response_delay, )?
        }
    };

    (
        [$server:ident, $room_list:ident, $stream:ident]
        sync matches $sync_result:pat,
        $( states = $pre_state:pat => $post_state:pat, )?
        assert request $assert_request:tt { $( $request_json:tt )* },
        respond with = $( ( code $code:expr ) )? { $( $response_json:tt )* }
        $( , after delay = $response_delay:expr )?
        $(,)?
    ) => {
        {
            $(
                use State::*;

                let mut state = $room_list.state();

                assert_matches!(state.get(), $pre_state, "pre state");
            )?

            let next = super::sliding_sync_then_assert_request_and_fake_response! {
                [$server, $stream]
                sync matches $sync_result,
                assert request $assert_request { $( $request_json )* },
                respond with = $( ( code $code ) )? { $( $response_json )* },
                $( after delay = $response_delay, )?
            };
            $( assert_matches!(state.next().now_or_never(), Some(Some($post_state)), "post state"); )?

            next
        }
    };
}

macro_rules! assert_entries_batch {
    // `append [$room_id, …]`
    ( @_ [ $entries:ident ] [ append [ $( $room_id:literal ),* $(,)? ] ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_entries_batch!(
            @_
            [ $entries ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                assert_matches!(
                    $entries.next(),
                    Some(&VectorDiff::Append { ref values }) => {
                        #[allow(unused)]
                        let mut values = values.iter();

                        $(
                            assert_eq!(
                                values
                                    .next()
                                    .expect("One more room is expected, but is not present")
                                    .room_id()
                                    .as_str(),
                                $room_id,
                            );
                        )*

                        assert!(values.next().is_none(), "`append` has more values to be asserted");
                    }
                );
            ]
        )
    };

    // `push front [$room_id]`
    ( @_ [ $entries:ident ] [ push front [ $room_id:literal ] ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_entries_batch!(
            @_
            [ $entries ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                assert_matches!(
                    $entries.next(),
                    Some(&VectorDiff::PushFront { ref value }) => {
                        assert_eq!(value.room_id().to_string(), $room_id);
                    }
                );
            ]
        )
    };

    // `push back [$room_id]`
    ( @_ [ $entries:ident ] [ push back [ $room_id:literal ] ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_entries_batch!(
            @_
            [ $entries ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                assert_matches!(
                    $entries.next(),
                    Some(&VectorDiff::PushBack { ref value }) => {
                        assert_eq!(value.room_id().to_string(), $room_id);
                    }
                );
            ]
        )
    };

    // `pop back [$room_id]`
    ( @_ [ $entries:ident ] [ pop back ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_entries_batch!(
            @_
            [ $entries ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                assert_matches!(
                    $entries.next(),
                    Some(&VectorDiff::PopBack)
                );
            ]
        )
    };

    // `set [$nth] [$room_id]`
    ( @_ [ $entries:ident ] [ set [ $index:literal ] [ $room_id:literal ] ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_entries_batch!(
            @_
            [ $entries ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                assert_matches!(
                    $entries.next(),
                    Some(&VectorDiff::Set { index: $index, ref value }) => {
                        assert_eq!(value.room_id().to_string(), $room_id);
                    }
                );
            ]
        )
    };

    // `remove [$nth]`
    ( @_ [ $entries:ident ] [ remove [ $index:literal ] ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_entries_batch!(
            @_
            [ $entries ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                assert_matches!(
                    $entries.next(),
                    Some(&VectorDiff::Remove { index: $index })
                );
            ]
        )
    };

    // `insert [$nth] [$room_id]`
    ( @_ [ $entries:ident ] [ insert [ $index:literal ] [ $room_id:literal ] ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_entries_batch!(
            @_
            [ $entries ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                assert_matches!(
                    $entries.next(),
                    Some(&VectorDiff::Insert { index: $index, ref value }) => {
                        assert_eq!(value.room_id().to_string(), $room_id);
                    }
                );
            ]
        )
    };

    // `truncate [$length]`
    ( @_ [ $entries:ident ] [ truncate [ $length:literal ] ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_entries_batch!(
            @_
            [ $entries ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                assert_matches!(
                    $entries.next(),
                    Some(&VectorDiff::Truncate { length: $length })
                );
            ]
        )
    };

    // `reset [$room_id, …]`
    ( @_ [ $entries:ident ] [ reset [ $( $room_id:literal ),* $(,)? ] ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_entries_batch!(
            @_
            [ $entries ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                assert_matches!(
                    $entries.next(),
                    Some(&VectorDiff::Reset { ref values }) => {
                        #[allow(unused)]
                        let mut values = values.iter();

                        $(
                            assert_eq!(
                                values
                                    .next()
                                    .expect("One more room is expected, but is not present")
                                    .room_id()
                                    .as_str(),
                                $room_id,
                            );
                        )*
                    }
                );
            ]
        )
    };

    // `end`
    ( @_ [ $entries:ident ] [ end ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_entries_batch!(
            @_
            [ $entries ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                assert!($entries.next().is_none());
            ]
        )
    };

    ( @_ [ $entries:ident ] [] [ $( $accumulator:tt )* ] ) => {
        $( $accumulator )*
    };

    ( [ $stream:ident ] $( $all:tt )* ) => {
        // Wait on Tokio to run all the tasks. Necessary only when testing.
        yield_now().await;

        let entries = $stream
            .next()
            .now_or_never()
            .expect("Stream entry wasn't in the ready state")
            .expect("Stream was stopped");

        let mut entries = entries.iter();

        assert_entries_batch!( @_ [ entries ] [ $( $all )* ] [] )
    };
}

#[async_test]
async fn test_sync_all_states() -> Result<(), Error> {
    let (_, server, room_list) = new_room_list_service().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Init => SettingUp,
        assert request = {
            "conn_id": "room-list",
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 19]],
                    "required_state": [
                        ["m.room.encryption", ""],
                        ["m.room.member", "$LAZY"],
                        ["m.room.member", "$ME"],
                        ["m.room.name", ""],
                        ["m.room.power_levels", ""],
                    ],
                    "include_heroes": true,
                    "filters": {
                        "not_room_types": ["m.space"],
                    },
                    "timeline_limit": 1,
                },
            },
            "extensions": {
                "account_data": {
                    "enabled": true
                },
                "receipts": {
                    "enabled": true,
                    "rooms": ["*"]
                },
                "typing": {
                    "enabled": true,
                },
            },
        },
        respond with = {
            "pos": "0",
            "lists": {
                ALL_ROOMS: {
                    "count": 420,
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
        states = SettingUp => Running,
        assert request = {
            "conn_id": "room-list",
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 99]],
                },
            },
        },
        respond with = {
            "pos": "1",
            "lists": {
                ALL_ROOMS: {
                    "count": 420,
                },
            },
            "rooms": {
                // let's ignore them for now
            },
        },
    };

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Running => Running,
        assert request = {
            "conn_id": "room-list",
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 199]],
                },
            },
        },
        respond with = {
            "pos": "2",
            "lists": {
                ALL_ROOMS: {
                    "count": 420,
                },
            },
            "rooms": {
                // let's ignore them for now
            },
        },
    };

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Running => Running,
        assert request = {
            "conn_id": "room-list",
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 299]],
                },
            },
        },
        respond with = {
            "pos": "3",
            "lists": {
                ALL_ROOMS: {
                    "count": 420,
                },
            },
            "rooms": {
                // let's ignore them for now
            },
        },
    };

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Running => Running,
        assert request = {
            "conn_id": "room-list",
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 399]],
                },
            },
        },
        respond with = {
            "pos": "4",
            "lists": {
                ALL_ROOMS: {
                    "count": 420,
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
    let (_, server, room_list) = new_room_list_service().await?;

    // Start a sync, and drop it at the end of the block.
    {
        let sync = room_list.sync();
        pin_mut!(sync);

        sync_then_assert_request_and_fake_response! {
            [server, room_list, sync]
            states = Init => SettingUp,
            assert request >= {
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
            states = SettingUp => Running,
            assert request >= {
                "lists": {
                    ALL_ROOMS: {
                        "ranges": [[0, 9]],
                    },
                },
            },
            respond with = {
                "pos": "1",
                "lists": {
                    ALL_ROOMS: {
                        "count": 10,
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
            states = Running => Running,
            assert request >= {
                "lists": {
                    ALL_ROOMS: {
                        "ranges": [[0, 9]],
                    },
                },
            },
            respond with = {
                "pos": "2",
                "lists": {
                    ALL_ROOMS: {
                        "count": 10,
                    },
                },
                "rooms": {},
            },
        };
    }

    Ok(())
}

#[async_test]
async fn test_sync_resumes_from_error() -> Result<(), Error> {
    let (_, server, room_list) = new_room_list_service().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        sync matches Some(Err(_)),
        states = Init => Error { .. },
        assert request >= {
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

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Error { .. } => SettingUp,
        assert request >= {
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
                    "count": 210,
                },
            },
            "rooms": {},
        },
    };

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        sync matches Some(Err(_)),
        states = SettingUp => Error { .. },
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // The sync-mode has changed to growing, with its initial range.
                    "ranges": [[0, 99]],
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

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Error { .. } => Recovering,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // Due to previous error, the sync-mode is back to selective, with its initial range.
                    "ranges": [[0, 19]],
                },
            },
        },
        respond with = {
            "pos": "2",
            "lists": {
                ALL_ROOMS: {
                    "count": 210,
                },
            },
            "rooms": {},
        },
    };

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Recovering => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // Sync-mode is now growing.
                    "ranges": [[0, 99]],
                },
            },
        },
        respond with = {
            "pos": "3",
            "lists": {
                ALL_ROOMS: {
                    "count": 210,
                },
            },
            "rooms": {},
        },
    };

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        sync matches Some(Err(_)),
        states = Running => Error { .. },
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // The sync-mode is still growing, and the range has made progress.
                    "ranges": [[0, 199]],
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

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Error { .. } => Recovering,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // Due to previous error, the sync-mode is back to selective, with its initial range.
                    "ranges": [[0, 19]],
                },
            },
        },
        respond with = {
            "pos": "4",
            "lists": {
                ALL_ROOMS: {
                    "count": 210,
                },
            },
            "rooms": {},
        },
    };

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Recovering => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // The sync-mode is now growing.
                    "ranges": [[0, 99]],
                },
            },
        },
        respond with = {
            "pos": "5",
            "lists": {
                ALL_ROOMS: {
                    "count": 210,
                },
            },
            "rooms": {},
        },
    };

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Running => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // No error. The range is making progress.
                    "ranges": [[0, 199]],
                },
            },
        },
        respond with = {
            "pos": "6",
            "lists": {
                ALL_ROOMS: {
                    "count": 210,
                },
            },
            "rooms": {},
        },
    };

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        sync matches Some(Err(_)),
        states = Running => Error { .. },
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // Range is making progress and is even reaching the maximum
                    // number of rooms.
                    "ranges": [[0, 209]],
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

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Error { .. } => Recovering,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // Due to previous error, the sync-mode is back to selective, with its initial range.
                    "ranges": [[0, 19]],
                },
            },
        },
        respond with = {
            "pos": "7",
            "lists": {
                ALL_ROOMS: {
                    "count": 210,
                },
            },
            "rooms": {},
        },
    };

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Recovering => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // Sync-mode is now growing.
                    "ranges": [[0, 99]],
                },
            },
        },
        respond with = {
            "pos": "8",
            "lists": {
                ALL_ROOMS: {
                    "count": 210,
                },
            },
            "rooms": {},
        },
    };

    // etc.

    Ok(())
}

#[async_test]
async fn test_sync_resumes_from_terminated() -> Result<(), Error> {
    let (_, server, room_list) = new_room_list_service().await?;

    // Let's stop the sync before actually syncing (we never know!).
    // We get an error, obviously.
    assert!(room_list.stop_sync().is_err());

    let sync = room_list.sync();
    pin_mut!(sync);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Init => SettingUp,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // The default range, in selective sync-mode.
                    "ranges": [[0, 19]],
                },
            },
        },
        respond with = {
            "pos": "0",
            "lists": {
                ALL_ROOMS: {
                    "count": 150,
                },
            },
            "rooms": {},
        },
    };

    // Stop the sync.
    room_list.stop_sync()?;
    assert!(sync.next().await.is_none());

    // Start a new sync.
    let sync = room_list.sync();
    pin_mut!(sync);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Terminated { .. } => Recovering,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // The sync-mode is still selective, with its initial range.
                    "ranges": [[0, 19]],
                },
            },
        },
        respond with = {
            "pos": "1",
            "lists": {
                ALL_ROOMS: {
                    "count": 150,
                },
            },
            "rooms": {},
        },
    };

    // Do a regular sync from the `Recovering` state.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Recovering => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // The sync-mode is now growing, with its initial range.
                    "ranges": [[0, 99]],
                },
            },
        },
        respond with = {
            "pos": "2",
            "lists": {
                ALL_ROOMS: {
                    "count": 150,
                },
            },
            "rooms": {},
        },
    };

    // Stop the sync.
    room_list.stop_sync()?;
    assert!(sync.next().await.is_none());

    // Start a new sync.
    let sync = room_list.sync();
    pin_mut!(sync);

    // Do a regular sync from the `Terminated` state.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Terminated { .. } => Recovering,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // The sync-mode is back to selective.
                    "ranges": [[0, 19]],
                },
            },
        },
        respond with = {
            "pos": "3",
            "lists": {
                ALL_ROOMS: {
                    "count": 150,
                },
            },
            "rooms": {},
        },
    };

    // Do a regular sync from the `Recovering` state.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Recovering => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // Sync-mode is growing, with its initial range.
                    "ranges": [[0, 99]],
                },
            },
        },
        respond with = {
            "pos": "4",
            "lists": {
                ALL_ROOMS: {
                    "count": 150,
                },
            },
            "rooms": {},
        },
    };

    // Do a regular sync from the `Running` state.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Running => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // Range is making progress, and has reached its maximum.
                    "ranges": [[0, 149]],
                },
            },
        },
        respond with = {
            "pos": "5",
            "lists": {
                ALL_ROOMS: {
                    "count": 150,
                },
            },
            "rooms": {},
        },
    };

    Ok(())
}

#[async_test]
async fn test_loading_states() -> Result<(), Error> {
    // Test with an empty client, so no cache.
    let (client, server) = {
        let (client, server, room_list) = new_room_list_service().await?;

        let sync = room_list.sync();
        pin_mut!(sync);

        let all_rooms = room_list.all_rooms().await?;
        let mut all_rooms_loading_state = all_rooms.loading_state();

        // The loading is not loaded.
        assert_matches!(all_rooms_loading_state.get(), RoomListLoadingState::NotLoaded);
        assert_pending!(all_rooms_loading_state);

        sync_then_assert_request_and_fake_response! {
            [server, room_list, sync]
            states = Init => SettingUp,
            assert request >= {
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
                    },
                },
                "rooms": {},
            },
        };

        // Wait on Tokio to run all the tasks. Necessary only when testing.
        yield_now().await;

        // There is a loading state update, it's loaded now!
        assert_next_matches!(
            all_rooms_loading_state,
            RoomListLoadingState::Loaded { maximum_number_of_rooms: Some(10) }
        );

        sync_then_assert_request_and_fake_response! {
            [server, room_list, sync]
            states = SettingUp => Running,
            assert request >= {
                "lists": {
                    ALL_ROOMS: {
                        "ranges": [[0, 9]],
                    },
                },
            },
            respond with = {
                "pos": "1",
                "lists": {
                    ALL_ROOMS: {
                        "count": 12, // 2 more rooms
                    },
                },
                "rooms": {},
            },
        };

        // Wait on Tokio to run all the tasks. Necessary only when testing.
        yield_now().await;

        // There is a loading state update because the number of rooms has been updated.
        assert_next_matches!(
            all_rooms_loading_state,
            RoomListLoadingState::Loaded { maximum_number_of_rooms: Some(12) }
        );

        sync_then_assert_request_and_fake_response! {
            [server, room_list, sync]
            states = Running => Running,
            assert request >= {
                "lists": {
                    ALL_ROOMS: {
                        "ranges": [[0, 11]],
                    },
                },
            },
            respond with = {
                "pos": "2",
                "lists": {
                    ALL_ROOMS: {
                        "count": 12, // no more rooms
                    },
                },
                "rooms": {},
            },
        };

        // Wait on Tokio to run all the tasks. Necessary only when testing.
        yield_now().await;

        // No loading state update.
        assert_pending!(all_rooms_loading_state);

        (client, server)
    };

    // Now, let's try with a cache!
    {
        let room_list = RoomListService::new(client).await?;

        let all_rooms = room_list.all_rooms().await?;
        let mut all_rooms_loading_state = all_rooms.loading_state();

        let sync = room_list.sync();
        pin_mut!(sync);

        // The loading state is loaded! Indeed, there is data loaded from the cache.
        assert_matches!(
            all_rooms_loading_state.get(),
            RoomListLoadingState::Loaded { maximum_number_of_rooms: Some(12) }
        );
        assert_pending!(all_rooms_loading_state);

        sync_then_assert_request_and_fake_response! {
            [server, room_list, sync]
            states = Init => SettingUp,
            assert request >= {
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
                        "count": 13, // 1 more room
                    },
                },
                "rooms": {},
            },
        };

        // Wait on Tokio to run all the tasks. Necessary only when testing.
        yield_now().await;

        // The loading state has been updated.
        assert_next_matches!(
            all_rooms_loading_state,
            RoomListLoadingState::Loaded { maximum_number_of_rooms: Some(13) }
        );
    }

    Ok(())
}

#[async_test]
async fn test_entries_stream() -> Result<(), Error> {
    let (_, server, room_list) = new_room_list_service().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    let all_rooms = room_list.all_rooms().await?;

    let (previous_entries, entries_stream) = all_rooms.entries();
    pin_mut!(entries_stream);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Init => SettingUp,
        assert request >= {
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
                },
            },
            "rooms": {
                "!r0:bar.org": {
                    "initial": true,
                    "timeline": [],
                },
                "!r1:bar.org": {
                    "initial": true,
                    "timeline": [],
                },
                "!r2:bar.org": {
                    "initial": true,
                    "timeline": [],
                },
            },
        },
    };

    assert!(previous_entries.is_empty());
    assert_entries_batch! {
        [entries_stream]
        push back [ "!r0:bar.org" ];
        push back [ "!r1:bar.org" ];
        push back [ "!r2:bar.org" ];
        end;
    };

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = SettingUp => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 9]],
                },
            },
        },
        respond with = {
            "pos": "1",
            "lists": {
                ALL_ROOMS: {
                    "count": 9,
                },
            },
            "rooms": {
                "!r3:bar.org": {
                    "initial": true,
                    "timeline": [],
                },
            },
        },
    };

    assert_entries_batch! {
        [entries_stream]
        push back [ "!r3:bar.org" ];
        end;
    };

    Ok(())
}

#[async_test]
async fn test_dynamic_entries_stream() -> Result<(), Error> {
    let (client, server, room_list) = new_room_list_service().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    let all_rooms = room_list.all_rooms().await?;

    let (dynamic_entries_stream, dynamic_entries) =
        all_rooms.entries_with_dynamic_adapters(5, client.room_info_notable_update_receiver());
    pin_mut!(dynamic_entries_stream);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Init => SettingUp,
        assert request >= {
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
                },
            },
            "rooms": {
                "!r0:bar.org": {
                    "initial": true,
                    "bump_stamp": 1,
                    "required_state": [
                        {
                            "content": {
                                "name": "Matrix Foobar"
                            },
                            "event_id": "$s0",
                            "origin_server_ts": 1,
                            "sender": "@example:bar.org",
                            "state_key": "",
                            "type": "m.room.name"
                        },
                    ],
                },
            },
        },
    };

    // Ensure the dynamic entries' stream is pending because there is no filter set
    // yet.
    assert_pending!(dynamic_entries_stream);

    // Now, let's define a filter.
    dynamic_entries.set_filter(Box::new(new_filter_fuzzy_match_room_name("mat ba")));

    // Assert the dynamic entries.
    assert_entries_batch! {
        [dynamic_entries_stream]
        // Receive a `reset` because the filter has been reset/set for the first time.
        reset [ "!r0:bar.org" ];
        end;
    };
    assert_pending!(dynamic_entries_stream);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = SettingUp => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 9]],
                },
            },
        },
        respond with = {
            "pos": "1",
            "lists": {
                ALL_ROOMS: {
                    "count": 10,
                },
            },
            "rooms": {
                "!r1:bar.org": {
                    "initial": true,
                    "bump_stamp": 2,
                    "required_state": [
                        {
                            "content": {
                                "name": "Matrix Foobaz"
                            },
                            "sender": "@example:bar.org",
                            "state_key": "",
                            "type": "m.room.name",
                            "event_id": "$s1",
                            "origin_server_ts": 2,
                        },
                    ],
                },
                "!r2:bar.org": {
                    "initial": true,
                    "bump_stamp": 3,
                    "required_state": [
                        {
                            "content": {
                                "name": "Hello"
                            },
                            "sender": "@example:bar.org",
                            "state_key": "",
                            "type": "m.room.name",
                            "event_id": "$s2",
                            "origin_server_ts": 3,
                        },
                    ],
                },
                "!r3:bar.org": {
                    "initial": true,
                    "bump_stamp": 4,
                    "required_state": [
                        {
                            "content": {
                                "name": "Helios live"
                            },
                            "sender": "@example:bar.org",
                            "state_key": "",
                            "type": "m.room.name",
                            "event_id": "$s3",
                            "origin_server_ts": 4,
                        },
                    ],
                },
                "!r4:bar.org": {
                    "initial": true,
                    "bump_stamp": 5,
                    "required_state": [
                        {
                            "content": {
                                "name": "Matrix Baz"
                            },
                            "sender": "@example:bar.org",
                            "state_key": "",
                            "type": "m.room.name",
                            "event_id": "$s4",
                            "origin_server_ts": 5,
                        },
                    ],
                },
            },
        },
    };

    // Assert the dynamic entries.
    // It's pushed on the front because rooms are sorted by recency.
    assert_entries_batch! {
        [dynamic_entries_stream]
        push front [ "!r1:bar.org" ];
        push front [ "!r4:bar.org" ];
        end;
    };
    // TODO (@hywan): Must be removed once we restore `RoomInfoNotableUpdate`
    // filtering inside `RoomList`.
    assert_entries_batch! {
        [dynamic_entries_stream]
        set [ 1 ] [ "!r1:bar.org" ];
        end;
    };
    assert_entries_batch! {
        [dynamic_entries_stream]
        set [ 0 ] [ "!r4:bar.org" ];
        end;
    };

    assert_pending!(dynamic_entries_stream);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Running => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 9]],
                },
            },
        },
        respond with = {
            "pos": "2",
            "lists": {
                ALL_ROOMS: {
                    "count": 10,
                },
            },
            "rooms": {
                "!r5:bar.org": {
                    "initial": true,
                    "bump_stamp": 6,
                    "required_state": [
                        {
                            "content": {
                                "name": "Matrix Barracuda Room"
                            },
                            "sender": "@example:bar.org",
                            "state_key": "",
                            "type": "m.room.name",
                            "event_id": "$s5",
                            "origin_server_ts": 6,
                        },
                    ],
                },
                "!r6:bar.org": {
                    "initial": true,
                    "bump_stamp": 7,
                    "required_state": [
                        {
                            "content": {
                                "name": "Matrix is real as hell"
                            },
                            "sender": "@example:bar.org",
                            "state_key": "",
                            "type": "m.room.name",
                            "event_id": "$s6",
                            "origin_server_ts": 7,
                        },
                    ],
                },
                "!r7:bar.org": {
                    "initial": true,
                    "bump_stamp": 8,
                    "required_state": [
                        {
                            "content": {
                                "name": "Matrix Baraka"
                            },
                            "sender": "@example:bar.org",
                            "state_key": "",
                            "type": "m.room.name",
                            "event_id": "$s7",
                            "origin_server_ts": 8,
                        },
                    ],
                },
            },
        },
    };

    // Assert the dynamic entries.
    assert_entries_batch! {
        [dynamic_entries_stream]
        push front [ "!r5:bar.org" ];
        push front [ "!r7:bar.org" ];
        end;
    };
    // TODO (@hywan): Must be removed once we restore `RoomInfoNotableUpdate`
    // filtering inside `RoomList`.
    assert_entries_batch! {
        [dynamic_entries_stream]
        set [ 1 ] [ "!r5:bar.org" ];
        end;
    };
    assert_entries_batch! {
        [dynamic_entries_stream]
        set [ 0 ] [ "!r7:bar.org" ];
        end;
    };

    assert_pending!(dynamic_entries_stream);

    // Now, let's change the dynamic entries!
    dynamic_entries.set_filter(Box::new(new_filter_fuzzy_match_room_name("hell")));

    // Assert the dynamic entries.
    assert_entries_batch! {
        [dynamic_entries_stream]
        // Receive a `reset` again because the filter has been reset.
        reset [ "!r6:bar.org", "!r3:bar.org", "!r2:bar.org" ];
        end;
    };
    assert_pending!(dynamic_entries_stream);

    // Now, let's change again the dynamic filter!
    dynamic_entries.set_filter(Box::new(new_filter_none()));

    // Assert the dynamic entries.
    assert_entries_batch! {
        [dynamic_entries_stream]
        // Receive a `reset` again because the filter has been reset.
        reset [];
        end;
    };

    // Now, let's change again the dynamic filter!
    dynamic_entries.set_filter(Box::new(new_filter_non_left()));

    // Assert the dynamic entries.
    assert_entries_batch! {
        [dynamic_entries_stream]
        // Receive a `reset` again because the filter has been reset.
        reset [
            "!r7:bar.org",
            "!r6:bar.org",
            "!r5:bar.org",
            "!r4:bar.org",
            "!r3:bar.org",
            // Stop! The page is full :-).
        ];
        end;
    }
    assert_pending!(dynamic_entries_stream);

    // Let's ask one more page.
    dynamic_entries.add_one_page();

    // Assert the dynamic entries.
    assert_entries_batch! {
        [dynamic_entries_stream]
        // Receive the next values.
        append [
            "!r2:bar.org",
            "!r1:bar.org",
            "!r0:bar.org",
        ];
        end;
    };
    assert_pending!(dynamic_entries_stream);

    // Let's reset to one page.
    dynamic_entries.reset_to_one_page();

    // Assert the dynamic entries.
    assert_entries_batch! {
        [dynamic_entries_stream]
        // Receive a `truncate`.
        truncate [5];
        end;
    }
    assert_pending!(dynamic_entries_stream);

    // Let's reset to one page again, it should do nothing.
    dynamic_entries.reset_to_one_page();
    assert_pending!(dynamic_entries_stream);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Running => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 9]],
                },
            },
        },
        respond with = {
            "pos": "3",
            "lists": {
                ALL_ROOMS: {
                    "count": 10,
                },
            },
            "rooms": {
                "!r0:bar.org": {
                    "initial": true,
                    "bump_stamp": 9,
                    "required_state": [],
                },
            },
        },
    };

    // Assert the dynamic entries.
    // `!r0:bar.org` has a more recent message.
    // The room must move in the room list.
    assert_entries_batch! {
        [dynamic_entries_stream]
        pop back;
        insert [ 0 ] [ "!r0:bar.org" ];
        end;
    };
    assert_pending!(dynamic_entries_stream);

    // Let's ask one more page again, because it's fun.
    dynamic_entries.add_one_page();

    // Assert the dynamic entries.
    assert_entries_batch! {
        [dynamic_entries_stream]
        // Receive the next values.
        append [
            "!r3:bar.org",
            "!r2:bar.org",
            "!r1:bar.org",
        ];
        end;
    };
    assert_pending!(dynamic_entries_stream);

    Ok(())
}

#[async_test]
async fn test_room_sorting() -> Result<(), Error> {
    let (client, server, room_list) = new_room_list_service().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    let all_rooms = room_list.all_rooms().await?;

    let (stream, dynamic_entries) =
        all_rooms.entries_with_dynamic_adapters(10, client.room_info_notable_update_receiver());
    pin_mut!(stream);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Init => SettingUp,
        assert request >= {
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
                    "count": 5,
                },
            },
            "rooms": {
                "!r0:bar.org": {
                    "initial": true,
                    "bump_stamp": 3,
                    "required_state": [
                        {
                            "content": {
                                "name": "Bbb"
                            },
                            "sender": "@example:bar.org",
                            "state_key": "",
                            "type": "m.room.name",
                            "event_id": "$s0",
                            "origin_server_ts": 3,
                        },
                    ],
                },
                "!r1:bar.org": {
                    "initial": true,
                    "bump_stamp": 3,
                    "required_state": [
                        {
                            "content": {
                                "name": "Aaa"
                            },
                            "sender": "@example:bar.org",
                            "state_key": "",
                            "type": "m.room.name",
                            "event_id": "$s1",
                            "origin_server_ts": 3,
                        },
                    ],
                },
                "!r2:bar.org": {
                    "initial": true,
                    "bump_stamp": 1,
                },
                "!r3:bar.org": {
                    "initial": true,
                    "bump_stamp": 4,
                },
                "!r4:bar.org": {
                    "initial": true,
                    "bump_stamp": 5,
                },
            },
        },
    };

    // Ensure the dynamic entries' stream is pending because there is no filter set
    // yet.
    assert_pending!(stream);

    // Now, let's define a filter.
    dynamic_entries.set_filter(Box::new(new_filter_non_left()));

    // Assert rooms are sorted by recency and by name!.
    assert_entries_batch! {
        [stream]
        reset [
            "!r4:bar.org", // recency of 5
            "!r3:bar.org", // recency of 4
            "!r1:bar.org", // recency of 3, but name comes before `!r0`
            "!r0:bar.org", // recency of 3, but name comes after `!r1`
            "!r2:bar.org", // recency of 1
        ];
        end;
    };

    // Now we have:
    //
    // | index | room ID | recency | name |
    // |-------|---------|---------|------|
    // | 0     | !r4     | 5       |      |
    // | 1     | !r3     | 4       |      |
    // | 2     | !r1     | 3       | Aaa  |
    // | 3     | !r0     | 3       | Bbb  |
    // | 4     | !r2     | 1       |      |

    assert_pending!(stream);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = SettingUp => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 4]],
                },
            },
        },
        respond with = {
            "pos": "1",
            "lists": {
                ALL_ROOMS: {
                    "count": 5,
                },
            },
            "rooms": {
                "!r0:bar.org": {
                    "bump_stamp": 7,
                },
                "!r1:bar.org": {
                    "bump_stamp": 6,
                },
                "!r2:bar.org": {
                    "bump_stamp": 9,
                },
            },
        },
    };

    // Assert rooms are moving.
    assert_entries_batch! {
        [stream]
        remove [ 3 ];
        insert [ 0 ] [ "!r0:bar.org" ];
        end;
    };

    // Now we have:
    //
    // | index | room ID | recency | name |
    // |-------|---------|---------|------|
    // | 0     | !r0     | 7       | Bbb  |
    // | 1     | !r4     | 5       |      |
    // | 2     | !r3     | 4       |      |
    // | 3     | !r1     | 3       | Aaa  |
    // | 4     | !r2     | 1       |      |

    assert_entries_batch! {
        [stream]
        remove [ 3 ];
        insert [ 1 ] [ "!r1:bar.org" ];
        end;
    };

    // Now we have:
    //
    // | index | room ID | recency | name |
    // |-------|---------|---------|------|
    // | 0     | !r0     | 7       | Bbb  |
    // | 1     | !r1     | 6       | Aaa  |
    // | 2     | !r4     | 5       |      |
    // | 3     | !r3     | 4       |      |
    // | 4     | !r2     | 1       |      |

    assert_entries_batch! {
        [stream]
        remove [ 4 ];
        insert [ 0 ] [ "!r2:bar.org" ];
        end;
    };

    // Now we have:
    //
    // | index | room ID | recency | name |
    // |-------|---------|---------|------|
    // | 0     | !r2     | 9       |      |
    // | 1     | !r0     | 7       | Bbb  |
    // | 2     | !r1     | 6       | Aaa  |
    // | 3     | !r4     | 5       |      |
    // | 4     | !r3     | 4       |      |

    assert_pending!(stream);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Running => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 4]],
                },
            },
        },
        respond with = {
            "pos": "2",
            "lists": {
                ALL_ROOMS: {
                    "count": 6,
                },
            },
            "rooms": {
                "!r6:bar.org": {
                    "initial": true,
                    "bump_stamp": 8,
                },
                "!r3:bar.org": {
                    "bump_stamp": 10,
                },
            },
        },
    };

    assert_entries_batch! {
        [stream]
        insert [ 1 ] [ "!r6:bar.org" ];
        end;
    };

    // Now we have:
    //
    // | index | room ID | recency | name |
    // |-------|---------|---------|------|
    // | 0     | !r2     | 9       |      |
    // | 1     | !r6     | 8       |      |
    // | 2     | !r0     | 7       | Bbb  |
    // | 3     | !r1     | 6       | Aaa  |
    // | 4     | !r4     | 5       |      |
    // | 5     | !r3     | 4       |      |

    assert_entries_batch! {
        [stream]
        remove [ 5 ];
        insert [ 0 ] [ "!r3:bar.org" ];
        end;
    };

    // Now we have:
    //
    // | index | room ID | recency | name |
    // |-------|---------|---------|------|
    // | 0     | !r3     | 10      |      |
    // | 1     | !r2     | 9       |      |
    // | 2     | !r6     | 8       |      |
    // | 3     | !r0     | 7       | Bbb  |
    // | 4     | !r1     | 6       | Aaa  |
    // | 5     | !r4     | 5       |      |

    // TODO (@hywan): Must be removed once we restore `RoomInfoNotableUpdate`
    // filtering inside `RoomList`.
    assert_entries_batch! {
        [stream]
        set [ 2 ] [ "!r6:bar.org" ];
        end;
    };

    assert_pending!(stream);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Running => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 5]],
                },
            },
        },
        respond with = {
            "pos": "3",
            "lists": {
                ALL_ROOMS: {
                    "count": 6,
                },
            },
            "rooms": {
                "!r3:bar.org": {
                    "bump_stamp": 11,
                },
            },
        },
    };

    assert_entries_batch! {
        [stream]
        set [ 0 ] [ "!r3:bar.org" ];
        end;
    };

    // Now we have:
    //
    // | index | room ID | recency | name |
    // |-------|---------|---------|------|
    // | 0     | !r3     | 11      |      |
    // | 1     | !r2     | 9       |      |
    // | 2     | !r6     | 8       |      |
    // | 3     | !r0     | 7       | Bbb  |
    // | 4     | !r1     | 6       | Aaa  |
    // | 5     | !r4     | 5       |      |

    assert_pending!(stream);

    Ok(())
}

#[async_test]
async fn test_room() -> Result<(), Error> {
    let (_, server, room_list) = new_room_list_service().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    let room_id_0 = room_id!("!r0:bar.org");
    let room_id_1 = room_id!("!r1:bar.org");

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        assert request >= {},
        respond with = {
            "pos": "0",
            "lists": {
                ALL_ROOMS: {
                    "count": 2,
                },
            },
            "rooms": {
                room_id_0: {
                    "avatar": "mxc://homeserver/media",
                    "initial": true,
                    "required_state": [
                        {
                            "content": {
                                "name": "Room #0"
                            },
                            "event_id": "$1",
                            "origin_server_ts": 42,
                            "sender": "@example:bar.org",
                            "state_key": "",
                            "type": "m.room.name"
                        },
                    ],
                },
                room_id_1: {
                    "initial": true,
                },
            },
        },
    };

    let room0 = room_list.room(room_id_0)?;

    // Room has received a name from sliding sync.
    assert_eq!(room0.cached_display_name(), Some("Room #0".to_owned()));

    // Room has received an avatar from sliding sync.
    assert_eq!(room0.avatar_url(), Some(mxc_uri!("mxc://homeserver/media").to_owned()));

    let room1 = room_list.room(room_id_1)?;

    // Room has not received a name from sliding sync, then it's calculated.
    assert_eq!(room1.cached_display_name(), Some("Empty Room".to_owned()));

    // Room has not received an avatar from sliding sync, then it's calculated, but
    // there is nothing to calculate from, so there is no URL.
    assert_eq!(room1.avatar_url(), None);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        assert request >= {},
        respond with = {
            "pos": "1",
            "lists": {
                ALL_ROOMS: {
                    "count": 2,
                },
            },
            "rooms": {
                room_id_1: {
                    "avatar": "mxc://homeserver/other-media",
                    "required_state": [
                        {
                            "content": {
                                "name": "Room #1"
                            },
                            "event_id": "$1",
                            "origin_server_ts": 42,
                            "sender": "@example:bar.org",
                            "state_key": "",
                            "type": "m.room.name"
                        },
                    ],
                },
            },
        },
    };

    // Room has _now_ received a name from sliding sync!
    assert_eq!(room1.cached_display_name(), Some("Room #1".to_owned()));

    // Room has _now_ received an avatar URL from sliding sync!
    assert_eq!(room1.avatar_url(), Some(mxc_uri!("mxc://homeserver/other-media").to_owned()));

    Ok(())
}

#[async_test]
async fn test_room_not_found() -> Result<(), Error> {
    let (_, _, room_list) = new_room_list_service().await?;

    let room_id = room_id!("!foo:bar.org");

    assert_matches!(
        room_list.room(room_id),
        Err(Error::RoomNotFound(error_room_id)) => {
            assert_eq!(error_room_id, room_id.to_owned());
        }
    );

    Ok(())
}

#[async_test]
async fn test_room_subscription() -> Result<(), Error> {
    let (_, server, room_list) = new_room_list_service().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    let room_id_0 = room_id!("!r0:bar.org");
    let room_id_1 = room_id!("!r1:bar.org");
    let room_id_2 = room_id!("!r2:bar.org");

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        assert request >= {
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
                    "count": 3,
                },
            },
            "rooms": {
                room_id_0: {
                    "initial": true,
                },
                room_id_1: {
                    "initial": true,
                },
                room_id_2: {
                    "initial": true,
                }
            },
        },
    };

    // Subscribe.

    room_list.subscribe_to_rooms(
        &[room_id_1],
        Some(assign!(RoomSubscription::default(), {
            required_state: vec![
                (StateEventType::RoomName, "".to_owned()),
                (StateEventType::RoomTopic, "".to_owned()),
                (StateEventType::RoomAvatar, "".to_owned()),
                (StateEventType::RoomCanonicalAlias, "".to_owned()),
            ],
            timeline_limit: Some(uint!(30)),
        })),
    );

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 2]],
                },
            },
            "room_subscriptions": {
                room_id_1: {
                    "required_state": [
                        ["m.room.name", ""],
                        ["m.room.topic", ""],
                        ["m.room.avatar", ""],
                        ["m.room.canonical_alias", ""],
                        ["m.room.create", ""],
                    ],
                    "timeline_limit": 30,
                },
            },
        },
        respond with = {
            "pos": "1",
            "lists": {},
            "rooms": {},
        },
    };

    // Subscribe to another room.

    room_list.subscribe_to_rooms(
        &[room_id_2],
        Some(assign!(RoomSubscription::default(), {
            required_state: vec![
                (StateEventType::RoomName, "".to_owned()),
                (StateEventType::RoomTopic, "".to_owned()),
                (StateEventType::RoomAvatar, "".to_owned()),
                (StateEventType::RoomCanonicalAlias, "".to_owned()),
            ],
            timeline_limit: Some(uint!(30)),
        })),
    );

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 2]],
                },
            },
            "room_subscriptions": {
                room_id_1: {
                    "required_state": [
                        ["m.room.name", ""],
                        ["m.room.topic", ""],
                        ["m.room.avatar", ""],
                        ["m.room.canonical_alias", ""],
                        ["m.room.create", ""],
                    ],
                    "timeline_limit": 30,
                },
                room_id_2: {
                    "required_state": [
                        ["m.room.name", ""],
                        ["m.room.topic", ""],
                        ["m.room.avatar", ""],
                        ["m.room.canonical_alias", ""],
                        ["m.room.create", ""],
                    ],
                    "timeline_limit": 30,
                },
            },
        },
        respond with = {
            "pos": "2",
            "lists": {},
            "rooms": {},
        },
    };

    Ok(())
}

#[async_test]
async fn test_room_unread_notifications() -> Result<(), Error> {
    let (_, server, room_list) = new_room_list_service().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    let room_id = room_id!("!r0:bar.org");

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        assert request >= {
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
                    "count": 1,
                },
            },
            "rooms": {
                room_id: {
                    "initial": true,
                },
            },
        },
    };

    let room = room_list.room(room_id).unwrap();

    assert_matches!(
        room.unread_notification_counts(),
        UnreadNotificationsCount { highlight_count: 0, notification_count: 0 }
    );

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 0]],
                },
            },
        },
        respond with = {
            "pos": "1",
            "lists": {},
            "rooms": {
                room_id: {
                    "timeline": [ /* … */ ],
                    "notification_count": 2,
                    "highlight_count": 1,
                },
            },
        },
    };

    assert_matches!(
        room.unread_notification_counts(),
        UnreadNotificationsCount { highlight_count: 1, notification_count: 2 }
    );

    Ok(())
}

#[async_test]
async fn test_room_timeline() -> Result<(), Error> {
    let (_, server, room_list) = new_room_list_service().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    let room_id = room_id!("!r0:bar.org");

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        assert request >= {},
        respond with = {
            "pos": "0",
            "lists": {
                ALL_ROOMS: {
                    "count": 2,
                },
            },
            "rooms": {
                room_id: {
                    "initial": true,
                    "timeline": [
                        timeline_event!("$x0:bar.org" at 0 sec),
                    ],
                },
            },
        },
    };

    mock_encryption_state(&server, false).await;

    let room = room_list.room(room_id)?;
    room.init_timeline_with_builder(room.default_room_timeline_builder().await.unwrap()).await?;
    let timeline = room.timeline().unwrap();

    let (previous_timeline_items, mut timeline_items_stream) = timeline.subscribe().await;

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        assert request >= {},
        respond with = {
            "pos": "1",
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
        **previous_timeline_items[0],
        TimelineItemKind::Virtual(VirtualTimelineItem::DayDivider(_))
    );
    assert_matches!(
        &**previous_timeline_items[1],
        TimelineItemKind::Event(item) => {
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
    let (_, server, room_list) = new_room_list_service().await?;
    mock_encryption_state(&server, false).await;

    let sync = room_list.sync();
    pin_mut!(sync);

    let room_id = room_id!("!r0:bar.org");

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        assert request >= {},
        respond with = {
            "pos": "0",
            "lists": {
                ALL_ROOMS: {
                    "count": 2,
                },
            },
            "rooms": {
                room_id: {
                    "initial": true,
                },
            },
        },
    };

    let room = room_list.room(room_id)?;
    room.init_timeline_with_builder(room.default_room_timeline_builder().await.unwrap()).await?;

    // The latest event does not exist.
    assert!(room.latest_event().await.is_none());

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        assert request >= {},
        respond with = {
            "pos": "1",
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
            assert!(event.is_local_echo().not());
            assert_eq!(event.event_id(), Some(event_id!("$x0:bar.org")));
        }
    );

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        assert request >= {},
        respond with = {
            "pos": "2",
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
    let latest_event = room.latest_event().await.unwrap();
    assert!(latest_event.is_local_echo().not());
    assert_eq!(latest_event.event_id(), Some(event_id!("$x1:bar.org")));

    // Now let's compare with the result of the `Timeline`.
    let timeline = room.timeline().unwrap();

    // The latest event matches the latest event of the `Timeline`.
    assert_matches!(
        timeline.latest_event().await,
        Some(timeline_event) => {
            assert_eq!(timeline_event.event_id(), latest_event.event_id());
        }
    );

    // Insert a local event in the `Timeline`.
    timeline.send(RoomMessageEventContent::text_plain("Hello, World!").into()).await.unwrap();

    // Let the send queue send the message, and the timeline process it.
    yield_now().await;

    // The latest event of the `Timeline` is a local event.
    assert_matches!(
        timeline.latest_event().await,
        Some(timeline_event) => {
            assert!(timeline_event.is_local_echo());
            assert_eq!(timeline_event.event_id(), None);
        }
    );

    // The latest event is a local event.
    assert_matches!(
        room.latest_event().await,
        Some(event) => {
            assert!(event.is_local_echo());
            assert_eq!(event.event_id(), None);
        }
    );

    Ok(())
}

// #[ignore = "Flaky"]
#[async_test]
async fn test_sync_indicator() -> Result<(), Error> {
    let (_, server, room_list) = new_room_list_service().await?;

    const DELAY_BEFORE_SHOWING: Duration = Duration::from_millis(20);
    const DELAY_BEFORE_HIDING: Duration = Duration::from_millis(0);

    let sync = room_list.sync();
    pin_mut!(sync);

    let sync_indicator = room_list.sync_indicator(DELAY_BEFORE_SHOWING, DELAY_BEFORE_HIDING);

    let request_margin = Duration::from_millis(100);
    let request_1_delay = DELAY_BEFORE_SHOWING * 2;
    let request_2_delay = DELAY_BEFORE_SHOWING * 3;
    let request_4_delay = DELAY_BEFORE_SHOWING * 2;
    let request_5_delay = DELAY_BEFORE_SHOWING * 2;

    let (in_between_requests_synchronizer_sender, mut in_between_requests_synchronizer) =
        channel(1);

    macro_rules! assert_next_sync_indicator {
        ($sync_indicator:ident, $pattern:pat, now $(,)?) => {
            assert_matches!($sync_indicator.next().now_or_never(), Some(Some($pattern)));
        };

        ($sync_indicator:ident, $pattern:pat, under $time:expr $(,)?) => {
            let now = Instant::now();
            assert_matches!($sync_indicator.next().await, Some($pattern));
            assert!(now.elapsed() < $time);
        };
    }

    let sync_indicator_task = spawn(async move {
        pin_mut!(sync_indicator);

        // `SyncIndicator` is forced to be hidden to begin with.
        assert_next_sync_indicator!(sync_indicator, SyncIndicator::Hide, now);

        // Request 1.
        {
            // Sync has started, the `SyncIndicator` must be shown… but not immediately!
            assert_next_sync_indicator!(
                sync_indicator,
                SyncIndicator::Show,
                under DELAY_BEFORE_SHOWING + request_margin,
            );

            // Then, once the sync is done, the `SyncIndicator` must be hidden.
            assert_next_sync_indicator!(
                sync_indicator,
                SyncIndicator::Hide,
                under request_1_delay - DELAY_BEFORE_SHOWING
                    + DELAY_BEFORE_HIDING
                    + request_margin,
            );
        }

        in_between_requests_synchronizer.recv().await.unwrap();
        assert_pending!(sync_indicator);

        // Request 2.
        {
            // Nothing happens, as the state transitions from `SettingUp` to
            // `Running`, no `SyncIndicator` must be shown.
        }

        in_between_requests_synchronizer.recv().await.unwrap();
        assert_pending!(sync_indicator);

        // Request 3.
        {
            // Sync has errored, the `SyncIndicator` should be show. Fortunately
            // for us (fictional situation), the sync is restarted
            // immediately, and `SyncIndicator` doesn't have time to
            // be shown for this particular state update.
        }

        in_between_requests_synchronizer.recv().await.unwrap();
        assert_pending!(sync_indicator);

        // Request 4.
        {
            // The system is recovering, It takes times (fictional situation)!
            // `SyncIndicator` has time to show (off).
            assert_next_sync_indicator!(
                sync_indicator,
                SyncIndicator::Show,
                under DELAY_BEFORE_SHOWING + request_margin,
            );

            // Then, once the sync is done, the `SyncIndicator` must be hidden.
            assert_next_sync_indicator!(
                sync_indicator,
                SyncIndicator::Hide,
                under request_4_delay - DELAY_BEFORE_SHOWING
                    + DELAY_BEFORE_HIDING
                    + request_margin,
            );
        }

        in_between_requests_synchronizer.recv().await.unwrap();
        assert_pending!(sync_indicator);

        // Request 5.

        in_between_requests_synchronizer.recv().await.unwrap();

        // Even though request 5 took a while, the `SyncIndicator` shouldn't show.
        assert_pending!(sync_indicator);
    });

    // Request 1.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Init => SettingUp,
        assert request >= {},
        respond with = {
            "pos": "0",
        },
        after delay = request_1_delay, // Slow request!
    };

    in_between_requests_synchronizer_sender.send(()).await.unwrap();

    // Request 2.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = SettingUp => Running,
        assert request >= {},
        respond with = {
            "pos": "1",
        },
        after delay = request_2_delay, // Slow request!
    };

    in_between_requests_synchronizer_sender.send(()).await.unwrap();

    // Request 3.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        sync matches Some(Err(_)),
        states = Running => Error { .. },
        assert request >= {},
        respond with = (code 400) {
            "error": "foo",
            "errcode": "M_UNKNOWN",
        },
    };

    let sync = room_list.sync();
    pin_mut!(sync);

    in_between_requests_synchronizer_sender.send(()).await.unwrap();

    // Request 4.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Error { .. } => Recovering,
        assert request >= {},
        respond with = {
            "pos": "2",
        },
        after delay = request_4_delay, // Slow request!
    };

    in_between_requests_synchronizer_sender.send(()).await.unwrap();

    // Request 5.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Recovering => Running,
        assert request >= {},
        respond with = {
            "pos": "3",
        },
        after delay = request_5_delay, // Slow request!
    };

    in_between_requests_synchronizer_sender.send(()).await.unwrap();

    sync_indicator_task.await.unwrap();

    Ok(())
}
