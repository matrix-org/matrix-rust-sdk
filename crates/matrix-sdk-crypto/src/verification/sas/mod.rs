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

mod helpers;
mod inner_sas;
mod sas_state;

use std::sync::{Arc, Mutex};

use futures_core::Stream;
use futures_signals::signal::{Mutable, SignalExt};
use inner_sas::InnerSas;
use ruma::{
    api::client::keys::upload_signatures::v3::Request as SignatureUploadRequest,
    events::{
        key::verification::{cancel::CancelCode, start::SasV1Content, ShortAuthenticationString},
        AnyMessageLikeEventContent, AnyToDeviceEventContent,
    },
    DeviceId, OwnedEventId, OwnedRoomId, OwnedTransactionId, RoomId, TransactionId, UserId,
};
pub use sas_state::AcceptedProtocols;
use tracing::{debug, error, trace};

use super::{
    cache::RequestInfo,
    event_enums::{AnyVerificationContent, OutgoingContent, OwnedAcceptContent, StartContent},
    requests::RequestHandle,
    CancelInfo, FlowId, IdentitiesBeingVerified, VerificationResult,
};
use crate::{
    identities::{ReadOnlyDevice, ReadOnlyUserIdentities},
    requests::{OutgoingVerificationRequest, RoomMessageRequest},
    store::CryptoStoreError,
    Emoji, ReadOnlyAccount, ToDeviceRequest,
};

/// Short authentication string object.
#[derive(Clone, Debug)]
pub struct Sas {
    inner: Arc<Mutex<InnerSas>>,
    state: Arc<Mutable<SasState>>,
    account: ReadOnlyAccount,
    identities_being_verified: IdentitiesBeingVerified,
    flow_id: Arc<FlowId>,
    we_started: bool,
    request_handle: Option<RequestHandle>,
}

#[derive(Debug, Clone, Copy)]
enum State {
    Created,
    Started,
    Accepted,
    WeAccepted,
    KeyReceived,
    KeySent,
    KeysExchanged,
    Confirmed,
    MacReceived,
    WaitingForDone,
    Done,
    Cancelled,
}

impl From<&InnerSas> for State {
    fn from(value: &InnerSas) -> Self {
        match value {
            InnerSas::Created(_) => Self::Created,
            InnerSas::Started(_) => Self::Started,
            InnerSas::Accepted(_) => Self::Accepted,
            InnerSas::WeAccepted(_) => Self::WeAccepted,
            InnerSas::KeyReceived(_) => Self::KeyReceived,
            InnerSas::KeySent(_) => Self::KeySent,
            InnerSas::KeysExchanged(_) => Self::KeysExchanged,
            InnerSas::Confirmed(_) => Self::Confirmed,
            InnerSas::MacReceived(_) => Self::MacReceived,
            InnerSas::WaitingForDone(_) => Self::WaitingForDone,
            InnerSas::Done(_) => Self::Done,
            InnerSas::Cancelled(_) => Self::Cancelled,
        }
    }
}

/// The short auth string for the emoji method of SAS verification.
#[derive(Debug, Clone)]
pub struct EmojiShortAuthString {
    /// A list of seven indices that should be used for the SAS verification.
    ///
    /// The indices can be put into the emoji table in the [spec] to figure out
    /// the symbols and descriptions.
    ///
    /// If you have a table of [translated descriptions] for the emojis you will
    /// want to use this field.
    ///
    /// [spec]: https://spec.matrix.org/unstable/client-server-api/#sas-method-emoji
    /// [translated descriptions]: https://github.com/matrix-org/matrix-doc/blob/master/data-definitions/
    pub indices: [u8; 7],

    /// A list of seven emojis that should be used for the SAS verification.
    pub emojis: [Emoji; 7],
}

