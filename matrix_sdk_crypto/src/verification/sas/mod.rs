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

mod event_enums;
mod helpers;
mod inner_sas;
mod sas_state;

#[cfg(test)]
use std::time::Instant;

use event_enums::AcceptContent;
use std::sync::{Arc, Mutex};
use tracing::{error, info, trace, warn};

use matrix_sdk_common::{
    api::r0::keys::upload_signatures::Request as SignatureUploadRequest,
    events::{
        key::verification::{
            accept::{AcceptEventContent, AcceptMethod, AcceptToDeviceEventContent},
            cancel::CancelCode,
            ShortAuthenticationString,
        },
        AnyMessageEvent, AnyMessageEventContent, AnyToDeviceEvent, AnyToDeviceEventContent,
    },
    identifiers::{DeviceId, EventId, RoomId, UserId},
    uuid::Uuid,
};

use crate::{
    error::SignatureError,
    identities::{LocalTrust, ReadOnlyDevice, UserIdentities},
    olm::PrivateCrossSigningIdentity,
    requests::{OutgoingVerificationRequest, RoomMessageRequest},
    store::{Changes, CryptoStore, CryptoStoreError, DeviceChanges},
    ReadOnlyAccount, ToDeviceRequest,
};

pub use helpers::content_to_request;
use inner_sas::InnerSas;
pub use sas_state::FlowId;

pub use event_enums::{OutgoingContent, StartContent};

use self::event_enums::CancelContent;

#[derive(Debug)]
/// A result of a verification flow.
#[allow(clippy::large_enum_variant)]
pub enum VerificationResult {
    /// The verification succeeded, nothing needs to be done.
    Ok,
    /// The verification was canceled.
    Cancel(OutgoingVerificationRequest),
    /// The verification is done and has signatures that need to be uploaded.
    SignatureUpload(SignatureUploadRequest),
}

