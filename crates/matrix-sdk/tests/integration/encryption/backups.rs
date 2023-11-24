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

use std::{fs::File, io::Write};

use assert_matches::assert_matches;
use futures_util::{pin_mut, StreamExt};
use matrix_sdk::{
    config::RequestConfig,
    encryption::{
        backups::{futures::SteadyStateError, BackupState, UploadState},
        EncryptionSettings,
    },
    matrix_auth::{MatrixSession, MatrixSessionTokens},
    Client,
};
use matrix_sdk_base::SessionMeta;
use matrix_sdk_test::{async_test, JoinedRoomBuilder, SyncResponseBuilder};
use ruma::{
    device_id, event_id, events::room::message::RoomMessageEvent, room_id, user_id, UserId,
};
use serde_json::json;
use tempfile::tempdir;
use tokio::spawn;
use wiremock::{
    matchers::{header, method, path},
    Mock, ResponseTemplate,
};

use crate::{mock_sync, no_retry_test_client, test_client_builder};

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

async fn mount_once(
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
async fn create() {
    let user_id = user_id!("@example:morpheus.localhost");

    let session = MatrixSession {
        meta: SessionMeta { user_id: user_id.into(), device_id: device_id!("DEVICEID").to_owned() },
        tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
    };

    let (client, server) = no_retry_test_client().await;

    assert!(
        !client.encryption().backups().are_enabled().await,
        "Backups can't be enabled before we logged in"
    );

    client.restore_session(session).await.unwrap();

    mount_once(
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

    mount_once(
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

    mount_once(
        &server,
        "POST",
        "_matrix/client/unstable/room_keys/version",
        ResponseTemplate::new(200).set_body_json(json!({ "version": "1"})),
    )
    .await;

    mount_once(
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

    task.await.unwrap();

    server.verify().await;
}

#[async_test]
#[cfg(feature = "sqlite")]
async fn backup_resumption() {
    use tempfile::tempdir;

    let dir = tempdir().unwrap();

    let user_id = user_id!("@example:morpheus.localhost");

    let (builder, server) = test_client_builder().await;
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .sqlite_store(dir.path(), None)
        .build()
        .await
        .unwrap();

    let session = MatrixSession {
        meta: SessionMeta { user_id: user_id.into(), device_id: device_id!("DEVICEID").to_owned() },
        tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
    };

    Mock::given(method("POST"))
        .and(path("_matrix/client/unstable/room_keys/version"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "version": "1" })))
        .expect(1)
        .mount(&server)
        .await;

    client.restore_session(session.to_owned()).await.unwrap();

    client.encryption().backups().create().await.expect("We should be able to create a new backup");

    assert_eq!(client.encryption().backups().state(), BackupState::Enabled);
    assert!(client.encryption().backups().are_enabled().await);

    drop(client);

    let builder = matrix_sdk::Client::builder()
        .homeserver_url(server.uri())
        .server_versions([ruma::api::MatrixVersion::V1_0]);

    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .sqlite_store(dir.path(), None)
        .build()
        .await
        .unwrap();

    client.restore_session(session).await.unwrap();
    client.encryption().wait_for_e2ee_initialization_tasks().await;

    assert_eq!(client.encryption().backups().state(), BackupState::Enabled);
    assert!(client.encryption().backups().are_enabled().await);
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

    mount_once(
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
async fn steady_state_waiting() {
    let user_id = user_id!("@example:morpheus.localhost");

    let session = MatrixSession {
        meta: SessionMeta { user_id: user_id.into(), device_id: device_id!("DEVICEID").to_owned() },
        tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
    };
    let (client, server) = no_retry_test_client().await;
    client.restore_session(session).await.unwrap();

    setup_backups(&client, &server).await;

    mount_once(
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

    task.await.unwrap();

    server.verify().await;
}

#[async_test]
async fn steady_state_waiting_errors() {
    let user_id = user_id!("@example:morpheus.localhost");

    let session = MatrixSession {
        meta: SessionMeta { user_id: user_id.into(), device_id: device_id!("DEVICEID").to_owned() },
        tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
    };
    let (client, server) = no_retry_test_client().await;
    client.restore_session(session).await.unwrap();

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

    mount_once(
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
                    assert_eq!(counter, 0, "The initial state should be the idle state");
                    counter += 1;
                }
                UploadState::Error => {
                    assert_eq!(counter, 1, "The second state should be the error state");
                    counter += 1;
                    break;
                }
                _ => panic!("We should not have entered any other state"),
            }
        }

        assert_eq!(counter, 2, "We should have gone through 2 states, counter: {counter}");
    });

    let result = wait_for_steady_state.await;

    assert_matches!(
        result,
        Err(SteadyStateError::BackupDisabled),
        "The steady state method should tell us that the backup is deleted"
    );

    task.await.unwrap();
}

async fn mock_secret_store_with_backup_key(
    user_id: &UserId,
    key_id: &str,
    server: &wiremock::MockServer,
) {
    Mock::given(method("GET"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.default_key"
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "key": key_id,
        })))
        .expect(1..)
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.key.{key_id}"
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "algorithm": "m.secret_storage.v1.aes-hmac-sha2",
            "iv": "1Sl4os6UhNRkVQcT6ArQ0g",
            "mac": "UCZlTzqVT7mNvLkwlcCJmuq9nA27oxqpXGdLr9SxD/Y",
            "name": null,
            "passphrase": {
                "algorithm": "m.pbkdf2",
                "iterations": 1,
                "salt": "ooLiz7Kz0TeWH2eYcyjP2fCegEB7PH5B"
            }
        })))
        .expect(1..)
        .mount(server)
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
            "_matrix/client/r0/user/{user_id}/account_data/m.cross_signing.user_signing"
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
            "_matrix/client/r0/user/{user_id}/account_data/m.cross_signing.self_signing"
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "Account data not found"
        })))
        .expect(1..)
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("_matrix/client/r0/keys/query"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_keys": {}
        })))
        .expect(1..)
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("_matrix/client/r0/user/{user_id}/account_data/m.megolm_backup.v1")))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "encrypted": {
                "yJWwBm2Ts8jHygTBslKpABFyykavhhfA": {
                    "ciphertext": "c39B25f6GSvW7gCUZI1OC0V821Ht2WUfxPWB43rvFSsubouHf16ImqLrwQ",
                    "iv": "hpyoGAElX8YRuigbqa7tfA",
                    "mac": "nE/RCVmFQxu+KuqxmYDDzIxf2JUlxz2oTpoJTj5pUxM"
                }
            }
        })))
        .expect(1..)
        .mount(server)
        .await;
}

