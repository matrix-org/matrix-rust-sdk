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

#![allow(dead_code)]

use std::sync::{Arc, Mutex};

use matrix_qrcode::QrVerificationData;
use matrix_sdk_common::uuid::Uuid;
use ruma::{
    events::{
        key::verification::{
            cancel::CancelCode,
            ready::{ReadyEventContent, ReadyToDeviceEventContent},
            request::RequestToDeviceEventContent,
            start::StartMethod,
            Relation, VerificationMethod,
        },
        room::message::KeyVerificationRequestEventContent,
        AnyMessageEventContent, AnyToDeviceEventContent,
    },
    to_device::DeviceIdOrAllDevices,
    DeviceId, DeviceIdBox, DeviceKeyAlgorithm, EventId, MilliSecondsSinceUnixEpoch, RoomId, UserId,
};
use tracing::{info, trace, warn};

use super::{
    cache::VerificationCache,
    event_enums::{
        CancelContent, DoneContent, OutgoingContent, ReadyContent, RequestContent, StartContent,
    },
    qrcode::{QrVerification, ScanError},
    Cancelled, FlowId, IdentitiesBeingVerified,
};
use crate::{
    olm::{PrivateCrossSigningIdentity, ReadOnlyAccount},
    store::CryptoStore,
    CryptoStoreError, OutgoingVerificationRequest, ReadOnlyDevice, RoomMessageRequest, Sas,
    ToDeviceRequest, UserIdentities,
};

const SUPPORTED_METHODS: &[VerificationMethod] = &[
    VerificationMethod::SasV1,
    VerificationMethod::QrCodeShowV1,
    VerificationMethod::ReciprocateV1,
];

/// An object controlling key verification requests.
///
/// Interactive verification flows usually start with a verification request,
/// this object lets you send and reply to such a verification request.
///
/// After the initial handshake the verification flow transitions into one of
/// the verification methods.
#[derive(Clone, Debug)]
pub struct VerificationRequest {
    verification_cache: VerificationCache,
    account: ReadOnlyAccount,
    flow_id: Arc<FlowId>,
    other_user_id: Arc<UserId>,
    inner: Arc<Mutex<InnerRequest>>,
    we_started: bool,
}

impl VerificationRequest {
    pub(crate) fn new(
        cache: VerificationCache,
        account: ReadOnlyAccount,
        private_cross_signing_identity: PrivateCrossSigningIdentity,
        store: Arc<dyn CryptoStore>,
        room_id: &RoomId,
        event_id: &EventId,
        other_user: &UserId,
    ) -> Self {
        let flow_id = (room_id.to_owned(), event_id.to_owned()).into();

        let inner = Mutex::new(InnerRequest::Created(RequestState::new(
            account.clone(),
            private_cross_signing_identity,
            cache.clone(),
            store,
            other_user,
            &flow_id,
        )))
        .into();

        Self {
            account,
            verification_cache: cache,
            flow_id: flow_id.into(),
            inner,
            other_user_id: other_user.to_owned().into(),
            we_started: true,
        }
    }

    pub(crate) fn new_to_device(
        cache: VerificationCache,
        account: ReadOnlyAccount,
        private_cross_signing_identity: PrivateCrossSigningIdentity,
        store: Arc<dyn CryptoStore>,
        other_user: &UserId,
    ) -> Self {
        let flow_id = Uuid::new_v4().to_string().into();

        let inner = Mutex::new(InnerRequest::Created(RequestState::new(
            account.clone(),
            private_cross_signing_identity,
            cache.clone(),
            store,
            other_user,
            &flow_id,
        )))
        .into();

        Self {
            account,
            verification_cache: cache,
            flow_id: flow_id.into(),
            inner,
            other_user_id: other_user.to_owned().into(),
            we_started: true,
        }
    }

    /// Create an event content that can be sent as a to-device event to request
    /// verification from the other side. This should be used only for
    /// self-verifications and it should be sent to the specific device that we
    /// want to verify.
    pub fn request_to_device(&self) -> RequestToDeviceEventContent {
        RequestToDeviceEventContent::new(
            self.account.device_id().into(),
            self.flow_id().as_str().to_string(),
            SUPPORTED_METHODS.to_vec(),
            MilliSecondsSinceUnixEpoch::now(),
        )
    }

    /// Create an event content that can be sent as a room event to request
    /// verification from the other side. This should be used only for
    /// verifications of other users and it should be sent to a room we consider
    /// to be a DM with the other user.
    pub fn request(
        own_user_id: &UserId,
        own_device_id: &DeviceId,
        other_user_id: &UserId,
    ) -> KeyVerificationRequestEventContent {
        KeyVerificationRequestEventContent::new(
            format!(
                "{} is requesting to verify your key, but your client does not \
                support in-chat key verification. You will need to use legacy \
                key verification to verify keys.",
                own_user_id
            ),
            SUPPORTED_METHODS.to_vec(),
            own_device_id.into(),
            other_user_id.to_owned(),
        )
    }

