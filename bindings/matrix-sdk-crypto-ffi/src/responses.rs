#![allow(missing_docs)]

use std::collections::HashMap;

use http::Response;
use matrix_sdk_crypto::{
    types::requests::{
        AnyIncomingResponse, KeysBackupRequest, OutgoingRequest,
        OutgoingVerificationRequest as SdkVerificationRequest, RoomMessageRequest, ToDeviceRequest,
        UploadSigningKeysRequest as RustUploadSigningKeysRequest,
    },
    CrossSigningBootstrapRequests,
};
use ruma::{
    api::client::{
        backup::add_backup_keys::v3::Response as KeysBackupResponse,
        keys::{
            claim_keys::v3::{Request as KeysClaimRequest, Response as KeysClaimResponse},
            get_keys::v3::Response as KeysQueryResponse,
            upload_keys::v3::Response as KeysUploadResponse,
            upload_signatures::v3::{
                Request as RustSignatureUploadRequest, Response as SignatureUploadResponse,
            },
        },
        message::send_message_event::v3::Response as RoomMessageResponse,
        sync::sync_events::DeviceLists as RumaDeviceLists,
        to_device::send_event_to_device::v3::Response as ToDeviceResponse,
    },
    assign,
    events::MessageLikeEventContent,
    OwnedTransactionId, UserId,
};
use serde_json::json;

#[derive(uniffi::Record)]
pub struct SignatureUploadRequest {
    pub body: String,
}

impl From<RustSignatureUploadRequest> for SignatureUploadRequest {
    fn from(r: RustSignatureUploadRequest) -> Self {
        Self {
            body: serde_json::to_string(&r.signed_keys)
                .expect("Can't serialize signature upload request"),
        }
    }
}

#[derive(uniffi::Record)]
pub struct UploadSigningKeysRequest {
    pub master_key: String,
    pub self_signing_key: String,
    pub user_signing_key: String,
}

impl From<RustUploadSigningKeysRequest> for UploadSigningKeysRequest {
    fn from(r: RustUploadSigningKeysRequest) -> Self {
        Self {
            master_key: serde_json::to_string(
                &r.master_key.expect("Request didn't contain a master key"),
            )
            .expect("Can't serialize cross signing master key"),
            self_signing_key: serde_json::to_string(
                &r.self_signing_key.expect("Request didn't contain a self-signing key"),
            )
            .expect("Can't serialize cross signing self-signing key"),
            user_signing_key: serde_json::to_string(
                &r.user_signing_key.expect("Request didn't contain a user-signing key"),
            )
            .expect("Can't serialize cross signing user-signing key"),
        }
    }
}

#[derive(uniffi::Record)]
pub struct BootstrapCrossSigningResult {
    /// Optional request to upload some device keys alongside.
    ///
    /// Must be sent first if present, and marked with `mark_request_as_sent`.
    pub upload_keys_request: Option<Request>,
    /// Request to upload the signing keys. Must be sent second.
    pub upload_signing_keys_request: UploadSigningKeysRequest,
    /// Request to upload the keys signatures, including for the signing keys
    /// and optionally for the device keys. Must be sent last.
    pub upload_signature_request: SignatureUploadRequest,
}

impl From<CrossSigningBootstrapRequests> for BootstrapCrossSigningResult {
    fn from(requests: CrossSigningBootstrapRequests) -> Self {
        Self {
            upload_signing_keys_request: requests.upload_signing_keys_req.into(),
            upload_keys_request: requests.upload_keys_req.map(Request::from),
            upload_signature_request: requests.upload_signatures_req.into(),
        }
    }
}

#[derive(uniffi::Enum)]
pub enum OutgoingVerificationRequest {
    ToDevice { request_id: String, event_type: String, body: String },
    InRoom { request_id: String, room_id: String, event_type: String, content: String },
}

impl From<SdkVerificationRequest> for OutgoingVerificationRequest {
    fn from(r: SdkVerificationRequest) -> Self {
        match r {
            SdkVerificationRequest::ToDevice(r) => r.into(),
            SdkVerificationRequest::InRoom(r) => Self::InRoom {
                request_id: r.txn_id.to_string(),
                room_id: r.room_id.to_string(),
                content: serde_json::to_string(&r.content)
                    .expect("Can't serialize message content"),
                event_type: r.content.event_type().to_string(),
            },
        }
    }
}