/// An Enum describing the state the SAS verification is in.
#[derive(Debug, Clone)]
pub enum SasState {
    /// The verification has been started, the protocols that should be used
    /// have been proposed and can be accepted.
    Started {
        /// The protocols that were proposed in the `m.key.verification.start`
        /// event.
        protocols: SasV1Content,
    },
    /// The verification has been accepted and both sides agreed to a set of
    /// protocols that will be used for the verification process.
    Accepted {
        /// The protocols that were accepted in the `m.key.verification.accept`
        /// event.
        accepted_protocols: AcceptedProtocols,
    },
    /// The public keys have been exchanged and the short auth string can be
    /// presented to the user.
    KeysExchanged {
        /// The emojis that represent the short auth string, will be `None` if
        /// the emoji SAS method wasn't part of the [`AcceptedProtocols`].
        emojis: Option<EmojiShortAuthString>,
        /// The list of decimals that represent the short auth string.
        decimals: (u16, u16, u16),
    },
    /// The verification process has been confirmed from our side, we're waiting
    /// for the other side to confirm as well.
    Confirmed,
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

impl PartialEq for SasState {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (Self::Started { .. }, Self::Started { .. })
                | (Self::Accepted { .. }, Self::Accepted { .. })
                | (Self::KeysExchanged { .. }, Self::KeysExchanged { .. })
                | (Self::Confirmed, Self::Confirmed)
                | (Self::Done { .. }, Self::Done { .. })
                | (Self::Cancelled(_), Self::Cancelled(_))
        )
    }
}

impl From<&InnerSas> for SasState {
    fn from(value: &InnerSas) -> Self {
        match value {
            InnerSas::Created(s) => {
                Self::Started { protocols: s.state.protocol_definitions.to_owned() }
            }
            InnerSas::Started(s) => {
                Self::Started { protocols: s.state.protocol_definitions.to_owned() }
            }
            InnerSas::Accepted(s) => {
                Self::Accepted { accepted_protocols: s.state.accepted_protocols.to_owned() }
            }
            InnerSas::WeAccepted(s) => {
                Self::Accepted { accepted_protocols: s.state.accepted_protocols.to_owned() }
            }
            InnerSas::KeySent(s) => {
                Self::Accepted { accepted_protocols: s.state.accepted_protocols.to_owned() }
            }
            InnerSas::KeyReceived(s) => {
                Self::Accepted { accepted_protocols: s.state.accepted_protocols.to_owned() }
            }
            InnerSas::KeysExchanged(s) => {
                let emojis = if value.supports_emoji() {
                    let emojis = s.get_emoji();
                    let indices = s.get_emoji_index();

                    Some(EmojiShortAuthString { emojis, indices })
                } else {
                    None
                };

                let decimals = s.get_decimal();

                Self::KeysExchanged { emojis, decimals }
            }
            InnerSas::MacReceived(s) => {
                let emojis = if value.supports_emoji() {
                    let emojis = s.get_emoji();
                    let indices = s.get_emoji_index();

                    Some(EmojiShortAuthString { emojis, indices })
                } else {
                    None
                };

                let decimals = s.get_decimal();

                Self::KeysExchanged { emojis, decimals }
            }
            InnerSas::Confirmed(_) => Self::Confirmed,
            InnerSas::WaitingForDone(_) => Self::Confirmed,
            InnerSas::Done(s) => Self::Done {
                verified_devices: s.verified_devices().to_vec(),
                verified_identities: s.verified_identities().to_vec(),
            },
            InnerSas::Cancelled(c) => Self::Cancelled(c.state.as_ref().clone().into()),
        }
    }
}

impl Sas {
    /// Get our own user id.
    pub fn user_id(&self) -> &UserId {
        self.account.user_id()
    }

    /// Get our own device ID.
    pub fn device_id(&self) -> &DeviceId {
        self.account.device_id()
    }

    /// Get the user id of the other side.
    pub fn other_user_id(&self) -> &UserId {
        self.identities_being_verified.other_user_id()
    }

    /// Get the device ID of the other side.
    pub fn other_device_id(&self) -> &DeviceId {
        self.identities_being_verified.other_device_id()
    }

