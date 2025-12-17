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

use std::{fs::File, io::Write, sync::Arc, time::Duration};

use anyhow::Result;
use assert_matches::assert_matches;
use futures_util::{FutureExt, StreamExt, pin_mut};
use matrix_sdk::{
    Client, SessionMeta,
    authentication::matrix::MatrixSession,
    config::RequestConfig,
    encryption::{
        BackupDownloadStrategy, EncryptionSettings,
        backups::{BackupState, UploadState, futures::SteadyStateError},
        secret_storage::SecretStore,
    },
    test_utils::{
        client::mock_session_tokens, no_retry_test_client_with_server,
        test_client_builder_with_server,
    },
};
use matrix_sdk_base::crypto::{
    olm::{InboundGroupSession, OutboundGroupSession, SenderData, SessionCreationError},
    store::types::BackupDecryptionKey,
    types::EventEncryptionAlgorithm,
};
use matrix_sdk_common::timeout::timeout;
use matrix_sdk_test::{JoinedRoomBuilder, SyncResponseBuilder, TestResult, async_test};
use ruma::{
    EventId, RoomId, TransactionId,
    api::client::room::create_room::v3::Request as CreateRoomRequest,
    assign, device_id, event_id,
    events::room::message::{RoomMessageEvent, RoomMessageEventContent},
    owned_device_id, owned_user_id, room_id, user_id,
};
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::spawn;
use vodozemac::{
    Curve25519PublicKey, Curve25519SecretKey, Ed25519PublicKey, Ed25519SecretKey, olm::IdentityKeys,
};
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{header, method, path, path_regex},
};

use crate::{
    encryption::{BACKUP_DECRYPTION_KEY_BASE64, mock_secret_store_with_backup_key},
    mock_sync,
};

const ROOM_KEY: &[u8] = b"\
        -----BEGIN MEGOLM SESSION DATA-----\n\
        ASKcWoiAVUM97482UAi83Avce62hSLce7i5JhsqoF6xeAAAACqt2Cg3nyJPRWTTMXxXH7TXnkfdlmBXbQtq5\
        bpHo3LRijcq2Gc6TXilESCmJN14pIsfKRJrWjZ0squ/XsoTFytuVLWwkNaW3QF6obeg2IoVtJXLMPdw3b2vO\
        vgwGY3OMP0XafH13j1vcb6YLzvgLkZQLnYvd47hv3yK/9GmKS9tokuaQ7dCVYckYcIOS09EDTs70YdxUd5WG\
        rQynATCLFP1p/NAGv70r9MK7Cy/mNpjD0r4qC7UEDIoi1kOWzHgnLo19wtvwsb8Fg8ATxcs3Wmtj8hIUYpDx\
        ia4sM10zbytUuaPUAfCDf42IyxdmOnGe1CueXhgI71y+RW0s0argNqUt7jB70JT0o9CyX6UBGRaqLk2MPY9T\
        hUu5J8X3UgIa6rcbWigzohzWm9rdbEHFrSWqjpfQYMaAKQQgETrjSy4XTrp2RhC2oNqG/hylI4ab+F4X6fpH\
        DYP1NqNMP5g36xNu7LhDnrUB5qsPjYOmWORxGLfudpF3oLYCSlr3DgHqEIB6HjQblLZ3KQuPBse3zxyROTnS\
        AhdPH4a/z1wioFtKNVph3hecsiKEdqnz4Y2coSIdhz58mJ9JWNQoFAENE5CSsoEZAGvafYZVpW4C75YY2zq1\
        wIeiFi1dT43/jLAUGkslsi1VvnyfUu8qO404RxYO3XHoGLMFoFLOO+lZ+VGci2Vz10AhxJhEBHxRKxw4k2uB\
        HztoSJUr/2Y\n\
        -----END MEGOLM SESSION DATA-----";

fn matrix_session_example() -> MatrixSession {
    MatrixSession {
        meta: SessionMeta {
            user_id: owned_user_id!("@example:morpheus.localhost"),
            device_id: owned_device_id!("DEVICEID"),
        },
        tokens: mock_session_tokens(),
    }
}

fn matrix_session_example2() -> MatrixSession {
    MatrixSession {
        meta: SessionMeta {
            user_id: owned_user_id!("@example2:morpheus.localhost"),
            device_id: owned_device_id!("DEVICEID"),
        },
        tokens: mock_session_tokens(),
    }
}

async fn mount_and_assert_called_once(
    server: &wiremock::MockServer,
    method_argument: &str,
    path_argument: &str,
    response: ResponseTemplate,
) {
    Mock::given(method(method_argument))
        .and(path(path_argument))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(response)
        .expect(1)
        .mount(server)
        .await;
}

#[async_test]
async fn test_create() -> TestResult {
    let session = matrix_session_example();

    let (client, server) = no_retry_test_client_with_server().await;

    assert!(
        !client.encryption().backups().are_enabled().await,
        "Backups can't be enabled before we logged in"
    );

    client.restore_session(session).await?;

    mount_and_assert_called_once(
        &server,
        "POST",
        "_matrix/client/unstable/room_keys/version",
        ResponseTemplate::new(200).set_body_json(json!({ "version": "1"})),
    )
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

    task.await?;
    server.verify().await;

    Ok(())
}

#[async_test]
async fn test_creation_failure() -> TestResult {
    let session = matrix_session_example();
    let (client, server) = no_retry_test_client_with_server().await;
    client.restore_session(session).await?;

    mount_and_assert_called_once(
        &server,
        "POST",
        "_matrix/client/unstable/room_keys/version",
        ResponseTemplate::new(200).set_body_json(json!({
            "errcode": "M_LIMIT_EXCEEDED",
            "error": "Too many requests",
            "retry_after_ms": 2000
        })),
    )
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
                    }
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

    task.await?;
    server.verify().await;

    Ok(())
}

#[async_test]
async fn test_disabling() -> TestResult {
    let session = matrix_session_example();
    let (client, server) = no_retry_test_client_with_server().await;
    client.restore_session(session).await?;

    mount_and_assert_called_once(
        &server,
        "POST",
        "_matrix/client/unstable/room_keys/version",
        ResponseTemplate::new(200).set_body_json(json!({ "version": "1"})),
    )
    .await;

    mount_and_assert_called_once(
        &server,
        "DELETE",
        "_matrix/client/r0/room_keys/version/1",
        ResponseTemplate::new(200).set_body_json(json!({})),
    )
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
                BackupState::Unknown => {
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
        BackupState::Unknown,
        "Backups should be in the unknown state."
    );

    task.await?;
    server.verify().await;

    Ok(())
}

