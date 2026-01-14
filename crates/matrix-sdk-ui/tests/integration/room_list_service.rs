use std::{collections::BTreeMap, sync::Arc};

use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use futures_util::{FutureExt, StreamExt, pin_mut};
use matrix_sdk::{
    Client, RoomDisplayName,
    config::RequestConfig,
    test_utils::{
        logged_in_client_with_server,
        mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
        set_client_session, test_client_builder,
    },
};
use matrix_sdk_base::sync::UnreadNotificationsCount;
use matrix_sdk_test::{
    ALICE, async_test, event_factory::EventFactory, mocks::mock_encryption_state,
};
use matrix_sdk_ui::{
    RoomListService,
    room_list_service::{
        ALL_ROOMS_LIST_NAME as ALL_ROOMS, Error, RoomListLoadingState, State, SyncIndicator,
        filters::{new_filter_fuzzy_match_room_name, new_filter_non_left, new_filter_none},
    },
    timeline::{LatestEventValue, RoomExt as _, TimelineItemKind, VirtualTimelineItem},
};
use ruma::{
    api::client::room::create_room::v3::Request as CreateRoomRequest,
    events::room::message::RoomMessageEventContent,
    mxc_uri, room_id,
    time::{Duration, Instant},
};
use serde_json::json;
use stream_assert::{assert_next_matches, assert_pending};
use tempfile::TempDir;
use tokio::{spawn, sync::Barrier, task::yield_now, time::sleep};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

use crate::timeline::sliding_sync::{assert_timeline_stream, timeline_event};

async fn new_room_list_service() -> Result<(Client, MockServer, RoomListService), Error> {
    let (client, server) = logged_in_client_with_server().await;
    let room_list = RoomListService::new(client.clone()).await?;

    Ok((client, server, room_list))
}

async fn new_persistent_room_list_service(
    store_path: &std::path::Path,
) -> Result<(MockServer, RoomListService), Error> {
    let server = MockServer::start().await;
    let client = test_client_builder(Some(server.uri().to_string()))
        .request_config(RequestConfig::new().disable_retry())
        .sqlite_store(store_path, None)
        .build()
        .await
        .unwrap();
    set_client_session(&client).await;

    let room_list = RoomListService::new(client.clone()).await?;

    Ok((server, room_list))
}

