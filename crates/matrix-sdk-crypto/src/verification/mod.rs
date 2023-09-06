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

mod cache;
mod event_enums;
mod machine;
#[cfg(feature = "qrcode")]
mod qrcode;
mod requests;
mod sas;

use std::{collections::HashMap, ops::Deref, sync::Arc};

use event_enums::OutgoingContent;
pub use machine::VerificationMachine;
#[cfg(feature = "qrcode")]
pub use qrcode::{QrVerification, QrVerificationState, ScanError};
pub use requests::{VerificationRequest, VerificationRequestState};
#[cfg(feature = "qrcode")]
use ruma::events::key::verification::done::{
    KeyVerificationDoneEventContent, ToDeviceKeyVerificationDoneEventContent,
};
use ruma::{
    api::client::keys::upload_signatures::v3::Request as SignatureUploadRequest,
    events::{
        key::verification::cancel::{
            CancelCode, KeyVerificationCancelEventContent,
            ToDeviceKeyVerificationCancelEventContent,
        },
        relation::Reference,
        AnyMessageLikeEventContent, AnyToDeviceEventContent,
    },
    DeviceId, EventId, OwnedDeviceId, OwnedEventId, OwnedRoomId, OwnedTransactionId, RoomId,
    UserId,
};
pub use sas::{AcceptSettings, AcceptedProtocols, EmojiShortAuthString, Sas, SasState};
use tokio::sync::Mutex;
use tracing::{error, info, trace, warn};

use crate::{
    error::SignatureError,
    gossiping::{GossipMachine, GossipRequest},
    olm::{PrivateCrossSigningIdentity, ReadOnlyAccount, Session},
    store::{Changes, CryptoStoreWrapper},
    types::Signatures,
    CryptoStoreError, LocalTrust, OutgoingVerificationRequest, ReadOnlyDevice,
    ReadOnlyOwnUserIdentity, ReadOnlyUserIdentities,
};

#[derive(Clone, Debug)]
pub(crate) struct VerificationStore {
    pub account: ReadOnlyAccount,
    pub private_identity: Arc<Mutex<PrivateCrossSigningIdentity>>,
    inner: Arc<CryptoStoreWrapper>,
}

/// An emoji that is used for interactive verification using a short auth
/// string.
///
/// This will contain a single emoji and description from the list of emojis
/// from the [spec].
///
/// [spec]: https://spec.matrix.org/unstable/client-server-api/#sas-method-emoji
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd)]
pub struct Emoji {
    /// The emoji symbol that represents a part of the short auth string, for
    /// example: 🐶
    pub symbol: &'static str,
    /// The description of the emoji, for example 'Dog'.
    pub description: &'static str,
}

/// Format the the list of emojis as a two line string.
///
/// The first line will contain the emojis spread out so the second line can
/// contain the descriptions centered bellow the emoji.
pub fn format_emojis(emojis: [Emoji; 7]) -> String {
    let (emojis, descriptions): (Vec<_>, Vec<_>) =
        emojis.iter().map(|e| (e.symbol, e.description)).unzip();

    let center_emoji = |emoji: &str| -> String {
        const EMOJI_WIDTH: usize = 2;
        // These are emojis that need VARIATION-SELECTOR-16 (U+FE0F) so that they are
        // rendered with coloured glyphs. For these, we need to add an extra
        // space after them so that they are rendered properly in terminals.
        const VARIATION_SELECTOR_EMOJIS: [&str; 7] = ["☁️", "❤️", "☂️", "✏️", "✂️", "☎️", "✈️"];

        // Hack to make terminals behave properly when one of the above is printed.
        let emoji = if VARIATION_SELECTOR_EMOJIS.contains(&emoji) {
            format!("{emoji} ")
        } else {
            emoji.to_owned()
        };

        // This is a trick to account for the fact that emojis are wider than other
        // monospace characters.
        let placeholder = ".".repeat(EMOJI_WIDTH);

        format!("{placeholder:^12}").replace(&placeholder, &emoji)
    };

    let emoji_string = emojis.iter().map(|e| center_emoji(e)).collect::<Vec<_>>().join("");

    let description = descriptions.iter().map(|d| format!("{d:^12}")).collect::<Vec<_>>().join("");

    format!("{emoji_string}\n{description}")
}

