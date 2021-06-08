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

#![allow(missing_docs)]

mod cache;
mod event_enums;
mod machine;
mod requests;
mod sas;

use std::sync::Arc;

use event_enums::OutgoingContent;
pub use machine::VerificationMachine;
pub use requests::VerificationRequest;
use ruma::{
    api::client::r0::keys::upload_signatures::Request as SignatureUploadRequest,
    events::{
        key::verification::{
            cancel::{CancelCode, CancelEventContent, CancelToDeviceEventContent},
            done::{DoneEventContent, DoneToDeviceEventContent},
            Relation,
        },
        AnyMessageEventContent, AnyToDeviceEventContent,
    },
    DeviceId, EventId, RoomId, UserId,
};
pub use sas::{AcceptSettings, Sas};
use tracing::{error, info, trace, warn};

use crate::{
    error::SignatureError,
    olm::PrivateCrossSigningIdentity,
    store::{Changes, CryptoStore},
    CryptoStoreError, LocalTrust, ReadOnlyDevice, UserIdentities,
};

#[derive(Clone, Debug)]
pub enum Verification {
    SasV1(Sas),
}

impl Verification {
    pub fn is_done(&self) -> bool {
        match self {
            Verification::SasV1(s) => s.is_done(),
        }
    }

    pub fn sas_v1(self) -> Option<Sas> {
        if let Verification::SasV1(sas) = self {
            Some(sas)
        } else {
            None
        }
    }