#[async_test]
async fn enable_from_secret_storage() {
    const SECRET_STORE_KEY: &str = "mypassphrase";
    const KEY_ID: &str = "yJWwBm2Ts8jHygTBslKpABFyykavhhfA";

    let user_id = user_id!("@example2:morpheus.localhost");
    let room_id = room_id!("!DovneieKSTkdHKpIXy:morpheus.localhost");
    let event_id = event_id!("$JbFHtZpEJiH8uaajZjPLz0QUZc1xtBR9rPGBOjF6WFM");

    let session = MatrixSession {
        meta: SessionMeta { user_id: user_id.into(), device_id: device_id!("DEVICEID").to_owned() },
        tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
    };
    let (builder, server) = test_client_builder().await;
    let encryption_settings =
        EncryptionSettings { auto_download_from_backup: true, ..Default::default() };
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .with_encryption_settings(encryption_settings)
        .build()
        .await
        .unwrap();

    client.restore_session(session).await.unwrap();

    mock_secret_store_with_backup_key(user_id, KEY_ID, &server).await;

    let sync = SyncResponseBuilder::new()
        .add_joined_room(JoinedRoomBuilder::new(room_id))
        .build_json_sync_response();
    mock_sync(&server, sync, None).await;

    client.sync_once(Default::default()).await.expect("We should be able to sync with the server");

    Mock::given(method("GET"))
        .and(path("_matrix/client/r0/rooms/!DovneieKSTkdHKpIXy:morpheus.localhost/event/$JbFHtZpEJiH8uaajZjPLz0QUZc1xtBR9rPGBOjF6WFM"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "content": {
                "algorithm": "m.megolm.v1.aes-sha2",
                "ciphertext": "AwgAEpABhetEzzZzyYrxtEVUtlJnZtJcURBlQUQJ9irVeklCTs06LwgTMQj61PMUS4Vy\
                               YOX+PD67+hhU40/8olOww+Ud0m2afjMjC3wFX+4fFfSkoWPVHEmRVucfcdSF1RSB4EmK\
                               PIP4eo1X6x8kCIMewBvxl2sI9j4VNvDvAN7M3zkLJfFLOFHbBviI4FN7hSFHFeM739Zg\
                               iwxEs3hIkUXEiAfrobzaMEM/zY7SDrTdyffZndgJo7CZOVhoV6vuaOhmAy4X2t4UnbuV\
                               JGJjKfV57NAhp8W+9oT7ugwO",
                "device_id": "KIUVQQSDTM",
                "sender_key": "LvryVyoCjdONdBCi2vvoSbI34yTOx7YrCFACUEKoXnc",
                "session_id": "64H7XKokIx0ASkYDHZKlT5zd/Zccz/cQspPNdvnNULA"
            },
            "event_id": "$JbFHtZpEJiH8uaajZjPLz0QUZc1xtBR9rPGBOjF6WFM",
            "origin_server_ts": 1698579035927u64,
            "sender": "@example2:morpheus.localhost",
            "type": "m.room.encrypted",
            "unsigned": {
                "age": 14393491
            }
        })))
        .expect(2)
        .mount(&server)
        .await;

    let room = client.get_room(room_id).expect("We should have access to the room after the sync");
    let event = room.event(event_id).await.expect("We should be able to fetch our encrypted event");

    assert_matches!(
        event.encryption_info,
        None,
        "We should not be able to decrypt our encrypted event before we import the room keys from \
         the backup"
    );

    let secret_storage = client.encryption().secret_storage();

    let store = secret_storage
        .open_secret_store(SECRET_STORE_KEY)
        .await
        .expect("We should be able to open our secret store");

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
        .expect(2)
        .mount(&server)
        .await;

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

    let task = spawn(async move {
        pin_mut!(room_key_stream);

        if let Some(Ok(room_keys)) = room_key_stream.next().await {
            let (_, room_key_set) = room_keys.first_key_value().unwrap();
            assert!(room_key_set.contains("64H7XKokIx0ASkYDHZKlT5zd/Zccz/cQspPNdvnNULA"));
        } else {
            panic!("Failed to get an update about room keys being imported from the backup")
        }
    });

    store
        .import_secrets()
        .await
        .expect("We should be able to import our secrets from the secret store");

    task.await.unwrap();

    let event = room.event(event_id).await.expect("We should be able to fetch our encrypted event");

    assert_matches!(event.encryption_info, Some(..), "The event should now be decrypted");
    let event: RoomMessageEvent =
        event.event.deserialize_as().expect("We should be able to deserialize the event");
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
}