    /// Our own user id.
    pub fn own_user_id(&self) -> &UserId {
        self.account.user_id()
    }

    /// The id of the other user that is participating in this verification
    /// request.
    pub fn other_user(&self) -> &UserId {
        &self.other_user_id
    }

    /// The id of the other device that is participating in this verification.
    pub fn other_device_id(&self) -> Option<DeviceIdBox> {
        match &*self.inner.lock().unwrap() {
            InnerRequest::Requested(r) => Some(r.state.other_device_id.clone()),
            InnerRequest::Ready(r) => Some(r.state.other_device_id.clone()),
            InnerRequest::Created(_)
            | InnerRequest::Passive(_)
            | InnerRequest::Done(_)
            | InnerRequest::Cancelled(_) => None,
        }
    }

    /// Get the room id if the verification is happening inside a room.
    pub fn room_id(&self) -> Option<&RoomId> {
        match self.flow_id.as_ref() {
            FlowId::ToDevice(_) => None,
            FlowId::InRoom(r, _) => Some(r),
        }
    }

    /// Get the `CancelCode` that cancelled this verification request.
    pub fn cancel_code(&self) -> Option<CancelCode> {
        match &*self.inner.lock().unwrap() {
            InnerRequest::Cancelled(c) => Some(c.state.cancel_code.to_owned()),
            _ => None,
        }
    }

    /// Has the verification request been answered by another device.
    pub fn is_passive(&self) -> bool {
        matches!(&*self.inner.lock().unwrap(), InnerRequest::Passive(_))
    }

    /// Is the verification request ready to start a verification flow.
    pub fn is_ready(&self) -> bool {
        matches!(&*self.inner.lock().unwrap(), InnerRequest::Ready(_))
    }

    /// Get the supported verification methods of the other side.
    ///
    /// Will be present only if the other side requested the verification or if
    /// we're in the ready state.
    pub fn their_supported_methods(&self) -> Option<Vec<VerificationMethod>> {
        match &*self.inner.lock().unwrap() {
            InnerRequest::Requested(r) => Some(r.state.their_methods.clone()),
            InnerRequest::Ready(r) => Some(r.state.their_methods.clone()),
            InnerRequest::Created(_)
            | InnerRequest::Passive(_)
            | InnerRequest::Done(_)
            | InnerRequest::Cancelled(_) => None,
        }
    }

    /// Get our own supported verification methods that we advertised.
    ///
    /// Will be present only we requested the verification or if we're in the
    /// ready state.
    pub fn our_supported_methods(&self) -> Option<Vec<VerificationMethod>> {
        match &*self.inner.lock().unwrap() {
            InnerRequest::Created(r) => Some(r.state.our_methods.clone()),
            InnerRequest::Ready(r) => Some(r.state.our_methods.clone()),
            InnerRequest::Requested(_)
            | InnerRequest::Passive(_)
            | InnerRequest::Done(_)
            | InnerRequest::Cancelled(_) => None,
        }
    }

    /// Get the unique ID of this verification request
    pub fn flow_id(&self) -> &FlowId {
        &self.flow_id
    }

    /// Is this a verification that is veryfying one of our own devices
    pub fn is_self_verification(&self) -> bool {
        self.account.user_id() == self.other_user()
    }

    /// Did we initiate the verification request
    pub fn we_started(&self) -> bool {
        self.we_started
    }

    /// Has the verification flow that was started with this request finished.
    pub fn is_done(&self) -> bool {
        matches!(&*self.inner.lock().unwrap(), InnerRequest::Done(_))
    }

    /// Has the verification flow that was started with this request been
    /// cancelled.
    pub fn is_cancelled(&self) -> bool {
        matches!(&*self.inner.lock().unwrap(), InnerRequest::Cancelled(_))
    }

    /// Generate a QR code that can be used by another client to start a QR code
    /// based verification.
    pub async fn generate_qr_code(&self) -> Result<Option<QrVerification>, CryptoStoreError> {
        self.inner.lock().unwrap().generate_qr_code(self.we_started).await
    }

    /// Start a QR code verification by providing a scanned QR code for this
    /// verification flow.
    ///
    /// Returns a `ScanError` if the QR code isn't valid, `None` if the
    /// verification request isn't in the ready state or we don't support QR
    /// code verification, otherwise a newly created `QrVerification` object
    /// which will be used for the remainder of the verification flow.
    pub async fn scan_qr_code(
        &self,
        data: QrVerificationData,
    ) -> Result<Option<QrVerification>, ScanError> {
        let state = self.inner.lock().unwrap();

        if let InnerRequest::Ready(r) = &*state {
            Ok(Some(
                QrVerification::from_scan(
                    r.store.clone(),
                    r.account.clone(),
                    r.private_cross_signing_identity.clone(),
                    r.other_user_id.clone(),
                    r.state.other_device_id.clone(),
                    r.flow_id.as_ref().to_owned(),
                    data,
                    self.we_started,
                )
                .await?,
            ))
        } else {
            Ok(None)
        }
    }

