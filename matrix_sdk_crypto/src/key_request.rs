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
// handle the case where we can't create a session with a device. clearing our
// stale key share requests that we'll never be able to handle.
//
// If we don't trust the device store an object that remembers the request and
// let the users introspect that object.

use dashmap::{mapref::entry::Entry, DashMap, DashSet};
use serde::{Deserialize, Serialize};
use serde_json::value::to_raw_value;
use std::{collections::BTreeMap, sync::Arc};
use thiserror::Error;
use tracing::{error, info, instrument, trace, warn};

use matrix_sdk_common::{
    api::r0::to_device::DeviceIdOrAllDevices,
    events::{
        forwarded_room_key::ForwardedRoomKeyEventContent,
        room_key_request::{Action, RequestedKeyInfo, RoomKeyRequestEventContent},
        AnyToDeviceEvent, EventType, ToDeviceEvent,
    },
    identifiers::{DeviceId, DeviceIdBox, EventEncryptionAlgorithm, RoomId, UserId},
    uuid::Uuid,
    Raw,
};

use crate::{
    error::{OlmError, OlmResult},
    olm::{InboundGroupSession, OutboundGroupSession},
    requests::{OutgoingRequest, ToDeviceRequest},
    store::{CryptoStoreError, Store},
    Device,
};

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

/// A queue where we store room key requests that we want to serve but the
/// device that requested the key doesn't share an Olm session with us.
#[derive(Debug, Clone)]
struct WaitQueue {
    requests_waiting_for_session:
        Arc<DashMap<(UserId, DeviceIdBox, String), ToDeviceEvent<RoomKeyRequestEventContent>>>,
    requests_ids_waiting: Arc<DashMap<(UserId, DeviceIdBox), DashSet<String>>>,
}

impl WaitQueue {
    fn new() -> Self {
        Self {
            requests_waiting_for_session: Arc::new(DashMap::new()),
            requests_ids_waiting: Arc::new(DashMap::new()),
        }
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.requests_ids_waiting.is_empty() && self.requests_waiting_for_session.is_empty()
    }

    fn insert(&self, device: &Device, event: &ToDeviceEvent<RoomKeyRequestEventContent>) {
        let key = (
            device.user_id().to_owned(),
            device.device_id().into(),
            event.content.request_id.to_owned(),
        );
        self.requests_waiting_for_session.insert(key, event.clone());

        let key = (device.user_id().to_owned(), device.device_id().into());
        self.requests_ids_waiting
            .entry(key)
            .or_insert_with(DashSet::new)
            .insert(event.content.request_id.clone());
    }

