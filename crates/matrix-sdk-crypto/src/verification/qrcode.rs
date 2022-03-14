// Copyright 2021 The Matrix.org Foundation C.I.C.
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

use std::sync::{Arc, Mutex};

use matrix_qrcode::{
    qrcode::QrCode, EncodingError, QrVerificationData, SelfVerificationData,
    SelfVerificationNoMasterKey, VerificationData,
};
use ruma::{
    api::client::keys::upload_signatures::v3::Request as SignatureUploadRequest,
    events::{
        key::verification::{
            cancel::CancelCode,
            done::{KeyVerificationDoneEventContent, ToDeviceKeyVerificationDoneEventContent},
            start::{
                self, KeyVerificationStartEventContent, ReciprocateV1Content, StartMethod,
                ToDeviceKeyVerificationStartEventContent,
            },
            Relation,
        },
        AnyMessageEventContent, AnyToDeviceEventContent,
    },
    serde::Base64,
    DeviceId, DeviceKeyAlgorithm, RoomId, TransactionId, UserId,
};
use thiserror::Error;
use tracing::trace;

use super::{
    event_enums::{CancelContent, DoneContent, OutgoingContent, OwnedStartContent, StartContent},
    requests::RequestHandle,
    CancelInfo, Cancelled, Done, FlowId, IdentitiesBeingVerified, VerificationResult,
    VerificationStore,
};
use crate::{
    olm::PrivateCrossSigningIdentity, CryptoStoreError, OutgoingVerificationRequest,
    ReadOnlyDevice, ReadOnlyUserIdentities, RoomMessageRequest, ToDeviceRequest,
};

const SECRET_SIZE: usize = 16;

/// An error for the different failure modes that can happen during the
/// validation of a scanned QR code.
#[derive(Debug, Error)]
pub enum ScanError {
    /// An IO error inside the crypto store happened during the validation of
    /// the QR code scan.
    #[error(transparent)]
    Store(#[from] CryptoStoreError),
    /// A key mismatch happened during the validation of the QR code scan.
    #[error("The keys that are being verified didn't match (expected {expected}, found {found})")]
    KeyMismatch {
        /// The expected ed25519 key.
        expected: String,
        /// The ed25519 key that we got.
        found: String,
    },
    /// One of the users that is participating in this verification doesn't have
    /// a valid cross signing identity.
    #[error("The user {0} is missing a valid cross signing identity")]
    MissingCrossSigningIdentity(Box<UserId>),
    /// The device of the user that is participating in this verification
    /// doesn't have a valid device key.
    #[error("The user's {0} device {1} is not E2E capable")]
    MissingDeviceKeys(Box<UserId>, Box<DeviceId>),
    /// The ID uniquely identifying this verification flow didn't match to the
    /// one that has been scanned.
    #[error("The unique verification flow id did not match (expected {expected}, found {found})")]
    FlowIdMismatch {
        /// The expected verification flow id.
        expected: String,
        /// The verification flow id that we instead got.
        found: String,
    },
}

/// An object controlling QR code style key verification flows.
#[derive(Clone)]
pub struct QrVerification {
    flow_id: FlowId,
    inner: Arc<QrVerificationData>,
    state: Arc<Mutex<InnerState>>,
    identities: IdentitiesBeingVerified,
    request_handle: Option<RequestHandle>,
    we_started: bool,
}

impl std::fmt::Debug for QrVerification {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QrVerification")
            .field("flow_id", &self.flow_id)
            .field("inner", self.inner.as_ref())
            .field("state", &self.state.lock().unwrap())
            .finish()
    }
}

impl QrVerification {
    /// Has the QR verification been scanned by the other side.
    ///
    /// When the verification object is in this state it's required that the
    /// user confirms that the other side has scanned the QR code.
    pub fn has_been_scanned(&self) -> bool {
        matches!(&*self.state.lock().unwrap(), InnerState::Scanned(_))
    }

    /// Has the scanning of the QR code been confirmed by us.
    pub fn has_been_confirmed(&self) -> bool {
        matches!(&*self.state.lock().unwrap(), InnerState::Confirmed(_))
    }

