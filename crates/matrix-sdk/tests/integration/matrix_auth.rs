use std::{collections::BTreeMap, sync::Mutex};

use assert_matches::assert_matches;
use matrix_sdk::{
    config::RequestConfig,
    matrix_auth::{MatrixSession, MatrixSessionTokens},
    AuthApi, AuthSession, Client, RumaApiError,
};
use matrix_sdk_base::SessionMeta;
use matrix_sdk_test::{async_test, test_json};
use ruma::{
    api::{
        client::{
            self as client_api,
            account::register::{v3::Request as RegistrationRequest, RegistrationKind},
            keys::upload_signatures::v3::SignedKeys,
            session::get_login_types::v3::LoginType,
            uiaa::{self, AuthData, UserIdentifier},
        },
        MatrixVersion,
    },
    assign, device_id,
    encryption::CrossSigningKey,
    serde::Raw,
    user_id, OwnedUserId,
};
use serde_json::{from_value as from_json_value, json, to_value as to_json_value};
use url::Url;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, ResponseTemplate,
};

use crate::{logged_in_client, no_retry_test_client, test_client_builder};

#[async_test]
async fn test_restore_session() {
    let (client, _) = logged_in_client().await;
    let auth = client.matrix_auth();

    assert!(auth.logged_in(), "Client should be logged in with the MatrixAuth API");

    assert_matches!(client.auth_api(), Some(AuthApi::Matrix(_)));
    assert_matches!(client.session(), Some(AuthSession::Matrix(_)));
}

#[async_test]
async fn test_login() {
    let (client, server) = no_retry_test_client().await;
    let homeserver = Url::parse(&server.uri()).unwrap();

    Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN_TYPES))
        .mount(&server)
        .await;

    let can_password = client
        .matrix_auth()
        .get_login_types()
        .await
        .unwrap()
        .flows
        .iter()
        .any(|flow| matches!(flow, LoginType::Password(_)));
    assert!(can_password);

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN))
        .mount(&server)
        .await;

    let auth = client.matrix_auth();
    auth.login_username("example", "wordpass").send().await.unwrap();

    assert!(client.logged_in(), "Client should be logged in");
    assert!(auth.logged_in(), "Client should be logged in with the MatrixAuth API");

    assert_matches!(client.auth_api(), Some(AuthApi::Matrix(_)));
    assert_matches!(client.session(), Some(AuthSession::Matrix(_)));

    assert_eq!(client.homeserver(), homeserver);
}

#[async_test]
async fn test_login_with_discovery() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN_WITH_DISCOVERY))
        .mount(&server)
        .await;

    client.matrix_auth().login_username("example", "wordpass").send().await.unwrap();

    let logged_in = client.logged_in();
    assert!(logged_in, "Client should be logged in");

    assert_eq!(client.homeserver().as_str(), "https://example.org/");
}

#[async_test]
async fn test_login_no_discovery() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN))
        .mount(&server)
        .await;

    client.matrix_auth().login_username("example", "wordpass").send().await.unwrap();

    let logged_in = client.logged_in();
    assert!(logged_in, "Client should be logged in");

    assert_eq!(client.homeserver(), Url::parse(&server.uri()).unwrap());
}

#[async_test]
#[cfg(feature = "sso-login")]
async fn test_login_with_sso() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN))
        .mount(&server)
        .await;

    let idp = ruma::api::client::session::get_login_types::v3::IdentityProvider::new(
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

    let logged_in = client.logged_in();
    assert!(logged_in, "Client should be logged in");
}

#[async_test]
async fn test_login_with_sso_token() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("GET"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN_TYPES))
        .mount(&server)
        .await;

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

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::LOGIN))
        .mount(&server)
        .await;

    auth.login_token("averysmalltoken").send().await.unwrap();

    let logged_in = client.logged_in();
    assert!(logged_in, "Client should be logged in");
}

#[async_test]
async fn test_login_error() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(403).set_body_json(&*test_json::LOGIN_RESPONSE_ERR))
        .mount(&server)
        .await;

    if let Err(err) = client.matrix_auth().login_username("example", "wordpass").send().await {
        if let Some(RumaApiError::ClientApi(api_err)) = err.as_ruma_api_error() {
            assert_eq!(api_err.status_code, http::StatusCode::from_u16(403).unwrap());

            if let client_api::error::ErrorBody::Standard { kind, message } = &api_err.body {
                if *kind != client_api::error::ErrorKind::Forbidden {
                    panic!("found the wrong `ErrorKind` {kind:?}, expected `Forbidden");
                }

                assert_eq!(message, "Invalid password");
            } else {
                panic!("non-standard error body")
            }
        } else {
            panic!("found the wrong `Error` type {err:?}, expected `Error::RumaResponse");
        }
    } else {
        panic!("this request should return an `Err` variant")
    }
}