    /// Get the device of the other user.
    pub fn other_device(&self) -> &ReadOnlyDevice {
        self.identities_being_verified.other_device()
    }

    /// Get the unique ID that identifies this SAS verification flow.
    pub fn flow_id(&self) -> &FlowId {
        &self.flow_id
    }

    /// Get the room id if the verification is happening inside a room.
    pub fn room_id(&self) -> Option<&RoomId> {
        if let FlowId::InRoom(r, _) = self.flow_id() {
            Some(r)
        } else {
            None
        }
    }

    /// Does this verification flow support displaying emoji for the short
    /// authentication string.
    pub fn supports_emoji(&self) -> bool {
        self.inner.lock().unwrap().supports_emoji()
    }

    /// Did this verification flow start from a verification request.
    pub fn started_from_request(&self) -> bool {
        self.inner.lock().unwrap().started_from_request()
    }

    /// Is this a verification that is veryfying one of our own devices.
    pub fn is_self_verification(&self) -> bool {
        self.identities_being_verified.is_self_verification()
    }

    /// Have we confirmed that the short auth string matches.
    pub fn have_we_confirmed(&self) -> bool {
        self.inner.lock().unwrap().have_we_confirmed()
    }

    /// Has the verification been accepted by both parties.
    pub fn has_been_accepted(&self) -> bool {
        self.inner.lock().unwrap().has_been_accepted()
    }

    /// Get info about the cancellation if the verification flow has been
    /// cancelled.
    pub fn cancel_info(&self) -> Option<CancelInfo> {
        if let InnerSas::Cancelled(c) = &*self.inner.lock().unwrap() {
            Some(c.state.as_ref().clone().into())
        } else {
            None
        }
    }

    /// Did we initiate the verification flow.
    pub fn we_started(&self) -> bool {
        self.we_started
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn set_creation_time(&self, time: matrix_sdk_common::instant::Instant) {
        self.inner.lock().unwrap().set_creation_time(time)
    }

    fn start_helper(
        flow_id: FlowId,
        identities: IdentitiesBeingVerified,
        we_started: bool,
        request_handle: Option<RequestHandle>,
    ) -> (Sas, OutgoingContent) {
        let (inner, content) = InnerSas::start(
            identities.store.account.clone(),
            identities.device_being_verified.clone(),
            identities.own_identity.clone(),
            identities.identity_being_verified.clone(),
            flow_id.clone(),
            request_handle.is_some(),
        );

        let state = (&inner).into();
        let account = identities.store.account.clone();

        (
            Sas {
                inner: Arc::new(Mutex::new(inner)),
                state: Mutable::new(state).into(),
                account,
                identities_being_verified: identities,
                flow_id: flow_id.into(),
                we_started,
                request_handle,
            },
            content,
        )
    }

    /// Start a new SAS auth flow with the given device.
    ///
    /// # Arguments
    ///
    /// * `account` - Our own account.
    ///
    /// * `other_device` - The other device which we are going to verify.
    ///
    /// Returns the new `Sas` object and a `StartEventContent` that needs to be
    /// sent out through the server to the other device.
    pub(crate) fn start(
        identities: IdentitiesBeingVerified,
        transaction_id: OwnedTransactionId,
        we_started: bool,
        request_handle: Option<RequestHandle>,
    ) -> (Sas, OutgoingContent) {
        let flow_id = FlowId::ToDevice(transaction_id);

        Self::start_helper(flow_id, identities, we_started, request_handle)
    }

    /// Start a new SAS auth flow with the given device inside the given room.
    ///
    /// # Arguments
    ///
    /// * `account` - Our own account.
    ///
    /// * `other_device` - The other device which we are going to verify.
    ///
    /// Returns the new `Sas` object and a `StartEventContent` that needs to be
    /// sent out through the server to the other device.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start_in_room(
        flow_id: OwnedEventId,
        room_id: OwnedRoomId,
        identities: IdentitiesBeingVerified,
        we_started: bool,
        request_handle: RequestHandle,
    ) -> (Sas, OutgoingContent) {
        let flow_id = FlowId::InRoom(room_id, flow_id);
        Self::start_helper(flow_id, identities, we_started, Some(request_handle))
    }

