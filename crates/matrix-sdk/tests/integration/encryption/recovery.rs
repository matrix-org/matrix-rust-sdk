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

use std::sync::{Arc, Mutex};

use assert_matches2::assert_let;
use futures_util::StreamExt;
use matrix_sdk::{
    Client,
    authentication::matrix::MatrixSession,
    config::RequestConfig,
    encryption::{
        BackupDownloadStrategy, CrossSigningResetAuthType,
        backups::BackupState,
        recovery::{EnableProgress, RecoveryState},
    },
    test_utils::{
        client::mock_session_tokens, no_retry_test_client_with_server,
        test_client_builder_with_server,
    },
};
use matrix_sdk_base::SessionMeta;
use matrix_sdk_test::async_test;
use ruma::{UserId, api::client::uiaa, device_id, user_id};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::spawn;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{header, method, path, path_regex},
};

use crate::{encryption::mock_secret_store_with_backup_key, logged_in_client_with_server};

async fn test_client(user_id: &UserId) -> (Client, wiremock::MockServer) {
    let session = MatrixSession {
        meta: SessionMeta { user_id: user_id.into(), device_id: device_id!("DEVICEID").to_owned() },
        tokens: mock_session_tokens(),
    };

    let (builder, server) = test_client_builder_with_server().await;
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .with_encryption_settings(matrix_sdk::encryption::EncryptionSettings {
            auto_enable_cross_signing: true,
            backup_download_strategy: BackupDownloadStrategy::Manual,
            auto_enable_backups: true,
        })
        .build()
        .await
        .unwrap();

    let _guard = Mock::given(method("GET"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.default_key"
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "Account data not found"
        })))
        .expect(2)
        .named("m.secret_storage.default_key account data GET")
        .mount_as_scoped(&server)
        .await;

    let _guard = Mock::given(method("POST"))
        .and(path("_matrix/client/r0/keys/upload"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "one_time_key_counts": {
                "signed_curve25519": 50
            }
        })))
        .expect(1)
        .named("/keys/upload POST")
        .mount_as_scoped(&server)
        .await;

    let _guard = Mock::given(method("POST"))
        .and(path("_matrix/client/unstable/keys/device_signing/upload"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .named("/keys/device_signing/upload POST")
        .mount_as_scoped(&server)
        .await;

    let _guard = Mock::given(method("POST"))
        .and(path("_matrix/client/unstable/keys/signatures/upload"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "failures": {},
        })))
        .expect(1)
        .named("/keys/signatures/upload POST")
        .mount_as_scoped(&server)
        .await;

    client.restore_session(session).await.unwrap();
    client.encryption().wait_for_e2ee_initialization_tasks().await;
    client
        .encryption()
        .bootstrap_cross_signing(None)
        .await
        .expect("We should be able to bootstrap our cross-signing");
    assert_eq!(client.encryption().recovery().state(), RecoveryState::Disabled);

    (client, server)
}

async fn mock_put_new_default_secret_storage_key(user_id: &UserId, server: &wiremock::MockServer) {
    let default_key_content = Arc::new(Mutex::new(None));

    Mock::given(method("PUT"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.default_key"
        )))
        .and(header("authorization", "Bearer 1234"))
        .and({
            let default_key_content = default_key_content.clone();
            move |request: &wiremock::Request| {
                let content: Value = request.body_json().expect("The body should be a JSON body");
                *default_key_content.lock().unwrap() = Some(content);

                true
            }
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1..)
        .named("m.secret_storage.default_key deletion")
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.default_key"
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(move |_: &wiremock::Request| {
            let content = default_key_content.lock().unwrap().take().unwrap();
            ResponseTemplate::new(200).set_body_json(content)
        })
        .named("m.secret_storage.default_key account data GET")
        .mount(server)
        .await;
}

#[async_test]
async fn test_recovery_status_server_unavailable() {
    let (client, _) = logged_in_client_with_server().await;
    client.encryption().wait_for_e2ee_initialization_tasks().await;
    assert_eq!(client.encryption().recovery().state(), RecoveryState::Unknown);
}

#[async_test]
async fn test_recovery_status_secret_storage_set_up() {
    const KEY_ID: &str = "yJWwBm2Ts8jHygTBslKpABFyykavhhfA";
    let user_id = user_id!("@example:morpheus.localhost");

    let session = MatrixSession {
        meta: SessionMeta { user_id: user_id.into(), device_id: device_id!("DEVICEID").to_owned() },
        tokens: mock_session_tokens(),
    };

    let (client, server) = no_retry_test_client_with_server().await;

    mock_secret_store_with_backup_key(user_id, KEY_ID, &server).await;

    client.restore_session(session).await.unwrap();
    client.encryption().wait_for_e2ee_initialization_tasks().await;

    assert_eq!(client.encryption().recovery().state(), RecoveryState::Incomplete);

    server.verify().await;
}