#[async_test]
async fn test_register_error() {
    let (client, server) = no_retry_test_client().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/register"))
        .respond_with(
            ResponseTemplate::new(403).set_body_json(&*test_json::REGISTRATION_RESPONSE_ERR),
        )
        .mount(&server)
        .await;

    let user = assign!(RegistrationRequest::new(), {
        username: Some("user".to_owned()),
        password: Some("password".to_owned()),
        auth: Some(uiaa::AuthData::FallbackAcknowledgement(
            uiaa::FallbackAcknowledgement::new("foobar".to_owned()),
        )),
        kind: RegistrationKind::User,
    });

    if let Err(err) = client.matrix_auth().register(user).await {
        if let Some(api_err) = err.as_client_api_error() {
            assert_eq!(api_err.status_code, http::StatusCode::from_u16(403).unwrap());
            if let client_api::error::ErrorBody::Standard { kind, message } = &api_err.body {
                if *kind != client_api::error::ErrorKind::Forbidden {
                    panic!("found the wrong `ErrorKind` {kind:?}, expected `Forbidden");
                }

                assert_eq!(message, "Invalid password");
            } else {
                panic!("non-standard error body")
            }
        } else {
            panic!("found the wrong `Error` type {err:#?}, expected `UiaaResponse`");
        }
    } else {
        panic!("this request should return an `Err` variant")
    }
}

#[test]
fn test_deserialize_session() {
    // First version, or second version without refresh token.
    let json = json!({
        "access_token": "abcd",
        "user_id": "@user:localhost",
        "device_id": "EFGHIJ",
    });
    let session: MatrixSession = from_json_value(json).unwrap();
    assert_eq!(session.tokens.access_token, "abcd");
    assert_eq!(session.meta.user_id, "@user:localhost");
    assert_eq!(session.meta.device_id, "EFGHIJ");
    assert_eq!(session.tokens.refresh_token, None);

    // Second version with refresh_token.
    let json = json!({
        "access_token": "abcd",
        "refresh_token": "wxyz",
        "user_id": "@user:localhost",
        "device_id": "EFGHIJ",
    });
    let session: MatrixSession = from_json_value(json).unwrap();
    assert_eq!(session.tokens.access_token, "abcd");
    assert_eq!(session.meta.user_id, "@user:localhost");
    assert_eq!(session.meta.device_id, "EFGHIJ");
    assert_eq!(session.tokens.refresh_token.as_deref(), Some("wxyz"));
}

#[test]
fn test_serialize_session() {
    // Without refresh token.
    let mut session = MatrixSession {
        meta: SessionMeta {
            user_id: user_id!("@user:localhost").to_owned(),
            device_id: device_id!("EFGHIJ").to_owned(),
        },
        tokens: MatrixSessionTokens { access_token: "abcd".to_owned(), refresh_token: None },
    };
    assert_eq!(
        to_json_value(session.clone()).unwrap(),
        json!({
            "access_token": "abcd",
            "user_id": "@user:localhost",
            "device_id": "EFGHIJ",
        })
    );

    // With refresh_token.
    session.tokens.refresh_token = Some("wxyz".to_owned());
    assert_eq!(
        to_json_value(session).unwrap(),
        json!({
            "access_token": "abcd",
            "refresh_token": "wxyz",
            "user_id": "@user:localhost",
            "device_id": "EFGHIJ",
        })
    );
}