    /// Create a new Sas object from a m.key.verification.start request.
    ///
    /// # Arguments
    ///
    /// * `account` - Our own account.
    ///
    /// * `other_device` - The other device which we are going to verify.
    ///
    /// * `event` - The m.key.verification.start event that was sent to us by
    /// the other side.
    pub(crate) fn from_start_event(
        flow_id: FlowId,
        content: &StartContent<'_>,
        identities: IdentitiesBeingVerified,
        request_handle: Option<RequestHandle>,
        we_started: bool,
    ) -> Result<Sas, OutgoingContent> {
        let inner = InnerSas::from_start_event(
            identities.store.account.clone(),
            identities.device_being_verified.clone(),
            flow_id.clone(),
            content,
            identities.own_identity.clone(),
            identities.identity_being_verified.clone(),
            request_handle.is_some(),
        )?;

        let state = (&inner).into();
        let account = identities.store.account.clone();

        Ok(Sas {
            inner: Arc::new(Mutex::new(inner)),
            state: Mutable::new(state).into(),
            account,
            identities_being_verified: identities,
            flow_id: flow_id.into(),
            we_started,
            request_handle,
        })
    }

    /// Accept the SAS verification.
    ///
    /// This does nothing if the verification was already accepted, otherwise it
    /// returns an `AcceptEventContent` that needs to be sent out.
    pub fn accept(&self) -> Option<OutgoingVerificationRequest> {
        self.accept_with_settings(Default::default())
    }

    /// Accept the SAS verification customizing the accept method.
    ///
    /// This does nothing if the verification was already accepted, otherwise it
    /// returns an `AcceptEventContent` that needs to be sent out.
    ///
    /// Specify a function modifying the attributes of the accept request.
    pub fn accept_with_settings(
        &self,
        settings: AcceptSettings,
    ) -> Option<OutgoingVerificationRequest> {
        let old_state = self.state_debug();

        let (request, state) = {
            let mut guard = self.inner.lock().unwrap();
            let sas: InnerSas = (*guard).clone();
            let methods = settings.allowed_methods;

            if let Some((sas, content)) = sas.accept(methods) {
                let state: SasState = (&sas).into();

                *guard = sas;

                (
                    Some(match content {
                        OwnedAcceptContent::ToDevice(c) => {
                            let content = AnyToDeviceEventContent::KeyVerificationAccept(c);
                            self.content_to_request(content).into()
                        }
                        OwnedAcceptContent::Room(room_id, content) => RoomMessageRequest {
                            room_id,
                            txn_id: TransactionId::new(),
                            content: AnyMessageLikeEventContent::KeyVerificationAccept(content),
                        }
                        .into(),
                    }),
                    Some(state),
                )
            } else {
                (None, None)
            }
        };

        if let Some(new_state) = state {
            self.update_state(new_state);
        }

        let new_state = self.state_debug();

        trace!(
            flow_id = self.flow_id().as_str(),
            ?old_state,
            ?new_state,
            "Accepted SAS verification"
        );

        request
    }

