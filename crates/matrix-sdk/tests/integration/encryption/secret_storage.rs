use std::sync::{Arc, Mutex};

use assert_matches::assert_matches;
use matrix_sdk::{
    authentication::matrix::MatrixSession,
    encryption::secret_storage::SecretStorageError,
    test_utils::{client::mock_session_tokens, no_retry_test_client_with_server},
};
use matrix_sdk_base::SessionMeta;
use matrix_sdk_test::async_test;
use ruma::{
    UserId, device_id,
    events::{
        secret::request::SecretName,
        secret_storage::{
            default_key::SecretStorageDefaultKeyEventContent, secret::SecretEventContent,
        },
    },
    user_id,
};
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path, path_regex},
};

use crate::logged_in_client_with_server;

const SECRET_STORE_KEY: &str = "EsTj 3yST y93F SLpB jJsz eAXc 2XzA ygD3 w69H fGaN TKBj jXEd";

async fn mock_secret_store_key(
    server: &MockServer,
    user_id: &UserId,
    key_id: &str,
    iv: &str,
    mac: &str,
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
        .named("default_key account data GET")
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.key.{key_id}"
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "algorithm": "m.secret_storage.v1.aes-hmac-sha2",
            "iv": iv,
            "mac": mac,
        })))
        .expect(1..)
        .named("m.direct account data GET")
        .mount(server)
        .await;
}

#[async_test]
async fn test_secret_store_create_default_key() {
    let (client, server) = logged_in_client_with_server().await;

    let user_id = client.user_id().expect("We should know our user ID by now");

    let key_id: Arc<Mutex<Option<String>>> = Mutex::new(None).into();

    let put_new_key_matcher = {
        let key_id = key_id.to_owned();

        move |request: &wiremock::Request| {
            let mut path_segments =
                request.url.path_segments().expect("The URL should be able to be a base");

            let key_id_segment = path_segments
                .next_back()
                .expect("The path should have a key ID as the last segment")
                .to_owned();

            *key_id.lock().unwrap() = Some(key_id_segment);

            true
        }
    };

    let put_new_key_id_matcher = move |request: &wiremock::Request| {
        let key_id = key_id.lock().unwrap().take().expect("We should know our new key ID by now");

        let content: SecretStorageDefaultKeyEventContent =
            request.body_json().expect("The content should be a default key event content");

        assert_eq!(
            key_id,
            format!("m.secret_storage.key.{}", content.key_id),
            "The key ID of the key we created should be the same as the key ID we're marking as the default one"
        );

        true
    };

    Mock::given(method("PUT"))
        .and(path_regex(format!(
            r"_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.key.[A-Za-z0-9]"
        )))
        .and(header("authorization", "Bearer 1234"))
        .and(put_new_key_matcher)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .named("m.direct account data GET")
        .mount(&server)
        .await;

    Mock::given(method("PUT"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.default_key"
        )))
        .and(header("authorization", "Bearer 1234"))
        .and(put_new_key_id_matcher)
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
        .expect(1)
        .named("default_key account data GET")
        .mount(&server)
        .await;

    let _ = client
        .encryption()
        .secret_storage()
        .create_secret_store()
        .await
        .expect("We should be able to create a new secret store");

    server.verify().await;
}

#[async_test]
async fn test_secret_store_missing_key_info() {
    let (client, server) = logged_in_client_with_server().await;

    let user_id = client.user_id().expect("We should know our user ID by now");
    let key_id = "bmur2d9ypPUH1msSwCxQOJkuKRmJI55e";

    Mock::given(method("GET"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.default_key"
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "key": key_id
        })))
        .expect(1..)
        .named("default_key account data GET")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.key.{key_id}"
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "Account data not found"
        })))
        .expect(1)
        .named("m.direct account data GET")
        .mount(&server)
        .await;

    let ret = client.encryption().secret_storage().open_secret_store(SECRET_STORE_KEY).await;

    let found_key_id = assert_matches!(
        ret,
        Err(SecretStorageError::MissingKeyInfo { key_id: Some(key_id) }) => key_id,
        "We should report that the key info for the default key is missing"
    );

    assert_eq!(
        key_id, found_key_id,
        "The key ID in the error should match to the key ID of the reported default key"
    );

    server.verify().await;
}

#[async_test]
async fn test_secret_store_not_setup() {
    let (client, server) = logged_in_client_with_server().await;

    let user_id = client.user_id().expect("We should know our user ID by now");

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
        .named("default_key account data GET")
        .mount(&server)
        .await;

    let ret = client.encryption().secret_storage().open_secret_store(SECRET_STORE_KEY).await;

    assert_matches!(
        ret,
        Err(SecretStorageError::MissingKeyInfo { key_id: None }),
        "We should report that the key info for the default key is missing"
    );

    server.verify().await;
}

