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
mod sas_state;

use std::sync::{Arc, Mutex};
use tracing::{info, trace, warn};

use matrix_sdk_common::{
    api::r0::to_device::send_event_to_device::IncomingRequest as OwnedToDeviceRequest,
    events::{
        key::verification::{
            accept::AcceptEventContent, cancel::CancelCode, mac::MacEventContent,
            start::StartEventContent,
        },
        AnyToDeviceEvent, AnyToDeviceEventContent, ToDeviceEvent,
    },
    identifiers::{DeviceId, UserId},
    locks::RwLock,
};

use crate::{Account, CryptoStore, CryptoStoreError, Device, TrustState};

pub use helpers::content_to_request;
use sas_state::{
    Accepted, Canceled, Confirmed, Created, Done, KeyReceived, MacReceived, SasState, Started,
};

#[derive(Clone, Debug)]
/// Short authentication string object.
pub struct Sas {
    inner: Arc<Mutex<InnerSas>>,
    store: Arc<RwLock<Box<dyn CryptoStore>>>,
    account: Account,
    other_device: Device,
    flow_id: Arc<String>,
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
    pub fn other_device(&self) -> Device {
        self.other_device.clone()
    }

    /// Get the unique ID that identifies this SAS verification flow.
    pub fn flow_id(&self) -> &str {
        &self.flow_id
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
        account: Account,
        other_device: Device,
        store: Arc<RwLock<Box<dyn CryptoStore>>>,
    ) -> (Sas, StartEventContent) {
        let (inner, content) = InnerSas::start(account.clone(), other_device.clone());
        let flow_id = inner.verification_flow_id();

        let sas = Sas {
            inner: Arc::new(Mutex::new(inner)),
            account,
            store,
            other_device,
            flow_id,
        };

        (sas, content)
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
        account: Account,
        other_device: Device,
        store: Arc<RwLock<Box<dyn CryptoStore>>>,
        event: &ToDeviceEvent<StartEventContent>,
    ) -> Result<Sas, AnyToDeviceEventContent> {
        let inner = InnerSas::from_start_event(account.clone(), other_device.clone(), event)?;
        let flow_id = inner.verification_flow_id();
        Ok(Sas {
            inner: Arc::new(Mutex::new(inner)),
            account,
            other_device,
            store,
            flow_id,
        })
    }

    /// Accept the SAS verification.
    ///
    /// This does nothing if the verification was already accepted, otherwise it
    /// returns an `AcceptEventContent` that needs to be sent out.
    pub fn accept(&self) -> Option<OwnedToDeviceRequest> {
        self.inner.lock().unwrap().accept().map(|c| {
            let content = AnyToDeviceEventContent::KeyVerificationAccept(c);
            self.content_to_request(content)
        })
    }

    /// Confirm the Sas verification.
    ///
    /// This confirms that the short auth strings match on both sides.
    ///
    /// Does nothing if we're not in a state where we can confirm the short auth
    /// string, otherwise returns a `MacEventContent` that needs to be sent to
    /// the server.
    pub async fn confirm(&self) -> Result<Option<OwnedToDeviceRequest>, CryptoStoreError> {
        let (content, done) = {
            let mut guard = self.inner.lock().unwrap();
            let sas: InnerSas = (*guard).clone();
            let (sas, content) = sas.confirm();

            *guard = sas;
            (content, guard.is_done())
        };

        if done && !self.mark_device_as_verified().await? {
            return Ok(self.cancel());
        }

        Ok(content.map(|c| {
            let content = AnyToDeviceEventContent::KeyVerificationMac(c);
            self.content_to_request(content)
        }))
    }

