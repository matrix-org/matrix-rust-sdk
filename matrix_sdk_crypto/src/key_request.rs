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

// TODO
//
// Incoming key requests:
// First handle the easy case, if we trust the device and have a session, queue
// up a to-device request.
//
// If we don't have a session, queue up a key claim request, once we get a
// session send out the key if we trust the device.
//
// If we don't trust the device store an object that remembers the request and
// let the users introspect that object.
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::value::to_raw_value;
use std::{collections::BTreeMap, sync::Arc};
use tracing::{error, info, trace};

use matrix_sdk_common::{
    api::r0::to_device::DeviceIdOrAllDevices,
    events::{
        forwarded_room_key::ForwardedRoomKeyEventContent,
        room_key_request::{Action, RequestedKeyInfo, RoomKeyRequestEventContent},
        EventType, ToDeviceEvent,
    },
    identifiers::{DeviceIdBox, EventEncryptionAlgorithm, RoomId, UserId},
    uuid::Uuid,
};

use crate::{
    olm::InboundGroupSession,
    requests::{OutgoingRequest, ToDeviceRequest},
    store::{CryptoStoreError, Store},
};

#[derive(Debug, Clone)]
struct KeyRequestMachine {
    user_id: Arc<UserId>,
    device_id: Arc<DeviceIdBox>,
    store: Store,
    outgoing_to_device_requests: Arc<DashMap<Uuid, OutgoingRequest>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct OugoingKeyInfo {
    request_id: Uuid,
    info: RequestedKeyInfo,
    sent_out: bool,
}

trait Encode {
    fn encode(&self) -> String;
}

impl Encode for RequestedKeyInfo {
    fn encode(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.sender_key, self.room_id, self.session_id, self.algorithm
        )
    }
}

impl Encode for ForwardedRoomKeyEventContent {
    fn encode(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.sender_key, self.room_id, self.session_id, self.algorithm
        )
    }
}

impl KeyRequestMachine {
    pub fn new(user_id: Arc<UserId>, device_id: Arc<DeviceIdBox>, store: Store) -> Self {
        Self {
            user_id,
            device_id,
            store,
            outgoing_to_device_requests: Arc::new(DashMap::new()),
        }
    }

