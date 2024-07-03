// Copyright 2024 The Matrix.org Foundation C.I.C.
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

use matrix_sdk::{
    matrix_auth::{MatrixSession, MatrixSessionTokens},
    test_utils::no_retry_test_client_with_server,
    SessionMeta,
};
use matrix_sdk_test::async_test;
use ruma::{device_id, user_id};

#[async_test]
async fn reset_oidc() {
    let user_id = user_id!("@example:morpheus.localhost");

    let session = MatrixSession {
        meta: SessionMeta { user_id: user_id.into(), device_id: device_id!("DEVICEID").to_owned() },
        tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
    };

    let (client, server) = no_retry_test_client_with_server().await;
    client.restore_session(session).await.unwrap();

    assert!(
        !client.encryption().cross_signing_status().await.unwrap().is_complete(),
        "Initially we shouldn't have any cross-signin keys",
    );

    let handle = client.encryption().reset_cross_signing().await.unwrap();
}
