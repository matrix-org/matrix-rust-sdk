use std::ops::DerefMut;

use eyeball_im::VectorDiff;
use futures_util::{pin_mut, StreamExt};
use matrix_sdk_test::async_test;
use matrix_sdk_ui::{
    room_list::{
        Error, RoomListEntry, State, ALL_ROOMS_LIST_NAME as ALL_ROOMS,
        VISIBLE_ROOMS_LIST_NAME as VISIBLE_ROOMS,
    },
    RoomList,
};
use ruma::room_id;
use serde_json::json;
use stream_assert::*;
use tokio::{spawn, sync::mpsc::unbounded_channel};
use wiremock::{http::Method, Match, Mock, MockServer, Request, ResponseTemplate};

use crate::logged_in_client;

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
        sync once,
        states = $state_0:ident $( -> $state_n:ident )+,
        assert request = { $( $request_json:tt )* },
        respond with = { $( $response_json:tt )* }
        $(,)?
    ) => {
        {
            let _mock_guard = Mock::given(SlidingSyncMatcher)
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    json!({ $( $response_json )* })
                ))
                .mount_as_scoped(&$server)
                .await;

            assert_eq!(State:: $state_0, $room_list.state(), "pre state");

            let mut states = $room_list.state_stream();
            let (state_sender, mut state_receiver) = unbounded_channel();

            let state_listener = spawn(async move {
                while let Some(state) = states.next().await {
                    state_sender.send(state).expect("sending state failed");
                }
            });

            let next = $room_list_sync_stream.next().await.unwrap()?;

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

            $(
                assert_eq!(
                    State:: $state_n ,
                    state_receiver.recv().await.expect("receiving state failed"),
                    "next state",
                );
            )+

            state_listener.abort();

            next
        }
    };
}

#[async_test]
async fn test_init_to_enjoy() -> Result<(), Error> {
    let (server, room_list) = new_room_list().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        sync once,
        states = Init -> LoadFirstRooms -> LoadAllRooms,
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [
                        [0, 19],
                    ],
                    "required_state": [
                        ["m.room.encryption", ""],
                    ],
                    "sort": ["by_recency", "by_name"],
                },
            },
            "extensions": {
                "e2ee": {
                    "enabled": true,
                },
                "to_device": {
                    "enabled": true,
                },
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
        sync once,
        states = LoadAllRooms -> Enjoy,
        assert request = {
            "lists": {
                ALL_ROOMS: {
                    "ranges": [
                        [0, 49],
                    ],
                },
                VISIBLE_ROOMS: {
                    "ranges": [],
                    "required_state": [
                        ["m.room.encryption", ""],
                    ],
                    "sort": ["by_recency", "by_name"],
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
        sync once,
        states = Enjoy -> Enjoy,
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

    Ok(())
}

#[async_test]
async fn test_entries_stream() -> Result<(), Error> {
    let (server, room_list) = new_room_list().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    sync_then_assert_request_and_fake_response! {
        [server, room_list, sync]
        sync once,
        states = Init -> LoadFirstRooms -> LoadAllRooms,
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
                            ]
                        }
                    ]
                }
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

    let mut entries = room_list.entries_stream();
    let entries = entries.deref_mut();
    pin_mut!(entries);

    assert_next_matches!(entries, VectorDiff::Append { values: _ });
    assert_next_matches!(
        entries,
        VectorDiff::Set { index: 0, value } => {
            assert_eq!(value, RoomListEntry::Filled(room_id!("!r0:bar.org").to_owned()));
        }
    );
    assert_next_matches!(
        entries,
        VectorDiff::Set { index: 1, value } => {
            assert_eq!(value, RoomListEntry::Filled(room_id!("!r1:bar.org").to_owned()));
        }
    );
    assert_next_matches!(
        entries,
        VectorDiff::Set { index: 2, value } => {
            assert_eq!(value, RoomListEntry::Filled(room_id!("!r2:bar.org").to_owned()));
        }
    );
    assert_pending!(entries);

    Ok(())
}