    /// Confirm the Sas verification.
    ///
    /// This confirms that the short auth strings match on both sides.
    ///
    /// Does nothing if we're not in a state where we can confirm the short auth
    /// string, otherwise returns a `MacEventContent` that needs to be sent to
    /// the server.
    pub async fn confirm(
        &self,
    ) -> Result<(Vec<OutgoingVerificationRequest>, Option<SignatureUploadRequest>), CryptoStoreError>
    {
        let (contents, done, state) = {
            let mut guard = self.inner.lock().unwrap();

            let sas: InnerSas = (*guard).clone();
            let (sas, contents) = sas.confirm();
            let state: SasState = (&sas).into();

            *guard = sas;
            (contents, guard.is_done(), state)
        };

        let mac_requests = contents
            .into_iter()
            .map(|c| match c {
                OutgoingContent::ToDevice(c) => self.content_to_request(c).into(),
                OutgoingContent::Room(r, c) => {
                    RoomMessageRequest { room_id: r, txn_id: TransactionId::new(), content: c }
                        .into()
                }
            })
            .collect::<Vec<_>>();

        if !mac_requests.is_empty() {
            trace!(
                user_id = self.other_user_id().as_str(),
                device_id = self.other_device_id().as_str(),
                "Confirming SAS verification"
            )
        }

        if done {
            match self.mark_as_done().await? {
                VerificationResult::Cancel(c) => {
                    Ok((self.cancel_with_code(c).into_iter().collect(), None))
                }
                VerificationResult::Ok => {
                    self.update_state(state);
                    Ok((mac_requests, None))
                }
                VerificationResult::SignatureUpload(r) => {
                    self.update_state(state);
                    Ok((mac_requests, Some(r)))
                }
            }
        } else {
            self.update_state(state);
            Ok((mac_requests, None))
        }
    }

    pub(crate) async fn mark_as_done(&self) -> Result<VerificationResult, CryptoStoreError> {
        self.identities_being_verified
            .mark_as_done(self.verified_devices().as_deref(), self.verified_identities().as_deref())
            .await
    }

    /// Cancel the verification.
    ///
    /// This cancels the verification with the `CancelCode::User`.
    ///
    /// Returns None if the `Sas` object is already in a canceled state,
    /// otherwise it returns a request that needs to be sent out.
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
        let (content, state) = {
            let mut guard = self.inner.lock().unwrap();

            if let Some(request) = &self.request_handle {
                request.cancel_with_code(&code);
            }

            let sas: InnerSas = (*guard).clone();
            let (sas, content) = sas.cancel(true, code);
            let state: SasState = (&sas).into();
            *guard = sas;

            (
                content.map(|c| match c {
                    OutgoingContent::Room(room_id, content) => {
                        RoomMessageRequest { room_id, txn_id: TransactionId::new(), content }.into()
                    }
                    OutgoingContent::ToDevice(c) => self.content_to_request(c).into(),
                }),
                state,
            )
        };

        self.update_state(state);