    pub(crate) fn from_request(
        cache: VerificationCache,
        account: ReadOnlyAccount,
        private_cross_signing_identity: PrivateCrossSigningIdentity,
        store: Arc<dyn CryptoStore>,
        sender: &UserId,
        flow_id: FlowId,
        content: &RequestContent,
    ) -> Self {
        Self {
            verification_cache: cache.clone(),
            inner: Arc::new(Mutex::new(InnerRequest::Requested(RequestState::from_request_event(
                account.clone(),
                private_cross_signing_identity,
                cache,
                store,
                sender,
                &flow_id,
                content,
            )))),
            account,
            other_user_id: sender.to_owned().into(),
            flow_id: flow_id.into(),
            we_started: false,
        }
    }

    /// Accept the verification request signaling that our client supports the
    /// given verification methods.
    ///
    /// # Arguments
    ///
    /// * `methods` - The methods that we should advertise as supported by us.
    pub fn accept_with_methods(
        &self,
        methods: Vec<VerificationMethod>,
    ) -> Option<OutgoingVerificationRequest> {
        let mut inner = self.inner.lock().unwrap();

        inner.accept(methods).map(|c| match c {
            OutgoingContent::ToDevice(content) => {
                ToDeviceRequest::new(&self.other_user(), inner.other_device_id(), content).into()
            }
            OutgoingContent::Room(room_id, content) => {
                RoomMessageRequest { room_id, txn_id: Uuid::new_v4(), content }.into()
            }
        })
    }

    /// Accept the verification request.
    ///
    /// This method will accept the request and signal that it supports the
    /// `m.sas.v1`, the `m.qr_code.show.v1`, and `m.reciprocate.v1` method.
    ///
    /// If QR code scanning should be supported or QR code showing shouldn't be
    /// supported the [`accept_with_methods()`] method should be used instead.
    ///
    /// [`accept_with_methods()`]: #method.accept_with_methods
    pub fn accept(&self) -> Option<OutgoingVerificationRequest> {
        self.accept_with_methods(SUPPORTED_METHODS.to_vec())
    }

    /// Cancel the verification request
    pub fn cancel(&self) -> Option<OutgoingVerificationRequest> {
        let mut inner = self.inner.lock().unwrap();
        inner.cancel(true, &CancelCode::User);

        let content = if let InnerRequest::Cancelled(c) = &*inner {
            Some(c.state.as_content(self.flow_id()))
        } else {
            None
        };

        content.map(|c| match c {
            OutgoingContent::ToDevice(content) => {
                ToDeviceRequest::new(&self.other_user(), inner.other_device_id(), content).into()
            }
            OutgoingContent::Room(room_id, content) => {
                RoomMessageRequest { room_id, txn_id: Uuid::new_v4(), content }.into()
            }
        })
    }

    pub(crate) fn receive_ready(&self, sender: &UserId, content: &ReadyContent) {
        let mut inner = self.inner.lock().unwrap();

        if let InnerRequest::Created(s) = &*inner {
            if sender == self.own_user_id() && content.from_device() == self.account.device_id() {
                *inner = InnerRequest::Passive(s.clone().into_passive(content))
            } else {
                *inner = InnerRequest::Ready(s.clone().into_ready(sender, content));
            }
        }
    }

    pub(crate) async fn receive_start(
        &self,
        sender: &UserId,
        content: &StartContent<'_>,
    ) -> Result<(), CryptoStoreError> {
        let inner = self.inner.lock().unwrap().clone();

        if let InnerRequest::Ready(s) = inner {
            s.receive_start(sender, content, self.we_started).await?;
        } else {
            warn!(
                sender = sender.as_str(),
                device_id = content.from_device().as_str(),
                "Received a key verification start event but we're not yet in the ready state"
            )
        }

        Ok(())
    }

    pub(crate) fn receive_done(&self, sender: &UserId, content: &DoneContent<'_>) {
        if sender == self.other_user() {
            let mut inner = self.inner.lock().unwrap().clone();
            inner.receive_done(content);
        }
    }

    pub(crate) fn receive_cancel(&self, sender: &UserId, content: &CancelContent<'_>) {
        if sender == self.other_user() {
            let mut inner = self.inner.lock().unwrap().clone();
            inner.cancel(false, content.cancel_code());
        }
    }