#[async_test]
async fn test_secret_store_opening() {
    let (client, server) = logged_in_client_with_server().await;

    mock_secret_store_key(
        &server,
        client.user_id().unwrap(),
        "bmur2d9ypPUH1msSwCxQOJkuKRmJI55e",
        "xv5b6/p3ExEw++wTyfSHEg==",
        "ujBBbXahnTAMkmPUX2/0+VTfUh63pGyVRuBcDMgmJC8=",
    )
    .await;

    Mock::given(method("GET"))
        .and(path("_matrix/client/r0/user/@example:localhost/account_data/m.cross_signing.master"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "encrypted": {
                "bmur2d9ypPUH1msSwCxQOJkuKRmJI55e": {
                    "ciphertext": "lCRSSA1lChONEXj/8RyogsgAa8ouQwYDnLr4XBCheRikrZykLRzPCx3doCE=",
                    "iv": "bdfCwu+ECYgZ/jWTkGrQ/A==",
                    "mac": "NXeV1dZaOe2JLvQ6Hh6tFto7AgFFdaQnY0l9pruwdtE="
                }
            }
        })))
        .expect(1..)
        .named("m.direct account data GET")
        .mount(&server)
        .await;

    let secret_store = client
        .encryption()
        .secret_storage()
        .open_secret_store(SECRET_STORE_KEY)
        .await
        .expect("We should be able to open our secret store");

    let secret = secret_store
        .get_secret(SecretName::CrossSigningMasterKey)
        .await
        .expect("We should be able to retrieve a secret from the secret store")
        .unwrap();

    assert_eq!(secret, "VcY1+LNyV6aSMne8mvi4lc/q+JiCHe+m6+hKLtgLP30=");

    server.verify().await;
}

#[async_test]
async fn test_set_in_secret_store() {
    let (client, server) = logged_in_client_with_server().await;

    mock_secret_store_key(
        &server,
        client.user_id().unwrap(),
        "bmur2d9ypPUH1msSwCxQOJkuKRmJI55e",
        "xv5b6/p3ExEw++wTyfSHEg==",
        "ujBBbXahnTAMkmPUX2/0+VTfUh63pGyVRuBcDMgmJC8=",
    )
    .await;

    let secret_store = client
        .encryption()
        .secret_storage()
        .open_secret_store(SECRET_STORE_KEY)
        .await
        .expect("We should be able to open our secret store");

    let uploaded_content: Arc<Mutex<Option<SecretEventContent>>> = Mutex::new(None).into();

    {
        // This mock is scoped because, at first we don't have a `foo` event in our
        // account data. Only when we call `secret_store.set_secret()` will we
        // have one, and a different mock will be required for the next GET request.
        let _guard = Mock::given(method("GET"))
            .and(path("_matrix/client/r0/user/@example:localhost/account_data/foo"))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "errcode": "M_NOT_FOUND",
                "error": "Account data not found"
            })))
            .expect(1)
            .named("foo account data GET")
            .mount_as_scoped(&server)
            .await;

        // Create a custom matcher to extract the body so we can put it into the
        // response for the next GET /account_data request.
        let put_matcher = {
            let uploaded_content = uploaded_content.to_owned();

            move |request: &wiremock::Request| {
                let content: SecretEventContent =
                    request.body_json().expect("The request body should be a SecretEventContent");

                *uploaded_content.lock().unwrap() = Some(content);

                true
            }
        };

        Mock::given(method("PUT"))
            .and(path("_matrix/client/r0/user/@example:localhost/account_data/foo"))
            .and(header("authorization", "Bearer 1234"))
            .and(put_matcher)
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .named("foo account data PUT")
            .mount(&server)
            .await;

        secret_store
            .put_secret("foo", "It's a secret to everybody")
            .await
            .expect("We should be able to store a secret to the secret store");
    }

    let uploaded_content = uploaded_content
        .lock()
        .unwrap()
        .take()
        .expect("The secret content should have been uploaded");

    let uploaded_content = serde_json::to_value(uploaded_content).unwrap();

    Mock::given(method("GET"))
        .and(path("_matrix/client/r0/user/@example:localhost/account_data/foo"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(uploaded_content))
        .expect(1)
        .named("foo account data GET")
        .mount(&server)
        .await;

    let secret = secret_store
        .get_secret("foo")
        .await
        .expect("We should be able to retrieve a secret from the secret store")
        .unwrap();

    assert_eq!(secret, "It's a secret to everybody");

    server.verify().await;
}

