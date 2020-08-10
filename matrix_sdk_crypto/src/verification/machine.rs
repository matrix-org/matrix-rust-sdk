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

use std::sync::Arc;

use dashmap::DashMap;

use tracing::{trace, warn};

use matrix_sdk_common::{
    api::r0::to_device::send_event_to_device::IncomingRequest as OwnedToDeviceRequest,
    events::{AnyToDeviceEvent, AnyToDeviceEventContent},
    identifiers::{DeviceId, UserId},
    locks::RwLock,
};

use super::sas::{content_to_request, Sas};
use crate::{Account, CryptoStore, CryptoStoreError};

#[derive(Clone, Debug)]
pub struct VerificationMachine {
    account: Account,
    store: Arc<RwLock<Box<dyn CryptoStore>>>,
    verifications: Arc<DashMap<String, Sas>>,
    outgoing_to_device_messages: Arc<DashMap<String, OwnedToDeviceRequest>>,
}

impl VerificationMachine {
    pub(crate) fn new(account: Account, store: Arc<RwLock<Box<dyn CryptoStore>>>) -> Self {
        Self {
            account,
            store,
            verifications: Arc::new(DashMap::new()),
            outgoing_to_device_messages: Arc::new(DashMap::new()),
        }
    }

    pub fn get_sas(&self, transaction_id: &str) -> Option<Sas> {
        #[allow(clippy::map_clone)]
        self.verifications.get(transaction_id).map(|s| s.clone())
    }

    fn queue_up_content(
        &self,
        recipient: &UserId,
        recipient_device: &DeviceId,
        content: AnyToDeviceEventContent,
    ) {
        let request = content_to_request(recipient, recipient_device, content);

        self.outgoing_to_device_messages
            .insert(request.txn_id.clone(), request);
    }

    fn receive_event_helper(&self, sas: &Sas, event: &mut AnyToDeviceEvent) {
        if let Some(c) = sas.receive_event(event) {
            self.queue_up_content(sas.other_user_id(), sas.other_device_id(), c);
        }
    }

    pub fn mark_requests_as_sent(&self, uuid: &str) {
        self.outgoing_to_device_messages.remove(uuid);
    }

    pub fn outgoing_to_device_requests(&self) -> Vec<OwnedToDeviceRequest> {
        #[allow(clippy::map_clone)]
        self.outgoing_to_device_messages
            .iter()
            .map(|r| OwnedToDeviceRequest {
                event_type: r.event_type.clone(),
                txn_id: r.txn_id.clone(),
                messages: r.messages.clone(),
            })
            .collect()
    }

    pub fn garbage_collect(&self) {
        self.verifications
            .retain(|_, s| !(s.is_done() || s.is_canceled()));

        for sas in self.verifications.iter() {
            if let Some(r) = sas.cancel_if_timed_out() {
                self.outgoing_to_device_messages.insert(r.txn_id.clone(), r);
            }
        }
    }

