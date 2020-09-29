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

#![allow(dead_code)]

use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::{value::to_raw_value, Value};
use std::{collections::BTreeMap, convert::TryInto, ops::Deref, sync::Arc};
use thiserror::Error;
use tracing::{info, instrument, trace, warn};

use matrix_sdk_common::{
    api::r0::to_device::DeviceIdOrAllDevices,
    events::{
        forwarded_room_key::ForwardedRoomKeyEventContent,
        room::encrypted::EncryptedEventContent,
        room_key_request::{Action, RequestedKeyInfo, RoomKeyRequestEventContent},
        AnyToDeviceEvent, EventType, ToDeviceEvent,
    },
    identifiers::{DeviceIdBox, EventEncryptionAlgorithm, RoomId, UserId},
    uuid::Uuid,
    Raw,
};

use crate::{
    error::OlmResult,
    identities::{OwnUserIdentity, ReadOnlyDevice, UserIdentities},
    olm::{InboundGroupSession, OutboundGroupSession},
    requests::{OutgoingRequest, ToDeviceRequest},
    store::{CryptoStoreError, Store},
};

struct Device {
    inner: ReadOnlyDevice,
    store: Store,
    own_identity: Option<OwnUserIdentity>,
    device_owner_identity: Option<UserIdentities>,
}

impl Device {
    fn trust_state(&self) -> bool {
        self.inner
            .trust_state(&self.own_identity, &self.device_owner_identity)
    }

    pub(crate) async fn encrypt(
        &self,
        event_type: EventType,
        content: Value,
    ) -> OlmResult<EncryptedEventContent> {
        self.inner
            .encrypt(self.store.clone(), event_type, content)
            .await
    }
}

impl Deref for Device {
    type Target = ReadOnlyDevice;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

/// An error describing why a key share request won't be honored.
#[derive(Debug, Clone, Error, PartialEq)]
pub enum KeyshareDecision {
    /// The key request is from a device that we don't own, we're only sharing
    /// sessions that we know the requesting device already was supposed to get.
    #[error("can't find an active outbound group session")]
    MissingOutboundSession,
    /// The key request is from a device that we don't own and the device wasn't
    /// meant to receive the session in the original key share.
    #[error("outbound session wasn't shared with the requesting device")]
    OutboundSessionNotShared,
    /// The key request is from a device we own, yet we don't trust it.
    #[error("requesting device isn't trusted")]
    UntrustedDevice,
}

#[derive(Debug, Clone)]
pub(crate) struct KeyRequestMachine {
    user_id: Arc<UserId>,
    device_id: Arc<DeviceIdBox>,
    store: Store,
    outbound_group_sessions: Arc<DashMap<RoomId, OutboundGroupSession>>,
    outgoing_to_device_requests: Arc<DashMap<Uuid, OutgoingRequest>>,
    incoming_key_requests:
        Arc<DashMap<(UserId, DeviceIdBox, String), ToDeviceEvent<RoomKeyRequestEventContent>>>,
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

fn wrap_key_request_content(
    recipient: UserId,
    id: Uuid,
    content: &RoomKeyRequestEventContent,
) -> Result<OutgoingRequest, serde_json::Error> {
    let mut messages = BTreeMap::new();

    messages
        .entry(recipient)
        .or_insert_with(BTreeMap::new)
        .insert(DeviceIdOrAllDevices::AllDevices, to_raw_value(content)?);

    Ok(OutgoingRequest {
        request_id: id,
        request: Arc::new(
            ToDeviceRequest {
                event_type: EventType::RoomKeyRequest,
                txn_id: id,
                messages,
            }
            .into(),
        ),
    })
}

impl KeyRequestMachine {
    pub fn new(
        user_id: Arc<UserId>,
        device_id: Arc<DeviceIdBox>,
        store: Store,
        outbound_group_sessions: Arc<DashMap<RoomId, OutboundGroupSession>>,
    ) -> Self {
        Self {
            user_id,
            device_id,
            store,
            outbound_group_sessions,
            outgoing_to_device_requests: Arc::new(DashMap::new()),
            incoming_key_requests: Arc::new(DashMap::new()),
        }
    }

