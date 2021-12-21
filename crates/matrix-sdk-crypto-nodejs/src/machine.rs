use crate::device::Device;
use crate::models::{DecryptedEvent, DeviceLists};
use crate::request::{key_claim_to_request, outgoing_req_to_json, RequestKind};
use crate::responses::{response_from_string, OwnedResponse};
use matrix_sdk_common::{deserialized_responses::AlgorithmInfo, uuid::Uuid};
use matrix_sdk_crypto::OlmMachine as RSOlmMachine;
use napi::Result;
use napi_derive::napi;
use ruma::{
    api::{
        client::r0::{
            backup::add_backup_keys::Response as KeysBackupResponse,
            keys::{
                claim_keys::Response as KeysClaimResponse, get_keys::Response as KeysQueryResponse,
                upload_keys::Response as KeysUploadResponse,
                upload_signatures::Response as SignatureUploadResponse,
            },
            message::send_message_event::Response as RoomMessageResponse,
            sync::sync_events::{DeviceLists as RumaDeviceLists, ToDevice},
            to_device::send_event_to_device::Response as ToDeviceResponse,
        },
        IncomingResponse,
    },
    events::{room::encrypted::RoomEncryptedEventContent, SyncMessageEvent},
    DeviceKeyAlgorithm,
};
use ruma::{
    events::{AnyMessageEventContent, EventContent},
    RoomId, UInt, UserId,
};
use serde_json::value::RawValue;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::ops::Deref;
use tokio::runtime::Runtime;

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
            inner: runtime
                .block_on(RSOlmMachine::new_with_default_store(
                    &user_id, device_id, sled_path, None,
                ))
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
        self.inner
            .identity_keys()
            .iter()
            .map(|(k, v)| (k.to_string(), Value::String(v.to_string())))
            .collect()
    }

    // We can't use structured enums in napi-rs, so export as a series of JSON-serialized Strings instead
    #[napi(getter)]
    pub fn outgoing_requests(&self) -> Result<Vec<String>> {
        Ok(self
            .runtime
            .block_on(self.inner.outgoing_requests())
            .expect("Unknown error waiting for outgoing requests")
            .into_iter()
            .map(|r| outgoing_req_to_json(r).expect("Serialization failed"))
            .collect())
    }

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

        Ok(self
            .runtime
            .block_on(self.inner.get_device(&user_id, device_id.as_str().into()))
            .expect("Failed to get device info")
            .map(|d| d.into()))
    }

    #[napi]
    pub fn get_user_devices(&self, user_id: String) -> Result<Vec<Device>> {
        let user_id = Box::<UserId>::try_from(user_id).expect("Failed to parse user ID");

        Ok(self
            .runtime
            .block_on(self.inner.get_user_devices(&user_id))
            .expect("Failed to get user device info")
            .devices()
            .map(|d| d.into())
            .collect())
    }

    #[napi]
    pub fn mark_request_as_sent(
        &self,
        request_id: String,
        request_kind: RequestKind,
        response_body: String,
    ) -> Result<()> {
        let req_id = Uuid::parse_str(request_id.as_str()).expect("Failed to parse request ID");
        let response = response_from_string(response_body.as_str());

        let response: OwnedResponse = match request_kind {
            RequestKind::KeysUpload => {
                KeysUploadResponse::try_from_http_response(response).map(Into::into)
            }
            RequestKind::KeysQuery => {
                KeysQueryResponse::try_from_http_response(response).map(Into::into)
            }
            RequestKind::ToDevice => {
                ToDeviceResponse::try_from_http_response(response).map(Into::into)
            }
            RequestKind::KeysClaim => {
                KeysClaimResponse::try_from_http_response(response).map(Into::into)
            }
            RequestKind::SignatureUpload => {
                SignatureUploadResponse::try_from_http_response(response).map(Into::into)
            }
            RequestKind::KeysBackup => {
                KeysBackupResponse::try_from_http_response(response).map(Into::into)
            }
            RequestKind::RoomMessage => {
                RoomMessageResponse::try_from_http_response(response).map(Into::into)
            }
        }
        .expect("Can't convert json string to response");

        self.runtime
            .block_on(self.inner.mark_request_as_sent(&req_id, &response))
            .expect("Failed to mark request as sent");

        Ok(())
    }

    #[napi]
    pub fn receive_sync_changes(
        &self,
        events: String,
        device_changes: DeviceLists,
        key_counts: Map<String, Value>,
        unused_fallback_keys: Option<Vec<String>>,
    ) -> Result<String> {
        // key_counts: Map<String, i32>

        let events: ToDevice = serde_json::from_str(events.as_str())?;
        let device_changes: RumaDeviceLists = device_changes.into();
        let key_counts: BTreeMap<DeviceKeyAlgorithm, UInt> = key_counts
            .into_iter()
            .map(|(k, v)| {
                (
                    DeviceKeyAlgorithm::try_from(k).expect("Failed to convert key algorithm"),
                    i32::try_from(v.as_i64().expect("Failed to get number"))
                        .expect("Failed to downcast")
                        .clamp(0, i32::MAX)
                        .try_into()
                        .expect("Couldn't convert key counts into an UInt"),
                )
            })
            .collect();

        let unused_fallback_keys: Option<Vec<DeviceKeyAlgorithm>> =
            unused_fallback_keys.map(|u| u.into_iter().map(DeviceKeyAlgorithm::from).collect());

        let events = self
            .runtime
            .block_on(self.inner.receive_sync_changes(
                events,
                &device_changes,
                &key_counts,
                unused_fallback_keys.as_deref(),
            ))
            .expect("Failed to handle sync changes");

        Ok(serde_json::to_string(&events)?)
    }

    #[napi]
    pub fn update_tracked_users(&self, users: Vec<String>) {
        let users: Vec<Box<UserId>> =
            users.into_iter().filter_map(|u| Box::<UserId>::try_from(u).ok()).collect();

        self.runtime.block_on(self.inner.update_tracked_users(users.iter().map(Deref::deref)));
    }

    #[napi]
    pub fn is_user_tracked(&self, user_id: String) -> Result<bool> {
        let user_id = Box::<UserId>::try_from(user_id).expect("Failed to parse user ID");

        Ok(self.inner.tracked_users().contains(&user_id))
    }

    #[napi]
    pub fn get_missing_sessions(&self, users: Vec<String>) -> Result<String> {
        let users: Vec<Box<UserId>> =
            users.into_iter().filter_map(|u| Box::<UserId>::try_from(u).ok()).collect();

        Ok(self
            .runtime
            .block_on(self.inner.get_missing_sessions(users.iter().map(Deref::deref)))
            .expect("Failed to get missing sessions")
            .map(|r| key_claim_to_request(r))
            .expect("Failed to serialize")
            .expect("Failed to unpack"))
    }

    // TODO: Can we make this accept and return objects?
    #[napi]
    pub fn encrypt(&self, room_id: String, event_type: String, content: String) -> Result<String> {
        let room_id = Box::<RoomId>::try_from(room_id).expect("Failed to convert room ID");
        let content: Box<RawValue> =
            serde_json::from_str(content.as_str()).expect("Failed to convert content");
        let content = AnyMessageEventContent::from_parts(event_type.as_str(), &content)
            .expect("Failed to parse content");
        let encrypted_content = self
            .runtime
            .block_on(self.inner.encrypt(&room_id, content))
            .expect("Encrypting an event produced an error");

        Ok(serde_json::to_string(&encrypted_content).expect("Failed to convert encrypted content"))
    }

    // TODO: Can we make this accept and return objects?
    #[napi]
    pub fn decrypt_room_event(&self, event: String, room_id: String) -> Result<DecryptedEvent> {
        let event: SyncMessageEvent<RoomEncryptedEventContent> =
            serde_json::from_str(event.as_str()).expect("Failed to parse event");
        let room_id = Box::<RoomId>::try_from(room_id).expect("Failed to parse room ID");

        let decrypted = self
            .runtime
            .block_on(self.inner.decrypt_room_event(&event, &room_id))
            .expect("Failed to decrypt");

        let encryption_info =
            decrypted.encryption_info.expect("Decrypted event didn't contain any encryption info");

        Ok(match &encryption_info.algorithm_info {
            AlgorithmInfo::MegolmV1AesSha2 {
                curve25519_key,
                sender_claimed_keys,
                forwarding_curve25519_key_chain,
            } => DecryptedEvent {
                clear_event: serde_json::to_string(decrypted.event.json().get())
                    .expect("Failed to serialize clear event"),
                sender_curve25519_key: curve25519_key.to_owned(),
                claimed_ed25519_key: sender_claimed_keys.get(&DeviceKeyAlgorithm::Ed25519).cloned(),
                forwarding_curve25519_chain: forwarding_curve25519_key_chain.to_owned(),
            },
        })
    }

    // TODO: Can we make this accept and return objects?
    #[napi]
    pub fn sign(&self, message: String) -> Result<String> {
        Ok(serde_json::to_string(&self.runtime.block_on(self.inner.sign(message.as_str())))
            .expect("Failed to serialize"))
    }
}
