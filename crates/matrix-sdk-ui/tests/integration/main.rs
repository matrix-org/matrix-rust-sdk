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

use matrix_sdk_test::test_json;
use serde::Serialize;
use wiremock::{
    matchers::{header, method, path, path_regex, query_param, query_param_is_missing},
    Mock, MockServer, ResponseTemplate,
};

mod encryption_sync_service;
mod notification_client;
mod room_list_service;
mod sliding_sync;
mod sync_service;
mod timeline;

matrix_sdk_test::init_tracing_for_tests!();

/// Mount a Mock on the given server to handle the `GET /sync` endpoint with
/// an optional `since` param that returns a 200 status code with the given
/// response body.
async fn mock_sync(server: &MockServer, response_body: impl Serialize, since: Option<String>) {
    let mut builder = Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/sync"))
        .and(header("authorization", "Bearer 1234"));

    if let Some(since) = since {
        builder = builder.and(query_param("since", since));
    } else {
        builder = builder.and(query_param_is_missing("since"));
    }

    builder
        .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
        .mount(server)
        .await;
}

/// Mount a Mock on the given server to handle the `GET
/// /rooms/.../state/m.room.encryption` endpoint with an option whether it
/// should return an encryption event or not.
async fn mock_encryption_state(server: &MockServer, is_encrypted: bool) {
    let builder = Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/m.*room.*encryption.?"))
        .and(header("authorization", "Bearer 1234"));

    if is_encrypted {
        builder
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(&*test_json::sync_events::ENCRYPTION_CONTENT),
            )
            .mount(server)
            .await;
    } else {
        builder
            .respond_with(ResponseTemplate::new(404).set_body_json(&*test_json::NOT_FOUND))
            .mount(server)
            .await;
    }
}
