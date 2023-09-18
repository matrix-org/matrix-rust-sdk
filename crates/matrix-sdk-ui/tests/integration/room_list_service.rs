use std::{
    ops::Not,
    time::{Duration, Instant},
};

use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use futures_util::{pin_mut, FutureExt, StreamExt};
use imbl::vector;
use matrix_sdk::Client;
use matrix_sdk_test::async_test;
use matrix_sdk_ui::{
    room_list_service::{
        filters::{new_filter_all, new_filter_fuzzy_match_room_name},
        Error, Input, InputResult, RoomListEntry, RoomListLoadingState, State, SyncIndicator,
        ALL_ROOMS_LIST_NAME as ALL_ROOMS, INVITES_LIST_NAME as INVITES,
        SYNC_INDICATOR_DELAY_BEFORE_HIDING, SYNC_INDICATOR_DELAY_BEFORE_SHOWING,
        VISIBLE_ROOMS_LIST_NAME as VISIBLE_ROOMS,
    },
    timeline::{TimelineItemKind, VirtualTimelineItem},
    RoomListService,
};
use ruma::{
    api::client::sync::sync_events::{v4::RoomSubscription, UnreadNotificationsCount},
    assign, event_id,
    events::{room::message::RoomMessageEventContent, StateEventType},
    mxc_uri, room_id, uint, TransactionId,
};
use serde_json::json;
use stream_assert::{assert_next_matches, assert_pending};
use tokio::{spawn, sync::mpsc::channel, task::yield_now};
use wiremock::MockServer;

use crate::{
    logged_in_client,
    timeline::sliding_sync::{assert_timeline_stream, timeline_event},
};

