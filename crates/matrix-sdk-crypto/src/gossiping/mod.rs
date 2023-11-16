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

mod machine;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, RwLock as StdRwLock},
};

pub(crate) use machine::GossipMachine;
use ruma::{
    events::{
        room_key_request::{Action, ToDeviceRoomKeyRequestEventContent},
        secret::request::{
            RequestAction, SecretName, ToDeviceSecretRequestEvent as SecretRequestEvent,
            ToDeviceSecretRequestEventContent as SecretRequestEventContent,
        },
        AnyToDeviceEventContent, ToDeviceEventType,
    },
    serde::Raw,
    to_device::DeviceIdOrAllDevices,
    DeviceId, OwnedDeviceId, OwnedTransactionId, OwnedUserId, TransactionId, UserId,
};
use serde::{Deserialize, Serialize};

use crate::{
    requests::{OutgoingRequest, ToDeviceRequest},
    types::events::{
        olm_v1::DecryptedSecretSendEvent,
        room_key_request::{RoomKeyRequestContent, RoomKeyRequestEvent, SupportedKeyInfo},
    },
    Device,
};

/// Struct containing a `m.secret.send` event and its acompanying info.
///
/// This struct is created only iff the following three things are true:
///
/// 1. We requested the secret.
/// 2. The secret was received over an encrypted channel.
/// 3. The secret it was received from one ouf our own verified devices.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossippedSecret {
    /// The name of the secret.
    pub secret_name: SecretName,
    /// The [`GossipRequest`] that has requested the secret.
    pub gossip_request: GossipRequest,
    /// The `m.secret.send` event containing the actual secret.
    pub event: DecryptedSecretSendEvent,
}

/// An error describing why a key share request won't be honored.
#[cfg(feature = "automatic-room-key-forwarding")]
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum KeyForwardDecision {
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
    /// The outbound session was shared with the device, but the device either
    /// accidentally or maliciously changed their curve25519 sender key.
    #[error("the device has changed their curve25519 sender key")]
    ChangedSenderKey,
}

/// A struct describing an outgoing key request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipRequest {
    /// The user we requested the secret from
    pub request_recipient: OwnedUserId,
    /// The unique id of the secret request.
    pub request_id: OwnedTransactionId,
    /// The info of the requested secret.
    pub info: SecretInfo,
    /// Has the request been sent out.
    pub sent_out: bool,
}

/// An enum over the various secret request types we can have.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum SecretInfo {
    /// Info for the `m.room_key_request` variant
    KeyRequest(SupportedKeyInfo),
    /// Info for the `m.secret.request` variant
    SecretRequest(SecretName),
}

impl SecretInfo {
    /// Serialize `SecretInfo` into `String` for usage as database keys and
    /// comparison.
    pub fn as_key(&self) -> String {
        match &self {
            SecretInfo::KeyRequest(info) => {
                format!("keyRequest:{}:{}:{}", info.room_id(), info.session_id(), info.algorithm())
            }
            SecretInfo::SecretRequest(sname) => format!("secretName:{sname}"),
        }
    }
}

impl<T> From<T> for SecretInfo
where
    T: Into<SupportedKeyInfo>,
{
    fn from(v: T) -> Self {
        Self::KeyRequest(v.into())
    }
}

impl From<SecretName> for SecretInfo {
    fn from(i: SecretName) -> Self {
        Self::SecretRequest(i)
    }
}