#[async_test]
async fn test_restore_cross_signing_from_secret_store() {
    let user_id = user_id!("@example:morpheus.localhost");

    let session = MatrixSession {
        meta: SessionMeta {
            user_id: user_id!("@example:morpheus.localhost").to_owned(),
            device_id: device_id!("DEVICEID").to_owned(),
        },
        tokens: mock_session_tokens(),
    };
    let (client, server) = no_retry_test_client_with_server().await;
    client.restore_session(session).await.unwrap();

    mock_secret_store_key(
        &server,
        user_id,
        "bmur2d9ypPUH1msSwCxQOJkuKRmJI55e",
        "xv5b6/p3ExEw++wTyfSHEg==",
        "ujBBbXahnTAMkmPUX2/0+VTfUh63pGyVRuBcDMgmJC8=",
    )
    .await;

    Mock::given(method("POST"))
        .and(path("_matrix/client/r0/keys/query"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "master_keys": {
                "@example:morpheus.localhost": {
                    "keys": {
                        "ed25519:fKJdf5b3ga1hshUT5obkBteMNTtLkfy8qh4h5/XLNew": "fKJdf5b3ga1hshUT5obkBteMNTtLkfy8qh4h5/XLNew"
                    },
                    "signatures": {
                        "@example:morpheus.localhost": {
                            "ed25519:QLEYYETKXR": "2UCY7IS+5NFdzGGPgyovI2+uHM13pbrsAJIUp0daUCzJdl6hvp5rK/6L5AgyxiVAhbK/XT4lDDJeSymeJAs7Cg"
                        }
                    },
                    "usage": [
                        "master"
                    ],
                    "user_id": "@example:morpheus.localhost"
                }
            },
            "self_signing_keys": {
                "@example:morpheus.localhost": {
                    "keys": {
                        "ed25519:vsEoa0HHxc8hmlWgoohTQBGWcCRh1BFoTzI8HNIxg5Q": "vsEoa0HHxc8hmlWgoohTQBGWcCRh1BFoTzI8HNIxg5Q"
                    },
                    "signatures": {
                        "@example:morpheus.localhost": {
                            "ed25519:fKJdf5b3ga1hshUT5obkBteMNTtLkfy8qh4h5/XLNew": "YFvSUX40E81EO8jibgn+kklMFkqfGTDPcENsq6UEepMY9UUOp9iredNCA/NR2INlfq4gkQhMJk2QRGZ31pQZCg"
                        }
                    },
                    "usage": [
                        "self_signing"
                    ],
                    "user_id": "@example:morpheus.localhost"
                }
            },
            "user_signing_keys": {
                "@example:morpheus.localhost": {
                    "keys": {
                        "ed25519:IGfFj6KhzqBWxiMm6/e+Pu/x/5NV6a7iEe8AKhuRiq0": "IGfFj6KhzqBWxiMm6/e+Pu/x/5NV6a7iEe8AKhuRiq0"
                    },
                    "signatures": {
                        "@example:morpheus.localhost": {
                            "ed25519:fKJdf5b3ga1hshUT5obkBteMNTtLkfy8qh4h5/XLNew": "+b9IdTCC7WezR/XJX+K/wI7DukcTOfBukyy9iyZJP3SFZCNWVRmMG1Kuc8iQg8txSNY5NP0M3PI/Srv9nHZjDQ"
                        }
                    },
                      "usage": [
                        "user_signing"
                    ],
                    "user_id": "@example:morpheus.localhost"
                }
            }
        })))
        .expect(2)
        .named("/keys/query POST")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("_matrix/client/r0/user/@example:morpheus.localhost/account_data/m.cross_signing.master"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "encrypted": {
                "bmur2d9ypPUH1msSwCxQOJkuKRmJI55e": {
                    "ciphertext": "lCRSSA1lChONEXj/8RyogsgAa8ouQwYDnLr4XBCheRikrZykLRzPCx3doCE=",
                    "iv": "bdfCwu+ECYgZ/jWTkGrQ/A==",
                    "mac": "NXeV1dZaOe2JLvQ6Hh6tFto7AgFFdaQnY0l9pruwdtE="
                }
            }
        })))
        .expect(1)
        .named("m.direct account data GET")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(
            "_matrix/client/r0/user/@example:morpheus.localhost/account_data/m.cross_signing.self_signing",
        ))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "encrypted": {
                "bmur2d9ypPUH1msSwCxQOJkuKRmJI55e": {
                    "ciphertext": "+B9WD02IvtQ8S4OaquhuYEZAx20xvz0oTN7r2VM9VOBxmlOyi+KkkWOvLAo=",
                    "iv": "3BCaKGCaSMkg1x9WnTqUmw==",
                    "mac": "xQEDxQbPH0bYeZUFC3wYJh0lsLkP2amcFGdaZ3VdfQg="
                }
            }
        })))
        .expect(1)
        .named("m.direct account data GET")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(
            "_matrix/client/r0/user/@example:morpheus.localhost/account_data/m.cross_signing.user_signing",
        ))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "encrypted": {
                "bmur2d9ypPUH1msSwCxQOJkuKRmJI55e": {
                    "ciphertext": "atqNy5IDzYRkRC+lkKoflwsyHkd0dr4UeoViwJdUzexiq0M8h1i8JMkADNg=",
                    "iv": "bjb1V2n9YmA8j31Z9muMqQ==",
                    "mac": "vusvNuV8Kkq50VxtC78oioofVBurnTTVEhiRyZkfu/4="
                }
            }
        })))
        .expect(1)
        .named("m.direct account data GET")
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path(
            "_matrix/client/r0/user/@example:morpheus.localhost/account_data/m.megolm_backup.v1",
        ))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "Account data not found"
        })))
        .expect(1)
        .named("m.megolm_backup.v1 account data GET")
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("_matrix/client/unstable/keys/signatures/upload"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "failures": {}
        })))
        .expect(1)
        .named("signatures upload POST")
        .mount(&server)
        .await;

    let secret_store = client
        .encryption()
        .secret_storage()
        .open_secret_store(SECRET_STORE_KEY)
        .await
        .expect("We should be able to open our secret store");

    let status = client
        .encryption()
        .cross_signing_status()
        .await
        .expect("We should be able to check our cross-signing status");

    assert!(!status.has_master, "Initially we should not have access to our cross signing key");

    secret_store
        .import_secrets()
        .await
        .expect("We should be able to import all our known secrets from 4S");

    let status = client
        .encryption()
        .cross_signing_status()
        .await
        .expect("We should be able to check our cross-signing status");

    assert!(
        status.is_complete(),
        "We should have access to our cross signing key after the import"
    );

    assert_eq!(
        SECRET_STORE_KEY,
        secret_store.secret_storage_key(),
        "We should be able to retrieve the secret storage key from the store",
    );

    server.verify().await;
}