    /// Create a new outgoing key request for the key with the given session id.
    ///
    /// This will queue up a new to-device request and store the key info so
    /// once we receive a forwarded room key we can check that it matches the
    /// key we requested.
    ///
    /// This does nothing if a request for this key has already been sent out.
    ///
    /// # Arguments
    /// * `room_id` - The id of the room where the key is used in.
    ///
    /// * `sender_key` - The curve25519 key of the sender that owns the key.
    ///
    /// * `session_id` - The id that uniquely identifies the session.
    async fn create_outgoing_key_request(
        &self,
        room_id: &RoomId,
        sender_key: &str,
        session_id: &str,
    ) -> Result<(), CryptoStoreError> {
        let key_info = RequestedKeyInfo {
            algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
            room_id: room_id.to_owned(),
            sender_key: sender_key.to_owned(),
            session_id: session_id.to_owned(),
        };

        let id: Option<String> = self.store.get_object(&key_info.encode()).await?;

        if id.is_some() {
            // We already sent out a request for this key, nothing to do.
            return Ok(());
        }

        info!("Creating new outgoing room key request {:#?}", key_info);

        let id = Uuid::new_v4();

        let content = RoomKeyRequestEventContent {
            action: Action::Request,
            request_id: id.to_string(),
            requesting_device_id: (&*self.device_id).clone(),
            body: Some(key_info),
        };

        let mut messages = BTreeMap::new();

        messages
            .entry((&*self.user_id).to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(DeviceIdOrAllDevices::AllDevices, to_raw_value(&content)?);

        let request = OutgoingRequest {
            request_id: id,
            request: Arc::new(
                ToDeviceRequest {
                    event_type: EventType::RoomKeyRequest,
                    txn_id: id,
                    messages,
                }
                .into(),
            ),
        };

        let info = OugoingKeyInfo {
            request_id: id,
            info: content.body.unwrap(),
            sent_out: false,
        };

        self.save_outgoing_key_info(id, info).await?;
        self.outgoing_to_device_requests.insert(id, request);

        Ok(())
    }

    /// Save an outgoing key info.
    async fn save_outgoing_key_info(
        &self,
        id: Uuid,
        info: OugoingKeyInfo,
    ) -> Result<(), CryptoStoreError> {
        // TODO we'll want to use a transaction to store those atomically.
        // To allow this we'll need to rework our cryptostore trait to return
        // a transaction trait and the transaction trait will have the save_X
        // methods.
        let id_string = id.to_string();
        self.store.save_object(&id_string, &info).await?;
        self.store.save_object(&info.info.encode(), &id).await?;

        Ok(())
    }

    /// Get an outgoing key info that matches the forwarded room key content.
    async fn get_key_info(
        &self,
        content: &ForwardedRoomKeyEventContent,
    ) -> Result<Option<OugoingKeyInfo>, CryptoStoreError> {
        let id: Option<Uuid> = self.store.get_object(&content.encode()).await?;

        if let Some(id) = id {
            self.store.get_object(&id.to_string()).await
        } else {
            Ok(None)
        }
    }

    /// Delete the given outgoing key info.
    async fn delete_key_info(&self, info: OugoingKeyInfo) -> Result<(), CryptoStoreError> {
        self.store
            .delete_object(&info.request_id.to_string())
            .await?;
        self.store.delete_object(&info.info.encode()).await?;

        Ok(())
    }

    /// Mark the outgoing request as sent.
    async fn mark_outgoing_request_as_sent(&self, id: Uuid) -> Result<(), CryptoStoreError> {
        self.outgoing_to_device_requests.remove(&id);
        let info: Option<OugoingKeyInfo> = self.store.get_object(&id.to_string()).await?;

        if let Some(mut info) = info {
            trace!("Marking outgoing key request as sent {:#?}", info);
            info.sent_out = true;
            self.save_outgoing_key_info(id, info).await?;
        } else {
            error!("Trying to mark a room key request with the id {} as sent, but no key info was found", id);
        }

        Ok(())
    }

    /// Save an inbound group session we received using a key forward.
    ///
    /// At the same time delete the key info since we received the wanted key.
    async fn save_session(
        &self,
        key_info: OugoingKeyInfo,
        session: InboundGroupSession,
    ) -> Result<(), CryptoStoreError> {
        // TODO perhaps only remove the key info if the first known index is 0.
        self.store.save_inbound_group_sessions(&[session]).await?;
        self.outgoing_to_device_requests
            .remove(&key_info.request_id);
        self.delete_key_info(key_info).await
    }

    /// Receive a forwarded room key event.
    async fn receive_forwarded_room_key(
        &self,
        sender_key: &str,
        event: &mut ToDeviceEvent<ForwardedRoomKeyEventContent>,
    ) -> Result<(), CryptoStoreError> {
        let key_info = self.get_key_info(&event.content).await?;

        if let Some(info) = key_info {
            let session = InboundGroupSession::from_forwarded_key(sender_key, &event.content)?;

            let old_session = self
                .store
                .get_inbound_group_session(
                    session.room_id(),
                    &session.sender_key,
                    session.session_id(),
                )
                .await?;

            // If we have a previous session, check if we have a better version
            // and store the new one if so.
            if let Some(old_session) = old_session {
                let first_old_index = old_session.first_known_index().await;
                let first_index = session.first_known_index().await;

                if first_old_index > first_index {
                    self.save_session(info, session).await?;
                }
            // If we didn't have a previous session, store it.
            } else {
                self.save_session(info, session).await?;
            }
        } else {
            info!(
                "Received a forwarded room key from {}, but no key info was found.",
                event.sender,
            );
        }

        Ok(())
    }

    fn handle_incoming_key_request(&self, event: &ToDeviceEvent<RoomKeyRequestEventContent>) {
        match event.content.action {
            Action::Request => todo!(),
            Action::CancelRequest => todo!(),
        }
    }

    fn cancel_request(&self) {
        todo!()
    }
}

#[cfg(test)]
mod test {
    use matrix_sdk_common::{
        events::{forwarded_room_key::ForwardedRoomKeyEventContent, ToDeviceEvent},
        identifiers::{room_id, user_id, DeviceIdBox, RoomId, UserId},
    };
    use matrix_sdk_test::async_test;
    use std::{convert::TryInto, sync::Arc};

    use crate::{
        olm::Account,
        store::{MemoryStore, Store},
    };

    use super::KeyRequestMachine;

    fn alice_id() -> UserId {
        user_id!("@alice:example.org")
    }