    /// Get our own user id.
    pub fn user_id(&self) -> &UserId {
        self.identities.user_id()
    }

    /// Get the user id of the other user that is participating in this
    /// verification flow.
    pub fn other_user_id(&self) -> &UserId {
        self.identities.other_user_id()
    }

    /// Get the device id of the other side.
    pub fn other_device_id(&self) -> &DeviceId {
        self.identities.other_device_id()
    }

    /// Did we initiate the verification request
    pub fn we_started(&self) -> bool {
        self.we_started
    }

    /// Get info about the cancellation if the verification flow has been
    /// cancelled.
    pub fn cancel_info(&self) -> Option<CancelInfo> {
        if let InnerState::Cancelled(c) = &*self.state.lock().unwrap() {
            Some(c.state.clone().into())
        } else {
            None
        }
    }

    /// Has the verification flow completed.
    pub fn is_done(&self) -> bool {
        matches!(&*self.state.lock().unwrap(), InnerState::Done(_))
    }

    /// Has the verification flow been cancelled.
    pub fn is_cancelled(&self) -> bool {
        matches!(&*self.state.lock().unwrap(), InnerState::Cancelled(_))
    }

    /// Is this a verification that is veryfying one of our own devices
    pub fn is_self_verification(&self) -> bool {
        self.identities.is_self_verification()
    }

    /// Have we successfully scanned the QR code and are able to send a
    /// reciprocation event.
    pub fn reciprocated(&self) -> bool {
        matches!(&*self.state.lock().unwrap(), InnerState::Reciprocated(_))
    }

    /// Get the unique ID that identifies this QR code verification flow.
    pub fn flow_id(&self) -> &FlowId {
        &self.flow_id
    }

    /// Get the room id if the verification is happening inside a room.
    pub fn room_id(&self) -> Option<&RoomId> {
        match self.flow_id() {
            FlowId::ToDevice(_) => None,
            FlowId::InRoom(r, _) => Some(r),
        }
    }

    /// Generate a QR code object that is representing this verification flow.
    ///
    /// The `QrCode` can then be rendered as an image or as an unicode string.
    ///
    /// The [`to_bytes()`](#method.to_bytes) method can be used to instead
    /// output the raw bytes that should be encoded as a QR code.
    pub fn to_qr_code(&self) -> Result<QrCode, EncodingError> {
        self.inner.to_qr_code()
    }

    /// Generate a the raw bytes that should be encoded as a QR code is
    /// representing this verification flow.
    ///
    /// The [`to_qr_code()`](#method.to_qr_code) method can be used to instead
    /// output a `QrCode` object that can be rendered.
    pub fn to_bytes(&self) -> Result<Vec<u8>, EncodingError> {
        self.inner.to_bytes()
    }

    /// Cancel the verification flow.
    pub fn cancel(&self) -> Option<OutgoingVerificationRequest> {
        self.cancel_with_code(CancelCode::User)
    }

    /// Cancel the verification.
    ///
    /// This cancels the verification with given `CancelCode`.
    ///
    /// **Note**: This method should generally not be used, the [`cancel()`]
    /// method should be preferred. The SDK will automatically cancel with the
    /// appropriate cancel code, user initiated cancellations should only cancel
    /// with the `CancelCode::User`
    ///
    /// Returns None if the `Sas` object is already in a canceled state,
    /// otherwise it returns a request that needs to be sent out.
    ///
    /// [`cancel()`]: #method.cancel
    pub fn cancel_with_code(&self, code: CancelCode) -> Option<OutgoingVerificationRequest> {
        let mut state = self.state.lock().unwrap();

        if let Some(request) = &self.request_handle {
            request.cancel_with_code(&code);
        }

        let new_state = QrState::<Cancelled>::new(true, code);
        let content = new_state.as_content(self.flow_id());

        match &*state {
            InnerState::Confirmed(_)
            | InnerState::Created(_)
            | InnerState::Scanned(_)
            | InnerState::Reciprocated(_)
            | InnerState::Done(_) => {
                *state = InnerState::Cancelled(new_state);
                Some(self.content_to_request(content))
            }
            InnerState::Cancelled(_) => None,
        }
    }