impl VerificationStore {
    pub async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<ReadOnlyDevice>, CryptoStoreError> {
        Ok(self.inner.get_device(user_id, device_id).await?.filter(|d| {
            !(d.user_id() == self.account.user_id() && d.device_id() == self.account.device_id())
        }))
    }

    pub async fn get_user_identity(
        &self,
        user_id: &UserId,
    ) -> Result<Option<ReadOnlyUserIdentities>, CryptoStoreError> {
        self.inner.get_user_identity(user_id).await
    }

    pub async fn get_identities(
        &self,
        device_being_verified: ReadOnlyDevice,
    ) -> Result<IdentitiesBeingVerified, CryptoStoreError> {
        let identity_being_verified =
            self.get_user_identity(device_being_verified.user_id()).await?;

        Ok(IdentitiesBeingVerified {
            private_identity: self.private_identity.lock().await.clone(),
            store: self.clone(),
            device_being_verified,
            own_identity: self
                .get_user_identity(self.account.user_id())
                .await?
                .and_then(|i| i.into_own()),
            identity_being_verified,
        })
    }

    pub async fn save_changes(&self, changes: Changes) -> Result<(), CryptoStoreError> {
        self.inner.save_changes(changes).await
    }

    pub async fn get_user_devices(
        &self,
        user_id: &UserId,
    ) -> Result<HashMap<OwnedDeviceId, ReadOnlyDevice>, CryptoStoreError> {
        self.inner.get_user_devices(user_id).await
    }

    pub async fn get_sessions(
        &self,
        sender_key: &str,
    ) -> Result<Option<Arc<Mutex<Vec<Session>>>>, CryptoStoreError> {
        self.inner.get_sessions(sender_key).await
    }

    /// Get the signatures that have signed our own device.
    pub async fn device_signatures(&self) -> Result<Option<Signatures>, CryptoStoreError> {
        Ok(self
            .inner
            .get_device(self.account.user_id(), self.account.device_id())
            .await?
            .map(|d| d.signatures().to_owned()))
    }

    pub fn inner(&self) -> &CryptoStoreWrapper {
        self.inner.deref()
    }
}

/// An enum over the different verification types the SDK supports.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Verification {
    /// The `m.sas.v1` verification variant.
    SasV1(Sas),
    /// The `m.qr_code.*.v1` verification variant.
    #[cfg(feature = "qrcode")]
    QrV1(QrVerification),
}

impl Verification {
    /// Try to deconstruct this verification enum into a SAS verification.
    pub fn sas_v1(self) -> Option<Sas> {
        #[allow(irrefutable_let_patterns)]
        if let Verification::SasV1(sas) = self {
            Some(sas)
        } else {
            None
        }
    }

    /// Try to deconstruct this verification enum into a QR code verification.
    #[cfg(feature = "qrcode")]
    pub fn qr_v1(self) -> Option<QrVerification> {
        if let Verification::QrV1(qr) = self {
            Some(qr)
        } else {
            None
        }
    }

    /// Has this verification finished.
    pub fn is_done(&self) -> bool {
        match self {
            Verification::SasV1(s) => s.is_done(),
            #[cfg(feature = "qrcode")]
            Verification::QrV1(qr) => qr.is_done(),
        }
    }

    /// Get the ID that uniquely identifies this verification flow.
    pub fn flow_id(&self) -> &str {
        match self {
            Verification::SasV1(s) => s.flow_id().as_str(),
            #[cfg(feature = "qrcode")]
            Verification::QrV1(qr) => qr.flow_id().as_str(),
        }
    }

    /// Has the verification been cancelled.
    pub fn is_cancelled(&self) -> bool {
        match self {
            Verification::SasV1(s) => s.is_cancelled(),
            #[cfg(feature = "qrcode")]
            Verification::QrV1(qr) => qr.is_cancelled(),
        }
    }

    /// Get our own user id that is participating in this verification.
    pub fn user_id(&self) -> &UserId {
        match self {
            Verification::SasV1(v) => v.user_id(),
            #[cfg(feature = "qrcode")]
            Verification::QrV1(v) => v.user_id(),
        }
    }