#[async_test]
async fn test_disable_if_only_enabled_remotely() -> TestResult {
    let session = matrix_session_example();
    let (client, server) = no_retry_test_client_with_server().await;
    client.restore_session(session).await?;

    assert_eq!(
        client.encryption().backups().state(),
        BackupState::Unknown,
        "We should initially be in the unknown state"
    );

    // Disabling backups should result in an error and keep the error in unknown
    // state

    client.encryption().backups().disable().await.expect_err("Disabling backups should fail");

    assert_eq!(
        client.encryption().backups().state(),
        BackupState::Unknown,
        "Backups should be in the unknown state."
    );

    server.verify().await;
    Ok(())
}

#[async_test]
#[cfg(feature = "sqlite")]
async fn test_backup_resumption() -> TestResult {
    use tempfile::tempdir;

    let dir = tempdir()?;

    let (builder, server) = test_client_builder_with_server().await;
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .sqlite_store(dir.path(), None)
        .build()
        .await?;

    let session = matrix_session_example();

    Mock::given(method("POST"))
        .and(path("_matrix/client/unstable/room_keys/version"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "version": "1" })))
        .expect(1)
        .mount(&server)
        .await;

    client.restore_session(session.to_owned()).await?;

    client.encryption().backups().create().await.expect("We should be able to create a new backup");

    assert_eq!(client.encryption().backups().state(), BackupState::Enabled);
    assert!(client.encryption().backups().are_enabled().await);

    drop(client);

    let builder = Client::builder()
        .homeserver_url(server.uri())
        .server_versions([ruma::api::MatrixVersion::V1_0]);

    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .sqlite_store(dir.path(), None)
        .build()
        .await?;

    client.restore_session(session).await?;
    client.encryption().wait_for_e2ee_initialization_tasks().await;

    assert_eq!(client.encryption().backups().state(), BackupState::Enabled);
    assert!(client.encryption().backups().are_enabled().await);

    Ok(())
}

async fn setup_backups(client: &Client, server: &wiremock::MockServer) {
    let dir = tempdir().unwrap();
    let mut room_key_path = dir.path().to_owned();
    room_key_path.push("room_key.txt");

    {
        let mut file =
            File::create(&room_key_path).expect("We should be able to create a temporary file");
        file.write_all(ROOM_KEY).unwrap();
    }

    client.encryption().import_room_keys(room_key_path, "1234").await.unwrap();

    mount_and_assert_called_once(
        server,
        "POST",
        "_matrix/client/unstable/room_keys/version",
        ResponseTemplate::new(200).set_body_json(json!({ "version": "1"})),
    )
    .await;

    let backups = client.encryption().backups();

    assert_eq!(
        backups.state(),
        BackupState::Unknown,
        "We should initially be in the unknown state"
    );

    backups.create().await.expect("We should be able to create a new backup");

    assert_eq!(
        backups.state(),
        BackupState::Enabled,
        "Backups should be enabled after the create call"
    );
}

#[async_test]
async fn test_steady_state_waiting() -> TestResult {
    let session = matrix_session_example();
    let (client, server) = no_retry_test_client_with_server().await;
    client.restore_session(session).await?;

    setup_backups(&client, &server).await;

    mount_and_assert_called_once(
        &server,
        "PUT",
        "_matrix/client/unstable/room_keys/keys",
        ResponseTemplate::new(200).set_body_json(json!({
            "count": 1,
            "etag": "abcdefg",
        }
        )),
    )
    .await;

    let backups = client.encryption().backups();

    let wait_for_steady_state = backups.wait_for_steady_state();

    let mut progress_stream = wait_for_steady_state.subscribe_to_progress();

    wait_for_steady_state
        .await
        .expect("The waiting for the steady state should return successfully");

    let task = spawn(async move {
        let mut counter = 0;

        while let Some(state) = progress_stream.next().await {
            let Ok(state) = state else { panic!("Error while waiting for the upload state") };

            match state {
                UploadState::Idle => {
                    assert_eq!(counter, 0, "The initial state should be the idle state");
                    counter += 1;
                }
                UploadState::Uploading(counts) => {
                    assert_eq!(counter, 1, "The third state should be the upload state");
                    assert_eq!(counts.total, 1, "We should have one room key in total");
                    assert_eq!(counts.backed_up, 1, "All room keys should be uploaded");
                    counter += 1;
                }
                UploadState::Done => {
                    assert_eq!(counter, 2, "The final state should be the Done state");
                    counter += 1;
                    break;
                }
                UploadState::Error => panic!("We should not have entered the error state"),
            }
        }

        assert_eq!(counter, 3, "We should have gone through 3 states, counter: {counter}");
    });

    task.await?;
    server.verify().await;

    Ok(())
}

async fn setup_create_room_and_send_message_mocks(server: &wiremock::MockServer) {
    Mock::given(method("POST"))
        .and(path("_matrix/client/unstable/room_keys/version"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "version": "1"})))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("_matrix/client/r0/createRoom"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "room_id": "!sefiuhWgwghwWgh:localhost"})),
        )
        .mount(server)
        .await;

    let state = json!(
        {
            "algorithm": "m.megolm.v1.aes-sha2",
            "rotation_period_ms": 604800000,
            "rotation_period_msgs": 100
        }
    );

    Mock::given(method("GET"))
        .and(path("_matrix/client/r0/rooms/!sefiuhWgwghwWgh:localhost/state/m.room.encryption/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(state))
        .mount(server)
        .await;

    Mock::given(method("GET"))
    .and(path("_matrix/client/r0/user/@example:morpheus.localhost/account_data/m.secret_storage.default_key"))
    .and(header("authorization", "Bearer 1234"))
    .respond_with(
        ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "Account data not found."
        }))
    )
    .mount(server)
    .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/keys/upload"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "one_time_key_counts": {
                "curve25519": 50,
                "signed_curve25519": 50
            }
        })))
        .mount(server)
        .await;

    let members = json!({
        "chunk": [
            {
                "content": {
                    "avatar_url": null,
                    "displayname": "example",
                    "membership": "join"
                },
                "event_id": "$151800140517rfvjc:localhost",
                "membership": "join",
                "origin_server_ts": 151800140,
                "room_id": "!sefiuhWgwghwWgh:localhost",
                "sender": "@example:morpheus.localhost",
                "state_key": "@example:morpheus.localhost",
                "type": "m.room.member",
                "unsigned": {
                    "age": 2970366,
                }
            }
        ]
    });

    Mock::given(method("GET"))
        .and(path("_matrix/client/r0/rooms/!sefiuhWgwghwWgh:localhost/members"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(members))
        .mount(server)
        .await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!(
        {
            "event_id": "$foo:localhost"
        })))
        .mount(server)
        .await;

    // we can just return an empty response here, we just encrypt to ourself
    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/keys/query"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_keys": {
                "@alice:example.org": {}
            }
        })))
        .mount(server)
        .await;
}

