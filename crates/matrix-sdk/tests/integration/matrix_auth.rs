use assert_matches::assert_matches;
use matrix_sdk::{
    AuthApi, AuthSession, SessionTokens,
    authentication::matrix::MatrixSession,
    test_utils::mocks::{LoginResponseTemplate200, MatrixMockServer},
};
use matrix_sdk_base::SessionMeta;
use matrix_sdk_test::{TestDevice, User, async_test};
use ruma::{
    api::{
        self,
        client::{
            account::register::{RegistrationKind, v3::Request as RegistrationRequest},
            session::get_login_types::v3::LoginType,
            uiaa::{self, AuthData},
        },
        error::StandardErrorBody,
    },
    assign,
};
use serde_json::{from_value as from_json_value, json, to_value as to_json_value};
use strass::assert_let;
use url::Url;

#[async_test]
async fn test_restore_session() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let auth = client.matrix_auth();

    assert!(auth.logged_in(), "Client should be logged in with the MatrixAuth API");

    assert_matches!(client.auth_api(), Some(AuthApi::Matrix(_)));
    assert_matches!(client.session(), Some(AuthSession::Matrix(_)));
}

#[async_test]
async fn test_login() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().unlogged().build().await;
    let homeserver = Url::parse(&server.uri()).unwrap();

    server.mock_get_login_types().ok().expect(1).mount().await;

    let can_password = client
        .matrix_auth()
        .get_login_types()
        .await
        .unwrap()
        .flows
        .iter()
        .any(|flow| matches!(flow, LoginType::Password(_)));
    assert!(can_password);

    server.mock_login().ok().expect(1).mount().await;

    let auth = client.matrix_auth();
    auth.login_username("example", "wordpass").send().await.unwrap();

    assert!(client.is_active(), "Client should be active");
    assert!(auth.logged_in(), "Client should be logged in with the MatrixAuth API");

    assert_matches!(client.auth_api(), Some(AuthApi::Matrix(_)));
    assert_matches!(client.session(), Some(AuthSession::Matrix(_)));

    assert_eq!(client.homeserver(), homeserver);
}

#[async_test]
async fn test_login_with_discovery() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().unlogged().build().await;
    let homeserver_url = Url::parse("https://example.org/").unwrap();

    server
        .mock_login()
        .ok_with(
            LoginResponseTemplate200::with_default_fields()
                .well_known(homeserver_url.to_string(), None),
        )
        .expect(1)
        .mount()
        .await;

    client.matrix_auth().login_username("example", "wordpass").send().await.unwrap();

    assert!(client.is_active(), "Client should be active");
    assert_eq!(client.homeserver(), homeserver_url);
}

#[async_test]
async fn test_login_no_discovery() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().unlogged().build().await;

    server.mock_login().ok().expect(1).mount().await;

    client.matrix_auth().login_username("example", "wordpass").send().await.unwrap();

    assert!(client.is_active(), "Client should be active");
    assert_eq!(client.homeserver(), Url::parse(&server.uri()).unwrap());
}

#[async_test]
#[cfg(feature = "sso-login")]
async fn test_login_with_sso() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().unlogged().build().await;

    server.mock_login().ok().expect(1).mount().await;

    let idp = api::client::session::get_login_types::v3::IdentityProvider::new(
        "some-id".to_owned(),
        "idp-name".to_owned(),
    );
    client
        .matrix_auth()
        .login_sso(|sso_url| async move {
            let sso_url = Url::parse(&sso_url).unwrap();

            let (_, redirect) =
                sso_url.query_pairs().find(|(key, _)| key == "redirectUrl").unwrap();

            let mut redirect_url = Url::parse(&redirect).unwrap();
            redirect_url.set_query(Some("loginToken=tinytoken"));

            reqwest::get(redirect_url.to_string()).await.unwrap();

            Ok(())
        })
        .identity_provider_id(&idp.id)
        .await
        .unwrap();

    assert!(client.is_active(), "Client should be active");
}

