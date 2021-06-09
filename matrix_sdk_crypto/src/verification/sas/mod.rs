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
#[cfg(test)]
use std::time::Instant;

pub use helpers::content_to_request;
use inner_sas::InnerSas;
use matrix_sdk_common::uuid::Uuid;
use ruma::{
    api::client::r0::keys::upload_signatures::Request as SignatureUploadRequest,
    events::{
        key::verification::{
            accept::{AcceptEventContent, AcceptMethod, AcceptToDeviceEventContent},
            cancel::CancelCode,
            ShortAuthenticationString,
        },
        AnyMessageEventContent, AnyToDeviceEventContent,
    },
    DeviceId, EventId, RoomId, UserId,
};
use tracing::trace;

use super::{
    event_enums::{AnyVerificationContent, OutgoingContent, OwnedAcceptContent, StartContent},
    FlowId, IdentitiesBeingVerified, VerificationResult,
};
use crate::{
    identities::{ReadOnlyDevice, UserIdentities},
    olm::PrivateCrossSigningIdentity,
    requests::{OutgoingVerificationRequest, RoomMessageRequest},
    store::{CryptoStore, CryptoStoreError},
    ReadOnlyAccount, ToDeviceRequest,
};

/// Short authentication string object.
#[derive(Clone, Debug)]
pub struct Sas {
    inner: Arc<Mutex<InnerSas>>,
    account: ReadOnlyAccount,
    identities_being_verified: IdentitiesBeingVerified,
    flow_id: Arc<FlowId>,
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

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn set_creation_time(&self, time: Instant) {
        self.inner.lock().unwrap().set_creation_time(time)
    }