async fn new_room_list_service() -> Result<(Client, MockServer, RoomListService), Error> {
    let (client, server) = logged_in_client().await;
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

macro_rules! assert_entries_batch {
    // `append [$entries]`
    ( @_ [ $entries:ident ] [ append [ $( $entry:tt )+ ] ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_entries_batch!(
            @_
            [ $entries ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                assert_matches!(
                    $entries.next(),
                    Some(&VectorDiff::Append { ref values }) => {
                        assert_eq!(values, &entries!( $( $entry )+ ));
                    }
                );
            ]
        )
    };

    // `set [$nth] [$entry]`
    ( @_ [ $entries:ident ] [ set [ $index:literal ] [ $( $entry:tt )+ ] ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_entries_batch!(
            @_
            [ $entries ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                assert_matches!(
                    $entries.next(),
                    Some(&VectorDiff::Set { index: $index, ref value }) => {
                        assert_eq!(value, &entries!( $( $entry )+ )[0]);
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
                assert_eq!(
                    $entries.next(),
                    Some(&VectorDiff::Remove { index: $index }),
                );
            ]
        )
    };

    // `insert [$nth] [$entry]`
    ( @_ [ $entries:ident ] [ insert [ $index:literal ] [ $( $entry:tt )+ ] ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_entries_batch!(
            @_
            [ $entries ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                assert_matches!(
                    $entries.next(),
                    Some(&VectorDiff::Insert { index: $index, ref value }) => {
                        assert_eq!(value, &entries!( $( $entry )+ )[0]);
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
                assert_eq!(
                    $entries.next(),
                    Some(&VectorDiff::Truncate { length: $length }),
                );
            ]
        )
    };

    // `reset [$entries]`
    ( @_ [ $entries:ident ] [ reset [ $( $entry:tt )+ ] ; $( $rest:tt )* ] [ $( $accumulator:tt )* ] ) => {
        assert_entries_batch!(
            @_
            [ $entries ]
            [ $( $rest )* ]
            [
                $( $accumulator )*
                assert_matches!(
                    $entries.next(),
                    Some(&VectorDiff::Reset { ref values }) => {
                        assert_eq!(values, &entries!( $( $entry )+ ));
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
                assert_eq!($entries.next(), None);
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
            .expect("stream entry wasn't in the ready state")
            .expect("stream was stopped");

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
                        ["m.room.avatar", ""],
                        ["m.room.encryption", ""],
                        ["m.room.power_levels", ""],
                    ],
                    "filters": {
                        "is_invite": false,
                        "is_tombstoned": false,
                        "not_room_types": ["m.space"],
                    },
                    "bump_event_types": [
                        "m.room.message",
                        "m.room.encrypted",
                        "m.sticker",
                    ],
                    "sort": ["by_recency", "by_name"],
                    "timeline_limit": 1,
                },
                INVITES: {
                    "ranges": [[0, 0]],
                    "required_state": [
                        ["m.room.avatar", ""],
                        ["m.room.encryption", ""],
                        ["m.room.member", "$ME"],
                        ["m.room.canonical_alias", ""],
                    ],
                    "filters": {
                        "is_invite": true,
                        "is_tombstoned": false,
                        "not_room_types": ["m.space"],
                    },
                    "sort": ["by_recency", "by_name"],
                    "timeline_limit": 0,
                },
            },
            "extensions": {
                "account_data": {
                    "enabled": true
                },
                "receipts": {
                    "enabled": true,
                    "rooms": ["*"]
                }
            },
        },
        respond with = {
            "pos": "0",
            "lists": {
                ALL_ROOMS: {
                    "count": 420,
                    "ops": [
                        // let's ignore them for now
                    ],
                },
                INVITES: {
                    "count": 40,
                    "ops": [],
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
                VISIBLE_ROOMS: {
                    "ranges": [[0, 19]],
                    "required_state": [
                        ["m.room.encryption", ""],
                        ["m.room.member", "$LAZY"],
                    ],
                    "filters": {
                        "is_invite": false,
                        "is_tombstoned": false,
                        "not_room_types": ["m.space"],
                    },
                    "bump_event_types": [
                        "m.room.message",
                        "m.room.encrypted",
                        "m.sticker",
                    ],
                    "sort": ["by_recency", "by_name"],
                    "timeline_limit": 20,
                },
                INVITES: {
                    "ranges": [[0, 19]],
                },
            },
        },
        respond with = {
            "pos": "1",
            "lists": {
                ALL_ROOMS: {
                    "count": 420,
                    "ops": [
                        // let's ignore them for now
                    ],
                },
                VISIBLE_ROOMS: {
                    "count": 0,
                    "ops": [],
                },
                INVITES: {
                    "count": 2,
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
        states = Running => Running,
        assert request = {
            "conn_id": "room-list",
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 199]],
                },
                VISIBLE_ROOMS: {
                    "ranges": [[0, 19]],
                },
                INVITES: {
                    "ranges": [[0, 1]],
                }
            },
        },
        respond with = {
            "pos": "2",
            "lists": {
                ALL_ROOMS: {
                    "count": 420,
                    "ops": [
                        // let's ignore them for now
                    ],
                },
                VISIBLE_ROOMS: {
                    "count": 0,
                    "ops": [],
                },
                INVITES: {
                    "count": 3,
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
        states = Running => Running,
        assert request = {
            "conn_id": "room-list",
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 299]],
                },
                VISIBLE_ROOMS: {
                    "ranges": [[0, 19]],
                },
                INVITES: {
                    "ranges": [[0, 2]],
                },
            },
        },
        respond with = {
            "pos": "3",
            "lists": {
                ALL_ROOMS: {
                    "count": 420,
                    "ops": [
                        // let's ignore them for now
                    ],
                },
                VISIBLE_ROOMS: {
                    "count": 0,
                    "ops": [],
                },
                INVITES: {
                    "count": 0,
                    "ops": [],
                }
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
                VISIBLE_ROOMS: {
                    "ranges": [[0, 19]],
                },
                INVITES: {
                    "ranges": [[0, 0]],
                },
            },
        },
        respond with = {
            "pos": "4",
            "lists": {
                ALL_ROOMS: {
                    "count": 420,
                    "ops": [
                        // let's ignore them for now
                    ],
                },
                VISIBLE_ROOMS: {
                    "count": 0,
                    "ops": [],
                },
                INVITES: {
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
            states = SettingUp => Running,
            assert request >= {
                "lists": {
                    ALL_ROOMS: {
                        "ranges": [[0, 9]],
                    },
                    VISIBLE_ROOMS: {
                        "ranges": [[0, 19]],
                    },
                    INVITES: {
                        "ranges": [[0, 19]],
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
                    INVITES: {
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
            states = Running => Running,
            assert request >= {
                "lists": {
                    ALL_ROOMS: {
                        "ranges": [[0, 9]],
                    },
                    VISIBLE_ROOMS: {
                        "ranges": [[0, 19]],
                    },
                    INVITES: {
                        "ranges": [[0, 0]],
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
                    INVITES: {
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
                INVITES: {
                    // The default range, in selective sync-mode.
                    "ranges": [[0, 0]],
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
                INVITES: {
                    // Stil the default range, in selective sync-mode.
                    "ranges": [[0, 0]],
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
                VISIBLE_ROOMS: {
                    // Hello new list.
                    "ranges": [[0, 19]],
                    "timeline_limit": 20,
                },
                INVITES: {
                    // The sync-mode has changed to growing, with its initial range.
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

    // Update the viewport, just to be sure it's not reset later.
    assert_eq!(room_list.apply_input(Input::Viewport(vec![5..=10])).await?, InputResult::Applied);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Error { .. } => Recovering,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // Due to previous error, the sync-mode is back to selective, with its initial range.
                    "ranges": [[0, 19]],
                },
                VISIBLE_ROOMS: {
                    // We have set a viewport, which reflects here.
                    "ranges": [[5, 10]],
                    // The `timeline_limit` has been set to zero.
                    "timeline_limit": 0,
                },
                INVITES: {
                    // Due to previous error, the sync-mode is back to selective, with its initial range.
                    "ranges": [[0, 0]],
                },
            },
        },
        respond with = {
            "pos": "2",
            "lists": {
                ALL_ROOMS: {
                    "count": 210,
                },
                INVITES: {
                    "count": 30,
                }
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
                VISIBLE_ROOMS: {
                    // Viewport hasn't changed.
                    "ranges": [[5, 10]],
                    // The `timeline_limit` has been restored.
                    "timeline_limit": 20,
                },
                INVITES: {
                    // Sync-mode is now growing.
                    "ranges": [[0, 19]],
                },
            },
        },
        respond with = {
            "pos": "3",
            "lists": {
                ALL_ROOMS: {
                    "count": 210,
                },
                INVITES: {
                    "count": 30,
                }
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
                VISIBLE_ROOMS: {
                    // The range is kept.
                    "ranges": [[5, 10]],
                    // `timeline_limit` is a sticky parameter, so it's absent now.
                },
                INVITES: {
                    // The sync-mode is still growing, and the range has made progress.
                    "ranges": [[0, 29]],
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
                VISIBLE_ROOMS: {
                    // We have set a viewport, which reflects here.
                    "ranges": [[5, 10]],
                    // The `timeline_limit` has been set to 0.
                    "timeline_limit": 0,
                },
                INVITES: {
                    // Due to previous error, the range has been reset.
                    "ranges": [[0, 0]],
                },
            },
        },
        respond with = {
            "pos": "4",
            "lists": {
                ALL_ROOMS: {
                    "count": 210,
                },
                INVITES: {
                    "count": 4,
                }
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
                VISIBLE_ROOMS: {
                    // Viewport hasn't changed.
                    "ranges": [[5, 10]],
                    // The `timeline_limit` has been restored.
                    "timeline_limit": 20,
                },
                INVITES: {
                    // The sync-mode is now growing, and has reached its maximum.
                    "ranges": [[0, 3]],
                },
            },
        },
        respond with = {
            "pos": "5",
            "lists": {
                ALL_ROOMS: {
                    "count": 210,
                },
                INVITES: {
                    "count": 4,
                }
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
                VISIBLE_ROOMS: {
                    // No error. The range is still here.
                    "ranges": [[5, 10]],
                    // `timeline_limit` is a sticky parameter, so it's absent now.
                },
                INVITES: {
                    // The range has reached its maximum.
                    "ranges": [[0, 3]],
                },
            },
        },
        respond with = {
            "pos": "6",
            "lists": {
                ALL_ROOMS: {
                    "count": 210,
                },
                INVITES: {
                    "count": 7,
                }
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
                VISIBLE_ROOMS: {
                    // The range is still here.
                    "ranges": [[5, 10]],
                    // `timeline_limit` is a sticky parameter, so it's absent now.
                },
                INVITES: {
                    // The range has reached a new maximum.
                    "ranges": [[0, 6]],
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
                VISIBLE_ROOMS: {
                    // We have set a viewport, which reflects here.
                    "ranges": [[5, 10]],
                    // The `timeline_limit` has been set to 0.
                    "timeline_limit": 0,
                },
                INVITES: {
                    // Due to previous error, the range is back to selective, with its initial range.
                    "ranges": [[0, 0]],
                },
            },
        },
        respond with = {
            "pos": "7",
            "lists": {
                ALL_ROOMS: {
                    "count": 210,
                },
                INVITES: {
                    "count": 4,
                }
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
                VISIBLE_ROOMS: {
                    // Viewport hasn't changed.
                    "ranges": [[5, 10]],
                    // The `timeline_limit` has been restored.
                    "timeline_limit": 20,
                },
                INVITES: {
                    // Sync-mode is now growing, and has reached its maximum.
                    "ranges": [[0, 3]],
                },
            },
        },
        respond with = {
            "pos": "8",
            "lists": {
                ALL_ROOMS: {
                    "count": 210,
                },
                INVITES: {
                    "count": 4,
                }
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
                INVITES: {
                    // The default range, in selective sync-mode.
                    "ranges": [[0, 0]],
                }
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
                VISIBLE_ROOMS: {
                    // Hello new list.
                    "ranges": [[0, 19]],
                    // `timeline_limit` has been set to zero.
                    "timeline_limit": 0,
                },
                INVITES: {
                    // The sync-mode is still selective, with its initial range.
                    "ranges": [[0, 0]],
                },
            },
        },
        respond with = {
            "pos": "1",
            "lists": {
                ALL_ROOMS: {
                    "count": 150,
                },
                INVITES: {
                    "count": 30,
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
                VISIBLE_ROOMS: {
                    "ranges": [[0, 19]],
                    // `timeline_limit` has been restored.
                    "timeline_limit": 20,
                },
                INVITES: {
                    // The sync-mode is now growing, with its initial range.
                    "ranges": [[0, 19]],
                },
            },
        },
        respond with = {
            "pos": "2",
            "lists": {
                ALL_ROOMS: {
                    "count": 150,
                },
                INVITES: {
                    "count": 3,
                }
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

    // Update the viewport, just to be sure it's not reset later.
    assert_eq!(room_list.apply_input(Input::Viewport(vec![5..=10])).await?, InputResult::Applied);

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
                VISIBLE_ROOMS: {
                    // We have set a viewport, which reflects here.
                    "ranges": [[5, 10]],
                    // `timeline_limit` has been set to zero.
                    "timeline_limit": 0,
                },
                INVITES: {
                    // The sync-mode is back to selective..
                    "ranges": [[0, 0]],
                },
            },
        },
        respond with = {
            "pos": "3",
            "lists": {
                ALL_ROOMS: {
                    "count": 150,
                },
                INVITES: {
                    "count": 4,
                }
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
                VISIBLE_ROOMS: {
                    // We have set a viewport, which reflects here.
                    "ranges": [[5, 10]],
                    // `timeline_limit` has been restored.
                    "timeline_limit": 20,
                },
                INVITES: {
                    // Sync-mode is growing, and has reached its maximum.
                    "ranges": [[0, 3]],
                },
            },
        },
        respond with = {
            "pos": "4",
            "lists": {
                ALL_ROOMS: {
                    "count": 150,
                },
                INVITES: {
                    "count": 4,
                }
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
                VISIBLE_ROOMS: {
                    // We have set a viewport, which reflects here.
                    "ranges": [[5, 10]],
                    // `timeline_limit` is a sticky parameter, so it's absent now.
                },
                INVITES: {
                    // Range has reached its maximum.
                    "ranges": [[0, 3]],
                },
            },
        },
        respond with = {
            "pos": "5",
            "lists": {
                ALL_ROOMS: {
                    "count": 150,
                },
                INVITES: {
                    "count": 4,
                }
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
                },
            },
        },
    };

    assert!(previous_entries.is_empty());
    assert_entries_batch! {
        [entries_stream]
        append [ E, E, E, E, E, E, E, E, E, E ];
        set[0] [ F("!r0:bar.org") ];
        set[1] [ F("!r1:bar.org") ];
        set[2] [ F("!r2:bar.org") ];
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
                VISIBLE_ROOMS: {
                    "ranges": [[0, 19]],
                },
                INVITES: {
                    "ranges": [[0, 19]],
                },
            },
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
                            "room_id": "!r3:bar.org",
                        },
                    ],
                },
                VISIBLE_ROOMS: {
                    "count": 0,
                },
                INVITES: {
                    "count": 0,
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

    assert_entries_batch! {
        [entries_stream]
        remove[1];
        remove[0];
        insert[0] [ F("!r3:bar.org") ];
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

    let (dynamic_entries_stream, dynamic_entries) = all_rooms.entries_with_dynamic_adapters(5);
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
                    "name": "Matrix Foobar",
                    "initial": true,
                    "timeline": [],
                },
            },
        },
    };

    // Ensure the dynamic entries' stream is pending because there is no filter set
    // yet.
    assert_pending!(dynamic_entries_stream);

    // Now, let's define a filter.
    dynamic_entries.set_filter(new_filter_fuzzy_match_room_name(&client, "mat ba"));

    // Assert the dynamic entries.
    assert_entries_batch! {
        [dynamic_entries_stream]
        // Receive a `reset` because the filter has been reset/set for the first time.
        reset [ F("!r0:bar.org") ];
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
                VISIBLE_ROOMS: {
                    "ranges": [[0, 19]],
                },
                INVITES: {
                    "ranges": [[0, 19]],
                },
            },
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
                                "!r2:bar.org",
                                "!r3:bar.org",
                                "!r4:bar.org",
                            ],
                        },
                    ],
                },
                VISIBLE_ROOMS: {
                    "count": 0,
                },
                INVITES: {
                    "count": 0,
                },
            },
            "rooms": {
                "!r1:bar.org": {
                    "name": "Matrix Bar",
                    "initial": true,
                    "timeline": [],
                },
                "!r2:bar.org": {
                    "name": "Hello",
                    "initial": true,
                    "timeline": [],
                },
                "!r3:bar.org": {
                    "name": "Helios live",
                    "initial": true,
                    "timeline": [],
                },
                "!r4:bar.org": {
                    "name": "Matrix Baz",
                    "initial": true,
                    "timeline": [],
                },
            },
        },
    };

    // Assert the dynamic entries.
    assert_entries_batch! {
        [dynamic_entries_stream]
        insert[1] [ F("!r1:bar.org") ];
        insert[2] [ F("!r4:bar.org") ];
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
                VISIBLE_ROOMS: {
                    "ranges": [[0, 19]],
                },
                INVITES: {
                    "ranges": [[0, 0]],
                },
            },
        },
        respond with = {
            "pos": "2",
            "lists": {
                ALL_ROOMS: {
                    "count": 10,
                    "ops": [
                        {
                            "op": "SYNC",
                            "range": [5, 7],
                            "room_ids": [
                                "!r5:bar.org",
                                "!r6:bar.org",
                                "!r7:bar.org",
                            ],
                        },
                    ],
                },
                VISIBLE_ROOMS: {
                    "count": 0,
                },
                INVITES: {
                    "count": 0,
                },
            },
            "rooms": {
                "!r5:bar.org": {
                    "name": "Matrix Barracuda Room",
                    "initial": true,
                    "timeline": [],
                },
                "!r6:bar.org": {
                    "name": "Matrix is real as hell",
                    "initial": true,
                    "timeline": [],
                },
                "!r7:bar.org": {
                    "name": "Matrix Baraka",
                    "initial": true,
                    "timeline": [],
                },
            },
        },
    };

    // Assert the dynamic entries.
    assert_entries_batch! {
        [dynamic_entries_stream]
        insert[3] [ F("!r5:bar.org") ];
        insert[4] [ F("!r7:bar.org") ];
        end;
    };
    assert_pending!(dynamic_entries_stream);

    // Now, let's change the dynamic entries!
    dynamic_entries.set_filter(new_filter_fuzzy_match_room_name(&client, "hell"));

    // Assert the dynamic entries.
    assert_entries_batch! {
        [dynamic_entries_stream]
        // Receive a `reset` again because the filter has been reset.
        reset [ F("!r2:bar.org"), F("!r3:bar.org"), F("!r6:bar.org") ];
        end;
    }
    assert_pending!(dynamic_entries_stream);

    // Now, let's change again the dynamic filter!
    dynamic_entries.set_filter(new_filter_all());

    // Assert the dynamic entries.
    assert_entries_batch! {
        [dynamic_entries_stream]
        // Receive a `reset` again because the filter has been reset.
        reset [
            F("!r0:bar.org"),
            F("!r1:bar.org"),
            F("!r2:bar.org"),
            F("!r3:bar.org"),
            F("!r4:bar.org"),
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
            F("!r5:bar.org"),
            F("!r6:bar.org"),
            F("!r7:bar.org"),
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

    // Let's ask one more page again, because it's fun.
    dynamic_entries.add_one_page();

    // Assert the dynamic entries.
    assert_entries_batch! {
        [dynamic_entries_stream]
        // Receive the next values.
        append [
            F("!r5:bar.org"),
            F("!r6:bar.org"),
            F("!r7:bar.org"),
        ];
        end;
    };
    assert_pending!(dynamic_entries_stream);

    Ok(())
}

#[async_test]
async fn test_invites_stream() -> Result<(), Error> {
    let (_, server, room_list) = new_room_list_service().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    // The invites is accessible from the beginning.
    assert!(room_list.invites().await.is_ok());

    let room_id_0 = room_id!("!r0:bar.org");

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Init => SettingUp,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 19]],
                },
                INVITES: {
                    "ranges": [[0, 0]],
                },
            },
        },
        respond with = {
            "pos": "0",
            "lists": {
                ALL_ROOMS: {
                    "count": 0,
                },
                INVITES: {
                    "count": 1,
                    "ops": [
                        {
                            "op": "SYNC",
                            "range": [0, 0],
                            "room_ids": [
                                room_id_0,
                            ],
                        },
                    ],
                }
            },
            "rooms": {
                room_id_0: {
                    "name": "Invitation for Room #0",
                    "initial": true,
                },
            },
        },
    };

    let invites = room_list.invites().await?;

    let (previous_invites, invites_stream) = invites.entries();
    pin_mut!(invites_stream);

    assert_eq!(previous_invites.len(), 1);
    assert_matches!(&previous_invites[0], RoomListEntry::Filled(room_id) => {
        assert_eq!(room_id, room_id_0);
    });

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = SettingUp => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 0]],
                },
                VISIBLE_ROOMS: {
                    "ranges": [[0, 19]],
                },
                INVITES: {
                    "ranges": [[0, 0]],
                },
            },
        },
        respond with = {
            "pos": "2",
            "lists": {
                ALL_ROOMS: {
                    "count": 0,
                },
                VISIBLE_ROOMS: {
                    "count": 0,
                },
                INVITES: {
                    "count": 1,
                    "ops": [
                        {
                            "op": "DELETE",
                            "index": 0,
                        },
                        {

                            "op": "INSERT",
                            "index": 0,
                            "room_id": "!r1:bar.org",
                        },
                    ],
                },
            },
            "rooms": {
                "!r1:bar.org": {
                    "name": "Invitation for Room #1",
                    "initial": true,
                },
            },
        },
    };

    assert_entries_batch! {
        [invites_stream]
        remove[0];
        insert[0] [ F("!r1:bar.org") ];
        end;
    };

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
                    "avatar": "mxc://homeserver/media",
                    "initial": true,
                },
                room_id_1: {
                    "initial": true,
                },
            },
        },
    };

    let room0 = room_list.room(room_id_0).await?;

    // Room has received a name from sliding sync.
    assert_eq!(room0.name().await, Some("Room #0".to_owned()));

    // Room has received an avatar from sliding sync.
    assert_eq!(room0.avatar_url(), Some(mxc_uri!("mxc://homeserver/media").to_owned()));

    let room1 = room_list.room(room_id_1).await?;

    // Room has not received a name from sliding sync, then it's calculated.
    assert_eq!(room1.name().await, Some("Empty Room".to_owned()));

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
                    "avatar": "mxc://homeserver/other-media",
                },
            },
        },
    };

    // Room has _now_ received a name from sliding sync!
    assert_eq!(room1.name().await, Some("Room #1".to_owned()));

    // Room has _now_ received an avatar URL from sliding sync!
    assert_eq!(room1.avatar_url(), Some(mxc_uri!("mxc://homeserver/other-media").to_owned()));

    Ok(())
}

