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

use std::{sync::Arc, time::Duration};

use eyeball::shared::{Observable as SharedObservable, ObservableWriteGuard};
use futures_core::Stream;
use futures_util::StreamExt;
use matrix_sdk_common::instant::Instant;
#[cfg(feature = "qrcode")]
use matrix_sdk_qrcode::QrVerificationData;
use ruma::{
    events::{
        key::verification::{
            cancel::CancelCode,
            ready::{KeyVerificationReadyEventContent, ToDeviceKeyVerificationReadyEventContent},
            request::ToDeviceKeyVerificationRequestEventContent,
            start::StartMethod,
            VerificationMethod,
        },
        relation::Reference,
        room::message::KeyVerificationRequestEventContent,
        AnyMessageLikeEventContent, AnyToDeviceEventContent,
    },
    to_device::DeviceIdOrAllDevices,
    DeviceId, MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedUserId, RoomId, TransactionId,
    UserId,
};
#[cfg(feature = "qrcode")]
use tracing::debug;
use tracing::{info, trace, warn};

#[cfg(feature = "qrcode")]
use super::qrcode::{QrVerification, ScanError};
use super::{
    cache::VerificationCache,
    event_enums::{
        CancelContent, DoneContent, OutgoingContent, ReadyContent, RequestContent, StartContent,
    },
    CancelInfo, Cancelled, FlowId, IdentitiesBeingVerified, Verification, VerificationStore,
};
use crate::{
    olm::ReadOnlyAccount, CryptoStoreError, OutgoingVerificationRequest, RoomMessageRequest, Sas,
    ToDeviceRequest,
};

const SUPPORTED_METHODS: &[VerificationMethod] = &[
    VerificationMethod::SasV1,
    #[cfg(feature = "qrcode")]
    VerificationMethod::QrCodeShowV1,
    VerificationMethod::ReciprocateV1,
];

const VERIFICATION_TIMEOUT: Duration = Duration::from_secs(60 * 10);

/// An Enum describing the state the verification request is in.
#[derive(Debug, Clone)]
pub enum VerificationRequestState {
    /// The verification request has been newly created by us.
    Created {
        /// The verification methods supported by us.
        our_methods: Vec<VerificationMethod>,
    },
    /// The verification request was received from the other party.
    Requested {
        /// The verification methods supported by the sender.
        their_methods: Vec<VerificationMethod>,

        /// The device ID of the device that responded to the verification
        /// request.
        other_device_id: OwnedDeviceId,
    },
    /// The verification request is ready to start a verification flow.
    Ready {
        /// The verification methods supported by the other side.
        their_methods: Vec<VerificationMethod>,

        /// The verification methods supported by the us.
        our_methods: Vec<VerificationMethod>,

        /// The device ID of the device that responded to the verification
        /// request.
        other_device_id: OwnedDeviceId,
    },
    /// The verification flow that was started with this request has finished.
    Done,
    /// The verification process has been cancelled.
    Cancelled(CancelInfo),
}

impl From<&InnerRequest> for VerificationRequestState {
    fn from(value: &InnerRequest) -> Self {
        match value {
            InnerRequest::Created(s) => {
                Self::Created { our_methods: s.state.our_methods.to_owned() }
            }
            InnerRequest::Requested(s) => Self::Requested {
                their_methods: s.state.their_methods.to_owned(),
                other_device_id: s.state.other_device_id.to_owned(),
            },
            InnerRequest::Ready(s) => Self::Ready {
                their_methods: s.state.their_methods.to_owned(),
                our_methods: s.state.our_methods.to_owned(),
                other_device_id: s.state.other_device_id.to_owned(),
            },
            InnerRequest::Passive(_) => {
                Self::Cancelled(Cancelled::new(true, CancelCode::Accepted).into())
            }
            InnerRequest::Done(_) => Self::Done,
            InnerRequest::Cancelled(s) => Self::Cancelled(s.state.to_owned().into()),
        }
    }
}

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
    inner: SharedObservable<InnerRequest>,
    creation_time: Arc<Instant>,
    we_started: bool,
    recipient_devices: Arc<Vec<OwnedDeviceId>>,
}

/// A handle to a request so child verification flows can cancel the request.
///
/// A verification flow can branch off into different types of verification
/// flows after the initial request handshake is done.
///
/// Cancelling a QR code verification should also cancel the request. This
/// `RequestHandle` allows the QR code verification object to cancel the parent
/// `VerificationRequest` object.
#[derive(Clone, Debug)]
pub(crate) struct RequestHandle {
    inner: SharedObservable<InnerRequest>,
}

impl RequestHandle {
    pub fn cancel_with_code(&self, cancel_code: &CancelCode) {
        let mut guard = self.inner.write();
        if let Some(updated) = guard.cancel(true, cancel_code) {
            ObservableWriteGuard::set(&mut guard, updated);
        }
    }
}

impl From<SharedObservable<InnerRequest>> for RequestHandle {
    fn from(inner: SharedObservable<InnerRequest>) -> Self {
        Self { inner }
    }
}

impl VerificationRequest {
    pub(crate) fn new(
        cache: VerificationCache,
        store: VerificationStore,
        flow_id: FlowId,
        other_user: &UserId,
        recipient_devices: Vec<OwnedDeviceId>,
        methods: Option<Vec<VerificationMethod>>,
    ) -> Self {
        let account = store.account.clone();
        let inner = SharedObservable::new(InnerRequest::Created(RequestState::new(
            cache.clone(),
            store,
            other_user,
            &flow_id,
            methods,
        )));

        Self {
            account,
            verification_cache: cache,
            flow_id: flow_id.into(),
            inner,
            other_user_id: other_user.into(),
            creation_time: Instant::now().into(),
            we_started: true,
            recipient_devices: recipient_devices.into(),
        }
    }

