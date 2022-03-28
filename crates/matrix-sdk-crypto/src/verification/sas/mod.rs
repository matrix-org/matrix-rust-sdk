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

use inner_sas::InnerSas;
#[cfg(test)]
use matrix_sdk_common::instant::Instant;
use ruma::{
    api::client::keys::upload_signatures::v3::Request as SignatureUploadRequest,
    events::{
        key::verification::{cancel::CancelCode, ShortAuthenticationString},
        AnyMessageEventContent, AnyToDeviceEventContent,
    },
    DeviceId, EventId, RoomId, TransactionId, UserId,
};
use tracing::trace;

use super::{
    event_enums::{AnyVerificationContent, OutgoingContent, OwnedAcceptContent, StartContent},
    requests::RequestHandle,
    CancelInfo, FlowId, IdentitiesBeingVerified, VerificationResult, VerificationStore,
};
use crate::{
    identities::{ReadOnlyDevice, ReadOnlyUserIdentities},
    olm::PrivateCrossSigningIdentity,
    requests::{OutgoingVerificationRequest, RoomMessageRequest},
    store::CryptoStoreError,
    Emoji, ReadOnlyAccount, ReadOnlyOwnUserIdentity, ToDeviceRequest,
};

/// Short authentication string object.
#[derive(Clone, Debug)]
pub struct Sas {
    inner: Arc<Mutex<InnerSas>>,
    account: ReadOnlyAccount,
    identities_being_verified: IdentitiesBeingVerified,
    flow_id: Arc<FlowId>,
    we_started: bool,
    request_handle: Option<RequestHandle>,
}

impl Sas {
    /// Get our own user id.
    pub fn user_id(&self) -> &UserId {
        self.account.user_id()
    }

    /// Get our own device id.
    pub fn device_id(&self) -> &DeviceId {
        self.account.device_id()
    }

    /// Get the user id of the other side.
    pub fn other_user_id(&self) -> &UserId {
        self.identities_being_verified.other_user_id()
    }

    /// Get the device id of the other side.
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
    pub(crate) fn set_creation_time(&self, time: Instant) {
        self.inner.lock().unwrap().set_creation_time(time)
    }

