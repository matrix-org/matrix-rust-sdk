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

use std::{
    convert::{TryFrom, TryInto},
    sync::Arc,
};

use dashmap::DashMap;
use matrix_sdk_common::locks::Mutex;
use ruma::{
    events::{
        key::verification::VerificationMethod, AnyToDeviceEvent, AnyToDeviceEventContent,
        ToDeviceEvent,
    },
    serde::Raw,
    uint, DeviceId, EventId, MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedUserId, RoomId,
    SecondsSinceUnixEpoch, TransactionId, UInt, UserId,
};
use tracing::{info, trace, warn};

use super::{
    cache::VerificationCache,
    event_enums::{AnyEvent, AnyVerificationContent, OutgoingContent},
    requests::VerificationRequest,
    sas::Sas,
    FlowId, Verification, VerificationResult, VerificationStore,
};
use crate::{
    olm::PrivateCrossSigningIdentity,
    requests::OutgoingRequest,
    store::{CryptoStore, CryptoStoreError},
    OutgoingVerificationRequest, ReadOnlyAccount, ReadOnlyDevice, ReadOnlyUserIdentity,
    RoomMessageRequest, ToDeviceRequest,
};

#[derive(Clone, Debug)]
pub struct VerificationMachine {
    pub(crate) store: VerificationStore,
    verifications: VerificationCache,
    requests: Arc<DashMap<OwnedUserId, DashMap<String, VerificationRequest>>>,
}

impl VerificationMachine {
    pub(crate) fn new(
        account: ReadOnlyAccount,
        identity: Arc<Mutex<PrivateCrossSigningIdentity>>,
        store: Arc<dyn CryptoStore>,
    ) -> Self {
        Self {
            store: VerificationStore { account, private_identity: identity, inner: store },
            verifications: VerificationCache::new(),
            requests: Default::default(),
        }
    }

    pub(crate) fn own_user_id(&self) -> &UserId {
        self.store.account.user_id()
    }

    pub(crate) fn own_device_id(&self) -> &DeviceId {
        self.store.account.device_id()
    }

    pub(crate) async fn request_to_device_verification(
        &self,
        user_id: &UserId,
        recipient_devices: Vec<OwnedDeviceId>,
        methods: Option<Vec<VerificationMethod>>,
    ) -> (VerificationRequest, OutgoingVerificationRequest) {
        let flow_id = FlowId::from(TransactionId::new());

        let verification = VerificationRequest::new(
            self.verifications.clone(),
            self.store.clone(),
            flow_id,
            user_id,
            recipient_devices,
            methods,
        );

        self.insert_request(verification.clone());

        let request = verification.request_to_device();

        (verification, request.into())
    }

    pub async fn request_verification(
        &self,
        identity: &ReadOnlyUserIdentity,
        room_id: &RoomId,
        request_event_id: &EventId,
        methods: Option<Vec<VerificationMethod>>,
    ) -> VerificationRequest {
        let flow_id = FlowId::InRoom(room_id.to_owned(), request_event_id.to_owned());

        let request = VerificationRequest::new(
            self.verifications.clone(),
            self.store.clone(),
            flow_id,
            identity.user_id(),
            vec![],
            methods,
        );

        self.insert_request(request.clone());

        request
    }

    pub async fn start_sas(
        &self,
        device: ReadOnlyDevice,
    ) -> Result<(Sas, OutgoingVerificationRequest), CryptoStoreError> {
        let identities = self.store.get_identities(device.clone()).await?;
        let (sas, content) = Sas::start(identities, None, true, None);

        let request = match content {
            OutgoingContent::Room(r, c) => {
                RoomMessageRequest { room_id: r, txn_id: TransactionId::new(), content: c }.into()
            }
            OutgoingContent::ToDevice(c) => {
                let request =
                    ToDeviceRequest::new(device.user_id(), device.device_id().to_owned(), c);

                self.verifications.insert_sas(sas.clone());

                request.into()
            }
        };

        Ok((sas, request))
    }