#[async_test]
async fn test_room_not_found() -> Result<(), Error> {
    let (_, _, room_list) = new_room_list_service().await?;

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
                    "ops": [
                        {
                            "op": "SYNC",
                            "range": [0, 2],
                            "room_ids": [
                                room_id_0,
                                room_id_1,
                                room_id_2,
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
                    "name": "Room #1",
                    "initial": true,
                },
                room_id_2: {
                    "name": "Room #2",
                    "initial": true,
                }
            },
        },
    };

    let room1 = room_list.room(room_id_1).await.unwrap();

    // Subscribe.

    room1.subscribe(Some(assign!(RoomSubscription::default(), {
        required_state: vec![
            (StateEventType::RoomName, "".to_owned()),
            (StateEventType::RoomTopic, "".to_owned()),
            (StateEventType::RoomAvatar, "".to_owned()),
            (StateEventType::RoomCanonicalAlias, "".to_owned()),
        ],
        timeline_limit: Some(uint!(30)),
    })));

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

    // Unsubscribe.

    room1.unsubscribe();
    room_list.room(room_id_2).await?.unsubscribe(); // unsubscribe from a room that has no subscription.

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 2]],
                },
            },
            "unsubscribe_rooms": [room_id_1, /* `room_id_2` is absent */],
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

    let room = room_list.room(room_id).await.unwrap();

    assert!(room.has_unread_notifications().not());
    assert_matches!(
        room.unread_notifications(),
        UnreadNotificationsCount { highlight_count: None, notification_count: None, .. }
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

    assert!(room.has_unread_notifications());
    assert_matches!(
        room.unread_notifications(),
        UnreadNotificationsCount {
            highlight_count,
            notification_count,
            ..
        } => {
            assert_eq!(highlight_count, Some(uint!(1)));
            assert_eq!(notification_count, Some(uint!(2)));
        }
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
    let latest_event = assert_matches!(
        room.latest_event().await,
        Some(event) => {
            assert!(event.is_local_echo().not());
            assert_eq!(event.event_id(), Some(event_id!("$x1:bar.org")));

            event
        }
    );

    // Now let's compare with the result of the `Timeline`.
    let timeline = room.timeline().await;

    // The latest event matches the latest event of the `Timeline`.
    assert_matches!(
        timeline.latest_event().await,
        Some(timeline_event) => {
            assert_eq!(timeline_event.event_id(), latest_event.event_id());
        }
    );

    // Insert a local event in the `Timeline`.
    let txn_id: &TransactionId = "foobar-txn-id".into();

    timeline.send(RoomMessageEventContent::text_plain("Hello, World!").into(), Some(txn_id)).await;

    // The latest event of the `Timeline` is a local event.
    assert_matches!(
        timeline.latest_event().await,
        Some(timeline_event) => {
            assert!(timeline_event.is_local_echo());
            assert_eq!(timeline_event.event_id(), None);
            assert_eq!(timeline_event.transaction_id(), Some(txn_id));
        }
    );

    // The latest event is a local event.
    assert_matches!(
        room.latest_event().await,
        Some(event) => {
            assert!(event.is_local_echo());
            assert_eq!(event.event_id(), None);
            assert_eq!(event.transaction_id(), Some(txn_id));
        }
    );

    Ok(())
}

#[async_test]
async fn test_input_viewport() -> Result<(), Error> {
    let (_, server, room_list) = new_room_list_service().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    // The input cannot be applied because the `VISIBLE_ROOMS_LIST_NAME` list isn't
    // present.
    assert_matches!(
        room_list.apply_input(Input::Viewport(vec![10..=15])).await,
        Err(Error::InputCannotBeApplied(_))
    );

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
            "lists": {},
            "rooms": {},
        },
    };

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = SettingUp => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 99]],
                },
                VISIBLE_ROOMS: {
                    "ranges": [[0, 19]],
                    "timeline_limit": 20,
                },
                INVITES: {
                    "ranges": [[0, 19]],
                },
            },
        },
        respond with = {
            "pos": "1",
            "lists": {},
            "rooms": {},
        },
    };

    // Now we can change the viewport..
    assert_eq!(
        room_list.apply_input(Input::Viewport(vec![10..=15, 20..=25])).await?,
        InputResult::Applied
    );

    // Re-changing the viewport has no effect.
    assert_eq!(
        room_list.apply_input(Input::Viewport(vec![10..=15, 20..=25])).await?,
        InputResult::Ignored
    );

    // The `timeline_limit` is not repeated because it's sticky.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Running => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 99]],
                },
                VISIBLE_ROOMS: {
                    "ranges": [[10, 15], [20, 25]],
                },
                INVITES: {
                    "ranges": [[0, 19]],
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
#[ignore] // flaky
async fn test_sync_indicator() -> Result<(), Error> {
    let (_, server, room_list) = new_room_list_service().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    let sync_indicator = room_list.sync_indicator();

    let request_margin = Duration::from_millis(100);
    let request_1_delay = SYNC_INDICATOR_DELAY_BEFORE_SHOWING * 2;
    let request_2_delay = SYNC_INDICATOR_DELAY_BEFORE_SHOWING * 3;
    let request_4_delay = SYNC_INDICATOR_DELAY_BEFORE_SHOWING * 2;
    let request_5_delay = SYNC_INDICATOR_DELAY_BEFORE_SHOWING * 2;

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
                under SYNC_INDICATOR_DELAY_BEFORE_SHOWING + request_margin,
            );

            // Then, once the sync is done, the `SyncIndicator` must be hidden.
            assert_next_sync_indicator!(
                sync_indicator,
                SyncIndicator::Hide,
                under request_1_delay - SYNC_INDICATOR_DELAY_BEFORE_SHOWING
                    + SYNC_INDICATOR_DELAY_BEFORE_HIDING
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
                under SYNC_INDICATOR_DELAY_BEFORE_SHOWING + request_margin,
            );
        }

        in_between_requests_synchronizer.recv().await.unwrap();
        assert_pending!(sync_indicator);

        // Request 5.
        {
            // It takes time for the system to recover…
            assert_next_sync_indicator!(
                sync_indicator,
                SyncIndicator::Show,
                under SYNC_INDICATOR_DELAY_BEFORE_SHOWING + request_margin,
            );

            // But finally, the system has recovered and is running. Time to hide the
            // `SyncIndicator`.
            assert_next_sync_indicator!(
                sync_indicator,
                SyncIndicator::Hide,
                under request_5_delay - SYNC_INDICATOR_DELAY_BEFORE_SHOWING
                    + SYNC_INDICATOR_DELAY_BEFORE_HIDING
                    + request_margin,
            );
        }
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

    sync_indicator_task.await.unwrap();

    Ok(())
}