impl GossipRequest {
    /// Create an ougoing secret request for the given secret.
    pub(crate) fn from_secret_name(own_user_id: OwnedUserId, secret_name: SecretName) -> Self {
        Self {
            request_recipient: own_user_id,
            request_id: TransactionId::new(),
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
        let request = match &self.info {
            SecretInfo::KeyRequest(r) => {
                let content = RoomKeyRequestContent::new_request(
                    r.clone().into(),
                    own_device_id.to_owned(),
                    self.request_id.to_owned(),
                );
                let content = Raw::new(&content)
                    .expect("We can always serialize a room key request info")
                    .cast();

                ToDeviceRequest::with_id_raw(
                    &self.request_recipient,
                    DeviceIdOrAllDevices::AllDevices,
                    content,
                    ToDeviceEventType::RoomKeyRequest,
                    self.request_id.clone(),
                )
            }
            SecretInfo::SecretRequest(s) => {
                let content =
                    AnyToDeviceEventContent::SecretRequest(SecretRequestEventContent::new(
                        RequestAction::Request(s.clone()),
                        own_device_id.to_owned(),
                        self.request_id.clone(),
                    ));

                ToDeviceRequest::with_id(
                    &self.request_recipient,
                    DeviceIdOrAllDevices::AllDevices,
                    &content,
                    self.request_id.clone(),
                )
            }
        };

        OutgoingRequest { request_id: request.txn_id.clone(), request: Arc::new(request.into()) }
    }

    fn to_cancellation(&self, own_device_id: &DeviceId) -> OutgoingRequest {
        let content = match self.info {
            SecretInfo::KeyRequest(_) => {
                AnyToDeviceEventContent::RoomKeyRequest(ToDeviceRoomKeyRequestEventContent::new(
                    Action::CancelRequest,
                    None,
                    own_device_id.to_owned(),
                    self.request_id.clone(),
                ))
            }
            SecretInfo::SecretRequest(_) => {
                AnyToDeviceEventContent::SecretRequest(SecretRequestEventContent::new(
                    RequestAction::RequestCancellation,
                    own_device_id.to_owned(),
                    self.request_id.clone(),
                ))
            }
        };

        let request = ToDeviceRequest::with_id(
            &self.request_recipient,
            DeviceIdOrAllDevices::AllDevices,
            &content,
            TransactionId::new(),
        );

        OutgoingRequest { request_id: request.txn_id.clone(), request: Arc::new(request.into()) }
    }
}

impl PartialEq for GossipRequest {
    fn eq(&self, other: &Self) -> bool {
        let is_info_equal = match (&self.info, &other.info) {
            (SecretInfo::KeyRequest(first), SecretInfo::KeyRequest(second)) => first == second,
            (SecretInfo::SecretRequest(first), SecretInfo::SecretRequest(second)) => {
                first == second
            }
            (SecretInfo::KeyRequest(_), SecretInfo::SecretRequest(_))
            | (SecretInfo::SecretRequest(_), SecretInfo::KeyRequest(_)) => false,
        };

        self.request_id == other.request_id && is_info_equal
    }
}

#[derive(Debug)]
enum RequestEvent {
    KeyShare(RoomKeyRequestEvent),
    Secret(SecretRequestEvent),
}

impl From<SecretRequestEvent> for RequestEvent {
    fn from(e: SecretRequestEvent) -> Self {
        Self::Secret(e)
    }
}

impl From<RoomKeyRequestEvent> for RequestEvent {
    fn from(e: RoomKeyRequestEvent) -> Self {
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

    fn request_id(&self) -> &TransactionId {
        match self {
            RequestEvent::KeyShare(e) => &e.content.request_id,
            RequestEvent::Secret(e) => &e.content.request_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct RequestInfo {
    sender: OwnedUserId,
    requesting_device_id: OwnedDeviceId,
    request_id: OwnedTransactionId,
}

impl RequestInfo {
    fn new(
        sender: OwnedUserId,
        requesting_device_id: OwnedDeviceId,
        request_id: OwnedTransactionId,
    ) -> Self {
        Self { sender, requesting_device_id, request_id }
    }
}

/// A queue where we store room key requests that we want to serve but the
/// device that requested the key doesn't share an Olm session with us.
#[derive(Clone, Debug, Default)]
struct WaitQueue {
    inner: Arc<StdRwLock<WaitQueueInner>>,
}

#[derive(Debug, Default)]
struct WaitQueueInner {
    requests_waiting_for_session: BTreeMap<RequestInfo, RequestEvent>,
    requests_ids_waiting: BTreeMap<(OwnedUserId, OwnedDeviceId), BTreeSet<OwnedTransactionId>>,
}

impl WaitQueue {
    fn new() -> Self {
        Self::default()
    }

    #[cfg(all(test, feature = "automatic-room-key-forwarding"))]
    fn is_empty(&self) -> bool {
        let read_guard = self.inner.read().unwrap();
        read_guard.requests_ids_waiting.is_empty()
            && read_guard.requests_waiting_for_session.is_empty()
    }

    fn insert(&self, device: &Device, event: RequestEvent) {
        let request_id = event.request_id().to_owned();
        let requests_waiting_key = RequestInfo::new(
            device.user_id().to_owned(),
            device.device_id().into(),
            request_id.clone(),
        );
        let ids_waiting_key = (device.user_id().to_owned(), device.device_id().into());

        let mut write_guard = self.inner.write().unwrap();
        write_guard.requests_waiting_for_session.insert(requests_waiting_key, event);
        write_guard.requests_ids_waiting.entry(ids_waiting_key).or_default().insert(request_id);
    }

    fn remove(&self, user_id: &UserId, device_id: &DeviceId) -> Vec<(RequestInfo, RequestEvent)> {
        let mut write_guard = self.inner.write().unwrap();
        write_guard
            .requests_ids_waiting
            .remove(&(user_id.to_owned(), device_id.into()))
            .map(|request_ids| {
                request_ids
                    .iter()
                    .filter_map(|id| {
                        let key =
                            RequestInfo::new(user_id.to_owned(), device_id.into(), id.to_owned());
                        write_guard.requests_waiting_for_session.remove_entry(&key)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}