// Same macro as in the main, with additional checking that the state
// before/after the sync loop match those we expect.
macro_rules! sync_then_assert_request_and_fake_response {
    (
        [$server:ident, $room_list:ident, $stream:ident]
        $( states = $pre_state:pat => $post_state:pat, )?
        $( assert pos $pos:expr, )?
        $( assert timeout $timeout:expr, )?
        assert request $assert_request:tt { $( $request_json:tt )* },
        respond with = $( ( code $code:expr ) )? { $( $response_json:tt )* }
        $( , after delay = $response_delay:expr )?
        $(,)?
    ) => {
        sync_then_assert_request_and_fake_response! {
            [$server, $room_list, $stream]
            sync matches Some(Ok(_)),
            $( states = $pre_state => $post_state, )?
            $( assert pos $pos, )?
            $( assert timeout $timeout, )?
            assert request $assert_request { $( $request_json )* },
            respond with = $( ( code $code ) )? { $( $response_json )* },
            $( after delay = $response_delay, )?
        }
    };

    (
        [$server:ident, $room_list:ident, $stream:ident]
        sync matches $sync_result:pat,
        $( states = $pre_state:pat => $post_state:pat, )?
        $( assert pos $pos:expr, )?
        $( assert timeout $timeout:expr, )?
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
                $( assert pos $pos, )?
                $( assert timeout $timeout, )?
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
        // No `pos` because it's the first fresh query.
        assert pos None,
        // `timeout=0` because we don't want long-polling.
        assert timeout Some(0),
        assert request = {
            "conn_id": "room-list",
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 19]],
                    "required_state": [
                        ["m.room.name", ""],
                        ["m.room.encryption", ""],
                        ["m.room.member", "$LAZY"],
                        ["m.room.member", "$ME"],
                        ["m.room.topic", ""],
                        ["m.room.avatar", ""],
                        ["m.room.canonical_alias", ""],
                        ["m.room.power_levels", ""],
                        ["org.matrix.msc3401.call.member", "*"],
                        ["m.room.join_rules", ""],
                        ["m.room.tombstone", ""],
                        ["m.room.create", ""],
                        ["m.room.history_visibility", ""],
                        ["io.element.functional_members", ""],
                        ["m.space.parent", "*"],
                        ["m.space.child", "*"],
                    ],
                    "filters": {},
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
                    "count": 220,
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
        // The previous `pos`.
        assert pos Some("0"),
        // Still no long-polling because the list isn't fully-loaded.
        assert timeout Some(0),
        assert request >= {
            "conn_id": "room-list",
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 99]],
                    "timeline_limit": 1,
                },
            },
        },
        respond with = {
            "pos": "1",
            "lists": {
                ALL_ROOMS: {
                    "count": 220,
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
        assert pos Some("1"),
        // Still no long-polling because the list isn't fully-loaded.
        assert timeout Some(0),
        assert request >= {
            "conn_id": "room-list",
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 199]],
                    "timeline_limit": 1,
                },
            },
        },
        respond with = {
            "pos": "2",
            "lists": {
                ALL_ROOMS: {
                    "count": 220,
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
        assert pos Some("2"),
        // Still no long-polling because the list isn't fully-loaded,
        // but it's about to be!
        assert timeout Some(0),
        assert request >= {
            "conn_id": "room-list",
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 219]],
                    "timeline_limit": 1,
                },
            },
        },
        respond with = {
            "pos": "3",
            "lists": {
                ALL_ROOMS: {
                    "count": 220,
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
        assert pos Some("3"),
        // The list is fully-loaded, we can start long-polling.
        assert timeout Some(30000),
        assert request >= {
            "conn_id": "room-list",
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 219]],
                    "timeline_limit": 1,
                },
            },
        },
        respond with = {
            "pos": "4",
            "lists": {
                ALL_ROOMS: {
                    "count": 220,
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
            assert pos None,
            assert request >= {
                "lists": {
                    ALL_ROOMS: {
                        "ranges": [[0, 19]],
                        "timeline_limit": 1,
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
            assert pos Some("0"),
            assert request >= {
                "lists": {
                    ALL_ROOMS: {
                        "ranges": [[0, 9]],
                        "timeline_limit": 1,
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
            assert pos Some("1"),
            assert request >= {
                "lists": {
                    ALL_ROOMS: {
                        "ranges": [[0, 9]],
                        "timeline_limit": 1,
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
#[ignore] // `share_pos()` has been disabled in the room list, see there to learn more.
async fn test_sync_resumes_from_previous_state_after_restart() -> Result<(), Error> {
    let tmp_dir = TempDir::new().unwrap();
    let store_path = tmp_dir.path();

    {
        let (server, room_list) = new_persistent_room_list_service(store_path).await?;
        let sync = room_list.sync();
        pin_mut!(sync);

        let all_rooms = room_list.all_rooms().await?;
        let mut all_rooms_loading_state = all_rooms.loading_state();

        // The loading is not loaded.
        assert_next_matches!(all_rooms_loading_state, RoomListLoadingState::NotLoaded);
        assert_pending!(all_rooms_loading_state);

        sync_then_assert_request_and_fake_response! {
            [server, room_list, sync]
            states = Init => SettingUp,
            assert pos None,
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

    {
        let (server, room_list) = new_persistent_room_list_service(store_path).await?;
        let sync = room_list.sync();
        pin_mut!(sync);

        let all_rooms = room_list.all_rooms().await?;
        let mut all_rooms_loading_state = all_rooms.loading_state();

        // Wait on Tokio to run all the tasks. Necessary only when testing.
        yield_now().await;

        // We already have a state stored so the list should already be loaded
        assert_next_matches!(
            all_rooms_loading_state,
            RoomListLoadingState::Loaded { maximum_number_of_rooms: Some(10) }
        );
        assert_pending!(all_rooms_loading_state);

        // pos has been restored and is used when doing the req
        sync_then_assert_request_and_fake_response! {
            [server, room_list, sync]
            states = Init => SettingUp,
            assert pos Some("0"),
            assert request >= {
                "lists": {
                    ALL_ROOMS: {
                        "ranges": [[0, 19]],
                    },
                },
            },
            respond with = {
                "pos": "1",
                "lists": {
                    ALL_ROOMS: {
                        "count": 12,
                    },
                },
                "rooms": {},
            },
        };

        // Wait on Tokio to run all the tasks. Necessary only when testing.
        yield_now().await;

        // maximum_number_of_rooms changed so we should get a new loaded state
        assert_next_matches!(
            all_rooms_loading_state,
            RoomListLoadingState::Loaded { maximum_number_of_rooms: Some(12) }
        );
        assert_pending!(all_rooms_loading_state);
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
                    "timeline_limit": 1,
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
                    "timeline_limit": 1,
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
                    "timeline_limit": 1,
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
                    "timeline_limit": 1,
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
                    "timeline_limit": 1,
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
                    "timeline_limit": 1,
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
                    "timeline_limit": 1,
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
                    "timeline_limit": 1,
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
                    "timeline_limit": 1,
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
                    "timeline_limit": 1,
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
                    "timeline_limit": 1,
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
                    "timeline_limit": 1,
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
                    "timeline_limit": 1,
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
                    "timeline_limit": 1,
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
                    "timeline_limit": 1,
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
                    "timeline_limit": 1,
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
                    "timeline_limit": 1,
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
                    "timeline_limit": 1,
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
        assert_next_matches!(all_rooms_loading_state, RoomListLoadingState::NotLoaded);

        sync_then_assert_request_and_fake_response! {
            [server, room_list, sync]
            states = Init => SettingUp,
            assert request >= {
                "lists": {
                    ALL_ROOMS: {
                        "ranges": [[0, 19]],
                        "timeline_limit": 1,
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
                        "timeline_limit": 1,
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
                        "timeline_limit": 1,
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
        assert_next_matches!(
            all_rooms_loading_state,
            RoomListLoadingState::Loaded { maximum_number_of_rooms: Some(12) }
        );

        // The sync starts at the `Init` state but the `pos` marker is set to 3.
        sync_then_assert_request_and_fake_response! {
            [server, room_list, sync]
            states = Init => SettingUp,
            assert request >= {
                "lists": {
                    ALL_ROOMS: {
                        "ranges": [[0, 19]],
                        "timeline_limit": 1,
                    },
                },
            },
            respond with = {
                "pos": "3",
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
async fn test_dynamic_entries_stream() -> Result<(), Error> {
    let (_client, server, room_list) = new_room_list_service().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    let all_rooms = room_list.all_rooms().await?;

    let (dynamic_entries_stream, dynamic_entries) = all_rooms.entries_with_dynamic_adapters(5);
    pin_mut!(dynamic_entries_stream);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Init => SettingUp,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 19]],
                    "timeline_limit": 1,
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
                    "timeline_limit": 1,
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
    // TODO (@hywan): Remove as soon as `RoomInfoNotableUpdateReasons::NONE` is
    // removed.
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
    // TODO (@hywan): Remove as soon as `RoomInfoNotableUpdateReasons::NONE` is
    // removed.
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
                    "timeline_limit": 1,
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
    // TODO (@hywan): Remove as soon as `RoomInfoNotableUpdateReasons::NONE` is
    // removed.
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

    // TODO (@hywan): Remove as soon as `RoomInfoNotableUpdateReasons::NONE` is
    // removed.
    assert_entries_batch! {
        [dynamic_entries_stream]
        set [ 1 ] [ "!r5:bar.org" ];
        end;
    };
    // TODO (@hywan): Remove as soon as `RoomInfoNotableUpdateReasons::NONE` is
    // removed.
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
                    "timeline_limit": 1,
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
                    "required_state": [
                        {
                            "content": {
                                "name": "Look, a new name"
                            },
                            "sender": "@example:bar.org",
                            "state_key": "",
                            "type": "m.room.name",
                            "event_id": "$s8",
                            "origin_server_ts": 9,
                        },
                    ],
                },
            },
        },
    };

    // Assert the dynamic entries.
    // `!r0:bar.org` has a new state event. The room must move in the room list
    // because it has the highest recency.
    assert_entries_batch! {
        [dynamic_entries_stream]
        pop back;
        push front [ "!r0:bar.org" ];
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
    let (_client, server, room_list) = new_room_list_service().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    let all_rooms = room_list.all_rooms().await?;

    let (stream, dynamic_entries) = all_rooms.entries_with_dynamic_adapters(10);
    pin_mut!(stream);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Init => SettingUp,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 19]],
                    "timeline_limit": 1,
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

    // Assert rooms are sorted by recency and by name!
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
                    "timeline_limit": 1,
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
                    "timeline": [{
                        "content": {
                              "body": "foo",
                              "msgtype": "m.text",
                          },
                          "event_id": "$ev7",
                          "origin_server_ts": 7,
                          "sender": "@example:bar.org",
                          "type": "m.room.message",
                    }],
                },
                "!r1:bar.org": {
                    "bump_stamp": 6,
                    "timeline": [{
                        "content": {
                              "body": "foo",
                              "msgtype": "m.text"
                          },
                          "event_id": "$ev6",
                          "origin_server_ts": 6,
                          "sender": "@example:bar.org",
                          "type": "m.room.message",
                    }],
                },
                "!r2:bar.org": {
                    "bump_stamp": 9,
                    "timeline": [{
                        "content": {
                              "body": "foo",
                              "msgtype": "m.text"
                          },
                          "event_id": "$ev9",
                          "origin_server_ts": 9,
                          "sender": "@example:bar.org",
                          "type": "m.room.message",
                    }],
                },
            },
        },
    };

    // Assert rooms are moving.
    assert_entries_batch! {
        [stream]
        remove [ 3 ];
        push front [ "!r0:bar.org" ];
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
        push front [ "!r2:bar.org" ];
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

    // Rooms are individually updated.
    assert_entries_batch! {
        [stream]
        set [ 1 ] [ "!r0:bar.org" ];
        end;
    };
    assert_entries_batch! {
        [stream]
        set [ 2 ] [ "!r1:bar.org" ];
        end;
    };
    assert_entries_batch! {
        [stream]
        set [ 0 ] [ "!r2:bar.org" ];
        end;
    };

    assert_pending!(stream);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Running => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 4]],
                    "timeline_limit": 1,
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
                    "timeline": [{
                        "content": {
                              "body": "foo",
                              "msgtype": "m.text"
                          },
                          "event_id": "$ev8",
                          "origin_server_ts": 8,
                          "sender": "@example:bar.org",
                          "type": "m.room.message",
                    }],
                },
                "!r3:bar.org": {
                    "bump_stamp": 10,
                    "timeline": [{
                        "content": {
                              "body": "foo",
                              "msgtype": "m.text"
                          },
                          "event_id": "$ev10",
                          "origin_server_ts": 10,
                          "sender": "@example:bar.org",
                          "type": "m.room.message",
                    }],
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

    // Rooms are individually updated.
    assert_entries_batch! {
        [stream]
        set [ 1 ] [ "!r6:bar.org" ];
        end;
    };
    assert_entries_batch! {
        [stream]
        set [ 1 ] [ "!r6:bar.org" ];
        end;
    };

    assert_entries_batch! {
        [stream]
        remove [ 5 ];
        push front [ "!r3:bar.org" ];
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

    // Rooms are individually updated.
    assert_entries_batch! {
        [stream]
        set [ 2 ] [ "!r6:bar.org" ];
        end;
    };
    assert_entries_batch! {
        [stream]
        set [ 0 ] [ "!r3:bar.org" ];
        end;
    };
    assert_entries_batch! {
        [stream]
        set [ 2 ] [ "!r6:bar.org" ];
        end;
    };

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
    assert_eq!(room0.cached_display_name(), Some(RoomDisplayName::Named("Room #0".to_owned())));

    // Room has received an avatar from sliding sync.
    assert_eq!(room0.avatar_url(), Some(mxc_uri!("mxc://homeserver/media").to_owned()));

    let room1 = room_list.room(room_id_1)?;

    // Room has not received a name from sliding sync, then it's calculated.
    assert_eq!(room1.cached_display_name(), Some(RoomDisplayName::Empty));

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
    assert_eq!(room1.cached_display_name(), Some(RoomDisplayName::Named("Room #1".to_owned())));

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
                    "timeline_limit": 1,
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
    room_list.subscribe_to_rooms(&[room_id_1]).await;

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 2]],
                    "timeline_limit": 1,
                },
            },
            "room_subscriptions": {
                room_id_1: {
                    "required_state": [
                        ["m.room.name", ""],
                        ["m.room.encryption", ""],
                        ["m.room.member", "$LAZY"],
                        ["m.room.member", "$ME"],
                        ["m.room.topic", ""],
                        ["m.room.avatar", ""],
                        ["m.room.canonical_alias", ""],
                        ["m.room.power_levels", ""],
                        ["org.matrix.msc3401.call.member", "*"],
                        ["m.room.join_rules", ""],
                        ["m.room.tombstone", ""],
                        ["m.room.create", ""],
                        ["m.room.history_visibility", ""],
                        ["io.element.functional_members", ""],
                        ["m.space.parent", "*"],
                        ["m.space.child", "*"],
                        ["m.room.pinned_events", ""],
                    ],
                    "timeline_limit": 20,
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
    room_list.subscribe_to_rooms(&[room_id_2]).await;

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        // strict comparison (with `=`) because we want to ensure
        // the exact shape of `room_subscriptions`.
        assert request = {
            "conn_id": "room-list",
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 2]],
                    "required_state": [
                        ["m.room.name", ""],
                        ["m.room.encryption", ""],
                        ["m.room.member", "$LAZY"],
                        ["m.room.member", "$ME"],
                        ["m.room.topic", ""],
                        ["m.room.avatar", ""],
                        ["m.room.canonical_alias", ""],
                        ["m.room.power_levels", ""],
                        ["org.matrix.msc3401.call.member", "*"],
                        ["m.room.join_rules", ""],
                        ["m.room.tombstone", ""],
                        ["m.room.create", ""],
                        ["m.room.history_visibility", ""],
                        ["io.element.functional_members", ""],
                        ["m.space.parent", "*"],
                        ["m.space.child", "*"],
                    ],
                    "filters": {},
                    "timeline_limit": 1,
                },
            },
            "room_subscriptions": {
                room_id_2: {
                    "required_state": [
                        ["m.room.name", ""],
                        ["m.room.encryption", ""],
                        ["m.room.member", "$LAZY"],
                        ["m.room.member", "$ME"],
                        ["m.room.topic", ""],
                        ["m.room.avatar", ""],
                        ["m.room.canonical_alias", ""],
                        ["m.room.power_levels", ""],
                        ["org.matrix.msc3401.call.member", "*"],
                        ["m.room.join_rules", ""],
                        ["m.room.tombstone", ""],
                        ["m.room.create", ""],
                        ["m.room.history_visibility", ""],
                        ["io.element.functional_members", ""],
                        ["m.space.parent", "*"],
                        ["m.space.child", "*"],
                        ["m.room.pinned_events", ""],
                    ],
                    "timeline_limit": 20,
                },
            },
            "extensions": {
                "account_data": { "enabled": true },
                "receipts": { "enabled": true, "rooms": [ "*" ] },
                "typing": { "enabled": true },
            },
        },
        respond with = {
            "pos": "2",
            "lists": {},
            "rooms": {},
        },
    };

    // Subscribe to an already subscribed room, plus a previously removed one.
    room_list.subscribe_to_rooms(&[room_id_1, room_id_2]).await;

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        // strict comparison (with `=`) because we want to ensure
        // the exact shape of `room_subscriptions`.
        assert request = {
            "conn_id": "room-list",
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 2]],
                    "required_state": [
                        ["m.room.name", ""],
                        ["m.room.encryption", ""],
                        ["m.room.member", "$LAZY"],
                        ["m.room.member", "$ME"],
                        ["m.room.topic", ""],
                        ["m.room.avatar", ""],
                        ["m.room.canonical_alias", ""],
                        ["m.room.power_levels", ""],
                        ["org.matrix.msc3401.call.member", "*"],
                        ["m.room.join_rules", ""],
                        ["m.room.tombstone", ""],
                        ["m.room.create", ""],
                        ["m.room.history_visibility", ""],
                        ["io.element.functional_members", ""],
                        ["m.space.parent", "*"],
                        ["m.space.child", "*"],
                    ],
                    "filters": {},
                    "timeline_limit": 1,
                },
            },
            "room_subscriptions": {
                room_id_1: {
                    "required_state": [
                        ["m.room.name", ""],
                        ["m.room.encryption", ""],
                        ["m.room.member", "$LAZY"],
                        ["m.room.member", "$ME"],
                        ["m.room.topic", ""],
                        ["m.room.avatar", ""],
                        ["m.room.canonical_alias", ""],
                        ["m.room.power_levels", ""],
                        ["org.matrix.msc3401.call.member", "*"],
                        ["m.room.join_rules", ""],
                        ["m.room.tombstone", ""],
                        ["m.room.create", ""],
                        ["m.room.history_visibility", ""],
                        ["io.element.functional_members", ""],
                        ["m.space.parent", "*"],
                        ["m.space.child", "*"],
                        ["m.room.pinned_events", ""],
                    ],
                    "timeline_limit": 20,
                },
                room_id_2: {
                    "required_state": [
                        ["m.room.name", ""],
                        ["m.room.encryption", ""],
                        ["m.room.member", "$LAZY"],
                        ["m.room.member", "$ME"],
                        ["m.room.topic", ""],
                        ["m.room.avatar", ""],
                        ["m.room.canonical_alias", ""],
                        ["m.room.power_levels", ""],
                        ["org.matrix.msc3401.call.member", "*"],
                        ["m.room.join_rules", ""],
                        ["m.room.tombstone", ""],
                        ["m.room.create", ""],
                        ["m.room.history_visibility", ""],
                        ["io.element.functional_members", ""],
                        ["m.space.parent", "*"],
                        ["m.space.child", "*"],
                        ["m.room.pinned_events", ""],
                    ],
                    "timeline_limit": 20,
                },
            },
            "extensions": {
                "account_data": { "enabled": true },
                "receipts": { "enabled": true, "rooms": [ "*" ] },
                "typing": { "enabled": true },
            },
        },
        respond with = {
            "pos": "3",
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
                    "timeline_limit": 1,
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
                    "timeline_limit": 1,
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
    let timeline = room.timeline_builder().build().await.unwrap();

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
        TimelineItemKind::Virtual(VirtualTimelineItem::DateDivider(_))
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
async fn test_room_empty_timeline() {
    let (client, server, room_list) = new_room_list_service().await.unwrap();
    mock_encryption_state(&server, false).await;

    Mock::given(method("POST"))
        .and(path("_matrix/client/r0/createRoom"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "room_id": "!example:localhost"})),
        )
        .mount(&server)
        .await;

    let room = client.create_room(CreateRoomRequest::default()).await.unwrap();
    let room_id = room.room_id().to_owned();

    // The room wasn't synced, but it will be available
    let room = room_list.room(&room_id).unwrap();
    let timeline = room.timeline_builder().build().await.unwrap();
    let (prev_items, _) = timeline.subscribe().await;

    // However, since the room wasn't synced its timeline won't have any initial
    // items
    assert!(prev_items.is_empty());
}