    fn alice_device_id() -> DeviceIdBox {
        "JLAFKJWSCS".into()
    }

    fn room_id() -> RoomId {
        room_id!("!test:example.org")
    }

    fn account() -> Account {
        Account::new(&alice_id(), &alice_device_id())
    }

    fn get_machine() -> KeyRequestMachine {
        let store = Store::new(Box::new(MemoryStore::new()));

        KeyRequestMachine::new(Arc::new(alice_id()), Arc::new(alice_device_id()), store)
    }

    #[test]
    fn create_machine() {
        let machine = get_machine();

        assert!(machine.outgoing_to_device_requests.is_empty());
    }

    #[async_test]
    async fn create_key_request() {
        let machine = get_machine();
        let account = account();

        let (_, session) = account
            .create_group_session_pair(&room_id(), Default::default())
            .await
            .unwrap();

        assert!(machine.outgoing_to_device_requests.is_empty());
        machine
            .create_outgoing_key_request(
                session.room_id(),
                &session.sender_key,
                session.session_id(),
            )
            .await
            .unwrap();
        assert!(!machine.outgoing_to_device_requests.is_empty());
        assert_eq!(machine.outgoing_to_device_requests.len(), 1);

        machine
            .create_outgoing_key_request(
                session.room_id(),
                &session.sender_key,
                session.session_id(),
            )
            .await
            .unwrap();
        assert_eq!(machine.outgoing_to_device_requests.len(), 1);

        let request = machine.outgoing_to_device_requests.iter().next().unwrap();

        let id = request.request_id;
        drop(request);

        machine.mark_outgoing_request_as_sent(id).await.unwrap();
        assert!(machine.outgoing_to_device_requests.is_empty());
    }

    #[async_test]
    async fn receive_forwarded_key() {
        let machine = get_machine();
        let account = account();

        let (_, session) = account
            .create_group_session_pair(&room_id(), Default::default())
            .await
            .unwrap();
        machine
            .create_outgoing_key_request(
                session.room_id(),
                &session.sender_key,
                session.session_id(),
            )
            .await
            .unwrap();

        let request = machine.outgoing_to_device_requests.iter().next().unwrap();
        let id = request.request_id;
        drop(request);

        machine.mark_outgoing_request_as_sent(id).await.unwrap();

        let export = session.export_at_index(10).await.unwrap();

        let content: ForwardedRoomKeyEventContent = export.try_into().unwrap();

        let mut event = ToDeviceEvent {
            sender: alice_id(),
            content,
        };

        assert!(
            machine
                .store
                .get_inbound_group_session(
                    session.room_id(),
                    &session.sender_key,
                    session.session_id(),
                )
                .await
                .unwrap()
                .is_none()
        );

        machine
            .receive_forwarded_room_key(&session.sender_key, &mut event)
            .await
            .unwrap();

        let first_session = machine
            .store
            .get_inbound_group_session(session.room_id(), &session.sender_key, session.session_id())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(first_session.first_known_index().await, 10);

        machine
            .create_outgoing_key_request(
                session.room_id(),
                &session.sender_key,
                session.session_id(),
            )
            .await
            .unwrap();

        let request = machine.outgoing_to_device_requests.iter().next().unwrap();
        let id = request.request_id;
        drop(request);

        machine.mark_outgoing_request_as_sent(id).await.unwrap();

        let export = session.export_at_index(15).await.unwrap();

        let content: ForwardedRoomKeyEventContent = export.try_into().unwrap();

        let mut event = ToDeviceEvent {
            sender: alice_id(),
            content,
        };

        machine
            .receive_forwarded_room_key(&session.sender_key, &mut event)
            .await
            .unwrap();

        let second_session = machine
            .store
            .get_inbound_group_session(session.room_id(), &session.sender_key, session.session_id())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(second_session.first_known_index().await, 10);

        let export = session.export_at_index(0).await.unwrap();

        let content: ForwardedRoomKeyEventContent = export.try_into().unwrap();

        let mut event = ToDeviceEvent {
            sender: alice_id(),
            content,
        };

        machine
            .receive_forwarded_room_key(&session.sender_key, &mut event)
            .await
            .unwrap();
        let second_session = machine
            .store
            .get_inbound_group_session(session.room_id(), &session.sender_key, session.session_id())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(second_session.first_known_index().await, 0);
    }
}