    /// Transition from this verification request into a SAS verification flow.
    pub async fn start_sas(
        &self,
    ) -> Result<Option<(Sas, OutgoingVerificationRequest)>, CryptoStoreError> {
        let inner = self.inner.lock().unwrap().clone();

        Ok(match &inner {
            InnerRequest::Ready(s) => {
                if let Some((sas, content)) = s
                    .clone()
                    .start_sas(
                        s.store.clone(),
                        s.account.clone(),
                        s.private_cross_signing_identity.clone(),
                        self.we_started,
                    )
                    .await?
                {
                    self.verification_cache.insert_sas(sas.clone());

                    let request = match content {
                        OutgoingContent::ToDevice(content) => ToDeviceRequest::new(
                            &self.other_user(),
                            inner.other_device_id(),
                            content,
                        )
                        .into(),
                        OutgoingContent::Room(room_id, content) => {
                            RoomMessageRequest { room_id, txn_id: Uuid::new_v4(), content }.into()
                        }
                    };

                    Some((sas, request))
                } else {
                    None
                }
            }
            _ => None,
        })
    }
}

#[derive(Clone, Debug)]
enum InnerRequest {
    Created(RequestState<Created>),
    Requested(RequestState<Requested>),
    Ready(RequestState<Ready>),
    Passive(RequestState<Passive>),
    Done(RequestState<Done>),
    Cancelled(RequestState<Cancelled>),
}

impl InnerRequest {
    fn other_device_id(&self) -> DeviceIdOrAllDevices {
        match self {
            InnerRequest::Created(_) => DeviceIdOrAllDevices::AllDevices,
            InnerRequest::Requested(_) => DeviceIdOrAllDevices::AllDevices,
            InnerRequest::Ready(r) => {
                DeviceIdOrAllDevices::DeviceId(r.state.other_device_id.to_owned())
            }
            InnerRequest::Passive(_) => DeviceIdOrAllDevices::AllDevices,
            InnerRequest::Done(_) => DeviceIdOrAllDevices::AllDevices,
            InnerRequest::Cancelled(_) => DeviceIdOrAllDevices::AllDevices,
        }
    }

    fn other_user_id(&self) -> &UserId {
        match self {
            InnerRequest::Created(s) => &s.other_user_id,
            InnerRequest::Requested(s) => &s.other_user_id,
            InnerRequest::Ready(s) => &s.other_user_id,
            InnerRequest::Passive(s) => &s.other_user_id,
            InnerRequest::Done(s) => &s.other_user_id,
            InnerRequest::Cancelled(s) => &s.other_user_id,
        }
    }

    fn accept(&mut self, methods: Vec<VerificationMethod>) -> Option<OutgoingContent> {
        if let InnerRequest::Requested(s) = self {
            let (state, content) = s.clone().accept(methods);
            *self = InnerRequest::Ready(state);

            Some(content)
        } else {
            None
        }
    }

    fn receive_done(&mut self, content: &DoneContent) {
        *self = InnerRequest::Done(match self {
            InnerRequest::Created(s) => s.clone().into_done(content),
            InnerRequest::Requested(s) => s.clone().into_done(content),
            InnerRequest::Ready(s) => s.clone().into_done(content),
            InnerRequest::Passive(s) => s.clone().into_done(content),
            InnerRequest::Done(s) => s.clone().into_done(content),
            InnerRequest::Cancelled(_) => return,
        })
    }

    fn cancel(&mut self, cancelled_by_us: bool, cancel_code: &CancelCode) {
        *self = InnerRequest::Cancelled(match self {
            InnerRequest::Created(s) => s.clone().into_canceled(cancelled_by_us, cancel_code),
            InnerRequest::Requested(s) => s.clone().into_canceled(cancelled_by_us, cancel_code),
            InnerRequest::Ready(s) => s.clone().into_canceled(cancelled_by_us, cancel_code),
            InnerRequest::Passive(s) => s.clone().into_canceled(cancelled_by_us, cancel_code),
            InnerRequest::Done(_) => return,
            InnerRequest::Cancelled(_) => return,
        })
    }

    async fn generate_qr_code(
        &self,
        we_started: bool,
    ) -> Result<Option<QrVerification>, CryptoStoreError> {
        match self {
            InnerRequest::Created(_) => Ok(None),
            InnerRequest::Requested(_) => Ok(None),
            InnerRequest::Ready(s) => s.generate_qr_code(we_started).await,
            InnerRequest::Passive(_) => Ok(None),
            InnerRequest::Done(_) => Ok(None),
            InnerRequest::Cancelled(_) => Ok(None),
        }
    }

    fn to_started_sas(
        &self,
        content: &StartContent,
        other_device: ReadOnlyDevice,
        other_identity: Option<UserIdentities>,
        we_started: bool,
    ) -> Result<Option<Sas>, OutgoingContent> {
        if let InnerRequest::Ready(s) = self {
            Ok(Some(s.to_started_sas(content, other_device, other_identity, we_started)?))
        } else {
            Ok(None)
        }
    }
}

#[derive(Clone, Debug)]
struct RequestState<S: Clone> {
    account: ReadOnlyAccount,
    private_cross_signing_identity: PrivateCrossSigningIdentity,
    verification_cache: VerificationCache,
    store: Arc<dyn CryptoStore>,
    flow_id: Arc<FlowId>,

