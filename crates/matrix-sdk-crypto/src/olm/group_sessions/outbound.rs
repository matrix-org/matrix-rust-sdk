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

use std::{
    cmp::max,
    collections::{BTreeMap, BTreeSet},
    fmt,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use dashmap::DashMap;
use matrix_sdk_common::locks::RwLock;
use ruma::{
    events::room::{encryption::RoomEncryptionEventContent, history_visibility::HistoryVisibility},
    serde::Raw,
    DeviceId, OwnedDeviceId, OwnedTransactionId, OwnedUserId, RoomId, SecondsSinceUnixEpoch,
    TransactionId, UserId,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::{debug, error, info};
use vodozemac::{megolm::SessionConfig, Curve25519PublicKey};
pub use vodozemac::{
    megolm::{GroupSession, GroupSessionPickle, MegolmMessage, SessionKey},
    olm::IdentityKeys,
    PickleError,
};

use super::SessionCreationError;
#[cfg(feature = "experimental-algorithms")]
use crate::types::events::room::encrypted::MegolmV2AesSha2Content;
use crate::{
    types::{
        events::{
            room::encrypted::{
                MegolmV1AesSha2Content, RoomEncryptedEventContent, RoomEventEncryptionScheme,
            },
            room_key::{MegolmV1AesSha2Content as MegolmV1AesSha2RoomKeyContent, RoomKeyContent},
            room_key_withheld::{RoomKeyWithheldContent, WithheldCode},
        },
        EventEncryptionAlgorithm,
    },
    Device, ToDeviceRequest,
};

const ROTATION_PERIOD: Duration = Duration::from_millis(604800000);
const ROTATION_MESSAGES: u64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ShareState {
    NotShared,
    SharedButChangedSenderKey,
    Shared(u32),
}

/// Settings for an encrypted room.
///
/// This determines the algorithm and rotation periods of a group session.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EncryptionSettings {
    /// The encryption algorithm that should be used in the room.
    pub algorithm: EventEncryptionAlgorithm,
    /// How long the session should be used before changing it.
    pub rotation_period: Duration,
    /// How many messages should be sent before changing the session.
    pub rotation_period_msgs: u64,
    /// The history visibility of the room when the session was created.
    pub history_visibility: HistoryVisibility,
    /// Should untrusted devices receive the room key, or should they be
    /// excluded from the conversation.
    #[serde(default)]
    pub only_allow_trusted_devices: bool,
}

impl Default for EncryptionSettings {
    fn default() -> Self {
        Self {
            algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
            rotation_period: ROTATION_PERIOD,
            rotation_period_msgs: ROTATION_MESSAGES,
            history_visibility: HistoryVisibility::Shared,
            only_allow_trusted_devices: false,
        }
    }
}

impl EncryptionSettings {
    /// Create new encryption settings using an `RoomEncryptionEventContent`,
    /// a history visibility, and setting if only trusted devices should receive
    /// a room key.
    pub fn new(
        content: RoomEncryptionEventContent,
        history_visibility: HistoryVisibility,
        only_allow_trusted_devices: bool,
    ) -> Self {
        let rotation_period: Duration =
            content.rotation_period_ms.map_or(ROTATION_PERIOD, |r| Duration::from_millis(r.into()));
        let rotation_period_msgs: u64 =
            content.rotation_period_msgs.map_or(ROTATION_MESSAGES, Into::into);

        Self {
            algorithm: EventEncryptionAlgorithm::from(content.algorithm.as_str()),
            rotation_period,
            rotation_period_msgs,
            history_visibility,
            only_allow_trusted_devices,
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
    inner: Arc<RwLock<GroupSession>>,
    device_id: Arc<DeviceId>,
    account_identity_keys: Arc<IdentityKeys>,
    session_id: Arc<str>,
    room_id: Arc<RoomId>,
    pub(crate) creation_time: SecondsSinceUnixEpoch,
    message_count: Arc<AtomicU64>,
    shared: Arc<AtomicBool>,
    invalidated: Arc<AtomicBool>,
    settings: Arc<EncryptionSettings>,
    pub(crate) shared_with_set: Arc<DashMap<OwnedUserId, DashMap<OwnedDeviceId, ShareInfo>>>,
    to_share_with_set: Arc<DashMap<OwnedTransactionId, (Arc<ToDeviceRequest>, ShareInfoSet)>>,
}

/// A a map of userid/device it to a `ShareInfo`.
///
/// Holds the `ShareInfo` for all the user/device pairs that will receive the
/// room key.
pub type ShareInfoSet = BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceId, ShareInfo>>;

/// Struct holding info about the share state of a outbound group session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ShareInfo {
    /// When the key has been shared
    Shared(SharedWith),
    /// When the session has been withheld
    Withheld(WithheldCode),
}

impl ShareInfo {
    /// Helper to create a SharedWith info
    pub fn new_shared(sender_key: Curve25519PublicKey, message_index: u32) -> Self {
        ShareInfo::Shared(SharedWith { sender_key, message_index })
    }

    /// Helper to create a Withheld info
    pub fn new_withheld(code: WithheldCode) -> Self {
        ShareInfo::Withheld(code)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SharedWith {
    /// The sender key of the device that was used to encrypt the room key.
    pub sender_key: Curve25519PublicKey,
    /// The message index that the device received.
    pub message_index: u32,
}

impl OutboundGroupSession {
    pub(super) fn session_config(
        algorithm: &EventEncryptionAlgorithm,
    ) -> Result<SessionConfig, SessionCreationError> {
        match algorithm {
            EventEncryptionAlgorithm::MegolmV1AesSha2 => Ok(SessionConfig::version_1()),
            #[cfg(feature = "experimental-algorithms")]
            EventEncryptionAlgorithm::MegolmV2AesSha2 => Ok(SessionConfig::version_2()),
            _ => Err(SessionCreationError::Algorithm(algorithm.to_owned())),
        }
    }

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
        device_id: Arc<DeviceId>,
        identity_keys: Arc<IdentityKeys>,
        room_id: &RoomId,
        settings: EncryptionSettings,
    ) -> Result<Self, SessionCreationError> {
        let config = Self::session_config(&settings.algorithm)?;

        let session = GroupSession::new(config);
        let session_id = session.session_id();

        Ok(OutboundGroupSession {
            inner: RwLock::new(session).into(),
            room_id: room_id.into(),
            device_id,
            account_identity_keys: identity_keys,
            session_id: session_id.into(),
            creation_time: SecondsSinceUnixEpoch::now(),
            message_count: Arc::new(AtomicU64::new(0)),
            shared: Arc::new(AtomicBool::new(false)),
            invalidated: Arc::new(AtomicBool::new(false)),
            settings: Arc::new(settings),
            shared_with_set: Arc::new(DashMap::new()),
            to_share_with_set: Arc::new(DashMap::new()),
        })
    }

    pub(crate) fn add_request(
        &self,
        request_id: OwnedTransactionId,
        request: Arc<ToDeviceRequest>,
        share_infos: ShareInfoSet,
    ) {
        self.to_share_with_set.insert(request_id, (request, share_infos));
    }

    /// Create a new `m.room_key.withheld` event content with the given code for
    /// this outbound group session.
    pub fn withheld_code(&self, code: WithheldCode) -> RoomKeyWithheldContent {
        RoomKeyWithheldContent::new(
            self.settings().algorithm.to_owned(),
            code,
            self.room_id().to_owned(),
            self.session_id().to_owned(),
            self.sender_key().to_owned(),
            (*self.device_id).to_owned(),
        )
    }

    /// This should be called if an the user wishes to rotate this session.
    pub fn invalidate_session(&self) {
        self.invalidated.store(true, Ordering::Relaxed)
    }

    /// Get the encryption settings of this outbound session.
    pub fn settings(&self) -> &EncryptionSettings {
        &self.settings
    }

    /// Mark the request with the given request id as sent.
    ///
    /// This removes the request from the queue and marks the set of
    /// users/devices that received the session.
    pub fn mark_request_as_sent(
        &self,
        request_id: &TransactionId,
    ) -> BTreeMap<OwnedUserId, BTreeSet<OwnedDeviceId>> {
        let mut no_olm_devices = BTreeMap::new();

        if let Some((_, (to_device, request))) = self.to_share_with_set.remove(request_id) {
            let recipients: BTreeMap<&UserId, BTreeSet<&DeviceId>> = request
                .iter()
                .map(|(u, d)| (&**u, d.keys().map(|d| d.as_ref()).collect()))
                .collect();

            info!(
                ?request_id,
                ?recipients,
                ?to_device.event_type,
                "Marking to-device request carrying a room key or a withheld as sent"
            );

            for (user_id, info) in request {
                let no_olms: BTreeSet<OwnedDeviceId> = info
                    .iter()
                    .filter(|(_, info)| matches!(info, ShareInfo::Withheld(WithheldCode::NoOlm)))
                    .map(|(d, _)| d.to_owned())
                    .collect();
                no_olm_devices.insert(user_id.to_owned(), no_olms);

                self.shared_with_set.entry(user_id).or_default().extend(info);
            }

            if self.to_share_with_set.is_empty() {
                debug!(
                    session_id = self.session_id(),
                    room_id = ?self.room_id,
                    "All m.room_key and withheld to-device requests were sent out, marking \
                        session as shared.",
                );

                self.mark_as_shared();
            }
        } else {
            let request_ids: Vec<String> =
                self.to_share_with_set.iter().map(|e| e.key().to_string()).collect();

            error!(
                all_request_ids = ?request_ids,
                request_id = ?request_id,
                "Marking to-device request carrying a room key as sent but no \
                    request found with the given id"
            );
        }

        no_olm_devices
    }

    /// Encrypt the given plaintext using this session.
    ///
    /// Returns the encrypted ciphertext.
    ///
    /// # Arguments
    ///
    /// * `plaintext` - The plaintext that should be encrypted.
    pub(crate) async fn encrypt_helper(&self, plaintext: String) -> MegolmMessage {
        let mut session = self.inner.write().await;
        self.message_count.fetch_add(1, Ordering::SeqCst);
        session.encrypt(&plaintext)
    }

    /// Encrypt a room message for the given room.
    ///
    /// Beware that a room key needs to be shared before this method
    /// can be called using the `share_room_key()` method.
    ///
    /// # Arguments
    ///
    /// * `content` - The plaintext content of the message that should be
    /// encrypted in raw json [`Value`] form.
    ///
    /// * `event_type` - The plaintext type of the event, the outer type of the
    /// event will become `m.room.encrypted`.
    ///
    /// # Panics
    ///
    /// Panics if the content can't be serialized.
    pub async fn encrypt(
        &self,
        content: Value,
        event_type: &str,
    ) -> Raw<RoomEncryptedEventContent> {
        let json_content = json!({
            "content": content,
            "room_id": &*self.room_id,
            "type": event_type,
        });

        let plaintext = json_content.to_string();
        let relates_to = content.get("m.relates_to").cloned();

        let ciphertext = self.encrypt_helper(plaintext).await;
        let scheme: RoomEventEncryptionScheme = match self.settings.algorithm {
            EventEncryptionAlgorithm::MegolmV1AesSha2 => MegolmV1AesSha2Content {
                ciphertext,
                sender_key: self.account_identity_keys.curve25519,
                session_id: self.session_id().to_owned(),
                device_id: (*self.device_id).to_owned(),
            }
            .into(),
            #[cfg(feature = "experimental-algorithms")]
            EventEncryptionAlgorithm::MegolmV2AesSha2 => {
                MegolmV2AesSha2Content { ciphertext, session_id: self.session_id().to_owned() }
                    .into()
            }
            _ => unreachable!(
                "An outbound group session is always using one of the supported algorithms"
            ),
        };

        let content = RoomEncryptedEventContent { scheme, relates_to, other: Default::default() };

        Raw::new(&content).expect("m.room.encrypted event content can always be serialized")
    }

    fn elapsed(&self) -> bool {
        let creation_time = Duration::from_secs(self.creation_time.get().into());
        let now = Duration::from_secs(SecondsSinceUnixEpoch::now().get().into());

        // Since the encryption settings are provided by users and not
        // checked someone could set a really low rotation period so
        // clamp it to an hour.
        now.checked_sub(creation_time)
            .map(|elapsed| elapsed >= max(self.settings.rotation_period, Duration::from_secs(3600)))
            .unwrap_or(true)
    }

    /// Check if the session has expired and if it should be rotated.
    ///
    /// A session will expire after some time or if enough messages have been
    /// encrypted using it.
    pub fn expired(&self) -> bool {
        let count = self.message_count.load(Ordering::SeqCst);
        // We clamp the rotation period for message counts to be between 1 and
        // 10000. The Megolm session should be usable for at least 1 message,
        // and at most 10000 messages. Realistically Megolm uses u32 for it's
        // internal counter and one could use the Megolm session for up to
        // u32::MAX messages, but we're staying on the safe side of things.
        let rotation_period_msgs = self.settings.rotation_period_msgs.clamp(1, 10_000);

        count >= rotation_period_msgs || self.elapsed()
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
    pub async fn session_key(&self) -> SessionKey {
        let session = self.inner.read().await;
        session.session_key()
    }

    /// Gets the Sender Key
    pub fn sender_key(&self) -> Curve25519PublicKey {
        self.account_identity_keys.as_ref().curve25519.to_owned()
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
        let session = self.inner.read().await;
        session.message_index()
    }

    pub(crate) async fn as_content(&self) -> RoomKeyContent {
        let session_key = self.session_key().await;

        RoomKeyContent::MegolmV1AesSha2(
            MegolmV1AesSha2RoomKeyContent::new(
                self.room_id().to_owned(),
                self.session_id().to_owned(),
                session_key,
            )
            .into(),
        )
    }

    /// Has or will the session be shared with the given user/device pair.
    pub(crate) fn is_shared_with(&self, device: &Device) -> ShareState {
        // Check if we shared the session.
        let shared_state = self.shared_with_set.get(device.user_id()).and_then(|d| {
            d.get(device.device_id()).map(|s| match s.value() {
                ShareInfo::Shared(s) => {
                    if Some(s.sender_key) == device.curve25519_key() {
                        ShareState::Shared(s.message_index)
                    } else {
                        ShareState::SharedButChangedSenderKey
                    }
                }
                ShareInfo::Withheld(_) => ShareState::NotShared,
            })
        });

        if let Some(state) = shared_state {
            state
        } else {
            // If we haven't shared the session, check if we're going to share
            // the session.

            // Find the first request that contains the given user id and
            // device ID.
            let shared = self.to_share_with_set.iter().find_map(|item| {
                let share_info = &item.value().1;

                share_info.get(device.user_id()).and_then(|d| {
                    d.get(device.device_id()).map(|info| match info {
                        ShareInfo::Shared(info) => {
                            if Some(info.sender_key) == device.curve25519_key() {
                                ShareState::Shared(info.message_index)
                            } else {
                                ShareState::SharedButChangedSenderKey
                            }
                        }
                        ShareInfo::Withheld(_) => ShareState::NotShared,
                    })
                })
            });

            shared.unwrap_or(ShareState::NotShared)
        }
    }

    pub(crate) fn is_withheld_to(&self, device: &Device, code: &WithheldCode) -> bool {
        self
            .shared_with_set.get(device.user_id())
            .and_then(|d| {
                let info = d.get(device.device_id())?;
                Some(matches!(info.value(), ShareInfo::Withheld(c) if c == code))
            })
            .unwrap_or_else(|| {
                // If we haven't yet withheld, check if we're going to withheld
                // the session.

                // Find the first request that contains the given user id and
                // device ID.
                self.to_share_with_set.iter().any(|item| {
                    let share_info = &item.value().1;

                    share_info
                        .get(device.user_id())
                        .and_then(|d| {
                            let info = d.get(device.device_id())?;
                            Some(matches!(info, ShareInfo::Withheld(c) if c == code))
                        })
                        .unwrap_or(false)
                })
            })
    }

    /// Mark the session as shared with the given user/device pair, starting
    /// from some message index.
    #[cfg(test)]
    pub fn mark_shared_with_from_index(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        sender_key: Curve25519PublicKey,
        index: u32,
    ) {
        self.shared_with_set
            .entry(user_id.to_owned())
            .or_default()
            .insert(device_id.to_owned(), ShareInfo::new_shared(sender_key, index));
    }

    /// Mark the session as shared with the given user/device pair, starting
    /// from the current index.
    #[cfg(test)]
    pub async fn mark_shared_with(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        sender_key: Curve25519PublicKey,
    ) {
        self.shared_with_set.entry(user_id.to_owned()).or_default().insert(
            device_id.to_owned(),
            ShareInfo::new_shared(sender_key, self.message_index().await),
        );
    }

    /// Get the list of requests that need to be sent out for this session to be
    /// marked as shared.
    pub(crate) fn pending_requests(&self) -> Vec<Arc<ToDeviceRequest>> {
        self.to_share_with_set.iter().map(|i| i.value().0.clone()).collect()
    }

    /// Get the list of request ids this session is waiting for to be sent out.
    pub(crate) fn pending_request_ids(&self) -> Vec<OwnedTransactionId> {
        self.to_share_with_set.iter().map(|e| e.key().clone()).collect()
    }

    /// Restore a Session from a previously pickled string.
    ///
    /// Returns the restored group session or a `OlmGroupSessionError` if there
    /// was an error.
    ///
    /// # Arguments
    ///
    /// * `device_id` - The device ID of the device that created this session.
    ///   Put differently, our own device ID.
    ///
    /// * `identity_keys` - The identity keys of the device that created this
    ///   session, our own identity keys.
    ///
    /// * `pickle` - The pickled version of the `OutboundGroupSession`.
    ///
    /// * `pickle_mode` - The mode that was used to pickle the session, either
    /// an unencrypted mode or an encrypted using passphrase.
    pub fn from_pickle(
        device_id: Arc<DeviceId>,
        identity_keys: Arc<IdentityKeys>,
        pickle: PickledOutboundGroupSession,
    ) -> Result<Self, PickleError> {
        let inner: GroupSession = pickle.pickle.into();
        let session_id = inner.session_id();

        Ok(Self {
            inner: Arc::new(RwLock::new(inner)),
            device_id,
            account_identity_keys: identity_keys,
            session_id: session_id.into(),
            room_id: pickle.room_id,
            creation_time: pickle.creation_time,
            message_count: AtomicU64::from(pickle.message_count).into(),
            shared: AtomicBool::from(pickle.shared).into(),
            invalidated: AtomicBool::from(pickle.invalidated).into(),
            settings: pickle.settings,
            shared_with_set: Arc::new(
                pickle
                    .shared_with_set
                    .into_iter()
                    .map(|(k, v)| (k, v.into_iter().collect()))
                    .collect(),
            ),
            to_share_with_set: Arc::new(pickle.requests.into_iter().collect()),
        })
    }

    /// Store the group session as a base64 encoded string and associated data
    /// belonging to the session.
    ///
    /// # Arguments
    ///
    /// * `pickle_mode` - The mode that should be used to pickle the group
    ///   session,
    /// either an unencrypted mode or an encrypted using passphrase.
    pub async fn pickle(&self) -> PickledOutboundGroupSession {
        let pickle = self.inner.read().await.pickle();

        PickledOutboundGroupSession {
            pickle,
            room_id: self.room_id.clone(),
            settings: self.settings.clone(),
            creation_time: self.creation_time,
            message_count: self.message_count.load(Ordering::SeqCst),
            shared: self.shared(),
            invalidated: self.invalidated(),
            shared_with_set: self
                .shared_with_set
                .iter()
                .map(|u| {
                    (
                        u.key().clone(),
                        u.value().iter().map(|d| (d.key().clone(), d.value().clone())).collect(),
                    )
                })
                .collect(),
            requests: self
                .to_share_with_set
                .iter()
                .map(|r| (r.key().clone(), r.value().clone()))
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutboundGroupSessionPickle(String);

impl From<String> for OutboundGroupSessionPickle {
    fn from(p: String) -> Self {
        Self(p)
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

/// A pickled version of an `InboundGroupSession`.
///
/// Holds all the information that needs to be stored in a database to restore
/// an InboundGroupSession.
#[derive(Deserialize, Serialize)]
#[allow(missing_debug_implementations)]
pub struct PickledOutboundGroupSession {
    /// The pickle string holding the OutboundGroupSession.
    pub pickle: GroupSessionPickle,
    /// The settings this session adheres to.
    pub settings: Arc<EncryptionSettings>,
    /// The room id this session is used for.
    pub room_id: Arc<RoomId>,
    /// The timestamp when this session was created.
    pub creation_time: SecondsSinceUnixEpoch,
    /// The number of messages this session has already encrypted.
    pub message_count: u64,
    /// Is the session shared.
    pub shared: bool,
    /// Has the session been invalidated.
    pub invalidated: bool,
    /// The set of users the session has been already shared with.
    pub shared_with_set: BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceId, ShareInfo>>,
    /// Requests that need to be sent out to share the session.
    pub requests: BTreeMap<OwnedTransactionId, (Arc<ToDeviceRequest>, ShareInfoSet)>,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use atomic::Ordering;
    use matrix_sdk_test::async_test;
    use ruma::{
        device_id,
        events::room::{
            encryption::RoomEncryptionEventContent, history_visibility::HistoryVisibility,
            message::RoomMessageEventContent,
        },
        room_id, uint, user_id, EventEncryptionAlgorithm,
    };

    use super::{EncryptionSettings, ROTATION_MESSAGES, ROTATION_PERIOD};
    use crate::{MegolmError, ReadOnlyAccount};

    #[test]
    fn encryption_settings_conversion() {
        let mut content =
            RoomEncryptionEventContent::new(EventEncryptionAlgorithm::MegolmV1AesSha2);
        let settings = EncryptionSettings::new(content.clone(), HistoryVisibility::Joined, false);

        assert_eq!(settings.rotation_period, ROTATION_PERIOD);
        assert_eq!(settings.rotation_period_msgs, ROTATION_MESSAGES);

        content.rotation_period_ms = Some(uint!(3600));
        content.rotation_period_msgs = Some(uint!(500));

        let settings = EncryptionSettings::new(content, HistoryVisibility::Shared, false);

        assert_eq!(settings.rotation_period, Duration::from_millis(3600));
        assert_eq!(settings.rotation_period_msgs, 500);
    }

    #[async_test]
    #[cfg(any(target_os = "linux", target_os = "macos", target_arch = "wasm32"))]
    async fn expiration() -> Result<(), MegolmError> {
        use ruma::SecondsSinceUnixEpoch;

        let settings = EncryptionSettings { rotation_period_msgs: 1, ..Default::default() };

        let account = ReadOnlyAccount::new(user_id!("@alice:example.org"), device_id!("DEVICEID"));
        let (session, _) = account
            .create_group_session_pair(room_id!("!test_room:example.org"), settings)
            .await
            .unwrap();

        assert!(!session.expired());
        let _ = session
            .encrypt(
                serde_json::to_value(RoomMessageEventContent::text_plain("Test message"))?,
                "m.room.message",
            )
            .await;
        assert!(session.expired());

        let settings = EncryptionSettings {
            rotation_period: Duration::from_millis(100),
            ..Default::default()
        };

        let (mut session, _) = account
            .create_group_session_pair(room_id!("!test_room:example.org"), settings)
            .await
            .unwrap();

        assert!(!session.expired());

        let now = SecondsSinceUnixEpoch::now();
        session.creation_time = SecondsSinceUnixEpoch(now.get() - uint!(3600));
        assert!(session.expired());

        let settings = EncryptionSettings { rotation_period_msgs: 0, ..Default::default() };

        let (session, _) = account
            .create_group_session_pair(room_id!("!test_room:example.org"), settings)
            .await
            .unwrap();

        assert!(!session.expired());

        let _ = session
            .encrypt(
                serde_json::to_value(RoomMessageEventContent::text_plain("Test message"))?,
                "m.room.message",
            )
            .await;
        assert!(session.expired());

        let settings = EncryptionSettings { rotation_period_msgs: 100_000, ..Default::default() };

        let (session, _) = account
            .create_group_session_pair(room_id!("!test_room:example.org"), settings)
            .await
            .unwrap();

        assert!(!session.expired());
        session.message_count.store(1000, Ordering::SeqCst);
        assert!(!session.expired());
        session.message_count.store(9999, Ordering::SeqCst);
        assert!(!session.expired());
        session.message_count.store(10_000, Ordering::SeqCst);
        assert!(session.expired());

        Ok(())
    }
}