/// Test that new room keys are uploaded to backup when they are known/imported.
/// Current implementation of the backup module will try to trigger a backup
/// upload at the end of a sync.
/// For simplicity we are testing here that the upload is triggered when a new
/// outbound room key is created. But it would work for a key received via a to
/// device event as well.
#[async_test]
async fn test_incremental_upload_of_keys() -> TestResult {
    let session = matrix_session_example();
    let (client, server) = no_retry_test_client_with_server().await;
    client.restore_session(session).await?;

    let backups = client.encryption().backups();

    // This is the call we want to check. The newly created outbound session should
    // be uploaded to backup.
    mount_and_assert_called_once(
        &server,
        "PUT",
        "_matrix/client/unstable/room_keys/keys",
        ResponseTemplate::new(200).set_body_json(json!({
            "count": 1,
            "etag": "abcdefg",
        }
        )),
    )
    .await;

    setup_create_room_and_send_message_mocks(&server).await;

    backups.create().await.expect("We should be able to create a new backup");

    let alice_room = client
        .create_room(assign!(CreateRoomRequest::new(), {
            invite: vec![],
            is_direct: true,
        }))
        .await?;

    alice_room.enable_encryption().await?;

    assert!(alice_room.latest_encryption_state().await?.is_encrypted(), "room should be encrypted");

    // Send a message to create an outbound session that should be uploaded to
    // backup
    let content = RoomMessageEventContent::text_plain("Hello world");
    let txn_id = TransactionId::new();
    let _ = alice_room.send(content).with_transaction_id(txn_id).await?;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/sync"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "next_batch": "sfooBar",
            "device_one_time_keys_count": {
                "signed_curve25519": 50
            },
            "org.matrix.msc2732.device_unused_fallback_key_types": [
                "signed_curve25519"
            ],
            "device_unused_fallback_key_types": [
                "signed_curve25519"
            ]
        })))
        .mount(&server)
        .await;

    client.sync_once(Default::default()).await?;

    server.verify().await;
    Ok(())
}

#[async_test]
async fn test_incremental_upload_of_keys_sliding_sync() -> TestResult {
    use tokio::task::spawn_blocking;

    let session = matrix_session_example();
    let server = wiremock::MockServer::start().await;
    let builder = Client::builder()
        .homeserver_url(server.uri())
        .server_versions([ruma::api::MatrixVersion::V1_0]);

    let client = builder.request_config(RequestConfig::new().disable_retry()).build().await?;

    client.restore_session(session).await?;

    let backups = client.encryption().backups();

    // This is the call we want to check. The newly created outbound session should
    // be uploaded to backup.
    let (endpoint_called_sender, endpoint_called_receiver) = std::sync::mpsc::channel();
    Mock::given(method("PUT"))
        .and(path("_matrix/client/unstable/room_keys/keys"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(move |_req: &wiremock::Request| {
            let _ = endpoint_called_sender.send(());
            ResponseTemplate::new(200).set_body_json(json!({
                "count": 1,
                "etag": "abcdefg",
            }))
        })
        .expect(1)
        .mount(&server)
        .await;

    setup_create_room_and_send_message_mocks(&server).await;

    backups.create().await.expect("We should be able to create a new backup");

    let invite = vec![];
    let request = assign!(CreateRoomRequest::new(), {
        invite,
        is_direct: true,
    });

    let alice_room = client.create_room(request).await?;

    alice_room.enable_encryption().await?;

    assert!(alice_room.latest_encryption_state().await?.is_encrypted(), "room should be encrypted");

    // Send a message to create an outbound session that should be uploaded to
    // backup
    let content = RoomMessageEventContent::text_plain("Hello world");
    let txn_id = TransactionId::new();
    let _ = alice_room.send(content).with_transaction_id(txn_id).await?;

    // Set up sliding sync.
    let sliding = client
        .sliding_sync("main")?
        .with_all_extensions()
        .poll_timeout(Duration::from_secs(3))
        .network_timeout(Duration::from_secs(3))
        .add_list(
            matrix_sdk::SlidingSyncList::builder("all")
                .sync_mode(matrix_sdk::SlidingSyncMode::new_selective().add_range(0..=20)),
        )
        .build()
        .await?;

    let sync_task = spawn(async move {
        let stream = sliding.sync();
        pin_mut!(stream);
        while let Some(up) = stream.next().await {
            tracing::warn!("received update: {up:?}");
        }
    });

    Mock::given(method("POST"))
        .and(path("_matrix/client/unstable/org.matrix.simplified_msc3575/sync"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "pos": "5",
            "extensions": {
                "e2ee": {
                    "device_one_time_keys_count": {
                        "signed_curve25519": 50
                    },
                    "org.matrix.msc2732.device_unused_fallback_key_types": [
                        "signed_curve25519"
                    ],
                    "device_unused_fallback_key_types": [
                        "signed_curve25519"
                    ]
                }
            }
        })))
        .mount(&server)
        .await;

    // Wait for the endpoint to be called, at most for 10 seconds.
    //
    // Don't plain use `recv_timeout()` on the main task, since this would prevent
    // forward progress of the wiremock code.
    timeout(
        spawn_blocking(move || endpoint_called_receiver.recv().unwrap()),
        Duration::from_secs(10),
    )
    .await
    .expect("timeout waiting for the key backup endpoint to be called")
    .expect("join error (internal timeout)");

    sync_task.abort();

    server.verify().await;
    Ok(())
}

