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

use std::{collections::BTreeMap, sync::Arc};

use dashmap::{mapref::entry::Entry, DashMap, DashSet};
use ruma::{
    api::client::keys::claim_keys::v3::Request as KeysClaimRequest,
    events::secret::request::{
        RequestAction, SecretName, ToDeviceSecretRequestEvent as SecretRequestEvent,
    },
    DeviceId, DeviceKeyAlgorithm, OwnedDeviceId, OwnedTransactionId, OwnedUserId, RoomId,
    TransactionId, UserId,
};
use tracing::{debug, info, trace, warn};
use vodozemac::{megolm::SessionOrdering, Curve25519PublicKey};

use super::{GossipRequest, KeyForwardDecision, RequestEvent, RequestInfo, SecretInfo, WaitQueue};
use crate::{
    error::{EventError, OlmError, OlmResult},
    olm::{InboundGroupSession, Session, ShareState},
    requests::{OutgoingRequest, ToDeviceRequest},
    session_manager::GroupSessionCache,
    store::{Changes, CryptoStoreError, SecretImportError, Store},
    types::{
        events::{
            forwarded_room_key::{ForwardedMegolmV1AesSha2Content, ForwardedRoomKeyContent},
            olm_v1::{DecryptedForwardedRoomKeyEvent, DecryptedSecretSendEvent},
            room::encrypted::EncryptedEvent,
            room_key_request::{
                Action, MegolmV1AesSha2Content, RequestedKeyInfo, RoomKeyRequestEvent,
            },
            secret_send::SecretSendContent,
            EventType,
        },
        EventEncryptionAlgorithm,
    },
    Device, MegolmError,
};

#[derive(Debug, Clone)]
pub(crate) struct GossipMachine {
    user_id: Arc<UserId>,
    device_id: Arc<DeviceId>,
    store: Store,
    outbound_group_sessions: GroupSessionCache,
    outgoing_requests: Arc<DashMap<OwnedTransactionId, OutgoingRequest>>,
    incoming_key_requests: Arc<DashMap<RequestInfo, RequestEvent>>,
    wait_queue: WaitQueue,
    users_for_key_claim: Arc<DashMap<OwnedUserId, DashSet<OwnedDeviceId>>>,
}

impl GossipMachine {
    pub fn new(
        user_id: Arc<UserId>,
        device_id: Arc<DeviceId>,
        store: Store,
        outbound_group_sessions: GroupSessionCache,
        users_for_key_claim: Arc<DashMap<OwnedUserId, DashSet<OwnedDeviceId>>>,
    ) -> Self {
        Self {
            user_id,
            device_id,
            store,
            outbound_group_sessions,
            outgoing_requests: Default::default(),
            incoming_key_requests: Default::default(),
            wait_queue: WaitQueue::new(),
            users_for_key_claim,
        }
    }

    /// Load stored outgoing requests that were not yet sent out.
    async fn load_outgoing_requests(&self) -> Result<Vec<OutgoingRequest>, CryptoStoreError> {
        Ok(self
            .store
            .get_unsent_secret_requests()
            .await?
            .into_iter()
            .filter(|i| !i.sent_out)
            .map(|info| info.to_request(self.device_id()))
            .collect())
    }

    /// Our own user id.
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    /// Our own device ID.
    pub fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    pub async fn outgoing_to_device_requests(
        &self,
    ) -> Result<Vec<OutgoingRequest>, CryptoStoreError> {
        let mut key_requests = self.load_outgoing_requests().await?;
        let key_forwards: Vec<OutgoingRequest> =
            self.outgoing_requests.iter().map(|i| i.value().clone()).collect();
        key_requests.extend(key_forwards);

        let users_for_key_claim: BTreeMap<_, _> = self
            .users_for_key_claim
            .iter()
            .map(|i| {
                let device_map = i
                    .value()
                    .iter()
                    .map(|d| (d.key().to_owned(), DeviceKeyAlgorithm::SignedCurve25519))
                    .collect();

                (i.key().to_owned(), device_map)
            })
            .collect();

        if !users_for_key_claim.is_empty() {
            let key_claim_request = KeysClaimRequest::new(users_for_key_claim);
            key_requests.push(OutgoingRequest {
                request_id: TransactionId::new(),
                request: Arc::new(key_claim_request.into()),
            });
        }

        Ok(key_requests)
    }

    /// Receive a room key request event.
    pub fn receive_incoming_key_request(&self, event: &RoomKeyRequestEvent) {
        self.receive_event(event.clone().into())
    }

    fn receive_event(&self, event: RequestEvent) {
        // Some servers might send to-device events to ourselves if we send one
        // out using a wildcard instead of a specific device as a recipient.
        //
        // Check if we're the sender of this request event and ignore it if
        // so.
        if event.sender() == self.user_id() && event.requesting_device_id() == self.device_id() {
            trace!("Received a secret request event from ourselves, ignoring")
        } else {
            let request_info = event.to_request_info();
            self.incoming_key_requests.insert(request_info, event);
        }
    }

    pub fn receive_incoming_secret_request(&self, event: &SecretRequestEvent) {
        self.receive_event(event.clone().into())
    }

    /// Handle all the incoming key requests that are queued up and empty our
    /// key request queue.
    pub async fn collect_incoming_key_requests(&self) -> OlmResult<Vec<Session>> {
        let mut changed_sessions = Vec::new();

        for item in self.incoming_key_requests.iter() {
            let event = item.value();

            if let Some(s) = match event {
                RequestEvent::KeyShare(e) => self.handle_key_request(e).await?,
                RequestEvent::Secret(e) => self.handle_secret_request(e).await?,
            } {
                changed_sessions.push(s);
            }
        }

        self.incoming_key_requests.clear();

        Ok(changed_sessions)
    }