    pub(crate) async fn mark_device_as_verified(&self) -> Result<bool, CryptoStoreError> {
        let device = self
            .store
            .read()
            .await
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

                    device.set_trust_state(TrustState::Verified);
                    self.store.read().await.save_devices(&[device]).await?;

                    Ok(true)
                } else {
                    info!(
                        "The interactive verification process didn't contain a \
                        MAC for the device {} {}",
                        device.user_id(),
                        device.device_id()
                    );

                    Ok(false)
                }
            } else {
                warn!(
                    "The device keys of {} {} have changed while an interactive \
                      verification was going on, not marking the device as verified.",
                    device.user_id(),
                    device.device_id()
                );
                Ok(false)
            }
        } else {
            let device = self.other_device();

            info!(
                "The device {} {} was deleted while a interactive \
                  verification was going on.",
                device.user_id(),
                device.device_id()
            );
            Ok(false)
        }
    }

    /// Cancel the verification.
    ///
    /// This cancels the verification with the `CancelCode::User`.
    ///
    /// Returns None if the `Sas` object is already in a canceled state,
    /// otherwise it returns a request that needs to be sent out.
    pub fn cancel(&self) -> Option<OwnedToDeviceRequest> {
        let mut guard = self.inner.lock().unwrap();
        let sas: InnerSas = (*guard).clone();
        let (sas, content) = sas.cancel(CancelCode::User);
        *guard = sas;

        content.map(|c| self.content_to_request(c))
    }

    pub(crate) fn cancel_if_timed_out(&self) -> Option<OwnedToDeviceRequest> {
        if self.is_canceled() || self.is_done() {
            None
        } else if self.timed_out() {
            let mut guard = self.inner.lock().unwrap();
            let sas: InnerSas = (*guard).clone();
            let (sas, content) = sas.cancel(CancelCode::Timeout);
            *guard = sas;
            content.map(|c| self.content_to_request(c))
        } else {
            None
        }
    }

    /// Has the SAS verification flow timed out.
    pub fn timed_out(&self) -> bool {
        self.inner.lock().unwrap().is_timed_out()
    }

    /// Are we in a state where we can show the short auth string.
    pub fn can_be_presented(&self) -> bool {
        self.inner.lock().unwrap().can_be_presented()
    }

    /// Is the SAS flow done.
    pub fn is_done(&self) -> bool {
        self.inner.lock().unwrap().is_done()
    }

    /// Is the SAS flow done.
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

    pub(crate) fn receive_event(
        &self,
        event: &mut AnyToDeviceEvent,
    ) -> Option<AnyToDeviceEventContent> {
        let mut guard = self.inner.lock().unwrap();
        let sas: InnerSas = (*guard).clone();
        let (sas, content) = sas.receive_event(event);
        *guard = sas;

        content
    }

    pub(crate) fn verified_devices(&self) -> Option<Arc<Vec<Device>>> {
        self.inner.lock().unwrap().verified_devices()
    }

    pub(crate) fn content_to_request(
        &self,
        content: AnyToDeviceEventContent,
    ) -> OwnedToDeviceRequest {
        content_to_request(self.other_user_id(), self.other_device_id(), content)
    }
}

#[derive(Clone, Debug)]
enum InnerSas {
    Created(SasState<Created>),
    Started(SasState<Started>),
    Accepted(SasState<Accepted>),
    KeyRecieved(SasState<KeyReceived>),
    Confirmed(SasState<Confirmed>),
    MacReceived(SasState<MacReceived>),
    Done(SasState<Done>),
    Canceled(SasState<Canceled>),
}

impl InnerSas {
    fn start(account: Account, other_device: Device) -> (InnerSas, StartEventContent) {
        let sas = SasState::<Created>::new(account, other_device);
        let content = sas.as_content();
        (InnerSas::Created(sas), content)
    }

    fn from_start_event(
        account: Account,
        other_device: Device,
        event: &ToDeviceEvent<StartEventContent>,
    ) -> Result<InnerSas, AnyToDeviceEventContent> {
        match SasState::<Started>::from_start_event(account, other_device, event) {
            Ok(s) => Ok(InnerSas::Started(s)),
            Err(s) => Err(s.as_content()),
        }
    }