#[async_test]
async fn test_room_latest_event() -> Result<(), Error> {
    let (client, server, room_list) = new_room_list_service().await?;
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
    let timeline = room.timeline_builder().build().await.unwrap();

    // We could subscribe to the room —with `RoomList::subscribe_to_rooms`— to
    // automatically listen to the latest event updates, but we will do it
    // manually here (so that we can ignore the subscription thingies).
    let latest_events = client.latest_events().await;
    latest_events.listen_to_room(room_id).await.unwrap();

    // The latest event does not exist.
    assert_matches!(room.latest_event().await, LatestEventValue::None);

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

    yield_now().await;

    // The latest event exists.
    assert_matches!(room.latest_event().await, LatestEventValue::Remote { .. });

    // Insert a local event in the `Timeline`.
    timeline.send(RoomMessageEventContent::text_plain("Hello, World!").into()).await.unwrap();

    // Let the latest event be computed.
    yield_now().await;

    // The latest event has been updated.
    assert_matches!(room.latest_event().await, LatestEventValue::Local { .. });

    Ok(())
}

#[async_test]
async fn test_sync_indicator() -> Result<(), Error> {
    let (_, server, room_list) = new_room_list_service().await?;

    const DELAY_BEFORE_SHOWING: Duration = Duration::from_millis(100);
    const DELAY_BEFORE_HIDING: Duration = Duration::from_millis(0);

    let sync = room_list.sync();
    pin_mut!(sync);

    let sync_indicator = room_list.sync_indicator(DELAY_BEFORE_SHOWING, DELAY_BEFORE_HIDING);

    let request_margin = Duration::from_millis(100);
    let request_delay = DELAY_BEFORE_SHOWING * 2;

    let barrier = Arc::new(Barrier::new(2));
    let barrier_sync_indicator = barrier.clone();

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

        let barrier = barrier_sync_indicator;

        // `SyncIndicator` is forced to be hidden to begin with.
        assert_next_sync_indicator!(sync_indicator, SyncIndicator::Hide, now);

        barrier.wait().await;

        // Request 1.
        {
            // The state transitions into `Init`. The `SyncIndicator` stays in `Hide` as
            // nothing is happening yet.
            assert_next_sync_indicator!(
                sync_indicator,
                SyncIndicator::Hide,
                under DELAY_BEFORE_SHOWING + request_margin,
            );
        }

        barrier.wait().await;

        // Request 2.
        {
            // The state transitions into `SettingUp`. The `SyncIndicator` must be `Show` as
            // the service has now been started.
            assert_next_sync_indicator!(
                sync_indicator,
                SyncIndicator::Show,
                under DELAY_BEFORE_SHOWING + request_margin,
            );
        }

        barrier.wait().await;

        // Request 3.
        {
            // The state transitions into `Running`. The `SyncIndicator` must be `Hide`.
            assert_next_sync_indicator!(
                sync_indicator,
                SyncIndicator::Hide,
                under DELAY_BEFORE_HIDING + request_margin,
            );
        }

        barrier.wait().await;

        // Request 4.
        {
            // The state transitions into `Error`. The `SyncIndicator` must be `Show`.
            assert_next_sync_indicator!(
                sync_indicator,
                SyncIndicator::Show,
                under DELAY_BEFORE_SHOWING + request_margin,
            );
        }

        barrier.wait().await;

        // Request 5.
        {
            // The state transitions into `Recovering`. The `SyncIndicator` must be `Hide`.
            assert_next_sync_indicator!(
                sync_indicator,
                SyncIndicator::Hide,
                under DELAY_BEFORE_HIDING + request_margin,
            );
        }

        assert_pending!(sync_indicator);
    });

    barrier.wait().await;

    // Request 1.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Init => SettingUp,
        assert request >= {},
        respond with = {
            "pos": "0",
        },
        after delay = request_delay,
    };

    barrier.wait().await;

    // Request 2.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = SettingUp => Running,
        assert request >= {},
        respond with = {
            "pos": "1",
        },
        after delay = request_delay,
    };

    barrier.wait().await;

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
        after delay = request_delay,
    };

    let sync = room_list.sync();
    pin_mut!(sync);

    barrier.wait().await;

    // Request 4.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Error { .. } => Recovering,
        assert request >= {},
        respond with = {
            "pos": "2",
        },
        after delay = request_delay,
    };

    barrier.wait().await;

    // Request 5.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Recovering => Running,
        assert request >= {},
        respond with = {
            "pos": "3",
        },
        after delay = request_delay,
    };

    sync_indicator_task.await.unwrap();

    Ok(())
}