    pub fn flow_id(&self) -> &str {
        match self {
            Verification::SasV1(s) => s.flow_id().as_str(),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        match self {
            Verification::SasV1(s) => s.is_cancelled(),
        }
    }

    pub fn other_user(&self) -> &UserId {
        match self {
            Verification::SasV1(s) => s.other_user_id(),
        }
    }
}

impl From<Sas> for Verification {
    fn from(sas: Sas) -> Self {
        Self::SasV1(sas)
    }
}

/// The verification state indicating that the verification finished
/// successfully.
///
/// We can now mark the device in our verified devices list as verified and sign
/// the master keys in the verified devices list.
#[derive(Clone, Debug)]
pub struct Done {
    verified_devices: Arc<[ReadOnlyDevice]>,
    verified_master_keys: Arc<[UserIdentities]>,
}

impl Done {
    pub fn as_content(&self, flow_id: &FlowId) -> OutgoingContent {
        match flow_id {
            FlowId::ToDevice(t) => AnyToDeviceEventContent::KeyVerificationDone(
                DoneToDeviceEventContent::new(t.to_owned()),
            )
            .into(),
            FlowId::InRoom(r, e) => (
                r.to_owned(),
                AnyMessageEventContent::KeyVerificationDone(DoneEventContent::new(Relation::new(
                    e.to_owned(),
                ))),
            )
                .into(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Cancelled {
    cancel_code: CancelCode,
    reason: &'static str,
}

impl Cancelled {
    fn new(code: CancelCode) -> Self {
        let reason = match code {
            CancelCode::Accepted => {
                "A m.key.verification.request was accepted by a different device."
            }
            CancelCode::InvalidMessage => "The received message was invalid.",
            CancelCode::KeyMismatch => "The expected key did not match the verified one",
            CancelCode::Timeout => "The verification process timed out.",
            CancelCode::UnexpectedMessage => "The device received an unexpected message.",
            CancelCode::UnknownMethod => {
                "The device does not know how to handle the requested method."
            }
            CancelCode::UnknownTransaction => {
                "The device does not know about the given transaction ID."
            }
            CancelCode::User => "The user cancelled the verification.",
            CancelCode::UserMismatch => "The expected user did not match the verified user",
            _ => "Unknown cancel reason",
        };

        Self { cancel_code: code, reason }
    }

    pub fn as_content(&self, flow_id: &FlowId) -> OutgoingContent {
        match flow_id {
            FlowId::ToDevice(s) => {
                AnyToDeviceEventContent::KeyVerificationCancel(CancelToDeviceEventContent::new(
                    s.clone(),
                    self.reason.to_string(),
                    self.cancel_code.clone(),
                ))
                .into()
            }

            FlowId::InRoom(r, e) => (
                r.clone(),
                AnyMessageEventContent::KeyVerificationCancel(CancelEventContent::new(
                    self.reason.to_string(),
                    self.cancel_code.clone(),
                    Relation::new(e.clone()),
                )),
            )
                .into(),
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, PartialOrd)]
pub enum FlowId {
    ToDevice(String),
    InRoom(RoomId, EventId),
}

impl FlowId {
    pub fn room_id(&self) -> Option<&RoomId> {
        if let FlowId::InRoom(r, _) = &self {
            Some(r)
        } else {
            None
        }
    }

    pub fn as_str(&self) -> &str {
        match self {
            FlowId::InRoom(_, r) => r.as_str(),
            FlowId::ToDevice(t) => t.as_str(),
        }
    }
}

impl From<String> for FlowId {
    fn from(transaction_id: String) -> Self {
        FlowId::ToDevice(transaction_id)
    }
}

impl From<(RoomId, EventId)> for FlowId {
    fn from(ids: (RoomId, EventId)) -> Self {
        FlowId::InRoom(ids.0, ids.1)
    }
}

impl From<(&RoomId, &EventId)> for FlowId {
    fn from(ids: (&RoomId, &EventId)) -> Self {
        FlowId::InRoom(ids.0.to_owned(), ids.1.to_owned())
    }
}

/// A result of a verification flow.
#[derive(Clone, Debug)]
pub enum VerificationResult {
    /// The verification succeeded, nothing needs to be done.
    Ok,
    /// The verification was canceled.
    Cancel(CancelCode),
    /// The verification is done and has signatures that need to be uploaded.
    SignatureUpload(SignatureUploadRequest),
}

#[derive(Clone, Debug)]
pub struct IdentitiesBeingVerified {
    private_identity: PrivateCrossSigningIdentity,
    store: Arc<dyn CryptoStore>,
    device_being_verified: ReadOnlyDevice,
    identity_being_verified: Option<UserIdentities>,
}

impl IdentitiesBeingVerified {
    fn user_id(&self) -> &UserId {
        self.private_identity.user_id()
    }

    fn other_user_id(&self) -> &UserId {
        self.device_being_verified.user_id()
    }

    fn other_device_id(&self) -> &DeviceId {
        self.device_being_verified.device_id()
    }

    fn other_device(&self) -> &ReadOnlyDevice {
        &self.device_being_verified
    }

    pub async fn mark_as_done(
        &self,
        verified_devices: Option<&[ReadOnlyDevice]>,
        verified_identities: Option<&[UserIdentities]>,
    ) -> Result<VerificationResult, CryptoStoreError> {
        let device = self.mark_device_as_verified(verified_devices).await?;
        let identity = self.mark_identity_as_verified(verified_identities).await?;

        if device.is_none() && identity.is_none() {
            // Something wen't wrong if nothing was verified, we use key
            // mismatch here, since it's the closest to nothing was verified
            return Ok(VerificationResult::Cancel(CancelCode::KeyMismatch));
        }

        let mut changes = Changes::default();

        let signature_request = if let Some(device) = device {
            // We only sign devices of our own user here.
            let signature_request = if device.user_id() == self.user_id() {
                match self.private_identity.sign_device(&device).await {
                    Ok(r) => Some(r),
                    Err(SignatureError::MissingSigningKey) => {
                        warn!(
                            "Can't sign the device keys for {} {}, \
                                  no private user signing key found",
                            device.user_id(),
                            device.device_id(),
                        );

                        None
                    }
                    Err(e) => {
                        error!(
                            "Error signing device keys for {} {} {:?}",
                            device.user_id(),
                            device.device_id(),
                            e
                        );
                        None
                    }
                }
            } else {
                None
            };

            changes.devices.changed.push(device);
            signature_request
        } else {
            None
        };

        let identity_signature_request = if let Some(i) = identity {
            // We only sign other users here.
            let request = if let Some(i) = i.other() {
                // Signing can fail if the user signing key is missing.
                match self.private_identity.sign_user(i).await {
                    Ok(r) => Some(r),
                    Err(SignatureError::MissingSigningKey) => {
                        warn!(
                            "Can't sign the public cross signing keys for {}, \
                              no private user signing key found",
                            i.user_id()
                        );
                        None
                    }
                    Err(e) => {
                        error!(
                            "Error signing the public cross signing keys for {} {:?}",
                            i.user_id(),
                            e
                        );
                        None
                    }
                }
            } else {
                None
            };

            changes.identities.changed.push(i);
            request
        } else {
            None
        };

        // If there are two signature upload requests, merge them. Otherwise
        // use the one we have or None.
        //
        // Realistically at most one request will be used but let's make
        // this future proof.
        let merged_request = if let Some(mut r) = signature_request {
            if let Some(user_request) = identity_signature_request {
                r.signed_keys.extend(user_request.signed_keys);
                Some(r)
            } else {
                Some(r)
            }
        } else {
            identity_signature_request
        };

        // TODO store the signature upload request as well.
        self.store.save_changes(changes).await?;

        Ok(merged_request
            .map(VerificationResult::SignatureUpload)
            .unwrap_or(VerificationResult::Ok))
    }

    async fn mark_identity_as_verified(
        &self,
        verified_identities: Option<&[UserIdentities]>,
    ) -> Result<Option<UserIdentities>, CryptoStoreError> {
        // If there wasn't an identity available during the verification flow
        // return early as there's nothing to do.
        if self.identity_being_verified.is_none() {
            return Ok(None);
        }

        // TODO signal an error, e.g. when the identity got deleted so we don't
        // verify/save the device either.
        let identity = self.store.get_user_identity(self.other_user_id()).await?;

        if let Some(identity) = identity {
            if self
                .identity_being_verified
                .as_ref()
                .map_or(false, |i| i.master_key() == identity.master_key())
            {
                if verified_identities.map_or(false, |i| i.contains(&identity)) {
                    trace!(
                        user_id = self.other_user_id().as_str(),
                        "Marking the user identity of as verified."
                    );

                    if let UserIdentities::Own(i) = &identity {
                        i.mark_as_verified();
                    }

                    Ok(Some(identity))
                } else {
                    info!(
                        user_id = self.other_user_id().as_str(),
                        "The interactive verification process didn't verify \
                         the user identity of the user that participated in \
                         the interactive verification",
                    );

                    Ok(None)
                }
            } else {
                warn!(
                    user_id = self.other_user_id().as_str(),
                    "The master keys of the user have changed while an interactive \
                      verification was going on, not marking the identity as verified.",
                );

                Ok(None)
            }
        } else {
            info!(
                user_id = self.other_user_id().as_str(),
                "The identity of the user was deleted while an interactive \
                 verification was going on.",
            );
            Ok(None)
        }
    }

    async fn mark_device_as_verified(
        &self,
        verified_devices: Option<&[ReadOnlyDevice]>,
    ) -> Result<Option<ReadOnlyDevice>, CryptoStoreError> {
        let device = self.store.get_device(self.other_user_id(), self.other_device_id()).await?;

        if let Some(device) = device {
            if device.keys() == self.device_being_verified.keys() {
                if verified_devices.map_or(false, |v| v.contains(&device)) {
                    trace!(
                        user_id = device.user_id().as_str(),
                        device_id = device.device_id().as_str(),
                        "Marking device as verified.",
                    );

                    device.set_trust_state(LocalTrust::Verified);

                    Ok(Some(device))
                } else {
                    info!(
                        user_id = device.user_id().as_str(),
                        device_id = device.device_id().as_str(),
                        "The interactive verification process didn't verify \
                        the device",
                    );

                    Ok(None)
                }
            } else {
                warn!(
                    user_id = device.user_id().as_str(),
                    device_id = device.device_id().as_str(),
                    "The device keys have changed while an interactive \
                     verification was going on, not marking the device as verified.",
                );
                Ok(None)
            }
        } else {
            let device = &self.device_being_verified;

            info!(
                user_id = device.user_id().as_str(),
                device_id = device.device_id().as_str(),
                "The device was deleted while an interactive verification was \
                 going on.",
            );

            Ok(None)
        }
    }
}

#[cfg(test)]
pub(crate) mod test {
    use ruma::{
        events::{AnyToDeviceEvent, AnyToDeviceEventContent, EventType, ToDeviceEvent},
        UserId,
    };
    use serde_json::Value;

    use super::event_enums::OutgoingContent;
    use crate::{
        requests::{OutgoingRequest, OutgoingRequests},
        OutgoingVerificationRequest,
    };

    pub(crate) fn request_to_event(
        sender: &UserId,
        request: &OutgoingVerificationRequest,
    ) -> AnyToDeviceEvent {
        let content = get_content_from_request(request);
        wrap_any_to_device_content(sender, content)
    }

    pub(crate) fn outgoing_request_to_event(
        sender: &UserId,
        request: &OutgoingRequest,
    ) -> AnyToDeviceEvent {
        match request.request() {
            OutgoingRequests::ToDeviceRequest(r) => request_to_event(sender, &r.clone().into()),
            _ => panic!("Unsupported outgoing request"),
        }
    }

    pub(crate) fn wrap_any_to_device_content(
        sender: &UserId,
        content: OutgoingContent,
    ) -> AnyToDeviceEvent {
        let content = if let OutgoingContent::ToDevice(c) = content { c } else { unreachable!() };

        match content {
            AnyToDeviceEventContent::KeyVerificationKey(c) => {
                AnyToDeviceEvent::KeyVerificationKey(ToDeviceEvent {
                    sender: sender.clone(),
                    content: c,
                })
            }
            AnyToDeviceEventContent::KeyVerificationStart(c) => {
                AnyToDeviceEvent::KeyVerificationStart(ToDeviceEvent {
                    sender: sender.clone(),
                    content: c,
                })
            }
            AnyToDeviceEventContent::KeyVerificationAccept(c) => {
                AnyToDeviceEvent::KeyVerificationAccept(ToDeviceEvent {
                    sender: sender.clone(),
                    content: c,
                })
            }
            AnyToDeviceEventContent::KeyVerificationMac(c) => {
                AnyToDeviceEvent::KeyVerificationMac(ToDeviceEvent {
                    sender: sender.clone(),
                    content: c,
                })
            }

            _ => unreachable!(),
        }
    }

    pub(crate) fn get_content_from_request(
        request: &OutgoingVerificationRequest,
    ) -> OutgoingContent {
        let request =
            if let OutgoingVerificationRequest::ToDevice(r) = request { r } else { unreachable!() };

        let json: Value = serde_json::from_str(
            request.messages.values().next().unwrap().values().next().unwrap().get(),
        )
        .unwrap();

        match request.event_type {
            EventType::KeyVerificationStart => {
                AnyToDeviceEventContent::KeyVerificationStart(serde_json::from_value(json).unwrap())
            }
            EventType::KeyVerificationKey => {
                AnyToDeviceEventContent::KeyVerificationKey(serde_json::from_value(json).unwrap())
            }
            EventType::KeyVerificationAccept => AnyToDeviceEventContent::KeyVerificationAccept(
                serde_json::from_value(json).unwrap(),
            ),
            EventType::KeyVerificationMac => {
                AnyToDeviceEventContent::KeyVerificationMac(serde_json::from_value(json).unwrap())
            }
            EventType::KeyVerificationCancel => AnyToDeviceEventContent::KeyVerificationCancel(
                serde_json::from_value(json).unwrap(),
            ),
            _ => unreachable!(),
        }
        .into()
    }
}