    fn start_helper(
        inner_sas: InnerSas,
        private_identity: PrivateCrossSigningIdentity,
        other_device: ReadOnlyDevice,
        store: VerificationStore,
        other_identity: Option<ReadOnlyUserIdentities>,
        we_started: bool,
        request_handle: Option<RequestHandle>,
    ) -> Sas {
        let flow_id = inner_sas.verification_flow_id();

        let account = store.account.clone();

        let identities = IdentitiesBeingVerified {
            private_identity,
            store,
            device_being_verified: other_device,
            identity_being_verified: other_identity,
        };

        Sas {
            inner: Arc::new(Mutex::new(inner_sas)),
            account,
            identities_being_verified: identities,
            flow_id,
            we_started,
            request_handle,
        }
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
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn start(
        private_identity: PrivateCrossSigningIdentity,
        other_device: ReadOnlyDevice,
        store: VerificationStore,
        own_identity: Option<ReadOnlyOwnUserIdentity>,
        other_identity: Option<ReadOnlyUserIdentities>,
        transaction_id: Option<Box<TransactionId>>,
        we_started: bool,
        request_handle: Option<RequestHandle>,
    ) -> (Sas, OutgoingContent) {
        let (inner, content) = InnerSas::start(
            store.account.clone(),
            other_device.clone(),
            own_identity,
            other_identity.clone(),
            transaction_id,
        );

        (
            Self::start_helper(
                inner,
                private_identity,
                other_device,
                store,
                other_identity,
                we_started,
                request_handle,
            ),
            content,
        )
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
        flow_id: Box<EventId>,
        room_id: Box<RoomId>,
        private_identity: PrivateCrossSigningIdentity,
        other_device: ReadOnlyDevice,
        store: VerificationStore,
        own_identity: Option<ReadOnlyOwnUserIdentity>,
        other_identity: Option<ReadOnlyUserIdentities>,
        we_started: bool,
        request_handle: RequestHandle,
    ) -> (Sas, OutgoingContent) {
        let (inner, content) = InnerSas::start_in_room(
            flow_id,
            room_id,
            store.account.clone(),
            other_device.clone(),
            own_identity,
            other_identity.clone(),
        );

        (
            Self::start_helper(
                inner,
                private_identity,
                other_device,
                store,
                other_identity,
                we_started,
                Some(request_handle),
            ),
            content,
        )
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
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn from_start_event(
        flow_id: FlowId,
        content: &StartContent<'_>,
        store: VerificationStore,
        private_identity: PrivateCrossSigningIdentity,
        other_device: ReadOnlyDevice,
        own_identity: Option<ReadOnlyOwnUserIdentity>,
        other_identity: Option<ReadOnlyUserIdentities>,
        request_handle: Option<RequestHandle>,
        we_started: bool,
    ) -> Result<Sas, OutgoingContent> {
        let inner = InnerSas::from_start_event(
            store.account.clone(),
            other_device.clone(),
            flow_id,
            content,
            own_identity,
            other_identity.clone(),
            request_handle.is_some(),
        )?;

        Ok(Self::start_helper(
            inner,
            private_identity,
            other_device,
            store,
            other_identity,
            we_started,
            request_handle,
        ))
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
        let mut guard = self.inner.lock().unwrap();
        let sas: InnerSas = (*guard).clone();
        let methods = settings.allowed_methods;

        if let Some((sas, content)) = sas.accept(methods) {
            *guard = sas;

            Some(match content {
                OwnedAcceptContent::ToDevice(c) => {
                    let content = AnyToDeviceEventContent::KeyVerificationAccept(c);
                    self.content_to_request(content).into()
                }
                OwnedAcceptContent::Room(room_id, content) => RoomMessageRequest {
                    room_id,
                    txn_id: TransactionId::new(),
                    content: AnyMessageEventContent::KeyVerificationAccept(content),
                }
                .into(),
            })
        } else {
            None
        }
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
        let (contents, done) = {
            let mut guard = self.inner.lock().unwrap();
            let sas: InnerSas = (*guard).clone();
            let (sas, contents) = sas.confirm();

            *guard = sas;
            (contents, guard.is_done())
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
                VerificationResult::Ok => Ok((mac_requests, None)),
                VerificationResult::SignatureUpload(r) => Ok((mac_requests, Some(r))),
            }
        } else {
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
        let mut guard = self.inner.lock().unwrap();

        if let Some(request) = &self.request_handle {
            request.cancel_with_code(&code)
        }

        let sas: InnerSas = (*guard).clone();
        let (sas, content) = sas.cancel(true, code);
        *guard = sas;
        content.map(|c| match c {
            OutgoingContent::Room(room_id, content) => {
                RoomMessageRequest { room_id, txn_id: TransactionId::new(), content }.into()
            }
            OutgoingContent::ToDevice(c) => self.content_to_request(c).into(),
        })
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

    pub(crate) fn receive_any_event(
        &self,
        sender: &UserId,
        content: &AnyVerificationContent<'_>,
    ) -> Option<OutgoingContent> {
        let mut guard = self.inner.lock().unwrap();
        let sas: InnerSas = (*guard).clone();
        let (sas, content) = sas.receive_any_event(sender, content);
        *guard = sas;

        content
    }

    pub(crate) fn verified_devices(&self) -> Option<Arc<[ReadOnlyDevice]>> {
        self.inner.lock().unwrap().verified_devices()
    }

    pub(crate) fn verified_identities(&self) -> Option<Arc<[ReadOnlyUserIdentities]>> {
        self.inner.lock().unwrap().verified_identities()
    }

    pub(crate) fn content_to_request(&self, content: AnyToDeviceEventContent) -> ToDeviceRequest {
        ToDeviceRequest::new(self.other_user_id(), self.other_device_id().to_owned(), content)
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
mod test {
    use std::{convert::TryFrom, sync::Arc};

    use matrix_sdk_test::async_test;
    use ruma::{device_id, user_id, DeviceId, UserId};

    use super::Sas;
    use crate::{
        olm::PrivateCrossSigningIdentity,
        store::MemoryStore,
        verification::{
            event_enums::{AcceptContent, KeyContent, MacContent, OutgoingContent, StartContent},
            VerificationStore,
        },
        ReadOnlyAccount, ReadOnlyDevice,
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

        let alice_store =
            VerificationStore { account: alice.clone(), inner: Arc::new(MemoryStore::new()) };

        let bob_store = MemoryStore::new();
        bob_store.save_devices(vec![alice_device.clone()]).await;

        let bob_store = VerificationStore { account: bob.clone(), inner: Arc::new(bob_store) };

        let (alice, content) = Sas::start(
            PrivateCrossSigningIdentity::empty(alice_id().to_owned()),
            bob_device,
            alice_store,
            None,
            None,
            None,
            true,
            None,
        );

        let flow_id = alice.flow_id().to_owned();
        let content = StartContent::try_from(&content).unwrap();

        let bob = Sas::from_start_event(
            flow_id,
            &content,
            bob_store,
            PrivateCrossSigningIdentity::empty(bob_id().to_owned()),
            alice_device,
            None,
            None,
            None,
            false,
        )
        .unwrap();

        let request = bob.accept().unwrap();
        let content = OutgoingContent::try_from(request).unwrap();
        let content = AcceptContent::try_from(&content).unwrap();

        let content = alice.receive_any_event(bob.user_id(), &content.into()).unwrap();

        assert!(!alice.can_be_presented());
        assert!(!bob.can_be_presented());

        let content = KeyContent::try_from(&content).unwrap();
        let content = bob.receive_any_event(alice.user_id(), &content.into()).unwrap();

        assert!(bob.can_be_presented());

        let content = KeyContent::try_from(&content).unwrap();
        alice.receive_any_event(bob.user_id(), &content.into());
        assert!(alice.can_be_presented());

        assert_eq!(alice.emoji().unwrap(), bob.emoji().unwrap());
        assert_eq!(alice.decimals().unwrap(), bob.decimals().unwrap());

        let mut requests = alice.confirm().await.unwrap().0;
        assert!(requests.len() == 1);
        let request = requests.pop().unwrap();
        let content = OutgoingContent::try_from(request).unwrap();
        let content = MacContent::try_from(&content).unwrap();
        bob.receive_any_event(alice.user_id(), &content.into());

        let mut requests = bob.confirm().await.unwrap().0;
        assert!(requests.len() == 1);
        let request = requests.pop().unwrap();
        let content = OutgoingContent::try_from(request).unwrap();
        let content = MacContent::try_from(&content).unwrap();
        alice.receive_any_event(bob.user_id(), &content.into());

        assert!(alice.verified_devices().unwrap().contains(alice.other_device()));
        assert!(bob.verified_devices().unwrap().contains(bob.other_device()));
    }
}