    /// Create an event content that can be sent as a to-device event to request
    /// verification from the other side. This should be used only for
    /// self-verifications and it should be sent to the specific device that we
    /// want to verify.
    pub(crate) fn request_to_device(&self) -> ToDeviceRequest {
        let inner = self.inner.read();

        let methods = if let InnerRequest::Created(c) = &*inner {
            c.state.our_methods.clone()
        } else {
            SUPPORTED_METHODS.to_vec()
        };

        let content = ToDeviceKeyVerificationRequestEventContent::new(
            self.account.device_id().into(),
            self.flow_id().as_str().into(),
            methods,
            MilliSecondsSinceUnixEpoch::now(),
        );

        ToDeviceRequest::for_recipients(
            self.other_user(),
            self.recipient_devices.to_vec(),
            AnyToDeviceEventContent::KeyVerificationRequest(content),
            TransactionId::new(),
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
        methods: Option<Vec<VerificationMethod>>,
    ) -> KeyVerificationRequestEventContent {
        KeyVerificationRequestEventContent::new(
            format!(
                "{own_user_id} is requesting to verify your key, but your client does not \
                support in-chat key verification. You will need to use legacy \
                key verification to verify keys."
            ),
            methods.unwrap_or_else(|| SUPPORTED_METHODS.to_vec()),
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
    pub fn other_device_id(&self) -> Option<OwnedDeviceId> {
        match &*self.inner.read() {
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

    /// Get info about the cancellation if the verification request has been
    /// cancelled.
    pub fn cancel_info(&self) -> Option<CancelInfo> {
        if let InnerRequest::Cancelled(c) = &*self.inner.read() {
            Some(c.state.clone().into())
        } else {
            None
        }
    }

    /// Has the verification request been answered by another device.
    pub fn is_passive(&self) -> bool {
        matches!(*self.inner.read(), InnerRequest::Passive(_))
    }

    /// Is the verification request ready to start a verification flow.
    pub fn is_ready(&self) -> bool {
        matches!(*self.inner.read(), InnerRequest::Ready(_))
    }

    /// Has the verification flow timed out.
    pub fn timed_out(&self) -> bool {
        self.creation_time.elapsed() > VERIFICATION_TIMEOUT
    }

    /// Get the supported verification methods of the other side.
    ///
    /// Will be present only if the other side requested the verification or if
    /// we're in the ready state.
    pub fn their_supported_methods(&self) -> Option<Vec<VerificationMethod>> {
        match &*self.inner.read() {
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
        match &*self.inner.read() {
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
        matches!(*self.inner.read(), InnerRequest::Done(_))
    }

    /// Has the verification flow that was started with this request been
    /// cancelled.
    pub fn is_cancelled(&self) -> bool {
        matches!(*self.inner.read(), InnerRequest::Cancelled(_))
    }

    /// Generate a QR code that can be used by another client to start a QR code
    /// based verification.
    #[cfg(feature = "qrcode")]
    pub async fn generate_qr_code(&self) -> Result<Option<QrVerification>, CryptoStoreError> {
        let inner = self.inner.get();
        inner.generate_qr_code(self.we_started, self.inner.clone().into()).await
    }

    /// Start a QR code verification by providing a scanned QR code for this
    /// verification flow.
    ///
    /// Returns a `ScanError` if the QR code isn't valid, `None` if the
    /// verification request isn't in the ready state or we don't support QR
    /// code verification, otherwise a newly created `QrVerification` object
    /// which will be used for the remainder of the verification flow.
    #[cfg(feature = "qrcode")]
    pub async fn scan_qr_code(
        &self,
        data: QrVerificationData,
    ) -> Result<Option<QrVerification>, ScanError> {
        let future = if let InnerRequest::Ready(r) = &*self.inner.read() {
            QrVerification::from_scan(
                r.store.clone(),
                r.other_user_id.clone(),
                r.state.other_device_id.clone(),
                r.flow_id.as_ref().to_owned(),
                data,
                self.we_started,
                Some(self.inner.clone().into()),
            )
        } else {
            return Ok(None);
        };

        // await future after self.inner read guard is released
        let qr_verification = future.await?;

        // We may have previously started our own QR verification (e.g. two devices
        // displaying QR code at the same time), so we need to replace it with the newly
        // scanned code.
        if self
            .verification_cache
            .get_qr(qr_verification.other_user_id(), qr_verification.flow_id().as_str())
            .is_some()
        {
            debug!(
                user_id = ?self.other_user(),
                flow_id = self.flow_id().as_str(),
                "Replacing existing QR verification"
            );
            self.verification_cache.replace_qr(qr_verification.clone());
        } else {
            debug!(
                user_id = ?self.other_user(),
                flow_id = self.flow_id().as_str(),
                "Inserting new QR verification"
            );
            self.verification_cache.insert_qr(qr_verification.clone());
        }

        Ok(Some(qr_verification))
    }

    pub(crate) fn from_request(
        cache: VerificationCache,
        store: VerificationStore,
        sender: &UserId,
        flow_id: FlowId,
        content: &RequestContent<'_>,
    ) -> Self {
        let account = store.account.clone();

        Self {
            verification_cache: cache.clone(),
            inner: SharedObservable::new(InnerRequest::Requested(
                RequestState::from_request_event(cache, store, sender, &flow_id, content),
            )),
            account,
            other_user_id: sender.into(),
            flow_id: flow_id.into(),
            we_started: false,
            creation_time: Instant::now().into(),
            recipient_devices: vec![].into(),
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
        let mut guard = self.inner.write();

        let Some((updated, content)) = guard.accept(methods) else {
            return None;
        };

        ObservableWriteGuard::set(&mut guard, updated);

        let request = match content {
            OutgoingContent::ToDevice(content) => ToDeviceRequest::with_id(
                self.other_user(),
                guard.other_device_id(),
                content,
                TransactionId::new(),
            )
            .into(),
            OutgoingContent::Room(room_id, content) => {
                RoomMessageRequest { room_id, txn_id: TransactionId::new(), content }.into()
            }
        };

        Some(request)
    }

    /// Accept the verification request.
    ///
    /// This method will accept the request and signal that it supports the
    /// `m.sas.v1`, the `m.qr_code.show.v1`, and `m.reciprocate.v1` method.
    ///
    /// `m.qr_code.show.v1` will only be signaled if the `qrcode` feature is
    /// enabled. This feature is disabled by default. If it's enabled and QR
    /// code scanning should be supported or QR code showing shouldn't be
    /// supported the [`accept_with_methods()`] method should be used
    /// instead.
    ///
    /// [`accept_with_methods()`]: #method.accept_with_methods
    pub fn accept(&self) -> Option<OutgoingVerificationRequest> {
        self.accept_with_methods(SUPPORTED_METHODS.to_vec())
    }

    /// Cancel the verification request
    pub fn cancel(&self) -> Option<OutgoingVerificationRequest> {
        self.cancel_with_code(CancelCode::User)
    }

    fn cancel_with_code(&self, cancel_code: CancelCode) -> Option<OutgoingVerificationRequest> {
        let mut guard = self.inner.write();

        let send_to_everyone = self.we_started() && matches!(*guard, InnerRequest::Created(_));
        let other_device = guard.other_device_id();

        if let Some(updated) = guard.cancel(true, &cancel_code) {
            ObservableWriteGuard::set(&mut guard, updated);
        }

        let content = if let InnerRequest::Cancelled(c) = &*guard {
            Some(c.state.as_content(self.flow_id()))
        } else {
            None
        };

        let request = content.map(|c| match c {
            OutgoingContent::ToDevice(content) => {
                if send_to_everyone {
                    ToDeviceRequest::for_recipients(
                        self.other_user(),
                        self.recipient_devices.to_vec(),
                        content,
                        TransactionId::new(),
                    )
                    .into()
                } else {
                    ToDeviceRequest::with_id(
                        self.other_user(),
                        other_device,
                        content,
                        TransactionId::new(),
                    )
                    .into()
                }
            }
            OutgoingContent::Room(room_id, content) => {
                RoomMessageRequest { room_id, txn_id: TransactionId::new(), content }.into()
            }
        });

        drop(guard);

        if let Some(verification) =
            self.verification_cache.get(self.other_user(), self.flow_id().as_str())
        {
            match verification {
                crate::Verification::SasV1(s) => s.cancel_with_code(cancel_code),
                #[cfg(feature = "qrcode")]
                crate::Verification::QrV1(q) => q.cancel_with_code(cancel_code),
            };
        }

        request
    }

    pub(crate) fn cancel_if_timed_out(&self) -> Option<OutgoingVerificationRequest> {
        if self.is_cancelled() || self.is_done() {
            None
        } else if self.timed_out() {
            let request = self.cancel_with_code(CancelCode::Timeout);

            if self.is_passive() {
                None
            } else {
                trace!(
                    other_user = self.other_user().as_str(),
                    flow_id = self.flow_id().as_str(),
                    "Timing a verification request out"
                );
                request
            }
        } else {
            None
        }
    }

    /// Create a key verification cancellation for devices that received the
    /// request but either shouldn't continue in the verification or didn't get
    /// notified that the other side cancelled.
    ///
    /// The spec states the following[1]:
    /// When Bob accepts or declines the verification on one of his devices
    /// (sending either an m.key.verification.ready or m.key.verification.cancel
    /// event), Alice will send an m.key.verification.cancel event to Bob’s
    /// other devices with a code of m.accepted in the case where Bob accepted
    /// the verification, or m.user in the case where Bob rejected the
    /// verification.
    ///
    /// Realistically sending the cancellation to Bob's other devices is only
    /// possible if Bob accepted the verification since we don't know the device
    /// id of Bob's device that rejected the verification.
    ///
    /// Thus, we're sending the cancellation to all devices that received the
    /// request in the rejection case.
    ///
    /// [1]: https://spec.matrix.org/unstable/client-server-api/#key-verification-framework
    pub(crate) fn cancel_for_other_devices(
        &self,
        code: CancelCode,
        filter_device: Option<&DeviceId>,
    ) -> Option<ToDeviceRequest> {
        let cancelled = Cancelled::new(true, code);
        let cancel_content = cancelled.as_content(self.flow_id());

        let OutgoingContent::ToDevice(c) = cancel_content else { return None };
        let recip_devices: Vec<OwnedDeviceId> = self
            .recipient_devices
            .iter()
            .filter(|&d| filter_device.map_or(true, |device| **d != *device))
            .cloned()
            .collect();

        if recip_devices.is_empty() && filter_device.is_some() {
            // We don't need to notify anyone if no recipients were present but
            // we did have a filter device, since this means that only a single
            // device received the `m.key.verification.request` and that device
            // accepted the request.
            return None;
        }

        let recipient = self.other_user();
        Some(ToDeviceRequest::for_recipients(recipient, recip_devices, c, TransactionId::new()))
    }

    pub(crate) fn receive_ready(&self, sender: &UserId, content: &ReadyContent<'_>) {
        let mut guard = self.inner.write();

        match &*guard {
            InnerRequest::Created(s) => {
                let new_value = InnerRequest::Ready(s.clone().into_ready(sender, content));
                ObservableWriteGuard::set(&mut guard, new_value);

                if let Some(request) =
                    self.cancel_for_other_devices(CancelCode::Accepted, Some(content.from_device()))
                {
                    self.verification_cache.add_verification_request(request.into());
                }
            }
            InnerRequest::Requested(s) => {
                if sender == self.own_user_id() && content.from_device() != self.account.device_id()
                {
                    let new_value = InnerRequest::Passive(s.clone().into_passive(content));
                    ObservableWriteGuard::set(&mut guard, new_value);
                }
            }
            InnerRequest::Ready(_)
            | InnerRequest::Passive(_)
            | InnerRequest::Done(_)
            | InnerRequest::Cancelled(_) => {}
        }
    }

    pub(crate) async fn receive_start(
        &self,
        sender: &UserId,
        content: &StartContent<'_>,
    ) -> Result<(), CryptoStoreError> {
        let inner = self.inner.get();

        let InnerRequest::Ready(s) = inner else {
            warn!(
                sender = sender.as_str(),
                device_id = content.from_device().as_str(),
                "Received a key verification start event but we're not yet in the ready state"
            );
            return Ok(());
        };

        s.receive_start(sender, content, self.we_started, self.inner.clone().into()).await
    }

    pub(crate) fn receive_done(&self, sender: &UserId, content: &DoneContent<'_>) {
        if sender == self.other_user() {
            trace!(
                other_user = self.other_user().as_str(),
                flow_id = self.flow_id().as_str(),
                "Marking a verification request as done"
            );

            let mut guard = self.inner.write();
            if let Some(updated) = guard.receive_done(content) {
                ObservableWriteGuard::set(&mut guard, updated);
            }
        }
    }

    pub(crate) fn receive_cancel(&self, sender: &UserId, content: &CancelContent<'_>) {
        if sender != self.other_user() {
            return;
        }

        trace!(
            sender = sender.as_str(),
            code = content.cancel_code().as_str(),
            "Cancelling a verification request, other user has cancelled"
        );
        let mut guard = self.inner.write();
        if let Some(updated) = guard.cancel(false, content.cancel_code()) {
            ObservableWriteGuard::set(&mut guard, updated);
        }

        if self.we_started() {
            if let Some(request) =
                self.cancel_for_other_devices(content.cancel_code().to_owned(), None)
            {
                self.verification_cache.add_verification_request(request.into());
            }
        }
    }

    /// Transition from this verification request into a SAS verification flow.
    pub async fn start_sas(
        &self,
    ) -> Result<Option<(Sas, OutgoingVerificationRequest)>, CryptoStoreError> {
        let inner = self.inner.get();

        Ok(match &inner {
            InnerRequest::Ready(s) => {
                if let Some((sas, content)) =
                    s.clone().start_sas(self.we_started, self.inner.clone().into()).await?
                {
                    // We may have previously started QR verification and generated a QR code. If we
                    // now switch to SAS flow, the previous verification has to be replaced
                    cfg_if::cfg_if! {
                        if #[cfg(feature = "qrcode")] {
                            if self.verification_cache.get_qr(sas.other_user_id(), sas.flow_id().as_str()).is_some() {
                                debug!(
                                    user_id = ?self.other_user(),
                                    flow_id = self.flow_id().as_str(),
                                    "We have an ongoing QR verification, replacing with SAS"
                                );
                                self.verification_cache.replace(sas.clone().into())
                            } else {
                                self.verification_cache.insert_sas(sas.clone());
                            }
                        } else {
                            self.verification_cache.insert_sas(sas.clone());
                        }
                    }

                    let request = match content {
                        OutgoingContent::ToDevice(content) => ToDeviceRequest::with_id(
                            self.other_user(),
                            inner.other_device_id(),
                            content,
                            TransactionId::new(),
                        )
                        .into(),
                        OutgoingContent::Room(room_id, content) => {
                            RoomMessageRequest { room_id, txn_id: TransactionId::new(), content }
                                .into()
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

    /// Listen for changes in the verification request.
    ///
    /// The changes are presented as a stream of [`VerificationRequestState`]
    /// values.
    pub fn changes(&self) -> impl Stream<Item = VerificationRequestState> {
        self.inner.subscribe().map(|s| (&s).into())
    }

    /// Get the current state the verification request is in.
    ///
    /// To listen to changes to the [`VerificationRequestState`] use the
    /// [`VerificationRequest::changes`] method.
    pub fn state(&self) -> VerificationRequestState {
        (&*self.inner.read()).into()
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

    fn accept(&self, methods: Vec<VerificationMethod>) -> Option<(InnerRequest, OutgoingContent)> {
        let InnerRequest::Requested(s) = self else { return None };
        let (state, content) = s.clone().accept(methods);

        Some((InnerRequest::Ready(state), content))
    }

    fn receive_done(&self, content: &DoneContent<'_>) -> Option<InnerRequest> {
        let state = InnerRequest::Done(match self {
            InnerRequest::Ready(s) => s.clone().into_done(content),
            InnerRequest::Passive(s) => s.clone().into_done(content),
            InnerRequest::Done(_)
            | InnerRequest::Created(_)
            | InnerRequest::Requested(_)
            | InnerRequest::Cancelled(_) => return None,
        });

        Some(state)
    }

    fn cancel(&self, cancelled_by_us: bool, cancel_code: &CancelCode) -> Option<InnerRequest> {
        let print_info = || {
            trace!(
                cancelled_by_us = cancelled_by_us,
                code = cancel_code.as_str(),
                "Verification request going into the cancelled state"
            );
        };

        let state = InnerRequest::Cancelled(match self {
            InnerRequest::Created(s) => {
                print_info();
                s.clone().into_canceled(cancelled_by_us, cancel_code)
            }
            InnerRequest::Requested(s) => {
                print_info();
                s.clone().into_canceled(cancelled_by_us, cancel_code)
            }
            InnerRequest::Ready(s) => {
                print_info();
                s.clone().into_canceled(cancelled_by_us, cancel_code)
            }
            InnerRequest::Passive(_) | InnerRequest::Done(_) | InnerRequest::Cancelled(_) => {
                return None
            }
        });

        Some(state)
    }

    #[cfg(feature = "qrcode")]
    async fn generate_qr_code(
        &self,
        we_started: bool,
        request_handle: RequestHandle,
    ) -> Result<Option<QrVerification>, CryptoStoreError> {
        match self {
            InnerRequest::Created(_) => Ok(None),
            InnerRequest::Requested(_) => Ok(None),
            InnerRequest::Ready(s) => s.generate_qr_code(we_started, request_handle).await,
            InnerRequest::Passive(_) => Ok(None),
            InnerRequest::Done(_) => Ok(None),
            InnerRequest::Cancelled(_) => Ok(None),
        }
    }
}

#[derive(Clone, Debug)]
struct RequestState<S: Clone> {
    verification_cache: VerificationCache,
    store: VerificationStore,
    flow_id: Arc<FlowId>,

    /// The id of the user which is participating in this verification request.
    pub other_user_id: OwnedUserId,

    /// The verification request state we are in.
    state: S,
}

impl<S: Clone> RequestState<S> {
    fn into_done(self, _: &DoneContent<'_>) -> RequestState<Done> {
        RequestState::<Done> {
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
        cache: VerificationCache,
        store: VerificationStore,
        other_user_id: &UserId,
        flow_id: &FlowId,
        methods: Option<Vec<VerificationMethod>>,
    ) -> Self {
        let our_methods = methods.unwrap_or_else(|| SUPPORTED_METHODS.to_vec());

        Self {
            other_user_id: other_user_id.to_owned(),
            state: Created { our_methods },
            verification_cache: cache,
            store,
            flow_id: flow_id.to_owned().into(),
        }
    }

    fn into_ready(self, _sender: &UserId, content: &ReadyContent<'_>) -> RequestState<Ready> {
        // TODO check the flow id, and that the methods match what we suggested.
        RequestState {
            flow_id: self.flow_id,
            verification_cache: self.verification_cache,
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

    /// The device ID of the device that responded to the verification request.
    pub other_device_id: OwnedDeviceId,
}

impl RequestState<Requested> {
    fn from_request_event(
        cache: VerificationCache,
        store: VerificationStore,
        sender: &UserId,
        flow_id: &FlowId,
        content: &RequestContent<'_>,
    ) -> RequestState<Requested> {
        // TODO only create this if we support the methods
        RequestState {
            store,
            verification_cache: cache,
            flow_id: flow_id.to_owned().into(),
            other_user_id: sender.to_owned(),
            state: Requested {
                their_methods: content.methods().to_owned(),
                other_device_id: content.from_device().into(),
            },
        }
    }

    fn into_passive(self, content: &ReadyContent<'_>) -> RequestState<Passive> {
        RequestState {
            flow_id: self.flow_id,
            verification_cache: self.verification_cache,
            store: self.store,
            other_user_id: self.other_user_id,
            state: Passive { other_device_id: content.from_device().to_owned() },
        }
    }

    fn accept(self, methods: Vec<VerificationMethod>) -> (RequestState<Ready>, OutgoingContent) {
        let state = RequestState {
            store: self.store,
            verification_cache: self.verification_cache,
            flow_id: self.flow_id.clone(),
            other_user_id: self.other_user_id,
            state: Ready {
                their_methods: self.state.their_methods,
                our_methods: methods.clone(),
                other_device_id: self.state.other_device_id.clone(),
            },
        };

        let content = match self.flow_id.as_ref() {
            FlowId::ToDevice(i) => AnyToDeviceEventContent::KeyVerificationReady(
                ToDeviceKeyVerificationReadyEventContent::new(
                    state.store.account.device_id().to_owned(),
                    methods,
                    i.to_owned(),
                ),
            )
            .into(),
            FlowId::InRoom(r, e) => (
                r.to_owned(),
                AnyMessageLikeEventContent::KeyVerificationReady(
                    KeyVerificationReadyEventContent::new(
                        state.store.account.device_id().to_owned(),
                        methods,
                        Reference::new(e.to_owned()),
                    ),
                ),
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

    /// The device ID of the device that responded to the verification request.
    pub other_device_id: OwnedDeviceId,
}

impl RequestState<Ready> {
    fn to_started_sas(
        &self,
        content: &StartContent<'_>,
        identities: IdentitiesBeingVerified,
        we_started: bool,
        request_handle: RequestHandle,
    ) -> Result<Sas, OutgoingContent> {
        Sas::from_start_event(
            (*self.flow_id).to_owned(),
            content,
            identities,
            Some(request_handle),
            we_started,
        )
    }

    #[cfg(feature = "qrcode")]
    async fn generate_qr_code(
        &self,
        we_started: bool,
        request_handle: RequestHandle,
    ) -> Result<Option<QrVerification>, CryptoStoreError> {
        use crate::ReadOnlyUserIdentities;

        // If we didn't state that we support showing QR codes or if the other
        // side doesn't support scanning QR codes bail early.
        if !self.state.our_methods.contains(&VerificationMethod::QrCodeShowV1)
            || !self.state.their_methods.contains(&VerificationMethod::QrCodeScanV1)
        {
            return Ok(None);
        }

        let Some(device) =
            self.store.get_device(&self.other_user_id, &self.state.other_device_id).await? else {
                warn!(
                    user_id = self.other_user_id.as_str(),
                    device_id = self.state.other_device_id.as_str(),
                    "Can't create a QR code, the device that accepted the \
                    verification doesn't exist"
                );
                return Ok(None);
            };

        let identities = self.store.get_identities(device).await?;

        let verification = if let Some(identity) = &identities.identity_being_verified {
            match &identity {
                ReadOnlyUserIdentities::Own(i) => {
                    if let Some(master_key) = i.master_key().get_first_key() {
                        if identities.can_sign_devices().await {
                            if let Some(device_key) = identities.other_device().ed25519_key() {
                                Some(QrVerification::new_self(
                                    self.flow_id.as_ref().to_owned(),
                                    master_key.to_owned(),
                                    device_key.to_owned(),
                                    identities,
                                    we_started,
                                    Some(request_handle),
                                ))
                            } else {
                                warn!(
                                    user_id = self.other_user_id.as_str(),
                                    device_id = self.state.other_device_id.as_str(),
                                    "Can't create a QR code, the other device \
                                     doesn't have a valid device key"
                                );
                                None
                            }
                        } else {
                            Some(QrVerification::new_self_no_master(
                                self.store.clone(),
                                self.flow_id.as_ref().to_owned(),
                                master_key.to_owned(),
                                identities,
                                we_started,
                                Some(request_handle),
                            ))
                        }
                    } else {
                        warn!(
                            user_id = self.other_user_id.as_str(),
                            device_id = self.state.other_device_id.as_str(),
                            "Can't create a QR code, our cross signing identity \
                             doesn't contain a valid master key"
                        );
                        None
                    }
                }
                ReadOnlyUserIdentities::Other(i) => {
                    if let Some(other_master) = i.master_key().get_first_key() {
                        // TODO we can get the master key from the public
                        // identity if we don't have the private one and we
                        // trust the public one.
                        if let Some(own_master) = identities
                            .private_identity
                            .master_public_key()
                            .await
                            .and_then(|m| m.get_first_key().map(|m| m.to_owned()))
                        {
                            Some(QrVerification::new_cross(
                                self.flow_id.as_ref().to_owned(),
                                own_master,
                                other_master.to_owned(),
                                identities,
                                we_started,
                                Some(request_handle),
                            ))
                        } else {
                            warn!(
                                user_id = self.other_user_id.as_str(),
                                device_id = self.state.other_device_id.as_str(),
                                "Can't create a QR code, we don't trust our own \
                                 master key"
                            );
                            None
                        }
                    } else {
                        warn!(
                            user_id = self.other_user_id.as_str(),
                            device_id = self.state.other_device_id.as_str(),
                            "Can't create a QR code, the user's identity \
                             doesn't have a valid master key"
                        );
                        None
                    }
                }
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
        request_handle: RequestHandle,
    ) -> Result<(), CryptoStoreError> {
        info!(
            sender = sender.as_str(),
            device = content.from_device().as_str(),
            "Received a new verification start event",
        );

        let Some(device) = self.store.get_device(sender, content.from_device()).await? else {
            warn!(
                sender = sender.as_str(),
                device = content.from_device().as_str(),
                "Received a key verification start event from an unknown device",
            );

            return Ok(());
        };

        let identities = self.store.get_identities(device.clone()).await?;
        let own_user_id = self.store.account.user_id();
        let own_device_id = self.store.account.device_id();

        match content.method() {
            StartMethod::SasV1(_) => {
                match self.to_started_sas(content, identities, we_started, request_handle) {
                    Ok(s) => {
                        let start_new = if let Some(Verification::SasV1(_sas)) =
                            self.verification_cache.get(sender, self.flow_id.as_str())
                        {
                            // If there is already a SAS verification, i.e. we already started one
                            // before the other side tried to do the same; ignore it if we did and
                            // we're the lexicographically smaller user ID (or device ID if equal).
                            use std::cmp::Ordering;
                            !matches!(
                                (sender.cmp(own_user_id), device.device_id().cmp(own_device_id)),
                                (Ordering::Greater, _) | (Ordering::Equal, Ordering::Greater)
                            )
                        } else {
                            true
                        };

                        if start_new {
                            info!("Started a new SAS verification.");
                            self.verification_cache.replace_sas(s);
                        }
                    }
                    Err(c) => {
                        warn!(
                            user_id = ?device.user_id(),
                            device_id = ?device.device_id(),
                            content = ?c,
                            "Can't start key verification, canceling.",
                        );
                        self.verification_cache.queue_up_content(
                            device.user_id(),
                            device.device_id(),
                            c,
                            None,
                        )
                    }
                }
            }
            #[cfg(feature = "qrcode")]
            StartMethod::ReciprocateV1(_) => {
                if let Some(qr_verification) =
                    self.verification_cache.get_qr(sender, content.flow_id())
                {
                    if let Some(request) = qr_verification.receive_reciprocation(content) {
                        self.verification_cache.add_request(request.into())
                    }
                    trace!(
                        sender = ?identities.device_being_verified.user_id(),
                        device_id = ?identities.device_being_verified.device_id(),
                        verification = ?qr_verification,
                        "Received a QR code reciprocation"
                    )
                }
            }
            m => {
                warn!(method = ?m, "Received a key verification start event with an unsupported method")
            }
        }

        Ok(())
    }

    async fn start_sas(
        self,
        we_started: bool,
        request_handle: RequestHandle,
    ) -> Result<Option<(Sas, OutgoingContent)>, CryptoStoreError> {
        if !self.state.their_methods.contains(&VerificationMethod::SasV1) {
            return Ok(None);
        }

        // TODO signal why starting the sas flow doesn't work?
        let Some(device) =
            self.store.get_device(&self.other_user_id, &self.state.other_device_id).await? else {
                warn!(
                    user_id = self.other_user_id.as_str(),
                    device_id = self.state.other_device_id.as_str(),
                    "Can't start the SAS verification flow, the device that \
                    accepted the verification doesn't exist"
                );
                return Ok(None);
            };

        let identities = self.store.get_identities(device).await?;

        Ok(Some(match self.flow_id.as_ref() {
            FlowId::ToDevice(t) => {
                let (sas, content) =
                    Sas::start(identities, t.to_owned(), we_started, Some(request_handle));
                (sas, content)
            }
            FlowId::InRoom(r, e) => {
                let (sas, content) = Sas::start_in_room(
                    e.to_owned(),
                    r.to_owned(),
                    identities,
                    we_started,
                    request_handle,
                );
                (sas, content)
            }
        }))
    }
}

#[derive(Clone, Debug)]
struct Passive {
    /// The device ID of the device that responded to the verification request.
    #[allow(dead_code)]
    pub other_device_id: OwnedDeviceId,
}

#[derive(Clone, Debug)]
struct Done {}

#[cfg(test)]
mod tests {

    use std::convert::{TryFrom, TryInto};

    use assert_matches::assert_matches;
    #[cfg(feature = "qrcode")]
    use matrix_sdk_qrcode::QrVerificationData;
    use matrix_sdk_test::async_test;
    #[cfg(feature = "qrcode")]
    use ruma::events::key::verification::VerificationMethod;
    use ruma::{event_id, room_id};

    use super::VerificationRequest;
    use crate::{
        verification::{
            cache::VerificationCache,
            event_enums::{OutgoingContent, ReadyContent, RequestContent, StartContent},
            test::{alice_id, bob_id, setup_stores},
            FlowId,
        },
        ReadOnlyDevice, VerificationRequestState,
    };

    #[async_test]
    async fn test_request_accepting() {
        let event_id = event_id!("$1234localhost").to_owned();
        let room_id = room_id!("!test:localhost").to_owned();

        let (alice_store, bob_store) = setup_stores().await;

        let content = VerificationRequest::request(
            bob_store.account.user_id(),
            bob_store.account.device_id(),
            alice_id(),
            None,
        );

        let flow_id = FlowId::InRoom(room_id, event_id);

        let bob_request = VerificationRequest::new(
            VerificationCache::new(),
            bob_store,
            flow_id.clone(),
            alice_id(),
            vec![],
            None,
        );

        assert_matches!(bob_request.state(), VerificationRequestState::Created { .. });

        #[allow(clippy::needless_borrow)]
        let alice_request = VerificationRequest::from_request(
            VerificationCache::new(),
            alice_store,
            bob_id(),
            flow_id,
            &(&content).into(),
        );

        assert_matches!(alice_request.state(), VerificationRequestState::Requested { .. });

        let content: OutgoingContent = alice_request.accept().unwrap().try_into().unwrap();
        let content = ReadyContent::try_from(&content).unwrap();

        bob_request.receive_ready(alice_id(), &content);

        assert_matches!(bob_request.state(), VerificationRequestState::Ready { .. });
        assert_matches!(alice_request.state(), VerificationRequestState::Ready { .. });
        assert!(bob_request.is_ready());
        assert!(alice_request.is_ready());
    }

    #[async_test]
    async fn test_requesting_until_sas() {
        let event_id = event_id!("$1234localhost");
        let room_id = room_id!("!test:localhost");

        let (alice_store, bob_store) = setup_stores().await;
        let bob_device = ReadOnlyDevice::from_account(&bob_store.account).await;

        let content = VerificationRequest::request(
            bob_store.account.user_id(),
            bob_store.account.device_id(),
            alice_id(),
            None,
        );
        let flow_id = FlowId::from((room_id, event_id));

        let bob_request = VerificationRequest::new(
            VerificationCache::new(),
            bob_store,
            flow_id.clone(),
            alice_id(),
            vec![],
            None,
        );

        #[allow(clippy::needless_borrow)]
        let alice_request = VerificationRequest::from_request(
            VerificationCache::new(),
            alice_store,
            bob_id(),
            flow_id,
            &(&content).into(),
        );

        let content: OutgoingContent = alice_request.accept().unwrap().try_into().unwrap();
        let content = ReadyContent::try_from(&content).unwrap();

        bob_request.receive_ready(alice_id(), &content);

        assert!(bob_request.is_ready());
        assert!(alice_request.is_ready());

        let (bob_sas, request) = bob_request.start_sas().await.unwrap().unwrap();

        let content: OutgoingContent = request.try_into().unwrap();
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
        let (alice_store, bob_store) = setup_stores().await;
        let bob_device = ReadOnlyDevice::from_account(&bob_store.account).await;

        let flow_id = FlowId::ToDevice("TEST_FLOW_ID".into());

        let bob_request = VerificationRequest::new(
            VerificationCache::new(),
            bob_store,
            flow_id,
            alice_id(),
            vec![],
            None,
        );

        let request = bob_request.request_to_device();
        let content: OutgoingContent = request.try_into().unwrap();
        let content = RequestContent::try_from(&content).unwrap();
        let flow_id = bob_request.flow_id().to_owned();

        let alice_request = VerificationRequest::from_request(
            VerificationCache::new(),
            alice_store,
            bob_id(),
            flow_id,
            &content,
        );

        let content: OutgoingContent = alice_request.accept().unwrap().try_into().unwrap();
        let content = ReadyContent::try_from(&content).unwrap();

        bob_request.receive_ready(alice_id(), &content);

        assert!(bob_request.is_ready());
        assert!(alice_request.is_ready());

        let (bob_sas, request) = bob_request.start_sas().await.unwrap().unwrap();

        let content: OutgoingContent = request.try_into().unwrap();
        let content = StartContent::try_from(&content).unwrap();
        let flow_id = content.flow_id().to_owned();
        alice_request.receive_start(bob_device.user_id(), &content).await.unwrap();
        let alice_sas =
            alice_request.verification_cache.get_sas(bob_device.user_id(), &flow_id).unwrap();

        assert!(!bob_sas.is_cancelled());
        assert!(!alice_sas.is_cancelled());
        assert!(alice_sas.started_from_request());
        assert!(bob_sas.started_from_request());
    }

    #[async_test]
    #[cfg(feature = "qrcode")]
    async fn can_scan_another_qr_after_creating_mine() {
        let (alice_store, bob_store) = setup_stores().await;

        let flow_id = FlowId::ToDevice("TEST_FLOW_ID".into());

        // We setup the initial verification request
        let bob_request = VerificationRequest::new(
            VerificationCache::new(),
            bob_store,
            flow_id.clone(),
            alice_id(),
            vec![],
            Some(vec![VerificationMethod::QrCodeScanV1, VerificationMethod::QrCodeShowV1]),
        );

        let request = bob_request.request_to_device();
        let content: OutgoingContent = request.try_into().unwrap();
        let content = RequestContent::try_from(&content).unwrap();

        let alice_request = VerificationRequest::from_request(
            VerificationCache::new(),
            alice_store,
            bob_id(),
            flow_id,
            &content,
        );

        let content: OutgoingContent = alice_request
            .accept_with_methods(vec![
                VerificationMethod::QrCodeScanV1,
                VerificationMethod::QrCodeShowV1,
            ])
            .unwrap()
            .try_into()
            .unwrap();
        let content = ReadyContent::try_from(&content).unwrap();
        bob_request.receive_ready(alice_id(), &content);

        assert!(bob_request.is_ready());
        assert!(alice_request.is_ready());

        // Each side can start its own QR verification flow by generating QR code
        let alice_verification = alice_request.generate_qr_code().await.unwrap();
        let bob_verification = bob_request.generate_qr_code().await.unwrap();

        assert!(alice_verification.is_some());
        assert!(bob_verification.is_some());

        // Now only Alice scans Bob's code
        let bob_qr_code = bob_verification.unwrap().to_bytes().unwrap();
        let bob_qr_code = QrVerificationData::from_bytes(bob_qr_code).unwrap();
        let alice_verification = alice_request.scan_qr_code(bob_qr_code).await.unwrap().unwrap();

        // Finally we assert that the verification has been reciprocated rather than
        // cancelled due to a duplicate verification flow
        assert!(!alice_verification.is_cancelled());
        assert!(alice_verification.reciprocated());
    }

    #[async_test]
    #[cfg(feature = "qrcode")]
    async fn can_start_sas_after_generating_qr_code() {
        let (alice_store, bob_store) = setup_stores().await;

        let flow_id = FlowId::ToDevice("TEST_FLOW_ID".into());

        // We setup the initial verification request
        let bob_request = VerificationRequest::new(
            VerificationCache::new(),
            bob_store,
            flow_id.clone(),
            alice_id(),
            vec![],
            Some(vec![
                VerificationMethod::QrCodeScanV1,
                VerificationMethod::QrCodeShowV1,
                VerificationMethod::SasV1,
            ]),
        );

        let request = bob_request.request_to_device();
        let content: OutgoingContent = request.try_into().unwrap();
        let content = RequestContent::try_from(&content).unwrap();

        let alice_request = VerificationRequest::from_request(
            VerificationCache::new(),
            alice_store,
            bob_id(),
            flow_id,
            &content,
        );

        let content: OutgoingContent = alice_request
            .accept_with_methods(vec![
                VerificationMethod::QrCodeScanV1,
                VerificationMethod::QrCodeShowV1,
            ])
            .unwrap()
            .try_into()
            .unwrap();
        let content = ReadyContent::try_from(&content).unwrap();
        bob_request.receive_ready(alice_id(), &content);

        assert!(bob_request.is_ready());
        assert!(alice_request.is_ready());

        // Each side can start its own QR verification flow by generating QR code
        let alice_verification = alice_request.generate_qr_code().await.unwrap();
        let bob_verification = bob_request.generate_qr_code().await.unwrap();

        assert!(alice_verification.is_some());
        assert!(bob_verification.is_some());

        // Alice can now start SAS verification flow instead of QR without cancelling
        // the request
        let (sas, _) = alice_request.start_sas().await.unwrap().unwrap();
        assert!(!sas.is_cancelled());
    }
}