        content
    }

    pub(crate) fn cancel_if_timed_out(&self) -> Option<OutgoingVerificationRequest> {
        if self.is_cancelled() || self.is_done() {
            None
        } else if self.timed_out() {
            self.cancel_with_code(CancelCode::Timeout)
        } else {
            None
        }
    }

    /// Has the SAS verification flow timed out.
    pub fn timed_out(&self) -> bool {
        self.inner.lock().unwrap().timed_out()
    }

    /// Are we in a state where we can show the short auth string.
    pub fn can_be_presented(&self) -> bool {
        self.inner.lock().unwrap().can_be_presented()
    }

    /// Is the SAS flow done.
    pub fn is_done(&self) -> bool {
        self.inner.lock().unwrap().is_done()
    }

    /// Is the SAS flow canceled.
    pub fn is_cancelled(&self) -> bool {
        self.inner.lock().unwrap().is_cancelled()
    }

    /// Get the emoji version of the short auth string.
    ///
    /// Returns None if we can't yet present the short auth string, otherwise
    /// seven tuples containing the emoji and description.
    pub fn emoji(&self) -> Option<[Emoji; 7]> {
        self.inner.lock().unwrap().emoji()
    }

    /// Get the index of the emoji representing the short auth string
    ///
    /// Returns None if we can't yet present the short auth string, otherwise
    /// seven u8 numbers in the range from 0 to 63 inclusive which can be
    /// converted to an emoji using the
    /// [relevant spec entry](https://spec.matrix.org/unstable/client-server-api/#sas-method-emoji).
    pub fn emoji_index(&self) -> Option<[u8; 7]> {
        self.inner.lock().unwrap().emoji_index()
    }

    /// Get the decimal version of the short auth string.
    ///
    /// Returns None if we can't yet present the short auth string, otherwise a
    /// tuple containing three 4-digit integers that represent the short auth
    /// string.
    pub fn decimals(&self) -> Option<(u16, u16, u16)> {
        self.inner.lock().unwrap().decimals()
    }

    /// Listen for changes in the SAS verification process.
    ///
    /// The changes are presented as a stream of [`SasState`] values.
    ///
    /// This method can be used to react to changes in the state of the
    /// verification process, or rather the method can be used to handle
    /// each step of the verification process.
    ///
    /// # Flowchart
    ///
    /// The flow of the verification process is pictured bellow. Please note
    /// that the process can be cancelled at each step of the process.
    /// Either side can cancel the process.
    ///
    /// ```text
    ///                ┌───────┐
    ///                │Started│
    ///                └───┬───┘
    ///                    │
    ///               ┌────⌄───┐
    ///               │Accepted│
    ///               └────┬───┘
    ///                    │
    ///            ┌───────⌄──────┐
    ///            │Keys Exchanged│
    ///            └───────┬──────┘
    ///                    │
    ///            ________⌄________
    ///           ╱                 ╲       ┌─────────┐
    ///          ╱   Does the short  ╲______│Cancelled│
    ///          ╲ auth string match ╱ no   └─────────┘
    ///           ╲_________________╱
    ///                    │yes
    ///                    │
    ///               ┌────⌄────┐
    ///               │Confirmed│
    ///               └────┬────┘
    ///                    │
    ///                ┌───⌄───┐
    ///                │  Done │
    ///                └───────┘
    /// ```
    /// # Example
    ///
    /// ```no_run
    /// use futures::stream::{Stream, StreamExt};
    /// use matrix_sdk_crypto::{Sas, SasState};
    ///
    /// # futures::executor::block_on(async {
    /// # let sas: Sas = unimplemented!();
    ///
    /// let mut stream = sas.changes();
    ///
    /// while let Some(state) = stream.next().await {
    ///     match state {
    ///         SasState::KeysExchanged { emojis, decimals: _ } => {
    ///             let emojis =
    ///                 emojis.expect("We only support emoji verification");
    ///             println!("Do these emojis match {emojis:#?}");
    ///
    ///             // Ask the user to confirm or cancel here.
    ///         }
    ///         SasState::Done { .. } => {
    ///             let device = sas.other_device();
    ///
    ///             println!(
    ///                 "Successfully verified device {} {} {:?}",
    ///                 device.user_id(),
    ///                 device.device_id(),
    ///                 device.local_trust_state()
    ///             );
    ///
    ///             break;
    ///         }
    ///         SasState::Cancelled(cancel_info) => {
    ///             println!(
    ///                 "The verification has been cancelled, reason: {}",
    ///                 cancel_info.reason()
    ///             );
    ///             break;
    ///         }
    ///         SasState::Started { .. }
    ///         | SasState::Accepted { .. }
    ///         | SasState::Confirmed => (),
    ///     }
    /// }
    /// # anyhow::Ok(()) });
    /// ```
    pub fn changes(&self) -> impl Stream<Item = SasState> {
        self.state.signal_cloned().to_stream()
    }

    /// Get the current state of the verification process.
    pub fn state(&self) -> SasState {
        self.state.lock_ref().to_owned()
    }

    fn update_state(&self, new_state: SasState) {
        let mut lock = self.state.lock_mut();

        // Only update the state if it differs, this is important so clients don't end
        // up printing the emoji twice. For example, the internal state might
        // change into a MacReceived, because the other side already confirmed,
        // but our side still needs to just show the emoji and wait for
        // confirmation.
        if *lock != new_state {
            *lock = new_state;
        }
    }

    fn state_debug(&self) -> State {
        (&*self.inner.lock().unwrap()).into()
    }

    pub(crate) fn receive_any_event(
        &self,
        sender: &UserId,
        content: &AnyVerificationContent<'_>,
    ) -> Option<(OutgoingContent, Option<RequestInfo>)> {
        let old_state = self.state_debug();

        let (content, state) = {
            let mut guard = self.inner.lock().unwrap();
            let sas: InnerSas = (*guard).clone();
            let (sas, content) = sas.receive_any_event(sender, content);

            let state: SasState = (&sas).into();

            *guard = sas;

            (content, state)
        };

        let new_state = self.state_debug();
        trace!(
            flow_id = self.flow_id().as_str(),
            ?old_state,
            ?new_state,
            "SAS received an event and changed its state"
        );

        self.update_state(state);
        content
    }

    pub(crate) fn mark_request_as_sent(&self, request_id: &TransactionId) {
        let old_state = self.state_debug();

        let state = {
            let mut guard = self.inner.lock().unwrap();

            let sas: InnerSas = (*guard).clone();

            if let Some(sas) = sas.mark_request_as_sent(request_id) {
                let state: SasState = (&sas).into();
                *guard = sas;

                Some(state)
            } else {
                error!(
                    flow_id = self.flow_id().as_str(),
                    %request_id,
                    "Tried to mark a request as sent, but the request ID didn't match"
                );
                None
            }
        };

        let new_state = self.state_debug();

        debug!(
            flow_id = self.flow_id().as_str(),
            ?old_state,
            ?new_state,
            %request_id,
            "Marked a SAS verification HTTP request as sent"
        );

        if let Some(state) = state {
            self.update_state(state);
        }
    }

    pub(crate) fn verified_devices(&self) -> Option<Arc<[ReadOnlyDevice]>> {
        self.inner.lock().unwrap().verified_devices()
    }

    pub(crate) fn verified_identities(&self) -> Option<Arc<[ReadOnlyUserIdentities]>> {
        self.inner.lock().unwrap().verified_identities()
    }

    pub(crate) fn content_to_request(&self, content: AnyToDeviceEventContent) -> ToDeviceRequest {
        ToDeviceRequest::with_id(
            self.other_user_id(),
            self.other_device_id().to_owned(),
            content,
            TransactionId::new(),
        )
    }
}