    /// Store the key share request for later, once we get an Olm session with
    /// the given device [`retry_keyshare`](#method.retry_keyshare) should be
    /// called.
    fn handle_key_share_without_session(&self, device: Device, event: RequestEvent) {
        self.users_for_key_claim
            .entry(device.user_id().to_owned())
            .or_default()
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
    /// * `device_id` - The device ID of the device that got the Olm session.
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

    async fn handle_secret_request(
        &self,
        event: &SecretRequestEvent,
    ) -> OlmResult<Option<Session>> {
        let secret_name = match &event.content.action {
            RequestAction::Request(s) => s,
            // We ignore cancellations here since there's nothing to serve.
            RequestAction::RequestCancellation => return Ok(None),
            action => {
                warn!(?action, "Unknown secret request action");
                return Ok(None);
            }
        };

        let content = if let Some(secret) = self.store.export_secret(secret_name).await {
            SecretSendContent::new(event.content.request_id.to_owned(), secret)
        } else {
            info!(?secret_name, "Can't serve a secret request, secret isn't found");
            return Ok(None);
        };

        let device =
            self.store.get_device(&event.sender, &event.content.requesting_device_id).await?;

        Ok(if let Some(device) = device {
            if device.user_id() == self.user_id() {
                if device.is_verified() {
                    info!(
                        user_id = device.user_id().as_str(),
                        device_id = device.device_id().as_str(),
                        ?secret_name,
                        "Sharing a secret with a device",
                    );

                    match self.share_secret(&device, content).await {
                        Ok(s) => Ok(Some(s)),
                        Err(OlmError::MissingSession) => {
                            info!(
                                user_id = device.user_id().as_str(),
                                device_id = device.device_id().as_str(),
                                secret_name = secret_name.as_ref(),
                                "Secret request is missing an Olm session, \
                                putting the request in the wait queue",
                            );
                            self.handle_key_share_without_session(device, event.clone().into());

                            Ok(None)
                        }
                        Err(e) => Err(e),
                    }?
                } else {
                    info!(
                        user_id = device.user_id().as_str(),
                        device_id = device.device_id().as_str(),
                        ?secret_name,
                        "Received a secret request that we won't serve, the device isn't trusted",
                    );

                    None
                }
            } else {
                info!(
                    user_id = device.user_id().as_str(),
                    device_id = device.device_id().as_str(),
                    ?secret_name,
                    "Received a secret request that we won't serve, the device doesn't belong to us",
                );

                None
            }
        } else {
            warn!(
                user_id = event.sender.as_str(),
                device_id = event.content.requesting_device_id.as_str(),
                ?secret_name,
                "Received a secret request form an unknown device",
            );
            self.store.update_tracked_user(&event.sender, true).await?;

            None
        })
    }

    async fn handle_megolm_v1_request(
        &self,
        event: &RoomKeyRequestEvent,
        key_info: &MegolmV1AesSha2Content,
    ) -> OlmResult<Option<Session>> {
        let session = self
            .store
            .get_inbound_group_session(
                &key_info.room_id,
                &key_info.sender_key.to_base64(),
                &key_info.session_id,
            )
            .await?;

        let session = if let Some(s) = session {
            s
        } else {
            debug!(
                user_id = event.sender.as_str(),
                device_id = event.content.requesting_device_id.as_str(),
                session_id = key_info.session_id.as_str(),
                room_id = key_info.room_id.as_str(),
                "Received a room key request for an unknown inbound group session",
            );
            return Ok(None);
        };

        let device =
            self.store.get_device(&event.sender, &event.content.requesting_device_id).await?;

        if let Some(device) = device {
            match self.should_share_key(&device, &session).await {
                Err(e) => {
                    if let KeyForwardDecision::ChangedSenderKey = e {
                        warn!(
                            user_id = device.user_id().as_str(),
                            device_id = device.device_id().as_str(),
                            "Received a key request from a device that changed \
                            their curve25519 sender key"
                        );
                    } else {
                        debug!(
                            user_id = device.user_id().as_str(),
                            device_id = device.device_id().as_str(),
                            reason = ?e,
                            "Received a key request that we won't serve",
                        );
                    }

                    Ok(None)
                }
                Ok(message_index) => {
                    info!(
                        user_id = device.user_id().as_str(),
                        device_id = device.device_id().as_str(),
                        session_id = key_info.session_id.as_str(),
                        room_id = key_info.room_id.as_str(),
                        ?message_index,
                        "Serving a room key request",
                    );

                    match self.share_session(&session, &device, message_index).await {
                        Ok(s) => Ok(Some(s)),
                        Err(OlmError::MissingSession) => {
                            info!(
                                user_id = device.user_id().as_str(),
                                device_id = device.device_id().as_str(),
                                session_id = key_info.session_id.as_str(),
                                "Key request is missing an Olm session, \
                                putting the request in the wait queue",
                            );
                            self.handle_key_share_without_session(device, event.to_owned().into());

                            Ok(None)
                        }
                        Err(OlmError::SessionExport(e)) => {
                            warn!(
                                user_id = device.user_id().as_str(),
                                device_id = device.device_id().as_str(),
                                session_id = key_info.session_id.as_str(),
                                "Can't serve a room key request, the session \
                                can't be exported into a forwarded room key: \
                                {:?}",
                                e
                            );
                            Ok(None)
                        }
                        Err(e) => Err(e),
                    }
                }
            }
        } else {
            warn!(
                user_id = event.sender.as_str(),
                device_id = event.content.requesting_device_id.as_str(),
                "Received a key request from an unknown device",
            );
            self.store.update_tracked_user(&event.sender, true).await?;

            Ok(None)
        }
    }

    /// Handle a single incoming key request.
    async fn handle_key_request(&self, event: &RoomKeyRequestEvent) -> OlmResult<Option<Session>> {
        match &event.content.action {
            Action::Request(info) => match info {
                RequestedKeyInfo::MegolmV1AesSha2(i) => {
                    self.handle_megolm_v1_request(event, i).await
                }
                // V2 room key requests don't have a sender_key field, we
                // currently can't fetch an inbound group session without a
                // sender key, so ignore the request.
                #[cfg(feature = "experimental-algorithms")]
                RequestedKeyInfo::MegolmV2AesSha2(_) => Ok(None),
                RequestedKeyInfo::Unknown(i) => {
                    debug!(
                        sender = %event.sender,
                        algorithm = %i.algorithm,
                        "Received a room key request for a unsupported algorithm"
                    );
                    Ok(None)
                }
            },
            // We ignore cancellations here since there's nothing to serve.
            Action::Cancellation => Ok(None),
        }
    }

    async fn share_secret(
        &self,
        device: &Device,
        content: SecretSendContent,
    ) -> OlmResult<Session> {
        let event_type = content.event_type();
        let content = serde_json::to_value(content)?;
        let (used_session, content) = device.encrypt(event_type, content).await?;

        let request = ToDeviceRequest::new(
            device.user_id(),
            device.device_id().to_owned(),
            content.event_type(),
            content.cast(),
        );

        let request = OutgoingRequest {
            request_id: request.txn_id.clone(),
            request: Arc::new(request.into()),
        };
        self.outgoing_requests.insert(request.request_id.clone(), request);

        Ok(used_session)
    }

    async fn share_session(
        &self,
        session: &InboundGroupSession,
        device: &Device,
        message_index: Option<u32>,
    ) -> OlmResult<Session> {
        let (used_session, content) =
            device.encrypt_room_key_for_forwarding(session.clone(), message_index).await?;

        let request = ToDeviceRequest::new(
            device.user_id(),
            device.device_id().to_owned(),
            content.event_type(),
            content.cast(),
        );

        let request = OutgoingRequest {
            request_id: request.txn_id.clone(),
            request: Arc::new(request.into()),
        };
        self.outgoing_requests.insert(request.request_id.clone(), request);

        Ok(used_session)
    }

    /// Check if it's ok to share a session with the given device.
    ///
    /// The logic for this is currently as follows:
    ///
    /// * Share the session in full, starting from the earliest known index, if
    /// the requesting device is our own, trusted (verified) device.
    ///
    /// * For other requesting devices, share only a limited session and only if
    /// we originally shared with that device because it was present when the
    /// message was initially sent. By limited, we mean that the session will
    /// not be shared in full, but only from the message index at that moment.
    /// Since this information is recorded in the outbound session, we need to
    /// have it for this to work.
    ///
    /// * In all other cases, refuse to share the session.
    ///
    /// # Arguments
    ///
    /// * `device` - The device that is requesting a session from us.
    ///
    /// * `session` - The session that was requested to be shared.
    ///
    /// # Return value
    ///
    /// A `Result` representing whether we should share the session:
    ///
    /// - `Ok(None)`: Should share the entire session, starting with the
    ///   earliest known index.
    /// - `Ok(Some(i))`: Should share the session, but only starting from index
    ///   i.
    /// - `Err(x)`: Should *refuse* to share the session. `x` is the reason for
    ///   the refusal.
    async fn should_share_key(
        &self,
        device: &Device,
        session: &InboundGroupSession,
    ) -> Result<Option<u32>, KeyForwardDecision> {
        let outbound_session = self
            .outbound_group_sessions
            .get_with_id(session.room_id(), session.session_id())
            .await
            .ok()
            .flatten();

        // If this is our own, verified device, we share the entire session from the
        // earliest known index.
        if device.user_id() == self.user_id() && device.is_verified() {
            Ok(None)
        // Otherwise, if the records show we previously shared with this device,
        // we'll reshare the session from the index we previously shared
        // at. For this, we need an outbound session because this
        // information is recorded there.
        } else if let Some(outbound) = outbound_session {
            match outbound.is_shared_with(device) {
                ShareState::Shared(message_index) => Ok(Some(message_index)),
                ShareState::SharedButChangedSenderKey => Err(KeyForwardDecision::ChangedSenderKey),
                ShareState::NotShared => Err(KeyForwardDecision::OutboundSessionNotShared),
            }
        // Otherwise, there's not enough info to decide if we can safely share
        // the session.
        } else if device.user_id() == self.user_id() {
            Err(KeyForwardDecision::UntrustedDevice)
        } else {
            Err(KeyForwardDecision::MissingOutboundSession)
        }
    }

    /// Check if it's ok, or rather if it makes sense to automatically request
    /// a key from our other devices.
    ///
    /// # Arguments
    ///
    /// * `key_info` - The info of our key request containing information about
    /// the key we wish to request.
    async fn should_request_key(&self, key_info: &SecretInfo) -> Result<bool, CryptoStoreError> {
        let request = self.store.get_secret_request_by_info(key_info).await?;

        // Don't send out duplicate requests, users can re-request them if they
        // think a second request might succeed.
        if request.is_none() {
            let devices = self.store.get_user_devices(self.user_id()).await?;

            // Devices will only respond to key requests if the devices are
            // verified, if the device isn't verified by us it's unlikely that
            // we're verified by them either. Don't request keys if there isn't
            // at least one verified device.
            if devices.is_any_verified() {
                Ok(true)
            } else {
                Ok(false)
            }
        } else {
            Ok(false)
        }
    }

    /// Create a new outgoing key request for the key with the given session id.
    ///
    /// This will queue up a new to-device request and store the key info so
    /// once we receive a forwarded room key we can check that it matches the
    /// key we requested.
    ///
    /// This method will return a cancel request and a new key request if the
    /// key was already requested, otherwise it will return just the key
    /// request.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The id of the room where the key is used in.
    ///
    /// * `event` - The event for which we would like to request the room key.
    pub async fn request_key(
        &self,
        room_id: &RoomId,
        event: &EncryptedEvent,
    ) -> Result<(Option<OutgoingRequest>, OutgoingRequest), MegolmError> {
        let secret_info =
            event.room_key_info(room_id).ok_or(EventError::UnsupportedAlgorithm)?.into();

        let request = self.store.get_secret_request_by_info(&secret_info).await?;

        if let Some(request) = request {
            let cancel = request.to_cancellation(self.device_id());
            let request = request.to_request(self.device_id());

            Ok((Some(cancel), request))
        } else {
            let request = self.request_key_helper(secret_info).await?;

            Ok((None, request))
        }
    }

    /// Create outgoing secret requests for the given
    pub fn request_missing_secrets(
        own_user_id: &UserId,
        secret_names: Vec<SecretName>,
    ) -> Vec<GossipRequest> {
        if !secret_names.is_empty() {
            info!(?secret_names, "Creating new outgoing secret requests");

            secret_names
                .into_iter()
                .map(|n| GossipRequest::from_secret_name(own_user_id.to_owned(), n))
                .collect()
        } else {
            trace!("No secrets are missing from our store, not requesting them");
            vec![]
        }
    }

    async fn request_key_helper(
        &self,
        key_info: SecretInfo,
    ) -> Result<OutgoingRequest, CryptoStoreError> {
        let request = GossipRequest {
            request_recipient: self.user_id().to_owned(),
            request_id: TransactionId::new(),
            info: key_info,
            sent_out: false,
        };

        let outgoing_request = request.to_request(self.device_id());
        self.save_outgoing_key_info(request).await?;

        Ok(outgoing_request)
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
    /// * `event` - The event for which we would like to request the room key.
    pub async fn create_outgoing_key_request(
        &self,
        room_id: &RoomId,
        event: &EncryptedEvent,
    ) -> Result<bool, CryptoStoreError> {
        Ok(if let Some(info) = event.room_key_info(room_id).map(|i| i.into()) {
            if self.should_request_key(&info).await? {
                self.request_key_helper(info).await?;
                true
            } else {
                false
            }
        } else {
            false
        })
    }

    /// Save an outgoing key info.
    async fn save_outgoing_key_info(&self, info: GossipRequest) -> Result<(), CryptoStoreError> {
        let mut changes = Changes::default();
        changes.key_requests.push(info);
        self.store.save_changes(changes).await?;

        Ok(())
    }

    /// Get an outgoing key info that matches the forwarded room key content.
    async fn get_key_info(
        &self,
        event: &DecryptedForwardedRoomKeyEvent,
    ) -> Result<Option<GossipRequest>, CryptoStoreError> {
        if let Some(info) = event.room_key_info().map(|i| i.into()) {
            self.store.get_secret_request_by_info(&info).await
        } else {
            Ok(None)
        }
    }

    /// Delete the given outgoing key info.
    async fn delete_key_info(&self, info: &GossipRequest) -> Result<(), CryptoStoreError> {
        self.store.delete_outgoing_secret_requests(&info.request_id).await
    }

    /// Mark the outgoing request as sent.
    pub async fn mark_outgoing_request_as_sent(
        &self,
        id: &TransactionId,
    ) -> Result<(), CryptoStoreError> {
        let info = self.store.get_outgoing_secret_requests(id).await?;

        if let Some(mut info) = info {
            trace!(
                recipient = info.request_recipient.as_str(),
                request_type = info.request_type(),
                request_id = info.request_id.to_string().as_str(),
                "Marking outgoing key request as sent"
            );
            info.sent_out = true;
            self.save_outgoing_key_info(info).await?;
        }

        self.outgoing_requests.remove(id);

        Ok(())
    }

    /// Mark the given outgoing key info as done.
    ///
    /// This will queue up a request cancellation.
    async fn mark_as_done(&self, key_info: &GossipRequest) -> Result<(), CryptoStoreError> {
        trace!(
            recipient = key_info.request_recipient.as_str(),
            request_type = key_info.request_type(),
            request_id = key_info.request_id.to_string().as_str(),
            "Successfully received a secret, removing the request"
        );

        self.outgoing_requests.remove(&key_info.request_id);
        // TODO return the key info instead of deleting it so the sync handler
        // can delete it in one transaction.
        self.delete_key_info(key_info).await?;

        let request = key_info.to_cancellation(self.device_id());
        self.outgoing_requests.insert(request.request_id.clone(), request);

        Ok(())
    }

    async fn accept_secret(
        &self,
        event: &DecryptedSecretSendEvent,
        request: &GossipRequest,
        secret_name: &SecretName,
    ) -> Result<(), CryptoStoreError> {
        if secret_name != &SecretName::RecoveryKey {
            match self.store.import_secret(secret_name, &event.content.secret).await {
                Ok(_) => self.mark_as_done(request).await?,
                Err(e) => {
                    // If this is a store error propagate it up
                    // the call stack.
                    if let SecretImportError::Store(e) = e {
                        return Err(e);
                    } else {
                        // Otherwise warn that there was
                        // something wrong with the secret.
                        warn!(
                            secret_name = secret_name.as_ref(),
                            error = ?e,
                            "Error while importing a secret"
                        )
                    }
                }
            }
        } else {
            // Skip importing the recovery key here since
            // we'll want to check if the public key matches
            // to the latest version on the server. The key
            // will not be zeroized and
            // instead leave the key in the event and let
            // the user import it later.
        }

        Ok(())
    }

    async fn receive_secret(
        &self,
        sender_key: Curve25519PublicKey,
        event: &DecryptedSecretSendEvent,
        request: &GossipRequest,
        secret_name: &SecretName,
    ) -> Result<(), CryptoStoreError> {
        debug!(
            sender = event.sender.as_str(),
            request_id = event.content.request_id.as_str(),
            secret_name = secret_name.as_ref(),
            "Received a m.secret.send event with a matching request"
        );

        if let Some(device) =
            self.store.get_device_from_curve_key(&event.sender, sender_key).await?
        {
            // Only accept secrets from one of our own trusted devices.
            if device.user_id() == self.user_id() && device.is_verified() {
                self.accept_secret(event, request, secret_name).await?;
            } else {
                warn!(
                    sender = event.sender.as_str(),
                    request_id = event.content.request_id.as_str(),
                    secret_name = secret_name.as_ref(),
                    "Received a m.secret.send event from another user or from \
                    unverified device"
                );
            }
        } else {
            warn!(
                sender = event.sender.as_str(),
                request_id = event.content.request_id.as_str(),
                secret_name = secret_name.as_ref(),
                "Received a m.secret.send event from an unknown device"
            );
            self.store.update_tracked_user(&event.sender, true).await?;
        }

        Ok(())
    }

    pub async fn receive_secret_event(
        &self,
        sender_key: Curve25519PublicKey,
        event: &DecryptedSecretSendEvent,
    ) -> Result<Option<SecretName>, CryptoStoreError> {
        debug!(
            sender = event.sender.as_str(),
            request_id = event.content.request_id.as_str(),
            "Received a m.secret.send event"
        );

        let request_id = <&TransactionId>::from(event.content.request_id.as_str());

        Ok(if let Some(request) = self.store.get_outgoing_secret_requests(request_id).await? {
            match &request.info {
                SecretInfo::KeyRequest(_) => {
                    warn!(
                        sender = event.sender.as_str(),
                        request_id = event.content.request_id.as_str(),
                        "Received a m.secret.send event but the request was for a room key"
                    );

                    None
                }
                SecretInfo::SecretRequest(secret_name) => {
                    self.receive_secret(sender_key, event, &request, secret_name).await?;

                    Some(secret_name.to_owned())
                }
            }
        } else {
            None
        })
    }

    async fn accept_forwarded_room_key(
        &self,
        info: &GossipRequest,
        sender: &UserId,
        sender_key: Curve25519PublicKey,
        algorithm: EventEncryptionAlgorithm,
        content: &ForwardedMegolmV1AesSha2Content,
    ) -> Result<Option<InboundGroupSession>, CryptoStoreError> {
        match InboundGroupSession::from_forwarded_key(&algorithm, content) {
            Ok(session) => {
                let old_session = self
                    .store
                    .get_inbound_group_session(
                        session.room_id(),
                        &session.sender_key.to_base64(),
                        session.session_id(),
                    )
                    .await?;

                let session_id = session.session_id().to_owned();

                // If we have a previous session, check if we have a better version
                // and store the new one if so.
                let session = if let Some(old_session) = old_session {
                    if session.compare(&old_session).await == SessionOrdering::Better {
                        self.mark_as_done(info).await?;
                        Some(session)
                    } else {
                        None
                    }
                // If we didn't have a previous session, store it.
                } else {
                    self.mark_as_done(info).await?;
                    Some(session)
                };

                if let Some(s) = &session {
                    info!(
                        %sender,
                        sender_key = sender_key.to_base64(),
                        claimed_sender_key = content.claimed_sender_key.to_base64(),
                        room_id = s.room_id().as_str(),
                        session_id = session_id.as_str(),
                        %algorithm,
                        "Received a forwarded room key",
                    );
                } else {
                    info!(
                        %sender,
                        sender_key = sender_key.to_base64(),
                        claimed_sender_key = content.claimed_sender_key.to_base64(),
                        room_id = %content.room_id,
                        session_id = session_id.as_str(),
                        %algorithm,
                        "Received a forwarded room key but we already have a better version of it",
                    );
                }

                Ok(session)
            }
            Err(e) => {
                warn!(
                    %sender,
                    sender_key = sender_key.to_base64(),
                    claimed_sender_key = content.claimed_sender_key.to_base64(),
                    room_id = content.room_id.as_str(),
                    %algorithm,
                    "Couldn't create a group session from a received room key"
                );
                Err(e.into())
            }
        }
    }

    /// Receive a forwarded room key event that was sent using any of our
    /// supported content types.
    async fn receive_supported_keys(
        &self,
        sender_key: Curve25519PublicKey,
        event: &DecryptedForwardedRoomKeyEvent,
        content: &ForwardedMegolmV1AesSha2Content,
    ) -> Result<Option<InboundGroupSession>, CryptoStoreError> {
        let algorithm = event.content.algorithm();

        if let Some(info) = self.get_key_info(event).await? {
            self.accept_forwarded_room_key(&info, &event.sender, sender_key, algorithm, content)
                .await
        } else {
            warn!(
                sender = %event.sender,
                sender_key = %sender_key,
                room_id = %content.room_id,
                session_id = content.session_id.as_str(),
                claimed_sender_key = %content.claimed_sender_key,
                algorithm = %algorithm,
                "Received a forwarded room key that we didn't request",
            );

            Ok(None)
        }
    }

    /// Receive a forwarded room key event.
    pub async fn receive_forwarded_room_key(
        &self,
        sender_key: Curve25519PublicKey,
        event: &DecryptedForwardedRoomKeyEvent,
    ) -> Result<Option<InboundGroupSession>, CryptoStoreError> {
        match &event.content {
            ForwardedRoomKeyContent::MegolmV1AesSha2(content) => {
                self.receive_supported_keys(sender_key, event, content).await
            }
            #[cfg(feature = "experimental-algorithms")]
            ForwardedRoomKeyContent::MegolmV2AesSha2(content) => {
                self.receive_supported_keys(sender_key, event, content).await
            }
            ForwardedRoomKeyContent::Unknown(_) => {
                warn!(
                    sender = event.sender.as_str(),
                    sender_key = sender_key.to_base64(),
                    algorithm = %event.content.algorithm(),
                    "Received a forwarded room key with an unsupported algorithm",
                );

                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use dashmap::DashMap;
    use matches::assert_matches;
    use matrix_sdk_common::locks::Mutex;
    use matrix_sdk_test::async_test;
    use ruma::{
        device_id, event_id,
        events::{
            secret::request::{RequestAction, SecretName, ToDeviceSecretRequestEventContent},
            AnyToDeviceEventContent, ToDeviceEvent as RumaToDeviceEvent,
        },
        room_id,
        serde::Raw,
        user_id, DeviceId, RoomId, UserId,
    };
    use serde::{de::DeserializeOwned, Serialize};
    use serde_json::json;

    use super::{GossipMachine, KeyForwardDecision};
    use crate::{
        identities::{LocalTrust, ReadOnlyDevice},
        olm::{Account, OutboundGroupSession, PrivateCrossSigningIdentity, ReadOnlyAccount},
        session_manager::GroupSessionCache,
        store::{Changes, CryptoStore, MemoryStore, Store},
        types::events::{
            forwarded_room_key::ForwardedRoomKeyContent,
            olm_v1::{AnyDecryptedOlmEvent, DecryptedOlmV1Event},
            room::encrypted::{EncryptedEvent, EncryptedToDeviceEvent, RoomEncryptedEventContent},
            EventType, ToDeviceEvent,
        },
        verification::VerificationMachine,
        OutgoingRequest, OutgoingRequests,
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
        device_id!("ILMLKASTES")
    }

    fn alice2_device_id() -> &'static DeviceId {
        device_id!("ILMLKASTES")
    }

    fn room_id() -> &'static RoomId {
        room_id!("!test:example.org")
    }

    fn account() -> ReadOnlyAccount {
        ReadOnlyAccount::new(alice_id(), alice_device_id())
    }

    fn bob_account() -> ReadOnlyAccount {
        ReadOnlyAccount::new(bob_id(), bob_device_id())
    }

    fn alice_2_account() -> ReadOnlyAccount {
        ReadOnlyAccount::new(alice_id(), alice2_device_id())
    }

    fn test_gossip_machine(user_id: &UserId) -> GossipMachine {
        let user_id = Arc::from(user_id);
        let device_id = DeviceId::new();

        let account = ReadOnlyAccount::new(&user_id, &device_id);
        let store: Arc<dyn CryptoStore> = Arc::new(MemoryStore::new());
        let identity = Arc::new(Mutex::new(PrivateCrossSigningIdentity::empty(alice_id())));
        let verification = VerificationMachine::new(account, identity.clone(), store.clone());
        let store = Store::new(user_id.to_owned(), identity, store, verification);
        let session_cache = GroupSessionCache::new(store.clone());

        GossipMachine::new(
            user_id,
            device_id.into(),
            store,
            session_cache,
            Arc::new(DashMap::new()),
        )
    }

    async fn get_machine() -> GossipMachine {
        let user_id: Arc<UserId> = alice_id().into();
        let account = ReadOnlyAccount::new(&user_id, alice_device_id());
        let device = ReadOnlyDevice::from_account(&account).await;
        let another_device =
            ReadOnlyDevice::from_account(&ReadOnlyAccount::new(&user_id, alice2_device_id())).await;

        let store: Arc<dyn CryptoStore> = Arc::new(MemoryStore::new());
        let identity = Arc::new(Mutex::new(PrivateCrossSigningIdentity::empty(alice_id())));
        let verification = VerificationMachine::new(account, identity.clone(), store.clone());

        let store = Store::new(user_id.clone(), identity, store, verification);
        store.save_devices(&[device, another_device]).await.unwrap();
        let session_cache = GroupSessionCache::new(store.clone());

        GossipMachine::new(
            user_id,
            alice_device_id().into(),
            store,
            session_cache,
            Arc::new(DashMap::new()),
        )
    }

    async fn machines_for_key_share(
        create_sessions: bool,
    ) -> (GossipMachine, Account, OutboundGroupSession, GossipMachine) {
        let alice_machine = get_machine().await;
        let alice_account = Account {
            inner: alice_machine.store.account().clone(),
            store: alice_machine.store.clone(),
        };
        let alice_device = ReadOnlyDevice::from_account(alice_machine.store.account()).await;

        let bob_machine = test_gossip_machine(alice_id());
        let bob_device = ReadOnlyDevice::from_account(bob_machine.store.account()).await;

        // We need a trusted device, otherwise we won't request keys
        bob_device.set_trust_state(LocalTrust::Verified);
        alice_machine.store.save_devices(&[bob_device]).await.unwrap();
        bob_machine.store.save_devices(&[alice_device.clone()]).await.unwrap();

        if create_sessions {
            // Create Olm sessions for our two accounts.
            let (alice_session, bob_session) =
                alice_machine.store.account().create_session_for(bob_machine.store.account()).await;

            // Populate our stores with Olm sessions and a Megolm session.

            alice_machine.store.save_sessions(&[alice_session]).await.unwrap();
            bob_machine.store.save_sessions(&[bob_session]).await.unwrap();
        }

        let (group_session, inbound_group_session) =
            bob_machine.store.account().create_group_session_pair_with_defaults(room_id()).await;

        let content = group_session.encrypt(json!({}), "m.dummy").await;
        let event = wrap_encrypted_content(bob_machine.user_id(), content);
        bob_machine.store.save_inbound_group_sessions(&[inbound_group_session]).await.unwrap();

        // Alice wants to request the outbound group session from bob.
        assert!(
            alice_machine.create_outgoing_key_request(room_id(), &event,).await.unwrap(),
            "We should request a room key"
        );

        group_session
            .mark_shared_with(
                alice_device.user_id(),
                alice_device.device_id(),
                alice_device.curve25519_key().unwrap(),
            )
            .await;

        // Put the outbound session into bobs store.
        bob_machine.outbound_group_sessions.insert(group_session.clone());

        (alice_machine, alice_account, group_session, bob_machine)
    }

    fn extract_content<'a>(
        recipient: &UserId,
        request: &'a OutgoingRequest,
    ) -> &'a Raw<AnyToDeviceEventContent> {
        request
            .request()
            .to_device()
            .expect("The request should be always a to-device request")
            .messages
            .get(recipient)
            .unwrap()
            .values()
            .next()
            .unwrap()
    }

    fn wrap_encrypted_content(
        sender: &UserId,
        content: Raw<RoomEncryptedEventContent>,
    ) -> EncryptedEvent {
        let content = content.deserialize().unwrap();

        EncryptedEvent {
            sender: sender.to_owned(),
            event_id: event_id!("$143273582443PhrSn:example.org").to_owned(),
            content,
            origin_server_ts: ruma::MilliSecondsSinceUnixEpoch::now(),
            unsigned: Default::default(),
            other: Default::default(),
        }
    }

    fn request_to_event<C>(
        recipient: &UserId,
        sender: &UserId,
        request: &OutgoingRequest,
    ) -> ToDeviceEvent<C>
    where
        C: EventType + DeserializeOwned + Serialize + std::fmt::Debug,
    {
        let content = extract_content(recipient, request);
        let content: C = content
            .deserialize_as()
            .expect("We can always deserialize the to-device event content");

        ToDeviceEvent { sender: sender.to_owned(), content, other: Default::default() }
    }

    #[async_test]
    async fn create_machine() {
        let machine = get_machine().await;

        assert!(machine.outgoing_to_device_requests().await.unwrap().is_empty());
    }

    #[async_test]
    async fn re_request_keys() {
        let machine = get_machine().await;
        let account = account();

        let (outbound, session) = account.create_group_session_pair_with_defaults(room_id()).await;

        let content = outbound.encrypt(json!({}), "m.dummy").await;
        let event = wrap_encrypted_content(machine.user_id(), content);

        assert!(machine.outgoing_to_device_requests().await.unwrap().is_empty());
        let (cancel, request) = machine.request_key(session.room_id(), &event).await.unwrap();

        assert!(cancel.is_none());

        machine.mark_outgoing_request_as_sent(&request.request_id).await.unwrap();

        let (cancel, _) = machine.request_key(session.room_id(), &event).await.unwrap();

        assert!(cancel.is_some());
    }

    #[async_test]
    async fn create_key_request() {
        let machine = get_machine().await;
        let account = account();
        let second_account = alice_2_account();
        let alice_device = ReadOnlyDevice::from_account(&second_account).await;

        // We need a trusted device, otherwise we won't request keys
        alice_device.set_trust_state(LocalTrust::Verified);
        machine.store.save_devices(&[alice_device]).await.unwrap();

        let (outbound, session) = account.create_group_session_pair_with_defaults(room_id()).await;
        let content = outbound.encrypt(json!({}), "m.dummy").await;
        let event = wrap_encrypted_content(machine.user_id(), content);

        assert!(machine.outgoing_to_device_requests().await.unwrap().is_empty());
        machine.create_outgoing_key_request(session.room_id(), &event).await.unwrap();
        assert!(!machine.outgoing_to_device_requests().await.unwrap().is_empty());
        assert_eq!(machine.outgoing_to_device_requests().await.unwrap().len(), 1);

        machine.create_outgoing_key_request(session.room_id(), &event).await.unwrap();

        let requests = machine.outgoing_to_device_requests().await.unwrap();
        assert_eq!(requests.len(), 1);

        let request = requests.get(0).unwrap();

        machine.mark_outgoing_request_as_sent(&request.request_id).await.unwrap();
        assert!(machine.outgoing_to_device_requests().await.unwrap().is_empty());
    }

    #[async_test]
    async fn receive_forwarded_key() {
        let machine = get_machine().await;
        let account = account();

        let second_account = alice_2_account();
        let alice_device = ReadOnlyDevice::from_account(&second_account).await;

        // We need a trusted device, otherwise we won't request keys
        alice_device.set_trust_state(LocalTrust::Verified);
        machine.store.save_devices(&[alice_device.clone()]).await.unwrap();

        let (outbound, session) = account.create_group_session_pair_with_defaults(room_id()).await;
        let content = outbound.encrypt(json!({}), "m.dummy").await;
        let room_event = wrap_encrypted_content(machine.user_id(), content);

        machine.create_outgoing_key_request(session.room_id(), &room_event).await.unwrap();

        let requests = machine.outgoing_to_device_requests().await.unwrap();
        let request = requests.get(0).unwrap();
        let id = &request.request_id;

        machine.mark_outgoing_request_as_sent(id).await.unwrap();

        let export = session.export_at_index(10).await;

        let content: ForwardedRoomKeyContent = export.try_into().unwrap();

        let event = DecryptedOlmV1Event::new(
            alice_id(),
            alice_id(),
            alice_device.ed25519_key().unwrap(),
            content,
        );

        assert!(machine
            .store
            .get_inbound_group_session(
                session.room_id(),
                &session.sender_key.to_base64(),
                session.session_id(),
            )
            .await
            .unwrap()
            .is_none());

        let first_session = machine
            .receive_forwarded_room_key(alice_device.curve25519_key().unwrap(), &event)
            .await
            .unwrap();
        let first_session = first_session.unwrap();

        assert_eq!(first_session.first_known_index(), 10);

        machine.store.save_inbound_group_sessions(&[first_session.clone()]).await.unwrap();

        // Get the cancel request.
        let request = machine.outgoing_requests.iter().next().unwrap();
        let id = request.request_id.clone();
        drop(request);
        machine.mark_outgoing_request_as_sent(&id).await.unwrap();

        machine.create_outgoing_key_request(session.room_id(), &room_event).await.unwrap();

        let requests = machine.outgoing_to_device_requests().await.unwrap();
        let request = &requests[0];

        machine.mark_outgoing_request_as_sent(&request.request_id).await.unwrap();

        let export = session.export_at_index(15).await;

        let content: ForwardedRoomKeyContent = export.try_into().unwrap();

        let event = DecryptedOlmV1Event::new(
            alice_id(),
            alice_id(),
            alice_device.ed25519_key().unwrap(),
            content,
        );

        let second_session = machine
            .receive_forwarded_room_key(alice_device.curve25519_key().unwrap(), &event)
            .await
            .unwrap();

        assert!(second_session.is_none());

        let export = session.export_at_index(0).await;

        let content: ForwardedRoomKeyContent = export.try_into().unwrap();

        let event = DecryptedOlmV1Event::new(
            alice_id(),
            alice_id(),
            alice_device.ed25519_key().unwrap(),
            content,
        );

        let second_session = machine
            .receive_forwarded_room_key(alice_device.curve25519_key().unwrap(), &event)
            .await
            .unwrap();

        assert_eq!(second_session.unwrap().first_known_index(), 0);
    }

    #[async_test]
    async fn should_share_key_test() {
        let machine = get_machine().await;
        let account = account();

        let own_device =
            machine.store.get_device(alice_id(), alice2_device_id()).await.unwrap().unwrap();

        let (outbound, inbound) = account.create_group_session_pair_with_defaults(room_id()).await;

        // We don't share keys with untrusted devices.
        assert_matches!(
            machine.should_share_key(&own_device, &inbound).await,
            Err(KeyForwardDecision::UntrustedDevice)
        );
        own_device.set_trust_state(LocalTrust::Verified);
        // Now we do want to share the keys.
        machine.should_share_key(&own_device, &inbound).await.unwrap();

        let bob_device = ReadOnlyDevice::from_account(&bob_account()).await;
        machine.store.save_devices(&[bob_device]).await.unwrap();

        let bob_device =
            machine.store.get_device(bob_id(), bob_device_id()).await.unwrap().unwrap();

        // We don't share sessions with other user's devices if no outbound
        // session was provided.
        assert_matches!(
            machine.should_share_key(&bob_device, &inbound).await,
            Err(KeyForwardDecision::MissingOutboundSession)
        );

        let mut changes = Changes::default();

        changes.outbound_group_sessions.push(outbound.clone());
        changes.inbound_group_sessions.push(inbound.clone());
        machine.store.save_changes(changes).await.unwrap();
        machine.outbound_group_sessions.insert(outbound.clone());

        // We don't share sessions with other user's devices if the session
        // wasn't shared in the first place.
        assert_matches!(
            machine.should_share_key(&bob_device, &inbound).await,
            Err(KeyForwardDecision::OutboundSessionNotShared)
        );

        bob_device.set_trust_state(LocalTrust::Verified);

        // We don't share sessions with other user's devices if the session
        // wasn't shared in the first place even if the device is trusted.
        assert_matches!(
            machine.should_share_key(&bob_device, &inbound).await,
            Err(KeyForwardDecision::OutboundSessionNotShared)
        );

        // We now share the session, since it was shared before.
        outbound
            .mark_shared_with(
                bob_device.user_id(),
                bob_device.device_id(),
                bob_device.curve25519_key().unwrap(),
            )
            .await;
        machine.should_share_key(&bob_device, &inbound).await.unwrap();

        let (other_outbound, other_inbound) =
            account.create_group_session_pair_with_defaults(room_id()).await;

        // But we don't share some other session that doesn't match our outbound
        // session.
        assert_matches!(
            machine.should_share_key(&bob_device, &other_inbound).await,
            Err(KeyForwardDecision::MissingOutboundSession)
        );

        // Finally, let's ensure we don't share the session with a device that rotated
        // its curve25519 key.
        let bob_device = ReadOnlyDevice::from_account(&bob_account()).await;
        machine.store.save_devices(&[bob_device]).await.unwrap();

        let bob_device =
            machine.store.get_device(bob_id(), bob_device_id()).await.unwrap().unwrap();
        assert_matches!(
            machine.should_share_key(&bob_device, &inbound).await,
            Err(KeyForwardDecision::ChangedSenderKey)
        );

        // Now let's encrypt some messages in another session to increment the message
        // index and then share it with our own untrusted device.
        own_device.set_trust_state(LocalTrust::Unset);

        for _ in 1..=3 {
            other_outbound.encrypt_helper("foo".to_owned()).await;
        }
        other_outbound
            .mark_shared_with(
                own_device.user_id(),
                own_device.device_id(),
                own_device.curve25519_key().unwrap(),
            )
            .await;

        machine.outbound_group_sessions.insert(other_outbound.clone());

        // Since our device is untrusted, we should share the session starting only from
        // the current index (at which the message was marked as shared). This
        // should be 3 since we encrypted 3 messages.
        assert_matches!(machine.should_share_key(&own_device, &other_inbound).await, Ok(Some(3)));

        own_device.set_trust_state(LocalTrust::Verified);

        // However once our device is trusted, we share the entire session.
        assert_matches!(machine.should_share_key(&own_device, &other_inbound).await, Ok(None));
    }

    #[async_test]
    async fn key_share_cycle() {
        let (alice_machine, alice_account, group_session, bob_machine) =
            machines_for_key_share(true).await;

        // Get the request and convert it into a event.
        let requests = alice_machine.outgoing_to_device_requests().await.unwrap();
        let request = &requests[0];
        let event = request_to_event(alice_id(), alice_id(), request);

        alice_machine.mark_outgoing_request_as_sent(&request.request_id).await.unwrap();

        // Bob doesn't have any outgoing requests.
        assert!(bob_machine.outgoing_requests.is_empty());

        // Receive the room key request from alice.
        bob_machine.receive_incoming_key_request(&event);
        bob_machine.collect_incoming_key_requests().await.unwrap();
        // Now bob does have an outgoing request.
        assert!(!bob_machine.outgoing_requests.is_empty());

        // Get the request and convert it to a encrypted to-device event.
        let requests = bob_machine.outgoing_to_device_requests().await.unwrap();
        let request = &requests[0];

        let event: EncryptedToDeviceEvent = request_to_event(alice_id(), alice_id(), request);
        bob_machine.mark_outgoing_request_as_sent(&request.request_id).await.unwrap();

        // Check that alice doesn't have the session.
        assert!(alice_machine
            .store
            .get_inbound_group_session(
                room_id(),
                &bob_machine.store.account().identity_keys().curve25519.to_base64(),
                group_session.session_id()
            )
            .await
            .unwrap()
            .is_none());

        let decrypted = alice_account.decrypt_to_device_event(&event).await.unwrap();

        if let AnyDecryptedOlmEvent::ForwardedRoomKey(e) = decrypted.result.event {
            let session = alice_machine
                .receive_forwarded_room_key(decrypted.result.sender_key, &e)
                .await
                .unwrap();
            alice_machine.store.save_inbound_group_sessions(&[session.unwrap()]).await.unwrap();
        } else {
            panic!("Invalid decrypted event type");
        }

        // Check that alice now does have the session.
        let session = alice_machine
            .store
            .get_inbound_group_session(
                room_id(),
                &decrypted.result.sender_key.to_base64(),
                group_session.session_id(),
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(session.session_id(), group_session.session_id())
    }

    #[async_test]
    async fn secret_share_cycle() {
        let alice_machine = get_machine().await;
        let alice_account = Account { inner: account(), store: alice_machine.store.clone() };

        let second_account = alice_2_account();
        let alice_device = ReadOnlyDevice::from_account(&second_account).await;

        let bob_account = bob_account();
        let bob_device = ReadOnlyDevice::from_account(&bob_account).await;

        alice_machine.store.save_devices(&[alice_device.clone()]).await.unwrap();

        // Create Olm sessions for our two accounts.
        let (alice_session, _) = alice_account.create_session_for(&second_account).await;

        alice_machine.store.save_sessions(&[alice_session]).await.unwrap();

        let event = RumaToDeviceEvent {
            sender: bob_account.user_id().to_owned(),
            content: ToDeviceSecretRequestEventContent::new(
                RequestAction::Request(SecretName::CrossSigningMasterKey),
                bob_account.device_id().to_owned(),
                "request_id".into(),
            ),
        };

        // No secret found
        assert!(alice_machine.outgoing_requests.is_empty());
        alice_machine.receive_incoming_secret_request(&event);
        alice_machine.collect_incoming_key_requests().await.unwrap();
        assert!(alice_machine.outgoing_requests.is_empty());

        // No device found
        alice_machine.store.reset_cross_signing_identity().await;
        alice_machine.receive_incoming_secret_request(&event);
        alice_machine.collect_incoming_key_requests().await.unwrap();
        assert!(alice_machine.outgoing_requests.is_empty());

        alice_machine.store.save_devices(&[bob_device]).await.unwrap();

        // The device doesn't belong to us
        alice_machine.store.reset_cross_signing_identity().await;
        alice_machine.receive_incoming_secret_request(&event);
        alice_machine.collect_incoming_key_requests().await.unwrap();
        assert!(alice_machine.outgoing_requests.is_empty());

        let event = RumaToDeviceEvent {
            sender: alice_id().to_owned(),
            content: ToDeviceSecretRequestEventContent::new(
                RequestAction::Request(SecretName::CrossSigningMasterKey),
                second_account.device_id().into(),
                "request_id".into(),
            ),
        };

        // The device isn't trusted
        alice_machine.receive_incoming_secret_request(&event);
        alice_machine.collect_incoming_key_requests().await.unwrap();
        assert!(alice_machine.outgoing_requests.is_empty());

        // We need a trusted device, otherwise we won't serve secrets
        alice_device.set_trust_state(LocalTrust::Verified);
        alice_machine.store.save_devices(&[alice_device.clone()]).await.unwrap();

        alice_machine.receive_incoming_secret_request(&event);
        alice_machine.collect_incoming_key_requests().await.unwrap();
        assert!(!alice_machine.outgoing_requests.is_empty());
    }

    #[async_test]
    async fn key_share_cycle_without_session() {
        let (alice_machine, alice_account, group_session, bob_machine) =
            machines_for_key_share(false).await;

        // Get the request and convert it into a event.
        let requests = alice_machine.outgoing_to_device_requests().await.unwrap();
        let request = &requests[0];
        let event = request_to_event(alice_id(), alice_id(), request);

        alice_machine.mark_outgoing_request_as_sent(&request.request_id).await.unwrap();

        // Bob doesn't have any outgoing requests.
        assert!(bob_machine.outgoing_to_device_requests().await.unwrap().is_empty());
        assert!(bob_machine.users_for_key_claim.is_empty());
        assert!(bob_machine.wait_queue.is_empty());

        // Receive the room key request from alice.
        bob_machine.receive_incoming_key_request(&event);
        bob_machine.collect_incoming_key_requests().await.unwrap();
        // Bob only has a keys claim request, since we're lacking a session
        assert_eq!(bob_machine.outgoing_to_device_requests().await.unwrap().len(), 1);
        assert_matches!(
            bob_machine.outgoing_to_device_requests().await.unwrap().first().unwrap().request(),
            OutgoingRequests::KeysClaim(_)
        );
        assert!(!bob_machine.users_for_key_claim.is_empty());
        assert!(!bob_machine.wait_queue.is_empty());

        let (alice_session, bob_session) =
            alice_machine.store.account().create_session_for(bob_machine.store.account()).await;
        // We create a session now.
        alice_machine.store.save_sessions(&[alice_session]).await.unwrap();
        bob_machine.store.save_sessions(&[bob_session]).await.unwrap();

        bob_machine.retry_keyshare(alice_id(), alice_device_id());
        assert!(bob_machine.users_for_key_claim.is_empty());
        bob_machine.collect_incoming_key_requests().await.unwrap();
        // Bob now has an outgoing requests.
        assert!(!bob_machine.outgoing_to_device_requests().await.unwrap().is_empty());
        assert!(bob_machine.wait_queue.is_empty());

        // Get the request and convert it to a encrypted to-device event.
        let requests = bob_machine.outgoing_to_device_requests().await.unwrap();
        let request = &requests[0];

        let event: EncryptedToDeviceEvent = request_to_event(alice_id(), alice_id(), request);
        bob_machine.mark_outgoing_request_as_sent(&request.request_id).await.unwrap();

        // Check that alice doesn't have the session.
        assert!(alice_machine
            .store
            .get_inbound_group_session(
                room_id(),
                &bob_machine.store.account().identity_keys().curve25519.to_base64(),
                group_session.session_id()
            )
            .await
            .unwrap()
            .is_none());

        let decrypted = alice_account.decrypt_to_device_event(&event).await.unwrap();

        if let AnyDecryptedOlmEvent::ForwardedRoomKey(e) = decrypted.result.event {
            let session = alice_machine
                .receive_forwarded_room_key(decrypted.result.sender_key, &e)
                .await
                .unwrap();
            alice_machine.store.save_inbound_group_sessions(&[session.unwrap()]).await.unwrap();
        } else {
            panic!("Invalid decrypted event type");
        }

        // Check that alice now does have the session.
        let session = alice_machine
            .store
            .get_inbound_group_session(
                room_id(),
                &decrypted.result.sender_key.to_base64(),
                group_session.session_id(),
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(session.session_id(), group_session.session_id())
    }
}
