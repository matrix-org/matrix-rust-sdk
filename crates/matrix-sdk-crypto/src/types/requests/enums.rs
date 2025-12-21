// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use std::sync::Arc;

use ruma::{
    TransactionId,
    api::client::{
        backup::add_backup_keys::v3::Response as KeysBackupResponse,
        keys::{
            claim_keys::v3::{Request as KeysClaimRequest, Response as KeysClaimResponse},
            get_keys::v3::Response as KeysQueryResponse,
            upload_keys::v3::{Request as KeysUploadRequest, Response as KeysUploadResponse},
            upload_signatures::v3::{
                Request as SignatureUploadRequest, Response as SignatureUploadResponse,
            },
            upload_signing_keys::v3::Response as SigningKeysUploadResponse,
        },
        message::send_message_event::v3::Response as RoomMessageResponse,
        to_device::send_event_to_device::v3::Response as ToDeviceResponse,
    },
};

use super::{
    KeysQueryRequest, OutgoingRequest, OutgoingVerificationRequest, RoomMessageRequest,
    ToDeviceRequest,
};

/// Enum over the different outgoing requests we can have.
#[derive(Debug)]
pub enum AnyOutgoingRequest {
    /// The `/keys/upload` request, uploading device and one-time keys.
    KeysUpload(KeysUploadRequest),
    /// The `/keys/query` request, fetching the device and cross signing keys of
    /// other users.
    KeysQuery(KeysQueryRequest),
    /// The request to claim one-time keys for a user/device pair from the
    /// server, after the response is received an 1-to-1 Olm session will be
    /// established with the user/device pair.
    KeysClaim(KeysClaimRequest),
    /// The to-device requests, this request is used for a couple of different
    /// things, the main use is key requests/forwards and interactive device
    /// verification.
    ToDeviceRequest(ToDeviceRequest),
    /// Signature upload request, this request is used after a successful device
    /// or user verification is done.
    SignatureUpload(SignatureUploadRequest),
    /// A room message request, usually for sending in-room interactive
    /// verification events.
    // `Box` the `RoomMessageRequest` to reduce the size of the enum.
    RoomMessage(Box<RoomMessageRequest>),
}

#[cfg(test)]
impl AnyOutgoingRequest {
    /// Test helper to destructure the [`OutgoingRequests`] as a
    /// [`ToDeviceRequest`].
    pub fn to_device(&self) -> Option<&ToDeviceRequest> {
        as_variant::as_variant!(self, Self::ToDeviceRequest)
    }
}

impl From<KeysQueryRequest> for AnyOutgoingRequest {
    fn from(request: KeysQueryRequest) -> Self {
        Self::KeysQuery(request)
    }
}

impl From<KeysClaimRequest> for AnyOutgoingRequest {
    fn from(r: KeysClaimRequest) -> Self {
        Self::KeysClaim(r)
    }
}

impl From<KeysUploadRequest> for AnyOutgoingRequest {
    fn from(request: KeysUploadRequest) -> Self {
        Self::KeysUpload(request)
    }
}

impl From<ToDeviceRequest> for AnyOutgoingRequest {
    fn from(request: ToDeviceRequest) -> Self {
        Self::ToDeviceRequest(request)
    }
}

impl From<RoomMessageRequest> for AnyOutgoingRequest {
    fn from(request: RoomMessageRequest) -> Self {
        Self::RoomMessage(Box::new(request))
    }
}

impl From<SignatureUploadRequest> for AnyOutgoingRequest {
    fn from(request: SignatureUploadRequest) -> Self {
        Self::SignatureUpload(request)
    }
}

impl From<OutgoingVerificationRequest> for OutgoingRequest {
    fn from(r: OutgoingVerificationRequest) -> Self {
        Self { request_id: r.request_id().to_owned(), request: Arc::new(r.into()) }
    }
}

impl From<SignatureUploadRequest> for OutgoingRequest {
    fn from(r: SignatureUploadRequest) -> Self {
        Self { request_id: TransactionId::new(), request: Arc::new(r.into()) }
    }
}

impl From<KeysUploadRequest> for OutgoingRequest {
    fn from(r: KeysUploadRequest) -> Self {
        Self { request_id: TransactionId::new(), request: Arc::new(r.into()) }
    }
}

impl From<OutgoingVerificationRequest> for AnyOutgoingRequest {
    fn from(request: OutgoingVerificationRequest) -> Self {
        match request {
            OutgoingVerificationRequest::ToDevice(r) => AnyOutgoingRequest::ToDeviceRequest(r),
            OutgoingVerificationRequest::InRoom(r) => AnyOutgoingRequest::RoomMessage(r),
        }
    }
}

/// Enum over all the incoming responses we need to receive.
#[derive(Debug)]
pub enum AnyIncomingResponse<'a> {
    /// The `/keys/upload` response, notifying us about the amount of uploaded
    /// one-time keys.
    KeysUpload(&'a KeysUploadResponse),
    /// The `/keys/query` response, giving us the device and cross signing keys
    /// of other users.
    KeysQuery(&'a KeysQueryResponse),
    /// The to-device response, an empty response.
    ToDevice(&'a ToDeviceResponse),
    /// The key claiming requests, giving us new one-time keys of other users so
    /// new Olm sessions can be created.
    KeysClaim(&'a KeysClaimResponse),
    /// The cross signing `/keys/upload` response, marking our private cross
    /// signing identity as shared.
    SigningKeysUpload(&'a SigningKeysUploadResponse),
    /// The cross signing signature upload response.
    SignatureUpload(&'a SignatureUploadResponse),
    /// A room message response, usually for interactive verifications.
    RoomMessage(&'a RoomMessageResponse),
    /// Response for the server-side room key backup request.
    KeysBackup(&'a KeysBackupResponse),
}

impl<'a> From<&'a KeysUploadResponse> for AnyIncomingResponse<'a> {
    fn from(response: &'a KeysUploadResponse) -> Self {
        AnyIncomingResponse::KeysUpload(response)
    }
}

impl<'a> From<&'a KeysBackupResponse> for AnyIncomingResponse<'a> {
    fn from(response: &'a KeysBackupResponse) -> Self {
        AnyIncomingResponse::KeysBackup(response)
    }
}

impl<'a> From<&'a KeysQueryResponse> for AnyIncomingResponse<'a> {
    fn from(response: &'a KeysQueryResponse) -> Self {
        AnyIncomingResponse::KeysQuery(response)
    }
}

impl<'a> From<&'a ToDeviceResponse> for AnyIncomingResponse<'a> {
    fn from(response: &'a ToDeviceResponse) -> Self {
        AnyIncomingResponse::ToDevice(response)
    }
}

impl<'a> From<&'a RoomMessageResponse> for AnyIncomingResponse<'a> {
    fn from(response: &'a RoomMessageResponse) -> Self {
        AnyIncomingResponse::RoomMessage(response)
    }
}

impl<'a> From<&'a KeysClaimResponse> for AnyIncomingResponse<'a> {
    fn from(response: &'a KeysClaimResponse) -> Self {
        AnyIncomingResponse::KeysClaim(response)
    }
}

impl<'a> From<&'a SignatureUploadResponse> for AnyIncomingResponse<'a> {
    fn from(response: &'a SignatureUploadResponse) -> Self {
        AnyIncomingResponse::SignatureUpload(response)
    }
}