    fn accept(&self) -> Option<AcceptEventContent> {
        if let InnerSas::Started(s) = self {
            Some(s.as_content())
        } else {
            None
        }
    }

    fn cancel(self, code: CancelCode) -> (InnerSas, Option<AnyToDeviceEventContent>) {
        let sas = match self {
            InnerSas::Created(s) => s.cancel(code),
            InnerSas::Started(s) => s.cancel(code),
            InnerSas::Accepted(s) => s.cancel(code),
            InnerSas::KeyRecieved(s) => s.cancel(code),
            InnerSas::MacReceived(s) => s.cancel(code),
            _ => return (self, None),
        };

        let content = sas.as_content();

        (InnerSas::Canceled(sas), Some(content))
    }

    fn confirm(self) -> (InnerSas, Option<MacEventContent>) {
        match self {
            InnerSas::KeyRecieved(s) => {
                let sas = s.confirm();
                let content = sas.as_content();
                (InnerSas::Confirmed(sas), Some(content))
            }
            InnerSas::MacReceived(s) => {
                let sas = s.confirm();
                let content = sas.as_content();
                (InnerSas::Done(sas), Some(content))
            }
            _ => (self, None),
        }
    }

    fn receive_event(
        self,
        event: &mut AnyToDeviceEvent,
    ) -> (InnerSas, Option<AnyToDeviceEventContent>) {
        match event {
            AnyToDeviceEvent::KeyVerificationAccept(e) => {
                if let InnerSas::Created(s) = self {
                    match s.into_accepted(e) {
                        Ok(s) => {
                            let content = s.as_content();
                            (
                                InnerSas::Accepted(s),
                                Some(AnyToDeviceEventContent::KeyVerificationKey(content)),
                            )
                        }
                        Err(s) => {
                            let content = s.as_content();
                            (InnerSas::Canceled(s), Some(content))
                        }
                    }
                } else {
                    (self, None)
                }
            }
            AnyToDeviceEvent::KeyVerificationKey(e) => match self {
                InnerSas::Accepted(s) => match s.into_key_received(e) {
                    Ok(s) => (InnerSas::KeyRecieved(s), None),
                    Err(s) => {
                        let content = s.as_content();
                        (InnerSas::Canceled(s), Some(content))
                    }
                },
                InnerSas::Started(s) => match s.into_key_received(e) {
                    Ok(s) => {
                        let content = s.as_content();
                        (
                            InnerSas::KeyRecieved(s),
                            Some(AnyToDeviceEventContent::KeyVerificationKey(content)),
                        )
                    }
                    Err(s) => {
                        let content = s.as_content();
                        (InnerSas::Canceled(s), Some(content))
                    }
                },
                _ => (self, None),
            },
            AnyToDeviceEvent::KeyVerificationMac(e) => match self {
                InnerSas::KeyRecieved(s) => match s.into_mac_received(e) {
                    Ok(s) => (InnerSas::MacReceived(s), None),
                    Err(s) => {
                        let content = s.as_content();
                        (InnerSas::Canceled(s), Some(content))
                    }
                },
                InnerSas::Confirmed(s) => match s.into_done(e) {
                    Ok(s) => (InnerSas::Done(s), None),
                    Err(s) => {
                        let content = s.as_content();
                        (InnerSas::Canceled(s), Some(content))
                    }
                },
                _ => (self, None),
            },
            _ => (self, None),
        }
    }

    fn can_be_presented(&self) -> bool {
        match self {
            InnerSas::KeyRecieved(_) => true,
            InnerSas::MacReceived(_) => true,
            _ => false,
        }
    }

    fn is_done(&self) -> bool {
        matches!(self, InnerSas::Done(_))
    }

    fn is_canceled(&self) -> bool {
        matches!(self, InnerSas::Canceled(_))
    }

