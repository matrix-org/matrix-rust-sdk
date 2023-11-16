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

use std::sync::Arc;

use as_variant::as_variant;
use eyeball::{ObservableWriteGuard, SharedObservable};
use futures_core::Stream;
use futures_util::StreamExt;
use matrix_sdk_qrcode::{
    qrcode::QrCode, EncodingError, QrVerificationData, SelfVerificationData,
    SelfVerificationNoMasterKey, VerificationData,
};
use rand::{thread_rng, RngCore};
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
        },
        relation::Reference,
        AnyMessageLikeEventContent, AnyToDeviceEventContent,
    },
    serde::Base64,
    DeviceId, OwnedDeviceId, OwnedUserId, RoomId, TransactionId, UserId,
};
use thiserror::Error;
use tracing::trace;
use vodozemac::Ed25519PublicKey;

use super::{
    event_enums::{CancelContent, DoneContent, OutgoingContent, OwnedStartContent, StartContent},
    requests::RequestHandle,
    CancelInfo, Cancelled, Done, FlowId, IdentitiesBeingVerified, VerificationResult,
    VerificationStore,
};
use crate::{
    CryptoStoreError, OutgoingVerificationRequest, ReadOnlyDevice, ReadOnlyUserIdentities,
    RoomMessageRequest, ToDeviceRequest,
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
    MissingCrossSigningIdentity(OwnedUserId),
    /// The device of the user that is participating in this verification
    /// doesn't have a valid device key.
    #[error("The user's {0} device {1} is not E2E capable")]
    MissingDeviceKeys(OwnedUserId, OwnedDeviceId),
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

/// An Enum describing the state the QrCode verification is in.
#[derive(Debug, Clone)]
pub enum QrVerificationState {
    /// The QR verification has been started.
    ///
    /// We have received the other device's details (from the
    /// `m.key.verification.request` or `m.key.verification.ready`) and
    /// established the shared secret, so can
    /// display the QR code.
    ///
    /// Note that despite the name of this state, we have not yet sent or
    /// received an `m.key.verification.start` message.
    Started,
    /// The QR verification has been scanned by the other side.
    Scanned,
    /// We have confirmed the other side's scan of the QR code.
    Confirmed,
    /// We have successfully scanned the QR code and are able to send a
    /// reciprocation event.
    ///
    /// Call `QrVerification::reciprocate` to build the reciprocation message.
    ///
    /// Note that, despite the name of this state, we have not necessarily
    /// yet sent the `m.reciprocate.v1` message.
    Reciprocated,
    /// The verification process has been successfully concluded.
    Done {
        /// The list of devices that has been verified.
        verified_devices: Vec<ReadOnlyDevice>,
        /// The list of user identities that has been verified.
        verified_identities: Vec<ReadOnlyUserIdentities>,
    },
    /// The verification process has been cancelled.
    Cancelled(CancelInfo),
}

impl From<&InnerState> for QrVerificationState {
    fn from(value: &InnerState) -> Self {
        match value {
            InnerState::Created(_) => Self::Started,
            InnerState::Scanned(_) => Self::Scanned,
            InnerState::Confirmed(_) => Self::Confirmed,
            InnerState::Reciprocated(_) => Self::Reciprocated,
            InnerState::Done(s) => Self::Done {
                verified_devices: s.state.verified_devices.to_vec(),
                verified_identities: s.state.verified_master_keys.to_vec(),
            },
            InnerState::Cancelled(s) => Self::Cancelled(s.state.to_owned().into()),
        }
    }
}

/// An object controlling QR code style key verification flows.
#[derive(Clone)]
pub struct QrVerification {
    flow_id: FlowId,
    inner: Arc<QrVerificationData>,
    state: SharedObservable<InnerState>,
    identities: IdentitiesBeingVerified,
    request_handle: Option<RequestHandle>,
    we_started: bool,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for QrVerification {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QrVerification")
            .field("flow_id", &self.flow_id)
            .field("inner", &self.inner)
            .field("state", &self.state)
            .finish()
    }
}

impl QrVerification {
    /// Has the QR verification been scanned by the other side.
    ///
    /// When the verification object is in this state it's required that the
    /// user confirms that the other side has scanned the QR code.
    pub fn has_been_scanned(&self) -> bool {
        matches!(*self.state.read(), InnerState::Scanned(_))
    }

    /// Has the scanning of the QR code been confirmed by us.
    pub fn has_been_confirmed(&self) -> bool {
        matches!(*self.state.read(), InnerState::Confirmed(_))
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

    /// Get the device ID of the other side.
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
        as_variant!(&*self.state.read(), InnerState::Cancelled(c) => {
            c.state.clone().into()
        })
    }

    /// Has the verification flow completed.
    pub fn is_done(&self) -> bool {
        matches!(*self.state.read(), InnerState::Done(_))
    }

    /// Has the verification flow been cancelled.
    pub fn is_cancelled(&self) -> bool {
        matches!(*self.state.read(), InnerState::Cancelled(_))
    }

    /// Is this a verification that is verifying one of our own devices
    pub fn is_self_verification(&self) -> bool {
        self.identities.is_self_verification()
    }

    /// Have we successfully scanned the QR code and are able to send a
    /// reciprocation event.
    pub fn reciprocated(&self) -> bool {
        matches!(*self.state.read(), InnerState::Reciprocated(_))
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
        let mut state = self.state.write();

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
                ObservableWriteGuard::set(&mut state, InnerState::Cancelled(new_state));
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
        match &*self.state.read() {
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
        let mut state = self.state.write();

        match &*state {
            InnerState::Scanned(s) => {
                let new_state = s.clone().confirm_scanning();
                let content = new_state.as_content(&self.flow_id);
                ObservableWriteGuard::set(&mut state, InnerState::Confirmed(new_state));

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
            OutgoingContent::ToDevice(c) => ToDeviceRequest::with_id(
                self.identities.other_user_id(),
                self.identities.other_device_id().to_owned(),
                &c,
                TransactionId::new(),
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

        self.state.set(new_state);
        Ok((content.map(|c| self.content_to_request(c)), request))
    }

    pub(crate) async fn receive_done(
        &self,
        content: &DoneContent<'_>,
    ) -> Result<
        (Option<OutgoingVerificationRequest>, Option<SignatureUploadRequest>),
        CryptoStoreError,
    > {
        let state = self.state.get();

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
        let mut state = self.state.write();

        match &*state {
            InnerState::Created(s) => match s.clone().receive_reciprocate(content) {
                Ok(s) => {
                    ObservableWriteGuard::set(&mut state, InnerState::Scanned(s));
                    None
                }
                Err(s) => {
                    let content = s.as_content(self.flow_id());
                    ObservableWriteGuard::set(&mut state, InnerState::Cancelled(s));
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
            let mut state = self.state.write();

            let new_state = match &*state {
                InnerState::Created(s) => s.clone().into_cancelled(content),
                InnerState::Scanned(s) => s.clone().into_cancelled(content),
                InnerState::Confirmed(s) => s.clone().into_cancelled(content),
                InnerState::Reciprocated(s) => s.clone().into_cancelled(content),
                InnerState::Done(_) | InnerState::Cancelled(_) => return,
            };

            trace!(
                ?sender,
                code = content.cancel_code().as_str(),
                "Cancelling a QR verification, other user has cancelled"
            );

            ObservableWriteGuard::set(&mut state, InnerState::Cancelled(new_state));
        }
    }

    fn generate_secret() -> Base64 {
        let mut shared_secret = vec![0u8; SECRET_SIZE];
        let mut rng = thread_rng();
        rng.fill_bytes(&mut shared_secret);

        Base64::new(shared_secret)
    }

    pub(crate) fn new_self(
        flow_id: FlowId,
        own_master_key: Ed25519PublicKey,
        other_device_key: Ed25519PublicKey,
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
        own_master_key: Ed25519PublicKey,
        identities: IdentitiesBeingVerified,
        we_started: bool,
        request_handle: Option<RequestHandle>,
    ) -> QrVerification {
        let secret = Self::generate_secret();

        let inner: QrVerificationData = SelfVerificationNoMasterKey::new(
            flow_id.as_str().to_owned(),
            store.account.identity_keys.ed25519,
            own_master_key,
            secret,
        )
        .into();

        Self::new_helper(flow_id, inner, identities, we_started, request_handle)
    }

    pub(crate) fn new_cross(
        flow_id: FlowId,
        own_master_key: Ed25519PublicKey,
        other_master_key: Ed25519PublicKey,
        identities: IdentitiesBeingVerified,
        we_started: bool,
        request_handle: Option<RequestHandle>,
    ) -> Self {
        let secret = Self::generate_secret();

        let inner: QrVerificationData = VerificationData::new(
            flow_id.as_str().to_owned(),
            own_master_key,
            other_master_key,
            secret,
        )
        .into();

        Self::new_helper(flow_id, inner, identities, we_started, request_handle)
    }

    pub(crate) async fn from_scan(
        store: VerificationStore,
        other_user_id: OwnedUserId,
        other_device_id: OwnedDeviceId,
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

        let other_device =
            store.get_device(&other_user_id, &other_device_id).await?.ok_or_else(|| {
                ScanError::MissingDeviceKeys(other_user_id.clone(), other_device_id.clone())
            })?;

        let identities = store.get_identities(other_device).await?;

        let own_identity = identities
            .own_identity
            .as_ref()
            .ok_or_else(|| ScanError::MissingCrossSigningIdentity(store.account.user_id.clone()))?;

        let other_identity = identities
            .identity_being_verified
            .as_ref()
            .ok_or_else(|| ScanError::MissingCrossSigningIdentity(other_user_id.clone()))?;

        let check_master_key = |key, identity: &ReadOnlyUserIdentities| {
            let master_key = identity.master_key().get_first_key().ok_or_else(|| {
                ScanError::MissingCrossSigningIdentity(identity.user_id().to_owned())
            })?;

            if key != master_key {
                Err(ScanError::KeyMismatch {
                    expected: master_key.to_base64(),
                    found: qr_code.first_key().to_base64(),
                })
            } else {
                Ok(())
            }
        };

        match qr_code {
            QrVerificationData::Verification(_) => {
                check_master_key(qr_code.first_key(), other_identity)?;
                check_master_key(qr_code.second_key(), &own_identity.to_owned().into())?;
            }
            QrVerificationData::SelfVerification(_) => {
                check_master_key(qr_code.first_key(), other_identity)?;
                if qr_code.second_key() != store.account.identity_keys.ed25519 {
                    return Err(ScanError::KeyMismatch {
                        expected: store.account.identity_keys.ed25519.to_base64(),
                        found: qr_code.second_key().to_base64(),
                    });
                }
            }
            QrVerificationData::SelfVerificationNoMasterKey(_) => {
                let device_key =
                    identities.device_being_verified.ed25519_key().ok_or_else(|| {
                        ScanError::MissingDeviceKeys(other_user_id.clone(), other_device_id.clone())
                    })?;

                if qr_code.first_key() != device_key {
                    return Err(ScanError::KeyMismatch {
                        expected: device_key.to_base64(),
                        found: qr_code.first_key().to_base64(),
                    });
                }
                check_master_key(qr_code.second_key(), other_identity)?;
            }
        };

        let secret = qr_code.secret().to_owned();
        let own_device_id = store.account.device_id.clone();

        Ok(Self {
            flow_id,
            inner: qr_code.into(),
            state: SharedObservable::new(InnerState::Reciprocated(QrState {
                state: Reciprocated { secret, own_device_id },
            })),
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
            state: SharedObservable::new(InnerState::Created(QrState {
                state: Created { secret },
            })),
            identities,
            we_started,
            request_handle,
        }
    }

    /// Listen for changes in the QrCode verification process.
    ///
    /// The changes are presented as a stream of [`QrVerificationState`] values.
    pub fn changes(&self) -> impl Stream<Item = QrVerificationState> {
        self.state.subscribe().map(|s| (&s).into())
    }

    /// Get the current state the verification process is in.
    ///
    /// To listen to changes to the [`QrVerificationState`] use the
    /// [`QrVerification::changes`] method.
    pub fn state(&self) -> QrVerificationState {
        (&*self.state.read()).into()
    }
}

#[derive(Debug, Clone)]
enum InnerState {
    /// We have received the other device's details (from the
    /// `m.key.verification.request` or `m.key.verification.ready`) and
    /// established the shared secret, so can
    /// display the QR code.
    Created(QrState<Created>),

    /// The other side has scanned our QR code and sent an
    /// `m.key.verification.start` message with `method: m.reciprocate.v1` with
    /// matching shared secret.
    Scanned(QrState<Scanned>),

    /// Our user has confirmed that the other device scanned successfully. We
    /// have sent an `m.key.verification.done`.
    Confirmed(QrState<Confirmed>),

    /// We have scanned the other side's QR code and are able to send a
    /// `m.key.verification.start` message with `method: m.reciprocate.v1`.
    ///
    /// Call `QrVerification::reciprocate` to build the start message.
    ///
    /// Note that, despite the name of this state, we have not necessarily
    /// yet sent the `m.reciprocate.v1` message.
    Reciprocated(QrState<Reciprocated>),

    /// Verification complete: we have received an `m.key.verification.done`
    /// from the other side.
    Done(QrState<Done>),

    /// Verification cancelled or failed.
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
    own_device_id: OwnedDeviceId,
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
                    Reference::new(e.clone()),
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
                AnyMessageLikeEventContent::KeyVerificationDone(
                    KeyVerificationDoneEventContent::new(Reference::new(e.to_owned())),
                ),
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
mod tests {
    use std::sync::Arc;

    use assert_matches::assert_matches;
    use matrix_sdk_qrcode::QrVerificationData;
    use matrix_sdk_test::async_test;
    use ruma::{device_id, event_id, room_id, user_id, DeviceId, UserId};
    use tokio::sync::Mutex;

    use crate::{
        olm::{Account, PrivateCrossSigningIdentity},
        store::{Changes, CryptoStoreWrapper, MemoryStore},
        verification::{
            event_enums::{DoneContent, OutgoingContent, StartContent},
            FlowId, VerificationStore,
        },
        QrVerification, QrVerificationState, ReadOnlyDevice,
    };

    fn user_id() -> &'static UserId {
        user_id!("@example:localhost")
    }

    fn memory_store(user_id: &UserId) -> Arc<CryptoStoreWrapper> {
        Arc::new(CryptoStoreWrapper::new(user_id, MemoryStore::new()))
    }

    fn device_id() -> &'static DeviceId {
        device_id!("DEVICEID")
    }

    #[async_test]
    async fn test_verification_creation() {
        let account = Account::with_device_id(user_id(), device_id());
        let store = memory_store(account.user_id());

        let private_identity = PrivateCrossSigningIdentity::new(user_id().to_owned());
        let master_key = private_identity.master_public_key().await.unwrap();
        let master_key = master_key.get_first_key().unwrap().to_owned();

        let store = VerificationStore {
            account: account.static_data.clone(),
            inner: store,
            private_identity: Mutex::new(private_identity).into(),
        };

        let flow_id = FlowId::ToDevice("test_transaction".into());

        let device_key = account.static_data.identity_keys.ed25519;
        let alice_device = ReadOnlyDevice::from_account(&account);

        let identities = store.get_identities(alice_device).await.unwrap();

        let verification = QrVerification::new_self_no_master(
            store.clone(),
            flow_id.clone(),
            master_key,
            identities.clone(),
            false,
            None,
        );

        assert_matches!(verification.state(), QrVerificationState::Started);
        assert_eq!(verification.inner.first_key(), device_key);
        assert_eq!(verification.inner.second_key(), master_key);

        let verification = QrVerification::new_self(
            flow_id,
            master_key,
            device_key,
            identities.clone(),
            false,
            None,
        );

        assert_matches!(verification.state(), QrVerificationState::Started);
        assert_eq!(verification.inner.first_key(), master_key);
        assert_eq!(verification.inner.second_key(), device_key);

        let bob_identity = PrivateCrossSigningIdentity::new(user_id!("@bob:example").to_owned());
        let bob_master_key = bob_identity.master_public_key().await.unwrap();
        let bob_master_key = bob_master_key.get_first_key().unwrap().to_owned();

        let flow_id =
            FlowId::InRoom(room_id!("!test:example").to_owned(), event_id!("$EVENTID").to_owned());

        let verification =
            QrVerification::new_cross(flow_id, master_key, bob_master_key, identities, false, None);

        assert_matches!(verification.state(), QrVerificationState::Started);
        assert_eq!(verification.inner.first_key(), master_key);
        assert_eq!(verification.inner.second_key(), bob_master_key);
    }

    #[async_test]
    async fn test_reciprocate_receival() {
        let test = |flow_id: FlowId| async move {
            let alice_account = Account::with_device_id(user_id(), device_id());
            let store = memory_store(alice_account.user_id());

            let private_identity = PrivateCrossSigningIdentity::new(user_id().to_owned());

            let store = VerificationStore {
                account: alice_account.static_data.clone(),
                inner: store,
                private_identity: Mutex::new(private_identity).into(),
            };

            let bob_account =
                Account::with_device_id(alice_account.user_id(), device_id!("BOBDEVICE"));

            let private_identity = PrivateCrossSigningIdentity::new(user_id().to_owned());
            let identity = private_identity.to_public_identity().await.unwrap();

            let master_key = private_identity.master_public_key().await.unwrap();
            let master_key = master_key.get_first_key().unwrap().to_owned();

            let alice_device = ReadOnlyDevice::from_account(&alice_account);
            let bob_device = ReadOnlyDevice::from_account(&bob_account);

            let mut changes = Changes::default();
            changes.identities.new.push(identity.clone().into());
            changes.devices.new.push(bob_device.clone());
            store.save_changes(changes).await.unwrap();

            let identities = store.get_identities(alice_device.clone()).await.unwrap();

            let alice_verification = QrVerification::new_self_no_master(
                store,
                flow_id.clone(),
                master_key,
                identities,
                false,
                None,
            );
            assert_matches!(alice_verification.state(), QrVerificationState::Started);

            let bob_store = memory_store(bob_account.user_id());

            let private_identity = PrivateCrossSigningIdentity::new(user_id().to_owned());
            let bob_store = VerificationStore {
                account: bob_account.static_data.clone(),
                inner: bob_store,
                private_identity: Mutex::new(private_identity).into(),
            };

            let mut changes = Changes::default();
            changes.identities.new.push(identity.into());
            changes.devices.new.push(alice_device.clone());
            bob_store.save_changes(changes).await.unwrap();

            let qr_code = alice_verification.to_bytes().unwrap();
            let qr_code = QrVerificationData::from_bytes(qr_code).unwrap();

            let bob_verification = QrVerification::from_scan(
                bob_store,
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
            assert_matches!(bob_verification.state(), QrVerificationState::Reciprocated);

            let content = OutgoingContent::try_from(request).unwrap();
            let content = StartContent::try_from(&content).unwrap();

            alice_verification.receive_reciprocation(&content);
            assert_matches!(alice_verification.state(), QrVerificationState::Scanned);

            let request = alice_verification.confirm_scanning().unwrap();
            assert_matches!(alice_verification.state(), QrVerificationState::Confirmed);

            let content = OutgoingContent::try_from(request).unwrap();
            let content = DoneContent::try_from(&content).unwrap();

            assert!(!alice_verification.is_done());
            assert!(!bob_verification.is_done());

            let (request, _) = bob_verification.receive_done(&content).await.unwrap();
            let content = OutgoingContent::try_from(request.unwrap()).unwrap();
            let content = DoneContent::try_from(&content).unwrap();
            alice_verification.receive_done(&content).await.unwrap();

            assert_matches!(alice_verification.state(), QrVerificationState::Done { .. });
            assert_matches!(bob_verification.state(), QrVerificationState::Done { .. });
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