#[async_test]
async fn test_recovery_status_secret_storage_not_set_up() {
    let user_id = user_id!("@example:morpheus.localhost");

    let session = MatrixSession {
        meta: SessionMeta { user_id: user_id.into(), device_id: device_id!("DEVICEID").to_owned() },
        tokens: mock_session_tokens(),
    };

    let (client, server) = no_retry_test_client_with_server().await;

    Mock::given(method("GET"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.default_key"
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "Account data not found"
        })))
        .expect(1..)
        .mount(&server)
        .await;

    client.restore_session(session).await.unwrap();
    client.encryption().wait_for_e2ee_initialization_tasks().await;

    assert_eq!(client.encryption().recovery().state(), RecoveryState::Disabled);

    server.verify().await;
}

async fn enable(
    user_id: &UserId,
    client: &Client,
    server: &wiremock::MockServer,
    wait_for_backups_to_upload: bool,
) {
    let recovery = client.encryption().recovery();

    let backup_disabled_content = Arc::new(Mutex::new(None));

    let _quard = Mock::given(method("PUT"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.org.matrix.custom.backup_disabled"
        )))
        .and(header("authorization", "Bearer 1234"))
        .and({
            let backup_disabled_content = backup_disabled_content.clone();
            move |request: &wiremock::Request| {
                let content: Value = request.body_json().expect("The body should be a JSON body");

                *backup_disabled_content.lock().unwrap() = Some(content);

                true
            }
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount_as_scoped(server)
        .await;

    let _guard = Mock::given(method("GET"))
        .and(path("_matrix/client/r0/room_keys/version"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "Account data not found"
        })))
        .expect(1)
        .mount_as_scoped(server)
        .await;

    let _guard = Mock::given(method("POST"))
        .and(path("_matrix/client/unstable/room_keys/version"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "version": "1"})))
        .expect(1)
        .mount_as_scoped(server)
        .await;

    let _guard = Mock::given(method("PUT"))
        .and(path_regex(format!(
            r"_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.key.[A-Za-z0-9]"
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .mount_as_scoped(server)
        .await;

    let _guard = Mock::given(method("GET"))
        .and(path(format!("_matrix/client/r0/user/{user_id}/account_data/m.megolm_backup.v1")))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "Account data not found"
        })))
        .expect(1)
        .mount_as_scoped(server)
        .await;

    let _guard = Mock::given(method("PUT"))
        .and(path(format!("_matrix/client/r0/user/{user_id}/account_data/m.megolm_backup.v1")))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount_as_scoped(server)
        .await;

    let default_key_content = Arc::new(Mutex::new(None));

    let _guard = Mock::given(method("PUT"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.default_key"
        )))
        .and(header("authorization", "Bearer 1234"))
        .and({
            let default_key_content = default_key_content.clone();
            move |request: &wiremock::Request| {
                let content: Value = request.body_json().expect("The body should be a JSON body");

                *default_key_content.lock().unwrap() = Some(content);

                true
            }
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount_as_scoped(server)
        .await;

    let _guard = Mock::given(method("GET"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.default_key"
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(move |_: &wiremock::Request| {
            if let Some(content) = default_key_content.lock().unwrap().as_ref() {
                ResponseTemplate::new(200).set_body_json(content)
            } else {
                ResponseTemplate::new(404)
            }
        })
        .expect(2)
        .mount_as_scoped(server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("_matrix/client/r0/user/{user_id}/account_data/m.cross_signing.master")))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "Account data not found"
        })))
        .expect(1..)
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.cross_signing.self_signing",
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "Account data not found"
        })))
        .expect(1..)
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.cross_signing.user_signing",
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "Account data not found"
        })))
        .expect(1..)
        .mount(server)
        .await;

    Mock::given(method("PUT"))
        .and(path(format!("_matrix/client/r0/user/{user_id}/account_data/m.cross_signing.master")))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1..)
        .mount(server)
        .await;

    Mock::given(method("PUT"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.cross_signing.self_signing",
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1..)
        .mount(server)
        .await;

    Mock::given(method("PUT"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.cross_signing.user_signing",
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1..)
        .mount(server)
        .await;

    let enable = if wait_for_backups_to_upload {
        recovery.enable().wait_for_backups_to_upload()
    } else {
        recovery.enable()
    };

    let mut progress_stream = enable.subscribe_to_progress();

    let task = spawn(async move {
        let mut counter = 0;

        while let Some(state) = progress_stream.next().await {
            let Ok(state) = state else { panic!("Error while waiting for the upload state") };

            match state {
                EnableProgress::Starting => {
                    assert_eq!(counter, 0, "The first state should be the starting state");
                    counter += 1;
                }

                EnableProgress::CreatingBackup => {
                    assert_eq!(counter, 1, "The second state should be the creating backup state");
                    counter += 1;
                }
                EnableProgress::CreatingRecoveryKey => {
                    assert_eq!(
                        counter, 2,
                        "The third state should be the creating recovery key state"
                    );
                    counter += 1;
                }
                EnableProgress::Done { .. } => {
                    assert_eq!(counter, 3, "The fifth state should be the done state");
                    counter += 1;
                }
                _ => panic!("No other states should be received"),
            }
        }

        assert_eq!(counter, 4, "We should have gone through 4 states, counter: {counter}");
    });

    enable.await.expect("We should be able to enable recovery");
    task.await.unwrap();

    server.verify().await
}

#[async_test]
async fn test_recovery_setup() {
    let user_id = user_id!("@example:morpheus.localhost");
    let (client, server) = test_client(user_id).await;

    enable(user_id, &client, &server, true).await;

    assert_eq!(client.encryption().backups().state(), BackupState::Enabled);
    assert_eq!(client.encryption().recovery().state(), RecoveryState::Enabled);

    server.verify().await
}

#[async_test]
async fn test_recovery_setup_without_wait() {
    let user_id = user_id!("@example:morpheus.localhost");
    let (client, server) = test_client(user_id).await;

    enable(user_id, &client, &server, false).await;

    assert_eq!(client.encryption().backups().state(), BackupState::Enabled);
    assert_eq!(client.encryption().recovery().state(), RecoveryState::Enabled);

    server.verify().await
}

#[async_test]
async fn test_backups_enabling() {
    let user_id = user_id!("@example:morpheus.localhost");
    let (client, server) = test_client(user_id).await;

    let recovery = client.encryption().recovery();

    assert_eq!(client.encryption().backups().state(), BackupState::Unknown);
    assert_eq!(recovery.state(), RecoveryState::Disabled);

    Mock::given(method("GET"))
        .and(path("_matrix/client/r0/room_keys/version"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "Account data not found"
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.org.matrix.custom.backup_disabled"
        )))
        .and(header("authorization", "Bearer 1234"))
        .and(|request: &wiremock::Request| {
            #[derive(Deserialize)]
            struct Disabled {
                disabled: bool,
            }

            let content: Disabled = request.body_json().expect("The body should be a JSON body");

            assert!(!content.disabled, "The backup support should be marked as enabled.");

            true
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("_matrix/client/unstable/room_keys/version"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "version": "1"})))
        .expect(1)
        .mount(&server)
        .await;

    recovery.enable_backup().await.expect("We should be able to only enable backups");

    assert_eq!(recovery.state(), RecoveryState::Disabled);
    assert_eq!(client.encryption().backups().state(), BackupState::Enabled);

    server.verify().await
}