    /// Get the other user id that is participating in this verification.
    pub fn other_user(&self) -> &UserId {
        match self {
            Verification::SasV1(s) => s.other_user_id(),
            #[cfg(feature = "qrcode")]
            Verification::QrV1(qr) => qr.other_user_id(),
        }
    }

    /// Is this a verification verifying a device that belongs to us.
    pub fn is_self_verification(&self) -> bool {
        match self {
            Verification::SasV1(v) => v.is_self_verification(),
            #[cfg(feature = "qrcode")]
            Verification::QrV1(v) => v.is_self_verification(),
        }
    }

    fn cancel(&self) -> Option<OutgoingVerificationRequest> {
        match self {
            Verification::SasV1(v) => v.cancel(),
            #[cfg(feature = "qrcode")]
            Verification::QrV1(v) => v.cancel(),
        }
    }
}

impl From<Sas> for Verification {
    fn from(sas: Sas) -> Self {
        Self::SasV1(sas)
    }
}

#[cfg(feature = "qrcode")]
impl From<QrVerification> for Verification {
    fn from(qr: QrVerification) -> Self {
        Self::QrV1(qr)
    }
}

/// The verification state indicating that the verification finished
/// successfully.
///
/// We can now mark the device in our verified devices list as verified and sign
/// the master keys in the verified devices list.
#[cfg(feature = "qrcode")]
#[derive(Clone, Debug)]
pub struct Done {
    verified_devices: Arc<[ReadOnlyDevice]>,
    verified_master_keys: Arc<[ReadOnlyUserIdentities]>,
}

#[cfg(feature = "qrcode")]
impl Done {
    pub fn as_content(&self, flow_id: &FlowId) -> OutgoingContent {
        match flow_id {
            FlowId::ToDevice(t) => AnyToDeviceEventContent::KeyVerificationDone(
                ToDeviceKeyVerificationDoneEventContent::new(t.to_owned()),
            )
            .into(),
            FlowId::InRoom(r, e) => (
                r.to_owned(),
                AnyMessageLikeEventContent::KeyVerificationDone(
                    KeyVerificationDoneEventContent::new(Reference::new(e.to_owned())),
                ),
            )
                .into(),
        }
    }
}

/// Information about the cancellation of a verification request or verification
/// flow.
#[derive(Clone, Debug)]
pub struct CancelInfo {
    cancelled_by_us: bool,
    cancel_code: CancelCode,
    reason: &'static str,
}

impl CancelInfo {
    /// Get the human readable reason of the cancellation.
    pub fn reason(&self) -> &'static str {
        self.reason
    }

    /// Get the `CancelCode` that cancelled this verification.
    pub fn cancel_code(&self) -> &CancelCode {
        &self.cancel_code
    }

    /// Was the verification cancelled by us?
    pub fn cancelled_by_us(&self) -> bool {
        self.cancelled_by_us
    }
}

impl From<Cancelled> for CancelInfo {
    fn from(c: Cancelled) -> Self {
        Self { cancelled_by_us: c.cancelled_by_us, cancel_code: c.cancel_code, reason: c.reason }
    }
}

#[derive(Clone, Debug)]
pub struct Cancelled {
    cancelled_by_us: bool,
    cancel_code: CancelCode,
    reason: &'static str,
}

impl Cancelled {
    fn new(cancelled_by_us: bool, code: CancelCode) -> Self {
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

        Self { cancelled_by_us, cancel_code: code, reason }
    }

    pub fn as_content(&self, flow_id: &FlowId) -> OutgoingContent {
        match flow_id {
            FlowId::ToDevice(s) => AnyToDeviceEventContent::KeyVerificationCancel(
                ToDeviceKeyVerificationCancelEventContent::new(
                    s.clone(),
                    self.reason.to_owned(),
                    self.cancel_code.clone(),
                ),
            )
            .into(),

            FlowId::InRoom(r, e) => (
                r.clone(),
                AnyMessageLikeEventContent::KeyVerificationCancel(
                    KeyVerificationCancelEventContent::new(
                        self.reason.to_owned(),
                        self.cancel_code.clone(),
                        Reference::new(e.clone()),
                    ),
                ),
            )
                .into(),
        }
    }
}

