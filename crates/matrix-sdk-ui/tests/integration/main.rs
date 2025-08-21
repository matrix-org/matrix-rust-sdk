// Copyright 2023 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use itertools::Itertools as _;
use matrix_sdk::deserialized_responses::TimelineEvent;
use ruma::{EventId, RoomId, events::AnyStateEvent, serde::Raw};
use serde::Serialize;
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path, path_regex, query_param, query_param_is_missing},
};

mod encryption_sync_service;
mod notification_client;
mod room_list_service;
mod sliding_sync;
mod sync_service;
mod timeline;

matrix_sdk_test_utils::init_tracing_for_tests!();

/// Mount a Mock on the given server to handle the `GET /sync` endpoint with
/// an optional `since` param that returns a 200 status code with the given
/// response body.
async fn mock_sync(server: &MockServer, response_body: impl Serialize, since: Option<String>) {
    let mut mock_builder = Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/sync"))
        .and(header("authorization", "Bearer 1234"));

    if let Some(since) = since {
        mock_builder = mock_builder.and(query_param("since", since));
    } else {
        mock_builder = mock_builder.and(query_param_is_missing("since"));
    }

    mock_builder
        .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
        .mount(server)
        .await;
}

/// Mocks the /context endpoint
///
/// Note: pass `events_before` in the normal order, I'll revert the order for
/// you.
// TODO: replace with MatrixMockServer
#[allow(clippy::too_many_arguments)] // clippy you've got such a fixed mindset
async fn mock_context(
    server: &MockServer,
    room_id: &RoomId,
    event_id: &EventId,
    prev_batch_token: Option<String>,
    events_before: Vec<TimelineEvent>,
    event: TimelineEvent,
    events_after: Vec<TimelineEvent>,
    next_batch_token: Option<String>,
    state: Vec<Raw<AnyStateEvent>>,
) {
    Mock::given(method("GET"))
        .and(path(format!("/_matrix/client/r0/rooms/{room_id}/context/{event_id}")))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "events_before": events_before.into_iter().rev().map(|ev| ev.into_raw()).collect_vec(),
            "event": event.into_raw(),
            "events_after": events_after.into_iter().map(|ev| ev.into_raw()).collect_vec(),
            "state": state,
            "start": prev_batch_token,
            "end": next_batch_token
        })))
        .mount(server)
        .await;
}

/// Mocks the /messages endpoint.
///
/// Note: pass `chunk` in the correct order: topological for forward pagination,
/// reverse topological for backwards pagination.
// TODO: replace with MatrixMockServer
async fn mock_messages(
    server: &MockServer,
    start: String,
    end: Option<String>,
    chunk: Vec<TimelineEvent>,
    state: Vec<Raw<AnyStateEvent>>,
) {
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/messages$"))
        .and(header("authorization", "Bearer 1234"))
        .and(query_param("from", start.clone()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "start": start,
            "end": end,
            "chunk": chunk.into_iter().map(|ev| ev.into_raw()).collect_vec(),
            "state": state,
        })))
        .expect(1)
        .mount(server)
        .await;
}