#[async_test]
async fn test_login_with_sso_token() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().unlogged().build().await;

    server.mock_get_login_types().ok().expect(1).mount().await;

    let auth = client.matrix_auth();
    let can_sso = auth
        .get_login_types()
        .await
        .unwrap()
        .flows
        .iter()
        .any(|flow| matches!(flow, LoginType::Sso(_)));
    assert!(can_sso);

    let sso_url = auth.get_sso_login_url("http://127.0.0.1:3030", None).await;
    sso_url.unwrap();

    server.mock_login().ok().expect(1).mount().await;

    auth.login_token("averysmalltoken").send().await.unwrap();

    assert!(client.is_active(), "Client should be active");
}

#[async_test]
async fn test_login_with_sso_callback() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().unlogged().build().await;

    server.mock_get_login_types().ok().expect(1).mount().await;

    let auth = client.matrix_auth();
    let can_sso = auth
        .get_login_types()
        .await
        .unwrap()
        .flows
        .iter()
        .any(|flow| matches!(flow, LoginType::Sso(_)));
    assert!(can_sso);

    let sso_url = auth.get_sso_login_url("http://127.0.0.1:3030", None).await;
    sso_url.unwrap();

    server.mock_login().ok().expect(1).mount().await;

    let callback_url = Url::parse("http://127.0.0.1:3030?loginToken=averysmalltoken").unwrap();
    auth.login_with_sso_callback(callback_url.into()).unwrap().await.unwrap();

    assert!(client.is_active(), "Client should be active");
}

#[async_test]
async fn test_login_error() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().unlogged().build().await;

    server.mock_login().error_forbidden().expect(1).mount().await;

    assert_let!(
        Err(err) = client.matrix_auth().login_username("example", "wordpass").send().await,
        "this request should return an `Err` variant"
    );
    assert_let!(
        Some(api_err) = err.as_client_api_error(),
        "found the wrong `Error` type {err:?}, expected `Error::RumaResponse"
    );
    assert_eq!(api_err.status_code, 403);

    assert_let!(
        api::error::ErrorBody::Standard(StandardErrorBody { kind, message, .. }) = &api_err.body,
        "non-standard error body"
    );
    assert_matches!(*kind, api::error::ErrorKind::Forbidden, "wrong `ErrorKind`");
    assert_eq!(message, "Invalid password");
}

#[async_test]
async fn test_register_error() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().unlogged().build().await;

    server.mock_register().error_forbidden().expect(1).mount().await;

    let user = assign!(RegistrationRequest::new(), {
        username: Some("user".to_owned()),
        password: Some("password".to_owned()),
        auth: Some(AuthData::FallbackAcknowledgement(
            uiaa::FallbackAcknowledgement::new("foobar".to_owned()),
        )),
        kind: RegistrationKind::User,
    });

    assert_let!(
        Err(err) = client.matrix_auth().register(user).await,
        "this request should return an `Err` variant"
    );
    assert_let!(
        Some(api_err) = err.as_client_api_error(),
        "found the wrong `Error` type {err:#?}, expected `UiaaResponse`"
    );
    assert_eq!(api_err.status_code, 403);

    assert_let!(
        api::error::ErrorBody::Standard(StandardErrorBody { kind, message, .. }) = &api_err.body,
        "non-standard error body"
    );
    assert_matches!(*kind, api::error::ErrorKind::Forbidden, "wrong `ErrorKind`");
    assert_eq!(message, "Invalid password");
}