    fn remove(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Vec<(
        (UserId, DeviceIdBox, String),
        ToDeviceEvent<RoomKeyRequestEventContent>,
    )> {
        self.requests_ids_waiting
            .remove(&(user_id.to_owned(), device_id.into()))
            .map(|(_, request_ids)| {
                request_ids
                    .iter()
                    .filter_map(|id| {
                        let key = (user_id.to_owned(), device_id.into(), id.to_owned());
                        self.requests_waiting_for_session.remove(&key)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
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
    wait_queue: WaitQueue,
    users_for_key_claim: Arc<DashMap<UserId, DashSet<DeviceIdBox>>>,
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
            wait_queue: WaitQueue::new(),
            users_for_key_claim: Arc::new(DashMap::new()),
        }
    }

    /// Our own user id.
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    /// Get the map of user/devices which we need to claim one-time for.
    pub fn users_for_key_claim(&self) -> &DashMap<UserId, DashSet<DeviceIdBox>> {
        &self.users_for_key_claim
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
    pub async fn collect_incoming_key_requests(&self) -> OlmResult<()> {
        for item in self.incoming_key_requests.iter() {
            let event = item.value();
            self.handle_key_request(event).await?;
        }

        self.incoming_key_requests.clear();

        Ok(())
    }

    /// Store the key share request for later, once we get an Olm session with
    /// the given device [`retry_keyshare`](#method.retry_keyshare) should be
    /// called.
    fn handle_key_share_without_session(
        &self,
        device: Device,
        event: &ToDeviceEvent<RoomKeyRequestEventContent>,
    ) {
        self.users_for_key_claim
            .entry(device.user_id().to_owned())
            .or_insert_with(DashSet::new)
            .insert(device.device_id().into());
        self.wait_queue.insert(&device, event);
    }

    /// Retry keyshares for a device that previously didn't have an Olm session
    /// with us.
    ///
    /// This should be only called if the given user/device got a new Olm
    /// session.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user id of the device that we created the Olm session
    /// with.
    ///
    /// * `device_id` - The device id of the device that got the Olm session.
    pub fn retry_keyshare(&self, user_id: &UserId, device_id: &DeviceId) {
        if let Entry::Occupied(e) = self.users_for_key_claim.entry(user_id.to_owned()) {
            e.get().remove(device_id);

            if e.get().is_empty() {
                e.remove();
            }
        }

        for (key, event) in self.wait_queue.remove(user_id, device_id) {
            if !self.incoming_key_requests.contains_key(&key) {
                self.incoming_key_requests.insert(key, event);
            }
        }
    }

    /// Handle a single incoming key request.
    #[instrument]
    async fn handle_key_request(
        &self,
        event: &ToDeviceEvent<RoomKeyRequestEventContent>,
    ) -> OlmResult<()> {
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
            .get_device(&event.sender, &event.content.requesting_device_id)
            .await?;

        if let Some(device) = device {
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

                if let Err(e) = self.share_session(&session, &device).await {
                    match e {
                        OlmError::MissingSession => {
                            info!(
                                "Key request from {} {} is missing an Olm session, \
                                 putting the request in the wait queue",
                                device.user_id(),
                                device.device_id()
                            );
                            self.handle_key_share_without_session(device, event);
                            return Ok(());
                        }
                        e => return Err(e),
                    }
                }
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

    async fn share_session(&self, session: &InboundGroupSession, device: &Device) -> OlmResult<()> {
        let content = device.encrypt_session(session.clone()).await?;

        let id = Uuid::new_v4();
        let mut messages = BTreeMap::new();

        messages
            .entry(device.user_id().to_owned())
            .or_insert_with(BTreeMap::new)
            .insert(
                DeviceIdOrAllDevices::DeviceId(device.device_id().into()),
                to_raw_value(&content)?,
            );

        let request = OutgoingRequest {
            request_id: id,
            request: Arc::new(
                ToDeviceRequest {
                    event_type: EventType::RoomEncrypted,
                    txn_id: id,
                    messages,
                }
                .into(),
            ),
        };

        self.outgoing_to_device_requests.insert(id, request);

        Ok(())
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
            if outbound.is_shared_with(device.user_id(), device.device_id()) {
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
            AnyToDeviceEvent, ToDeviceEvent,
        },
        identifiers::{room_id, user_id, DeviceIdBox, RoomId, UserId},
    };
    use matrix_sdk_test::async_test;
    use std::{convert::TryInto, sync::Arc};

    use crate::{
        identities::{LocalTrust, ReadOnlyDevice},
        olm::{Account, ReadOnlyAccount},
        store::{CryptoStore, MemoryStore, Store},
        verification::VerificationMachine,
    };

    use super::{KeyRequestMachine, KeyshareDecision};

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

    fn account() -> ReadOnlyAccount {
        ReadOnlyAccount::new(&alice_id(), &alice_device_id())
    }

    fn bob_account() -> ReadOnlyAccount {
        ReadOnlyAccount::new(&bob_id(), &bob_device_id())
    }

    fn bob_machine() -> KeyRequestMachine {
        let user_id = Arc::new(bob_id());
        let account = ReadOnlyAccount::new(&user_id, &alice_device_id());
        let store: Arc<Box<dyn CryptoStore>> = Arc::new(Box::new(MemoryStore::new()));
        let verification = VerificationMachine::new(account, store.clone());
        let store = Store::new(user_id.clone(), store, verification);

        KeyRequestMachine::new(
            user_id,
            Arc::new(bob_device_id()),
            store,
            Arc::new(DashMap::new()),
        )
    }

    async fn get_machine() -> KeyRequestMachine {
        let user_id = Arc::new(alice_id());
        let account = ReadOnlyAccount::new(&user_id, &alice_device_id());
        let device = ReadOnlyDevice::from_account(&account).await;
        let store: Arc<Box<dyn CryptoStore>> = Arc::new(Box::new(MemoryStore::new()));
        let verification = VerificationMachine::new(account, store.clone());
        let store = Store::new(user_id.clone(), store, verification);
        store.save_devices(&[device]).await.unwrap();

        KeyRequestMachine::new(
            user_id,
            Arc::new(alice_device_id()),
            store,
            Arc::new(DashMap::new()),
        )
    }

    #[async_test]
    async fn create_machine() {
        let machine = get_machine().await;

        assert!(machine.outgoing_to_device_requests().is_empty());
    }

    #[async_test]
    async fn create_key_request() {
        let machine = get_machine().await;
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
        let machine = get_machine().await;
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
        let machine = get_machine().await;
        let account = account();

        let own_device = machine
            .store
            .get_device(&alice_id(), &alice_device_id())
            .await
            .unwrap()
            .unwrap();

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

        let bob_device = ReadOnlyDevice::from_account(&bob_account()).await;
        machine.store.save_devices(&[bob_device]).await.unwrap();

        let bob_device = machine
            .store
            .get_device(&bob_id(), &bob_device_id())
            .await
            .unwrap()
            .unwrap();

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
        let alice_machine = get_machine().await;
        let alice_account = Account {
            inner: account(),
            store: alice_machine.store.clone(),
        };

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
            .insert(room_id(), group_session.clone());

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

        let event = ToDeviceEvent {
            sender: bob_id(),
            content,
        };

        // Check that alice doesn't have the session.
        assert!(alice_machine
            .store
            .get_inbound_group_session(
                &room_id(),
                &bob_account.identity_keys().curve25519(),
                group_session.session_id()
            )
            .await
            .unwrap()
            .is_none());

        let (decrypted, sender_key, _) =
            alice_account.decrypt_to_device_event(&event).await.unwrap();

        if let AnyToDeviceEvent::ForwardedRoomKey(mut e) = decrypted.deserialize().unwrap() {
            alice_machine
                .receive_forwarded_room_key(&sender_key, &mut e)
                .await
                .unwrap();
        } else {
            panic!("Invalid decrypted event type");
        }

        // Check that alice now does have the session.
        let session = alice_machine
            .store
            .get_inbound_group_session(&room_id(), &sender_key, group_session.session_id())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(session.session_id(), group_session.session_id())
    }

    #[async_test]
    async fn key_share_cycle_without_session() {
        let alice_machine = get_machine().await;
        let alice_account = Account {
            inner: account(),
            store: alice_machine.store.clone(),
        };

        let bob_machine = bob_machine();
        let bob_account = bob_account();

        // Create Olm sessions for our two accounts.
        let (alice_session, bob_session) = alice_account.create_session_for(&bob_account).await;

        let alice_device = ReadOnlyDevice::from_account(&alice_account).await;
        let bob_device = ReadOnlyDevice::from_account(&bob_account).await;

        // Populate our stores with Olm sessions and a Megolm session.

        alice_machine
            .store
            .save_devices(&[bob_device])
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
            .insert(room_id(), group_session.clone());

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
        assert!(bob_machine.users_for_key_claim.is_empty());
        assert!(bob_machine.wait_queue.is_empty());

        // Receive the room key request from alice.
        bob_machine.receive_incoming_key_request(&event);
        bob_machine.collect_incoming_key_requests().await.unwrap();
        // Bob doens't have an outgoing requests since we're lacking a session.
        assert!(bob_machine.outgoing_to_device_requests.is_empty());
        assert!(!bob_machine.users_for_key_claim.is_empty());
        assert!(!bob_machine.wait_queue.is_empty());

        // We create a session now.
        alice_machine
            .store
            .save_sessions(&[alice_session])
            .await
            .unwrap();
        bob_machine
            .store
            .save_sessions(&[bob_session])
            .await
            .unwrap();

        bob_machine.retry_keyshare(&alice_id(), &alice_device_id());
        assert!(bob_machine.users_for_key_claim.is_empty());
        bob_machine.collect_incoming_key_requests().await.unwrap();
        // Bob now has an outgoing requests.
        assert!(!bob_machine.outgoing_to_device_requests.is_empty());
        assert!(bob_machine.wait_queue.is_empty());

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

        let event = ToDeviceEvent {
            sender: bob_id(),
            content,
        };

        // Check that alice doesn't have the session.
        assert!(alice_machine
            .store
            .get_inbound_group_session(
                &room_id(),
                &bob_account.identity_keys().curve25519(),
                group_session.session_id()
            )
            .await
            .unwrap()
            .is_none());

        let (decrypted, sender_key, _) =
            alice_account.decrypt_to_device_event(&event).await.unwrap();

        if let AnyToDeviceEvent::ForwardedRoomKey(mut e) = decrypted.deserialize().unwrap() {
            alice_machine
                .receive_forwarded_room_key(&sender_key, &mut e)
                .await
                .unwrap();
        } else {
            panic!("Invalid decrypted event type");
        }

        // Check that alice now does have the session.
        let session = alice_machine
            .store
            .get_inbound_group_session(&room_id(), &sender_key, group_session.session_id())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(session.session_id(), group_session.session_id())
    }
}