    /// The id of the user which is participating in this verification request.
    pub other_user_id: UserId,

    /// The verification request state we are in.
    state: S,
}

impl<S: Clone> RequestState<S> {
    fn into_done(self, _: &DoneContent) -> RequestState<Done> {
        RequestState::<Done> {
            account: self.account,
            private_cross_signing_identity: self.private_cross_signing_identity,
            verification_cache: self.verification_cache,
            store: self.store,
            flow_id: self.flow_id,
            other_user_id: self.other_user_id,
            state: Done {},
        }
    }

    fn into_canceled(
        self,
        cancelled_by_us: bool,
        cancel_code: &CancelCode,
    ) -> RequestState<Cancelled> {
        RequestState::<Cancelled> {
            account: self.account,
            private_cross_signing_identity: self.private_cross_signing_identity,
            verification_cache: self.verification_cache,
            store: self.store,
            flow_id: self.flow_id,
            other_user_id: self.other_user_id,
            state: Cancelled::new(cancelled_by_us, cancel_code.clone()),
        }
    }
}

impl RequestState<Created> {
    fn new(
        account: ReadOnlyAccount,
        private_identity: PrivateCrossSigningIdentity,
        cache: VerificationCache,
        store: Arc<dyn CryptoStore>,
        other_user_id: &UserId,
        flow_id: &FlowId,
    ) -> Self {
        Self {
            account,
            other_user_id: other_user_id.to_owned(),
            private_cross_signing_identity: private_identity,
            state: Created { our_methods: SUPPORTED_METHODS.to_vec() },
            verification_cache: cache,
            store,
            flow_id: flow_id.to_owned().into(),
        }
    }

    fn into_passive(self, content: &ReadyContent) -> RequestState<Passive> {
        RequestState {
            account: self.account,
            flow_id: self.flow_id,
            verification_cache: self.verification_cache,
            private_cross_signing_identity: self.private_cross_signing_identity,
            store: self.store,
            other_user_id: self.other_user_id,
            state: Passive { other_device_id: content.from_device().to_owned() },
        }
    }

    fn into_ready(self, _sender: &UserId, content: &ReadyContent) -> RequestState<Ready> {
        // TODO check the flow id, and that the methods match what we suggested.
        RequestState {
            account: self.account,
            flow_id: self.flow_id,
            verification_cache: self.verification_cache,
            private_cross_signing_identity: self.private_cross_signing_identity,
            store: self.store,
            other_user_id: self.other_user_id,
            state: Ready {
                their_methods: content.methods().to_owned(),
                our_methods: self.state.our_methods,
                other_device_id: content.from_device().into(),
            },
        }
    }
}

#[derive(Clone, Debug)]
struct Created {
    /// The verification methods supported by us.
    pub our_methods: Vec<VerificationMethod>,
}

#[derive(Clone, Debug)]
struct Requested {
    /// The verification methods supported by the sender.
    pub their_methods: Vec<VerificationMethod>,

    /// The device id of the device that responded to the verification request.
    pub other_device_id: DeviceIdBox,
}

impl RequestState<Requested> {
    fn from_request_event(
        account: ReadOnlyAccount,
        private_identity: PrivateCrossSigningIdentity,
        cache: VerificationCache,
        store: Arc<dyn CryptoStore>,
        sender: &UserId,
        flow_id: &FlowId,
        content: &RequestContent,
    ) -> RequestState<Requested> {
        // TODO only create this if we support the methods
        RequestState {
            account,
            private_cross_signing_identity: private_identity,
            store,
            verification_cache: cache,
            flow_id: flow_id.to_owned().into(),
            other_user_id: sender.clone(),
            state: Requested {
                their_methods: content.methods().to_owned(),
                other_device_id: content.from_device().into(),
            },
        }
    }

    fn accept(self, methods: Vec<VerificationMethod>) -> (RequestState<Ready>, OutgoingContent) {
        let state = RequestState {
            account: self.account.clone(),
            store: self.store,
            verification_cache: self.verification_cache,
            private_cross_signing_identity: self.private_cross_signing_identity,
            flow_id: self.flow_id.clone(),
            other_user_id: self.other_user_id,
            state: Ready {
                their_methods: self.state.their_methods,
                our_methods: methods.clone(),
                other_device_id: self.state.other_device_id.clone(),
            },
        };

        let content = match self.flow_id.as_ref() {
            FlowId::ToDevice(i) => {
                AnyToDeviceEventContent::KeyVerificationReady(ReadyToDeviceEventContent::new(
                    self.account.device_id().to_owned(),
                    methods,
                    i.to_owned(),
                ))
                .into()
            }
            FlowId::InRoom(r, e) => (
                r.to_owned(),
                AnyMessageEventContent::KeyVerificationReady(ReadyEventContent::new(
                    self.account.device_id().to_owned(),
                    methods,
                    Relation::new(e.to_owned()),
                )),
            )
                .into(),
        };

        (state, content)
    }
}

