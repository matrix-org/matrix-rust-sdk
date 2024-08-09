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
    encryption::CrossSigningResetAuthType,
    matrix_auth::{MatrixSession, MatrixSessionTokens},
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
    use std::sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    };

    use assert_matches2::assert_let;
    use mas_oidc_client::types::{
        client_credentials::ClientCredentials,
        iana::oauth::OAuthClientAuthenticationMethod,
        registration::{ClientMetadata, VerifiedClientMetadata},
    };
    use matrix_sdk::{
        encryption::CrossSigningResetAuthType,
        oidc::{OidcSession, OidcSessionTokens, UserSession},
    };
    use similar_asserts::assert_eq;
    use url::Url;
    use wiremock::MockServer;

    const CLIENT_ID: &str = "test_client_id";
    const REDIRECT_URI_STRING: &str = "http://matrix.example.com/oidc/callback";

    let (client, server) = no_retry_test_client_with_server().await;

    let auth_issuer_body = json!({
      "issuer": server.uri(),
      "authorization_endpoint": format!("{}/authorize", server.uri()),
      "token_endpoint": format!("{}/oauth2/token", server.uri()),
      "jwks_uri": format!("{}/oauth2/keys.json", server.uri()),
      "response_types_supported": [
        "code",
      ],
      "response_modes_supported": [
        "fragment"
      ],
      "subject_types_supported": [
        "public"
      ],
      "id_token_signing_alg_values_supported": [
        "RS256",
      ],
      "claim_types_supported": [
        "normal"
      ],
      "account_management_uri": format!("{}/account/", server.uri()),
      "account_management_actions_supported": [
        "org.matrix.cross_signing_reset"
      ]
    });

    pub fn mock_registered_client_data() -> (ClientCredentials, VerifiedClientMetadata) {
        (
            ClientCredentials::None { client_id: CLIENT_ID.to_owned() },
            ClientMetadata {
                redirect_uris: Some(vec![Url::parse(REDIRECT_URI_STRING).unwrap()]),
                token_endpoint_auth_method: Some(OAuthClientAuthenticationMethod::None),
                ..ClientMetadata::default()
            }
            .validate()
            .expect("validate client metadata"),
        )
    }

    pub fn mock_session(tokens: OidcSessionTokens, server: &MockServer) -> OidcSession {
        let (credentials, metadata) = mock_registered_client_data();
        OidcSession {
            credentials,
            metadata,
            user: UserSession {
                meta: SessionMeta {
                    user_id: ruma::user_id!("@u:e.uk").to_owned(),
                    device_id: ruma::device_id!("XYZ").to_owned(),
                },
                tokens,
                issuer: server.uri(),
            },
        }
    }

    let tokens = OidcSessionTokens {
        access_token: "4cc3ss".to_owned(),
        refresh_token: Some("r3fr3sh".to_owned()),
        latest_id_token: None,
    };

    let session = mock_session(tokens.clone(), &server);

    client
        .oidc()
        .restore_session(session.clone())
        .await
        .expect("We should be able to restore the OIDC session");

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

    Mock::given(method("GET"))
        .and(path(".well-known/openid-configuration"))
        .respond_with(ResponseTemplate::new(200).set_body_json(auth_issuer_body))
        .expect(1)
        .named("Auth issuer discovery")
        .mount(&server)
        .await;

    let reset_handle = {
        let _guard = Mock::given(method("POST"))
            .and(path("/_matrix/client/unstable/keys/device_signing/upload"))
            .respond_with(ResponseTemplate::new(501).set_body_json(json!({
                "errcode": "M_UNRECOGNIZED",
                "error": "To reset your cross-signing keys you first need to approve it in your auth issuer settings",
            })))
            .expect(1)
            .named("Initial cross-signing upload attempt")
            .mount_as_scoped(&server)
            .await;

        let handle = client
            .encryption()
            .reset_cross_signing()
            .await
            .unwrap()
            .expect("We should have received a reset handle");

        assert_let!(CrossSigningResetAuthType::Oidc(oidc_info) = handle.auth_type());
        assert_eq!(
            oidc_info.approval_url.as_str(),
            format!("{}/account/?action=org.matrix.cross_signing_reset", server.uri())
        );

        handle
    };

    let counter = Arc::new(AtomicU8::default());

    Mock::given(method("POST"))
        .and(path("/_matrix/client/unstable/keys/device_signing/upload"))
        .respond_with({
            let counter = counter.clone();

            move |_: &wiremock::Request| {
                let current_value = counter.fetch_add(1, Ordering::SeqCst);

                println!("Hello {current_value}");
                // Only allow us to proceed on the 5th attempt, count started at 0, so if the
                // current value is at 4 it's the 5th attempt.
                if current_value >= 4 {
                    ResponseTemplate::new(200).set_body_json(json!({}))
                } else {
                    ResponseTemplate::new(501).set_body_json(json!({
                        "errcode": "M_UNRECOGNIZED",
                        "error": "",
                    }))
                }
            }
        })
        .expect(1..)
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

    reset_handle.auth(None).await.expect("We should be able to reset the cross-signing keys after some attempts, waiting for the auth issue to allow us to upload");

    // 5 because we incremented the counter once more in the request handler
    // closure.
    assert_eq!(counter.load(Ordering::SeqCst), 5);

    assert!(
        client.encryption().cross_signing_status().await.unwrap().is_complete(),
        "After the reset we have the cross-signing available.",
    );
}