#[async_test]
async fn test_backups_enabling_already_enabled() {
    let user_id = user_id!("@example:morpheus.localhost");
    let (client, server) = test_client(user_id).await;

    let recovery = client.encryption().recovery();

    assert_eq!(recovery.state(), RecoveryState::Disabled);
    assert_eq!(client.encryption().backups().state(), BackupState::Unknown);

    Mock::given(method("GET"))
        .and(path("_matrix/client/r0/room_keys/version"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "algorithm": "m.megolm_backup.v1.curve25519-aes-sha2",
            "auth_data": {
                "public_key": "hdx5rSn94rBuvJI5cwnhKAVmFyZgfJjk7vwEBD6mIHc",
                "signatures": {}
            },
            "count": 1,
            "etag": "1",
            "version": "6"
        })))
        .expect(1)
        .mount(&server)
        .await;

    recovery
        .enable_backup()
        .await
        .expect_err("We should throw an error if a backup already exists on the server");

    assert_eq!(client.encryption().backups().state(), BackupState::Unknown);
}

#[async_test]
async fn test_recovery_disabling() {
    let user_id = user_id!("@example:morpheus.localhost");
    let (client, server) = test_client(user_id).await;

    enable(user_id, &client, &server, true).await;

    let recovery = client.encryption().recovery();
    assert_eq!(recovery.state(), RecoveryState::Enabled);

    Mock::given(method("DELETE"))
        .and(path("_matrix/client/r0/room_keys/version/1"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    let default_key_content = Arc::new(Mutex::new(None));

    Mock::given(method("PUT"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.default_key"
        )))
        .and(header("authorization", "Bearer 1234"))
        .and({
            let default_key_content = default_key_content.clone();
            move |request: &wiremock::Request| {
                let content: Value = request.body_json().expect("The body should be a JSON body");

                assert_eq!(
                    content,
                    json!({}),
                    "We should have put the default key to an empty JSON content"
                );
                *default_key_content.lock().unwrap() = Some(content);

                true
            }
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .named("m.secret_storage.default_key deletion")
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.org.matrix.custom.backup_disabled"
        )))
        .and(header("authorization", "Bearer 1234"))
        .and(|request: &wiremock::Request| {
            #[derive(Deserialize)]
            struct Disabled {
                disabled: bool,
            }

            let content: Disabled = request.body_json().expect("The body should be a JSON body");

            assert!(content.disabled, "The backup support should be marked as disabled.");

            true
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.default_key"
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(move |_: &wiremock::Request| {
            if let Some(content) = default_key_content.lock().unwrap().as_ref() {
                ResponseTemplate::new(200).set_body_json(content)
            } else {
                ResponseTemplate::new(404)
            }
        })
        .expect(3)
        .named("m.secret_storage.default_key account data GET")
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path(format!("_matrix/client/r0/user/{user_id}/account_data/m.megolm_backup.v1")))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1..)
        .mount(&server)
        .await;

    recovery.disable().await.expect("We should be able to disable recovery again.");
    assert_eq!(client.encryption().backups().state(), BackupState::Unknown);
    assert_eq!(recovery.state(), RecoveryState::Disabled);

    server.verify().await
}

