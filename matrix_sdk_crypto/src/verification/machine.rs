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

use std::{convert::TryFrom, sync::Arc};

use dashmap::DashMap;
use matrix_sdk_common::{locks::Mutex, uuid::Uuid};
use ruma::{uint, DeviceId, MilliSecondsSinceUnixEpoch, UInt, UserId};
use tracing::{info, trace, warn};

use super::{
    cache::VerificationCache,
    event_enums::{AnyEvent, AnyVerificationContent, OutgoingContent},
    requests::VerificationRequest,
    sas::{content_to_request, Sas},
    FlowId, VerificationResult,
};
use crate::{
    olm::PrivateCrossSigningIdentity,
    requests::OutgoingRequest,
    store::{CryptoStore, CryptoStoreError},
    OutgoingVerificationRequest, ReadOnlyAccount, ReadOnlyDevice, RoomMessageRequest,
};

#[derive(Clone, Debug)]
pub struct VerificationMachine {
    account: ReadOnlyAccount,
    private_identity: Arc<Mutex<PrivateCrossSigningIdentity>>,
    pub(crate) store: Arc<dyn CryptoStore>,
    verifications: VerificationCache,
    requests: Arc<DashMap<String, VerificationRequest>>,
}

impl VerificationMachine {
    pub(crate) fn new(
        account: ReadOnlyAccount,
        identity: Arc<Mutex<PrivateCrossSigningIdentity>>,
        store: Arc<dyn CryptoStore>,
    ) -> Self {
        Self {
            account,
            private_identity: identity,
            store,
            verifications: VerificationCache::new(),
            requests: DashMap::new().into(),
        }
    }

    pub async fn start_sas(
        &self,
        device: ReadOnlyDevice,
    ) -> Result<(Sas, OutgoingVerificationRequest), CryptoStoreError> {
        let identity = self.store.get_user_identity(device.user_id()).await?;
        let private_identity = self.private_identity.lock().await.clone();

        let (sas, content) = Sas::start(
            self.account.clone(),
            private_identity,
            device.clone(),
            self.store.clone(),
            identity,
            None,
        );

        let request = match content {
            OutgoingContent::Room(r, c) => {
                RoomMessageRequest { room_id: r, txn_id: Uuid::new_v4(), content: c }.into()
            }
            OutgoingContent::ToDevice(c) => {
                let request =
                    content_to_request(device.user_id(), device.device_id().to_owned(), c);

                self.verifications.insert_sas(sas.clone());

                request.into()
            }
        };

        Ok((sas, request))
    }

    pub fn get_request(&self, flow_id: impl AsRef<str>) -> Option<VerificationRequest> {
        self.requests.get(flow_id.as_ref()).map(|s| s.clone())
    }

