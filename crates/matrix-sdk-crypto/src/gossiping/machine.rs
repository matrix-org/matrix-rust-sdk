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

use std::{
    collections::{btree_map::Entry, BTreeMap, BTreeSet},
    mem,
    sync::{atomic::AtomicBool, Arc, RwLock as StdRwLock},
};

use atomic::Ordering;
use ruma::{
    api::client::keys::claim_keys::v3::Request as KeysClaimRequest,
    events::secret::request::{
        RequestAction, SecretName, ToDeviceSecretRequestEvent as SecretRequestEvent,
    },
    DeviceId, DeviceKeyAlgorithm, OwnedDeviceId, OwnedTransactionId, OwnedUserId, RoomId,
    TransactionId, UserId,
};
use tracing::{debug, field::debug, info, instrument, trace, warn, Span};
use vodozemac::{megolm::SessionOrdering, Curve25519PublicKey};

use super::{GossipRequest, GossippedSecret, RequestEvent, RequestInfo, SecretInfo, WaitQueue};
use crate::{
    error::{EventError, OlmError, OlmResult},
    olm::{InboundGroupSession, Session},
    requests::{OutgoingRequest, ToDeviceRequest},
    session_manager::GroupSessionCache,
    store::{Changes, CryptoStoreError, SecretImportError, Store},
    types::events::{
        forwarded_room_key::ForwardedRoomKeyContent,
        olm_v1::{DecryptedForwardedRoomKeyEvent, DecryptedSecretSendEvent},
        room::encrypted::EncryptedEvent,
        room_key_request::RoomKeyRequestEvent,
        secret_send::SecretSendContent,
        EventType,
    },
    Device, MegolmError,
};

#[derive(Clone, Debug)]
pub(crate) struct GossipMachine {
    inner: Arc<GossipMachineInner>,
}

#[derive(Debug)]
pub(crate) struct GossipMachineInner {
    store: Store,
    #[cfg(feature = "automatic-room-key-forwarding")]
    outbound_group_sessions: GroupSessionCache,
    outgoing_requests: StdRwLock<BTreeMap<OwnedTransactionId, OutgoingRequest>>,
    incoming_key_requests: StdRwLock<BTreeMap<RequestInfo, RequestEvent>>,
    wait_queue: WaitQueue,
    users_for_key_claim: Arc<StdRwLock<BTreeMap<OwnedUserId, BTreeSet<OwnedDeviceId>>>>,
    room_key_forwarding_enabled: AtomicBool,
}

impl GossipMachine {
    pub fn new(
        store: Store,
        #[allow(unused)] outbound_group_sessions: GroupSessionCache,
        users_for_key_claim: Arc<StdRwLock<BTreeMap<OwnedUserId, BTreeSet<OwnedDeviceId>>>>,
    ) -> Self {
        let room_key_forwarding_enabled =
            AtomicBool::new(cfg!(feature = "automatic-room-key-forwarding"));

        Self {
            inner: Arc::new(GossipMachineInner {
                store,
                #[cfg(feature = "automatic-room-key-forwarding")]
                outbound_group_sessions,
                outgoing_requests: Default::default(),
                incoming_key_requests: Default::default(),
                wait_queue: WaitQueue::new(),
                users_for_key_claim,
                room_key_forwarding_enabled,
            }),
        }
    }

    #[cfg(feature = "automatic-room-key-forwarding")]
    pub fn toggle_room_key_forwarding(&self, enabled: bool) {
        self.inner.room_key_forwarding_enabled.store(enabled, Ordering::SeqCst)
    }

    pub fn is_room_key_forwarding_enabled(&self) -> bool {
        self.inner.room_key_forwarding_enabled.load(Ordering::SeqCst)
    }