#[async_test]
async fn test_multiple_timeline_init() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_list = RoomListService::new(client.clone()).await.unwrap();

    let sync = room_list.sync();
    pin_mut!(sync);

    let room_id = room_id!("!r0:bar.org");

    let mock_server = server.server();
    sync_then_assert_request_and_fake_response! {
        [mock_server, room_list, sync]
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
                    "prev_batch": "prev-batch-token"
                },
            },
        },
    };

    server.mock_room_state_encryption().plain().mount().await;

    let f = EventFactory::new().room(room_id).sender(*ALICE);

    // Send back-pagination responses with a small delay.
    server
        .mock_room_messages()
        .ok(RoomMessagesResponseTemplate::default()
            .events(vec![f.text_msg("hello").into_raw_timeline()])
            .with_delay(Duration::from_millis(500)))
        .mount()
        .await;

    let task = {
        // Get a RoomListService::Room, initialize the timeline, start a pagination.
        let room = room_list.room(room_id).unwrap();

        let timeline = room.timeline_builder().build().await.unwrap();

        spawn(async move { timeline.paginate_backwards(20).await })
    };

    // Rinse and repeat.
    let room = room_list.room(room_id).unwrap();

    // Let the pagination start in the other timeline, and quickly abort it.
    sleep(Duration::from_millis(200)).await;
    task.abort();

    // A new timeline for the same room can still be constructed.
    room.timeline_builder().build().await.unwrap();
}