    pub fn get_sas(&self, transaction_id: &str) -> Option<Sas> {
        self.verifications.get_sas(transaction_id)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn is_timestamp_valid(timestamp: &MilliSecondsSinceUnixEpoch) -> bool {
        // The event should be ignored if the event is older than 10 minutes
        let old_timestamp_threshold: UInt = uint!(600);
        // The event should be ignored if the event is 5 minutes or more into the
        // future.
        let timestamp_threshold: UInt = uint!(300);

        let timestamp = timestamp.as_secs();
        let now = MilliSecondsSinceUnixEpoch::now().as_secs();

        !(now.saturating_sub(timestamp) > old_timestamp_threshold
            || timestamp.saturating_sub(now) > timestamp_threshold)
    }

    #[cfg(target_arch = "wasm32")]
    fn is_timestamp_valid(timestamp: &MilliSecondsSinceUnixEpoch) -> bool {
        // TODO the non-wasm method with the same name uses
        // `MilliSecondsSinceUnixEpoch::now()` which internally uses
        // `SystemTime::now()` this panics under WASM, thus we're returning here
        // true for now.
        true
    }

    fn queue_up_content(
        &self,
        recipient: &UserId,
        recipient_device: &DeviceId,
        content: OutgoingContent,
    ) {
        self.verifications.queue_up_content(recipient, recipient_device, content)
    }

    pub fn mark_request_as_sent(&self, uuid: &Uuid) {
        self.verifications.mark_request_as_sent(uuid);
    }

    pub fn outgoing_messages(&self) -> Vec<OutgoingRequest> {
        self.verifications.outgoing_requests()
    }

    pub fn garbage_collect(&self) {
        self.requests.retain(|_, r| !(r.is_done() || r.is_cancelled()));

        for request in self.verifications.garbage_collect() {
            self.verifications.add_request(request)
        }
    }

    async fn mark_sas_as_done(
        &self,
        sas: Sas,
        out_content: Option<OutgoingContent>,
    ) -> Result<(), CryptoStoreError> {
        match sas.mark_as_done().await? {
            VerificationResult::Ok => {
                if let Some(c) = out_content {
                    self.queue_up_content(sas.other_user_id(), sas.other_device_id(), c);
                }
            }
            VerificationResult::Cancel(c) => {
                if let Some(r) = sas.cancel_with_code(c) {
                    self.verifications.add_request(r.into());
                }
            }
            VerificationResult::SignatureUpload(r) => {
                self.verifications.add_request(r.into());

                if let Some(c) = out_content {
                    self.queue_up_content(sas.other_user_id(), sas.other_device_id(), c);
                }
            }
        }

        Ok(())
    }

    pub async fn receive_any_event(
        &self,
        event: impl Into<AnyEvent<'_>>,
    ) -> Result<(), CryptoStoreError> {
        let event = event.into();

        let flow_id = if let Ok(flow_id) = FlowId::try_from(&event) {
            flow_id
        } else {
            // This isn't a verification event, return early.
            return Ok(());
        };

        let flow_id_mismatch = || {
            warn!(
                sender = event.sender().as_str(),
                flow_id = flow_id.as_str(),
                "Received a verification event with a mismatched flow id, \
                  the verification object was created for a in-room \
                  verification but a event was received over to-device \
                  messaging or vice versa"
            );
        };

        if let Some(content) = event.verification_content() {
            match &content {
                AnyVerificationContent::Request(r) => {
                    info!(
                        sender = event.sender().as_str(),
                        from_device = r.from_device().as_str(),
                        "Received a new verification request",
                    );

                    if let Some(timestamp) = event.timestamp() {
                        if Self::is_timestamp_valid(timestamp) {
                            let request = VerificationRequest::from_request(
                                self.verifications.clone(),
                                self.account.clone(),
                                self.private_identity.lock().await.clone(),
                                self.store.clone(),
                                event.sender(),
                                flow_id,
                                r,
                            );

                            self.requests.insert(request.flow_id().as_str().to_owned(), request);
                        } else {
                            trace!(
                                sender = event.sender().as_str(),
                                from_device = r.from_device().as_str(),
                                timestamp =? timestamp,
                                "The received verification request was too old or too far into the future",
                            );
                        }
                    } else {
                        warn!(
                            sender = event.sender().as_str(),
                            from_device = r.from_device().as_str(),
                            "The key verification request didn't contain a valid timestamp"
                        );
                    }
                }
                AnyVerificationContent::Cancel(c) => {
                    if let Some(verification) = self.get_request(flow_id.as_str()) {
                        verification.receive_cancel(event.sender(), c);
                    }

                    if let Some(sas) = self.verifications.get_sas(flow_id.as_str()) {
                        // This won't produce an outgoing content
                        let _ = sas.receive_any_event(event.sender(), &content);
                    }
                }
                AnyVerificationContent::Ready(c) => {
                    if let Some(request) = self.requests.get(flow_id.as_str()) {
                        if request.flow_id() == &flow_id {
                            // TODO remove this unwrap.
                            request.receive_ready(event.sender(), c).unwrap();
                        } else {
                            flow_id_mismatch();
                        }
                    }
                }
                AnyVerificationContent::Start(c) => {
                    if let Some(request) = self.requests.get(flow_id.as_str()) {
                        if request.flow_id() == &flow_id {
                            request.receive_start(event.sender(), c).await?
                        } else {
                            flow_id_mismatch();
                        }
                    } else if let FlowId::ToDevice(_) = flow_id {
                        // TODO remove this soon, this has been deprecated by
                        // MSC3122 https://github.com/matrix-org/matrix-doc/pull/3122
                        if let Some(device) =
                            self.store.get_device(event.sender(), c.from_device()).await?
                        {
                            let private_identity = self.private_identity.lock().await.clone();
                            let identity = self.store.get_user_identity(event.sender()).await?;

                            match Sas::from_start_event(
                                flow_id,
                                c,
                                self.store.clone(),
                                self.account.clone(),
                                private_identity,
                                device,
                                identity,
                                false,
                            ) {
                                Ok(sas) => {
                                    self.verifications.insert_sas(sas);
                                }
                                Err(cancellation) => self.queue_up_content(
                                    event.sender(),
                                    c.from_device(),
                                    cancellation,
                                ),
                            }
                        }
                    }
                }
                AnyVerificationContent::Accept(_) | AnyVerificationContent::Key(_) => {
                    if let Some(sas) = self.verifications.get_sas(flow_id.as_str()) {
                        if sas.flow_id() == &flow_id {
                            if let Some(content) = sas.receive_any_event(event.sender(), &content) {
                                self.queue_up_content(
                                    sas.other_user_id(),
                                    sas.other_device_id(),
                                    content,
                                );
                            }
                        } else {
                            flow_id_mismatch();
                        }
                    }
                }
                AnyVerificationContent::Mac(_) => {
                    if let Some(s) = self.verifications.get_sas(flow_id.as_str()) {
                        if s.flow_id() == &flow_id {
                            let content = s.receive_any_event(event.sender(), &content);

                            if s.is_done() {
                                self.mark_sas_as_done(s, content).await?;
                            }
                        } else {
                            flow_id_mismatch();
                        }
                    }
                }
                AnyVerificationContent::Done(c) => {
                    if let Some(verification) = self.get_request(flow_id.as_str()) {
                        verification.receive_done(event.sender(), c);
                    }

                    if let Some(s) = self.verifications.get_sas(flow_id.as_str()) {
                        let content = s.receive_any_event(event.sender(), &content);

                        if s.is_done() {
                            self.mark_sas_as_done(s, content).await?;
                        }
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod test {

    use std::{
        convert::TryFrom,
        sync::Arc,
        time::{Duration, Instant},
    };

    use matrix_sdk_common::locks::Mutex;
    use ruma::{DeviceId, UserId};

    use super::{Sas, VerificationMachine};
    use crate::{
        olm::PrivateCrossSigningIdentity,
        store::{CryptoStore, MemoryStore},
        verification::{
            event_enums::{AcceptContent, KeyContent, MacContent, OutgoingContent},
            test::wrap_any_to_device_content,
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

    async fn setup_verification_machine() -> (VerificationMachine, Sas) {
        let alice = ReadOnlyAccount::new(&alice_id(), &alice_device_id());
        let bob = ReadOnlyAccount::new(&bob_id(), &bob_device_id());
        let store = MemoryStore::new();
        let bob_store = MemoryStore::new();

        let bob_device = ReadOnlyDevice::from_account(&bob).await;
        let alice_device = ReadOnlyDevice::from_account(&alice).await;

        store.save_devices(vec![bob_device]).await;
        bob_store.save_devices(vec![alice_device.clone()]).await;

        let bob_store: Arc<dyn CryptoStore> = Arc::new(bob_store);
        let identity = Arc::new(Mutex::new(PrivateCrossSigningIdentity::empty(alice_id())));
        let machine = VerificationMachine::new(alice, identity, Arc::new(store));
        let (bob_sas, start_content) = Sas::start(
            bob,
            PrivateCrossSigningIdentity::empty(bob_id()),
            alice_device,
            bob_store,
            None,
            None,
        );

        machine
            .receive_any_event(&wrap_any_to_device_content(bob_sas.user_id(), start_content))
            .await
            .unwrap();

        (machine, bob_sas)
    }

    #[test]
    fn create() {
        let alice = ReadOnlyAccount::new(&alice_id(), &alice_device_id());
        let identity = Arc::new(Mutex::new(PrivateCrossSigningIdentity::empty(alice_id())));
        let store = MemoryStore::new();
        let _ = VerificationMachine::new(alice, identity, Arc::new(store));
    }

    #[tokio::test]
    async fn full_flow() {
        let (alice_machine, bob) = setup_verification_machine().await;

        let alice = alice_machine.get_sas(bob.flow_id().as_str()).unwrap();

        let request = alice.accept().unwrap();

        let content = OutgoingContent::try_from(request).unwrap();
        let content = AcceptContent::try_from(&content).unwrap().into();

        let content = bob.receive_any_event(alice.user_id(), &content).unwrap();

        let event = wrap_any_to_device_content(bob.user_id(), content);

        assert!(alice_machine.verifications.outgoing_requests().is_empty());
        alice_machine.receive_any_event(&event).await.unwrap();
        assert!(!alice_machine.verifications.outgoing_requests().is_empty());

        let request = alice_machine.verifications.outgoing_requests().get(0).cloned().unwrap();
        let txn_id = *request.request_id();
        let content = OutgoingContent::try_from(request).unwrap();
        let content = KeyContent::try_from(&content).unwrap().into();

        alice_machine.mark_request_as_sent(&txn_id);

        assert!(bob.receive_any_event(alice.user_id(), &content).is_none());

        assert!(alice.emoji().is_some());
        assert!(bob.emoji().is_some());
        assert_eq!(alice.emoji(), bob.emoji());

        let request = alice.confirm().await.unwrap().0.unwrap();
        let content = OutgoingContent::try_from(request).unwrap();
        let content = MacContent::try_from(&content).unwrap().into();
        bob.receive_any_event(alice.user_id(), &content);

        let request = bob.confirm().await.unwrap().0.unwrap();
        let content = OutgoingContent::try_from(request).unwrap();
        let content = MacContent::try_from(&content).unwrap().into();
        alice.receive_any_event(bob.user_id(), &content);

        assert!(alice.is_done());
        assert!(bob.is_done());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn timing_out() {
        let (alice_machine, bob) = setup_verification_machine().await;
        let alice = alice_machine.get_sas(bob.flow_id().as_str()).unwrap();

        assert!(!alice.timed_out());
        assert!(alice_machine.verifications.outgoing_requests().is_empty());

        // This line panics on macOS, so we're disabled for now.
        alice.set_creation_time(Instant::now() - Duration::from_secs(60 * 15));
        assert!(alice.timed_out());
        assert!(alice_machine.verifications.outgoing_requests().is_empty());
        alice_machine.garbage_collect();
        assert!(!alice_machine.verifications.outgoing_requests().is_empty());
        alice_machine.garbage_collect();
        assert!(alice_machine.verifications.is_empty());
    }
}