#[test]
fn test_deserialize_session() {
    let user_id = User::Default;
    let device_id = TestDevice::Default;
    let access_token = "abcd";
    let refresh_token = "wxyz";

    // First version, or second version without refresh token.
    let json = json!({
        "access_token": access_token,
        "user_id": user_id,
        "device_id": device_id,
    });
    let session: MatrixSession = from_json_value(json).unwrap();
    assert_eq!(session.tokens.access_token, access_token);
    assert_eq!(session.meta.user_id, user_id);
    assert_eq!(session.meta.device_id, device_id);
    assert_eq!(session.tokens.refresh_token, None);

    // Second version with refresh_token.
    let json = json!({
        "access_token": access_token,
        "refresh_token": refresh_token,
        "user_id": user_id,
        "device_id": device_id,
    });
    let session: MatrixSession = from_json_value(json).unwrap();
    assert_eq!(session.tokens.access_token, access_token);
    assert_eq!(session.meta.user_id, user_id);
    assert_eq!(session.meta.device_id, device_id);
    assert_eq!(session.tokens.refresh_token.as_deref(), Some(refresh_token));
}

#[test]
fn test_serialize_session() {
    let user_id = User::Default;
    let device_id = TestDevice::Default;
    let access_token = "abcd";
    let refresh_token = "wxyz";

    // Without refresh token.
    let mut session = MatrixSession {
        meta: SessionMeta { user_id: user_id.into(), device_id: device_id.into() },
        tokens: SessionTokens { access_token: access_token.into(), refresh_token: None },
    };
    assert_eq!(
        to_json_value(session.clone()).unwrap(),
        json!({
            "access_token": access_token,
            "user_id": user_id,
            "device_id": device_id,
        })
    );

    // With refresh_token.
    session.tokens.refresh_token = Some(refresh_token.into());
    assert_eq!(
        to_json_value(session).unwrap(),
        json!({
            "access_token": access_token,
            "refresh_token": refresh_token,
            "user_id": user_id,
            "device_id": device_id,
        })
    );
}

#[cfg(feature = "e2e-encryption")]
#[async_test]
async fn test_login_with_cross_signing_bootstrapping() {
    let server = MatrixMockServer::new().await;
    let username = "example";
    let password = "hunter2";
    let token = "HUNTER2";

    server.mock_query_keys().ok().expect(2..).mount().await;
    server.mock_upload_keys().ok().expect(2).mount().await;

    server
        .mock_upload_cross_signing_signatures()
        .body_matches_partial_json(json!({
            User::Default: {}
        }))
        .ok()
        .expect(2)
        .mount()
        .await;

    {
        // Login with username and password.
        let _login_guard = server
            .mock_login()
            .body_json(json!({
                "type": "m.login.password",
                "identifier": {
                    "type": "m.id.user",
                    "user": username,
                },
                "password": password,
            }))
            .ok()
            .expect(1)
            .mount_as_scoped()
            .await;

        let _upload_guard = server
            .mock_upload_cross_signing_keys()
            .body_matches_partial_json(json!({
                "auth": {
                    "type": "m.login.password",
                    "identifier": {
                        "type": "m.id.user",
                        "user": username,
                    },
                    "password": password,
                },
                "master_key": {},
                "self_signing_key": {},
                "user_signing_key": {},
            }))
            .ok()
            .expect(1)
            .mount_as_scoped()
            .await;

        let client = server
            .client_builder()
            .unlogged()
            .on_builder(|builder| {
                builder.with_encryption_settings(matrix_sdk::encryption::EncryptionSettings {
                    auto_enable_cross_signing: true,
                    ..Default::default()
                })
            })
            .build()
            .await;

        let auth = client.matrix_auth();
        auth.login_username(username, password).send().await.unwrap();

        assert!(client.is_active(), "Client should be active");
        assert!(auth.logged_in(), "Client should be logged in with the MatrixAuth API");

        client.encryption().wait_for_e2ee_initialization_tasks().await;

        let me = client.user_id().expect("we are now logged in");
        let own_identity =
            client.encryption().get_user_identity(me).await.expect("succeeds").expect("is present");

        assert_eq!(own_identity.user_id(), me);
        assert!(own_identity.is_verified());
    }

    {
        // Login with a token.
        let _login_guard = server
            .mock_login()
            .body_json(json!({
                "type": "m.login.token",
                "token": token,
            }))
            .ok()
            .expect(1)
            .mount_as_scoped()
            .await;

        let _upload_guard = server
            .mock_upload_cross_signing_keys()
            .body_matches_partial_json(json!({
                "master_key": {},
                "self_signing_key": {},
                "user_signing_key": {},
            }))
            .ok()
            .expect(1)
            .mount_as_scoped()
            .await;

        let client = server
            .client_builder()
            .unlogged()
            .on_builder(|builder| {
                builder.with_encryption_settings(matrix_sdk::encryption::EncryptionSettings {
                    auto_enable_cross_signing: true,
                    ..Default::default()
                })
            })
            .build()
            .await;

        let auth = client.matrix_auth();
        auth.login_token(token).send().await.unwrap();

        assert!(client.is_active(), "Client should be active");
        assert!(auth.logged_in(), "Client should be logged in with the MatrixAuth API");

        client.encryption().wait_for_e2ee_initialization_tasks().await;

        let me = client.user_id().expect("we are now logged in");
        let own_identity =
            client.encryption().get_user_identity(me).await.expect("succeeds").expect("is present");

        assert_eq!(own_identity.user_id(), me);
        assert!(own_identity.is_verified());
    }
}