    /// Our own user id.
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    pub fn outgoing_to_device_requests(&self) -> Vec<OutgoingRequest> {
        #[allow(clippy::map_clone)]
        self.outgoing_to_device_requests
            .iter()
            .map(|r| (*r).clone())
            .collect()
    }

    /// Receive a room key request event.
    pub fn receive_incoming_key_request(&self, event: &ToDeviceEvent<RoomKeyRequestEventContent>) {
        let sender = event.sender.clone();
        let device_id = event.content.requesting_device_id.clone();
        let request_id = event.content.request_id.clone();
        self.incoming_key_requests
            .insert((sender, device_id, request_id), event.clone());
    }

    /// Handle all the incoming key requests that are queued up and empty our
    /// key request queue.
    pub async fn collect_incoming_key_requests(&self) -> Result<(), CryptoStoreError> {
        for item in self.incoming_key_requests.iter() {
            let event = item.value();
            self.handle_key_request(event).await?;
        }

        self.incoming_key_requests.clear();

        Ok(())
    }

    /// Handle a single incoming key request.
    #[instrument]
    async fn handle_key_request(
        &self,
        event: &ToDeviceEvent<RoomKeyRequestEventContent>,
    ) -> Result<(), CryptoStoreError> {
        let key_info = match event.content.action {
            Action::Request => {
                if let Some(info) = &event.content.body {
                    info
                } else {
                    warn!(
                        "Received a key request from {} {} with a request \
                          action, but no key info was found",
                        event.sender, event.content.requesting_device_id
                    );
                    return Ok(());
                }
            }
            // We ignore cancellations here since there's nothing to serve.
            Action::CancelRequest => return Ok(()),
        };

        let session = self
            .store
            .get_inbound_group_session(
                &key_info.room_id,
                &key_info.sender_key,
                &key_info.session_id,
            )
            .await?;

        let session = if let Some(s) = session {
            s
        } else {
            info!(
                "Received a key request from {} {} for an unknown inbound group session {}.",
                &event.sender, &event.content.requesting_device_id, &key_info.session_id
            );
            return Ok(());
        };

        let device = self
            .store
            .get_device_and_users(&event.sender, &event.content.requesting_device_id)
            .await?
            .map(|(d, o, u)| Device {
                inner: d,
                store: self.store.clone(),
                own_identity: o,
                device_owner_identity: u,
            });

        if let Some(device) = device {
            // TODO get the matching outbound session.
            if let Err(e) = self.should_share_session(
                &device,
                self.outbound_group_sessions
                    .get(&key_info.room_id)
                    .as_deref(),
            ) {
                info!(
                    "Received a key request from {} {} that we won't serve: {}",
                    device.user_id(),
                    device.device_id(),
                    e
                );
            } else {
                info!(
                    "Serving a key request for {} from {} {}.",
                    key_info.session_id,
                    device.user_id(),
                    device.device_id()
                );

                self.share_session(session, device).await;
            }
        } else {
            warn!(
                "Received a key request from an unknown device {} {}.",
                &event.sender, &event.content.requesting_device_id
            );
            self.store.update_tracked_user(&event.sender, true).await?;
        }

        Ok(())
    }