    pub fn get_request(
        &self,
        user_id: &UserId,
        flow_id: impl AsRef<str>,
    ) -> Option<VerificationRequest> {
        self.requests.get(user_id).and_then(|v| v.get(flow_id.as_ref()).map(|s| s.clone()))
    }

    pub fn get_requests(&self, user_id: &UserId) -> Vec<VerificationRequest> {
        self.requests
            .get(user_id)
            .map(|v| v.iter().map(|i| i.value().clone()).collect())
            .unwrap_or_default()
    }

    fn insert_request(&self, request: VerificationRequest) {
        self.requests
            .entry(request.other_user().to_owned())
            .or_default()
            .insert(request.flow_id().as_str().to_owned(), request);
    }

    pub fn get_verification(&self, user_id: &UserId, flow_id: &str) -> Option<Verification> {
        self.verifications.get(user_id, flow_id)
    }

    pub fn get_sas(&self, user_id: &UserId, flow_id: &str) -> Option<Sas> {
        self.verifications.get_sas(user_id, flow_id)
    }

    fn is_timestamp_valid(timestamp: MilliSecondsSinceUnixEpoch) -> bool {
        // The event should be ignored if the event is older than 10 minutes
        let old_timestamp_threshold: UInt = uint!(600);
        // The event should be ignored if the event is 5 minutes or more into the
        // future.
        let timestamp_threshold: UInt = uint!(300);

        let timestamp = timestamp.as_secs();
        let now = SecondsSinceUnixEpoch::now().get();

        !(now.saturating_sub(timestamp) > old_timestamp_threshold
            || timestamp.saturating_sub(now) > timestamp_threshold)
    }

    fn queue_up_content(
        &self,
        recipient: &UserId,
        recipient_device: &DeviceId,
        content: OutgoingContent,
    ) {
        self.verifications.queue_up_content(recipient, recipient_device, content)
    }

    pub fn mark_request_as_sent(&self, txn_id: &TransactionId) {
        self.verifications.mark_request_as_sent(txn_id);
    }

    pub fn outgoing_messages(&self) -> Vec<OutgoingRequest> {
        self.verifications.outgoing_requests()
    }