#[async_test]
async fn enable_from_secret_storage_no_existing_backup() {
    const SECRET_STORE_KEY: &str = "mypassphrase";
    const KEY_ID: &str = "yJWwBm2Ts8jHygTBslKpABFyykavhhfA";
    let user_id = user_id!("@example2:morpheus.localhost");

    let session = MatrixSession {
        meta: SessionMeta { user_id: user_id.into(), device_id: device_id!("DEVICEID").to_owned() },
        tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
    };
    let (builder, server) = test_client_builder().await;
    let encryption_settings =
        EncryptionSettings { auto_download_from_backup: true, ..Default::default() };
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .with_encryption_settings(encryption_settings)
        .build()
        .await
        .unwrap();

    client.restore_session(session).await.unwrap();

    mock_secret_store_with_backup_key(user_id, KEY_ID, &server).await;

    let secret_storage = client.encryption().secret_storage();

    let store = secret_storage
        .open_secret_store(SECRET_STORE_KEY)
        .await
        .expect("We should be able to open our secret store");

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

    store.import_secrets().await.unwrap();
    assert_eq!(client.encryption().backups().state(), BackupState::Unknown);
}

#[async_test]
async fn enable_from_secret_storage_mismatched_key() {
    const SECRET_STORE_KEY: &str = "mypassphrase";
    const KEY_ID: &str = "yJWwBm2Ts8jHygTBslKpABFyykavhhfA";
    let user_id = user_id!("@example2:morpheus.localhost");

    let session = MatrixSession {
        meta: SessionMeta { user_id: user_id.into(), device_id: device_id!("DEVICEID").to_owned() },
        tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
    };
    let (builder, server) = test_client_builder().await;
    let encryption_settings =
        EncryptionSettings { auto_download_from_backup: true, ..Default::default() };
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .with_encryption_settings(encryption_settings)
        .build()
        .await
        .unwrap();

    client.restore_session(session).await.unwrap();

    mock_secret_store_with_backup_key(user_id, KEY_ID, &server).await;

    let secret_storage = client.encryption().secret_storage();

    let store = secret_storage
        .open_secret_store(SECRET_STORE_KEY)
        .await
        .expect("We should be able to open our secret store");

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

    store.import_secrets().await.unwrap();
    assert_eq!(
        client.encryption().backups().state(),
        BackupState::Unknown,
        "The backup should go into the disabled state if we the current backup isn't using the \
         backup recovery key we received from secret storage"
    );
}