#[async_test]
async fn test_reset_recovery_key() {
    let user_id = user_id!("@example:morpheus.localhost");
    let (client, server) = test_client(user_id).await;

    enable(user_id, &client, &server, true).await;

    let recovery = client.encryption().recovery();
    assert_eq!(recovery.state(), RecoveryState::Enabled);

    Mock::given(method("PUT"))
        .and(path_regex(format!(
            r"_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.key.[A-Za-z0-9]"
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("_matrix/client/r0/user/{user_id}/account_data/m.megolm_backup.v1")))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "Account data not found"
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path(format!("_matrix/client/r0/user/{user_id}/account_data/m.megolm_backup.v1")))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    mock_put_new_default_secret_storage_key(user_id, &server).await;

    recovery.reset_key().await.expect("We should be able to reset our recovery key");

    server.verify().await
}

#[async_test]
async fn test_recover_and_reset() {
    let user_id = user_id!("@example:morpheus.localhost");
    const SECRET_STORE_KEY: &str = "mypassphrase";
    const KEY_ID: &str = "yJWwBm2Ts8jHygTBslKpABFyykavhhfA";

    let session = MatrixSession {
        meta: SessionMeta { user_id: user_id.into(), device_id: device_id!("DEVICEID").to_owned() },
        tokens: mock_session_tokens(),
    };

    let (client, server) = no_retry_test_client_with_server().await;

    mock_secret_store_with_backup_key(user_id, KEY_ID, &server).await;

    Mock::given(method("GET"))
        .and(path("_matrix/client/r0/room_keys/version"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "algorithm": "m.megolm_backup.v1.curve25519-aes-sha2",
            "auth_data": {
                "public_key": "hdx5rSn94rBuvJI5cwnhKAVmFyZgfJjk7vwEBD6mIHc",
                "signatures": {}
            },
            "count": 1,
            "etag": "1",
            "version": "6"
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path_regex(format!(
            r"_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.key.[A-Za-z0-9]"
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path(format!("_matrix/client/r0/user/{user_id}/account_data/m.megolm_backup.v1")))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .mount(&server)
        .await;

    mock_put_new_default_secret_storage_key(user_id, &server).await;

    client.restore_session(session).await.unwrap();
    client.encryption().wait_for_e2ee_initialization_tasks().await;

    let recovery = client.encryption().recovery();

    assert_eq!(recovery.state(), RecoveryState::Incomplete);

    recovery
        .recover_and_reset(SECRET_STORE_KEY)
        .await
        .expect("We should be able to recover our secrets and reset the secret storage key");

    server.verify().await
}

#[async_test]
async fn test_reset_identity() {
    let user_id = user_id!("@example:morpheus.localhost");
    let (client, server) = test_client(user_id).await;

    enable(user_id, &client, &server, true).await;

    // At this point both backups and recovery should be enabled
    assert_eq!(client.encryption().backups().state(), BackupState::Enabled);
    assert_eq!(client.encryption().recovery().state(), RecoveryState::Enabled);

    let did_delete_backup = Arc::new(Mutex::new(false));

    // Disabling backups
    Mock::given(method("GET"))
        .and(path("_matrix/client/r0/room_keys/version"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with({
            let did_delete_backup = did_delete_backup.clone();
            move |_: &wiremock::Request| {
                if *did_delete_backup.lock().unwrap() {
                    ResponseTemplate::new(404).set_body_json(json!({
                        "errcode": "M_NOT_FOUND",
                        "error": "No current backup version"
                    }))
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "algorithm": "m.megolm_backup.v1.curve25519-aes-sha2",
                        "auth_data": {
                            "public_key": "hdx5rSn94rBuvJI5cwnhKAVmFyZgfJjk7vwEBD6mIHc",
                            "signatures": {}
                        },
                        "count": 1,
                        "etag": "1",
                        "version": "1"
                    }))
                }
            }
        })
        .expect(3)
        .named("room_keys/version GET")
        .mount(&server)
        .await;

    Mock::given(method("DELETE"))
        .and(path("_matrix/client/r0/room_keys/version/1"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with({
            let did_delete_backup = did_delete_backup.clone();
            move |_: &wiremock::Request| {
                *did_delete_backup.lock().unwrap() = true;
                ResponseTemplate::new(200).set_body_json(json!({}))
            }
        })
        .expect(1)
        .mount(&server)
        .await;

    // Disabling recovery
    Mock::given(method("PUT"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.default_key"
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .named("m.secret_storage.default_key PUT")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.default_key"
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .named("m.secret_storage.default_key account data GET")
        .mount(&server)
        .await;

    // Resetting cross-signing keys
    let reset_handle = {
        let _guard = Mock::given(method("POST"))
            .and(path("/_matrix/client/unstable/keys/device_signing/upload"))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "flows": [
                    {
                        "stages": [
                            "m.login.password"
                        ]
                    }
                ],
                "params": {},
                "session": "oFIJVvtEOCKmRUTYKTYIIPHL"
            })))
            .expect(1)
            .named("Initial cross-signing upload attempt")
            .mount_as_scoped(&server)
            .await;

        client
            .encryption()
            .recovery()
            .reset_identity()
            .await
            .unwrap()
            .expect("We should have received a reset handle")
    };

    Mock::given(method("POST"))
        .and(path("/_matrix/client/unstable/keys/device_signing/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .named("Retrying to upload the cross-signing keys")
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/unstable/keys/signatures/upload"))
        .respond_with(move |_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(json!({}))
        })
        .named("Final signatures upload")
        .expect(1)
        .mount(&server)
        .await;

    // Re-enable backups
    Mock::given(method("PUT"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.org.matrix.custom.backup_disabled"
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .named("m.org.matrix.custom.backup_disabled PUT")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.org.matrix.custom.backup_disabled"
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"type": "m.org.matrix.custom.backup_disabled",
            "content": {
              "disabled": false
            }}),
        ))
        .expect(1)
        .named("m.org.matrix.custom.backup_disabled GET")
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("_matrix/client/unstable/room_keys/version"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "version": "1" })))
        .expect(1)
        .named("room_keys/version POST")
        .mount(&server)
        .await;

    assert_let!(CrossSigningResetAuthType::Uiaa(uiaa_info) = reset_handle.auth_type());

    let mut password = uiaa::Password::new(user_id.to_owned().into(), "1234".to_owned());
    password.session = uiaa_info.session.clone();
    reset_handle
        .reset(Some(uiaa::AuthData::Password(password)))
        .await
        .expect("Failed retrieving identity reset handle");

    assert!(
        client.encryption().cross_signing_status().await.unwrap().is_complete(),
        "After the reset we have the cross-signing available.",
    );

    // After reset backups should get renabled but recovery needs setting up again
    assert_eq!(client.encryption().backups().state(), BackupState::Enabled);
    assert_eq!(client.encryption().recovery().state(), RecoveryState::Disabled);

    server.verify().await;
}