#[async_test]
async fn test_steady_state_waiting_errors() -> TestResult {
    let session = matrix_session_example();
    let (client, server) = no_retry_test_client_with_server().await;
    client.restore_session(session).await?;

    let result = client.encryption().backups().wait_for_steady_state().await;

    assert_matches!(
        result,
        Err(SteadyStateError::BackupDisabled),
        "The steady state method should tell us that the backup is not yet enabled"
    );

    setup_backups(&client, &server).await;
    let backups = client.encryption().backups();

    let result = backups.wait_for_steady_state().await;

    assert_matches!(
        result,
        Err(SteadyStateError::Connection),
        "The steady state method should tell us that it couldn't reach the homeserver"
    );

    mount_and_assert_called_once(
        &server,
        "PUT",
        "_matrix/client/unstable/room_keys/keys",
        ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "No current backup version"
        })),
    )
    .await;

    let wait_for_steady_state = backups.wait_for_steady_state();
    let mut progress_stream = wait_for_steady_state.subscribe_to_progress();

    let task = spawn(async move {
        let mut counter = 0;

        while let Some(state) = progress_stream.next().await {
            let Ok(state) = state else { panic!("Error while waiting for the upload state") };

            match state {
                UploadState::Idle => {
                    if counter != 0 && counter != 2 {
                        panic!("the first and third state should be the idle states");
                    }
                    counter += 1;
                    if counter == 3 {
                        break;
                    }
                }
                UploadState::Error => {
                    assert_eq!(counter, 1, "The second state should be the error state");
                    counter += 1;
                }
                _ => panic!("We should not have entered any other state"),
            }
        }

        assert_eq!(counter, 3, "We should have gone through 3 states, counter: {counter}");
    });

    let result = wait_for_steady_state.await;

    assert_matches!(
        result,
        Err(SteadyStateError::BackupDisabled),
        "The steady state method should tell us that the backup is deleted"
    );

    task.await?;
    Ok(())
}