#[async_test]
async fn enable_from_secret_storage_manual_download() {
    const SECRET_STORE_KEY: &str = "mypassphrase";
    const KEY_ID: &str = "yJWwBm2Ts8jHygTBslKpABFyykavhhfA";
    let user_id = user_id!("@example2:morpheus.localhost");

    let session = MatrixSession {
        meta: SessionMeta { user_id: user_id.into(), device_id: device_id!("DEVICEID").to_owned() },
        tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
    };
    let (builder, server) = test_client_builder().await;
    let client =
        builder.request_config(RequestConfig::new().disable_retry()).build().await.unwrap();

    client.restore_session(session).await.unwrap();

    mock_secret_store_with_backup_key(user_id, KEY_ID, &server).await;

    let secret_storage = client.encryption().secret_storage();

    let store = secret_storage
        .open_secret_store(SECRET_STORE_KEY)
        .await
        .expect("We should be able to open our secret store");

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

    store.import_secrets().await.unwrap();
    assert_eq!(client.encryption().backups().state(), BackupState::Unknown);
}

#[async_test]
async fn enable_from_secret_storage_and_manual_download() {
    const SECRET_STORE_KEY: &str = "mypassphrase";
    const KEY_ID: &str = "yJWwBm2Ts8jHygTBslKpABFyykavhhfA";

    let user_id = user_id!("@example2:morpheus.localhost");
    let room_id = room_id!("!DovneieKSTkdHKpIXy:morpheus.localhost");

    let session = MatrixSession {
        meta: SessionMeta { user_id: user_id.into(), device_id: device_id!("DEVICEID").to_owned() },
        tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
    };
    let (builder, server) = test_client_builder().await;
    let encryption_settings =
        EncryptionSettings { auto_download_from_backup: true, ..Default::default() };
    let client = builder
        .request_config(RequestConfig::new().disable_retry())
        .with_encryption_settings(encryption_settings)
        .build()
        .await
        .unwrap();

    client.restore_session(session).await.unwrap();

    mock_secret_store_with_backup_key(user_id, KEY_ID, &server).await;

    let secret_storage = client.encryption().secret_storage();

    let store = secret_storage
        .open_secret_store(SECRET_STORE_KEY)
        .await
        .expect("We should be able to open our secret store");

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

    store.import_secrets().await.unwrap();

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

    let task = spawn(async move {
        pin_mut!(room_key_stream);

        if let Some(Ok(room_keys)) = room_key_stream.next().await {
            let (_, room_key_set) = room_keys.first_key_value().unwrap();
            assert!(room_key_set.contains("64H7XKokIx0ASkYDHZKlT5zd/Zccz/cQspPNdvnNULA"));
        } else {
            panic!("Failed to get an update about room keys being imported from the backup")
        }
    });

    client
        .encryption()
        .backups()
        .download_room_keys_for_room(room_id)
        .await
        .expect("We should be able to download room keys for a certain room");

    task.await.unwrap();

    let room_key_stream = client.encryption().backups().room_keys_for_room_stream(room_id);

    let task = spawn(async move {
        pin_mut!(room_key_stream);

        if let Some(Ok(room_keys)) = room_key_stream.next().await {
            let (_, room_key_set) = room_keys.first_key_value().unwrap();
            assert!(room_key_set.contains("D5SdVi/nyxdkl97K6EZrpb5N6GcF3YzmvE9EegkVDns"));
        } else {
            panic!("Failed to get an update about room keys being imported from the backup")
        }
    });

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

    client
        .encryption()
        .backups()
        .download_room_key(room_id, "D5SdVi/nyxdkl97K6EZrpb5N6GcF3YzmvE9EegkVDns")
        .await
        .expect("We should be able to download a single room key");

    task.await.unwrap();

    server.verify().await;
}