/// Customize the accept-reply for a verification process
#[derive(Debug)]
pub struct AcceptSettings {
    allowed_methods: Vec<ShortAuthenticationString>,
}

impl Default for AcceptSettings {
    /// All methods are allowed
    fn default() -> Self {
        Self {
            allowed_methods: vec![
                ShortAuthenticationString::Decimal,
                ShortAuthenticationString::Emoji,
            ],
        }
    }
}

impl AcceptSettings {
    /// Create settings restricting the allowed SAS methods
    ///
    /// # Arguments
    ///
    /// * `methods` - The methods this client allows at most
    pub fn with_allowed_methods(methods: Vec<ShortAuthenticationString>) -> Self {
        Self { allowed_methods: methods }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use matrix_sdk_common::locks::Mutex;
    use matrix_sdk_test::async_test;
    use ruma::{device_id, user_id, DeviceId, TransactionId, UserId};

    use super::Sas;
    use crate::{
        olm::PrivateCrossSigningIdentity,
        store::MemoryStore,
        verification::{
            event_enums::{AcceptContent, KeyContent, MacContent, OutgoingContent, StartContent},
            VerificationStore,
        },
        ReadOnlyAccount, ReadOnlyDevice, SasState,
    };

    fn alice_id() -> &'static UserId {
        user_id!("@alice:example.org")
    }

