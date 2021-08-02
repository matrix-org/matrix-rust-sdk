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

use std::sync::Arc;

use dashmap::{mapref::entry::Entry, DashMap, DashSet};
use matrix_sdk_common::uuid::Uuid;
use ruma::{
    events::{
        forwarded_room_key::ForwardedRoomKeyToDeviceEventContent,
        room_key_request::{Action, RequestedKeyInfo, RoomKeyRequestToDeviceEventContent},
        secret::{
            request::{
                RequestAction, RequestToDeviceEventContent as SecretRequestEventContent, SecretName,
            },
            send::SendToDeviceEventContent as SecretSendEventContent,
        },
        AnyToDeviceEvent, AnyToDeviceEventContent, ToDeviceEvent,
    },
    to_device::DeviceIdOrAllDevices,
    DeviceId, DeviceIdBox, EventEncryptionAlgorithm, RoomId, UserId,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, error, info, trace, warn};

use crate::{
    error::{OlmError, OlmResult},
    olm::{InboundGroupSession, Session, ShareState},
    requests::{OutgoingRequest, ToDeviceRequest},
    session_manager::GroupSessionCache,
    store::{Changes, CryptoStoreError, SecretImportError, Store},
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

#[derive(Debug, Clone)]
enum RequestEvent {
    KeyShare(ToDeviceEvent<RoomKeyRequestToDeviceEventContent>),
    Secret(ToDeviceEvent<SecretRequestEventContent>),
}

impl From<ToDeviceEvent<SecretRequestEventContent>> for RequestEvent {
    fn from(e: ToDeviceEvent<SecretRequestEventContent>) -> Self {
        Self::Secret(e)
    }
}

impl From<ToDeviceEvent<RoomKeyRequestToDeviceEventContent>> for RequestEvent {
    fn from(e: ToDeviceEvent<RoomKeyRequestToDeviceEventContent>) -> Self {
        Self::KeyShare(e)
    }
}

impl RequestEvent {
    fn to_request_info(&self) -> RequestInfo {
        RequestInfo::new(
            self.sender().to_owned(),
            self.requesting_device_id().into(),
            self.request_id().to_owned(),
        )
    }

    fn sender(&self) -> &UserId {
        match self {
            RequestEvent::KeyShare(e) => &e.sender,
            RequestEvent::Secret(e) => &e.sender,
        }
    }

    fn requesting_device_id(&self) -> &DeviceId {
        match self {
            RequestEvent::KeyShare(e) => &e.content.requesting_device_id,
            RequestEvent::Secret(e) => &e.content.requesting_device_id,
        }
    }

    fn request_id(&self) -> &str {
        match self {
            RequestEvent::KeyShare(e) => &e.content.request_id,
            RequestEvent::Secret(e) => &e.content.request_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct RequestInfo {
    sender: UserId,
    requesting_device_id: DeviceIdBox,
    request_id: String,
}

impl RequestInfo {
    fn new(sender: UserId, requesting_device_id: DeviceIdBox, request_id: String) -> Self {
        Self { sender, requesting_device_id, request_id }
    }
}

/// A queue where we store room key requests that we want to serve but the
/// device that requested the key doesn't share an Olm session with us.
#[derive(Debug, Clone)]
struct WaitQueue {
    requests_waiting_for_session: Arc<DashMap<RequestInfo, RequestEvent>>,
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

    fn insert(&self, device: &Device, event: &ToDeviceEvent<RoomKeyRequestToDeviceEventContent>) {
        let key = RequestInfo::new(
            device.user_id().to_owned(),
            device.device_id().into(),
            event.content.request_id.to_owned(),
        );
        self.requests_waiting_for_session.insert(key, event.clone().into());

        let key = (device.user_id().to_owned(), device.device_id().into());
        self.requests_ids_waiting
            .entry(key)
            .or_insert_with(DashSet::new)
            .insert(event.content.request_id.clone());
    }

    fn remove(&self, user_id: &UserId, device_id: &DeviceId) -> Vec<(RequestInfo, RequestEvent)> {
        self.requests_ids_waiting
            .remove(&(user_id.to_owned(), device_id.into()))
            .map(|(_, request_ids)| {
                request_ids
                    .iter()
                    .filter_map(|id| {
                        let key =
                            RequestInfo::new(user_id.to_owned(), device_id.into(), id.to_owned());
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
    device_id: Arc<DeviceId>,
    store: Store,
    outbound_group_sessions: GroupSessionCache,
    outgoing_requests: Arc<DashMap<Uuid, OutgoingRequest>>,
    incoming_key_requests: Arc<DashMap<RequestInfo, RequestEvent>>,
    wait_queue: WaitQueue,
    users_for_key_claim: Arc<DashMap<UserId, DashSet<DeviceIdBox>>>,
}

/// A struct describing an outgoing key request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutgoingKeyRequest {
    /// The user we requested the key from
    pub request_recipient: UserId,
    /// The unique id of the key request.
    pub request_id: Uuid,
    /// The info of the requested key.
    pub info: SecretInfo,
    /// Has the request been sent out.
    pub sent_out: bool,
}

/// An enum over the various secret request types we can have.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SecretInfo {
    // Info for the `m.room_key_request` variant
    KeyRequest(RequestedKeyInfo),
    // Info for the `m.secret.request` variant
    SecretRequest(SecretName),
}

impl From<RequestedKeyInfo> for SecretInfo {
    fn from(i: RequestedKeyInfo) -> Self {
        Self::KeyRequest(i)
    }
}

impl From<SecretName> for SecretInfo {
    fn from(i: SecretName) -> Self {
        Self::SecretRequest(i)
    }
}

impl OutgoingKeyRequest {
    /// Create an ougoing secret request for the given secret.
    pub(crate) fn from_secret_name(own_user_id: UserId, secret_name: SecretName) -> Self {
        Self {
            request_recipient: own_user_id,
            request_id: Uuid::new_v4(),
            info: secret_name.into(),
            sent_out: false,
        }
    }

    fn request_type(&self) -> &str {
        match &self.info {
            SecretInfo::KeyRequest(_) => "m.room_key_request",
            SecretInfo::SecretRequest(s) => s.as_ref(),
        }
    }

    fn to_request(&self, own_device_id: &DeviceId) -> OutgoingRequest {
        let content = match &self.info {
            SecretInfo::KeyRequest(r) => {
                AnyToDeviceEventContent::RoomKeyRequest(RoomKeyRequestToDeviceEventContent::new(
                    Action::Request,
                    Some(r.clone()),
                    own_device_id.to_owned(),
                    self.request_id.to_string(),
                ))
            }
            SecretInfo::SecretRequest(s) => {
                AnyToDeviceEventContent::SecretRequest(SecretRequestEventContent::new(
                    RequestAction::Request(s.clone()),
                    own_device_id.to_owned(),
                    self.request_id.to_string(),
                ))
            }
        };

        let request = ToDeviceRequest::new_with_id(
            &self.request_recipient,
            DeviceIdOrAllDevices::AllDevices,
            content,
            self.request_id,
        );

        OutgoingRequest { request_id: request.txn_id, request: Arc::new(request.into()) }
    }

    fn to_cancellation(&self, own_device_id: &DeviceId) -> OutgoingRequest {
        let content = match self.info {
            SecretInfo::KeyRequest(_) => {
                AnyToDeviceEventContent::RoomKeyRequest(RoomKeyRequestToDeviceEventContent::new(
                    Action::CancelRequest,
                    None,
                    own_device_id.to_owned(),
                    self.request_id.to_string(),
                ))
            }
            SecretInfo::SecretRequest(_) => {
                AnyToDeviceEventContent::SecretRequest(SecretRequestEventContent::new(
                    RequestAction::RequestCancellation,
                    own_device_id.to_owned(),
                    self.request_id.to_string(),
                ))
            }
        };

        let request = ToDeviceRequest::new(
            &self.request_recipient,
            DeviceIdOrAllDevices::AllDevices,
            content,
        );

        OutgoingRequest { request_id: request.txn_id, request: Arc::new(request.into()) }
    }
}

impl PartialEq for OutgoingKeyRequest {
    fn eq(&self, other: &Self) -> bool {
        let is_info_equal = match (&self.info, &other.info) {
            (SecretInfo::KeyRequest(first), SecretInfo::KeyRequest(second)) => {
                first.algorithm == second.algorithm
                    && first.room_id == second.room_id
                    && first.session_id == second.session_id
            }
            (SecretInfo::SecretRequest(first), SecretInfo::SecretRequest(second)) => {
                first == second
            }
            (SecretInfo::KeyRequest(_), SecretInfo::SecretRequest(_))
            | (SecretInfo::SecretRequest(_), SecretInfo::KeyRequest(_)) => false,
        };

        self.request_id == other.request_id && is_info_equal
    }
}

impl KeyRequestMachine {
    pub fn new(
        user_id: Arc<UserId>,
        device_id: Arc<DeviceId>,
        store: Store,
        outbound_group_sessions: GroupSessionCache,
        users_for_key_claim: Arc<DashMap<UserId, DashSet<DeviceIdBox>>>,
    ) -> Self {
        Self {
            user_id,
            device_id,
            store,
            outbound_group_sessions,
            outgoing_requests: DashMap::new().into(),
            incoming_key_requests: DashMap::new().into(),
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

    /// Our own device id.
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

        Ok(key_requests)
    }

    /// Receive a room key request event.
    pub fn receive_incoming_key_request(
        &self,
        event: &ToDeviceEvent<RoomKeyRequestToDeviceEventContent>,
    ) {
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

    #[allow(dead_code)]
    pub fn receive_incoming_secret_request(
        &self,
        event: &ToDeviceEvent<SecretRequestEventContent>,
    ) {
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
    fn handle_key_share_without_session(
        &self,
        device: Device,
        event: &ToDeviceEvent<RoomKeyRequestToDeviceEventContent>,
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

    async fn handle_secret_request(
        &self,
        event: &ToDeviceEvent<SecretRequestEventContent>,
    ) -> OlmResult<Option<Session>> {
        let secret_name = match &event.content.action {
            RequestAction::Request(s) => s,
            // We ignore cancellations here since there's nothing to serve.
            RequestAction::RequestCancellation => return Ok(None),
            action => {
                warn!(action =? action, "Unknown secret request action");
                return Ok(None);
            }
        };

        let content = if let Some(secret) = self.store.export_secret(secret_name).await {
            SecretSendEventContent::new(event.content.request_id.to_owned(), secret)
        } else {
            info!(secret_name =? secret_name, "Can't server a secret request, secret isn't found");
            return Ok(None);
        };

        let device =
            self.store.get_device(&event.sender, &event.content.requesting_device_id).await?;

        Ok(if let Some(device) = device {
            if device.user_id() == self.user_id() {
                if device.verified() {
                    info!(
                        user_id = device.user_id().as_str(),
                        device_id = device.device_id().as_str(),
                        secret_name =? secret_name,
                        "Sharing a secret with a device",
                    );

                    Some(self.share_secret(&device, content).await?)
                } else {
                    info!(
                        user_id = device.user_id().as_str(),
                        device_id = device.device_id().as_str(),
                        secret_name =? secret_name,
                        "Received a secret request that we won't serve, the device isn't trusted",
                    );

                    None
                }
            } else {
                info!(
                    user_id = device.user_id().as_str(),
                    device_id = device.device_id().as_str(),
                    secret_name =? secret_name,
                    "Received a secret request that we won't serve, the device doesn't belong to us",
                );

                None
            }
        } else {
            warn!(
                user_id = event.sender.as_str(),
                device_id = event.content.requesting_device_id.as_str(),
                secret_name =? secret_name,
                "Received a secret request form an unknown device",
            );
            self.store.update_tracked_user(&event.sender, true).await?;

            None
        })
    }

    /// Handle a single incoming key request.
    async fn handle_key_request(
        &self,
        event: &ToDeviceEvent<RoomKeyRequestToDeviceEventContent>,
    ) -> OlmResult<Option<Session>> {
        let key_info = match &event.content.action {
            Action::Request => {
                if let Some(info) = &event.content.body {
                    info
                } else {
                    warn!(
                        sender = event.sender.as_str(),
                        requesting_device_id = event.content.requesting_device_id.as_str(),
                        "Received a key request with a request of action, but
                        no key info was found",
                    );
                    return Ok(None);
                }
            }
            // We ignore cancellations here since there's nothing to serve.
            Action::CancelRequest => return Ok(None),
            action => {
                warn!(
                    sender = event.sender.as_str(),
                    requesting_device_id = event.content.requesting_device_id.as_str(),
                    action = action.as_ref(),
                    "Received a room key request with an unknown action",
                );
                return Ok(None);
            }
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
                    debug!(
                        user_id = device.user_id().as_str(),
                        device_id = device.device_id().as_str(),
                        reason =? e,
                        "Received a key request that we won't serve",
                    );

                    Ok(None)
                }
                Ok(message_index) => {
                    info!(
                        user_id = device.user_id().as_str(),
                        device_id = device.device_id().as_str(),
                        session_id = key_info.session_id.as_str(),
                        room_id = key_info.room_id.as_str(),
                        message_index =? message_index,
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
                            self.handle_key_share_without_session(device, event);

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

    async fn share_secret(
        &self,
        device: &Device,
        content: SecretSendEventContent,
    ) -> OlmResult<Session> {
        let (used_session, content) =
            device.encrypt(AnyToDeviceEventContent::SecretSend(content)).await?;

        let request = ToDeviceRequest::new(
            device.user_id(),
            device.device_id().to_owned(),
            AnyToDeviceEventContent::RoomEncrypted(content),
        );

        let request =
            OutgoingRequest { request_id: request.txn_id, request: Arc::new(request.into()) };
        self.outgoing_requests.insert(request.request_id, request);

        Ok(used_session)
    }

    async fn share_session(
        &self,
        session: &InboundGroupSession,
        device: &Device,
        message_index: Option<u32>,
    ) -> OlmResult<Session> {
        let (used_session, content) =
            device.encrypt_session(session.clone(), message_index).await?;

        let request = ToDeviceRequest::new(
            device.user_id(),
            device.device_id().to_owned(),
            AnyToDeviceEventContent::RoomEncrypted(content),
        );

        let request =
            OutgoingRequest { request_id: request.txn_id, request: Arc::new(request.into()) };
        self.outgoing_requests.insert(request.request_id, request);

        Ok(used_session)
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
    /// * `session` - The session that was requested to be shared.
    async fn should_share_key(
        &self,
        device: &Device,
        session: &InboundGroupSession,
    ) -> Result<Option<u32>, KeyshareDecision> {
        let outbound_session = self
            .outbound_group_sessions
            .get_with_id(session.room_id(), session.session_id())
            .await
            .ok()
            .flatten();

        let own_device_check = || {
            if device.verified() {
                Ok(None)
            } else {
                Err(KeyshareDecision::UntrustedDevice)
            }
        };

        // If we have a matching outbound session we can check the list of
        // users/devices that received the session, if it wasn't shared check if
        // it's our own device and if it's trusted.
        if let Some(outbound) = outbound_session {
            if let ShareState::Shared(message_index) =
                outbound.is_shared_with(device.user_id(), device.device_id())
            {
                Ok(Some(message_index))
            } else if device.user_id() == self.user_id() {
                own_device_check()
            } else {
                Err(KeyshareDecision::OutboundSessionNotShared)
            }
        // Else just check if it's one of our own devices that requested the key
        // and check if the device is trusted.
        } else if device.user_id() == self.user_id() {
            own_device_check()
        // Otherwise, there's not enough info to decide if we can safely share
        // the session.
        } else {
            Err(KeyshareDecision::MissingOutboundSession)
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
    /// * `sender_key` - The curve25519 key of the sender that owns the key.
    ///
    /// * `session_id` - The id that uniquely identifies the session.
    pub async fn request_key(
        &self,
        room_id: &RoomId,
        sender_key: &str,
        session_id: &str,
    ) -> Result<(Option<OutgoingRequest>, OutgoingRequest), CryptoStoreError> {
        let key_info = RequestedKeyInfo::new(
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            room_id.to_owned(),
            sender_key.to_owned(),
            session_id.to_owned(),
        )
        .into();

        let request = self.store.get_secret_request_by_info(&key_info).await?;

        if let Some(request) = request {
            let cancel = request.to_cancellation(self.device_id());
            let request = request.to_request(self.device_id());

            Ok((Some(cancel), request))
        } else {
            let request = self.request_key_helper(key_info).await?;

            Ok((None, request))
        }
    }

    /// Create outgoing secret requests for the given
    pub fn request_missing_secrets(
        own_user_id: &UserId,
        secret_names: Vec<SecretName>,
    ) -> Vec<OutgoingKeyRequest> {
        if !secret_names.is_empty() {
            info!(secret_names =? secret_names, "Creating new outgoing secret requests");

            secret_names
                .into_iter()
                .map(|n| OutgoingKeyRequest::from_secret_name(own_user_id.to_owned(), n))
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
        let request = OutgoingKeyRequest {
            request_recipient: self.user_id().to_owned(),
            request_id: Uuid::new_v4(),
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
    /// * `sender_key` - The curve25519 key of the sender that owns the key.
    ///
    /// * `session_id` - The id that uniquely identifies the session.
    pub async fn create_outgoing_key_request(
        &self,
        room_id: &RoomId,
        sender_key: &str,
        session_id: &str,
    ) -> Result<(), CryptoStoreError> {
        let key_info = RequestedKeyInfo::new(
            EventEncryptionAlgorithm::MegolmV1AesSha2,
            room_id.to_owned(),
            sender_key.to_owned(),
            session_id.to_owned(),
        )
        .into();

        if self.should_request_key(&key_info).await? {
            self.request_key_helper(key_info).await?;
        }

        Ok(())
    }

    /// Save an outgoing key info.
    async fn save_outgoing_key_info(
        &self,
        info: OutgoingKeyRequest,
    ) -> Result<(), CryptoStoreError> {
        let mut changes = Changes::default();
        changes.key_requests.push(info);
        self.store.save_changes(changes).await?;

        Ok(())
    }

    /// Get an outgoing key info that matches the forwarded room key content.
    async fn get_key_info(
        &self,
        content: &ForwardedRoomKeyToDeviceEventContent,
    ) -> Result<Option<OutgoingKeyRequest>, CryptoStoreError> {
        let info = RequestedKeyInfo::new(
            content.algorithm.clone(),
            content.room_id.clone(),
            content.sender_key.clone(),
            content.session_id.clone(),
        )
        .into();

        self.store.get_secret_request_by_info(&info).await
    }

    /// Delete the given outgoing key info.
    async fn delete_key_info(&self, info: &OutgoingKeyRequest) -> Result<(), CryptoStoreError> {
        self.store.delete_outgoing_secret_requests(info.request_id).await
    }

    /// Mark the outgoing request as sent.
    pub async fn mark_outgoing_request_as_sent(&self, id: Uuid) -> Result<(), CryptoStoreError> {
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

        self.outgoing_requests.remove(&id);

        Ok(())
    }

    /// Mark the given outgoing key info as done.
    ///
    /// This will queue up a request cancellation.
    async fn mark_as_done(&self, key_info: OutgoingKeyRequest) -> Result<(), CryptoStoreError> {
        trace!(
            recipient = key_info.request_recipient.as_str(),
            request_type = key_info.request_type(),
            request_id = key_info.request_id.to_string().as_str(),
            "Successfully received a secret, removing the request"
        );

        self.outgoing_requests.remove(&key_info.request_id);
        // TODO return the key info instead of deleting it so the sync handler
        // can delete it in one transaction.
        self.delete_key_info(&key_info).await?;

        let request = key_info.to_cancellation(self.device_id());
        self.outgoing_requests.insert(request.request_id, request);

        Ok(())
    }

    pub async fn receive_secret(
        &self,
        sender_key: &str,
        event: &mut ToDeviceEvent<SecretSendEventContent>,
    ) -> Result<Option<AnyToDeviceEvent>, CryptoStoreError> {
        debug!(
            sender = event.sender.as_str(),
            request_id = event.content.request_id.as_str(),
            "Received a m.secret.send event"
        );

        let request_id = if let Ok(r) = Uuid::parse_str(&event.content.request_id) {
            r
        } else {
            warn!("Received a m.secret.send event but the request ID is invalid");
            return Ok(None);
        };

        if let Some(request) = self.store.get_outgoing_secret_requests(request_id).await? {
            match &request.info {
                SecretInfo::KeyRequest(_) => {
                    warn!(
                        sender = event.sender.as_str(),
                        request_id = event.content.request_id.as_str(),
                        "Received a m.secret.send event but the request was for a room key"
                    );
                }
                SecretInfo::SecretRequest(secret_name) => {
                    debug!(
                        sender = event.sender.as_str(),
                        request_id = event.content.request_id.as_str(),
                        secret_name = secret_name.as_ref(),
                        "Received a m.secret.send event with a matching request"
                    );

                    if let Some(device) =
                        self.store.get_device_from_curve_key(&event.sender, sender_key).await?
                    {
                        if device.verified() {
                            match self
                                .store
                                .import_secret(
                                    secret_name,
                                    std::mem::take(&mut event.content.secret),
                                )
                                .await
                            {
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
                                            error =? e,
                                            "Error while importing a secret"
                                        )
                                    }
                                }
                            }
                        } else {
                            warn!(
                                sender = event.sender.as_str(),
                                request_id = event.content.request_id.as_str(),
                                secret_name = secret_name.as_ref(),
                                "Received a m.secret.send event from an unverified device"
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
                }
            }
        }

        Ok(Some(AnyToDeviceEvent::SecretSend(event.clone())))
    }

    /// Receive a forwarded room key event.
    pub async fn receive_forwarded_room_key(
        &self,
        sender_key: &str,
        event: &mut ToDeviceEvent<ForwardedRoomKeyToDeviceEventContent>,
    ) -> Result<(Option<AnyToDeviceEvent>, Option<InboundGroupSession>), CryptoStoreError> {
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
            let session = if let Some(old_session) = old_session {
                let first_old_index = old_session.first_known_index();
                let first_index = session.first_known_index();

                if first_old_index > first_index {
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
                    sender = event.sender.as_str(),
                    room_id = s.room_id().as_str(),
                    session_id = s.session_id(),
                    "Received a forwarded room key",
                );
            }

            Ok((Some(AnyToDeviceEvent::ForwardedRoomKey(event.clone())), session))
        } else {
            info!(
                sender = event.sender.as_str(),
                "Received a forwarded room key but no key info was found.",
            );
            Ok((None, None))
        }
    }
}

#[cfg(test)]
mod test {
    use std::{convert::TryInto, sync::Arc};

    use dashmap::DashMap;
    use matrix_sdk_common::locks::Mutex;
    use matrix_sdk_test::async_test;
    use ruma::{
        events::{
            forwarded_room_key::ForwardedRoomKeyToDeviceEventContent,
            room::encrypted::EncryptedToDeviceEventContent,
            room_key_request::RoomKeyRequestToDeviceEventContent, AnyToDeviceEvent, ToDeviceEvent,
            secret::request::{RequestAction, RequestToDeviceEventContent, SecretName},
        },
        room_id,
        to_device::DeviceIdOrAllDevices,
        user_id, DeviceIdBox, RoomId, UserId,
    };

    use super::{KeyRequestMachine, KeyshareDecision};
    use crate::{
        identities::{LocalTrust, ReadOnlyDevice},
        olm::{Account, PrivateCrossSigningIdentity, ReadOnlyAccount},
        session_manager::GroupSessionCache,
        store::{Changes, CryptoStore, MemoryStore, Store},
        verification::VerificationMachine,
    };

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

    fn alice2_device_id() -> DeviceIdBox {
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

    fn alice_2_account() -> ReadOnlyAccount {
        ReadOnlyAccount::new(&alice_id(), &alice2_device_id())
    }

    fn bob_machine() -> KeyRequestMachine {
        let user_id = Arc::new(bob_id());
        let account = ReadOnlyAccount::new(&user_id, &alice_device_id());
        let store: Arc<dyn CryptoStore> = Arc::new(MemoryStore::new());
        let identity = Arc::new(Mutex::new(PrivateCrossSigningIdentity::empty(bob_id())));
        let verification = VerificationMachine::new(account, identity.clone(), store.clone());
        let store = Store::new(user_id.clone(), identity, store, verification);
        let session_cache = GroupSessionCache::new(store.clone());

        KeyRequestMachine::new(
            user_id,
            bob_device_id().into(),
            store,
            session_cache,
            Arc::new(DashMap::new()),
        )
    }

    async fn get_machine() -> KeyRequestMachine {
        let user_id: Arc<UserId> = alice_id().into();
        let account = ReadOnlyAccount::new(&user_id, &alice_device_id());
        let device = ReadOnlyDevice::from_account(&account).await;
        let store: Arc<dyn CryptoStore> = Arc::new(MemoryStore::new());
        let identity = Arc::new(Mutex::new(PrivateCrossSigningIdentity::empty(alice_id())));
        let verification = VerificationMachine::new(account, identity.clone(), store.clone());
        let store = Store::new(user_id.clone(), identity, store, verification);
        store.save_devices(&[device]).await.unwrap();
        let session_cache = GroupSessionCache::new(store.clone());

        KeyRequestMachine::new(
            user_id,
            alice_device_id().into(),
            store,
            session_cache,
            Arc::new(DashMap::new()),
        )
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

        let (_, session) =
            account.create_group_session_pair_with_defaults(&room_id()).await.unwrap();

        assert!(machine.outgoing_to_device_requests().await.unwrap().is_empty());
        let (cancel, request) = machine
            .request_key(session.room_id(), &session.sender_key, session.session_id())
            .await
            .unwrap();

        assert!(cancel.is_none());

        machine.mark_outgoing_request_as_sent(request.request_id).await.unwrap();

        let (cancel, _) = machine
            .request_key(session.room_id(), &session.sender_key, session.session_id())
            .await
            .unwrap();

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

        let (_, session) =
            account.create_group_session_pair_with_defaults(&room_id()).await.unwrap();

        assert!(machine.outgoing_to_device_requests().await.unwrap().is_empty());
        machine
            .create_outgoing_key_request(
                session.room_id(),
                &session.sender_key,
                session.session_id(),
            )
            .await
            .unwrap();
        assert!(!machine.outgoing_to_device_requests().await.unwrap().is_empty());
        assert_eq!(machine.outgoing_to_device_requests().await.unwrap().len(), 1);

        machine
            .create_outgoing_key_request(
                session.room_id(),
                &session.sender_key,
                session.session_id(),
            )
            .await
            .unwrap();

        let requests = machine.outgoing_to_device_requests().await.unwrap();
        assert_eq!(requests.len(), 1);

        let request = requests.get(0).unwrap();

        machine.mark_outgoing_request_as_sent(request.request_id).await.unwrap();
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
        machine.store.save_devices(&[alice_device]).await.unwrap();

        let (_, session) =
            account.create_group_session_pair_with_defaults(&room_id()).await.unwrap();
        machine
            .create_outgoing_key_request(
                session.room_id(),
                &session.sender_key,
                session.session_id(),
            )
            .await
            .unwrap();

        let requests = machine.outgoing_to_device_requests().await.unwrap();
        let request = requests.get(0).unwrap();
        let id = request.request_id;

        machine.mark_outgoing_request_as_sent(id).await.unwrap();

        let export = session.export_at_index(10).await;

        let content: ForwardedRoomKeyToDeviceEventContent = export.try_into().unwrap();

        let mut event = ToDeviceEvent { sender: alice_id(), content };

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

        let (_, first_session) =
            machine.receive_forwarded_room_key(&session.sender_key, &mut event).await.unwrap();
        let first_session = first_session.unwrap();

        assert_eq!(first_session.first_known_index(), 10);

        machine.store.save_inbound_group_sessions(&[first_session.clone()]).await.unwrap();

        // Get the cancel request.
        let request = machine.outgoing_requests.iter().next().unwrap();
        let id = request.request_id;
        drop(request);
        machine.mark_outgoing_request_as_sent(id).await.unwrap();

        machine
            .create_outgoing_key_request(
                session.room_id(),
                &session.sender_key,
                session.session_id(),
            )
            .await
            .unwrap();

        let requests = machine.outgoing_to_device_requests().await.unwrap();
        let request = &requests[0];

        machine.mark_outgoing_request_as_sent(request.request_id).await.unwrap();

        let export = session.export_at_index(15).await;

        let content: ForwardedRoomKeyToDeviceEventContent = export.try_into().unwrap();

        let mut event = ToDeviceEvent { sender: alice_id(), content };

        let (_, second_session) =
            machine.receive_forwarded_room_key(&session.sender_key, &mut event).await.unwrap();

        assert!(second_session.is_none());

        let export = session.export_at_index(0).await;

        let content: ForwardedRoomKeyToDeviceEventContent = export.try_into().unwrap();

        let mut event = ToDeviceEvent { sender: alice_id(), content };

        let (_, second_session) =
            machine.receive_forwarded_room_key(&session.sender_key, &mut event).await.unwrap();

        assert_eq!(second_session.unwrap().first_known_index(), 0);
    }

    #[async_test]
    async fn should_share_key_test() {
        let machine = get_machine().await;
        let account = account();

        let own_device =
            machine.store.get_device(&alice_id(), &alice_device_id()).await.unwrap().unwrap();

        let (outbound, inbound) =
            account.create_group_session_pair_with_defaults(&room_id()).await.unwrap();

        // We don't share keys with untrusted devices.
        assert_eq!(
            machine
                .should_share_key(&own_device, &inbound)
                .await
                .expect_err("Should not share with untrusted"),
            KeyshareDecision::UntrustedDevice
        );
        own_device.set_trust_state(LocalTrust::Verified);
        // Now we do want to share the keys.
        assert!(machine.should_share_key(&own_device, &inbound).await.is_ok());

        let bob_device = ReadOnlyDevice::from_account(&bob_account()).await;
        machine.store.save_devices(&[bob_device]).await.unwrap();

        let bob_device =
            machine.store.get_device(&bob_id(), &bob_device_id()).await.unwrap().unwrap();

        // We don't share sessions with other user's devices if no outbound
        // session was provided.
        assert_eq!(
            machine
                .should_share_key(&bob_device, &inbound)
                .await
                .expect_err("Should not share with other."),
            KeyshareDecision::MissingOutboundSession
        );

        let mut changes = Changes::default();

        changes.outbound_group_sessions.push(outbound.clone());
        changes.inbound_group_sessions.push(inbound.clone());
        machine.store.save_changes(changes).await.unwrap();
        machine.outbound_group_sessions.insert(outbound.clone());

        // We don't share sessions with other user's devices if the session
        // wasn't shared in the first place.
        assert_eq!(
            machine
                .should_share_key(&bob_device, &inbound)
                .await
                .expect_err("Should not share with other unless shared."),
            KeyshareDecision::OutboundSessionNotShared
        );

        bob_device.set_trust_state(LocalTrust::Verified);

        // We don't share sessions with other user's devices if the session
        // wasn't shared in the first place even if the device is trusted.
        assert_eq!(
            machine
                .should_share_key(&bob_device, &inbound)
                .await
                .expect_err("Should not share with other unless shared."),
            KeyshareDecision::OutboundSessionNotShared
        );

        // We now share the session, since it was shared before.
        outbound.mark_shared_with(bob_device.user_id(), bob_device.device_id());
        assert!(machine.should_share_key(&bob_device, &inbound).await.is_ok());

        // But we don't share some other session that doesn't match our outbound
        // session
        let (_, other_inbound) =
            account.create_group_session_pair_with_defaults(&room_id()).await.unwrap();

        assert_eq!(
            machine
                .should_share_key(&bob_device, &other_inbound)
                .await
                .expect_err("Should not share with other unless shared."),
            KeyshareDecision::MissingOutboundSession
        );
    }

    #[async_test]
    async fn key_share_cycle() {
        let alice_machine = get_machine().await;
        let alice_account = Account { inner: account(), store: alice_machine.store.clone() };

        let bob_machine = bob_machine();
        let bob_account = bob_account();

        let second_account = alice_2_account();
        let alice_device = ReadOnlyDevice::from_account(&second_account).await;

        // We need a trusted device, otherwise we won't request keys
        alice_device.set_trust_state(LocalTrust::Verified);
        alice_machine.store.save_devices(&[alice_device]).await.unwrap();

        // Create Olm sessions for our two accounts.
        let (alice_session, bob_session) = alice_account.create_session_for(&bob_account).await;

        let alice_device = ReadOnlyDevice::from_account(&alice_account).await;
        let bob_device = ReadOnlyDevice::from_account(&bob_account).await;

        // Populate our stores with Olm sessions and a Megolm session.

        alice_machine.store.save_sessions(&[alice_session]).await.unwrap();
        alice_machine.store.save_devices(&[bob_device]).await.unwrap();
        bob_machine.store.save_sessions(&[bob_session]).await.unwrap();
        bob_machine.store.save_devices(&[alice_device]).await.unwrap();

        let (group_session, inbound_group_session) =
            bob_account.create_group_session_pair_with_defaults(&room_id()).await.unwrap();

        bob_machine.store.save_inbound_group_sessions(&[inbound_group_session]).await.unwrap();

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
        bob_machine.outbound_group_sessions.insert(group_session.clone());

        // Get the request and convert it into a event.
        let requests = alice_machine.outgoing_to_device_requests().await.unwrap();
        let request = &requests[0];
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
        let content: RoomKeyRequestToDeviceEventContent = content.deserialize_as().unwrap();

        alice_machine.mark_outgoing_request_as_sent(id).await.unwrap();

        let event = ToDeviceEvent { sender: alice_id(), content };

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
        let content: EncryptedToDeviceEventContent = content.deserialize_as().unwrap();

        bob_machine.mark_outgoing_request_as_sent(id).await.unwrap();

        let event = ToDeviceEvent { sender: bob_id(), content };

        // Check that alice doesn't have the session.
        assert!(alice_machine
            .store
            .get_inbound_group_session(
                &room_id(),
                bob_account.identity_keys().curve25519(),
                group_session.session_id()
            )
            .await
            .unwrap()
            .is_none());

        let decrypted = alice_account.decrypt_to_device_event(&event).await.unwrap();

        if let AnyToDeviceEvent::ForwardedRoomKey(mut e) = decrypted.event.deserialize().unwrap() {
            let (_, session) = alice_machine
                .receive_forwarded_room_key(&decrypted.sender_key, &mut e)
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
                &room_id(),
                &decrypted.sender_key,
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

        let event = ToDeviceEvent {
            sender: bob_account.user_id().to_owned(),
            content: RequestToDeviceEventContent::new(
                RequestAction::Request(SecretName::CrossSigningMasterKey),
                bob_account.device_id().to_owned(),
                "request_id".to_owned(),
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

        let event = ToDeviceEvent {
            sender: alice_id(),
            content: RequestToDeviceEventContent::new(
                RequestAction::Request(SecretName::CrossSigningMasterKey),
                second_account.device_id().into(),
                "request_id".to_owned(),
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
        let alice_machine = get_machine().await;
        let alice_account = Account { inner: account(), store: alice_machine.store.clone() };

        let bob_machine = bob_machine();
        let bob_account = bob_account();

        let second_account = alice_2_account();
        let alice_device = ReadOnlyDevice::from_account(&second_account).await;

        // We need a trusted device, otherwise we won't request keys
        alice_device.set_trust_state(LocalTrust::Verified);
        alice_machine.store.save_devices(&[alice_device]).await.unwrap();

        // Create Olm sessions for our two accounts.
        let (alice_session, bob_session) = alice_account.create_session_for(&bob_account).await;

        let alice_device = ReadOnlyDevice::from_account(&alice_account).await;
        let bob_device = ReadOnlyDevice::from_account(&bob_account).await;

        // Populate our stores with Olm sessions and a Megolm session.

        alice_machine.store.save_devices(&[bob_device]).await.unwrap();
        bob_machine.store.save_devices(&[alice_device]).await.unwrap();

        let (group_session, inbound_group_session) =
            bob_account.create_group_session_pair_with_defaults(&room_id()).await.unwrap();

        bob_machine.store.save_inbound_group_sessions(&[inbound_group_session]).await.unwrap();

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
        bob_machine.outbound_group_sessions.insert(group_session.clone());

        // Get the request and convert it into a event.
        let requests = alice_machine.outgoing_to_device_requests().await.unwrap();
        let request = &requests[0];
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
        let content: RoomKeyRequestToDeviceEventContent = content.deserialize_as().unwrap();

        alice_machine.mark_outgoing_request_as_sent(id).await.unwrap();

        let event = ToDeviceEvent { sender: alice_id(), content };

        // Bob doesn't have any outgoing requests.
        assert!(bob_machine.outgoing_to_device_requests().await.unwrap().is_empty());
        assert!(bob_machine.users_for_key_claim.is_empty());
        assert!(bob_machine.wait_queue.is_empty());

        // Receive the room key request from alice.
        bob_machine.receive_incoming_key_request(&event);
        bob_machine.collect_incoming_key_requests().await.unwrap();
        // Bob doesn't have an outgoing requests since we're lacking a session.
        assert!(bob_machine.outgoing_to_device_requests().await.unwrap().is_empty());
        assert!(!bob_machine.users_for_key_claim.is_empty());
        assert!(!bob_machine.wait_queue.is_empty());

        // We create a session now.
        alice_machine.store.save_sessions(&[alice_session]).await.unwrap();
        bob_machine.store.save_sessions(&[bob_session]).await.unwrap();

        bob_machine.retry_keyshare(&alice_id(), &alice_device_id());
        assert!(bob_machine.users_for_key_claim.is_empty());
        bob_machine.collect_incoming_key_requests().await.unwrap();
        // Bob now has an outgoing requests.
        assert!(!bob_machine.outgoing_to_device_requests().await.unwrap().is_empty());
        assert!(bob_machine.wait_queue.is_empty());

        // Get the request and convert it to a encrypted to-device event.
        let requests = bob_machine.outgoing_to_device_requests().await.unwrap();

        let request = &requests[0];

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
        let content: EncryptedToDeviceEventContent = content.deserialize_as().unwrap();

        bob_machine.mark_outgoing_request_as_sent(id).await.unwrap();

        let event = ToDeviceEvent { sender: bob_id(), content };

        // Check that alice doesn't have the session.
        assert!(alice_machine
            .store
            .get_inbound_group_session(
                &room_id(),
                bob_account.identity_keys().curve25519(),
                group_session.session_id()
            )
            .await
            .unwrap()
            .is_none());

        let decrypted = alice_account.decrypt_to_device_event(&event).await.unwrap();

        if let AnyToDeviceEvent::ForwardedRoomKey(mut e) = decrypted.event.deserialize().unwrap() {
            let (_, session) = alice_machine
                .receive_forwarded_room_key(&decrypted.sender_key, &mut e)
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
                &room_id(),
                &decrypted.sender_key,
                group_session.session_id(),
            )
            .await
            .unwrap()
            .unwrap();

        assert_eq!(session.session_id(), group_session.session_id())
    }
}