    async fn share_session(&self, session: InboundGroupSession, device: Device) {
        let export = session.export().await;
        let content: ForwardedRoomKeyEventContent = export.try_into().unwrap();
        let content = serde_json::to_value(content).unwrap();
        let content = device
            .encrypt(EventType::ForwardedRoomKey, content)
            .await
            .unwrap();

        let id = Uuid::new_v4();

        let mut messages = BTreeMap::new();

        messages
            .entry(device.user_id().to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(
                DeviceIdOrAllDevices::DeviceId(device.device_id().into()),
                to_raw_value(&content).unwrap(),
            );

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

        self.outgoing_to_device_requests.insert(id, request);
    }

    /// Check if it's ok to share a session with the given device.
    ///
    /// The logic for this currently is as follows:
    ///
    /// * Share any session with our own devices as long as they are trusted.
    ///
    /// * Share with devices of other users only sessions that were meant to be
    /// shared with them in the first place, in other words if an outbound
    /// session still exists and the session was shared with that user/device
    /// pair.
    ///
    /// # Arguments
    ///
    /// * `device` - The device that is requesting a session from us.
    ///
    /// * `outbound_session` - If one still exists, the matching outbound
    /// session that was used to create the inbound session that is being
    /// requested.
    fn should_share_session(
        &self,
        device: &Device,
        outbound_session: Option<&OutboundGroupSession>,
    ) -> Result<(), KeyshareDecision> {
        if device.user_id() == self.user_id() {
            if device.trust_state() {
                Ok(())
            } else {
                Err(KeyshareDecision::UntrustedDevice)
            }
        } else if let Some(outbound) = outbound_session {
            if outbound
                .shared_with()
                .contains(&(device.user_id().to_owned(), device.device_id().to_owned()))
            {
                Ok(())
            } else {
                Err(KeyshareDecision::OutboundSessionNotShared)
            }
        } else {
            Err(KeyshareDecision::MissingOutboundSession)
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
    pub async fn create_outgoing_key_request(
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

        let request = wrap_key_request_content(self.user_id().clone(), id, &content)?;

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
    async fn delete_key_info(&self, info: &OugoingKeyInfo) -> Result<(), CryptoStoreError> {
        self.store
            .delete_object(&info.request_id.to_string())
            .await?;
        self.store.delete_object(&info.info.encode()).await?;

        Ok(())
    }

    /// Mark the outgoing request as sent.
    pub async fn mark_outgoing_request_as_sent(&self, id: &Uuid) -> Result<(), CryptoStoreError> {
        self.outgoing_to_device_requests.remove(id);
        let info: Option<OugoingKeyInfo> = self.store.get_object(&id.to_string()).await?;

        if let Some(mut info) = info {
            trace!("Marking outgoing key request as sent {:#?}", info);
            info.sent_out = true;
            self.save_outgoing_key_info(*id, info).await?;
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
        trace!(
            "Successfully received a forwarded room key for {:#?}",
            key_info
        );

        self.store.save_inbound_group_sessions(&[session]).await?;
        self.outgoing_to_device_requests
            .remove(&key_info.request_id);
        self.delete_key_info(&key_info).await?;

        let content = RoomKeyRequestEventContent {
            action: Action::CancelRequest,
            request_id: key_info.request_id.to_string(),
            requesting_device_id: (&*self.device_id).clone(),
            body: None,
        };

        let id = Uuid::new_v4();

        let request = wrap_key_request_content(self.user_id().clone(), id, &content)?;

        self.outgoing_to_device_requests.insert(id, request);

        Ok(())
    }

    /// Receive a forwarded room key event.
    pub async fn receive_forwarded_room_key(
        &self,
        sender_key: &str,
        event: &mut ToDeviceEvent<ForwardedRoomKeyEventContent>,
    ) -> Result<Option<Raw<AnyToDeviceEvent>>, CryptoStoreError> {
        let key_info = self.get_key_info(&event.content).await?;

        if let Some(info) = key_info {
            let session = InboundGroupSession::from_forwarded_key(sender_key, &mut event.content)?;

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

            Ok(Some(Raw::from(AnyToDeviceEvent::ForwardedRoomKey(
                event.clone(),
            ))))
        } else {
            info!(
                "Received a forwarded room key from {}, but no key info was found.",
                event.sender,
            );
            Ok(None)
        }
    }
}

#[cfg(test)]
mod test {
    use dashmap::DashMap;
    use matrix_sdk_common::{
        api::r0::to_device::DeviceIdOrAllDevices,
        events::{
            forwarded_room_key::ForwardedRoomKeyEventContent,
            room::encrypted::EncryptedEventContent, room_key_request::RoomKeyRequestEventContent,
            ToDeviceEvent,
        },
        identifiers::{room_id, user_id, DeviceIdBox, RoomId, UserId},
    };
    use matrix_sdk_test::async_test;
    use std::{convert::TryInto, sync::Arc};

    use crate::{
        identities::{LocalTrust, ReadOnlyDevice},
        olm::Account,
        store::{MemoryStore, Store},
    };

    use super::{Device, KeyRequestMachine, KeyshareDecision};

    fn alice_id() -> UserId {
        user_id!("@alice:example.org")
    }

    fn alice_device_id() -> DeviceIdBox {
        "JLAFKJWSCS".into()
    }

    fn bob_id() -> UserId {
        user_id!("@bob:example.org")
    }

    fn bob_device_id() -> DeviceIdBox {
        "ILMLKASTES".into()
    }

    fn room_id() -> RoomId {
        room_id!("!test:example.org")
    }

    fn account() -> Account {
        Account::new(&alice_id(), &alice_device_id())
    }

    fn bob_account() -> Account {
        Account::new(&bob_id(), &bob_device_id())
    }

    fn bob_machine() -> KeyRequestMachine {
        let user_id = Arc::new(bob_id());
        let store = Store::new(user_id.clone(), Box::new(MemoryStore::new()));

        KeyRequestMachine::new(
            user_id,
            Arc::new(bob_device_id()),
            store,
            Arc::new(DashMap::new()),
        )
    }

    fn get_machine() -> KeyRequestMachine {
        let user_id = Arc::new(alice_id());
        let store = Store::new(user_id.clone(), Box::new(MemoryStore::new()));

        KeyRequestMachine::new(
            user_id,
            Arc::new(alice_device_id()),
            store,
            Arc::new(DashMap::new()),
        )
    }

    #[test]
    fn create_machine() {
        let machine = get_machine();

        assert!(machine.outgoing_to_device_requests().is_empty());
    }

    #[async_test]
    async fn create_key_request() {
        let machine = get_machine();
        let account = account();

        let (_, session) = account
            .create_group_session_pair_with_defaults(&room_id())
            .await
            .unwrap();

        assert!(machine.outgoing_to_device_requests().is_empty());
        machine
            .create_outgoing_key_request(
                session.room_id(),
                &session.sender_key,
                session.session_id(),
            )
            .await
            .unwrap();
        assert!(!machine.outgoing_to_device_requests().is_empty());
        assert_eq!(machine.outgoing_to_device_requests().len(), 1);

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

        machine.mark_outgoing_request_as_sent(&id).await.unwrap();
        assert!(machine.outgoing_to_device_requests.is_empty());
    }

    #[async_test]
    async fn receive_forwarded_key() {
        let machine = get_machine();
        let account = account();

        let (_, session) = account
            .create_group_session_pair_with_defaults(&room_id())
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

        machine.mark_outgoing_request_as_sent(&id).await.unwrap();

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

        // Get the cancel request.
        let request = machine.outgoing_to_device_requests.iter().next().unwrap();
        let id = request.request_id;
        drop(request);
        machine.mark_outgoing_request_as_sent(&id).await.unwrap();

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

        machine.mark_outgoing_request_as_sent(&id).await.unwrap();

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

    #[async_test]
    async fn should_share_key_test() {
        let machine = get_machine();
        let account = account();

        let own_device = Device {
            store: machine.store.clone(),
            inner: ReadOnlyDevice::from_account(&account).await,
            own_identity: None,
            device_owner_identity: None,
        };

        // We don't share keys with untrusted devices.
        assert_eq!(
            machine
                .should_share_session(&own_device, None)
                .expect_err("Should not share with untrusted"),
            KeyshareDecision::UntrustedDevice
        );
        own_device.set_trust_state(LocalTrust::Verified);
        // Now we do want to share the keys.
        assert!(machine.should_share_session(&own_device, None).is_ok());

        let bob_device = Device {
            store: machine.store.clone(),
            inner: ReadOnlyDevice::from_account(&bob_account()).await,
            own_identity: None,
            device_owner_identity: None,
        };

        // We don't share sessions with other user's devices if no outbound
        // session was provided.
        assert_eq!(
            machine
                .should_share_session(&bob_device, None)
                .expect_err("Should not share with other."),
            KeyshareDecision::MissingOutboundSession
        );

        let (session, _) = account
            .create_group_session_pair_with_defaults(&room_id())
            .await
            .unwrap();

        // We don't share sessions with other user's devices if the session
        // wasn't shared in the first place.
        assert_eq!(
            machine
                .should_share_session(&bob_device, Some(&session))
                .expect_err("Should not share with other unless shared."),
            KeyshareDecision::OutboundSessionNotShared
        );

        bob_device.set_trust_state(LocalTrust::Verified);

        // We don't share sessions with other user's devices if the session
        // wasn't shared in the first place even if the device is trusted.
        assert_eq!(
            machine
                .should_share_session(&bob_device, Some(&session))
                .expect_err("Should not share with other unless shared."),
            KeyshareDecision::OutboundSessionNotShared
        );

        session.mark_shared_with(bob_device.user_id(), bob_device.device_id());
        assert!(machine
            .should_share_session(&bob_device, Some(&session))
            .is_ok());
    }

    #[async_test]
    async fn key_share_cycle() {
        let alice_machine = get_machine();
        let alice_account = account();

        let bob_machine = bob_machine();
        let bob_account = bob_account();

        // Create Olm sessions for our two accounts.
        let (alice_session, bob_session) = alice_account.create_session_for(&bob_account).await;

        let alice_device = ReadOnlyDevice::from_account(&alice_account).await;
        let bob_device = ReadOnlyDevice::from_account(&bob_account).await;

        // Populate our stores with Olm sessions and a Megolm session.

        alice_machine
            .store
            .save_sessions(&[alice_session])
            .await
            .unwrap();
        alice_machine
            .store
            .save_devices(&[bob_device])
            .await
            .unwrap();
        bob_machine
            .store
            .save_sessions(&[bob_session])
            .await
            .unwrap();
        bob_machine
            .store
            .save_devices(&[alice_device])
            .await
            .unwrap();

        let (group_session, inbound_group_session) = bob_account
            .create_group_session_pair_with_defaults(&room_id())
            .await
            .unwrap();

        bob_machine
            .store
            .save_inbound_group_sessions(&[inbound_group_session])
            .await
            .unwrap();

        // Alice wants to request the outbound group session from bob.
        alice_machine
            .create_outgoing_key_request(
                &room_id(),
                bob_account.identity_keys.curve25519(),
                group_session.session_id(),
            )
            .await
            .unwrap();
        group_session.mark_shared_with(&alice_id(), &alice_device_id());

        // Put the outbound session into bobs store.
        bob_machine
            .outbound_group_sessions
            .insert(room_id(), group_session);

        // Get the request and convert it into a event.
        let request = alice_machine
            .outgoing_to_device_requests
            .iter()
            .next()
            .unwrap();
        let id = request.request_id;
        let content = request
            .request
            .to_device()
            .unwrap()
            .messages
            .get(&alice_id())
            .unwrap()
            .get(&DeviceIdOrAllDevices::AllDevices)
            .unwrap();
        let content: RoomKeyRequestEventContent = serde_json::from_str(content.get()).unwrap();

        drop(request);
        alice_machine
            .mark_outgoing_request_as_sent(&id)
            .await
            .unwrap();

        let event = ToDeviceEvent {
            sender: alice_id(),
            content,
        };

        // Bob doesn't have any outgoing requests.
        assert!(bob_machine.outgoing_to_device_requests.is_empty());

        // Receive the room key request from alice.
        bob_machine.receive_incoming_key_request(&event);
        bob_machine.collect_incoming_key_requests().await.unwrap();
        // Now bob does have an outgoing request.
        assert!(!bob_machine.outgoing_to_device_requests.is_empty());

        // Get the request and convert it to a encrypted to-device event.
        let request = bob_machine
            .outgoing_to_device_requests
            .iter()
            .next()
            .unwrap();

        let id = request.request_id;
        let content = request
            .request
            .to_device()
            .unwrap()
            .messages
            .get(&alice_id())
            .unwrap()
            .get(&DeviceIdOrAllDevices::DeviceId(alice_device_id()))
            .unwrap();
        let content: EncryptedEventContent = serde_json::from_str(content.get()).unwrap();

        drop(request);
        bob_machine
            .mark_outgoing_request_as_sent(&id)
            .await
            .unwrap();

        let _event = ToDeviceEvent {
            sender: bob_id(),
            content,
        };

        // TODO test that alice can receive, decrypt and add the requested key
        // to the store.
    }
}
