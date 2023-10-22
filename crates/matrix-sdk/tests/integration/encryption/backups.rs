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

use futures_util::{pin_mut, StreamExt};
use matrix_sdk::{
    encryption::backups::BackupState,
    matrix_auth::{MatrixSession, MatrixSessionTokens},
};
use matrix_sdk_base::SessionMeta;
use matrix_sdk_test::async_test;
use ruma::{device_id, user_id};
use serde_json::json;
use tokio::spawn;
use wiremock::{
    matchers::{header, method, path},
    Mock, ResponseTemplate,
};

use crate::no_retry_test_client;

#[async_test]
async fn create() {
    let user_id = user_id!("@example:morpheus.localhost");

    let session = MatrixSession {
        meta: SessionMeta { user_id: user_id.into(), device_id: device_id!("DEVICEID").to_owned() },
        tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
    };
    let (client, server) = no_retry_test_client().await;
    client.restore_session(session).await.unwrap();

    Mock::given(method("POST"))
        .and(path(format!("_matrix/client/unstable/room_keys/version")))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
          "version": "1"
        })))
        .expect(1)
        .named("POST for the backup creation")
        .mount(&server)
        .await;

    assert_eq!(
        client.encryption().backups().state(),
        BackupState::Unknown,
        "We should initially be in the unknown state"
    );

    let mut states = client.encryption().backups().state_stream();

    let task = spawn(async move {
        let mut counter = 0;

        while let Some(state) = states.next().await {
            let Ok(state) = state else { panic!("Error while receiving backup state updates") };

            match state {
                BackupState::Unknown => {
                    assert_eq!(counter, 0, "The initial state should be unknown");
                    counter += 1;
                }
                BackupState::Creating => {
                    assert_eq!(counter, 1, "The second state should be the creation state");
                    counter += 1;
                }
                BackupState::Enabled => {
                    assert_eq!(counter, 2, "The third and final state should be the enabled state");
                    counter += 1;
                    break;
                }
                state => {
                    panic!("Received an invalid state for the creation of the backp {state:?}")
                }
            }
        }

        assert_eq!(counter, 3, "We should have gone through 3 states");
    });

    client.encryption().backups().create().await.expect("We should be able to create a new backup");

    assert_eq!(
        client.encryption().backups().state(),
        BackupState::Enabled,
        "Backups should be enabled after the create call"
    );

    task.await.unwrap();

    server.verify().await;
}

#[async_test]
async fn creation_failure() {
    let user_id = user_id!("@example:morpheus.localhost");

    let session = MatrixSession {
        meta: SessionMeta { user_id: user_id.into(), device_id: device_id!("DEVICEID").to_owned() },
        tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
    };
    let (client, server) = no_retry_test_client().await;
    client.restore_session(session).await.unwrap();

    Mock::given(method("POST"))
        .and(path(format!("_matrix/client/unstable/room_keys/version")))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "errcode": "M_LIMIT_EXCEEDED",
            "error": "Too many requests",
            "retry_after_ms": 2000
        })))
        .expect(1)
        .named("POST for the backup creation")
        .mount(&server)
        .await;

    assert_eq!(
        client.encryption().backups().state(),
        BackupState::Unknown,
        "We should initially be in the unknown state"
    );

    let states = client.encryption().backups().state_stream();

    let task = spawn(async move {
        pin_mut!(states);

        let mut counter = 0;
        let mut unknown_counter = 0;

        while let Some(state) = states.next().await {
            let Ok(state) = state else { panic!("Error while receiving backup state updates") };

            match state {
                BackupState::Creating => {
                    assert_eq!(counter, 1, "The second state should be the creation state");
                    counter += 1;
                }
                BackupState::Unknown => {
                    counter += 1;
                    unknown_counter += 1;

                    if counter == 3 {
                        break;
                    };
                }
                state => {
                    panic!("Received an invalid state for the creation of the backp {state:?}")
                }
            }
        }

        assert_eq!(unknown_counter, 2, "We should have gone through 2 Unknown states");
        assert_eq!(counter, 3, "We should have gone through 3 states");
    });

    client
        .encryption()
        .backups()
        .create()
        .await
        .expect_err("Creating a new backup should have failed");

    assert_eq!(
        client.encryption().backups().state(),
        BackupState::Unknown,
        "Backups should not be enabled since the creation step failed"
    );

    task.await.unwrap();

    server.verify().await;
}

#[async_test]
async fn disabling() {
    let user_id = user_id!("@example:morpheus.localhost");

    let session = MatrixSession {
        meta: SessionMeta { user_id: user_id.into(), device_id: device_id!("DEVICEID").to_owned() },
        tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
    };
    let (client, server) = no_retry_test_client().await;
    client.restore_session(session).await.unwrap();

    Mock::given(method("POST"))
        .and(path(format!("_matrix/client/unstable/room_keys/version")))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
          "version": "1"
        })))
        .expect(1)
        .named("POST for the backup creation")
        .mount(&server)
        .await;

    Mock::given(method("DELETE"))
        .and(path(format!("_matrix/client/r0/room_keys/version/1")))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .named("DELETE for the backup deletion")
        .mount(&server)
        .await;

    assert_eq!(
        client.encryption().backups().state(),
        BackupState::Unknown,
        "We should initially be in the unknown state"
    );

    client.encryption().backups().create().await.expect("We should be able to create a new backup");

    assert_eq!(
        client.encryption().backups().state(),
        BackupState::Enabled,
        "Backups should be enabled after they were created"
    );

    let states = client.encryption().backups().state_stream();

    client.encryption().backups().disable().await.expect("We should be able to disable our backup");

    let task = spawn(async move {
        pin_mut!(states);

        let mut counter = 0;

        while let Some(state) = states.next().await {
            let Ok(state) = state else { panic!("Error while receiving backup state updates") };

            match state {
                BackupState::Enabled => {
                    assert_eq!(counter, 0, "The initial state should be the enabled state");
                    counter += 1;
                }
                BackupState::Disabling => {
                    assert_eq!(counter, 1, "The second state should be the disabling state");
                    counter += 1;
                }
                BackupState::Disabled => {
                    assert_eq!(counter, 2, "The final state should be the disabled state");
                    counter += 1;
                    break;
                }
                state => {
                    panic!("Received an invalid state for the creation of the backp {state:?}")
                }
            }
        }

        assert_eq!(counter, 3, "We should have gone through 3 states");
    });

    assert_eq!(
        client.encryption().backups().state(),
        BackupState::Disabled,
        "Backups should be disabled."
    );

    task.await.unwrap();

    server.verify().await;
}