/// A key verification can be requested and started by a to-device
/// request or a room event. `FlowId` helps to represent both
/// usecases.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd)]
pub enum FlowId {
    /// The flow ID comes from a to-device request.
    ToDevice(OwnedTransactionId),

    /// The flow ID comes from a room event.
    InRoom(OwnedRoomId, OwnedEventId),
}

impl FlowId {
    /// Get the room ID if the flow ID comes from a room event.
    pub fn room_id(&self) -> Option<&RoomId> {
        if let Self::InRoom(room_id, _) = &self {
            Some(room_id)
        } else {
            None
        }
    }

    /// Get the ID a string.
    pub fn as_str(&self) -> &str {
        match self {
            Self::InRoom(_, event_id) => event_id.as_str(),
            Self::ToDevice(transaction_id) => transaction_id.as_str(),
        }
    }
}

impl From<OwnedTransactionId> for FlowId {
    fn from(transaction_id: OwnedTransactionId) -> Self {
        Self::ToDevice(transaction_id)
    }
}

impl From<(OwnedRoomId, OwnedEventId)> for FlowId {
    fn from(ids: (OwnedRoomId, OwnedEventId)) -> Self {
        Self::InRoom(ids.0, ids.1)
    }
}

impl From<(&RoomId, &EventId)> for FlowId {
    fn from(ids: (&RoomId, &EventId)) -> Self {
        Self::InRoom(ids.0.to_owned(), ids.1.to_owned())
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
    store: VerificationStore,
    device_being_verified: ReadOnlyDevice,
    own_identity: Option<ReadOnlyOwnUserIdentity>,
    identity_being_verified: Option<ReadOnlyUserIdentities>,
}

impl IdentitiesBeingVerified {
    #[cfg(feature = "qrcode")]
    async fn can_sign_devices(&self) -> bool {
        self.private_identity.can_sign_devices().await
    }

    fn user_id(&self) -> &UserId {
        self.private_identity.user_id()
    }

    fn is_self_verification(&self) -> bool {
        self.user_id() == self.other_user_id()
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
        verified_identities: Option<&[ReadOnlyUserIdentities]>,
    ) -> Result<VerificationResult, CryptoStoreError> {
        let device = self.mark_device_as_verified(verified_devices).await?;
        let (identity, should_request_secrets) =
            self.mark_identity_as_verified(verified_identities).await?;

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
                                  no private device signing key found",
                            device.user_id(),
                            device.device_id(),
                        );

                        None
                    }
                    Err(e) => {
                        error!(
                            user_id = ?device.user_id(),
                            device_id = ?device.device_id(),
                            "Error signing device keys: {e:?}",
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
                            user_id = ?i.user_id(),
                            "Can't sign the public cross signing keys, \
                             no private user signing key found",
                        );
                        None
                    }
                    Err(e) => {
                        error!(
                            user_id = ?i.user_id(),
                            "Error signing the public cross signing keys: {e:?}",
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
            }

            Some(r)
        } else {
            identity_signature_request
        };

        if should_request_secrets {
            let secret_requests = self.request_missing_secrets().await?;
            changes.key_requests = secret_requests;
        }

        // TODO store the signature upload request as well.
        self.store.save_changes(changes).await?;