#[derive(Clone, Debug)]
/// Short authentication string object.
pub struct Sas {
    inner: Arc<Mutex<InnerSas>>,
    store: Arc<Box<dyn CryptoStore>>,
    account: ReadOnlyAccount,
    private_identity: PrivateCrossSigningIdentity,
    other_device: ReadOnlyDevice,
    other_identity: Option<UserIdentities>,
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
        self.other_device.user_id()
    }

    /// Get the device id of the other side.
    pub fn other_device_id(&self) -> &DeviceId {
        self.other_device.device_id()
    }

    /// Get the device of the other user.
    pub fn other_device(&self) -> ReadOnlyDevice {
        self.other_device.clone()
    }

    /// Get the unique ID that identifies this SAS verification flow.
    pub fn flow_id(&self) -> &FlowId {
        &self.flow_id
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
        store: Arc<Box<dyn CryptoStore>>,
        other_identity: Option<UserIdentities>,
    ) -> Sas {
        let flow_id = inner_sas.verification_flow_id();

        Sas {
            inner: Arc::new(Mutex::new(inner_sas)),
            account,
            private_identity,
            store,
            other_device,
            flow_id,
            other_identity,
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
        store: Arc<Box<dyn CryptoStore>>,
        other_identity: Option<UserIdentities>,
    ) -> (Sas, StartContent) {
        let (inner, content) = InnerSas::start(
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
    #[allow(dead_code)]
    pub(crate) fn start_in_room(
        flow_id: EventId,
        room_id: RoomId,
        account: ReadOnlyAccount,
        private_identity: PrivateCrossSigningIdentity,
        other_device: ReadOnlyDevice,
        store: Arc<Box<dyn CryptoStore>>,
        other_identity: Option<UserIdentities>,
    ) -> (Sas, StartContent) {
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
    pub(crate) fn from_start_event(
        account: ReadOnlyAccount,
        private_identity: PrivateCrossSigningIdentity,
        other_device: ReadOnlyDevice,
        store: Arc<Box<dyn CryptoStore>>,
        content: impl Into<StartContent>,
        other_identity: Option<UserIdentities>,
    ) -> Result<Sas, OutgoingContent> {
        let inner = InnerSas::from_start_event(
            account.clone(),
            other_device.clone(),
            content,
            other_identity.clone(),
        )?;

        let flow_id = inner.verification_flow_id();

        Ok(Sas {
            inner: Arc::new(Mutex::new(inner)),
            account,
            private_identity,
            other_device,
            other_identity,
            store,
            flow_id,
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
        self.inner
            .lock()
            .unwrap()
            .accept()
            .map(|c| match settings.apply(c) {
                AcceptContent::ToDevice(c) => {
                    let content = AnyToDeviceEventContent::KeyVerificationAccept(c);
                    self.content_to_request(content).into()
                }
                AcceptContent::Room(room_id, content) => RoomMessageRequest {
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
        (
            Option<OutgoingVerificationRequest>,
            Option<SignatureUploadRequest>,
        ),
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
            event_enums::MacContent::ToDevice(c) => self
                .content_to_request(AnyToDeviceEventContent::KeyVerificationMac(c))
                .into(),
            event_enums::MacContent::Room(r, c) => RoomMessageRequest {
                room_id: r,
                txn_id: Uuid::new_v4(),
                content: AnyMessageEventContent::KeyVerificationMac(c),
            }
            .into(),
        });

        if done {
            match self.mark_as_done().await? {
                VerificationResult::Cancel(r) => Ok((Some(r), None)),
                VerificationResult::Ok => Ok((mac_request, None)),
                VerificationResult::SignatureUpload(r) => Ok((mac_request, Some(r))),
            }
        } else {
            Ok((mac_request, None))
        }
    }

    pub(crate) async fn mark_as_done(&self) -> Result<VerificationResult, CryptoStoreError> {
        if let Some(device) = self.mark_device_as_verified().await? {
            let identity = self.mark_identity_as_verified().await?;

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

            let mut changes = Changes {
                devices: DeviceChanges {
                    changed: vec![device],
                    ..Default::default()
                },
                ..Default::default()
            };

            let identity_signature_request = if let Some(i) = identity {
                // We only sign other users here.
                let request = if let Some(i) = i.other() {
                    // Signing can fail if the user signing key is missing.
                    match self.private_identity.sign_user(&i).await {
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
            // Realistically at most one reuqest will be used but let's make
            // this future proof.
            let merged_request = if let Some(mut r) = signature_request {
                if let Some(user_request) = identity_signature_request {
                    r.signed_keys.extend(user_request.signed_keys);
                    Some(r)
                } else {
                    Some(r)
                }
            } else if let Some(r) = identity_signature_request {
                Some(r)
            } else {
                None
            };

            // TODO store the request as well.
            self.store.save_changes(changes).await?;
            Ok(merged_request
                .map(VerificationResult::SignatureUpload)
                .unwrap_or(VerificationResult::Ok))
        } else {
            Ok(self
                .cancel()
                .map(VerificationResult::Cancel)
                .unwrap_or(VerificationResult::Ok))
        }
    }

    pub(crate) async fn mark_identity_as_verified(
        &self,
    ) -> Result<Option<UserIdentities>, CryptoStoreError> {
        // If there wasn't an identity available during the verification flow
        // return early as there's nothing to do.
        if self.other_identity.is_none() {
            return Ok(None);
        }

        // TODO signal an error, e.g. when the identity got deleted so we don't
        // verify/save the device either.
        let identity = self.store.get_user_identity(self.other_user_id()).await?;

        if let Some(identity) = identity {
            if self
                .other_identity
                .as_ref()
                .map_or(false, |i| i.master_key() == identity.master_key())
            {
                if self
                    .verified_identities()
                    .map_or(false, |i| i.contains(&identity))
                {
                    trace!(
                        "Marking user identity of {} as verified.",
                        identity.user_id(),
                    );

                    if let UserIdentities::Own(i) = &identity {
                        i.mark_as_verified();
                    }

                    Ok(Some(identity))
                } else {
                    info!(
                        "The interactive verification process didn't contain a \
                        MAC for the user identity of {} {:?}",
                        identity.user_id(),
                        self.verified_identities(),
                    );

                    Ok(None)
                }
            } else {
                warn!(
                    "The master keys of {} have changed while an interactive \
                      verification was going on, not marking the identity as verified.",
                    identity.user_id(),
                );

                Ok(None)
            }
        } else {
            info!(
                "The identity for {} was deleted while an interactive \
                  verification was going on.",
                self.other_user_id(),
            );
            Ok(None)
        }
    }

    pub(crate) async fn mark_device_as_verified(
        &self,
    ) -> Result<Option<ReadOnlyDevice>, CryptoStoreError> {
        let device = self
            .store
            .get_device(self.other_user_id(), self.other_device_id())
            .await?;

        if let Some(device) = device {
            if device.keys() == self.other_device.keys() {
                if self
                    .verified_devices()
                    .map_or(false, |v| v.contains(&device))
                {
                    trace!(
                        "Marking device {} {} as verified.",
                        device.user_id(),
                        device.device_id()
                    );

                    device.set_trust_state(LocalTrust::Verified);

                    Ok(Some(device))
                } else {
                    info!(
                        "The interactive verification process didn't contain a \
                        MAC for the device {} {}",
                        device.user_id(),
                        device.device_id()
                    );

                    Ok(None)
                }
            } else {
                warn!(
                    "The device keys of {} {} have changed while an interactive \
                      verification was going on, not marking the device as verified.",
                    device.user_id(),
                    device.device_id()
                );
                Ok(None)
            }
        } else {
            let device = self.other_device();

            info!(
                "The device {} {} was deleted while an interactive \
                  verification was going on.",
                device.user_id(),
                device.device_id()
            );
            Ok(None)
        }
    }

    /// Cancel the verification.
    ///
    /// This cancels the verification with the `CancelCode::User`.
    ///
    /// Returns None if the `Sas` object is already in a canceled state,
    /// otherwise it returns a request that needs to be sent out.
    pub fn cancel(&self) -> Option<OutgoingVerificationRequest> {
        let mut guard = self.inner.lock().unwrap();
        let sas: InnerSas = (*guard).clone();
        let (sas, content) = sas.cancel(CancelCode::User);
        *guard = sas;

        content.map(|c| match c {
            CancelContent::Room(room_id, content) => RoomMessageRequest {
                room_id,
                txn_id: Uuid::new_v4(),
                content: AnyMessageEventContent::KeyVerificationCancel(content),
            }
            .into(),
            CancelContent::ToDevice(c) => self
                .content_to_request(AnyToDeviceEventContent::KeyVerificationCancel(c))
                .into(),
        })
    }

    pub(crate) fn cancel_if_timed_out(&self) -> Option<OutgoingVerificationRequest> {
        if self.is_canceled() || self.is_done() {
            None
        } else if self.timed_out() {
            let mut guard = self.inner.lock().unwrap();
            let sas: InnerSas = (*guard).clone();
            let (sas, content) = sas.cancel(CancelCode::Timeout);
            *guard = sas;
            content.map(|c| match c {
                CancelContent::Room(room_id, content) => RoomMessageRequest {
                    room_id,
                    txn_id: Uuid::new_v4(),
                    content: AnyMessageEventContent::KeyVerificationCancel(content),
                }
                .into(),
                CancelContent::ToDevice(c) => self
                    .content_to_request(AnyToDeviceEventContent::KeyVerificationCancel(c))
                    .into(),
            })
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
    pub fn is_canceled(&self) -> bool {
        self.inner.lock().unwrap().is_canceled()
    }

    /// Get the emoji version of the short auth string.
    ///
    /// Returns None if we can't yet present the short auth string, otherwise a
    /// Vec of tuples with the emoji and description.
    pub fn emoji(&self) -> Option<Vec<(&'static str, &'static str)>> {
        self.inner.lock().unwrap().emoji()
    }

    /// Get the decimal version of the short auth string.
    ///
    /// Returns None if we can't yet present the short auth string, otherwise a
    /// tuple containing three 4-digit integers that represent the short auth
    /// string.
    pub fn decimals(&self) -> Option<(u16, u16, u16)> {
        self.inner.lock().unwrap().decimals()
    }

    pub(crate) fn receive_room_event(&self, event: &AnyMessageEvent) -> Option<OutgoingContent> {
        let mut guard = self.inner.lock().unwrap();
        let sas: InnerSas = (*guard).clone();
        let (sas, content) = sas.receive_room_event(event);
        *guard = sas;

        content
    }

    pub(crate) fn receive_event(&self, event: &AnyToDeviceEvent) -> Option<OutgoingContent> {
        let mut guard = self.inner.lock().unwrap();
        let sas: InnerSas = (*guard).clone();
        let (sas, content) = sas.receive_event(event);
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
        content_to_request(self.other_user_id(), self.other_device_id(), content)
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
        Self {
            allowed_methods: methods,
        }
    }

    fn apply(self, mut content: AcceptContent) -> AcceptContent {
        match &mut content {
            AcceptContent::ToDevice(AcceptToDeviceEventContent {
                method: AcceptMethod::MSasV1(c),
                ..
            })
            | AcceptContent::Room(
                _,
                AcceptEventContent {
                    method: AcceptMethod::MSasV1(c),
                    ..
                },
            ) => {
                c.short_authentication_string
                    .retain(|sas| self.allowed_methods.contains(sas));
                content
            }
            _ => content,
        }
    }
}

#[cfg(test)]
mod test {
    use std::{convert::TryFrom, sync::Arc};

    use matrix_sdk_common::identifiers::{DeviceId, UserId};

    use crate::{
        olm::PrivateCrossSigningIdentity,
        store::{CryptoStore, MemoryStore},
        verification::test::{get_content_from_request, wrap_any_to_device_content},
        ReadOnlyAccount, ReadOnlyDevice,
    };

    use super::Sas;

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

        let alice_store: Arc<Box<dyn CryptoStore>> = Arc::new(Box::new(MemoryStore::new()));
        let bob_store = MemoryStore::new();

        bob_store.save_devices(vec![alice_device.clone()]).await;

        let bob_store: Arc<Box<dyn CryptoStore>> = Arc::new(Box::new(bob_store));

        let (alice, content) = Sas::start(
            alice,
            PrivateCrossSigningIdentity::empty(alice_id()),
            bob_device,
            alice_store,
            None,
        );

        let bob = Sas::from_start_event(
            bob,
            PrivateCrossSigningIdentity::empty(bob_id()),
            alice_device,
            bob_store,
            content,
            None,
        )
        .unwrap();
        let event = wrap_any_to_device_content(
            bob.user_id(),
            get_content_from_request(&bob.accept().unwrap()),
        );

        let content = alice.receive_event(&event);

        assert!(!alice.can_be_presented());
        assert!(!bob.can_be_presented());

        let event = wrap_any_to_device_content(alice.user_id(), content.unwrap());
        let event = wrap_any_to_device_content(bob.user_id(), bob.receive_event(&event).unwrap());

        assert!(bob.can_be_presented());

        alice.receive_event(&event);
        assert!(alice.can_be_presented());

        assert_eq!(alice.emoji().unwrap(), bob.emoji().unwrap());
        assert_eq!(alice.decimals().unwrap(), bob.decimals().unwrap());

        let event = wrap_any_to_device_content(
            alice.user_id(),
            get_content_from_request(&alice.confirm().await.unwrap().0.unwrap()),
        );
        bob.receive_event(&event);

        let event = wrap_any_to_device_content(
            bob.user_id(),
            get_content_from_request(&bob.confirm().await.unwrap().0.unwrap()),
        );
        alice.receive_event(&event);

        assert!(alice
            .verified_devices()
            .unwrap()
            .contains(&alice.other_device()));
        assert!(bob
            .verified_devices()
            .unwrap()
            .contains(&bob.other_device()));
    }
}