#[derive(Clone, Debug)]
struct Ready {
    /// The verification methods supported by the other side.
    pub their_methods: Vec<VerificationMethod>,

    /// The verification methods supported by the us.
    pub our_methods: Vec<VerificationMethod>,

    /// The device id of the device that responded to the verification request.
    pub other_device_id: DeviceIdBox,
}

impl RequestState<Ready> {
    fn to_started_sas<'a>(
        &self,
        content: &StartContent<'a>,
        other_device: ReadOnlyDevice,
        other_identity: Option<UserIdentities>,
        we_started: bool,
    ) -> Result<Sas, OutgoingContent> {
        Sas::from_start_event(
            (&*self.flow_id).to_owned(),
            content,
            self.store.clone(),
            self.account.clone(),
            self.private_cross_signing_identity.clone(),
            other_device,
            other_identity,
            true,
            we_started,
        )
    }

    async fn generate_qr_code(
        &self,
        we_started: bool,
    ) -> Result<Option<QrVerification>, CryptoStoreError> {
        // TODO return an error explaining why we can't generate a QR code?

        // If we didn't state that we support showing QR codes or if the other
        // side doesn't support scanning QR codes bail early.
        if !self.state.our_methods.contains(&VerificationMethod::QrCodeShowV1)
            || !self.state.their_methods.contains(&VerificationMethod::QrCodeScanV1)
        {
            return Ok(None);
        }

        let device = if let Some(device) =
            self.store.get_device(&self.other_user_id, &self.state.other_device_id).await?
        {
            device
        } else {
            warn!(
                user_id = self.other_user_id.as_str(),
                device_id = self.state.other_device_id.as_str(),
                "Can't create a QR code, the device that accepted the \
                 verification doesn't exist"
            );
            return Ok(None);
        };

        let identites = IdentitiesBeingVerified {
            private_identity: self.private_cross_signing_identity.clone(),
            store: self.store.clone(),
            device_being_verified: device,
            identity_being_verified: self.store.get_user_identity(&self.other_user_id).await?,
        };

        let verification = if let Some(identity) = &identites.identity_being_verified {
            match &identity {
                UserIdentities::Own(i) => {
                    if identites.can_sign_devices().await {
                        Some(QrVerification::new_self(
                            self.store.clone(),
                            self.flow_id.as_ref().to_owned(),
                            i.master_key().get_first_key().unwrap().to_owned(),
                            identites
                                .other_device()
                                .get_key(DeviceKeyAlgorithm::Ed25519)
                                .unwrap()
                                .to_owned(),
                            identites,
                            we_started,
                        ))
                    } else {
                        Some(QrVerification::new_self_no_master(
                            self.account.clone(),
                            self.store.clone(),
                            self.flow_id.as_ref().to_owned(),
                            i.master_key().get_first_key().unwrap().to_owned(),
                            identites,
                            we_started,
                        ))
                    }
                }
                UserIdentities::Other(i) => Some(QrVerification::new_cross(
                    self.store.clone(),
                    self.flow_id.as_ref().to_owned(),
                    self.private_cross_signing_identity
                        .master_public_key()
                        .await
                        .unwrap()
                        .get_first_key()
                        .unwrap()
                        .to_owned(),
                    i.master_key().get_first_key().unwrap().to_owned(),
                    identites,
                    we_started,
                )),
            }
        } else {
            warn!(
                user_id = self.other_user_id.as_str(),
                device_id = self.state.other_device_id.as_str(),
                "Can't create a QR code, the user doesn't have a valid cross \
                 signing identity."
            );

            None
        };

        if let Some(verification) = &verification {
            self.verification_cache.insert_qr(verification.clone());
        }

        Ok(verification)
    }

    async fn receive_start(
        &self,
        sender: &UserId,
        content: &StartContent<'_>,
        we_started: bool,
    ) -> Result<(), CryptoStoreError> {
        info!(
            sender = sender.as_str(),
            device = content.from_device().as_str(),
            "Received a new verification start event",
        );

        let device = if let Some(d) = self.store.get_device(sender, content.from_device()).await? {
            d
        } else {
            warn!(
                sender = sender.as_str(),
                device = content.from_device().as_str(),
                "Received a key verification start event from an unknown device",
            );

            return Ok(());
        };

        let identity = self.store.get_user_identity(sender).await?;

        match content.method() {
            StartMethod::SasV1(_) => {
                match self.to_started_sas(content, device.clone(), identity, we_started) {
                    // TODO check if there is already a SAS verification, i.e. we
                    // already started one before the other side tried to do the
                    // same; ignore it if we did and we're the lexicographically
                    // smaller user ID, otherwise auto-accept the newly started one.
                    Ok(s) => {
                        info!("Started a new SAS verification.");
                        self.verification_cache.insert_sas(s);
                    }
                    Err(c) => {
                        warn!(
                            user_id = device.user_id().as_str(),
                            device_id = device.device_id().as_str(),
                            content =? c,
                            "Can't start key verification, canceling.",
                        );
                        self.verification_cache.queue_up_content(
                            device.user_id(),
                            device.device_id(),
                            c,
                        )
                    }
                }
            }
            StartMethod::ReciprocateV1(_) => {
                if let Some(qr_verification) =
                    self.verification_cache.get_qr(sender, content.flow_id())
                {
                    if let Some(request) = qr_verification.receive_reciprocation(content) {
                        self.verification_cache.add_request(request.into())
                    }
                    trace!(
                        sender = device.user_id().as_str(),
                        device_id = device.device_id().as_str(),
                        verification =? qr_verification,
                        "Received a QR code reciprocation"
                    )
                }
            }
            m => {
                warn!(method =? m, "Received a key verification start event with an unsupported method")
            }
        }

        Ok(())
    }

    async fn start_sas(
        self,
        store: Arc<dyn CryptoStore>,
        account: ReadOnlyAccount,
        private_identity: PrivateCrossSigningIdentity,
        we_started: bool,
    ) -> Result<Option<(Sas, OutgoingContent)>, CryptoStoreError> {
        if !self.state.their_methods.contains(&VerificationMethod::SasV1) {
            return Ok(None);
        }

        // TODO signal why starting the sas flow doesn't work?
        let other_identity = store.get_user_identity(&self.other_user_id).await?;

        let device = if let Some(device) =
            self.store.get_device(&self.other_user_id, &self.state.other_device_id).await?
        {
            device
        } else {
            warn!(
                user_id = self.other_user_id.as_str(),
                device_id = self.state.other_device_id.as_str(),
                "Can't start the SAS verificaiton flow, the device that \
                accepted the verification doesn't exist"
            );
            return Ok(None);
        };

        Ok(Some(match self.flow_id.as_ref() {
            FlowId::ToDevice(t) => {
                let (sas, content) = Sas::start(
                    account,
                    private_identity,
                    device,
                    store,
                    other_identity,
                    Some(t.to_owned()),
                    we_started,
                );
                (sas, content)
            }
            FlowId::InRoom(r, e) => {
                let (sas, content) = Sas::start_in_room(
                    e.to_owned(),
                    r.to_owned(),
                    account,
                    private_identity,
                    device,
                    store,
                    other_identity,
                    we_started,
                );
                (sas, content)
            }
        }))
    }
}