impl From<ToDeviceRequest> for OutgoingVerificationRequest {
    fn from(r: ToDeviceRequest) -> Self {
        Self::ToDevice {
            request_id: r.txn_id.to_string(),
            event_type: r.event_type.to_string(),
            body: serde_json::to_string(&r.messages).expect("Can't serialize to-device body"),
        }
    }
}

#[derive(Debug, uniffi::Enum)]
pub enum Request {
    ToDevice { request_id: String, event_type: String, body: String },
    KeysUpload { request_id: String, body: String },
    KeysQuery { request_id: String, users: Vec<String> },
    KeysClaim { request_id: String, one_time_keys: HashMap<String, HashMap<String, String>> },
    KeysBackup { request_id: String, version: String, rooms: String },
    RoomMessage { request_id: String, room_id: String, event_type: String, content: String },
    SignatureUpload { request_id: String, body: String },
}

impl From<OutgoingRequest> for Request {
    fn from(r: OutgoingRequest) -> Self {
        use matrix_sdk_crypto::types::requests::AnyOutgoingRequest::*;

        match r.request() {
            KeysUpload(u) => {
                let body = json!({
                    "device_keys": u.device_keys,
                    "one_time_keys": u.one_time_keys,
                    "fallback_keys": u.fallback_keys,
                });

                Request::KeysUpload {
                    request_id: r.request_id().to_string(),
                    body: serde_json::to_string(&body)
                        .expect("Can't serialize `/keys/upload` request"),
                }
            }
            KeysQuery(k) => {
                let users: Vec<String> = k.device_keys.keys().map(|u| u.to_string()).collect();
                Request::KeysQuery { request_id: r.request_id().to_string(), users }
            }
            ToDeviceRequest(t) => Request::from(t),
            SignatureUpload(t) => Request::SignatureUpload {
                request_id: r.request_id().to_string(),
                body: serde_json::to_string(&t.signed_keys)
                    .expect("Can't serialize signature upload request"),
            },
            RoomMessage(r) => Request::from(r),
            KeysClaim(c) => (r.request_id().to_owned(), c.clone()).into(),
        }
    }
}

impl From<ToDeviceRequest> for Request {
    fn from(r: ToDeviceRequest) -> Self {
        Request::ToDevice {
            request_id: r.txn_id.to_string(),
            event_type: r.event_type.to_string(),
            body: serde_json::to_string(&r.messages).expect("Can't serialize to-device body"),
        }
    }
}

impl From<(OwnedTransactionId, KeysClaimRequest)> for Request {
    fn from(request_tuple: (OwnedTransactionId, KeysClaimRequest)) -> Self {
        let (request_id, request) = request_tuple;

        Request::KeysClaim {
            request_id: request_id.to_string(),
            one_time_keys: request
                .one_time_keys
                .into_iter()
                .map(|(u, d)| {
                    (
                        u.to_string(),
                        d.into_iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
                    )
                })
                .collect(),
        }
    }
}

impl From<(OwnedTransactionId, KeysBackupRequest)> for Request {
    fn from(request_tuple: (OwnedTransactionId, KeysBackupRequest)) -> Self {
        let (request_id, request) = request_tuple;

        Request::KeysBackup {
            request_id: request_id.to_string(),
            version: request.version.to_owned(),
            rooms: serde_json::to_string(&request.rooms)
                .expect("Can't serialize keys backup request"),
        }
    }
}

impl From<&ToDeviceRequest> for Request {
    fn from(r: &ToDeviceRequest) -> Self {
        Request::ToDevice {
            request_id: r.txn_id.to_string(),
            event_type: r.event_type.to_string(),
            body: serde_json::to_string(&r.messages).expect("Can't serialize to-device body"),
        }
    }
}