    /// Notify the other side that we have successfully scanned the QR code and
    /// that the QR verification flow can start.
    ///
    /// This will return some `OutgoingContent` if the object is in the correct
    /// state to start the verification flow, otherwise `None`.
    pub fn reciprocate(&self) -> Option<OutgoingVerificationRequest> {
        match &*self.state.lock().unwrap() {
            InnerState::Reciprocated(s) => {
                Some(self.content_to_request(s.as_content(self.flow_id())))
            }
            InnerState::Created(_)
            | InnerState::Scanned(_)
            | InnerState::Confirmed(_)
            | InnerState::Done(_)
            | InnerState::Cancelled(_) => None,
        }
    }

    /// Confirm that the other side has scanned our QR code.
    pub fn confirm_scanning(&self) -> Option<OutgoingVerificationRequest> {
        let mut state = self.state.lock().unwrap();

        match &*state {
            InnerState::Scanned(s) => {
                let new_state = s.clone().confirm_scanning();
                let content = new_state.as_content(&self.flow_id);
                *state = InnerState::Confirmed(new_state);

                Some(self.content_to_request(content))
            }
            InnerState::Created(_)
            | InnerState::Cancelled(_)
            | InnerState::Confirmed(_)
            | InnerState::Reciprocated(_)
            | InnerState::Done(_) => None,
        }
    }

    fn content_to_request(&self, content: OutgoingContent) -> OutgoingVerificationRequest {
        match content {
            OutgoingContent::Room(room_id, content) => {
                RoomMessageRequest { room_id, txn_id: TransactionId::new(), content }.into()
            }
            OutgoingContent::ToDevice(c) => ToDeviceRequest::new(
                self.identities.other_user_id(),
                self.identities.other_device_id().to_owned(),
                c,
            )
            .into(),
        }
    }

    async fn mark_as_done(
        &self,
        new_state: QrState<Done>,
    ) -> Result<
        (Option<OutgoingVerificationRequest>, Option<SignatureUploadRequest>),
        CryptoStoreError,
    > {
        let (devices, identities) = new_state.verified_identities();

        let mut new_state = InnerState::Done(new_state);

        let (content, request) =
            match self.identities.mark_as_done(Some(&devices), Some(&identities)).await? {
                VerificationResult::Ok => (None, None),
                VerificationResult::Cancel(c) => {
                    let canceled = QrState::<Cancelled>::new(false, c);
                    let content = canceled.as_content(self.flow_id());
                    new_state = InnerState::Cancelled(canceled);
                    (Some(content), None)
                }
                VerificationResult::SignatureUpload(s) => (None, Some(s)),
            };

        *self.state.lock().unwrap() = new_state;

        Ok((content.map(|c| self.content_to_request(c)), request))
    }

    pub(crate) async fn receive_done(
        &self,
        content: &DoneContent<'_>,
    ) -> Result<
        (Option<OutgoingVerificationRequest>, Option<SignatureUploadRequest>),
        CryptoStoreError,
    > {
        let state = (*self.state.lock().unwrap()).clone();

        Ok(match state {
            InnerState::Confirmed(s) => {
                let (verified_device, verified_identity) = match &*self.inner {
                    QrVerificationData::Verification(_) => {
                        (None, self.identities.identity_being_verified.as_ref())
                    }
                    QrVerificationData::SelfVerification(_) => {
                        (Some(&self.identities.device_being_verified), None)
                    }
                    QrVerificationData::SelfVerificationNoMasterKey(_) => {
                        (None, self.identities.identity_being_verified.as_ref())
                    }
                };

                let new_state = s.clone().into_done(content, verified_device, verified_identity);
                self.mark_as_done(new_state).await?
            }
            InnerState::Reciprocated(s) => {
                let (verified_device, verified_identity) = match &*self.inner {
                    QrVerificationData::Verification(_) => {
                        (None, self.identities.identity_being_verified.as_ref())
                    }
                    QrVerificationData::SelfVerification(_) => {
                        (None, self.identities.identity_being_verified.as_ref())
                    }
                    QrVerificationData::SelfVerificationNoMasterKey(_) => {
                        (Some(&self.identities.device_being_verified), None)
                    }
                };

                let new_state = s.clone().into_done(content, verified_device, verified_identity);
                let content = Some(new_state.as_content(self.flow_id()));
                let (cancel_content, request) = self.mark_as_done(new_state).await?;

                if cancel_content.is_some() {
                    (cancel_content, request)
                } else {
                    (content.map(|c| self.content_to_request(c)), request)
                }
            }
            InnerState::Created(_)
            | InnerState::Scanned(_)
            | InnerState::Done(_)
            | InnerState::Cancelled(_) => (None, None),
        })
    }