    pub fn garbage_collect(&self) -> Vec<Raw<AnyToDeviceEvent>> {
        let mut events = vec![];

        for user_verification in self.requests.iter() {
            user_verification.retain(|_, v| !(v.is_done() || v.is_cancelled()));
        }
        self.requests.retain(|_, v| !v.is_empty());

        let mut requests: Vec<OutgoingVerificationRequest> = self
            .requests
            .iter()
            .flat_map(|v| {
                let requests: Vec<OutgoingVerificationRequest> =
                    v.value().iter().filter_map(|v| v.cancel_if_timed_out()).collect();
                requests
            })
            .collect();

        requests.extend(self.verifications.garbage_collect().into_iter());

        for request in requests {
            if let Ok(OutgoingContent::ToDevice(AnyToDeviceEventContent::KeyVerificationCancel(
                content,
            ))) = request.clone().try_into()
            {
                let event = ToDeviceEvent { content, sender: self.own_user_id().to_owned() };

                events.push(
                    Raw::new(&event)
                        .expect("Failed to serialize m.key_verification.cancel event")
                        .cast(),
                );
            }

            self.verifications.add_verification_request(request)
        }

        events
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

        #[allow(clippy::question_mark)]
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

        let event_sent_from_us = |event: &AnyEvent<'_>, from_device: &DeviceId| {
            if event.sender() == self.store.account.user_id() {
                from_device == self.store.account.device_id() || event.is_room_event()
            } else {
                false
            }
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
                            if !event_sent_from_us(&event, r.from_device()) {
                                let request = VerificationRequest::from_request(
                                    self.verifications.clone(),
                                    self.store.clone(),
                                    event.sender(),
                                    flow_id,
                                    r,
                                );

                                self.insert_request(request);
                            } else {
                                trace!(
                                    sender = event.sender().as_str(),
                                    from_device = r.from_device().as_str(),
                                    "The received verification request was sent by us, ignoring it",
                                );
                            }
                        } else {
                            trace!(
                                sender = event.sender().as_str(),
                                from_device = r.from_device().as_str(),
                                ?timestamp,
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
                    if let Some(verification) = self.get_request(event.sender(), flow_id.as_str()) {
                        verification.receive_cancel(event.sender(), c);
                    }

                    if let Some(verification) =
                        self.get_verification(event.sender(), flow_id.as_str())
                    {
                        match verification {
                            Verification::SasV1(sas) => {
                                // This won't produce an outgoing content
                                let _ = sas.receive_any_event(event.sender(), &content);
                            }
                            #[cfg(feature = "qrcode")]
                            Verification::QrV1(qr) => qr.receive_cancel(event.sender(), c),
                        }
                    }
                }
                AnyVerificationContent::Ready(c) => {
                    if let Some(request) = self.get_request(event.sender(), flow_id.as_str()) {
                        if request.flow_id() == &flow_id {
                            request.receive_ready(event.sender(), c);
                        } else {
                            flow_id_mismatch();
                        }
                    }
                }
                AnyVerificationContent::Start(c) => {
                    if let Some(request) = self.get_request(event.sender(), flow_id.as_str()) {
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
                            let identities = self.store.get_identities(device).await?;

                            match Sas::from_start_event(flow_id, c, identities, None, false) {
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
                    if let Some(sas) = self.get_sas(event.sender(), flow_id.as_str()) {
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
                    if let Some(s) = self.get_sas(event.sender(), flow_id.as_str()) {
                        if s.flow_id() == &flow_id {
                            let content = s.receive_any_event(event.sender(), &content);

                            if s.is_done() {
                                self.mark_sas_as_done(s, content).await?;
                            } else {
                                // Even if we are not done (yet), there might be content to send
                                // out, e.g. in the case where we are done with our side of the
                                // verification process, but the other side has not yet sent their
                                // "done".
                                if let Some(content) = content {
                                    self.queue_up_content(
                                        s.other_user_id(),
                                        s.other_device_id(),
                                        content,
                                    );
                                }
                            }
                        } else {
                            flow_id_mismatch();
                        }
                    }
                }
                AnyVerificationContent::Done(c) => {
                    if let Some(verification) = self.get_request(event.sender(), flow_id.as_str()) {
                        verification.receive_done(event.sender(), c);
                    }

                    #[allow(clippy::single_match)]
                    match self.get_verification(event.sender(), flow_id.as_str()) {
                        Some(Verification::SasV1(sas)) => {
                            let content = sas.receive_any_event(event.sender(), &content);

                            if sas.is_done() {
                                self.mark_sas_as_done(sas, content).await?;
                            }
                        }
                        #[cfg(feature = "qrcode")]
                        Some(Verification::QrV1(qr)) => {
                            let (cancellation, request) = qr.receive_done(c).await?;

                            if let Some(c) = cancellation {
                                self.verifications.add_request(c.into())
                            }

                            if let Some(s) = request {
                                self.verifications.add_request(s.into())
                            }
                        }
                        None => (),
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::{convert::TryFrom, sync::Arc, time::Duration};

    use matrix_sdk_common::{instant::Instant, locks::Mutex};
    use matrix_sdk_test::async_test;
    use ruma::{device_id, user_id, DeviceId, UserId};

    use super::{Sas, VerificationMachine};
    use crate::{
        olm::PrivateCrossSigningIdentity,
        store::MemoryStore,
        verification::{
            event_enums::{AcceptContent, KeyContent, MacContent, OutgoingContent},
            tests::wrap_any_to_device_content,
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

    async fn setup_verification_machine() -> (VerificationMachine, Sas) {
        let alice = ReadOnlyAccount::new(alice_id(), alice_device_id());
        let bob = ReadOnlyAccount::new(bob_id(), bob_device_id());
        let store = MemoryStore::new();
        let bob_store = MemoryStore::new();

        let bob_device = ReadOnlyDevice::from_account(&bob).await;
        let alice_device = ReadOnlyDevice::from_account(&alice).await;

        store.save_devices(vec![bob_device]).await;
        bob_store.save_devices(vec![alice_device.clone()]).await;

        let identity = Arc::new(Mutex::new(PrivateCrossSigningIdentity::empty(bob_id())));
        let bob_store = VerificationStore {
            account: bob,
            inner: Arc::new(bob_store),
            private_identity: identity,
        };

        let identities = bob_store.get_identities(alice_device).await.unwrap();
        let machine = VerificationMachine::new(
            alice,
            Mutex::new(PrivateCrossSigningIdentity::empty(alice_id())).into(),
            Arc::new(store),
        );
        let (bob_sas, start_content) = Sas::start(identities, None, true, None);

        machine
            .receive_any_event(&wrap_any_to_device_content(bob_sas.user_id(), start_content))
            .await
            .unwrap();

        (machine, bob_sas)
    }

    #[async_test]
    async fn create() {
        let alice = ReadOnlyAccount::new(alice_id(), alice_device_id());
        let identity = Arc::new(Mutex::new(PrivateCrossSigningIdentity::empty(alice_id())));
        let store = MemoryStore::new();
        let _ = VerificationMachine::new(alice, identity, Arc::new(store));
    }

    #[async_test]
    async fn full_flow() {
        let (alice_machine, bob) = setup_verification_machine().await;

        let alice = alice_machine.get_sas(bob.user_id(), bob.flow_id().as_str()).unwrap();

        let request = alice.accept().unwrap();

        let content = OutgoingContent::try_from(request).unwrap();
        let content = AcceptContent::try_from(&content).unwrap().into();

        let content = bob.receive_any_event(alice.user_id(), &content).unwrap();

        let event = wrap_any_to_device_content(bob.user_id(), content);

        assert!(alice_machine.verifications.outgoing_requests().is_empty());
        alice_machine.receive_any_event(&event).await.unwrap();
        assert!(!alice_machine.verifications.outgoing_requests().is_empty());

        let request = alice_machine.verifications.outgoing_requests().get(0).cloned().unwrap();
        let txn_id = request.request_id().to_owned();
        let content = OutgoingContent::try_from(request).unwrap();
        let content = KeyContent::try_from(&content).unwrap().into();

        alice_machine.mark_request_as_sent(&txn_id);

        assert!(bob.receive_any_event(alice.user_id(), &content).is_none());

        assert!(alice.emoji().is_some());
        assert!(bob.emoji().is_some());
        assert_eq!(alice.emoji(), bob.emoji());

        let mut requests = alice.confirm().await.unwrap().0;
        assert!(requests.len() == 1);
        let request = requests.pop().unwrap();
        let content = OutgoingContent::try_from(request).unwrap();
        let content = MacContent::try_from(&content).unwrap().into();
        bob.receive_any_event(alice.user_id(), &content);

        let mut requests = bob.confirm().await.unwrap().0;
        assert!(requests.len() == 1);
        let request = requests.pop().unwrap();
        let content = OutgoingContent::try_from(request).unwrap();
        let content = MacContent::try_from(&content).unwrap().into();
        alice.receive_any_event(bob.user_id(), &content);

        assert!(alice.is_done());
        assert!(bob.is_done());
    }

    #[cfg(not(target_os = "macos"))]
    #[async_test]
    async fn timing_out() {
        let (alice_machine, bob) = setup_verification_machine().await;
        let alice = alice_machine.get_sas(bob.user_id(), bob.flow_id().as_str()).unwrap();

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
