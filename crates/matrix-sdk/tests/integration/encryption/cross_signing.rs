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

use assert_matches2::assert_let;
use matrix_sdk::{encryption::CrossSigningResetAuthType, test_utils::mocks::MatrixMockServer};
use matrix_sdk_test::async_test;
use ruma::api::client::uiaa;
use similar_asserts::assert_eq;

#[async_test]
async fn test_reset_legacy_auth() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().expect("We should be able to access the user ID by now");

    assert!(
        !client.encryption().cross_signing_status().await.unwrap().is_complete(),
        "Initially we shouldn't have any cross-signin keys",
    );

    server.mock_upload_keys().ok().mock_once().mount().await;

    let reset_handle = {
        let _guard =
            server.mock_upload_cross_signing_keys().uiaa().expect(1).mount_as_scoped().await;

        client
            .encryption()
            .reset_cross_signing()
            .await
            .unwrap()
            .expect("We should have received a reset handle")
    };

    server.mock_upload_cross_signing_keys().ok().expect(1).mount().await;
    server.mock_upload_cross_signing_signatures().ok().expect(1).mount().await;

    assert_let!(CrossSigningResetAuthType::Uiaa(uiaa_info) = reset_handle.auth_type());

    let mut password = uiaa::Password::new(user_id.to_owned().into(), "1234".to_owned());
    password.session = uiaa_info.session.clone();
    reset_handle
        .auth(Some(uiaa::AuthData::Password(password)))
        .await
        .expect("We should be able to reset the cross-signing keys using the reset handle");

    assert!(
        client.encryption().cross_signing_status().await.unwrap().is_complete(),
        "After the reset we have the cross-signing available.",
    );
}

#[async_test]
async fn test_reset_legacy_auth_invalid_password() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().expect("We should be able to access the user ID by now");

    assert!(
        !client.encryption().cross_signing_status().await.unwrap().is_complete(),
        "Initially we shouldn't have any cross-signin keys",
    );

    server.mock_upload_keys().ok().mock_once().mount().await;

    let reset_handle = {
        let _guard =
            server.mock_upload_cross_signing_keys().uiaa().expect(1).mount_as_scoped().await;

        client
            .encryption()
            .reset_cross_signing()
            .await
            .unwrap()
            .expect("We should have received a reset handle")
    };

    server.mock_upload_cross_signing_keys().uiaa_invalid_password().expect(1).mount().await;

    assert_let!(CrossSigningResetAuthType::Uiaa(uiaa_info) = reset_handle.auth_type());

    let mut password = uiaa::Password::new(user_id.to_owned().into(), "wrong-password".to_owned());
    password.session = uiaa_info.session.clone();
    reset_handle
        .auth(Some(uiaa::AuthData::Password(password)))
        .await
        .expect_err("Resetting with the wrong password should return the error");
}

#[async_test]
async fn test_reset_unstable_oauth() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().logged_in_with_oauth().build().await;

    assert!(
        !client.encryption().cross_signing_status().await.unwrap().is_complete(),
        "Initially we shouldn't have any cross-signing keys",
    );

    server.mock_upload_keys().ok().expect(1).named("Initial device keys upload").mount().await;

    // Return the UIAA response 5 times.
    server
        .mock_upload_cross_signing_keys()
        .uiaa_unstable_oauth()
        .up_to_n_times(5)
        .expect(5)
        .named("Trying to upload the cross-signing keys with UIAA response")
        .mount()
        .await;

    // And finally succeed.
    // This works because the first mocked endpoint that matches the path is used
    // until it is invalidated by `up_to_n_times`.
    server
        .mock_upload_cross_signing_keys()
        .ok()
        .expect(1)
        .named("Succeeding to upload the cross-signing keys")
        .mount()
        .await;

    server
        .mock_upload_cross_signing_signatures()
        .ok()
        .expect(1)
        .named("Final signatures upload")
        .mount()
        .await;

    // First requests gives us a reset handle.
    let reset_handle = client
        .encryption()
        .reset_cross_signing()
        .await
        .unwrap()
        .expect("We should have received a reset handle");

    assert_let!(CrossSigningResetAuthType::OAuth(oauth_info) = reset_handle.auth_type());
    assert_eq!(
        oauth_info.approval_url.as_str(),
        format!("{}/account/?action=org.matrix.cross_signing_reset", server.uri())
    );

    // Then it retries until it succeeds.
    reset_handle.auth(None).await.expect("We should be able to reset the cross-signing keys after some attempts, waiting for the auth issue to allow us to upload");

    assert!(
        client.encryption().cross_signing_status().await.unwrap().is_complete(),
        "After the reset we have the cross-signing available.",
    );
}

#[async_test]
async fn test_reset_stable_oauth() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().logged_in_with_oauth().build().await;

    assert!(
        !client.encryption().cross_signing_status().await.unwrap().is_complete(),
        "Initially we shouldn't have any cross-signing keys",
    );

    server.mock_upload_keys().ok().expect(1).named("Initial device keys upload").mount().await;

    // Return the UIAA response 5 times.
    server
        .mock_upload_cross_signing_keys()
        .uiaa_stable_oauth()
        .up_to_n_times(5)
        .expect(5)
        .named("Trying to upload the cross-signing keys with UIAA response")
        .mount()
        .await;

    // And finally succeed.
    // This works because the first mocked endpoint that matches the path is used
    // until it is invalidated by `up_to_n_times`.
    server
        .mock_upload_cross_signing_keys()
        .ok()
        .expect(1)
        .named("Succeeding to upload the cross-signing keys")
        .mount()
        .await;

    server
        .mock_upload_cross_signing_signatures()
        .ok()
        .expect(1)
        .named("Final signatures upload")
        .mount()
        .await;

    // First requests gives us a reset handle.
    let reset_handle = client
        .encryption()
        .reset_cross_signing()
        .await
        .unwrap()
        .expect("We should have received a reset handle");

    assert_let!(CrossSigningResetAuthType::OAuth(oauth_info) = reset_handle.auth_type());
    assert_eq!(
        oauth_info.approval_url.as_str(),
        format!("{}/account/?action=org.matrix.cross_signing_reset", server.uri())
    );

    // Then it retries until it succeeds.
    reset_handle.auth(None).await.expect("We should be able to reset the cross-signing keys after some attempts, waiting for the auth issue to allow us to upload");

    assert!(
        client.encryption().cross_signing_status().await.unwrap().is_complete(),
        "After the reset we have the cross-signing available.",
    );
}