#[async_test]
async fn test_is_secret_storage_enabled() {
    let user_id = user_id!("@example:morpheus.localhost");

    let session = MatrixSession {
        meta: SessionMeta {
            user_id: user_id!("@example:morpheus.localhost").to_owned(),
            device_id: device_id!("DEVICEID").to_owned(),
        },
        tokens: mock_session_tokens(),
    };
    let (client, server) = no_retry_test_client_with_server().await;
    client.restore_session(session).await.unwrap();

    {
        let _scope = Mock::given(method("GET"))
            .and(path(format!(
                "_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.default_key"
            )))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                "errcode": "M_NOT_FOUND",
                "error": "Account data not found"
            })))
            .expect(1..)
            .named("default_key account data GET")
            .mount_as_scoped(&server)
            .await;

        let enabled = client
            .encryption()
            .secret_storage()
            .is_enabled()
            .await
            .expect("We should be able to check if secret storage is enabled");

        assert!(
            !enabled,
            "If we didn't find the default key account data event, we should assume that \
             secret storage is disabled."
        );
    }

    {
        let _scope = Mock::given(method("GET"))
            .and(path(format!(
                "_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.default_key"
            )))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({})))
            .expect(1)
            .named("default_key account data GET")
            .mount_as_scoped(&server)
            .await;

        let enabled = client
            .encryption()
            .secret_storage()
            .is_enabled()
            .await
            .expect("We should be able to check if secret storage is enabled");

        assert!(
            !enabled,
            "If deserialization of the default key account data event failed, we should assume \
             that secret storage is disabled"
        );
    }

    {
        let _scope = Mock::given(method("GET"))
            .and(path(format!(
                "_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.default_key"
            )))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "key": "some_key_id",
            })))
            .expect(1)
            .named("default_key account data GET")
            .mount_as_scoped(&server)
            .await;

        let enabled = client
            .encryption()
            .secret_storage()
            .is_enabled()
            .await
            .expect("We should be able to check if secret storage is enabled");

        assert!(
            enabled,
            "If there is a default key event and deserialization did not fail, we're assuming \
             that secret storage is enabled"
        );
    }
}
