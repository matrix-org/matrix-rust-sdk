//! Helpers for integration tests involving sliding sync.

use wiremock::{Match, MockServer, Request, http::Method};

pub(crate) async fn check_requests(server: MockServer, expected_requests: &[serde_json::Value]) {
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
