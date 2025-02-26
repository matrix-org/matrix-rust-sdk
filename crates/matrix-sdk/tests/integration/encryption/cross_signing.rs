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
use matrix_sdk::{
    authentication::matrix::{MatrixSession, MatrixSessionTokens},
    encryption::CrossSigningResetAuthType,
    test_utils::no_retry_test_client_with_server,
    SessionMeta,
};
use matrix_sdk_test::async_test;
use ruma::{api::client::uiaa, device_id, user_id};
use serde_json::json;
use wiremock::{
    matchers::{method, path},
    Mock, ResponseTemplate,
};

#[async_test]
async fn test_reset_legacy_auth() {
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

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/keys/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "one_time_key_counts": {
                "signed_curve25519": 50
            }
        })))
        .expect(1)
        .named("Initial device keys upload")
        .mount(&server)
        .await;

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
            .reset_cross_signing()
            .await
            .unwrap()
            .expect("We should have received a reset handle")
    };

    Mock::given(method("POST"))
        .and(path("/_matrix/client/unstable/keys/device_signing/upload"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .named("Retrying to upload the cross-signing keys")
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/unstable/keys/signatures/upload"))
        .respond_with(move |_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_json(json!({}))
        })
        .expect(1)
        .named("Final signatures upload")
        .mount(&server)
        .await;

    assert_let!(CrossSigningResetAuthType::Uiaa(uiaa_info) = reset_handle.auth_type());

    let mut password = uiaa::Password::new(user_id.to_owned().into(), "1234".to_owned());
    password.session = uiaa_info.session.clone();
    reset_handle.auth(Some(uiaa::AuthData::Password(password))).await.expect("FOO BAR");

    assert!(
        client.encryption().cross_signing_status().await.unwrap().is_complete(),
        "After the reset we have the cross-signing available.",
    );
}

#[cfg(feature = "experimental-oidc")]
#[async_test]
async fn test_reset_oidc() {
    use assert_matches2::assert_let;
    use matrix_sdk::{encryption::CrossSigningResetAuthType, test_utils::mocks::MatrixMockServer};
    use similar_asserts::assert_eq;

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().logged_in_with_oauth(server.server().uri()).build().await;

    assert!(
        !client.encryption().cross_signing_status().await.unwrap().is_complete(),
        "Initially we shouldn't have any cross-signin keys",
    );

    server.mock_upload_keys().ok().expect(1).named("Initial device keys upload").mount().await;

    // Return the UIAA response 5 times.
    server
        .mock_upload_cross_signing_keys()
        .uiaa_oauth()
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

    assert_let!(CrossSigningResetAuthType::Oidc(oidc_info) = reset_handle.auth_type());
    assert_eq!(
        oidc_info.approval_url.as_str(),
        format!("{}/account/?action=org.matrix.cross_signing_reset", server.server().uri())
    );

    // Then it retries until it succeeds.
    reset_handle.auth(None).await.expect("We should be able to reset the cross-signing keys after some attempts, waiting for the auth issue to allow us to upload");

    assert!(
        client.encryption().cross_signing_status().await.unwrap().is_complete(),
        "After the reset we have the cross-signing available.",
    );
}