    pub(crate) fn receive_reciprocation(
        &self,
        content: &StartContent<'_>,
    ) -> Option<OutgoingVerificationRequest> {
        let mut state = self.state.lock().unwrap();

        match &*state {
            InnerState::Created(s) => match s.clone().receive_reciprocate(content) {
                Ok(s) => {
                    *state = InnerState::Scanned(s);
                    None
                }
                Err(s) => {
                    let content = s.as_content(self.flow_id());
                    *state = InnerState::Cancelled(s);
                    Some(self.content_to_request(content))
                }
            },
            InnerState::Confirmed(_)
            | InnerState::Scanned(_)
            | InnerState::Reciprocated(_)
            | InnerState::Done(_)
            | InnerState::Cancelled(_) => None,
        }
    }

    pub(crate) fn receive_cancel(&self, sender: &UserId, content: &CancelContent<'_>) {
        if sender == self.other_user_id() {
            let mut state = self.state.lock().unwrap();

            let new_state = match &*state {
                InnerState::Created(s) => s.clone().into_cancelled(content),
                InnerState::Scanned(s) => s.clone().into_cancelled(content),
                InnerState::Confirmed(s) => s.clone().into_cancelled(content),
                InnerState::Reciprocated(s) => s.clone().into_cancelled(content),
                InnerState::Done(_) | InnerState::Cancelled(_) => return,
            };

            trace!(
                sender = sender.as_str(),
                code = content.cancel_code().as_str(),
                "Cancelling a QR verification, other user has cancelled"
            );

            *state = InnerState::Cancelled(new_state);
        }
    }

    fn generate_secret() -> Base64 {
        let mut shared_secret = [0u8; SECRET_SIZE];
        getrandom::getrandom(&mut shared_secret)
            .expect("Can't generate randomness for the shared secret");
        Base64::new(shared_secret.to_vec())
    }

    pub(crate) fn new_self(
        flow_id: FlowId,
        own_master_key: String,
        other_device_key: String,
        identities: IdentitiesBeingVerified,
        we_started: bool,
        request_handle: Option<RequestHandle>,
    ) -> Self {
        let secret = Self::generate_secret();

        let inner: QrVerificationData = SelfVerificationData::new(
            flow_id.as_str().to_owned(),
            own_master_key,
            other_device_key,
            secret,
        )
        .into();

        Self::new_helper(flow_id, inner, identities, we_started, request_handle)
    }

    pub(crate) fn new_self_no_master(
        store: VerificationStore,
        flow_id: FlowId,
        own_master_key: String,
        identities: IdentitiesBeingVerified,
        we_started: bool,
        request_handle: Option<RequestHandle>,
    ) -> QrVerification {
        let secret = Self::generate_secret();

        let inner: QrVerificationData = SelfVerificationNoMasterKey::new(
            flow_id.as_str().to_owned(),
            store.account.identity_keys().ed25519().to_owned(),
            own_master_key,
            secret,
        )
        .into();

        Self::new_helper(flow_id, inner, identities, we_started, request_handle)
    }