    fn start_helper(
        inner_sas: InnerSas,
        account: ReadOnlyAccount,
        private_identity: PrivateCrossSigningIdentity,
        other_device: ReadOnlyDevice,
        store: Arc<dyn CryptoStore>,
        other_identity: Option<UserIdentities>,
    ) -> Sas {
        let flow_id = inner_sas.verification_flow_id();

        let identities = IdentitiesBeingVerified {
            private_identity,
            store: store.clone(),
            device_being_verified: other_device,
            identity_being_verified: other_identity,
        };

        Sas {
            inner: Arc::new(Mutex::new(inner_sas)),
            account,
            identities_being_verified: identities,
            flow_id,
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
    pub(crate) fn start(
        account: ReadOnlyAccount,
        private_identity: PrivateCrossSigningIdentity,
        other_device: ReadOnlyDevice,
        store: Arc<dyn CryptoStore>,
        other_identity: Option<UserIdentities>,
        transaction_id: Option<String>,
    ) -> (Sas, OutgoingContent) {
        let (inner, content) = InnerSas::start(
            account.clone(),
            other_device.clone(),
            other_identity.clone(),
            transaction_id,
        );

        (
            Self::start_helper(
                inner,
                account,
                private_identity,
                other_device,
                store,
                other_identity,
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
    pub(crate) fn start_in_room(
        flow_id: EventId,
        room_id: RoomId,
        account: ReadOnlyAccount,
        private_identity: PrivateCrossSigningIdentity,
        other_device: ReadOnlyDevice,
        store: Arc<dyn CryptoStore>,
        other_identity: Option<UserIdentities>,
    ) -> (Sas, OutgoingContent) {
        let (inner, content) = InnerSas::start_in_room(
            flow_id,
            room_id,
            account.clone(),
            other_device.clone(),
            other_identity.clone(),
        );

        (
            Self::start_helper(
                inner,
                account,
                private_identity,
                other_device,
                store,
                other_identity,
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
        content: &StartContent,
        store: Arc<dyn CryptoStore>,
        account: ReadOnlyAccount,
        private_identity: PrivateCrossSigningIdentity,
        other_device: ReadOnlyDevice,
        other_identity: Option<UserIdentities>,
        started_from_request: bool,
    ) -> Result<Sas, OutgoingContent> {
        let inner = InnerSas::from_start_event(
            account.clone(),
            other_device.clone(),
            flow_id,
            content,
            other_identity.clone(),
            started_from_request,
        )?;

        Ok(Self::start_helper(
            inner,
            account,
            private_identity,
            other_device,
            store,
            other_identity,
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
        self.inner.lock().unwrap().accept().map(|c| match settings.apply(c) {
            OwnedAcceptContent::ToDevice(c) => {
                let content = AnyToDeviceEventContent::KeyVerificationAccept(c);
                self.content_to_request(content).into()
            }
            OwnedAcceptContent::Room(room_id, content) => RoomMessageRequest {
                room_id,
                txn_id: Uuid::new_v4(),
                content: AnyMessageEventContent::KeyVerificationAccept(content),
            }
            .into(),
        })
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
    ) -> Result<
        (Option<OutgoingVerificationRequest>, Option<SignatureUploadRequest>),
        CryptoStoreError,
    > {
        let (content, done) = {
            let mut guard = self.inner.lock().unwrap();
            let sas: InnerSas = (*guard).clone();
            let (sas, content) = sas.confirm();

            *guard = sas;
            (content, guard.is_done())
        };

        let mac_request = content.map(|c| match c {
            OutgoingContent::ToDevice(c) => self.content_to_request(c).into(),
            OutgoingContent::Room(r, c) => {
                RoomMessageRequest { room_id: r, txn_id: Uuid::new_v4(), content: c }.into()
            }
        });

        if mac_request.is_some() {
            trace!(
                user_id = self.other_user_id().as_str(),
                device_id = self.other_device_id().as_str(),
                "Confirming SAS verification"
            )
        }

        if done {
            match self.mark_as_done().await? {
                VerificationResult::Cancel(c) => Ok((self.cancel_with_code(c), None)),
                VerificationResult::Ok => Ok((mac_request, None)),
                VerificationResult::SignatureUpload(r) => Ok((mac_request, Some(r))),
            }
        } else {
            Ok((mac_request, None))
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

    pub(crate) fn cancel_with_code(&self, code: CancelCode) -> Option<OutgoingVerificationRequest> {
        let mut guard = self.inner.lock().unwrap();
        let sas: InnerSas = (*guard).clone();
        let (sas, content) = sas.cancel(code);
        *guard = sas;
        content.map(|c| match c {
            OutgoingContent::Room(room_id, content) => {
                RoomMessageRequest { room_id, txn_id: Uuid::new_v4(), content }.into()
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
    pub fn emoji(&self) -> Option<[(&'static str, &'static str); 7]> {
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
        content: &AnyVerificationContent,
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

    pub(crate) fn verified_identities(&self) -> Option<Arc<[UserIdentities]>> {
        self.inner.lock().unwrap().verified_identities()
    }

    pub(crate) fn content_to_request(&self, content: AnyToDeviceEventContent) -> ToDeviceRequest {
        content_to_request(self.other_user_id(), self.other_device_id().to_owned(), content)
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

    fn apply(self, mut content: OwnedAcceptContent) -> OwnedAcceptContent {
        match &mut content {
            OwnedAcceptContent::ToDevice(AcceptToDeviceEventContent {
                method: AcceptMethod::MSasV1(c),
                ..
            })
            | OwnedAcceptContent::Room(
                _,
                AcceptEventContent { method: AcceptMethod::MSasV1(c), .. },
            ) => {
                c.short_authentication_string.retain(|sas| self.allowed_methods.contains(sas));
                content
            }
            _ => content,
        }
    }
}

#[cfg(test)]
mod test {
    use std::{convert::TryFrom, sync::Arc};

    use ruma::{DeviceId, UserId};

    use super::Sas;
    use crate::{
        olm::PrivateCrossSigningIdentity,
        store::{CryptoStore, MemoryStore},
        verification::event_enums::{
            AcceptContent, KeyContent, MacContent, OutgoingContent, StartContent,
        },
        ReadOnlyAccount, ReadOnlyDevice,
    };

    fn alice_id() -> UserId {
        UserId::try_from("@alice:example.org").unwrap()
    }

    fn alice_device_id() -> Box<DeviceId> {
        "JLAFKJWSCS".into()
    }

    fn bob_id() -> UserId {
        UserId::try_from("@bob:example.org").unwrap()
    }

    fn bob_device_id() -> Box<DeviceId> {
        "BOBDEVCIE".into()
    }

    #[tokio::test]
    async fn sas_wrapper_full() {
        let alice = ReadOnlyAccount::new(&alice_id(), &alice_device_id());
        let alice_device = ReadOnlyDevice::from_account(&alice).await;

        let bob = ReadOnlyAccount::new(&bob_id(), &bob_device_id());
        let bob_device = ReadOnlyDevice::from_account(&bob).await;

        let alice_store: Arc<dyn CryptoStore> = Arc::new(MemoryStore::new());
        let bob_store = MemoryStore::new();

        bob_store.save_devices(vec![alice_device.clone()]).await;

        let bob_store: Arc<dyn CryptoStore> = Arc::new(bob_store);

        let (alice, content) = Sas::start(
            alice,
            PrivateCrossSigningIdentity::empty(alice_id()),
            bob_device,
            alice_store,
            None,
            None,
        );

        let flow_id = alice.flow_id().to_owned();
        let content = StartContent::try_from(&content).unwrap();

        let bob = Sas::from_start_event(
            flow_id,
            &content,
            bob_store,
            bob,
            PrivateCrossSigningIdentity::empty(bob_id()),
            alice_device,
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

        let request = alice.confirm().await.unwrap().0.unwrap();
        let content = OutgoingContent::try_from(request).unwrap();
        let content = MacContent::try_from(&content).unwrap();
        bob.receive_any_event(alice.user_id(), &content.into());

        let request = bob.confirm().await.unwrap().0.unwrap();
        let content = OutgoingContent::try_from(request).unwrap();
        let content = MacContent::try_from(&content).unwrap();
        alice.receive_any_event(bob.user_id(), &content.into());

        assert!(alice.verified_devices().unwrap().contains(alice.other_device()));
        assert!(bob.verified_devices().unwrap().contains(bob.other_device()));
    }
}