    fn is_timed_out(&self) -> bool {
        match self {
            InnerSas::Created(s) => s.timed_out(),
            InnerSas::Started(s) => s.timed_out(),
            InnerSas::Canceled(s) => s.timed_out(),
            InnerSas::Accepted(s) => s.timed_out(),
            InnerSas::KeyRecieved(s) => s.timed_out(),
            InnerSas::Confirmed(s) => s.timed_out(),
            InnerSas::MacReceived(s) => s.timed_out(),
            InnerSas::Done(s) => s.timed_out(),
        }
    }

    fn verification_flow_id(&self) -> Arc<String> {
        match self {
            InnerSas::Created(s) => s.verification_flow_id.clone(),
            InnerSas::Started(s) => s.verification_flow_id.clone(),
            InnerSas::Canceled(s) => s.verification_flow_id.clone(),
            InnerSas::Accepted(s) => s.verification_flow_id.clone(),
            InnerSas::KeyRecieved(s) => s.verification_flow_id.clone(),
            InnerSas::Confirmed(s) => s.verification_flow_id.clone(),
            InnerSas::MacReceived(s) => s.verification_flow_id.clone(),
            InnerSas::Done(s) => s.verification_flow_id.clone(),
        }
    }

    fn emoji(&self) -> Option<Vec<(&'static str, &'static str)>> {
        match self {
            InnerSas::KeyRecieved(s) => Some(s.get_emoji()),
            InnerSas::MacReceived(s) => Some(s.get_emoji()),
            _ => None,
        }
    }

    fn decimals(&self) -> Option<(u16, u16, u16)> {
        match self {
            InnerSas::KeyRecieved(s) => Some(s.get_decimal()),
            InnerSas::MacReceived(s) => Some(s.get_decimal()),
            _ => None,
        }
    }

    fn verified_devices(&self) -> Option<Arc<Vec<Device>>> {
        if let InnerSas::Done(s) = self {
            Some(s.verified_devices())
        } else {
            None
        }
    }

    fn verified_master_keys(&self) -> Option<Arc<Vec<String>>> {
        if let InnerSas::Done(s) = self {
            Some(s.verified_master_keys())
        } else {
            None
        }
    }
}

#[cfg(test)]
mod test {
    use std::{convert::TryFrom, sync::Arc};

    use matrix_sdk_common::{
        events::{EventContent, ToDeviceEvent},
        identifiers::{DeviceId, UserId},
        locks::RwLock,
    };

    use crate::{
        store::memorystore::MemoryStore,
        verification::test::{get_content_from_request, wrap_any_to_device_content},
        Account, CryptoStore, Device,
    };

    use super::{Accepted, Created, Sas, SasState, Started};

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

    fn wrap_to_device_event<C: EventContent>(sender: &UserId, content: C) -> ToDeviceEvent<C> {
        ToDeviceEvent {
            sender: sender.clone(),
            content,
        }
    }

    async fn get_sas_pair() -> (SasState<Created>, SasState<Started>) {
        let alice = Account::new(&alice_id(), &alice_device_id());
        let alice_device = Device::from_account(&alice).await;

        let bob = Account::new(&bob_id(), &bob_device_id());
        let bob_device = Device::from_account(&bob).await;

        let alice_sas = SasState::<Created>::new(alice.clone(), bob_device);

        let start_content = alice_sas.as_content();
        let event = wrap_to_device_event(alice_sas.user_id(), start_content);

        let bob_sas = SasState::<Started>::from_start_event(bob.clone(), alice_device, &event);

        (alice_sas, bob_sas.unwrap())
    }

    #[tokio::test]
    async fn create_sas() {
        let (_, _) = get_sas_pair().await;
    }

    #[tokio::test]
    async fn sas_accept() {
        let (alice, bob) = get_sas_pair().await;

        let event = wrap_to_device_event(bob.user_id(), bob.as_content());

        alice.into_accepted(&event).unwrap();
    }