        Ok(merged_request
            .map(VerificationResult::SignatureUpload)
            .unwrap_or(VerificationResult::Ok))
    }

    async fn request_missing_secrets(&self) -> Result<Vec<GossipRequest>, CryptoStoreError> {
        #[allow(unused_mut)]
        let mut secrets = self.private_identity.get_missing_secrets().await;

        #[cfg(feature = "backups_v1")]
        if self.store.inner.load_backup_keys().await?.decryption_key.is_none() {
            secrets.push(ruma::events::secret::request::SecretName::RecoveryKey);
        }

        Ok(GossipMachine::request_missing_secrets(self.user_id(), secrets))
    }

    async fn mark_identity_as_verified(
        &self,
        verified_identities: Option<&[ReadOnlyUserIdentities]>,
    ) -> Result<(Option<ReadOnlyUserIdentities>, bool), CryptoStoreError> {
        // If there wasn't an identity available during the verification flow
        // return early as there's nothing to do.
        if self.identity_being_verified.is_none() {
            return Ok((None, false));
        }

        let identity = self.store.get_user_identity(self.other_user_id()).await?;

        Ok(if let Some(identity) = identity {
            if self
                .identity_being_verified
                .as_ref()
                .is_some_and(|i| i.master_key() == identity.master_key())
            {
                if verified_identities.is_some_and(|i| i.contains(&identity)) {
                    trace!(
                        user_id = self.other_user_id().as_str(),
                        "Marking the user identity of as verified."
                    );

                    let should_request_secrets = if let ReadOnlyUserIdentities::Own(i) = &identity {
                        i.mark_as_verified();
                        true
                    } else {
                        false
                    };

                    (Some(identity), should_request_secrets)
                } else {
                    info!(
                        user_id = self.other_user_id().as_str(),
                        "The interactive verification process didn't verify \
                         the user identity of the user that participated in \
                         the interactive verification",
                    );

                    (None, false)
                }
            } else {
                warn!(
                    user_id = self.other_user_id().as_str(),
                    "The master keys of the user have changed while an interactive \
                      verification was going on, not marking the identity as verified.",
                );

                (None, false)
            }
        } else {
            info!(
                user_id = self.other_user_id().as_str(),
                "The identity of the user was deleted while an interactive \
                 verification was going on.",
            );
            (None, false)
        })
    }

    async fn mark_device_as_verified(
        &self,
        verified_devices: Option<&[ReadOnlyDevice]>,
    ) -> Result<Option<ReadOnlyDevice>, CryptoStoreError> {
        let device = self.store.get_device(self.other_user_id(), self.other_device_id()).await?;

        let Some(device) = device else {
            let device = &self.device_being_verified;
            info!(
                user_id = device.user_id().as_str(),
                device_id = device.device_id().as_str(),
                "The device was deleted while an interactive verification was going on.",
            );
            return Ok(None);
        };

        if device.keys() != self.device_being_verified.keys() {
            warn!(
                user_id = device.user_id().as_str(),
                device_id = device.device_id().as_str(),
                "The device keys have changed while an interactive verification \
                 was going on, not marking the device as verified.",
            );
            return Ok(None);
        }

        if verified_devices.is_some_and(|v| v.contains(&device)) {
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
                "The interactive verification process didn't verify the device",
            );

            Ok(None)
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use ruma::{
        device_id,
        events::{AnyToDeviceEventContent, ToDeviceEvent},
        user_id, DeviceId, UserId,
    };
    use tokio::sync::Mutex;

    use super::{event_enums::OutgoingContent, VerificationStore};
    use crate::{
        olm::PrivateCrossSigningIdentity,
        requests::{OutgoingRequest, OutgoingRequests},
        store::{Changes, CryptoStore, CryptoStoreWrapper, IdentityChanges, MemoryStore},
        types::events::ToDeviceEvents,
        OutgoingVerificationRequest, ReadOnlyAccount, ReadOnlyDevice, ReadOnlyOwnUserIdentity,
        ReadOnlyUserIdentity,
    };

    pub(crate) fn request_to_event(
        sender: &UserId,
        request: &OutgoingVerificationRequest,
    ) -> ToDeviceEvents {
        let content =
            request.to_owned().try_into().expect("Can't fetch content out of the request");
        wrap_any_to_device_content(sender, content)
    }

    pub(crate) fn outgoing_request_to_event(
        sender: &UserId,
        request: &OutgoingRequest,
    ) -> ToDeviceEvents {
        match request.request() {
            OutgoingRequests::ToDeviceRequest(r) => request_to_event(sender, &r.clone().into()),
            _ => panic!("Unsupported outgoing request"),
        }
    }

    pub(crate) fn wrap_any_to_device_content(
        sender: &UserId,
        content: OutgoingContent,
    ) -> ToDeviceEvents {
        let content = if let OutgoingContent::ToDevice(c) = content { c } else { unreachable!() };
        let sender = sender.to_owned();

        match content {
            AnyToDeviceEventContent::KeyVerificationRequest(c) => {
                ToDeviceEvents::KeyVerificationRequest(ToDeviceEvent { sender, content: c })
            }
            AnyToDeviceEventContent::KeyVerificationReady(c) => {
                ToDeviceEvents::KeyVerificationReady(ToDeviceEvent { sender, content: c })
            }
            AnyToDeviceEventContent::KeyVerificationKey(c) => {
                ToDeviceEvents::KeyVerificationKey(ToDeviceEvent { sender, content: c })
            }
            AnyToDeviceEventContent::KeyVerificationStart(c) => {
                ToDeviceEvents::KeyVerificationStart(ToDeviceEvent { sender, content: c })
            }
            AnyToDeviceEventContent::KeyVerificationAccept(c) => {
                ToDeviceEvents::KeyVerificationAccept(ToDeviceEvent { sender, content: c })
            }
            AnyToDeviceEventContent::KeyVerificationMac(c) => {
                ToDeviceEvents::KeyVerificationMac(ToDeviceEvent { sender, content: c })
            }
            AnyToDeviceEventContent::KeyVerificationDone(c) => {
                ToDeviceEvents::KeyVerificationDone(ToDeviceEvent { sender, content: c })
            }

            _ => unreachable!(),
        }
    }

    pub fn alice_id() -> &'static UserId {
        user_id!("@alice:example.org")
    }

    pub fn alice_device_id() -> &'static DeviceId {
        device_id!("JLAFKJWSCS")
    }

    pub fn bob_id() -> &'static UserId {
        user_id!("@bob:example.org")
    }

    pub fn bob_device_id() -> &'static DeviceId {
        device_id!("BOBDEVCIE")
    }

    pub(crate) async fn setup_stores() -> (VerificationStore, VerificationStore) {
        let alice = ReadOnlyAccount::with_device_id(alice_id(), alice_device_id());
        let alice_store = MemoryStore::new();
        let (alice_private_identity, _, _) =
            PrivateCrossSigningIdentity::with_account(&alice).await;
        let alice_private_identity = Mutex::new(alice_private_identity);

        let bob = ReadOnlyAccount::with_device_id(bob_id(), bob_device_id());
        let bob_store = MemoryStore::new();
        let (bob_private_identity, _, _) = PrivateCrossSigningIdentity::with_account(&bob).await;
        let bob_private_identity = Mutex::new(bob_private_identity);

        let alice_public_identity =
            ReadOnlyUserIdentity::from_private(&*alice_private_identity.lock().await).await;
        let alice_readonly_identity =
            ReadOnlyOwnUserIdentity::from_private(&*alice_private_identity.lock().await).await;
        let bob_public_identity =
            ReadOnlyUserIdentity::from_private(&*bob_private_identity.lock().await).await;
        let bob_readonly_identity =
            ReadOnlyOwnUserIdentity::from_private(&*bob_private_identity.lock().await).await;

        let alice_device = ReadOnlyDevice::from_account(&alice).await;
        let bob_device = ReadOnlyDevice::from_account(&bob).await;

        let alice_changes = Changes {
            identities: IdentityChanges {
                new: vec![alice_readonly_identity.into(), bob_public_identity.into()],
                changed: vec![],
            },
            ..Default::default()
        };
        alice_store.save_changes(alice_changes).await.unwrap();
        alice_store.save_devices(vec![bob_device]).await;

        let bob_changes = Changes {
            identities: IdentityChanges {
                new: vec![bob_readonly_identity.into(), alice_public_identity.into()],
                changed: vec![],
            },
            ..Default::default()
        };
        bob_store.save_changes(bob_changes).await.unwrap();
        bob_store.save_devices(vec![alice_device]).await;

        let alice_store = VerificationStore {
            inner: Arc::new(CryptoStoreWrapper::new(alice.user_id(), alice_store)),
            account: alice,
            private_identity: alice_private_identity.into(),
        };

        let bob_store = VerificationStore {
            account: bob.clone(),
            inner: Arc::new(CryptoStoreWrapper::new(bob.user_id(), bob_store)),
            private_identity: bob_private_identity.into(),
        };

        (alice_store, bob_store)
    }
}