#[async_test]
async fn test_enable_from_secret_storage() -> TestResult {
    const SECRET_STORE_KEY: &str = "mypassphrase";
    const KEY_ID: &str = "yJWwBm2Ts8jHygTBslKpABFyykavhhfA";

    let user_id = user_id!("@example2:morpheus.localhost");
    let room_id = room_id!("!DovneieKSTkdHKpIXy:morpheus.localhost");
    let event_id = event_id!("$JbFHtZpEJiH8uaajZjPLz0QUZc1xtBR9rPGBOjF6WFM");

    let session = matrix_session_example2();
    let (builder, server) = test_client_builder_with_server().await;
    let encryption_settings = EncryptionSettings {
        backup_download_strategy: BackupDownloadStrategy::OneShot,
        ..Default::default()
    };
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .with_encryption_settings(encryption_settings)
        .build()
        .await?;

    client.restore_session(session).await?;

    mock_secret_store_with_backup_key(user_id, KEY_ID, &server).await;

    let sync = SyncResponseBuilder::new()
        .add_joined_room(JoinedRoomBuilder::new(room_id))
        .build_json_sync_response();
    mock_sync(&server, sync, None).await;

    client.sync_once(Default::default()).await.expect("We should be able to sync with the server");

    let event_content = json!({
        "algorithm": "m.megolm.v1.aes-sha2",
        "ciphertext": "AwgAEpABhetEzzZzyYrxtEVUtlJnZtJcURBlQUQJ9irVeklCTs06LwgTMQj61PMUS4Vy\
                       YOX+PD67+hhU40/8olOww+Ud0m2afjMjC3wFX+4fFfSkoWPVHEmRVucfcdSF1RSB4EmK\
                       PIP4eo1X6x8kCIMewBvxl2sI9j4VNvDvAN7M3zkLJfFLOFHbBviI4FN7hSFHFeM739Zg\
                       iwxEs3hIkUXEiAfrobzaMEM/zY7SDrTdyffZndgJo7CZOVhoV6vuaOhmAy4X2t4UnbuV\
                       JGJjKfV57NAhp8W+9oT7ugwO",
        "device_id": "KIUVQQSDTM",
        "sender_key": "LvryVyoCjdONdBCi2vvoSbI34yTOx7YrCFACUEKoXnc",
        "session_id": "64H7XKokIx0ASkYDHZKlT5zd/Zccz/cQspPNdvnNULA"
    });
    mock_get_event(room_id, event_id, event_content, &server).await;

    let room = client.get_room(room_id).expect("We should have access to the room after the sync");
    let event =
        room.event(event_id, None).await.expect("We should be able to fetch our encrypted event");

    assert_matches!(
        event.encryption_info(),
        None,
        "We should not be able to decrypt our encrypted event before we import the room keys from \
         the backup"
    );

    let secret_storage = client.encryption().secret_storage();

    let store = secret_storage
        .open_secret_store(SECRET_STORE_KEY)
        .await
        .expect("We should be able to open our secret store");

    mock_query_key_backup(&server).await;

    Mock::given(method("GET"))
        .and(path("_matrix/client/r0/room_keys/keys"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "rooms": {
                room_id: {
                    "sessions": {
                        "64H7XKokIx0ASkYDHZKlT5zd/Zccz/cQspPNdvnNULA": {
                            "first_message_index": 0,
                            "forwarded_count": 0,
                            "is_verified": true,
                            "session_data": {
                                "ciphertext": "UaxxJxPZN5jqhSoFw59s83KlK0k77KJRxowPUC3P2/bS+TIBXw2y\
                                               qMHCpv01s+8mE95XU6RZO2/elktHiW1/mzx/2vqb4pFuARtj3rxF\
                                               zCBO7cpVhmrSU6uKW9KH2HirZMZzyXLqr3v6xoOTe5roIF5scPR0\
                                               cWxPcS/4+BZz4xGhGCVuTPFjWDszY1/iz4JAVosAF7XZLGh7aVhF\
                                               +ciDDoaaqwkD2nnMUlGEl2uchWuZv7v2q9Pmmd+qzRCdLx5c+GK3\
                                               OyT8qCSxubOvuSruwTliBl++drlMnh4vRO8UKPTuMNvEN89YKiSC\
                                               MVzXVDCS6tnjligxUENYkyUqYCKdASLDFs1cCXJDED16oQGonkU8\
                                               Lf7ccGg6XboJCmJfobrmDc3s/9IymtKaxquA2Vw2pW8Otoy4x9PK\
                                               17xHLo2nT2nf3Amp6xaCYx+tblGkLIqw8H3YZZVPVuKAVpPdAhgC\
                                               +aJA9n8qow3BLcCJSdGRMSV9MquidGgbEA/DCd6Eq3jokshcXR4v\
                                               Ma5nT4CokeZ6OdAtMWgZSaGltyNNoc+b6hk6AqcYaoMslG58DC32\
                                               EVSiFFwtSpKx7I6+J+hlV813Vx6IK0DoqTcYyVm4kFMvKnIoyAKJ\
                                               yoCSik4NQpL7DcokDhs56UJ1LcDgQTnGLqhH2Q",
                                "ephemeral": "+KmnQw7ECkCD+s2Hc0hhntT8n9zTLJvFHgX7g3XKBjs",
                                "mac": "xdzih3IkRv4"
                            }
                        }
                    }
                }
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let room_key_stream = client.encryption().backups().room_keys_for_room_stream(room_id);
    pin_mut!(room_key_stream);

    store
        .import_secrets()
        .await
        .expect("We should be able to import our secrets from the secret store");

    if let Some(Ok(room_keys)) = room_key_stream.next().now_or_never().flatten() {
        let (_, room_key_set) = room_keys.first_key_value().unwrap();
        assert!(room_key_set.contains("64H7XKokIx0ASkYDHZKlT5zd/Zccz/cQspPNdvnNULA"));
    } else {
        panic!("Failed to get an update about room keys being imported from the backup")
    }

    let event =
        room.event(event_id, None).await.expect("We should be able to fetch our encrypted event");

    assert_matches!(event.encryption_info(), Some(..), "The event should now be decrypted");
    let event: RoomMessageEvent =
        event.raw().deserialize_as_unchecked().expect("We should be able to deserialize the event");
    let event = event.as_original().unwrap();
    assert_eq!(event.content.body(), "tt");

    assert_eq!(client.encryption().backups().state(), BackupState::Enabled);

    store
        .import_secrets()
        .await
        .expect("We should be able to import our secrets from the secret store");
    assert_eq!(
        client.encryption().backups().state(),
        BackupState::Enabled,
        "Importing the secrets again should leave the backups in the enabled state."
    );

    Ok(())
}

#[async_test]
async fn test_enable_from_secret_storage_no_existing_backup() -> TestResult {
    let session = matrix_session_example2();
    let (builder, server) = test_client_builder_with_server().await;
    let encryption_settings = EncryptionSettings {
        backup_download_strategy: BackupDownloadStrategy::OneShot,
        ..Default::default()
    };
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .with_encryption_settings(encryption_settings)
        .build()
        .await?;

    client.restore_session(session).await?;

    let store = init_secret_store(&client, &server).await;
    store.import_secrets().await.expect_err(
        "We should return an error if we couldn't fetch the backup version from the server",
    );
    assert_eq!(client.encryption().backups().state(), BackupState::Unknown);

    Mock::given(method("GET"))
        .and(path("_matrix/client/r0/room_keys/version"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "No current backup version"
        })))
        .expect(1)
        .mount(&server)
        .await;

    store.import_secrets().await?;
    assert_eq!(client.encryption().backups().state(), BackupState::Unknown);

    Ok(())
}

#[async_test]
async fn test_enable_from_secret_storage_mismatched_key() -> TestResult {
    let session = matrix_session_example2();
    let (builder, server) = test_client_builder_with_server().await;
    let encryption_settings = EncryptionSettings {
        backup_download_strategy: BackupDownloadStrategy::OneShot,
        ..Default::default()
    };
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .with_encryption_settings(encryption_settings)
        .build()
        .await?;

    client.restore_session(session).await?;

    let store = init_secret_store(&client, &server).await;

    Mock::given(method("GET"))
        .and(path("_matrix/client/r0/room_keys/version"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "algorithm": "m.megolm_backup.v1.curve25519-aes-sha2",
            "auth_data": {
                "public_key": "SISFU86lzyzyS0RpkVZRDot/TScaShnbILRYfw1uVSk",
                "signatures": {}
            },
            "count": 1,
            "etag": "1",
            "version": "6"


        })))
        .expect(1)
        .mount(&server)
        .await;

    store.import_secrets().await?;
    assert_eq!(
        client.encryption().backups().state(),
        BackupState::Unknown,
        "The backup should go into the disabled state if we the current backup isn't using the \
         backup recovery key we received from secret storage"
    );

    Ok(())
}

#[async_test]
async fn test_enable_from_secret_storage_manual_download() -> TestResult {
    let session = matrix_session_example2();
    let (builder, server) = test_client_builder_with_server().await;
    let client = builder.request_config(RequestConfig::new().disable_retry()).build().await?;

    client.restore_session(session).await?;

    let store = init_secret_store(&client, &server).await;

    Mock::given(method("GET"))
        .and(path("_matrix/client/r0/room_keys/version"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "No current backup version"
        })))
        .expect(1)
        .mount(&server)
        .await;

    store.import_secrets().await?;
    assert_eq!(client.encryption().backups().state(), BackupState::Unknown);

    Ok(())
}