    #[tokio::test]
    async fn sas_key_share() {
        let (alice, bob) = get_sas_pair().await;

        let event = wrap_to_device_event(bob.user_id(), bob.as_content());

        let alice: SasState<Accepted> = alice.into_accepted(&event).unwrap();
        let mut event = wrap_to_device_event(alice.user_id(), alice.as_content());

        let bob = bob.into_key_received(&mut event).unwrap();

        let mut event = wrap_to_device_event(bob.user_id(), bob.as_content());

        let alice = alice.into_key_received(&mut event).unwrap();

        assert_eq!(alice.get_decimal(), bob.get_decimal());
        assert_eq!(alice.get_emoji(), bob.get_emoji());
    }

    #[tokio::test]
    async fn sas_full() {
        let (alice, bob) = get_sas_pair().await;

        let event = wrap_to_device_event(bob.user_id(), bob.as_content());

        let alice: SasState<Accepted> = alice.into_accepted(&event).unwrap();
        let mut event = wrap_to_device_event(alice.user_id(), alice.as_content());

        let bob = bob.into_key_received(&mut event).unwrap();

        let mut event = wrap_to_device_event(bob.user_id(), bob.as_content());

        let alice = alice.into_key_received(&mut event).unwrap();

        assert_eq!(alice.get_decimal(), bob.get_decimal());
        assert_eq!(alice.get_emoji(), bob.get_emoji());

        let bob = bob.confirm();

        let event = wrap_to_device_event(bob.user_id(), bob.as_content());

        let alice = alice.into_mac_received(&event).unwrap();
        assert!(!alice.get_emoji().is_empty());
        let alice = alice.confirm();

        let event = wrap_to_device_event(alice.user_id(), alice.as_content());
        let bob = bob.into_done(&event).unwrap();

        assert!(bob.verified_devices().contains(&bob.other_device()));
        assert!(alice.verified_devices().contains(&alice.other_device()));
    }

    #[tokio::test]
    async fn sas_wrapper_full() {
        let alice = Account::new(&alice_id(), &alice_device_id());
        let alice_device = Device::from_account(&alice).await;

        let bob = Account::new(&bob_id(), &bob_device_id());
        let bob_device = Device::from_account(&bob).await;

        let alice_store: Arc<RwLock<Box<dyn CryptoStore>>> =
            Arc::new(RwLock::new(Box::new(MemoryStore::new())));
        let bob_store: Arc<RwLock<Box<dyn CryptoStore>>> =
            Arc::new(RwLock::new(Box::new(MemoryStore::new())));

        bob_store
            .read()
            .await
            .save_devices(&[alice_device.clone()])
            .await
            .unwrap();

        let (alice, content) = Sas::start(alice, bob_device, alice_store);
        let event = wrap_to_device_event(alice.user_id(), content);

        let bob = Sas::from_start_event(bob, alice_device, bob_store, &event).unwrap();
        let mut event = wrap_any_to_device_content(
            bob.user_id(),
            get_content_from_request(&bob.accept().unwrap()),
        );

        let content = alice.receive_event(&mut event);

        assert!(!alice.can_be_presented());
        assert!(!bob.can_be_presented());

        let mut event = wrap_any_to_device_content(alice.user_id(), content.unwrap());
        let mut event =
            wrap_any_to_device_content(bob.user_id(), bob.receive_event(&mut event).unwrap());

        assert!(bob.can_be_presented());

        alice.receive_event(&mut event);
        assert!(alice.can_be_presented());

        assert_eq!(alice.emoji().unwrap(), bob.emoji().unwrap());
        assert_eq!(alice.decimals().unwrap(), bob.decimals().unwrap());

        let mut event = wrap_any_to_device_content(
            alice.user_id(),
            get_content_from_request(&alice.confirm().await.unwrap().unwrap()),
        );
        bob.receive_event(&mut event);

        let mut event = wrap_any_to_device_content(
            bob.user_id(),
            get_content_from_request(&bob.confirm().await.unwrap().unwrap()),
        );
        alice.receive_event(&mut event);

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