#[async_test]
async fn test_thread_subscriptions_extension_enabled_only_if_server_advertises_it() {
    let server = MatrixMockServer::new().await;

    {
        // The first time, don't advertise support for MSC4306; the extension will NOT
        // enabled in this case, despite the client requesting it.
        let features_map = BTreeMap::new();

        server
            .mock_versions()
            .ok_custom(&["v1.11"], &features_map)
            .named("/versions, first time")
            // This used to be a `mock_once()`, but we're not caching the versions in the
            // `RoomListService::new()` method anymore, so we're now doing a couple more requests
            // for this. The sync will want to know about the `/versions` once it tries to build
            // the request path.
            .up_to_n_times(3)
            .expect(3..)
            .mount()
            .await;

        let client = server
            .client_builder()
            .no_server_versions()
            .on_builder(|b| {
                b.with_threading_support(matrix_sdk::ThreadingSupport::Enabled {
                    with_subscriptions: true,
                })
            })
            .build()
            .await;
        let room_list = RoomListService::new(client.clone()).await.unwrap();

        let sync = room_list.sync();
        pin_mut!(sync);

        let room_id = room_id!("!r0:bar.org");

        let mock_server = server.server();
        sync_then_assert_request_and_fake_response! {
            [mock_server, room_list, sync]
            assert request = {
                "conn_id": "room-list",
                "extensions": {
                    "account_data": {
                        "enabled": true,
                    },
                    "receipts": {
                        "enabled": true,
                        "rooms": ["*"],
                    },
                    "typing": {
                        "enabled": true,
                    },
                },
                "lists": {
                    "all_rooms": {
                        "filters": {},
                        "ranges": [ [ 0, 19, ], ],
                        "required_state": [
                            [ "m.room.name", "" ],
                            [ "m.room.encryption", "" ],
                            [ "m.room.member", "$LAZY" ],
                            [ "m.room.member", "$ME" ],
                            [ "m.room.topic", "", ],
                            [ "m.room.avatar", "", ],
                            [ "m.room.canonical_alias", "", ],
                            [ "m.room.power_levels", "", ],
                            [ "org.matrix.msc3401.call.member", "*", ],
                            [ "m.room.join_rules", "", ],
                            [ "m.room.tombstone", "", ],
                            [ "m.room.create", "", ],
                            [ "m.room.history_visibility", "", ],
                            [ "io.element.functional_members", "", ],
                            [ "m.space.parent", "*", ],
                            [ "m.space.child", "*", ],
                        ],
                        "timeline_limit": 1,
                    },
                },
            },
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
                        "prev_batch": "prev-batch-token"
                    },
                },
            },
        };

        mock_server.reset().await;
    }

    // Then, advertise support with support for MSC4306; the extension will be
    // enabled in this case.
    let features_map = BTreeMap::from([("org.matrix.msc4306", true)]);

    server
        .mock_versions()
        .ok_custom(&["v1.11"], &features_map)
        .named("/versions, second time")
        .up_to_n_times(2)
        .mount()
        .await;

    let client = server
        .client_builder()
        .no_server_versions()
        .on_builder(|b| {
            b.with_threading_support(matrix_sdk::ThreadingSupport::Enabled {
                with_subscriptions: true,
            })
        })
        .build()
        .await;
    let room_list = RoomListService::new(client.clone()).await.unwrap();

    let sync = room_list.sync();
    pin_mut!(sync);

    let room_id = room_id!("!r0:bar.org");

    let mock_server = server.server();
    sync_then_assert_request_and_fake_response! {
        [mock_server, room_list, sync]
        assert request = {
            "conn_id": "room-list",
            "extensions": {
                "account_data": {
                    "enabled": true,
                },
                "receipts": {
                    "enabled": true,
                    "rooms": ["*"],
                },
                "typing": {
                    "enabled": true,
                },
                "io.element.msc4308.thread_subscriptions": {
                    "enabled": true,
                    "limit": 10,
                },
            },
            "lists": {
                "all_rooms": {
                    "filters": {},
                    "ranges": [ [ 0, 19, ], ],
                    "required_state": [
                        [ "m.room.name", "" ],
                        [ "m.room.encryption", "" ],
                        [ "m.room.member", "$LAZY" ],
                        [ "m.room.member", "$ME" ],
                        [ "m.room.topic", "", ],
                        [ "m.room.avatar", "", ],
                        [ "m.room.canonical_alias", "", ],
                        [ "m.room.power_levels", "", ],
                        [ "org.matrix.msc3401.call.member", "*", ],
                        [ "m.room.join_rules", "", ],
                        [ "m.room.tombstone", "", ],
                        [ "m.room.create", "", ],
                        [ "m.room.history_visibility", "", ],
                        [ "io.element.functional_members", "", ],
                        [ "m.space.parent", "*", ],
                        [ "m.space.child", "*", ],
                    ],
                    "timeline_limit": 1,
                },
            },
        },
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
                    "prev_batch": "prev-batch-token"
                },
            },
        },
    };
}