impl From<&Box<RoomMessageRequest>> for Request {
    fn from(r: &Box<RoomMessageRequest>) -> Self {
        Self::RoomMessage {
            request_id: r.txn_id.to_string(),
            room_id: r.room_id.to_string(),
            event_type: r.content.event_type().to_string(),
            content: serde_json::to_string(&r.content).expect("Can't serialize message content"),
        }
    }
}

pub(crate) fn response_from_string(body: &str) -> Response<Vec<u8>> {
    Response::builder()
        .status(200)
        .body(body.as_bytes().to_vec())
        .expect("Can't create HTTP response")
}

#[derive(uniffi::Enum)]
pub enum RequestType {
    KeysQuery,
    KeysClaim,
    KeysUpload,
    ToDevice,
    SignatureUpload,
    KeysBackup,
    RoomMessage,
}

#[derive(uniffi::Record)]
pub struct DeviceLists {
    pub changed: Vec<String>,
    pub left: Vec<String>,
}

impl From<DeviceLists> for RumaDeviceLists {
    fn from(d: DeviceLists) -> Self {
        assign!(RumaDeviceLists::new(), {
            changed: d
                .changed
                .into_iter()
                .filter_map(|u| UserId::parse(u).ok())
                .collect(),
            left: d
                .left
                .into_iter()
                .filter_map(|u| UserId::parse(u).ok())
                .collect(),
        })
    }
}

#[derive(uniffi::Record)]
pub struct KeysImportResult {
    /// The number of room keys that were imported.
    pub imported: i64,
    /// The total number of room keys that were found in the export.
    pub total: i64,
    /// The map of keys that were imported.
    ///
    /// It's a map from room id to a map of the sender key to a list of session
    /// ids.
    pub keys: HashMap<String, HashMap<String, Vec<String>>>,
}

pub(crate) enum OwnedResponse {
    KeysClaim(KeysClaimResponse),
    KeysUpload(KeysUploadResponse),
    KeysQuery(KeysQueryResponse),
    ToDevice(ToDeviceResponse),
    SignatureUpload(SignatureUploadResponse),
    KeysBackup(KeysBackupResponse),
    RoomMessage(RoomMessageResponse),
}

impl From<KeysClaimResponse> for OwnedResponse {
    fn from(response: KeysClaimResponse) -> Self {
        OwnedResponse::KeysClaim(response)
    }
}

impl From<KeysQueryResponse> for OwnedResponse {
    fn from(response: KeysQueryResponse) -> Self {
        OwnedResponse::KeysQuery(response)
    }
}

impl From<KeysUploadResponse> for OwnedResponse {
    fn from(response: KeysUploadResponse) -> Self {
        OwnedResponse::KeysUpload(response)
    }
}

impl From<ToDeviceResponse> for OwnedResponse {
    fn from(response: ToDeviceResponse) -> Self {
        OwnedResponse::ToDevice(response)
    }
}

impl From<SignatureUploadResponse> for OwnedResponse {
    fn from(response: SignatureUploadResponse) -> Self {
        Self::SignatureUpload(response)
    }
}

impl From<KeysBackupResponse> for OwnedResponse {
    fn from(r: KeysBackupResponse) -> Self {
        Self::KeysBackup(r)
    }
}

impl From<RoomMessageResponse> for OwnedResponse {
    fn from(response: RoomMessageResponse) -> Self {
        OwnedResponse::RoomMessage(response)
    }
}

impl<'a> From<&'a OwnedResponse> for AnyIncomingResponse<'a> {
    fn from(r: &'a OwnedResponse) -> Self {
        match r {
            OwnedResponse::KeysClaim(r) => AnyIncomingResponse::KeysClaim(r),
            OwnedResponse::KeysQuery(r) => AnyIncomingResponse::KeysQuery(r),
            OwnedResponse::KeysUpload(r) => AnyIncomingResponse::KeysUpload(r),
            OwnedResponse::ToDevice(r) => AnyIncomingResponse::ToDevice(r),
            OwnedResponse::SignatureUpload(r) => AnyIncomingResponse::SignatureUpload(r),
            OwnedResponse::KeysBackup(r) => AnyIncomingResponse::KeysBackup(r),
            OwnedResponse::RoomMessage(r) => AnyIncomingResponse::RoomMessage(r),
        }
    }
}