    pub async fn receive_event(
        &self,
        event: &mut AnyToDeviceEvent,
    ) -> Result<(), CryptoStoreError> {
        trace!("Received a key verification event {:?}", event);

        match event {
            AnyToDeviceEvent::KeyVerificationStart(e) => {
                trace!(
                    "Received a m.key.verification start event from {} {}",
                    e.sender,
                    e.content.from_device
                );

                if let Some(d) = self
                    .store
                    .read()
                    .await
                    .get_device(&e.sender, &e.content.from_device)
                    .await?
                {
                    match Sas::from_start_event(self.account.clone(), d, self.store.clone(), e) {
                        Ok(s) => {
                            self.verifications
                                .insert(e.content.transaction_id.clone(), s);
                        }
                        Err(c) => {
                            warn!(
                                "Can't start key verification with {} {}, canceling: {:?}",
                                e.sender, e.content.from_device, c
                            );
                            self.queue_up_content(&e.sender, &e.content.from_device, c)
                        }
                    }
                } else {
                    warn!(
                        "Received a key verification start event from an unknown device {} {}",
                        e.sender, e.content.from_device
                    );
                }
            }
            AnyToDeviceEvent::KeyVerificationCancel(e) => {
                self.verifications.remove(&e.content.transaction_id);
            }
            AnyToDeviceEvent::KeyVerificationAccept(e) => {
                if let Some(s) = self.get_sas(&e.content.transaction_id) {
                    self.receive_event_helper(&s, event)
                };
            }
            AnyToDeviceEvent::KeyVerificationKey(e) => {
                if let Some(s) = self.get_sas(&e.content.transaction_id) {
                    self.receive_event_helper(&s, event)
                };
            }
            AnyToDeviceEvent::KeyVerificationMac(e) => {
                if let Some(s) = self.get_sas(&e.content.transaction_id) {
                    self.receive_event_helper(&s, event);

                    if s.is_done() && !s.mark_device_as_verified().await? {
                        if let Some(r) = s.cancel() {
                            self.outgoing_to_device_messages.insert(r.txn_id.clone(), r);
                        }
                    }
                };
            }
            _ => (),
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

    use matrix_sdk_common::{
        events::AnyToDeviceEventContent,
        identifiers::{DeviceId, UserId},
        locks::RwLock,
    };

    use super::{Sas, VerificationMachine};
    use crate::{
        store::memorystore::MemoryStore,
        verification::test::{get_content_from_request, wrap_any_to_device_content},
        Account, CryptoStore, Device,
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
        let alice = Account::new(&alice_id(), &alice_device_id());
        let bob = Account::new(&bob_id(), &bob_device_id());
        let store = MemoryStore::new();
        let bob_store: Arc<RwLock<Box<dyn CryptoStore>>> =
            Arc::new(RwLock::new(Box::new(MemoryStore::new())));

        let bob_device = Device::from_account(&bob).await;
        let alice_device = Device::from_account(&alice).await;

        store.save_devices(&[bob_device]).await.unwrap();
        bob_store
            .read()
            .await
            .save_devices(&[alice_device.clone()])
            .await
            .unwrap();

        let machine = VerificationMachine::new(alice, Arc::new(RwLock::new(Box::new(store))));
        let (bob_sas, start_content) = Sas::start(bob, alice_device, bob_store);
        machine
            .receive_event(&mut wrap_any_to_device_content(
                bob_sas.user_id(),
                AnyToDeviceEventContent::KeyVerificationStart(start_content),
            ))
            .await
            .unwrap();

        (machine, bob_sas)
    }

    #[test]
    fn create() {
        let alice = Account::new(&alice_id(), &alice_device_id());
        let store = MemoryStore::new();
        let _ = VerificationMachine::new(alice, Arc::new(RwLock::new(Box::new(store))));
    }

    #[tokio::test]
    async fn full_flow() {
        let (alice_machine, bob) = setup_verification_machine().await;

        let alice = alice_machine.get_sas(bob.flow_id()).unwrap();

        let mut event = alice
            .accept()
            .map(|c| wrap_any_to_device_content(alice.user_id(), get_content_from_request(&c)))
            .unwrap();

        let mut event = bob
            .receive_event(&mut event)
            .map(|c| wrap_any_to_device_content(bob.user_id(), c))
            .unwrap();

        assert!(alice_machine.outgoing_to_device_messages.is_empty());
        alice_machine.receive_event(&mut event).await.unwrap();
        assert!(!alice_machine.outgoing_to_device_messages.is_empty());

        let request = alice_machine
            .outgoing_to_device_messages
            .iter()
            .next()
            .unwrap();

        let txn_id = request.txn_id.clone();

        let mut event =
            wrap_any_to_device_content(alice.user_id(), get_content_from_request(&request));
        drop(request);
        alice_machine.mark_requests_as_sent(&txn_id);

        assert!(bob.receive_event(&mut event).is_none());

        assert!(alice.emoji().is_some());
        assert!(bob.emoji().is_some());

        assert_eq!(alice.emoji(), bob.emoji());

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

        assert!(alice.is_done());
        assert!(bob.is_done());
    }

    #[tokio::test]
    async fn timing_out() {
        let (alice_machine, bob) = setup_verification_machine().await;
        let alice = alice_machine.get_sas(bob.flow_id()).unwrap();

        assert!(!alice.timed_out());
        assert!(alice_machine.outgoing_to_device_messages.is_empty());

        alice.set_creation_time(
            Instant::now()
                .checked_sub(Duration::from_secs(60 * 15))
                .unwrap(),
        );
        assert!(alice.timed_out());
        assert!(alice_machine.outgoing_to_device_messages.is_empty());
        alice_machine.garbage_collect();
        assert!(!alice_machine.outgoing_to_device_messages.is_empty());
        alice_machine.garbage_collect();
        assert!(alice_machine.verifications.is_empty());
    }
}