    /// Load stored outgoing requests that were not yet sent out.
    async fn load_outgoing_requests(&self) -> Result<Vec<OutgoingRequest>, CryptoStoreError> {
        Ok(self
            .inner
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
        &self.inner.store.static_account().user_id
    }

    /// Our own device ID.
    pub fn device_id(&self) -> &DeviceId {
        &self.inner.store.static_account().device_id
    }

    pub async fn outgoing_to_device_requests(
        &self,
    ) -> Result<Vec<OutgoingRequest>, CryptoStoreError> {
        let mut key_requests = self.load_outgoing_requests().await?;
        let key_forwards: Vec<OutgoingRequest> =
            self.inner.outgoing_requests.read().unwrap().values().cloned().collect();
        key_requests.extend(key_forwards);

        let users_for_key_claim: BTreeMap<_, _> = self
            .inner
            .users_for_key_claim
            .read()
            .unwrap()
            .iter()
            .map(|(key, value)| {
                let device_map = value
                    .iter()
                    .map(|d| (d.to_owned(), DeviceKeyAlgorithm::SignedCurve25519))
                    .collect();

                (key.to_owned(), device_map)
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
            self.inner.incoming_key_requests.write().unwrap().insert(request_info, event);
        }
    }

    pub fn receive_incoming_secret_request(&self, event: &SecretRequestEvent) {
        self.receive_event(event.clone().into())
    }

    /// Handle all the incoming key requests that are queued up and empty our
    /// key request queue.
    pub async fn collect_incoming_key_requests(&self) -> OlmResult<Vec<Session>> {
        let mut changed_sessions = Vec::new();

        let incoming_key_requests =
            mem::take(&mut *self.inner.incoming_key_requests.write().unwrap());

        for event in incoming_key_requests.values() {
            if let Some(s) = match event {
                #[cfg(feature = "automatic-room-key-forwarding")]
                RequestEvent::KeyShare(e) => Box::pin(self.handle_key_request(e)).await?,
                RequestEvent::Secret(e) => Box::pin(self.handle_secret_request(e)).await?,
                #[cfg(not(feature = "automatic-room-key-forwarding"))]
                _ => None,
            } {
                changed_sessions.push(s);
            }
        }

        Ok(changed_sessions)
    }

    /// Store the key share request for later, once we get an Olm session with
    /// the given device [`retry_keyshare`](#method.retry_keyshare) should be
    /// called.
    fn handle_key_share_without_session(&self, device: Device, event: RequestEvent) {
        self.inner
            .users_for_key_claim
            .write()
            .unwrap()
            .entry(device.user_id().to_owned())
            .or_default()
            .insert(device.device_id().into());
        self.inner.wait_queue.insert(&device, event);
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
        if let Entry::Occupied(mut e) =
            self.inner.users_for_key_claim.write().unwrap().entry(user_id.to_owned())
        {
            e.get_mut().remove(device_id);

            if e.get().is_empty() {
                e.remove();
            }
        }

        let mut incoming_key_requests = self.inner.incoming_key_requests.write().unwrap();
        for (key, event) in self.inner.wait_queue.remove(user_id, device_id) {
            incoming_key_requests.entry(key).or_insert(event);
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

        let content = if let Some(secret) = self.inner.store.export_secret(secret_name).await? {
            SecretSendContent::new(event.content.request_id.to_owned(), secret)
        } else {
            info!(?secret_name, "Can't serve a secret request, secret isn't found");
            return Ok(None);
        };

        let device =
            self.inner.store.get_device(&event.sender, &event.content.requesting_device_id).await?;

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
            self.inner.store.mark_user_as_changed(&event.sender).await?;

            None
        })
    }

    /// Try to encrypt the given `InboundGroupSession` for the given `Device` as
    /// a forwarded room key.
    ///
    /// This method might fail if we do not share an 1-to-1 Olm session with the
    /// given `Device`, in that case we're going to queue up an
    /// `/keys/claim` request to be sent out and retry once the 1-to-1 Olm
    /// session has been established.
    #[cfg(feature = "automatic-room-key-forwarding")]
    async fn try_to_forward_room_key(
        &self,
        event: &RoomKeyRequestEvent,
        device: Device,
        session: &InboundGroupSession,
        message_index: Option<u32>,
    ) -> OlmResult<Option<Session>> {
        info!(?message_index, "Serving a room key request",);

        match self.forward_room_key(session, &device, message_index).await {
            Ok(s) => Ok(Some(s)),
            Err(OlmError::MissingSession) => {
                info!(
                    "Key request is missing an Olm session, putting the request in the wait queue",
                );
                self.handle_key_share_without_session(device, event.to_owned().into());

                Ok(None)
            }
            Err(OlmError::SessionExport(e)) => {
                warn!(
                    "Can't serve a room key request, the session \
                     can't be exported into a forwarded room key: {e:?}",
                );
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// Answer a room key request after we found the matching
    /// `InboundGroupSession`.
    #[cfg(feature = "automatic-room-key-forwarding")]
    async fn answer_room_key_request(
        &self,
        event: &RoomKeyRequestEvent,
        session: &InboundGroupSession,
    ) -> OlmResult<Option<Session>> {
        use super::KeyForwardDecision;

        let device =
            self.inner.store.get_device(&event.sender, &event.content.requesting_device_id).await?;

        let Some(device) = device else {
            warn!("Received a key request from an unknown device");
            self.inner.store.mark_user_as_changed(&event.sender).await?;

            return Ok(None);
        };

        match self.should_share_key(&device, session).await {
            Ok(message_index) => {
                self.try_to_forward_room_key(event, device, session, message_index).await
            }
            Err(e) => {
                if let KeyForwardDecision::ChangedSenderKey = e {
                    warn!(
                        "Received a key request from a device that changed \
                         their Curve25519 sender key"
                    );
                } else {
                    debug!(
                        reason = ?e,
                        "Received a key request that we won't serve",
                    );
                }

                Ok(None)
            }
        }
    }

    #[cfg(feature = "automatic-room-key-forwarding")]
    #[tracing::instrument(
        skip_all,
        fields(
            user_id = %event.sender,
            device_id = %event.content.requesting_device_id,
            %room_id,
            session_id
        )
    )]
    async fn handle_supported_key_request(
        &self,
        event: &RoomKeyRequestEvent,
        room_id: &RoomId,
        session_id: &str,
    ) -> OlmResult<Option<Session>> {
        let session = self.inner.store.get_inbound_group_session(room_id, session_id).await?;

        if let Some(s) = session {
            self.answer_room_key_request(event, &s).await
        } else {
            debug!("Received a room key request for an unknown inbound group session",);

            Ok(None)
        }
    }

    /// Handle a single incoming key request.
    #[cfg(feature = "automatic-room-key-forwarding")]
    async fn handle_key_request(&self, event: &RoomKeyRequestEvent) -> OlmResult<Option<Session>> {
        use crate::types::events::room_key_request::{Action, RequestedKeyInfo};

        if self.inner.room_key_forwarding_enabled.load(Ordering::SeqCst) {
            match &event.content.action {
                Action::Request(info) => match info {
                    RequestedKeyInfo::MegolmV1AesSha2(i) => {
                        self.handle_supported_key_request(event, &i.room_id, &i.session_id).await
                    }
                    #[cfg(feature = "experimental-algorithms")]
                    RequestedKeyInfo::MegolmV2AesSha2(i) => {
                        self.handle_supported_key_request(event, &i.room_id, &i.session_id).await
                    }
                    RequestedKeyInfo::Unknown(i) => {
                        debug!(
                            sender = ?event.sender,
                            algorithm = ?i.algorithm,
                            "Received a room key request for a unsupported algorithm"
                        );
                        Ok(None)
                    }
                },
                // We ignore cancellations here since there's nothing to serve.
                Action::Cancellation => Ok(None),
            }
        } else {
            debug!(
                sender = ?event.sender,
                "Received a room key request, but room key forwarding has been turned off"
            );
            Ok(None)
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
        self.inner.outgoing_requests.write().unwrap().insert(request.request_id.clone(), request);

        Ok(used_session)
    }

    #[cfg(feature = "automatic-room-key-forwarding")]
    async fn forward_room_key(
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
        self.inner.outgoing_requests.write().unwrap().insert(request.request_id.clone(), request);

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

    #[cfg(feature = "automatic-room-key-forwarding")]
    async fn should_share_key(
        &self,
        device: &Device,
        session: &InboundGroupSession,
    ) -> Result<Option<u32>, super::KeyForwardDecision> {
        use super::KeyForwardDecision;
        use crate::olm::ShareState;

        let outbound_session = self
            .inner
            .outbound_group_sessions
            .get_with_id(session.room_id(), session.session_id())
            .await;

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
    #[cfg(feature = "automatic-room-key-forwarding")]
    async fn should_request_key(&self, key_info: &SecretInfo) -> Result<bool, CryptoStoreError> {
        if self.inner.room_key_forwarding_enabled.load(Ordering::SeqCst) {
            let request = self.inner.store.get_secret_request_by_info(key_info).await?;

            // Don't send out duplicate requests, users can re-request them if they
            // think a second request might succeed.
            if request.is_none() {
                let devices = self.inner.store.get_user_devices(self.user_id()).await?;

                // Devices will only respond to key requests if the devices are
                // verified, if the device isn't verified by us it's unlikely that
                // we're verified by them either. Don't request keys if there isn't
                // at least one verified device.
                Ok(devices.is_any_verified())
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

        let request = self.inner.store.get_secret_request_by_info(&secret_info).await?;

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
    #[cfg(feature = "automatic-room-key-forwarding")]
    pub async fn create_outgoing_key_request(
        &self,
        room_id: &RoomId,
        event: &EncryptedEvent,
    ) -> Result<bool, CryptoStoreError> {
        if let Some(info) = event.room_key_info(room_id).map(|i| i.into()) {
            if self.should_request_key(&info).await? {
                self.request_key_helper(info).await?;
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Save an outgoing key info.
    async fn save_outgoing_key_info(&self, info: GossipRequest) -> Result<(), CryptoStoreError> {
        let mut changes = Changes::default();
        changes.key_requests.push(info);
        self.inner.store.save_changes(changes).await?;

        Ok(())
    }

    /// Delete the given outgoing key info.
    async fn delete_key_info(&self, info: &GossipRequest) -> Result<(), CryptoStoreError> {
        self.inner.store.delete_outgoing_secret_requests(&info.request_id).await
    }

    /// Mark the outgoing request as sent.
    pub async fn mark_outgoing_request_as_sent(
        &self,
        id: &TransactionId,
    ) -> Result<(), CryptoStoreError> {
        let info = self.inner.store.get_outgoing_secret_requests(id).await?;

        if let Some(mut info) = info {
            trace!(
                recipient = info.request_recipient.as_str(),
                request_type = info.request_type(),
                request_id = info.request_id.to_string().as_str(),
                "Marking outgoing secret request as sent"
            );
            info.sent_out = true;
            self.save_outgoing_key_info(info).await?;
        } else {
            warn!(
                request_id = id.to_string(),
                "Unknown outgoing secret request; cannot mark as sent"
            )
        }

        self.inner.outgoing_requests.write().unwrap().remove(id);

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

        self.inner.outgoing_requests.write().unwrap().remove(&key_info.request_id);
        // TODO return the key info instead of deleting it so the sync handler
        // can delete it in one transaction.
        self.delete_key_info(key_info).await?;

        let request = key_info.to_cancellation(self.device_id());
        self.inner.outgoing_requests.write().unwrap().insert(request.request_id.clone(), request);

        Ok(())
    }

    async fn accept_secret(
        &self,
        secret: GossippedSecret,
        changes: &mut Changes,
    ) -> Result<(), CryptoStoreError> {
        if secret.secret_name != SecretName::RecoveryKey {
            match self.inner.store.import_secret(&secret).await {
                Ok(_) => self.mark_as_done(&secret.gossip_request).await?,
                // If this is a store error propagate it up the call stack.
                Err(SecretImportError::Store(e)) => return Err(e),
                // Otherwise warn that there was something wrong with the
                // secret.
                Err(e) => {
                    warn!(
                        secret_name = %secret.secret_name,
                        error = ?e,
                        "Error while importing a secret"
                    );
                }
            }
        } else {
            // We would need to fire out a request to figure out if this backup decryption
            // key is the one that is used for the current backup and if the
            // backup is trusted.
            //
            // So we put the secret into our inbox. Later users can inspect the contents of
            // the inbox and decide if they want to activate the backup.
            info!("Received a backup decryption key, storing it into the secret inbox.");
            changes.secrets.push(secret);
        }

        Ok(())
    }

    async fn receive_secret(
        &self,
        sender_key: Curve25519PublicKey,
        secret: GossippedSecret,
        changes: &mut Changes,
    ) -> Result<(), CryptoStoreError> {
        debug!("Received a m.secret.send event with a matching request");

        if let Some(device) =
            self.inner.store.get_device_from_curve_key(&secret.event.sender, sender_key).await?
        {
            // Only accept secrets from one of our own trusted devices.
            if device.user_id() == self.user_id() && device.is_verified() {
                self.accept_secret(secret, changes).await?;
            } else {
                warn!("Received a m.secret.send event from another user or from unverified device");
            }
        } else {
            warn!("Received a m.secret.send event from an unknown device");

            self.inner.store.mark_user_as_changed(&secret.event.sender).await?;
        }

        Ok(())
    }

    #[instrument(skip_all, fields(sender_key, sender = ?event.sender, request_id = ?event.content.request_id, secret_name))]
    pub async fn receive_secret_event(
        &self,
        sender_key: Curve25519PublicKey,
        event: &DecryptedSecretSendEvent,
        changes: &mut Changes,
    ) -> Result<Option<SecretName>, CryptoStoreError> {
        debug!("Received a m.secret.send event");

        let request_id = &event.content.request_id;

        let name = if let Some(request) =
            self.inner.store.get_outgoing_secret_requests(request_id).await?
        {
            match &request.info {
                SecretInfo::KeyRequest(_) => {
                    warn!("Received a m.secret.send event but the request was for a room key");

                    None
                }
                SecretInfo::SecretRequest(secret_name) => {
                    Span::current().record("secret_name", debug(secret_name));

                    let secret_name = secret_name.to_owned();

                    let secret = GossippedSecret {
                        secret_name: secret_name.to_owned(),
                        event: event.to_owned(),
                        gossip_request: request,
                    };

                    self.receive_secret(sender_key, secret, changes).await?;

                    Some(secret_name)
                }
            }
        } else {
            warn!("Received a m.secret.send event, but no matching request was found");
            None
        };

        Ok(name)
    }

    async fn accept_forwarded_room_key(
        &self,
        info: &GossipRequest,
        sender_key: Curve25519PublicKey,
        event: &DecryptedForwardedRoomKeyEvent,
    ) -> Result<Option<InboundGroupSession>, CryptoStoreError> {
        match InboundGroupSession::try_from(event) {
            Ok(session) => {
                if self.inner.store.compare_group_session(&session).await?
                    == SessionOrdering::Better
                {
                    self.mark_as_done(info).await?;

                    info!(
                        ?sender_key,
                        claimed_sender_key = ?session.sender_key(),
                        room_id = session.room_id().as_str(),
                        session_id = session.session_id(),
                        algorithm = ?session.algorithm(),
                        "Received a forwarded room key",
                    );

                    Ok(Some(session))
                } else {
                    info!(
                        ?sender_key,
                        claimed_sender_key = ?session.sender_key(),
                        room_id = ?session.room_id(),
                        session_id = session.session_id(),
                        algorithm = ?session.algorithm(),
                        "Received a forwarded room key but we already have a better version of it",
                    );

                    Ok(None)
                }
            }
            Err(e) => {
                warn!(?sender_key, "Couldn't create a group session from a received room key");
                Err(e.into())
            }
        }
    }

    async fn should_accept_forward(
        &self,
        info: &GossipRequest,
        sender_key: Curve25519PublicKey,
    ) -> Result<bool, CryptoStoreError> {
        let device =
            self.inner.store.get_device_from_curve_key(&info.request_recipient, sender_key).await?;

        if let Some(device) = device {
            Ok(device.user_id() == self.user_id() && device.is_verified())
        } else {
            Ok(false)
        }
    }

    /// Receive a forwarded room key event that was sent using any of our
    /// supported content types.
    async fn receive_supported_keys(
        &self,
        sender_key: Curve25519PublicKey,
        event: &DecryptedForwardedRoomKeyEvent,
    ) -> Result<Option<InboundGroupSession>, CryptoStoreError> {
        let Some(info) = event.room_key_info() else {
            warn!(
                sender_key = sender_key.to_base64(),
                algorithm = ?event.content.algorithm(),
                "Received a forwarded room key with an unsupported algorithm",
            );
            return Ok(None);
        };

        let Some(request) =
            self.inner.store.get_secret_request_by_info(&info.clone().into()).await?
        else {
            warn!(
                sender_key = ?sender_key,
                room_id = ?info.room_id(),
                session_id = info.session_id(),
                sender_key = ?sender_key,
                algorithm = ?info.algorithm(),
                "Received a forwarded room key that we didn't request",
            );
            return Ok(None);
        };

        if self.should_accept_forward(&request, sender_key).await? {
            self.accept_forwarded_room_key(&request, sender_key, event).await
        } else {
            warn!(
                ?sender_key,
                room_id = ?info.room_id(),
                session_id = info.session_id(),
                "Received a forwarded room key from an unknown device, or \
                 from a device that the key request recipient doesn't own",
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
        match event.content {
            ForwardedRoomKeyContent::MegolmV1AesSha2(_) => {
                self.receive_supported_keys(sender_key, event).await
            }
            #[cfg(feature = "experimental-algorithms")]
            ForwardedRoomKeyContent::MegolmV2AesSha2(_) => {
                self.receive_supported_keys(sender_key, event).await
            }
            ForwardedRoomKeyContent::Unknown(_) => {
                warn!(
                    sender = event.sender.as_str(),
                    sender_key = sender_key.to_base64(),
                    algorithm = ?event.content.algorithm(),
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

    #[cfg(feature = "automatic-room-key-forwarding")]
    use assert_matches::assert_matches;
    use matrix_sdk_test::async_test;
    use ruma::{
        device_id, event_id,
        events::{
            secret::request::{RequestAction, SecretName, ToDeviceSecretRequestEventContent},
            ToDeviceEvent as RumaToDeviceEvent,
        },
        room_id,
        serde::Raw,
        user_id, DeviceId, RoomId, UserId,
    };
    use serde_json::json;
    use tokio::sync::Mutex;

    use super::GossipMachine;
    #[cfg(feature = "automatic-room-key-forwarding")]
    use crate::{
        gossiping::KeyForwardDecision,
        olm::{Account, OutboundGroupSession},
        types::{
            events::{
                forwarded_room_key::ForwardedRoomKeyContent, olm_v1::AnyDecryptedOlmEvent,
                olm_v1::DecryptedOlmV1Event,
            },
            EventEncryptionAlgorithm,
        },
        EncryptionSettings, OutgoingRequests,
    };
    use crate::{
        identities::{LocalTrust, ReadOnlyDevice},
        olm::{PrivateCrossSigningIdentity, ReadOnlyAccount},
        session_manager::GroupSessionCache,
        store::{CryptoStoreWrapper, MemoryStore, Store},
        types::events::room::encrypted::{EncryptedEvent, RoomEncryptedEventContent},
        verification::VerificationMachine,
    };
    #[cfg(any(feature = "automatic-room-key-forwarding", feature = "backups_v1"))]
    use crate::{store::Changes, types::events::room::encrypted::EncryptedToDeviceEvent};

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
        ReadOnlyAccount::with_device_id(alice_id(), alice_device_id())
    }

    fn bob_account() -> ReadOnlyAccount {
        ReadOnlyAccount::with_device_id(bob_id(), bob_device_id())
    }

    fn alice_2_account() -> ReadOnlyAccount {
        ReadOnlyAccount::with_device_id(alice_id(), alice2_device_id())
    }

    #[cfg(feature = "automatic-room-key-forwarding")]
    fn test_gossip_machine(user_id: &UserId) -> GossipMachine {
        let user_id = user_id.to_owned();
        let device_id = DeviceId::new();

        let account = ReadOnlyAccount::with_device_id(&user_id, &device_id);
        let store = Arc::new(CryptoStoreWrapper::new(&user_id, MemoryStore::new()));
        let identity = Arc::new(Mutex::new(PrivateCrossSigningIdentity::empty(alice_id())));
        let verification =
            VerificationMachine::new(account.static_data.clone(), identity.clone(), store.clone());
        let store = Store::new(account, identity, store, verification);
        let session_cache = GroupSessionCache::new(store.clone());

        GossipMachine::new(store, session_cache, Default::default())
    }

    async fn get_machine() -> GossipMachine {
        let user_id = alice_id().to_owned();
        let account = ReadOnlyAccount::with_device_id(&user_id, alice_device_id());
        let device = ReadOnlyDevice::from_account(&account).await;
        let another_device = ReadOnlyDevice::from_account(&ReadOnlyAccount::with_device_id(
            &user_id,
            alice2_device_id(),
        ))
        .await;

        let store = Arc::new(CryptoStoreWrapper::new(&user_id, MemoryStore::new()));
        let identity = Arc::new(Mutex::new(PrivateCrossSigningIdentity::empty(alice_id())));
        let verification =
            VerificationMachine::new(account.static_data.clone(), identity.clone(), store.clone());

        let store = Store::new(account, identity, store, verification);
        store.save_devices(&[device, another_device]).await.unwrap();
        let session_cache = GroupSessionCache::new(store.clone());

        GossipMachine::new(store, session_cache, Default::default())
    }

    #[cfg(feature = "automatic-room-key-forwarding")]
    async fn machines_for_key_share(
        other_machine_owner: &UserId,
        create_sessions: bool,
        algorithm: EventEncryptionAlgorithm,
    ) -> (GossipMachine, Account, OutboundGroupSession, GossipMachine) {
        let alice_machine = get_machine().await;
        let alice_account = Account {
            static_data: alice_machine.inner.store.account().static_data().clone(),
            store: alice_machine.inner.store.clone(),
        };
        let alice_device = ReadOnlyDevice::from_account(alice_machine.inner.store.account()).await;

        let bob_machine = test_gossip_machine(other_machine_owner);
        let bob_device = ReadOnlyDevice::from_account(bob_machine.inner.store.account()).await;

        // We need a trusted device, otherwise we won't request keys
        let second_device = ReadOnlyDevice::from_account(&alice_2_account()).await;
        second_device.set_trust_state(LocalTrust::Verified);
        bob_device.set_trust_state(LocalTrust::Verified);
        alice_machine.inner.store.save_devices(&[bob_device, second_device]).await.unwrap();
        bob_machine.inner.store.save_devices(&[alice_device.clone()]).await.unwrap();

        if create_sessions {
            // Create Olm sessions for our two accounts.
            let (alice_session, bob_session) = alice_machine
                .inner
                .store
                .account()
                .create_session_for(bob_machine.inner.store.account())
                .await;

            // Populate our stores with Olm sessions and a Megolm session.

            alice_machine.inner.store.save_sessions(&[alice_session]).await.unwrap();
            bob_machine.inner.store.save_sessions(&[bob_session]).await.unwrap();
        }

        let settings = EncryptionSettings { algorithm, ..Default::default() };
        let (group_session, inbound_group_session) = bob_machine
            .inner
            .store
            .static_account()
            .create_group_session_pair(room_id(), settings)
            .await
            .unwrap();

        bob_machine
            .inner
            .store
            .save_inbound_group_sessions(&[inbound_group_session])
            .await
            .unwrap();

        let content = group_session.encrypt(json!({}), "m.dummy").await;
        let event = wrap_encrypted_content(bob_machine.user_id(), content);

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
        bob_machine.inner.outbound_group_sessions.insert(group_session.clone());

        (alice_machine, alice_account, group_session, bob_machine)
    }

    #[cfg(any(feature = "automatic-room-key-forwarding", feature = "backups_v1"))]
    fn extract_content<'a>(
        recipient: &UserId,
        request: &'a crate::OutgoingRequest,
    ) -> &'a Raw<ruma::events::AnyToDeviceEventContent> {
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

    #[cfg(any(feature = "automatic-room-key-forwarding", feature = "backups_v1"))]
    fn request_to_event<C>(
        recipient: &UserId,
        sender: &UserId,
        request: &crate::OutgoingRequest,
    ) -> crate::types::events::ToDeviceEvent<C>
    where
        C: crate::types::events::EventType
            + serde::de::DeserializeOwned
            + serde::ser::Serialize
            + std::fmt::Debug,
    {
        let content = extract_content(recipient, request);
        let content: C = content.deserialize_as().unwrap_or_else(|_| {
            panic!("We can always deserialize the to-device event content {content:?}")
        });

        crate::types::events::ToDeviceEvent::new(sender.to_owned(), content)
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
    #[cfg(feature = "automatic-room-key-forwarding")]
    async fn create_key_request() {
        let machine = get_machine().await;
        let account = account();
        let second_account = alice_2_account();
        let alice_device = ReadOnlyDevice::from_account(&second_account).await;

        // We need a trusted device, otherwise we won't request keys
        alice_device.set_trust_state(LocalTrust::Verified);
        machine.inner.store.save_devices(&[alice_device]).await.unwrap();

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
    #[cfg(feature = "automatic-room-key-forwarding")]
    async fn receive_forwarded_key() {
        let machine = get_machine().await;
        let account = account();

        let second_account = alice_2_account();
        let alice_device = ReadOnlyDevice::from_account(&second_account).await;

        // We need a trusted device, otherwise we won't request keys
        alice_device.set_trust_state(LocalTrust::Verified);
        machine.inner.store.save_devices(&[alice_device.clone()]).await.unwrap();

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
            .inner
            .store
            .get_inbound_group_session(session.room_id(), session.session_id(),)
            .await
            .unwrap()
            .is_none());

        let first_session = machine
            .receive_forwarded_room_key(alice_device.curve25519_key().unwrap(), &event)
            .await
            .unwrap();
        let first_session = first_session.unwrap();

        assert_eq!(first_session.first_known_index(), 10);

        machine.inner.store.save_inbound_group_sessions(&[first_session.clone()]).await.unwrap();

        // Get the cancel request.
        let id = machine
            .inner
            .outgoing_requests
            .read()
            .unwrap()
            .first_key_value()
            .map(|(_, r)| r.request_id.clone())
            .unwrap();
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
    #[cfg(feature = "automatic-room-key-forwarding")]
    async fn should_share_key_test() {
        let machine = get_machine().await;
        let account = account();

        let own_device =
            machine.inner.store.get_device(alice_id(), alice2_device_id()).await.unwrap().unwrap();

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
        machine.inner.store.save_devices(&[bob_device]).await.unwrap();

        let bob_device =
            machine.inner.store.get_device(bob_id(), bob_device_id()).await.unwrap().unwrap();

        // We don't share sessions with other user's devices if no outbound
        // session was provided.
        assert_matches!(
            machine.should_share_key(&bob_device, &inbound).await,
            Err(KeyForwardDecision::MissingOutboundSession)
        );

        let mut changes = Changes::default();

        changes.outbound_group_sessions.push(outbound.clone());
        changes.inbound_group_sessions.push(inbound.clone());
        machine.inner.store.save_changes(changes).await.unwrap();
        machine.inner.outbound_group_sessions.insert(outbound.clone());

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
        machine.inner.store.save_devices(&[bob_device]).await.unwrap();

        let bob_device =
            machine.inner.store.get_device(bob_id(), bob_device_id()).await.unwrap().unwrap();
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

        machine.inner.outbound_group_sessions.insert(other_outbound.clone());

        // Since our device is untrusted, we should share the session starting only from
        // the current index (at which the message was marked as shared). This
        // should be 3 since we encrypted 3 messages.
        assert_matches!(machine.should_share_key(&own_device, &other_inbound).await, Ok(Some(3)));

        own_device.set_trust_state(LocalTrust::Verified);

        // However once our device is trusted, we share the entire session.
        assert_matches!(machine.should_share_key(&own_device, &other_inbound).await, Ok(None));
    }

    #[cfg(feature = "automatic-room-key-forwarding")]
    async fn key_share_cycle(algorithm: EventEncryptionAlgorithm) {
        let (alice_machine, alice_account, group_session, bob_machine) =
            machines_for_key_share(alice_id(), true, algorithm).await;

        // Get the request and convert it into a event.
        let requests = alice_machine.outgoing_to_device_requests().await.unwrap();
        let request = &requests[0];
        let event = request_to_event(alice_id(), alice_id(), request);

        alice_machine.mark_outgoing_request_as_sent(&request.request_id).await.unwrap();

        // Bob doesn't have any outgoing requests.
        assert!(bob_machine.inner.outgoing_requests.read().unwrap().is_empty());

        // Receive the room key request from alice.
        bob_machine.receive_incoming_key_request(&event);
        bob_machine.collect_incoming_key_requests().await.unwrap();
        // Now bob does have an outgoing request.
        assert!(!bob_machine.inner.outgoing_requests.read().unwrap().is_empty());

        // Get the request and convert it to a encrypted to-device event.
        let requests = bob_machine.outgoing_to_device_requests().await.unwrap();
        let request = &requests[0];

        let event: EncryptedToDeviceEvent = request_to_event(alice_id(), alice_id(), request);
        bob_machine.mark_outgoing_request_as_sent(&request.request_id).await.unwrap();

        // Check that alice doesn't have the session.
        assert!(alice_machine
            .inner
            .store
            .get_inbound_group_session(room_id(), group_session.session_id())
            .await
            .unwrap()
            .is_none());

        let decrypted = alice_account.decrypt_to_device_event(&event).await.unwrap();

        let AnyDecryptedOlmEvent::ForwardedRoomKey(ev) = &*decrypted.result.event else {
            panic!("Invalid decrypted event type");
        };

        let session = alice_machine
            .receive_forwarded_room_key(decrypted.result.sender_key, ev)
            .await
            .unwrap();
        alice_machine.inner.store.save_inbound_group_sessions(&[session.unwrap()]).await.unwrap();

        // Check that alice now does have the session.
        let session = alice_machine
            .inner
            .store
            .get_inbound_group_session(room_id(), group_session.session_id())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(session.session_id(), group_session.session_id())
    }

    #[async_test]
    #[cfg(feature = "automatic-room-key-forwarding")]
    async fn reject_forward_from_another_user() {
        let (alice_machine, alice_account, group_session, bob_machine) =
            machines_for_key_share(bob_id(), true, EventEncryptionAlgorithm::MegolmV1AesSha2).await;

        // Get the request and convert it into a event.
        let requests = alice_machine.outgoing_to_device_requests().await.unwrap();
        let request = &requests[0];
        let event = request_to_event(alice_id(), alice_id(), request);

        alice_machine.mark_outgoing_request_as_sent(&request.request_id).await.unwrap();

        // Bob doesn't have any outgoing requests.
        assert!(bob_machine.inner.outgoing_requests.read().unwrap().is_empty());

        // Receive the room key request from alice.
        bob_machine.receive_incoming_key_request(&event);
        bob_machine.collect_incoming_key_requests().await.unwrap();
        // Now bob does have an outgoing request.
        assert!(!bob_machine.inner.outgoing_requests.read().unwrap().is_empty());

        // Get the request and convert it to a encrypted to-device event.
        let requests = bob_machine.outgoing_to_device_requests().await.unwrap();
        let request = &requests[0];

        let event: EncryptedToDeviceEvent = request_to_event(alice_id(), bob_id(), request);
        bob_machine.mark_outgoing_request_as_sent(&request.request_id).await.unwrap();

        // Check that alice doesn't have the session.
        assert!(alice_machine
            .inner
            .store
            .get_inbound_group_session(room_id(), group_session.session_id())
            .await
            .unwrap()
            .is_none());

        let decrypted = alice_account.decrypt_to_device_event(&event).await.unwrap();
        let AnyDecryptedOlmEvent::ForwardedRoomKey(ev) = &*decrypted.result.event else {
            panic!("Invalid decrypted event type");
        };

        let session = alice_machine
            .receive_forwarded_room_key(decrypted.result.sender_key, ev)
            .await
            .unwrap();

        assert!(session.is_none(), "We should not receive a room key from another user");
    }

    #[async_test]
    #[cfg(feature = "automatic-room-key-forwarding")]
    async fn key_share_cycle_megolm_v1() {
        key_share_cycle(EventEncryptionAlgorithm::MegolmV1AesSha2).await;
    }

    #[async_test]
    #[cfg(all(feature = "experimental-algorithms", feature = "automatic-room-key-forwarding"))]
    async fn key_share_cycle_megolm_v2() {
        key_share_cycle(EventEncryptionAlgorithm::MegolmV2AesSha2).await;
    }

    #[async_test]
    async fn secret_share_cycle() {
        let alice_machine = get_machine().await;

        let second_account = alice_2_account();
        let alice_device = ReadOnlyDevice::from_account(&second_account).await;

        let bob_account = bob_account();
        let bob_device = ReadOnlyDevice::from_account(&bob_account).await;

        alice_machine.inner.store.save_devices(&[alice_device.clone()]).await.unwrap();

        // Create Olm sessions for our two accounts.
        let (alice_session, _) =
            alice_machine.inner.store.account().create_session_for(&second_account).await;

        alice_machine.inner.store.save_sessions(&[alice_session]).await.unwrap();

        let event = RumaToDeviceEvent {
            sender: bob_account.user_id().to_owned(),
            content: ToDeviceSecretRequestEventContent::new(
                RequestAction::Request(SecretName::CrossSigningMasterKey),
                bob_account.device_id().to_owned(),
                "request_id".into(),
            ),
        };

        // No secret found
        assert!(alice_machine.inner.outgoing_requests.read().unwrap().is_empty());
        alice_machine.receive_incoming_secret_request(&event);
        alice_machine.collect_incoming_key_requests().await.unwrap();
        assert!(alice_machine.inner.outgoing_requests.read().unwrap().is_empty());

        // No device found
        alice_machine.inner.store.reset_cross_signing_identity().await;
        alice_machine.receive_incoming_secret_request(&event);
        alice_machine.collect_incoming_key_requests().await.unwrap();
        assert!(alice_machine.inner.outgoing_requests.read().unwrap().is_empty());

        alice_machine.inner.store.save_devices(&[bob_device]).await.unwrap();

        // The device doesn't belong to us
        alice_machine.inner.store.reset_cross_signing_identity().await;
        alice_machine.receive_incoming_secret_request(&event);
        alice_machine.collect_incoming_key_requests().await.unwrap();
        assert!(alice_machine.inner.outgoing_requests.read().unwrap().is_empty());

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
        assert!(alice_machine.inner.outgoing_requests.read().unwrap().is_empty());

        // We need a trusted device, otherwise we won't serve secrets
        alice_device.set_trust_state(LocalTrust::Verified);
        alice_machine.inner.store.save_devices(&[alice_device.clone()]).await.unwrap();

        alice_machine.receive_incoming_secret_request(&event);
        alice_machine.collect_incoming_key_requests().await.unwrap();
        assert!(!alice_machine.inner.outgoing_requests.read().unwrap().is_empty());
    }

    #[async_test]
    #[cfg(feature = "backups_v1")]
    async fn secret_broadcasting() {
        use futures_util::{pin_mut, FutureExt};
        use ruma::api::client::to_device::send_event_to_device::v3::Response as ToDeviceResponse;
        use serde_json::value::to_raw_value;
        use tokio_stream::StreamExt;

        use crate::{machine::tests::get_machine_pair_with_setup_sessions, EncryptionSyncChanges};

        let alice_id = user_id!("@alice:localhost");

        let (alice_machine, bob_machine) =
            get_machine_pair_with_setup_sessions(alice_id, alice_id, false).await;

        let key_requests = GossipMachine::request_missing_secrets(
            bob_machine.user_id(),
            vec![SecretName::RecoveryKey],
        );
        let mut changes = Changes::default();
        let request_id = key_requests.first().unwrap().request_id.to_owned();
        changes.key_requests = key_requests;
        bob_machine.store().save_changes(changes).await.unwrap();
        for request in bob_machine.outgoing_requests().await.unwrap() {
            bob_machine
                .mark_request_as_sent(request.request_id(), &ToDeviceResponse::new())
                .await
                .unwrap();
        }

        let event = RumaToDeviceEvent {
            sender: alice_machine.user_id().to_owned(),
            content: ToDeviceSecretRequestEventContent::new(
                RequestAction::Request(SecretName::RecoveryKey),
                bob_machine.device_id().to_owned(),
                request_id,
            ),
        };

        let bob_device = alice_machine
            .get_device(alice_id, bob_machine.device_id(), None)
            .await
            .unwrap()
            .unwrap();
        let alice_device = bob_machine
            .get_device(alice_id, alice_machine.device_id(), None)
            .await
            .unwrap()
            .unwrap();

        // We need a trusted device, otherwise we won't serve nor accept secrets.
        bob_device.set_trust_state(LocalTrust::Verified);
        alice_device.set_trust_state(LocalTrust::Verified);
        alice_machine.store().save_devices(&[bob_device.inner]).await.unwrap();
        bob_machine.store().save_devices(&[alice_device.inner]).await.unwrap();

        let decryption_key = crate::store::BackupDecryptionKey::new().unwrap();
        alice_machine
            .backup_machine()
            .save_decryption_key(Some(decryption_key), None)
            .await
            .unwrap();
        alice_machine.inner.key_request_machine.receive_incoming_secret_request(&event);
        alice_machine.inner.key_request_machine.collect_incoming_key_requests().await.unwrap();

        let requests =
            alice_machine.inner.key_request_machine.outgoing_to_device_requests().await.unwrap();

        assert_eq!(requests.len(), 1);
        let request = requests.first().expect("We should have an outgoing to-device request");

        let event: EncryptedToDeviceEvent =
            request_to_event(bob_machine.user_id(), alice_machine.user_id(), request);
        let event = Raw::from_json(to_raw_value(&event).unwrap());

        let stream = bob_machine.store().secrets_stream();
        pin_mut!(stream);

        bob_machine
            .receive_sync_changes(EncryptionSyncChanges {
                to_device_events: vec![event],
                changed_devices: &Default::default(),
                one_time_keys_counts: &Default::default(),
                unused_fallback_keys: None,
                next_batch_token: None,
            })
            .await
            .unwrap();

        stream.next().now_or_never().expect("The broadcaster should have sent out the secret");
    }

    #[async_test]
    #[cfg(feature = "automatic-room-key-forwarding")]
    async fn key_share_cycle_without_session() {
        let (alice_machine, alice_account, group_session, bob_machine) =
            machines_for_key_share(alice_id(), false, EventEncryptionAlgorithm::MegolmV1AesSha2)
                .await;

        // Get the request and convert it into a event.
        let requests = alice_machine.outgoing_to_device_requests().await.unwrap();
        let request = &requests[0];
        let event = request_to_event(alice_id(), alice_id(), request);

        alice_machine.mark_outgoing_request_as_sent(&request.request_id).await.unwrap();

        // Bob doesn't have any outgoing requests.
        assert!(bob_machine.outgoing_to_device_requests().await.unwrap().is_empty());
        assert!(bob_machine.inner.users_for_key_claim.read().unwrap().is_empty());
        assert!(bob_machine.inner.wait_queue.is_empty());

        // Receive the room key request from alice.
        bob_machine.receive_incoming_key_request(&event);
        bob_machine.collect_incoming_key_requests().await.unwrap();
        // Bob only has a keys claim request, since we're lacking a session
        assert_eq!(bob_machine.outgoing_to_device_requests().await.unwrap().len(), 1);
        assert_matches!(
            bob_machine.outgoing_to_device_requests().await.unwrap().first().unwrap().request(),
            OutgoingRequests::KeysClaim(_)
        );
        assert!(!bob_machine.inner.users_for_key_claim.read().unwrap().is_empty());
        assert!(!bob_machine.inner.wait_queue.is_empty());

        let (alice_session, bob_session) = alice_machine
            .inner
            .store
            .account()
            .create_session_for(bob_machine.inner.store.account())
            .await;
        // We create a session now.
        alice_machine.inner.store.save_sessions(&[alice_session]).await.unwrap();
        bob_machine.inner.store.save_sessions(&[bob_session]).await.unwrap();

        bob_machine.retry_keyshare(alice_id(), alice_device_id());
        assert!(bob_machine.inner.users_for_key_claim.read().unwrap().is_empty());
        bob_machine.collect_incoming_key_requests().await.unwrap();
        // Bob now has an outgoing requests.
        assert!(!bob_machine.outgoing_to_device_requests().await.unwrap().is_empty());
        assert!(bob_machine.inner.wait_queue.is_empty());

        // Get the request and convert it to a encrypted to-device event.
        let requests = bob_machine.outgoing_to_device_requests().await.unwrap();
        let request = &requests[0];

        let event: EncryptedToDeviceEvent = request_to_event(alice_id(), alice_id(), request);
        bob_machine.mark_outgoing_request_as_sent(&request.request_id).await.unwrap();

        // Check that alice doesn't have the session.
        assert!(alice_machine
            .inner
            .store
            .get_inbound_group_session(room_id(), group_session.session_id())
            .await
            .unwrap()
            .is_none());

        let decrypted = alice_account.decrypt_to_device_event(&event).await.unwrap();

        let AnyDecryptedOlmEvent::ForwardedRoomKey(ev) = &*decrypted.result.event else {
            panic!("Invalid decrypted event type");
        };

        let session = alice_machine
            .receive_forwarded_room_key(decrypted.result.sender_key, ev)
            .await
            .unwrap();
        alice_machine.inner.store.save_inbound_group_sessions(&[session.unwrap()]).await.unwrap();

        // Check that alice now does have the session.
        let session = alice_machine
            .inner
            .store
            .get_inbound_group_session(room_id(), group_session.session_id())
            .await
            .unwrap()
            .unwrap();

        assert_eq!(session.session_id(), group_session.session_id())
    }
}
