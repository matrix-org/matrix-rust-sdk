use std::ops::Deref;
use napi_derive::napi;
use napi::{Result};
use ruma::{RoomId, UserId, events::{{AnyMessageEventContent}}};
use serde_json::{Map, Value};
use serde_json::value::RawValue;
use tokio::runtime::Runtime;
use matrix_sdk_crypto::{
    OlmMachine as RSOlmMachine,
};
use crate::device::Device;

#[napi]
pub struct SledBackedOlmMachine {
    inner: RSOlmMachine,
    runtime: Runtime,
}

#[napi]
impl SledBackedOlmMachine {
    #[napi(constructor)]
    pub fn new(user_id: String, device_id: String, sled_path: String) -> Result<Self> {
        let user_id = Box::<UserId>::try_from(user_id.as_str()).expect("Failed to parse user ID");
        let device_id = device_id.as_str().into();
        let sled_path = sled_path.as_str();
        let runtime = Runtime::new().expect("Couldn't create a tokio runtime");
        Ok(SledBackedOlmMachine {
            // TODO: Should we be passing a passphrase through?
            inner: runtime.block_on(RSOlmMachine::new_with_default_store(&user_id, device_id, sled_path, None))
                .expect("Failed to create inner Olm machine"),
            runtime,
        })
    }

    #[napi(getter)]
    pub fn user_id(&self) -> String {
        self.inner.user_id().to_string()
    }

    #[napi(getter)]
    pub fn device_id(&self) -> String {
        self.inner.device_id().to_string()
    }

    #[napi(getter)]
    pub fn device_display_name(&self) -> Result<Option<String>> {
        Ok(self.runtime.block_on(self.inner.display_name()).expect("Failed to get display name"))
    }

    #[napi(getter)]
    pub fn identity_keys(&self) -> Map<String, Value> {
        self.inner.identity_keys().iter().map(|(k, v)| (k.to_string(), Value::String(v.to_string()))).collect()
    }

    // TODO: TravisR
    // #[napi(getter)]
    // pub fn outgoing_requests(&self) -> Result<Vec<Request>> {
    //     Ok(self
    //         .runtime
    //         .block_on(self.inner.outgoing_requests())?
    //         .into_iter()
    //         .map(|r| r.into())
    //         .collect())
    // }

    // Function names from https://github.com/poljar/element-android/blob/rust/rust-sdk/src/machine.rs
    // Some of the functions might be best served as getters (put above this comment block, with the other ones)
    // TODO: get_identity
    // TODO: is_identity_verified
    // TODO: verify_identity
    // TODO: verify_device
    // TODO: mark_device_as_trusted
    // TODO: share_room_key
    // TODO: request_room_key
    // TODO: export_keys
    // TODO: import_keys
    // TODO: import_decrypted_keys
    // TODO: discard_room_key
    // TODO: get_verification_requests
    // TODO: get_verification_request
    // TODO: accept_verification_request
    // TODO: verification_request_content
    // TODO: request_verification
    // TODO: request_verification_with_device
    // TODO: request_self_verification
    // TODO: get_verification
    // TODO: cancel_verification
    // TODO: confirm_verification
    // TODO: start_qr_verification
    // TODO: generate_qr_code
    // TODO: scan_qr_code
    // TODO: start_sas_verification
    // TODO: start_sas_with_device
    // TODO: accept_sas_verification
    // TODO: get_emoji_index
    // TODO: get_decimals
    // TODO: bootstrap_cross_signing
    // TODO: cross_signing_status
    // TODO: export_cross_signing_keys
    // TODO: import_cross_signing_keys
    // TODO: enable_backup_v1
    // TODO: backup_enabled
    // TODO: disable_backup
    // TODO: backup_room_keys
    // TODO: room_key_counts
    // TODO: save_recovery_key
    // TODO: verify_backup

    #[napi]
    pub fn get_device(&self, user_id: String, device_id: String) -> Result<Option<Device>> {
        let user_id = Box::<UserId>::try_from(user_id).expect("Failed to parse user ID");

        Ok(self.runtime.block_on(self.inner.get_device(&user_id, device_id.as_str().into())).expect("Failed to get device info").map(|d| d.into()))
    }

    #[napi]
    pub fn get_user_devices(&self, user_id: String) -> Result<Vec<Device>> {
        let user_id = Box::<UserId>::try_from(user_id).expect("Failed to parse user ID");

        Ok(self.runtime.block_on(self.inner.get_user_devices(&user_id)).expect("Failed to get user device info").devices().map(|d| d.into()).collect())
    }

    // TODO: TravisR
    // #[napi]
    // pub fn mark_request_as_sent(&self, request_id: String, request_type: RequestType, response_body: String) -> Result<()> {
    //     // TODO: Impl
    // }

    // TODO: TravisR
    // #[napi]
    // pub fn receive_sync_changes(&self, events: String, device_changes: DeviceLists, key_counts: Map<String, Value>, unused_fallback_keys: Option<Vec<String>>) -> Result<String> {
    //     // TODO: Impl
    // }

    #[napi]
    pub fn update_tracked_users(&self, users: Vec<String>) {
        let users: Vec<Box<UserId>> = users
            .into_iter()
            .filter_map(|u| Box::<UserId>::try_from(u).ok())
            .collect();

        self.runtime.block_on(
            self.inner
                .update_tracked_users(users.iter().map(Deref::deref)),
        );
    }

    #[napi]
    pub fn is_user_tracked(&self, user_id: String) -> Result<bool> {
        let user_id = Box::<UserId>::try_from(user_id).expect("Failed to parse user ID");

        Ok(self.inner.tracked_users().contains(&user_id))
    }

    // TODO: TravisR
    // #[napi]
    // pub fn get_missing_sessions(&self, users: Vec<String>) -> Result<Request> {
    //     // TODO: Impl
    // }

    // TODO: Can we make this accept and return objects?
    #[napi]
    pub fn encrypt(&self, room_id: String, event_type: String, content: String) -> Result<String> {
        let room_id = Box::<RoomId>::try_from(room_id).expect("Failed to convert room ID");
        let content: Box<RawValue> = serde_json::from_str(content.as_str()).expect("Failed to convert content");
        let content = AnyMessageEventContent::from_parts(event_type.as_str(), &content).failed("Failed to parse content");
        let encrypted_content = self
            .runtime
            .block_on(self.inner.encrypt(&room_id, content))
            .expect("Encrypting an event produced an error");

        Ok(serde_json::to_string(&encrypted_content).expect("Failed to convert encrypted content"))
    }

    // TODO: TravisR
    // #[napi]
    // pub fn decrypt_room_event(&self, event: String, room_id: String) -> Result<DecryptedEvent> {
    //     // TODO: Impl
    //     // TODO: Try to return objects
    // }

    // TODO: TravisR
    // #[napi]
    // pub fn sign(&self, message: String) -> Map<String, Value> {
    //     // TODO: Impl
    // }
}