#[derive(Clone, Debug)]
struct Passive {
    /// The device id of the device that responded to the verification request.
    pub other_device_id: DeviceIdBox,
}

#[derive(Clone, Debug)]
struct Done {}

#[cfg(test)]
mod test {
    use std::convert::TryFrom;

    use matrix_sdk_test::async_test;
    use ruma::{event_id, room_id, DeviceIdBox, UserId};

    use super::VerificationRequest;
    use crate::{
        olm::{PrivateCrossSigningIdentity, ReadOnlyAccount},
        store::{Changes, CryptoStore, MemoryStore},
        verification::{
            cache::VerificationCache,
            event_enums::{OutgoingContent, ReadyContent, StartContent},
            FlowId,
        },
        ReadOnlyDevice,
    };

    fn alice_id() -> UserId {
        UserId::try_from("@alice:example.org").unwrap()
    }

    fn alice_device_id() -> DeviceIdBox {
        "JLAFKJWSCS".into()
    }

    fn bob_id() -> UserId {
        UserId::try_from("@bob:example.org").unwrap()
    }

    fn bob_device_id() -> DeviceIdBox {
        "BOBDEVCIE".into()
    }

    #[async_test]
    async fn test_request_accepting() {
        let event_id = event_id!("$1234localhost");
        let room_id = room_id!("!test:localhost");

        let alice = ReadOnlyAccount::new(&alice_id(), &alice_device_id());
        let alice_store: Box<dyn CryptoStore> = Box::new(MemoryStore::new());
        let alice_identity = PrivateCrossSigningIdentity::empty(alice_id());

        let bob = ReadOnlyAccount::new(&bob_id(), &bob_device_id());
        let bob_store: Box<dyn CryptoStore> = Box::new(MemoryStore::new());
        let bob_identity = PrivateCrossSigningIdentity::empty(alice_id());

        let content = VerificationRequest::request(bob.user_id(), bob.device_id(), &alice_id());

        let bob_request = VerificationRequest::new(
            VerificationCache::new(),
            bob,
            bob_identity,
            bob_store.into(),
            &room_id,
            &event_id,
            &alice_id(),
        );

        let flow_id = FlowId::from((room_id, event_id));

        let alice_request = VerificationRequest::from_request(
            VerificationCache::new(),
            alice,
            alice_identity,
            alice_store.into(),
            &bob_id(),
            flow_id,
            &(&content).into(),
        );

        let content: OutgoingContent = alice_request.accept().unwrap().into();
        let content = ReadyContent::try_from(&content).unwrap();

        bob_request.receive_ready(&alice_id(), &content);

        assert!(bob_request.is_ready());
        assert!(alice_request.is_ready());
    }

