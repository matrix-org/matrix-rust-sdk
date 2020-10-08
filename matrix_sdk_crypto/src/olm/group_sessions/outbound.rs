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

use dashmap::{DashMap, DashSet};
use matrix_sdk_common::{api::r0::to_device::DeviceIdOrAllDevices, uuid::Uuid};
use std::{
    cmp::min,
    fmt,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tracing::debug;

use matrix_sdk_common::{
    events::{
        room::{encrypted::EncryptedEventContent, encryption::EncryptionEventContent},
        AnyMessageEventContent, EventContent,
    },
    identifiers::{DeviceId, DeviceIdBox, EventEncryptionAlgorithm, RoomId, UserId},
    instant::Instant,
    locks::Mutex,
};
use serde_json::{json, Value};

use olm_rs::outbound_group_session::OlmOutboundGroupSession;
pub use olm_rs::{
    account::IdentityKeys,
    session::{OlmMessage, PreKeyMessage},
    utility::OlmUtility,
};

use crate::ToDeviceRequest;

use super::GroupSessionKey;

const ROTATION_PERIOD: Duration = Duration::from_millis(604800000);
const ROTATION_MESSAGES: u64 = 100;

/// Settings for an encrypted room.
///
/// This determines the algorithm and rotation periods of a group session.
#[derive(Debug)]
pub struct EncryptionSettings {
    /// The encryption algorithm that should be used in the room.
    pub algorithm: EventEncryptionAlgorithm,
    /// How long the session should be used before changing it.
    pub rotation_period: Duration,
    /// How many messages should be sent before changing the session.
    pub rotation_period_msgs: u64,
}

impl Default for EncryptionSettings {
    fn default() -> Self {
        Self {
            algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
            rotation_period: ROTATION_PERIOD,
            rotation_period_msgs: ROTATION_MESSAGES,
        }
    }
}

impl From<&EncryptionEventContent> for EncryptionSettings {
    fn from(content: &EncryptionEventContent) -> Self {
        let rotation_period: Duration = content
            .rotation_period_ms
            .map_or(ROTATION_PERIOD, |r| Duration::from_millis(r.into()));
        let rotation_period_msgs: u64 = content
            .rotation_period_msgs
            .map_or(ROTATION_MESSAGES, Into::into);

        Self {
            algorithm: content.algorithm.clone(),
            rotation_period,
            rotation_period_msgs,
        }
    }
}
/// Outbound group session.
///
/// Outbound group sessions are used to exchange room messages between a group
/// of participants. Outbound group sessions are used to encrypt the room
/// messages.
#[derive(Clone)]
pub struct OutboundGroupSession {
    inner: Arc<Mutex<OlmOutboundGroupSession>>,
    device_id: Arc<DeviceIdBox>,
    account_identity_keys: Arc<IdentityKeys>,
    session_id: Arc<String>,
    room_id: Arc<RoomId>,
    pub(crate) creation_time: Arc<Instant>,
    message_count: Arc<AtomicU64>,
    shared: Arc<AtomicBool>,
    invalidated: Arc<AtomicBool>,
    settings: Arc<EncryptionSettings>,
    shared_with_set: Arc<DashMap<UserId, DashSet<DeviceIdBox>>>,
    to_share_with_set: Arc<DashMap<Uuid, Arc<ToDeviceRequest>>>,
}

impl OutboundGroupSession {
    /// Create a new outbound group session for the given room.
    ///
    /// Outbound group sessions are used to encrypt room messages.
    ///
    /// # Arguments
    ///
    /// * `device_id` - The id of the device that created this session.
    ///
    /// * `identity_keys` - The identity keys of the account that created this
    /// session.
    ///
    /// * `room_id` - The id of the room that the session is used in.
    ///
    /// * `settings` - Settings determining the algorithm and rotation period of
    /// the outbound group session.
    pub fn new(
        device_id: Arc<DeviceIdBox>,
        identity_keys: Arc<IdentityKeys>,
        room_id: &RoomId,
        settings: EncryptionSettings,
    ) -> Self {
        let session = OlmOutboundGroupSession::new();
        let session_id = session.session_id();

        OutboundGroupSession {
            inner: Arc::new(Mutex::new(session)),
            room_id: Arc::new(room_id.to_owned()),
            device_id,
            account_identity_keys: identity_keys,
            session_id: Arc::new(session_id),
            creation_time: Arc::new(Instant::now()),
            message_count: Arc::new(AtomicU64::new(0)),
            shared: Arc::new(AtomicBool::new(false)),
            invalidated: Arc::new(AtomicBool::new(false)),
            settings: Arc::new(settings),
            shared_with_set: Arc::new(DashMap::new()),
            to_share_with_set: Arc::new(DashMap::new()),
        }
    }

    pub fn add_request(&self, request_id: Uuid, request: Arc<ToDeviceRequest>) {
        self.to_share_with_set.insert(request_id, request);
    }

    pub fn add_recipient(&self, user_id: &UserId) {
        self.shared_with_set
            .entry(user_id.to_owned())
            .or_insert_with(DashSet::new);
    }

    pub fn contains_recipient(&self, user_id: &UserId) -> bool {
        self.shared_with_set.contains_key(user_id)
    }

    /// Mark the request with the given request id as sent.
    ///
    /// This removes the request from the queue and marks the set of
    /// users/devices that received the session.
    pub fn mark_request_as_sent(&self, request_id: &Uuid) {
        if let Some((_, r)) = self.to_share_with_set.remove(request_id) {
            let user_pairs = r.messages.iter().map(|(u, v)| {
                (
                    u.clone(),
                    v.keys().filter_map(|d| {
                        if let DeviceIdOrAllDevices::DeviceId(d) = d {
                            Some(d.clone())
                        } else {
                            None
                        }
                    }),
                )
            });

            user_pairs.for_each(|(u, d)| {
                self.shared_with_set
                    .entry(u)
                    .or_insert_with(DashSet::new)
                    .extend(d);
            });

            if self.to_share_with_set.is_empty() {
                debug!(
                    "Marking session {} for room {} as shared.",
                    self.session_id(),
                    self.room_id
                );
                self.mark_as_shared();
            }
        }
    }

    /// Encrypt the given plaintext using this session.
    ///
    /// Returns the encrypted ciphertext.
    ///
    /// # Arguments
    ///
    /// * `plaintext` - The plaintext that should be encrypted.
    pub(crate) async fn encrypt_helper(&self, plaintext: String) -> String {
        let session = self.inner.lock().await;
        self.message_count.fetch_add(1, Ordering::SeqCst);
        session.encrypt(plaintext)
    }

    /// Encrypt a room message for the given room.
    ///
    /// Beware that a group session needs to be shared before this method can be
    /// called using the `share_group_session()` method.
    ///
    /// Since group sessions can expire or become invalid if the room membership
    /// changes client authors should check with the
    /// `should_share_group_session()` method if a new group session needs to
    /// be shared.
    ///
    /// # Arguments
    ///
    /// * `content` - The plaintext content of the message that should be
    /// encrypted.
    ///
    /// # Panics
    ///
    /// Panics if the content can't be serialized.
    pub async fn encrypt(&self, content: AnyMessageEventContent) -> EncryptedEventContent {
        let json_content = json!({
            "content": content,
            "room_id": &*self.room_id,
            "type": content.event_type(),
        });

        let plaintext = cjson::to_string(&json_content).unwrap_or_else(|_| {
            panic!(format!(
                "Can't serialize {} to canonical JSON",
                json_content
            ))
        });

        let ciphertext = self.encrypt_helper(plaintext).await;

        EncryptedEventContent::MegolmV1AesSha2(
            matrix_sdk_common::events::room::encrypted::MegolmV1AesSha2ContentInit {
                ciphertext,
                sender_key: self.account_identity_keys.curve25519().to_owned(),
                session_id: self.session_id().to_owned(),
                device_id: (&*self.device_id).to_owned(),
            }
            .into(),
        )
    }

    /// Check if the session has expired and if it should be rotated.
    ///
    /// A session will expire after some time or if enough messages have been
    /// encrypted using it.
    pub fn expired(&self) -> bool {
        let count = self.message_count.load(Ordering::SeqCst);

        count >= self.settings.rotation_period_msgs
            || self.creation_time.elapsed()
                // Since the encryption settings are provided by users and not
                // checked someone could set a really low rotation perdiod so
                // clamp it at a minute.
                >= min(self.settings.rotation_period, Duration::from_secs(3600))
    }

    /// Has the session been invalidated.
    pub fn invalidated(&self) -> bool {
        self.invalidated.load(Ordering::Relaxed)
    }

    /// Mark the session as shared.
    ///
    /// Messages shouldn't be encrypted with the session before it has been
    /// shared.
    pub fn mark_as_shared(&self) {
        self.shared.store(true, Ordering::Relaxed);
    }

    /// Check if the session has been marked as shared.
    pub fn shared(&self) -> bool {
        self.shared.load(Ordering::Relaxed)
    }

    /// Get the session key of this session.
    ///
    /// A session key can be used to to create an `InboundGroupSession`.
    pub async fn session_key(&self) -> GroupSessionKey {
        let session = self.inner.lock().await;
        GroupSessionKey(session.session_key())
    }

    /// Get the room id of the room this session belongs to.
    pub fn room_id(&self) -> &RoomId {
        &self.room_id
    }

    /// Returns the unique identifier for this session.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Get the current message index for this session.
    ///
    /// Each message is sent with an increasing index. This returns the
    /// message index that will be used for the next encrypted message.
    pub async fn message_index(&self) -> u32 {
        let session = self.inner.lock().await;
        session.session_message_index()
    }

    /// Get the outbound group session key as a json value that can be sent as a
    /// m.room_key.
    pub async fn as_json(&self) -> Value {
        json!({
            "algorithm": EventEncryptionAlgorithm::MegolmV1AesSha2,
            "room_id": &*self.room_id,
            "session_id": &*self.session_id,
            "session_key": self.session_key().await,
            "chain_index": self.message_index().await,
        })
    }

    /// Mark the session as invalid.
    ///
    /// This should be called if an user/device deletes a device that received
    /// this session.
    pub fn invalidate_session(&self) {
        self.invalidated.store(true, Ordering::Relaxed)
    }

    /// Clear out the requests returning the request ids.
    pub fn clear_requests(&self) -> Vec<Uuid> {
        let request_ids = self
            .to_share_with_set
            .iter()
            .map(|item| *item.key())
            .collect();
        self.to_share_with_set.clear();
        request_ids
    }

    /// Has or will the session be shared with the given user/device pair.
    pub(crate) fn is_shared_with(&self, user_id: &UserId, device_id: &DeviceId) -> bool {
        let shared_with = self
            .shared_with_set
            .get(user_id)
            .map(|d| d.contains(device_id))
            .unwrap_or(false);

        let should_be_shared_with = if self.shared() {
            false
        } else {
            let device_id = DeviceIdOrAllDevices::DeviceId(device_id.into());

            self.to_share_with_set.iter().any(|item| {
                if let Some(e) = item.value().messages.get(user_id) {
                    e.contains_key(&device_id)
                } else {
                    false
                }
            })
        };

        shared_with || should_be_shared_with
    }

    /// Mark that the session was shared with the given user/device pair.
    #[cfg(test)]
    pub fn mark_shared_with(&self, user_id: &UserId, device_id: &DeviceId) {
        self.shared_with_set
            .entry(user_id.to_owned())
            .or_insert_with(DashSet::new)
            .insert(device_id.to_owned());
    }
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for OutboundGroupSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutboundGroupSession")
            .field("session_id", &self.session_id)
            .field("room_id", &self.room_id)
            .field("creation_time", &self.creation_time)
            .field("message_count", &self.message_count)
            .finish()
    }
}

#[cfg(test)]
mod test {
    use std::time::Duration;

    use matrix_sdk_common::{
        events::room::encryption::EncryptionEventContent, identifiers::EventEncryptionAlgorithm,
        js_int::uint,
    };

    use super::{EncryptionSettings, ROTATION_MESSAGES, ROTATION_PERIOD};

    #[test]
    fn encryption_settings_conversion() {
        let mut content = EncryptionEventContent::new(EventEncryptionAlgorithm::MegolmV1AesSha2);
        let settings = EncryptionSettings::from(&content);

        assert_eq!(settings.rotation_period, ROTATION_PERIOD);
        assert_eq!(settings.rotation_period_msgs, ROTATION_MESSAGES);

        content.rotation_period_ms = Some(uint!(3600));
        content.rotation_period_msgs = Some(uint!(500));

        let settings = EncryptionSettings::from(&content);

        assert_eq!(settings.rotation_period, Duration::from_millis(3600));
        assert_eq!(settings.rotation_period_msgs, 500);
    }
}
