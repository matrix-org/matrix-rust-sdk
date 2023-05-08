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

use std::sync::Arc;

use dashmap::{DashMap, DashSet};
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
    types::events::room_key_request::{
        RoomKeyRequestContent, RoomKeyRequestEvent, SupportedKeyInfo,
    },
    Device,
};

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
            SecretInfo::KeyRequest(info) => format!(
                "keyRequest:{}:{}:{}",
                info.room_id().as_str(),
                info.session_id(),
                &info.algorithm(),
            ),
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
#[derive(Debug, Clone)]
struct WaitQueue {
    requests_waiting_for_session: Arc<DashMap<RequestInfo, RequestEvent>>,
    #[allow(clippy::type_complexity)]
    requests_ids_waiting: Arc<DashMap<(OwnedUserId, OwnedDeviceId), DashSet<OwnedTransactionId>>>,
}

impl WaitQueue {
    fn new() -> Self {
        Self {
            requests_waiting_for_session: Arc::new(DashMap::new()),
            requests_ids_waiting: Arc::new(DashMap::new()),
        }
    }

    #[cfg(all(test, feature = "automatic-room-key-forwarding"))]
    fn is_empty(&self) -> bool {
        self.requests_ids_waiting.is_empty() && self.requests_waiting_for_session.is_empty()
    }

    fn insert(&self, device: &Device, event: RequestEvent) {
        let request_id = event.request_id().to_owned();

        let key = RequestInfo::new(
            device.user_id().to_owned(),
            device.device_id().into(),
            request_id.clone(),
        );
        self.requests_waiting_for_session.insert(key, event);

        let key = (device.user_id().to_owned(), device.device_id().into());
        self.requests_ids_waiting.entry(key).or_default().insert(request_id);
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