    #[async_test]
    async fn test_requesting_until_sas() {
        let event_id = event_id!("$1234localhost");
        let room_id = room_id!("!test:localhost");

        let alice = ReadOnlyAccount::new(&alice_id(), &alice_device_id());
        let alice_device = ReadOnlyDevice::from_account(&alice).await;

        let alice_store: Box<dyn CryptoStore> = Box::new(MemoryStore::new());
        let alice_identity = PrivateCrossSigningIdentity::empty(alice_id());

        let bob = ReadOnlyAccount::new(&bob_id(), &bob_device_id());
        let bob_device = ReadOnlyDevice::from_account(&bob).await;
        let bob_store: Box<dyn CryptoStore> = Box::new(MemoryStore::new());
        let bob_identity = PrivateCrossSigningIdentity::empty(alice_id());

        let mut changes = Changes::default();
        changes.devices.new.push(bob_device.clone());
        alice_store.save_changes(changes).await.unwrap();

        let mut changes = Changes::default();
        changes.devices.new.push(alice_device.clone());
        bob_store.save_changes(changes).await.unwrap();

        let content = VerificationRequest::request(bob.user_id(), bob.device_id(), &alice_id());

        let bob_request = VerificationRequest::new(
            VerificationCache::new(),
            bob,
            bob_identity,
            bob_store.into(),
            &room_id,
            &event_id,
            &alice_id(),
        );

        let flow_id = FlowId::from((room_id, event_id));

        let alice_request = VerificationRequest::from_request(
            VerificationCache::new(),
            alice,
            alice_identity,
            alice_store.into(),
            &bob_id(),
            flow_id,
            &(&content).into(),
        );

        let content: OutgoingContent = alice_request.accept().unwrap().into();
        let content = ReadyContent::try_from(&content).unwrap();

        bob_request.receive_ready(&alice_id(), &content);

        assert!(bob_request.is_ready());
        assert!(alice_request.is_ready());

        let (bob_sas, request) = bob_request.start_sas().await.unwrap().unwrap();

        let content: OutgoingContent = request.into();
        let content = StartContent::try_from(&content).unwrap();
        let flow_id = content.flow_id().to_owned();
        alice_request.receive_start(bob_device.user_id(), &content).await.unwrap();
        let alice_sas =
            alice_request.verification_cache.get_sas(bob_device.user_id(), &flow_id).unwrap();

        assert!(!bob_sas.is_cancelled());
        assert!(!alice_sas.is_cancelled());
    }

    #[async_test]
    async fn test_requesting_until_sas_to_device() {
        let alice = ReadOnlyAccount::new(&alice_id(), &alice_device_id());
        let alice_device = ReadOnlyDevice::from_account(&alice).await;

        let alice_store: Box<dyn CryptoStore> = Box::new(MemoryStore::new());
        let alice_identity = PrivateCrossSigningIdentity::empty(alice_id());

        let bob = ReadOnlyAccount::new(&bob_id(), &bob_device_id());
        let bob_device = ReadOnlyDevice::from_account(&bob).await;
        let bob_store: Box<dyn CryptoStore> = Box::new(MemoryStore::new());
        let bob_identity = PrivateCrossSigningIdentity::empty(alice_id());

        let mut changes = Changes::default();
        changes.devices.new.push(bob_device.clone());
        alice_store.save_changes(changes).await.unwrap();

        let mut changes = Changes::default();
        changes.devices.new.push(alice_device.clone());
        bob_store.save_changes(changes).await.unwrap();

        let bob_request = VerificationRequest::new_to_device(
            VerificationCache::new(),
            bob,
            bob_identity,
            bob_store.into(),
            &alice_id(),
        );

        let content = bob_request.request_to_device();
        let flow_id = bob_request.flow_id().to_owned();

        let alice_request = VerificationRequest::from_request(
            VerificationCache::new(),
            alice,
            alice_identity,
            alice_store.into(),
            &bob_id(),
            flow_id,
            &(&content).into(),
        );

        let content: OutgoingContent = alice_request.accept().unwrap().into();
        let content = ReadyContent::try_from(&content).unwrap();

        bob_request.receive_ready(&alice_id(), &content);

        assert!(bob_request.is_ready());
        assert!(alice_request.is_ready());

        let (bob_sas, request) = bob_request.start_sas().await.unwrap().unwrap();

        let content: OutgoingContent = request.into();
        let content = StartContent::try_from(&content).unwrap();
        let flow_id = content.flow_id().to_owned();
        alice_request.receive_start(bob_device.user_id(), &content).await.unwrap();
        let alice_sas =
            alice_request.verification_cache.get_sas(bob_device.user_id(), &flow_id).unwrap();

        assert!(!bob_sas.is_cancelled());
        assert!(!alice_sas.is_cancelled());
    }
}
