use std::ops::Not;

use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use futures_util::{pin_mut, FutureExt, StreamExt};
use imbl::vector;
use matrix_sdk_test::async_test;
use matrix_sdk_ui::{
    room_list::{
        EntriesLoadingState, Error, Input, RoomListEntry, State, ALL_ROOMS_LIST_NAME as ALL_ROOMS,
        INVITES_LIST_NAME as INVITES, VISIBLE_ROOMS_LIST_NAME as VISIBLE_ROOMS,
    },
    timeline::{TimelineItem, VirtualTimelineItem},
    RoomList,
};
use ruma::{
    api::client::sync::sync_events::{v4::RoomSubscription, UnreadNotificationsCount},
    assign, event_id,
    events::StateEventType,
    room_id, uint,
};
use serde_json::json;
use stream_assert::{assert_next_eq, assert_pending};
use wiremock::MockServer;

use crate::{
    logged_in_client,
    timeline::sliding_sync::{assert_timeline_stream, timeline_event},
};

async fn new_room_list() -> Result<(MockServer, RoomList), Error> {
    let (client, server) = logged_in_client().await;
    let room_list = RoomList::new(client).await?;

    Ok((server, room_list))
}

// Same macro as in the main, with additional checking that the state
// before/after the sync loop match those we expect.
macro_rules! sync_then_assert_request_and_fake_response {
    (
        [$server:ident, $room_list:ident, $stream:ident]
        $( states = $pre_state:pat => $post_state:pat, )?
        assert request >= { $( $request_json:tt )* },
        respond with = $( ( code $code:expr ) )? { $( $response_json:tt )* }
        $(,)?
    ) => {
        sync_then_assert_request_and_fake_response! {
            [$server, $room_list, $stream]
            sync matches Some(Ok(_)),
            $( states = $pre_state => $post_state, )?
            assert request >= { $( $request_json )* },
            respond with = $( ( code $code ) )? { $( $response_json )* },
        }
    };

    (
        [$server:ident, $room_list:ident, $stream:ident]
        sync matches $sync_result:pat,
        $( states = $pre_state:pat => $post_state:pat, )?
        assert request >= { $( $request_json:tt )* },
        respond with = $( ( code $code:expr ) )? { $( $response_json:tt )* }
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
                assert request >= { $( $request_json )* },
                respond with = $( ( code $code ) )? { $( $response_json )* },
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
async fn test_sync_all_states() -> Result<(), Error> {
    let (server, room_list) = new_room_list().await?;

    let (entries_loading_state, mut entries_loading_state_stream) =
        room_list.entries_loading_state().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    assert_eq!(entries_loading_state, EntriesLoadingState::NotLoaded);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Init => SettingUp,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [
                        [0, 19],
                    ],
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

    assert_next_eq!(
        entries_loading_state_stream,
        // It's `FullyLoaded` because it was a `Selective` sync-mode.
        EntriesLoadingState::FullyLoaded
    );

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = SettingUp => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [
                        [0, 49],
                    ],
                },
                VISIBLE_ROOMS: {
                    "ranges": [[0, 19]],
                    "required_state": [
                        ["m.room.encryption", ""],
                    ],
                    "filters": {
                        "is_invite": false,
                        "is_tombstoned": false,
                        "not_room_types": ["m.space"],
                    },
                    "sort": ["by_recency", "by_name"],
                    "timeline_limit": 20,
                },
                INVITES: {
                    "ranges": [[0, 99]],
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

    assert_next_eq!(
        entries_loading_state_stream,
        // It's `PartiallyLoaded` because it's in `Growing` sync-mode,
        // and it's not finished.
        EntriesLoadingState::PartiallyLoaded
    );

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Running => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 99]],
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
                    "count": 200,
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

    assert_pending!(entries_loading_state_stream);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Running => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 149]],
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

    assert_pending!(entries_loading_state_stream);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Running => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 199]],
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
                    "count": 200,
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

    assert_next_eq!(
        entries_loading_state_stream,
        // Finally, it's `FullyLoaded`!.
        EntriesLoadingState::FullyLoaded
    );

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
                        "ranges": [[0, 99]],
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
async fn test_sync_resumes_from_terminated() -> Result<(), Error> {
    let (server, room_list) = new_room_list().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    // Simulate an error from the `Init` state.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        sync matches Some(Err(_)),
        states = Init => Terminated { .. },
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

    // Do a regular sync from the `Terminated` state.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Terminated { .. } => SettingUp,
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
                    "count": 110,
                },
            },
            "rooms": {},
        },
    };

    // Simulate an error from the `SettingUp` state.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        sync matches Some(Err(_)),
        states = SettingUp => Terminated { .. },
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // In `SettingUp`, the sync-mode has changed to growing, with
                    // its initial range.
                    "ranges": [[0, 49]],
                },
                VISIBLE_ROOMS: {
                    // Hello new list.
                    "ranges": [[0, 19]],
                },
                INVITES: {
                    // Hello new list.
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

    // Update the viewport, just to be sure it's not reset later.
    room_list.apply_input(Input::Viewport(vec![5..=10])).await?;

    // Do a regular sync from the `Terminated` state.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Terminated { .. } => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // In `Running`, the sync-mode is still growing, but the range
                    // hasn't been modified due to previous error.
                    "ranges": [[0, 49]],
                },
                VISIBLE_ROOMS: {
                    // We have set a viewport, which reflects here.
                    "ranges": [[5, 10]],
                },
                INVITES: {
                    // The range hasn't been modified due to previous error.
                    "ranges": [[0, 99]],
                },
            },
        },
        respond with = {
            "pos": "2",
            "lists": {
                ALL_ROOMS: {
                    "count": 110,
                },
                INVITES: {
                    "count": 3,
                }
            },
            "rooms": {},
        },
    };

    // Simulate an error from the `Running` state.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        sync matches Some(Err(_)),
        states = Running => Terminated { .. },
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // In `Running`, the sync-mode is still growing, and the range
                    // has made progress.
                    "ranges": [[0, 99]],
                },
                VISIBLE_ROOMS: {
                    // Despites the error, the range is kept.
                    "ranges": [[5, 10]],
                },
                INVITES: {
                    // Despites the error, the range has made progress.
                    "ranges": [[0, 2]],
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
        states = Terminated { .. } => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // Due to the error, the range is reset to its initial value.
                    "ranges": [[0, 49]],
                },
                VISIBLE_ROOMS: {
                    // Despites the error, the range is kept.
                    "ranges": [[5, 10]],
                },
                INVITES: {
                    // Despites the error, the range is kept.
                    "ranges": [[0, 2]],
                }
            },
        },
        respond with = {
            "pos": "3",
            "lists": {
                ALL_ROOMS: {
                    "count": 110,
                },
                INVITES: {
                    "count": 0,
                },
            },
            "rooms": {},
        },
    };

    // Do a regular sync from the `Running` state to update the `ALL_ROOMS` list
    // again.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Running => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    // No error. The range is making progress.
                    "ranges": [[0, 99]],
                },
                VISIBLE_ROOMS: {
                    // No error. The range is still here.
                    "ranges": [[5, 10]],
                },
                INVITES: {
                    // The range is making progress.
                    "ranges": [[0, 0]],
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

    // Simulate an error from the `Running` state.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        sync matches Some(Err(_)),
        states = Running => Terminated { .. },
        assert request >= {
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
                INVITES: {
                    // The range is kept as it was.
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

    // Do a regular sync from the `Terminated` state.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Terminated { .. } => Running,
        assert request >= {
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
                INVITES: {
                    // The range is kept as it was.
                    "ranges": [[0, 0]],
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
async fn test_sync_can_be_stopped() -> Result<(), Error> {
    let (server, room_list) = new_room_list().await?;

    // Let's stop the sync before actually syncing (we never know!).
    // We get an error, obviously.
    assert!(room_list.stop_sync().is_err());

    let sync = room_list.sync();
    pin_mut!(sync);

    // First sync, everything is alright and up and running.
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

    // Now let's stop the `sync`.
    room_list.stop_sync()?;

    // `sync` doesn't provide more value.
    assert!(sync.next().await.is_none());

    // If a new sync is started, it will resume from a `Terminated` state.
    let sync = room_list.sync();
    pin_mut!(sync);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Terminated { .. } => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 9]],
                },
                VISIBLE_ROOMS: {
                    "ranges": [[0, 19]],
                },
                INVITES: {
                    "ranges": [[0, 99]],
                },
            },
        },
        respond with = {
            "pos": "1",
            "lists": {
                ALL_ROOMS: {
                    "count": 0,
                },
                VISIBLE_ROOMS: {
                    "count": 0,
                },
                INVITES: {
                    "count": 0,
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
                    "ranges": [[0, 99]],
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

    assert_entries_stream! {
        [entries_stream]
        remove[1];
        remove[0];
        insert[0] [ F("!r3:bar.org") ];
        pending;
    };

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
                    "ranges": [[0, 99]],
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
                                "!r2:qux.org",
                                "!r3:qux.org",
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
async fn test_invites_stream() -> Result<(), Error> {
    let (server, room_list) = new_room_list().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    // The invites aren't accessible yet.
    assert!(room_list.invites().await.is_err());

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
                    "count": 0,
                },
            },
            "rooms": {},
        },
    };

    // The invites aren't accessible yet.
    assert!(room_list.invites().await.is_err());

    let room_id_0 = room_id!("!r0:bar.org");

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
                    "ranges": [[0, 99]],
                },
            },
        },
        respond with = {
            "pos": "1",
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
                            "op": "SYNC",
                            "range": [0, 0],
                            "room_ids": [
                                room_id_0,
                            ],
                        },
                    ],
                },
            },
            "rooms": {
                room_id_0: {
                    "name": "Invitation for Room #0",
                    "initial": true,
                },
            },
        },
    };

    let (previous_invites, invites_stream) = room_list.invites().await?;
    pin_mut!(invites_stream);

    assert_eq!(previous_invites.len(), 1);
    assert_matches!(&previous_invites[0], RoomListEntry::Filled(room_id) => {
        assert_eq!(room_id, room_id_0);
    });

    assert_entries_stream! {
        [invites_stream]
        pending;
    };

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Running => Running,
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

    assert_entries_stream! {
        [invites_stream]
        remove[0];
        insert[0] [ F("!r1:bar.org") ];
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
async fn test_room_subscription() -> Result<(), Error> {
    let (server, room_list) = new_room_list().await?;

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
    let (server, room_list) = new_room_list().await?;

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
    let (server, room_list) = new_room_list().await?;

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
        assert request >= {},
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
                    "ranges": [[0, 49]],
                },
                VISIBLE_ROOMS: {
                    "ranges": [[0, 19]],
                    "timeline_limit": 20,
                },
                INVITES: {
                    "ranges": [[0, 99]],
                },
            },
        },
        respond with = {
            "pos": "1",
            "lists": {},
            "rooms": {},
        },
    };

    assert!(room_list.apply_input(Input::Viewport(vec![10..=15, 20..=25])).await.is_ok());

    // The `timeline_limit` is not repeated because it's sticky.
    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        states = Running => Running,
        assert request >= {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [[0, 49]],
                },
                VISIBLE_ROOMS: {
                    "ranges": [[10, 15], [20, 25]],
                },
                INVITES: {
                    "ranges": [[0, 99]],
                },
            },
        },
        respond with = {
            "pos": "1",
            "lists": {},
            "rooms": {},
        },
    };

    Ok(())
}