    pub(crate) fn new_cross(
        flow_id: FlowId,
        own_master_key: String,
        other_master_key: String,
        identities: IdentitiesBeingVerified,
        we_started: bool,
        request_handle: Option<RequestHandle>,
    ) -> Self {
        let secret = Self::generate_secret();

        let event_id = if let FlowId::InRoom(_, e) = &flow_id {
            e.to_owned()
        } else {
            panic!("A verification between users is only valid in a room");
        };

        let inner: QrVerificationData =
            VerificationData::new(event_id, own_master_key, other_master_key, secret).into();

        Self::new_helper(flow_id, inner, identities, we_started, request_handle)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn from_scan(
        store: VerificationStore,
        private_identity: PrivateCrossSigningIdentity,
        other_user_id: Box<UserId>,
        other_device_id: Box<DeviceId>,
        flow_id: FlowId,
        qr_code: QrVerificationData,
        we_started: bool,
        request_handle: Option<RequestHandle>,
    ) -> Result<Self, ScanError> {
        if flow_id.as_str() != qr_code.flow_id() {
            return Err(ScanError::FlowIdMismatch {
                expected: flow_id.as_str().to_owned(),
                found: qr_code.flow_id().to_owned(),
            });
        }

        let own_identity =
            store.get_user_identity(store.account.user_id()).await?.ok_or_else(|| {
                ScanError::MissingCrossSigningIdentity(store.account.user_id().to_owned())
            })?;
        let other_identity = store
            .get_user_identity(&other_user_id)
            .await?
            .ok_or_else(|| ScanError::MissingCrossSigningIdentity(other_user_id.clone()))?;
        let other_device =
            store.get_device(&other_user_id, &other_device_id).await?.ok_or_else(|| {
                ScanError::MissingDeviceKeys(other_user_id.clone(), other_device_id.clone())
            })?;

        let check_master_key = |key, identity: &ReadOnlyUserIdentities| {
            let master_key = identity.master_key().get_first_key().ok_or_else(|| {
                ScanError::MissingCrossSigningIdentity(identity.user_id().to_owned())
            })?;

            if key != master_key {
                Err(ScanError::KeyMismatch {
                    expected: master_key.to_owned(),
                    found: qr_code.first_key().to_owned(),
                })
            } else {
                Ok(())
            }
        };

        let (device_being_verified, identity_being_verified) = match qr_code {
            QrVerificationData::Verification(_) => {
                check_master_key(qr_code.first_key(), &other_identity)?;
                check_master_key(qr_code.second_key(), &own_identity)?;

                (other_device, Some(other_identity))
            }
            QrVerificationData::SelfVerification(_) => {
                check_master_key(qr_code.first_key(), &other_identity)?;
                if qr_code.second_key() != store.account.identity_keys().ed25519() {
                    return Err(ScanError::KeyMismatch {
                        expected: store.account.identity_keys().ed25519().to_owned(),
                        found: qr_code.second_key().to_owned(),
                    });
                }

                (other_device, Some(other_identity))
            }
            QrVerificationData::SelfVerificationNoMasterKey(_) => {
                let device_key =
                    other_device.get_key(DeviceKeyAlgorithm::Ed25519).ok_or_else(|| {
                        ScanError::MissingDeviceKeys(other_user_id.clone(), other_device_id.clone())
                    })?;
                if qr_code.first_key() != device_key {
                    return Err(ScanError::KeyMismatch {
                        expected: device_key.to_owned(),
                        found: qr_code.first_key().to_owned(),
                    });
                }
                check_master_key(qr_code.second_key(), &other_identity)?;
                (other_device, Some(other_identity))
            }
        };

        let identities = IdentitiesBeingVerified {
            private_identity,
            store: store.clone(),
            device_being_verified,
            identity_being_verified,
        };

        let secret = qr_code.secret().to_owned();
        let own_device_id = store.account.device_id().to_owned();

        Ok(Self {
            flow_id,
            inner: qr_code.into(),
            state: Mutex::new(InnerState::Reciprocated(QrState {
                state: Reciprocated { secret, own_device_id },
            }))
            .into(),
            identities,
            we_started,
            request_handle,
        })
    }

    fn new_helper(
        flow_id: FlowId,
        inner: QrVerificationData,
        identities: IdentitiesBeingVerified,
        we_started: bool,
        request_handle: Option<RequestHandle>,
    ) -> Self {
        let secret = inner.secret().to_owned();

        Self {
            flow_id,
            inner: inner.into(),
            state: Mutex::new(InnerState::Created(QrState { state: Created { secret } })).into(),
            identities,
            we_started,
            request_handle,
        }
    }
}

#[derive(Debug, Clone)]
enum InnerState {
    Created(QrState<Created>),
    Scanned(QrState<Scanned>),
    Confirmed(QrState<Confirmed>),
    Reciprocated(QrState<Reciprocated>),
    Done(QrState<Done>),
    Cancelled(QrState<Cancelled>),
}

#[derive(Clone, Debug)]
struct QrState<S: Clone> {
    state: S,
}

impl<S: Clone> QrState<S> {
    pub fn into_cancelled(self, content: &CancelContent<'_>) -> QrState<Cancelled> {
        QrState { state: Cancelled::new(false, content.cancel_code().to_owned()) }
    }
}

#[derive(Clone, Debug)]
struct Created {
    secret: Base64,
}

#[derive(Clone, Debug)]
struct Scanned {}

#[derive(Clone, Debug)]
struct Confirmed {}

#[derive(Clone, Debug)]
struct Reciprocated {
    own_device_id: Box<DeviceId>,
    secret: Base64,
}

impl Reciprocated {
    fn as_content(&self, flow_id: &FlowId) -> OutgoingContent {
        let content = ReciprocateV1Content::new(self.secret.clone());
        let method = StartMethod::ReciprocateV1(content);

        let content: OwnedStartContent = match flow_id {
            FlowId::ToDevice(t) => ToDeviceKeyVerificationStartEventContent::new(
                self.own_device_id.clone(),
                t.clone(),
                method,
            )
            .into(),
            FlowId::InRoom(r, e) => (
                r.clone(),
                KeyVerificationStartEventContent::new(
                    self.own_device_id.clone(),
                    method,
                    Relation::new(e.clone()),
                ),
            )
                .into(),
        };

        content.into()
    }
}

impl QrState<Scanned> {
    fn confirm_scanning(self) -> QrState<Confirmed> {
        QrState { state: Confirmed {} }
    }
}

impl QrState<Cancelled> {
    fn new(cancelled_by_us: bool, cancel_code: CancelCode) -> Self {
        QrState { state: Cancelled::new(cancelled_by_us, cancel_code) }
    }

