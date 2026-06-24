//! Helpers for integration tests involving sliding sync.

use std::collections::BTreeSet;

use wiremock::{Match, MockServer, Request, http::Method};

pub(crate) async fn check_requests(server: &MockServer, expected_requests: &[serde_json::Value]) {
    let mut num_requests = 0;

    for request in &server.received_requests().await.expect("Request recording has been disabled") {
        if !SlidingSyncMatcher.matches(request) {
            continue;
        }

        assert!(
            num_requests < expected_requests.len(),
            "unexpected extra requests received in the server"
        );

        let mut json_value = serde_json::from_slice::<serde_json::Value>(&request.body).unwrap();

        // Strip the transaction id, if present.
        if let Some(root) = json_value.as_object_mut() {
            root.remove("txn_id");
        }

        if let Err(error) = assert_json_diff::assert_json_matches_no_panic(
            &json_value,
            &expected_requests[num_requests],
            assert_json_diff::Config::new(assert_json_diff::CompareMode::Strict),
        ) {
            panic!(
                "{error}\n\nexpected_requests[{num_requests}] = {expected_request}\n\njson_value = {json_value:?}",
                expected_request = expected_requests[num_requests],
            );
        }

        num_requests += 1;
    }

    assert_eq!(num_requests, expected_requests.len(), "missing requests");
}

pub(crate) struct SlidingSyncMatcher;

#[derive(serde::Deserialize)]
pub(crate) struct PartialSlidingSyncRequest {
    pub txn_id: Option<String>,
    #[serde(default)]
    pub conn_id: Option<String>,
}

impl Match for SlidingSyncMatcher {
    fn matches(&self, request: &Request) -> bool {
        request.url.path() == "/_matrix/client/unstable/org.matrix.simplified_msc3575/sync"
            && request.method == Method::POST
    }
}

/// Run a single sliding sync request, checking that the request is a subset of
/// what we expect it to be, and providing the given next response.
#[macro_export]
macro_rules! sliding_sync_then_assert_request_and_fake_response {
    (
        [$server:ident, $stream:ident]
        $( assert pos $pos:expr, )?
        $( assert timeout $timeout:expr, )?
        assert request $sign:tt { $( $request_json:tt )* },
        respond with = $( ( code $code:expr ) )? { $( $response_json:tt )* }
        $( , after delay = $response_delay:expr )?
        $(,)?
    ) => {
        sliding_sync_then_assert_request_and_fake_response! {
            [$server, $stream]
            sync matches Some(Ok(_)),
            $( assert pos $pos, )?
            $( assert timeout $timeout, )?
            assert request $sign { $( $request_json )* },
            respond with = $( ( code $code ) )? { $( $response_json )* },
            $( after delay = $response_delay, )?
        }
    };

    (
        [$server:ident, $stream:ident]
        sync matches $sync_result:pat,
        $( assert pos $pos:expr, )?
        $( assert timeout $timeout:expr, )?
        assert request $sign:tt { $( $request_json:tt )* },
        respond with = $( ( code $code:expr ) )? { $( $response_json:tt )* }
        $( , after delay = $response_delay:expr )?
        $(,)?
    ) => {
        {
            use $crate::sliding_sync::{SlidingSyncMatcher, PartialSlidingSyncRequest};
            use wiremock::{Mock, ResponseTemplate, Match as _, Request};
            use assert_matches::assert_matches;
            use serde_json::json;

            let _code = 200;
            $( let _code = $code; )?

            let _mock_guard = Mock::given(SlidingSyncMatcher)
                .respond_with(move |request: &Request| {
                    let partial_request: PartialSlidingSyncRequest = request.body_json().unwrap();
                    // Repeat the transaction id in the response, to validate sticky parameters.
                    ResponseTemplate::new(_code).set_body_json(
                        json!({
                            "txn_id": partial_request.txn_id,
                            $( $response_json )*
                        })
                    )
                    $( .set_delay( $response_delay ) )?
                })
                .mount_as_scoped(& $server )
                .await;

            let next = $stream.next().await;

            assert_matches!(next, $sync_result, "sync's result");

            for request in $server.received_requests().await.expect("Request recording has been disabled").iter().rev() {
                if SlidingSyncMatcher.matches(request) {
                    let mut json_value = serde_json::from_slice::<serde_json::Value>(&request.body).unwrap();

                    // Strip the transaction id, if present.
                    if let Some(root) = json_value.as_object_mut() {
                        root.remove("txn_id");
                    }

                    // Validate `pos` from the query parameter if specified.
                    $(
                    let pos: Option<&str> = $pos;
                    match pos {
                        Some(pos) => assert!(wiremock::matchers::query_param("pos", pos).matches(request), "Invalid `pos` query parameter"),
                        None => assert!(wiremock::matchers::query_param_is_missing("pos").matches(request), "Received an unexpected `pos` query parameter"),
                    }
                    )?

                    // Validate `timeout` from the query parameter if specified.
                    $(
                    let timeout: Option<usize> = $timeout;
                    match timeout.map(|t| t.to_string()) {
                        Some(timeout) => assert!(wiremock::matchers::query_param("timeout", timeout).matches(request), "Invalid `timeout` query parameter"),
                        None => assert!(wiremock::matchers::query_param_is_missing("timeout").matches(request), "Received an unexpected `timeout` query parameter"),
                    }
                    )?

                    if let Err(error) = assert_json_diff::assert_json_matches_no_panic(
                        &json_value,
                        &json!({ $( $request_json )* }),
                        $crate::sliding_sync_then_assert_request_and_fake_response!(@assertion_config $sign)
                    ) {
                        #[allow(clippy::dbg_macro)]
                        {
                            dbg!(json_value);
                        }
                        panic!("{error}");
                    }

                    break;
                }
            }

            next
        }
    };

    (@assertion_config >=) => { assert_json_diff::Config::new(assert_json_diff::CompareMode::Inclusive) };
    (@assertion_config =) => { assert_json_diff::Config::new(assert_json_diff::CompareMode::Strict) };
}

pub(crate) async fn assert_sliding_sync_presence_for_conn_ids(
    server: &MockServer,
    expected_presence: Option<&str>,
    expected_conn_ids: &[&str],
) {
    let num_expected_conn_ids = expected_conn_ids.len();
    let expected_conn_ids =
        expected_conn_ids.iter().map(|conn_id| (*conn_id).to_owned()).collect::<BTreeSet<_>>();
    assert_eq!(expected_conn_ids.len(), num_expected_conn_ids, "duplicate expected conn IDs");

    let mut seen_conn_ids = BTreeSet::new();

    for request in &server.received_requests().await.expect("Request recording has been disabled") {
        if !SlidingSyncMatcher.matches(request) {
            continue;
        }

        let json_value = serde_json::from_slice::<serde_json::Value>(&request.body).unwrap();
        let Some(conn_id) = json_value.get("conn_id").and_then(|obj| obj.as_str()) else {
            panic!("sliding sync request missing conn_id: {json_value:?}");
        };
        assert!(
            expected_conn_ids.contains(conn_id),
            "unexpected conn id seen server side: {conn_id}"
        );

        seen_conn_ids.insert(conn_id.to_owned());

        let set_presence = request
            .url
            .query_pairs()
            .find_map(|(key, value)| (key == "set_presence").then_some(value.into_owned()));

        assert_eq!(set_presence.as_deref(), expected_presence, "conn_id: {conn_id}");
    }

    assert_eq!(seen_conn_ids, expected_conn_ids);
}