#[cfg(feature = "e2e-encryption")]
#[async_test]
async fn test_login_doesnt_fail_if_cross_signing_bootstrapping_failed() {
    let server = MatrixMockServer::new().await;
    let username = "example";
    let password = "hunter2";

    server.mock_query_keys().ok().expect(1..).mount().await;
    server.mock_upload_keys().ok().expect(1).mount().await;

    // Login with username and password.
    server
        .mock_login()
        .body_json(json!({
            "type": "m.login.password",
            "identifier": {
                "type": "m.id.user",
                "user": username,
            },
            "password": password,
        }))
        .ok()
        .expect(1)
        .mount()
        .await;

    let client = server
        .client_builder()
        .unlogged()
        .on_builder(|builder| {
            builder.with_encryption_settings(matrix_sdk::encryption::EncryptionSettings {
                auto_enable_cross_signing: true,
                ..Default::default()
            })
        })
        .build()
        .await;

    let auth = client.matrix_auth();
    auth.login_username(username, password).send().await.unwrap();

    assert!(client.is_active(), "Client should be active");
    assert!(auth.logged_in(), "Client should be logged in with the MatrixAuth API");

    let me = client.user_id().expect("we are now logged in");

    client.encryption().wait_for_e2ee_initialization_tasks().await;

    let own_identity = client.encryption().get_user_identity(me).await.expect("succeeds");
    let identity = own_identity.expect("created local default identity");
    assert!(identity.is_verified());
}

