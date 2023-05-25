use anyhow::{Context, Result};
use futures_util::{pin_mut, StreamExt};
use matrix_sdk_test::async_test;
use matrix_sdk_ui::{room_list, RoomList};
use serde_json::json;
use wiremock::{http::Method, Match, Mock, MockServer, Request, ResponseTemplate};

use crate::logged_in_client;

async fn new_room_list() -> Result<(MockServer, RoomList)> {
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
        sync with $server:ident and $room_list_sync_stream:ident,
        assert request = { $( $request_json:tt )* },
        respond with = { $( $response_json:tt )* }
        $(,)?
    ) => {
        {
            let matcher = SlidingSyncMatcher;

            let _mock_guard = Mock::given(matcher)
                .respond_with(ResponseTemplate::new(200).set_body_json(
                    json!({ $( $response_json )* })
                ))
                .mount_as_scoped(&$server)
                .await;

            let next = $room_list_sync_stream.next().await.context("`sync` trip")??;

            for request in $server.received_requests().await.expect("Request recording has been disabled").iter().rev() {
                if matcher.matches(request) {
                    let json_value = serde_json::from_slice::<serde_json::Value>(&request.body).unwrap();

                    if let Err(error) = assert_json_diff::assert_json_matches_no_panic(
                        &json_value,
                        &json!({ $( $request_json )* }),
                        assert_json_diff::Config::new(assert_json_diff::CompareMode::Inclusive),
                    ) {
                        dbg!(json_value);
                        panic!("{}", error);
                    }
                }
            }

            next
        }
    };
}

#[async_test]
async fn test_one_list() -> Result<()> {
    let (_, room_list) = new_room_list().await?;
    let sliding_sync = room_list.sliding_sync();

    assert_eq!(sliding_sync.on_list(room_list::ALL_ROOMS_LIST_NAME, |_list| ()), Some(()));
    assert_eq!(sliding_sync.on_list(room_list::VISIBLE_ROOMS_LIST_NAME, |_list| ()), None);

    Ok(())
}

#[async_test]
async fn test_foo() -> Result<()> {
    let (server, room_list) = new_room_list().await?;

    let sync = room_list.sync();
    pin_mut!(sync);

    sync_then_assert_request_and_fake_response! {
        sync with server and sync,
        assert request = {
            "lists": {
                "all_rooms": {
                    "ranges": [],
                    "required_state": [
                        ["m.room.encryption", ""]
                    ],
                    "sort": ["by_recency", "by_name"],
                }
            },
            "extensions": {
                "e2ee": {
                    "enabled": true,
                },
                "to_device": {
                    "enabled": true,
                }
            }
        },
        respond with = {
            "pos": "foo",
            "lists": {},
            "rooms": {},
            "extensions": {},
        },
    };

    assert_eq!(
        room_list.sliding_sync().on_list(room_list::VISIBLE_ROOMS_LIST_NAME, |_list| ()),
        Some(())
    );

    Ok(())
}
