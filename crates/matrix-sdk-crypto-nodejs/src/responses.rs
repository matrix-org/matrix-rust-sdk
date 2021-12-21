use http::Response;
use ruma::{
    api::client::r0::{
        backup::add_backup_keys::Response as KeysBackupResponse,
        keys::{
            claim_keys::{Request as KeysClaimRequest, Response as KeysClaimResponse},
            get_keys::Response as KeysQueryResponse,
            upload_keys::Response as KeysUploadResponse,
            upload_signatures::{
                Request as RustSignatureUploadRequest, Response as SignatureUploadResponse,
            },
        },
        sync::sync_events::DeviceLists as RumaDeviceLists,
        to_device::send_event_to_device::Response as ToDeviceResponse,
        message::send_message_event::Response as RoomMessageResponse,
    },
    assign,
    events::EventContent,
    identifiers::UserId,
};
use matrix_sdk_crypto::{
    IncomingResponse, OutgoingRequest, OutgoingVerificationRequest as SdkVerificationRequest,
    RoomMessageRequest, ToDeviceRequest, UploadSigningKeysRequest as RustUploadSigningKeysRequest,
};

pub(crate) fn response_from_string(body: &str) -> Response<Vec<u8>> {
    Response::builder()
        .status(200)
        .body(body.as_bytes().to_vec())
        .expect("Can't create HTTP response")
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
    fn from(r: RoomMessageResponse) -> Self {
        Self::RoomMessage(r)
    }
}

impl<'a> From<&'a OwnedResponse> for IncomingResponse<'a> {
    fn from(r: &'a OwnedResponse) -> Self {
        match r {
            OwnedResponse::KeysClaim(r) => IncomingResponse::KeysClaim(r),
            OwnedResponse::KeysQuery(r) => IncomingResponse::KeysQuery(r),
            OwnedResponse::KeysUpload(r) => IncomingResponse::KeysUpload(r),
            OwnedResponse::ToDevice(r) => IncomingResponse::ToDevice(r),
            OwnedResponse::SignatureUpload(r) => IncomingResponse::SignatureUpload(r),
            OwnedResponse::KeysBackup(r) => IncomingResponse::KeysBackup(r),
            OwnedResponse::RoomMessage(r) => IncomingResponse::RoomMessage(r),
        }
    }
}