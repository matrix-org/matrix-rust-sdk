mod backups;
mod cross_signing;
mod recovery;
mod secret_storage;
mod shared_history;
#[cfg(feature = "experimental-encrypted-state-events")]
mod state_events;
mod to_device;
mod verification;

/// The backup key, which is also returned (encrypted) as part of the secret
/// storage data by [`mock_secret_store_with_backup_key`].
const BACKUP_DECRYPTION_KEY_BASE64: &str = "IeJv45zHC4AamrFtCi3MedkLMBZYXjPbKUnv53Iqzho";

async fn mock_secret_store_with_backup_key(
    user_id: &ruma::UserId,
    key_id: &str,
    server: &wiremock::MockServer,
) {
    use serde_json::json;
    use wiremock::{
        Mock, ResponseTemplate,
        matchers::{header, method, path},
    };

    Mock::given(method("GET"))
        .and(path(format!(
            "_matrix/client/r0/user/{user_id}/account_data/m.secret_storage.default_key"
        )))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "key": key_id,
        })))
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
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("_matrix/client/r0/user/{user_id}/account_data/m.cross_signing.master")))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "Account data not found"
        })))
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
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("_matrix/client/r0/keys/query"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_keys": {}
        })))
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
        .mount(server)
        .await;
}