#[cfg(feature = "e2e-encryption")]
#[async_test]
async fn test_login_with_cross_signing_bootstrapping() {
    let server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/keys/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_keys": {
                "@alice:example.org": {}
            }
        })))
        .mount(&server)
        .await;

    let num_calls = Mutex::new(0);
    Mock::given(method("POST"))
        .and(path("/_matrix/client/unstable/keys/device_signing/upload"))
        .respond_with(move |req: &Request| {
            #[derive(Debug, serde::Deserialize)]
            struct Parameters {
                auth: Option<AuthData>,
                master_key: Option<Raw<CrossSigningKey>>,
                self_signing_key: Option<Raw<CrossSigningKey>>,
                user_signing_key: Option<Raw<CrossSigningKey>>,
            }

            let params: Parameters = req.body_json().unwrap();

            {
                let mut num_calls = num_calls.lock().unwrap();
                if *num_calls == 0 {
                    // First time, we use a password.
                    let password =
                        assert_matches!(&params.auth, Some(AuthData::Password(pwd)) => pwd);
                    assert_eq!(
                        password.identifier,
                        UserIdentifier::UserIdOrLocalpart("example".to_owned())
                    );
                    assert_eq!(password.password, "hunter2");

                    *num_calls += 1;
                } else {
                    // Second time, we use a login token. Pretend MSC3967 is enabled and require an
                    // empty auth.
                    assert!(params.auth.is_none());
                }
            }

            assert!(params.master_key.is_some());
            assert!(params.self_signing_key.is_some());
            assert!(params.user_signing_key.is_some());

            ResponseTemplate::new(200).set_body_json(json!({}))
        })
        .expect(2)
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/unstable/keys/signatures/upload"))
        .respond_with(|req: &Request| {
            #[derive(Debug, serde::Deserialize)]
            #[serde(transparent)]
            struct Parameters(BTreeMap<OwnedUserId, SignedKeys>);

            let params: Parameters = req.body_json().unwrap();
            assert!(params.0.contains_key(user_id!("@alice:example.org")));

            ResponseTemplate::new(200).set_body_json(json!({
                "failures": {}
            }))
        })
        .mount(&server)
        .await;

    {
        // Login with username and password.
        let _guard = Mock::given(method("POST"))
            .and(path("/_matrix/client/r0/login"))
            .respond_with(|req: &Request| {
                #[derive(serde::Deserialize)]
                struct Parameters {
                    r#type: String,
                    password: String,
                }

                let params: Parameters = req.body_json().unwrap();
                assert_eq!(params.r#type, "m.login.password");
                assert_eq!(params.password, "hunter2");

                ResponseTemplate::new(200).set_body_json(json!({
                    "access_token": "abc123",
                    "device_id": "GHTYAJCE",
                    "home_server": "example.org",
                    "user_id": "@alice:example.org"
                }))
            })
            .mount_as_scoped(&server)
            .await;

        let client = Client::builder()
            .homeserver_url(server.uri())
            .server_versions([MatrixVersion::V1_0])
            .with_encryption_settings(matrix_sdk::encryption::EncryptionSettings {
                auto_enable_cross_signing: true,
            })
            .request_config(RequestConfig::new().disable_retry())
            .build()
            .await
            .unwrap();

        let auth = client.matrix_auth();
        auth.login_username("example", "hunter2").send().await.unwrap();

        assert!(client.logged_in(), "Client should be logged in");
        assert!(auth.logged_in(), "Client should be logged in with the MatrixAuth API");

        let me = client.user_id().expect("we are now logged in");
        let own_identity =
            client.encryption().get_user_identity(me).await.expect("succeeds").expect("is present");

        assert_eq!(own_identity.user_id(), me);
        assert!(own_identity.is_verified());
    }

    {
        // Login with a token.
        let _guard = Mock::given(method("POST"))
            .and(path("/_matrix/client/r0/login"))
            .respond_with(|req: &Request| {
                #[derive(serde::Deserialize)]
                struct Parameters {
                    r#type: String,
                    token: String,
                }

                let params: Parameters = req.body_json().unwrap();
                assert_eq!(params.r#type, "m.login.token");
                assert_eq!(params.token, "HUNTER2");

                ResponseTemplate::new(200).set_body_json(json!({
                    "access_token": "abc123",
                    "device_id": "GHTYAJCE",
                    "home_server": "example.org",
                    "user_id": "@alice:example.org"
                }))
            })
            .mount_as_scoped(&server)
            .await;

        let client = Client::builder()
            .homeserver_url(server.uri())
            .server_versions([MatrixVersion::V1_0])
            .with_encryption_settings(matrix_sdk::encryption::EncryptionSettings {
                auto_enable_cross_signing: true,
            })
            .request_config(RequestConfig::new().disable_retry())
            .build()
            .await
            .unwrap();

        let auth = client.matrix_auth();
        auth.login_token("HUNTER2").send().await.unwrap();

        assert!(client.logged_in(), "Client should be logged in");
        assert!(auth.logged_in(), "Client should be logged in with the MatrixAuth API");

        let me = client.user_id().expect("we are now logged in");
        let own_identity =
            client.encryption().get_user_identity(me).await.expect("succeeds").expect("is present");

        assert_eq!(own_identity.user_id(), me);
        assert!(own_identity.is_verified());
    }
}

#[cfg(feature = "e2e-encryption")]
#[async_test]
async fn test_login_with_cross_signing_bootstrapping_already_bootstrapped() {
    // Even if we enabled cross-signing bootstrap for another device, it won't
    // restart the procedure.
    let (builder, server) = test_client_builder().await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "abc123",
            "device_id": "FEJILWLI",
            "home_server": "example.org",
            "user_id": "@alice:example.org"
        })))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/client/r0/keys/query"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_keys": {
                "@alice:example.org": {
                    "GHTYAJCE": {
                      "user_id": "@alice:example.org",
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
                        "@alice:example.org": {
                          "ed25519:784pBUxon7VPcJJs69XkvN+AbC1ks07bvMh4qOPnVgY": "369BRaMHLW4nwrpy34eBYl0TpUeZoCs+IFXvTWJUBAv8Va4iqgB07Wi7XcJ+mmE4M7asyKnf5f7Zh4kGjOoNAQ"
                        }
                      }
                    }
                }
            },
            "failures": {},
            "master_keys": {
                "@alice:example.org": {
                    "user_id": "@alice:example.org",
                    "usage": [
                      "master"
                    ],
                    "keys": {
                      "ed25519:qGlcu2K7qaDn6wBG3DHOtnOeTgu6Dj1QLsxHSEGtODg": "qGlcu2K7qaDn6wBG3DHOtnOeTgu6Dj1QLsxHSEGtODg"
                    },
                    "signatures": {
                      "@alice:example.org": {
                        "ed25519:GHTYAJCE": "L3v/GSbEN+qO/vJipVupW6j3fHFn1CPSt8w5Ob0IpByM+LOuxKTc60kpisl94cueQZnl40mnKEFoYzI0JZWTDA",
                        "ed25519:qGlcu2K7qaDn6wBG3DHOtnOeTgu6Dj1QLsxHSEGtODg": "rb1Y9O5nfF0bU2p7aWF+I4095C4sm3uc/IWxdC55Q8GtrGFNsiR+YTvi3tJahMLDxYOCzgXl7dJ1mXsvzRNwBA"
                      }
                    }
                }
            },
            "self_signing_keys": {
                "@alice:example.org": {
                    "user_id": "@alice:example.org",
                    "usage": [
                      "self_signing"
                    ],
                    "keys": {
                      "ed25519:784pBUxon7VPcJJs69XkvN+AbC1ks07bvMh4qOPnVgY": "784pBUxon7VPcJJs69XkvN+AbC1ks07bvMh4qOPnVgY"
                    },
                    "signatures": {
                      "@alice:example.org": {
                        "ed25519:qGlcu2K7qaDn6wBG3DHOtnOeTgu6Dj1QLsxHSEGtODg": "TQQOP7BYFB6aZ/cVOa2qOzmzsap2kTpCLMEI1U8nO1kVtGRjXMGU+xoJ43DDWEgRvy2iUA7AMQpC1yCxo79BBA"
                      }
                    }
                }
            },
            "user_signing_keys": {
                "@alice:example.org": {
                    "user_id": "@alice:example.org",
                    "usage": [
                      "user_signing"
                    ],
                    "keys": {
                      "ed25519:D5nFYOzvmWUab4084Tahqhe4NgfQnuJ2XvdETSbOqrs": "D5nFYOzvmWUab4084Tahqhe4NgfQnuJ2XvdETSbOqrs"
                    },
                    "signatures": {
                      "@alice:example.org": {
                        "ed25519:qGlcu2K7qaDn6wBG3DHOtnOeTgu6Dj1QLsxHSEGtODg": "fFf76W6aPyxiwrINjlEjYxTIvC+35uth/WK7mzNLtQgHCGyzhJqRZECvHVQ4slr/oSu1EAAYJbAkq/QU0bniDg"
                      }
                    }
                }
            },
        })))
        .mount(&server)
        .await;

    let client = builder
        .with_encryption_settings(matrix_sdk::encryption::EncryptionSettings {
            auto_enable_cross_signing: true,
        })
        .request_config(RequestConfig::new().disable_retry())
        .build()
        .await
        .unwrap();

    let auth = client.matrix_auth();
    auth.login_username("example", "hunter2").send().await.unwrap();

    assert!(client.logged_in(), "Client should be logged in");
    assert!(auth.logged_in(), "Client should be logged in with the MatrixAuth API");
}