    fn as_content(&self, flow_id: &FlowId) -> OutgoingContent {
        self.state.as_content(flow_id)
    }
}

impl QrState<Created> {
    fn receive_reciprocate(
        self,
        content: &StartContent<'_>,
    ) -> Result<QrState<Scanned>, QrState<Cancelled>> {
        match content.method() {
            start::StartMethod::ReciprocateV1(m) => {
                // TODO use constant time eq here.
                if self.state.secret == m.secret {
                    Ok(QrState { state: Scanned {} })
                } else {
                    Err(QrState::<Cancelled>::new(false, CancelCode::KeyMismatch))
                }
            }
            _ => Err(QrState::<Cancelled>::new(false, CancelCode::UnknownMethod)),
        }
    }
}

impl QrState<Done> {
    fn as_content(&self, flow_id: &FlowId) -> OutgoingContent {
        self.state.as_content(flow_id)
    }

    fn verified_identities(&self) -> (Arc<[ReadOnlyDevice]>, Arc<[ReadOnlyUserIdentities]>) {
        (self.state.verified_devices.clone(), self.state.verified_master_keys.clone())
    }
}

impl QrState<Confirmed> {
    fn into_done(
        self,
        _: &DoneContent<'_>,
        verified_device: Option<&ReadOnlyDevice>,
        verified_identity: Option<&ReadOnlyUserIdentities>,
    ) -> QrState<Done> {
        let devices: Vec<_> = verified_device.into_iter().cloned().collect();
        let identities: Vec<_> = verified_identity.into_iter().cloned().collect();

        QrState {
            state: Done {
                verified_devices: devices.into(),
                verified_master_keys: identities.into(),
            },
        }
    }