#[cfg(feature = "e2e-encryption")]
#[async_test]
async fn test_login_with_cross_signing_bootstrapping_already_bootstrapped() {
    // Even if we enabled cross-signing bootstrap for another device, it won't
    // restart the procedure.
    let server = MatrixMockServer::new().await;
    let user_id = User::Default;
    let username = "example";
    let password = "hunter2";

    server
        .mock_login()
        .body_json(json!({
            "type": "m.login.password",
            "identifier": {
                "type": "m.id.user",
                "user": username,
            },
            "password": password,
        }))
        .ok()
        .expect(1)
        .mount()
        .await;

    server.mock_query_keys().ok_json(json!({
            "device_keys": {
                user_id: {
                    "GHTYAJCE": {
                        "user_id": user_id,
                        "device_id": "GHTYAJCE",
                        "algorithms": [
                            "m.olm.v1.curve25519-aes-sha2",
                            "m.megolm.v1.aes-sha2"
                        ],
                        "keys": {
                            "curve25519:GHTYAJCE": "okg/vMIocD10QuctIUhBOk9ccrrNLUtBRzTDSJlVRw4",
                            "ed25519:GHTYAJCE": "MxZSkgCAPVM4KZ3VCy0zG88vYp7Z+jjy8l5z1Ji3B7Y"
                        },
                        "signatures": {
                            user_id: {
                                "ed25519:784pBUxon7VPcJJs69XkvN+AbC1ks07bvMh4qOPnVgY": "369BRaMHLW4nwrpy34eBYl0TpUeZoCs+IFXvTWJUBAv8Va4iqgB07Wi7XcJ+mmE4M7asyKnf5f7Zh4kGjOoNAQ"
                            }
                        }
                    }
                }
            },
            "failures": {},
            "master_keys": {
                user_id: {
                    "user_id": user_id,
                    "usage": [
                        "master"
                    ],
                    "keys": {
                        "ed25519:qGlcu2K7qaDn6wBG3DHOtnOeTgu6Dj1QLsxHSEGtODg": "qGlcu2K7qaDn6wBG3DHOtnOeTgu6Dj1QLsxHSEGtODg"
                    },
                    "signatures": {
                        user_id: {
                            "ed25519:GHTYAJCE": "L3v/GSbEN+qO/vJipVupW6j3fHFn1CPSt8w5Ob0IpByM+LOuxKTc60kpisl94cueQZnl40mnKEFoYzI0JZWTDA",
                            "ed25519:qGlcu2K7qaDn6wBG3DHOtnOeTgu6Dj1QLsxHSEGtODg": "rb1Y9O5nfF0bU2p7aWF+I4095C4sm3uc/IWxdC55Q8GtrGFNsiR+YTvi3tJahMLDxYOCzgXl7dJ1mXsvzRNwBA"
                        }
                    }
                }
            },
            "self_signing_keys": {
                user_id: {
                    "user_id": user_id,
                    "usage": [
                      "self_signing"
                    ],
                    "keys": {
                        "ed25519:784pBUxon7VPcJJs69XkvN+AbC1ks07bvMh4qOPnVgY": "784pBUxon7VPcJJs69XkvN+AbC1ks07bvMh4qOPnVgY"
                    },
                    "signatures": {
                        user_id: {
                            "ed25519:qGlcu2K7qaDn6wBG3DHOtnOeTgu6Dj1QLsxHSEGtODg": "TQQOP7BYFB6aZ/cVOa2qOzmzsap2kTpCLMEI1U8nO1kVtGRjXMGU+xoJ43DDWEgRvy2iUA7AMQpC1yCxo79BBA"
                        }
                    }
                }
            },
            "user_signing_keys": {
                user_id: {
                    "user_id": user_id,
                    "usage": [
                        "user_signing"
                    ],
                    "keys": {
                      "ed25519:D5nFYOzvmWUab4084Tahqhe4NgfQnuJ2XvdETSbOqrs": "D5nFYOzvmWUab4084Tahqhe4NgfQnuJ2XvdETSbOqrs"
                    },
                    "signatures": {
                        user_id: {
                            "ed25519:qGlcu2K7qaDn6wBG3DHOtnOeTgu6Dj1QLsxHSEGtODg": "fFf76W6aPyxiwrINjlEjYxTIvC+35uth/WK7mzNLtQgHCGyzhJqRZECvHVQ4slr/oSu1EAAYJbAkq/QU0bniDg"
                        }
                    }
                }
            },
        }))
        .expect(1..)
        .mount()
        .await;

    let client = server
        .client_builder()
        .unlogged()
        .on_builder(|builder| {
            builder.with_encryption_settings(matrix_sdk::encryption::EncryptionSettings {
                auto_enable_cross_signing: true,
                ..Default::default()
            })
        })
        .build()
        .await;

    let auth = client.matrix_auth();
    auth.login_username(username, password).send().await.unwrap();

    assert!(client.is_active(), "Client should be active");
    assert!(auth.logged_in(), "Client should be logged in with the MatrixAuth API");

    client.encryption().wait_for_e2ee_initialization_tasks().await;

    let me = client.user_id().expect("we are now logged in");
    let own_identity = client.encryption().get_user_identity(me).await.expect("succeeds");
    let identity = own_identity.expect("created local default identity");
    assert!(identity.is_verified());
}