#[async_test]
async fn test_enable_from_secret_storage_and_manual_download() -> TestResult {
    let room_id = room_id!("!DovneieKSTkdHKpIXy:morpheus.localhost");

    let session = matrix_session_example2();
    let (builder, server) = test_client_builder_with_server().await;
    let encryption_settings = EncryptionSettings {
        backup_download_strategy: BackupDownloadStrategy::Manual,
        ..Default::default()
    };
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .with_encryption_settings(encryption_settings)
        .build()
        .await?;

    client.restore_session(session).await?;

    init_client_secret_storage_and_backup(&client, &server).await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/room_keys/keys/!DovneieKSTkdHKpIXy:morpheus.localhost"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "sessions": {
                "64H7XKokIx0ASkYDHZKlT5zd/Zccz/cQspPNdvnNULA": {
                    "first_message_index": 0,
                    "forwarded_count": 0,
                    "is_verified": true,
                    "session_data": {
                        "ciphertext": "UaxxJxPZN5jqhSoFw59s83KlK0k77KJRxowPUC3P2/bS+TIBXw2y\
                                       qMHCpv01s+8mE95XU6RZO2/elktHiW1/mzx/2vqb4pFuARtj3rxF\
                                       zCBO7cpVhmrSU6uKW9KH2HirZMZzyXLqr3v6xoOTe5roIF5scPR0\
                                       cWxPcS/4+BZz4xGhGCVuTPFjWDszY1/iz4JAVosAF7XZLGh7aVhF\
                                       +ciDDoaaqwkD2nnMUlGEl2uchWuZv7v2q9Pmmd+qzRCdLx5c+GK3\
                                       OyT8qCSxubOvuSruwTliBl++drlMnh4vRO8UKPTuMNvEN89YKiSC\
                                       MVzXVDCS6tnjligxUENYkyUqYCKdASLDFs1cCXJDED16oQGonkU8\
                                       Lf7ccGg6XboJCmJfobrmDc3s/9IymtKaxquA2Vw2pW8Otoy4x9PK\
                                       17xHLo2nT2nf3Amp6xaCYx+tblGkLIqw8H3YZZVPVuKAVpPdAhgC\
                                       +aJA9n8qow3BLcCJSdGRMSV9MquidGgbEA/DCd6Eq3jokshcXR4v\
                                       Ma5nT4CokeZ6OdAtMWgZSaGltyNNoc+b6hk6AqcYaoMslG58DC32\
                                       EVSiFFwtSpKx7I6+J+hlV813Vx6IK0DoqTcYyVm4kFMvKnIoyAKJ\
                                       yoCSik4NQpL7DcokDhs56UJ1LcDgQTnGLqhH2Q",
                        "ephemeral": "+KmnQw7ECkCD+s2Hc0hhntT8n9zTLJvFHgX7g3XKBjs",
                        "mac": "xdzih3IkRv4"
                    }
                }
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let room_key_stream = client.encryption().backups().room_keys_for_room_stream(room_id);
    pin_mut!(room_key_stream);

    client
        .encryption()
        .backups()
        .download_room_keys_for_room(room_id)
        .await
        .expect("We should be able to download room keys for a certain room");

    if let Some(Ok(room_keys)) = room_key_stream.next().now_or_never().flatten() {
        let (_, room_key_set) = room_keys.first_key_value().unwrap();
        assert!(room_key_set.contains("64H7XKokIx0ASkYDHZKlT5zd/Zccz/cQspPNdvnNULA"));
    } else {
        panic!("Failed to get an update about room keys being imported from the backup")
    }

    Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/room_keys/keys/!DovneieKSTkdHKpIXy:morpheus.localhost/D5SdVi%2Fnyxdkl97K6EZrpb5N6GcF3YzmvE9EegkVDns"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "first_message_index": 0,
            "forwarded_count": 0,
            "is_verified": true,
            "session_data": {
                "ciphertext": "JSPY1qaa8QwuurezB8l2QsK+wcwXJ6Rm3gA5AHQYrJCK1wnbIexJMx6vKFklpobTFiV6\
                               9fh7VtcpYlZoiWTjiqwPU8ceUsmI7+Q1ZXjwS6Z6PbKszvWbUdaTKY7gcJKQWz93NAmV\
                               PkAh/xjRqkKeJBlKZWzWctZ2k6QkwH5c9gHbPgQBe1usQefln7RHsEjM0+6nSV6+6qBm\
                               20uK+xfpElMBZ8d3IZvbapoT11UktzUikSQ0E6DXMj+cAfX9CftXbA5BsStXvThNldad\
                               49ZByrntoJ0yMLMk6G0uom4NaPTt75u8tX+AEHrgxFV8C7hICUPFsOFPU2ykb5qvK0JU\
                               JdJ0qkZ2GJybhCZiQdLOC5Ciwm12k4eYBKktJAGYlPhh9oWTlITGoaDpHorDFwZpSZqY\
                               rXaHyuCpAtd8Gc8L5HuZXDt9uN29ZTCGr3R8zpMqUG4DbpV1aV2QBrLfIZGt9OURU502\
                               OSonHf+USrfR3ap+Yunde8gYnkyMuydRZ/0dvWqBKST0CtRQrQ+uWbPP1ATcjdhs3XnI\
                               +N5FRIOrcrJtxbqDk1Lz+sRbFBnMZzuYTJZpPazu94AZx/t1CZyk9NZ5qbnE3wNxp2mj\
                               YvMjwbEEQ98zvwdF7PzeDoMa/9M+tXzEOuM/A+LjMpczxKFAqQ",
                "ephemeral": "Kv+mvdiIk4gvrocQWM5kdr5FzyFLgwJ4o6WL/r1EC0s",
                "mac": "5MTP4/BAzXc"
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let room_key_stream = client.encryption().backups().room_keys_for_room_stream(room_id);
    pin_mut!(room_key_stream);

    client
        .encryption()
        .backups()
        .download_room_key(room_id, "D5SdVi/nyxdkl97K6EZrpb5N6GcF3YzmvE9EegkVDns")
        .await
        .expect("We should be able to download a single room key");

    if let Some(Ok(room_keys)) = room_key_stream.next().now_or_never().flatten() {
        let (_, room_key_set) = room_keys.first_key_value().unwrap();
        assert!(room_key_set.contains("D5SdVi/nyxdkl97K6EZrpb5N6GcF3YzmvE9EegkVDns"));
    } else {
        panic!("Failed to get an update about room keys being imported from the backup")
    }

    server.verify().await;
    Ok(())
}

#[async_test]
async fn test_enable_from_secret_storage_and_download_after_utd() -> TestResult {
    let room_id = room_id!("!DovneieKSTkdHKpIXy:morpheus.localhost");
    let event_id = event_id!("$JbFHtZpEJiH8uaajZjPLz0QUZc1xtBR9rPGBOjF6WFM");

    let session = matrix_session_example2();
    let (builder, server) = test_client_builder_with_server().await;
    let encryption_settings = EncryptionSettings {
        backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
        ..Default::default()
    };
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .with_encryption_settings(encryption_settings)
        .build()
        .await?;

    client.restore_session(session).await?;

    let sync = SyncResponseBuilder::new()
        .add_joined_room(JoinedRoomBuilder::new(room_id))
        .build_json_sync_response();
    mock_sync(&server, sync, None).await;

    client.sync_once(Default::default()).await.expect("We should be able to sync with the server");

    init_client_secret_storage_and_backup(&client, &server).await;

    let event_content = json!({
        "algorithm": "m.megolm.v1.aes-sha2",
        "ciphertext": "AwgAEpABhetEzzZzyYrxtEVUtlJnZtJcURBlQUQJ9irVeklCTs06LwgTMQj61PMUS4Vy\
                       YOX+PD67+hhU40/8olOww+Ud0m2afjMjC3wFX+4fFfSkoWPVHEmRVucfcdSF1RSB4EmK\
                       PIP4eo1X6x8kCIMewBvxl2sI9j4VNvDvAN7M3zkLJfFLOFHbBviI4FN7hSFHFeM739Zg\
                       iwxEs3hIkUXEiAfrobzaMEM/zY7SDrTdyffZndgJo7CZOVhoV6vuaOhmAy4X2t4UnbuV\
                       JGJjKfV57NAhp8W+9oT7ugwO",
        "device_id": "KIUVQQSDTM",
        "sender_key": "LvryVyoCjdONdBCi2vvoSbI34yTOx7YrCFACUEKoXnc",
        "session_id": "64H7XKokIx0ASkYDHZKlT5zd/Zccz/cQspPNdvnNULA"
    });

    mock_get_event(room_id, event_id, event_content, &server).await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/room_keys/keys/!DovneieKSTkdHKpIXy:morpheus.localhost/64H7XKokIx0ASkYDHZKlT5zd%2FZccz%2FcQspPNdvnNULA"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "first_message_index": 0,
            "forwarded_count": 0,
            "is_verified": true,
            "session_data": {
                "ciphertext": "UaxxJxPZN5jqhSoFw59s83KlK0k77KJRxowPUC3P2/bS+TIBXw2y\
                               qMHCpv01s+8mE95XU6RZO2/elktHiW1/mzx/2vqb4pFuARtj3rxF\
                               zCBO7cpVhmrSU6uKW9KH2HirZMZzyXLqr3v6xoOTe5roIF5scPR0\
                               cWxPcS/4+BZz4xGhGCVuTPFjWDszY1/iz4JAVosAF7XZLGh7aVhF\
                               +ciDDoaaqwkD2nnMUlGEl2uchWuZv7v2q9Pmmd+qzRCdLx5c+GK3\
                               OyT8qCSxubOvuSruwTliBl++drlMnh4vRO8UKPTuMNvEN89YKiSC\
                               MVzXVDCS6tnjligxUENYkyUqYCKdASLDFs1cCXJDED16oQGonkU8\
                               Lf7ccGg6XboJCmJfobrmDc3s/9IymtKaxquA2Vw2pW8Otoy4x9PK\
                               17xHLo2nT2nf3Amp6xaCYx+tblGkLIqw8H3YZZVPVuKAVpPdAhgC\
                               +aJA9n8qow3BLcCJSdGRMSV9MquidGgbEA/DCd6Eq3jokshcXR4v\
                               Ma5nT4CokeZ6OdAtMWgZSaGltyNNoc+b6hk6AqcYaoMslG58DC32\
                               EVSiFFwtSpKx7I6+J+hlV813Vx6IK0DoqTcYyVm4kFMvKnIoyAKJ\
                               yoCSik4NQpL7DcokDhs56UJ1LcDgQTnGLqhH2Q",
                "ephemeral": "+KmnQw7ECkCD+s2Hc0hhntT8n9zTLJvFHgX7g3XKBjs",
                "mac": "xdzih3IkRv4"
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let room_key_stream = client.encryption().backups().room_keys_for_room_stream(room_id);
    pin_mut!(room_key_stream);

    let room = client.get_room(room_id).expect("We should have access to the room after the sync");
    let event =
        room.event(event_id, None).await.expect("We should be able to fetch our encrypted event");

    assert_matches!(
        event.encryption_info(),
        None,
        "We should not be able to decrypt the event right away"
    );

    // Wait for the key to be downloaded from backup.
    {
        let room_keys = timeout(room_key_stream.next(), Duration::from_secs(5))
            .await
            .expect("did not get a room key stream update within 5 seconds")
            .expect("room_key_stream.next() returned None")
            .expect("room_key_stream.next() returned an error");

        let (_, room_key_set) = room_keys.first_key_value().unwrap();
        assert!(room_key_set.contains("64H7XKokIx0ASkYDHZKlT5zd/Zccz/cQspPNdvnNULA"));
    }

    let event =
        room.event(event_id, None).await.expect("We should be able to fetch our encrypted event");

    assert_matches!(event.encryption_info(), Some(..), "The event should now be decrypted");
    let event: RoomMessageEvent =
        event.raw().deserialize_as_unchecked().expect("We should be able to deserialize the event");
    let event = event.as_original().unwrap();
    assert_eq!(event.content.body(), "tt");

    server.verify().await;
    Ok(())
}

/// Even if we have a key to the session, we should still attempt a backup
/// download if the UTD message has a lower megolm ratchet index than we have.
#[async_test]
async fn test_enable_from_secret_storage_and_download_after_utd_from_old_message_index()
-> TestResult {
    let room_id = room_id!("!DovneieKSTkdHKpIXy:morpheus.localhost");
    let event_id = event_id!("$JbFHtZpEJiH8uaajZjPLz0QUZc1xtBR9rPGBOjF6WFM");

    let session = matrix_session_example2();
    let (builder, server) = test_client_builder_with_server().await;
    let encryption_settings = EncryptionSettings {
        backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
        ..Default::default()
    };
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .with_encryption_settings(encryption_settings)
        .build()
        .await?;

    client.restore_session(session).await?;

    let sync = SyncResponseBuilder::new()
        .add_joined_room(JoinedRoomBuilder::new(room_id))
        .build_json_sync_response();
    mock_sync(&server, sync, None).await;

    client.sync_once(Default::default()).await.expect("We should be able to sync with the server");

    init_client_secret_storage_and_backup(&client, &server).await;

    // Create an outbound group session which we will use to encrypt a test event.
    let sender_identity_keys = IdentityKeys {
        ed25519: Ed25519SecretKey::new().public_key(),
        curve25519: Curve25519PublicKey::from(&Curve25519SecretKey::new()),
    };
    let outbound_group_session = OutboundGroupSession::new(
        device_id!("KIUVQQSDTM").to_owned(),
        Arc::new(sender_identity_keys),
        room_id,
        matrix_sdk_base::crypto::EncryptionSettings::default(),
    )?;

    // Export the `OutboundGroupSession` to an `InboundGroupSession`, and export it
    // to the backup. We do this now, at ratchet index 0.
    let inbound_group_session = inbound_session_from_outbound_session(
        sender_identity_keys.ed25519,
        room_id,
        &outbound_group_session,
    )
    .await?;
    mock_download_session_from_key_backup(room_id, inbound_group_session, &server).await;

    // Encrypt an event and prepare for the client to download it.
    let event_body = json!({"body":"tt","msgtype":"m.text"});
    let encrypted_event_content = serde_json::to_value(
        outbound_group_session
            .encrypt("m.room.message", &serde_json::from_value(event_body)?)
            .await
            .content,
    )?;
    mock_get_event(room_id, event_id, encrypted_event_content, &server).await;

    // Now, import the megolm session into the client's store, at ratchet index 1.
    {
        let inbound_group_session = inbound_session_from_outbound_session(
            sender_identity_keys.ed25519,
            room_id,
            &outbound_group_session,
        )
        .await?;
        // sanity-check that we got the session at index 1.
        assert_eq!(inbound_group_session.first_known_index(), 1);

        let machine_guard = client.olm_machine_for_testing().await;
        let olm_machine = machine_guard.as_ref().unwrap();
        olm_machine
            .store()
            .import_room_keys(vec![inbound_group_session.export().await], None, |_, _| ())
            .await
            .expect("should be able to import room key");
    }

    // Listen out for key downloads
    let room_key_stream = client.encryption().backups().room_keys_for_room_stream(room_id);
    pin_mut!(room_key_stream);

    // Finally, make a request for the event. That should kick off an attempt to
    // fetch from backup.
    let room = client.get_room(room_id).expect("We should have access to the room after the sync");
    let event =
        room.event(event_id, None).await.expect("We should be able to fetch our encrypted event");

    assert_matches!(
        event.encryption_info(),
        None,
        "We should not be able to decrypt the event right away"
    );

    // Wait for the key to be downloaded from backup.
    {
        let room_keys = timeout(room_key_stream.next(), Duration::from_secs(5))
            .await
            .expect("did not get a room key stream update within 5 seconds")
            .expect("room_key_stream.next() returned None")
            .expect("room_key_stream.next() returned an error");

        let (_, room_key_set) = room_keys.first_key_value().unwrap();
        assert!(room_key_set.contains(outbound_group_session.session_id()));
    }

    let event =
        room.event(event_id, None).await.expect("We should be able to fetch our encrypted event");

    assert_matches!(event.encryption_info(), Some(..), "The event should now be decrypted");
    let event: RoomMessageEvent =
        event.raw().deserialize_as_unchecked().expect("We should be able to deserialize the event");
    let event = event.as_original().unwrap();
    assert_eq!(event.content.body(), "tt");

    server.verify().await;

    Ok(())
}

/// Set up secret storage, and allow the client to import the backup
/// decryption key from 4S.
async fn init_client_secret_storage_and_backup(client: &Client, server: &wiremock::MockServer) {
    let store = init_secret_store(client, server).await;
    mock_query_key_backup(server).await;
    store.import_secrets().await.unwrap();
}

/// Mock the data for secret storage, and use it to set up a `SecretStore`.
async fn init_secret_store(client: &Client, server: &wiremock::MockServer) -> SecretStore {
    const SECRET_STORE_KEY: &str = "mypassphrase";
    const KEY_ID: &str = "yJWwBm2Ts8jHygTBslKpABFyykavhhfA";

    mock_secret_store_with_backup_key(client.user_id().unwrap(), KEY_ID, server).await;
    client
        .encryption()
        .secret_storage()
        .open_secret_store(SECRET_STORE_KEY)
        .await
        .expect("We should be able to open our secret store")
}

/// Given an `OutboundGroupSession`, create an `InboundGroupSession` from it.
async fn inbound_session_from_outbound_session(
    sender_signing_key: Ed25519PublicKey,
    room_id: &RoomId,
    outbound_group_session: &OutboundGroupSession,
) -> Result<InboundGroupSession, SessionCreationError> {
    InboundGroupSession::new(
        outbound_group_session.sender_key(),
        sender_signing_key,
        room_id,
        &outbound_group_session.session_key().await,
        SenderData::unknown(),
        None,
        EventEncryptionAlgorithm::MegolmV1AesSha2,
        None,
        false,
    )
}

/// Add a mock for a `GET /_matrix/client/r0/rooms/{}/event/{}` for the given
/// room/event ID.
async fn mock_get_event(
    room_id: &RoomId,
    event_id: &EventId,
    event_content_json: Value,
    server: &wiremock::MockServer,
) {
    let event_json = json!({
        "content": event_content_json,
        "event_id": event_id,
        "origin_server_ts": 1698579035927u64,
        "sender": "@example2:morpheus.localhost",
        "type": "m.room.encrypted",
        "unsigned": {
            "age": 14393491
        }
    });
    Mock::given(method("GET"))
        .and(path(format!("_matrix/client/r0/rooms/{room_id}/event/{event_id}")))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(event_json))
        .expect(2)
        .mount(server)
        .await;
}

/// Add a mock for a `GET /_matrix/client/r0/room_keys/version` request; return
/// some suitable backup data.
async fn mock_query_key_backup(server: &wiremock::MockServer) {
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
        .mount(server)
        .await;
}

/// Encrypt the given session with the backup key, and add a mock for a `GET
/// /_matrix/client/r0/room_keys/keys/{}/{}` request which will return it.
async fn mock_download_session_from_key_backup(
    room_id: &RoomId,
    inbound_group_session: InboundGroupSession,
    server: &wiremock::MockServer,
) {
    let session_id = inbound_group_session.session_id().to_owned();
    let session_backup_data = BackupDecryptionKey::from_base64(BACKUP_DECRYPTION_KEY_BASE64)
        .unwrap()
        .megolm_v1_public_key()
        .encrypt(inbound_group_session)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/_matrix/client/r0/room_keys/keys/{}/{}",
            room_id,
            // urlencode escapes things like `+`, which we do not want to escape.
            session_id.replace("/", "%2F"),
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(session_backup_data))
        .expect(1)
        .mount(server)
        .await;
}