    fn as_content(&self, flow_id: &FlowId) -> OutgoingContent {
        match flow_id {
            FlowId::ToDevice(t) => AnyToDeviceEventContent::KeyVerificationDone(
                ToDeviceKeyVerificationDoneEventContent::new(t.to_owned()),
            )
            .into(),
            FlowId::InRoom(r, e) => (
                r.to_owned(),
                AnyMessageEventContent::KeyVerificationDone(KeyVerificationDoneEventContent::new(
                    Relation::new(e.to_owned()),
                )),
            )
                .into(),
        }
    }
}

impl QrState<Reciprocated> {
    fn as_content(&self, flow_id: &FlowId) -> OutgoingContent {
        self.state.as_content(flow_id)
    }

    fn into_done(
        self,
        _: &DoneContent<'_>,
        verified_device: Option<&ReadOnlyDevice>,
        verified_identity: Option<&ReadOnlyUserIdentities>,
    ) -> QrState<Done> {
        let devices: Vec<_> = verified_device.into_iter().cloned().collect();
        let identities: Vec<_> = verified_identity.into_iter().cloned().collect();

        QrState {
            state: Done {
                verified_devices: devices.into(),
                verified_master_keys: identities.into(),
            },
        }
    }
}

#[cfg(test)]
mod test {
    use std::{convert::TryFrom, sync::Arc};

    use matrix_qrcode::QrVerificationData;
    use matrix_sdk_test::async_test;
    use ruma::{device_id, event_id, room_id, user_id, DeviceId, UserId};

    use crate::{
        olm::{PrivateCrossSigningIdentity, ReadOnlyAccount},
        store::{Changes, CryptoStore, MemoryStore},
        verification::{
            event_enums::{DoneContent, OutgoingContent, StartContent},
            FlowId, IdentitiesBeingVerified, VerificationStore,
        },
        QrVerification, ReadOnlyDevice,
    };

    fn user_id() -> &'static UserId {
        user_id!("@example:localhost")
    }

    fn memory_store() -> Arc<dyn CryptoStore> {
        Arc::new(MemoryStore::new())
    }