    fn alice_device_id() -> &'static DeviceId {
        device_id!("JLAFKJWSCS")
    }

    fn bob_id() -> &'static UserId {
        user_id!("@bob:example.org")
    }

    fn bob_device_id() -> &'static DeviceId {
        device_id!("BOBDEVCIE")
    }

    #[async_test]
    async fn sas_wrapper_full() {
        let alice = ReadOnlyAccount::new(alice_id(), alice_device_id());
        let alice_device = ReadOnlyDevice::from_account(&alice).await;

        let bob = ReadOnlyAccount::new(bob_id(), bob_device_id());
        let bob_device = ReadOnlyDevice::from_account(&bob).await;

        let alice_store = VerificationStore {
            account: alice.clone(),
            inner: Arc::new(MemoryStore::new()),
            private_identity: Mutex::new(PrivateCrossSigningIdentity::empty(alice_id())).into(),
        };

        let bob_store = MemoryStore::new();
        bob_store.save_devices(vec![alice_device.clone()]).await;

        let bob_store = VerificationStore {
            account: bob.clone(),
            inner: Arc::new(bob_store),
            private_identity: Mutex::new(PrivateCrossSigningIdentity::empty(bob_id())).into(),
        };

        let identities = alice_store.get_identities(bob_device).await.unwrap();

        let (alice, content) = Sas::start(identities, TransactionId::new(), true, None);

        matches!(alice.state(), SasState::Started { .. });

        let flow_id = alice.flow_id().to_owned();
        let content = StartContent::try_from(&content).unwrap();

        let identities = bob_store.get_identities(alice_device).await.unwrap();
        let bob = Sas::from_start_event(flow_id, &content, identities, None, false).unwrap();

        matches!(bob.state(), SasState::Started { .. });

        let request = bob.accept().unwrap();

        let content = OutgoingContent::try_from(request).unwrap();
        let content = AcceptContent::try_from(&content).unwrap();

        let (content, request_info) =
            alice.receive_any_event(bob.user_id(), &content.into()).unwrap();

        matches!(alice.state(), SasState::Accepted { .. });
        matches!(bob.state(), SasState::Accepted { .. });
        assert!(!alice.can_be_presented());
        assert!(!bob.can_be_presented());

        alice.mark_request_as_sent(&request_info.unwrap().request_id);

        let content = KeyContent::try_from(&content).unwrap();
        let (content, request_info) =
            bob.receive_any_event(alice.user_id(), &content.into()).unwrap();
        assert!(!bob.can_be_presented());
        matches!(bob.state(), SasState::Accepted { .. });
        bob.mark_request_as_sent(&request_info.unwrap().request_id);

        assert!(bob.can_be_presented());
        matches!(bob.state(), SasState::KeysExchanged { .. });

        let content = KeyContent::try_from(&content).unwrap();
        alice.receive_any_event(bob.user_id(), &content.into());
        matches!(alice.state(), SasState::KeysExchanged { .. });
        assert!(alice.can_be_presented());

        assert_eq!(alice.emoji().unwrap(), bob.emoji().unwrap());
        assert_eq!(alice.decimals().unwrap(), bob.decimals().unwrap());

        let mut requests = alice.confirm().await.unwrap().0;
        matches!(alice.state(), SasState::Confirmed);
        assert!(requests.len() == 1);
        let request = requests.pop().unwrap();
        let content = OutgoingContent::try_from(request).unwrap();
        let content = MacContent::try_from(&content).unwrap();
        bob.receive_any_event(alice.user_id(), &content.into());
        matches!(bob.state(), SasState::KeysExchanged { .. });

        let mut requests = bob.confirm().await.unwrap().0;
        matches!(bob.state(), SasState::Confirmed);
        assert!(requests.len() == 1);
        let request = requests.pop().unwrap();
        let content = OutgoingContent::try_from(request).unwrap();
        let content = MacContent::try_from(&content).unwrap();
        alice.receive_any_event(bob.user_id(), &content.into());

        assert!(alice.verified_devices().unwrap().contains(alice.other_device()));
        assert!(bob.verified_devices().unwrap().contains(bob.other_device()));
        matches!(alice.state(), SasState::Done { .. });
        matches!(bob.state(), SasState::Done { .. });
    }
}