    fn device_id() -> &'static DeviceId {
        device_id!("DEVICEID")
    }

    #[async_test]
    async fn test_verification_creation() {
        let store = memory_store();
        let account = ReadOnlyAccount::new(user_id(), device_id());

        let store = VerificationStore { account: account.clone(), inner: store };

        let private_identity = PrivateCrossSigningIdentity::new(user_id().to_owned()).await;
        let flow_id = FlowId::ToDevice("test_transaction".into());

        let device_key = account.identity_keys().ed25519().to_owned();
        let master_key = private_identity.master_public_key().await.unwrap();
        let master_key = master_key.get_first_key().unwrap().to_owned();

        let alice_device = ReadOnlyDevice::from_account(&account).await;

        let identities = IdentitiesBeingVerified {
            private_identity,
            store: store.clone(),
            device_being_verified: alice_device,
            identity_being_verified: None,
        };

        let verification = QrVerification::new_self_no_master(
            store.clone(),
            flow_id.clone(),
            master_key.clone(),
            identities.clone(),
            false,
            None,
        );

        assert_eq!(verification.inner.first_key(), &device_key);
        assert_eq!(verification.inner.second_key(), &master_key);

        let verification = QrVerification::new_self(
            flow_id,
            master_key.clone(),
            device_key.clone(),
            identities.clone(),
            false,
            None,
        );

        assert_eq!(verification.inner.first_key(), &master_key);
        assert_eq!(verification.inner.second_key(), &device_key);

        let bob_identity =
            PrivateCrossSigningIdentity::new(user_id!("@bob:example").to_owned()).await;
        let bob_master_key = bob_identity.master_public_key().await.unwrap();
        let bob_master_key = bob_master_key.get_first_key().unwrap().to_owned();

        let flow_id =
            FlowId::InRoom(room_id!("!test:example").to_owned(), event_id!("$EVENTID").to_owned());

        let verification = QrVerification::new_cross(
            flow_id,
            master_key.clone(),
            bob_master_key.clone(),
            identities,
            false,
            None,
        );

        assert_eq!(verification.inner.first_key(), &master_key);
        assert_eq!(verification.inner.second_key(), &bob_master_key);
    }

    #[async_test]
    async fn test_reciprocate_receival() {
        let test = |flow_id: FlowId| async move {
            let alice_account = ReadOnlyAccount::new(user_id(), device_id());
            let store = memory_store();

            let store = VerificationStore { account: alice_account.clone(), inner: store };

            let bob_account =
                ReadOnlyAccount::new(alice_account.user_id(), device_id!("BOBDEVICE"));

            let private_identity = PrivateCrossSigningIdentity::new(user_id().to_owned()).await;
            let identity = private_identity.to_public_identity().await.unwrap();

            let master_key = private_identity.master_public_key().await.unwrap();
            let master_key = master_key.get_first_key().unwrap().to_owned();

            let alice_device = ReadOnlyDevice::from_account(&alice_account).await;
            let bob_device = ReadOnlyDevice::from_account(&bob_account).await;

            let mut changes = Changes::default();
            changes.identities.new.push(identity.clone().into());
            changes.devices.new.push(bob_device.clone());
            store.save_changes(changes).await.unwrap();

            let identities = IdentitiesBeingVerified {
                private_identity: PrivateCrossSigningIdentity::empty(
                    alice_account.user_id().to_owned(),
                ),
                store: store.clone(),
                device_being_verified: alice_device.clone(),
                identity_being_verified: Some(identity.clone().into()),
            };

            let alice_verification = QrVerification::new_self_no_master(
                store,
                flow_id.clone(),
                master_key.clone(),
                identities,
                false,
                None,
            );

            let bob_store = memory_store();

            let bob_store = VerificationStore { account: bob_account.clone(), inner: bob_store };

            let mut changes = Changes::default();
            changes.identities.new.push(identity.into());
            changes.devices.new.push(alice_device.clone());
            bob_store.save_changes(changes).await.unwrap();

            let qr_code = alice_verification.to_bytes().unwrap();
            let qr_code = QrVerificationData::from_bytes(qr_code).unwrap();

            let bob_verification = QrVerification::from_scan(
                bob_store,
                private_identity,
                alice_account.user_id().to_owned(),
                alice_account.device_id().to_owned(),
                flow_id,
                qr_code,
                false,
                None,
            )
            .await
            .unwrap();

            let request = bob_verification.reciprocate().unwrap();
            let content = OutgoingContent::try_from(request).unwrap();
            let content = StartContent::try_from(&content).unwrap();

            alice_verification.receive_reciprocation(&content);

            let request = alice_verification.confirm_scanning().unwrap();
            let content = OutgoingContent::try_from(request).unwrap();
            let content = DoneContent::try_from(&content).unwrap();

            assert!(!alice_verification.is_done());
            assert!(!bob_verification.is_done());

            let (request, _) = bob_verification.receive_done(&content).await.unwrap();
            let content = OutgoingContent::try_from(request.unwrap()).unwrap();
            let content = DoneContent::try_from(&content).unwrap();
            alice_verification.receive_done(&content).await.unwrap();

            assert!(alice_verification.is_done());
            assert!(bob_verification.is_done());

            let identity = alice_verification
                .identities
                .store
                .get_user_identity(alice_account.user_id())
                .await
                .unwrap()
                .unwrap();

            let identity = identity.own().unwrap();

            assert!(!bob_device.is_locally_trusted());
            assert!(alice_device.is_locally_trusted());
            assert!(identity.is_verified());
        };

        let flow_id = FlowId::ToDevice("test_transaction".into());
        test(flow_id).await;

        let flow_id =
            FlowId::InRoom(room_id!("!test:example").to_owned(), event_id!("$EVENTID").to_owned());
        test(flow_id).await;
    }
}
