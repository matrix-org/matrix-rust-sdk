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
    ops::Bound,
    sync::{
        Arc, RwLockReadGuard,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use matrix_sdk_common::{deserialized_responses::WithheldCode, locks::RwLock as StdRwLock};
#[cfg(feature = "experimental-encrypted-state-events")]
use ruma::events::AnyStateEventContent;
use ruma::{
    DeviceId, OwnedDeviceId, OwnedRoomId, OwnedTransactionId, OwnedUserId, RoomId,
    SecondsSinceUnixEpoch, TransactionId, UserId,
    events::{
        AnyMessageLikeEventContent,
        room::{encryption::RoomEncryptionEventContent, history_visibility::HistoryVisibility},
    },
    serde::Raw,
};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, error, info};
use vodozemac::{Curve25519PublicKey, megolm::SessionConfig};
pub use vodozemac::{
    PickleError,
    megolm::{GroupSession, GroupSessionPickle, MegolmMessage, SessionKey},
    olm::IdentityKeys,
};

use super::SessionCreationError;
#[cfg(feature = "experimental-algorithms")]
use crate::types::events::room::encrypted::MegolmV2AesSha2Content;
use crate::{
    DeviceData,
    olm::account::shared_history_from_history_visibility,
    session_manager::CollectStrategy,
    store::caches::SequenceNumber,
    types::{
        EventEncryptionAlgorithm,
        events::{
            room::encrypted::{
                MegolmV1AesSha2Content, RoomEncryptedEventContent, RoomEventEncryptionScheme,
            },
            room_key::{MegolmV1AesSha2Content as MegolmV1AesSha2RoomKeyContent, RoomKeyContent},
            room_key_withheld::RoomKeyWithheldContent,
        },
        requests::ToDeviceRequest,
    },
};

const ONE_HOUR: Duration = Duration::from_secs(60 * 60);
const ONE_WEEK: Duration = Duration::from_secs(60 * 60 * 24 * 7);

const ROTATION_PERIOD: Duration = ONE_WEEK;
const ROTATION_MESSAGES: u64 = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
/// Information about whether a session was shared with a device.
pub(crate) enum ShareState {
    /// The session was not shared with the device.
    NotShared,
    /// The session was shared with the device with the given device ID, but
    /// with a different curve25519 key.
    SharedButChangedSenderKey,
    /// The session was shared with the device, at the given message index. The
    /// `olm_wedging_index` is the value of the `olm_wedging_index` from the
    /// [`DeviceData`] at the time that we last shared the session with the
    /// device, and indicates whether we need to re-share the session with the
    /// device.
    Shared { message_index: u32, olm_wedging_index: SequenceNumber },
}

/// Settings for an encrypted room.
///
/// This determines the algorithm and rotation periods of a group session.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EncryptionSettings {
    /// The encryption algorithm that should be used in the room.
    pub algorithm: EventEncryptionAlgorithm,
    /// Whether state event encryption is enabled.
    #[cfg(feature = "experimental-encrypted-state-events")]
    #[serde(default)]
    pub encrypt_state_events: bool,
    /// How long the session should be used before changing it.
    pub rotation_period: Duration,
    /// How many messages should be sent before changing the session.
    pub rotation_period_msgs: u64,
    /// The history visibility of the room when the session was created.
    pub history_visibility: HistoryVisibility,
    /// The strategy used to distribute the room keys to participant.
    /// Default will send to all devices.
    #[serde(default)]
    pub sharing_strategy: CollectStrategy,
}

impl Default for EncryptionSettings {
    fn default() -> Self {
        Self {
            algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
            #[cfg(feature = "experimental-encrypted-state-events")]
            encrypt_state_events: false,
            rotation_period: ROTATION_PERIOD,
            rotation_period_msgs: ROTATION_MESSAGES,
            history_visibility: HistoryVisibility::Shared,
            sharing_strategy: CollectStrategy::default(),
        }
    }
}

impl EncryptionSettings {
    /// Create new encryption settings using an `RoomEncryptionEventContent`,
    /// a history visibility, and key sharing strategy.
    pub fn new(
        content: RoomEncryptionEventContent,
        history_visibility: HistoryVisibility,
        sharing_strategy: CollectStrategy,
    ) -> Self {
        let rotation_period: Duration =
            content.rotation_period_ms.map_or(ROTATION_PERIOD, |r| Duration::from_millis(r.into()));
        let rotation_period_msgs: u64 =
            content.rotation_period_msgs.map_or(ROTATION_MESSAGES, Into::into);

        Self {
            algorithm: EventEncryptionAlgorithm::from(content.algorithm.as_str()),
            #[cfg(feature = "experimental-encrypted-state-events")]
            encrypt_state_events: false,
            rotation_period,
            rotation_period_msgs,
            history_visibility,
            sharing_strategy,
        }
    }
}

/// The result of encrypting a message with an outbound group session.
///
/// Contains the encrypted content, the algorithm used, and the session ID.
#[derive(Debug)]
pub struct OutboundGroupSessionEncryptionResult {
    /// The encrypted content of the message.
    pub content: Raw<RoomEncryptedEventContent>,
    /// The algorithm used to encrypt the message.
    pub algorithm: EventEncryptionAlgorithm,
    /// The session ID used to encrypt the message.
    pub session_id: Arc<str>,
}

/// Outbound group session.
///
/// Outbound group sessions are used to exchange room messages between a group
/// of participants. Outbound group sessions are used to encrypt the room
/// messages.
#[derive(Clone)]
pub struct OutboundGroupSession {
    inner: Arc<RwLock<GroupSession>>,
    device_id: OwnedDeviceId,
    account_identity_keys: Arc<IdentityKeys>,
    session_id: Arc<str>,
    room_id: OwnedRoomId,
    pub(crate) creation_time: SecondsSinceUnixEpoch,
    message_count: Arc<AtomicU64>,
    shared: Arc<AtomicBool>,
    invalidated: Arc<AtomicBool>,
    settings: Arc<EncryptionSettings>,
    shared_with_set: Arc<StdRwLock<ShareInfoSet>>,
    to_share_with_set: Arc<StdRwLock<ToShareMap>>,
}

/// A a map of userid/device it to a `ShareInfo`.
///
/// Holds the `ShareInfo` for all the user/device pairs that will receive the
/// room key.
pub type ShareInfoSet = BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceId, ShareInfo>>;

type ToShareMap = BTreeMap<OwnedTransactionId, (Arc<ToDeviceRequest>, ShareInfoSet)>;

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
    pub fn new_shared(
        sender_key: Curve25519PublicKey,
        message_index: u32,
        olm_wedging_index: SequenceNumber,
    ) -> Self {
        ShareInfo::Shared(SharedWith { sender_key, message_index, olm_wedging_index })
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
    /// The Olm wedging index of the device at the time the session was shared.
    #[serde(default)]
    pub olm_wedging_index: SequenceNumber,
}

/// A read-only view into the device sharing state of an
/// [`OutboundGroupSession`].
pub(crate) struct SharingView<'a> {
    shared_with_set: RwLockReadGuard<'a, ShareInfoSet>,
    to_share_with_set: RwLockReadGuard<'a, ToShareMap>,
}

impl SharingView<'_> {
    /// Has the session been shared with the given user/device pair (or if not,
    /// is there such a request pending).
    pub(crate) fn get_share_state(&self, device: &DeviceData) -> ShareState {
        self.iter_shares(Some(device.user_id()), Some(device.device_id()))
            .map(|(_, _, info)| match info {
                ShareInfo::Shared(info) => {
                    if device.curve25519_key() == Some(info.sender_key) {
                        ShareState::Shared {
                            message_index: info.message_index,
                            olm_wedging_index: info.olm_wedging_index,
                        }
                    } else {
                        ShareState::SharedButChangedSenderKey
                    }
                }
                ShareInfo::Withheld(_) => ShareState::NotShared,
            })
            // Return the most "definitive" ShareState found (in case there
            // are multiple entries for the same device).
            .max()
            .unwrap_or(ShareState::NotShared)
    }

    /// Has the session been withheld for the given user/device pair (or if not,
    /// is there such a request pending).
    pub(crate) fn is_withheld_to(&self, device: &DeviceData, code: &WithheldCode) -> bool {
        self.iter_shares(Some(device.user_id()), Some(device.device_id()))
            .any(|(_, _, info)| matches!(info, ShareInfo::Withheld(c) if c == code))
    }

    /// Enumerate all sent or pending sharing requests for the given device (or
    /// for all devices if not specified).  This can yield the same device
    /// multiple times.
    pub(crate) fn iter_shares<'b, 'c>(
        &self,
        user_id: Option<&'b UserId>,
        device_id: Option<&'c DeviceId>,
    ) -> impl Iterator<Item = (&UserId, &DeviceId, &ShareInfo)> + use<'_, 'b, 'c> {
        fn iter_share_info_set<'a, 'b, 'c>(
            set: &'a ShareInfoSet,
            user_ids: (Bound<&'b UserId>, Bound<&'b UserId>),
            device_ids: (Bound<&'c DeviceId>, Bound<&'c DeviceId>),
        ) -> impl Iterator<Item = (&'a UserId, &'a DeviceId, &'a ShareInfo)> + use<'a, 'b, 'c>
        {
            set.range::<UserId, _>(user_ids).flat_map(move |(uid, d)| {
                d.range::<DeviceId, _>(device_ids)
                    .map(|(id, info)| (uid.as_ref(), id.as_ref(), info))
            })
        }

        let user_ids = user_id
            .map(|u| (Bound::Included(u), Bound::Included(u)))
            .unwrap_or((Bound::Unbounded, Bound::Unbounded));
        let device_ids = device_id
            .map(|d| (Bound::Included(d), Bound::Included(d)))
            .unwrap_or((Bound::Unbounded, Bound::Unbounded));

        let already_shared = iter_share_info_set(&self.shared_with_set, user_ids, device_ids);
        let pending = self
            .to_share_with_set
            .values()
            .flat_map(move |(_, set)| iter_share_info_set(set, user_ids, device_ids));
        already_shared.chain(pending)
    }

    /// Enumerate all users that have received the session, or have pending
    /// requests to receive it.  This can yield the same user multiple times,
    /// so you may want to `collect()` the result into a `BTreeSet`.
    pub(crate) fn shared_with_users(&self) -> impl Iterator<Item = &UserId> {
        self.iter_shares(None, None).filter_map(|(u, _, info)| match info {
            ShareInfo::Shared(_) => Some(u),
            ShareInfo::Withheld(_) => None,
        })
    }
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
    ///   session.
    ///
    /// * `room_id` - The id of the room that the session is used in.
    ///
    /// * `settings` - Settings determining the algorithm and rotation period of
    ///   the outbound group session.
    pub fn new(
        device_id: OwnedDeviceId,
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
            shared_with_set: Default::default(),
            to_share_with_set: Default::default(),
        })
    }

    /// Add a to-device request that is sending the session key (or room key)
    /// belonging to this [`OutboundGroupSession`] to other members of the
    /// group.
    ///
    /// The request will get persisted with the session which allows seamless
    /// session reuse across application restarts.
    ///
    /// **Warning** this method is only exposed to be used in integration tests
    /// of crypto-store implementations. **Do not use this outside of tests**.
    pub fn add_request(
        &self,
        request_id: OwnedTransactionId,
        request: Arc<ToDeviceRequest>,
        share_infos: ShareInfoSet,
    ) {
        self.to_share_with_set.write().insert(request_id, (request, share_infos));
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

        let removed = self.to_share_with_set.write().remove(request_id);
        if let Some((to_device, request)) = removed {
            let recipients: BTreeMap<&UserId, BTreeSet<&DeviceId>> = request
                .iter()
                .map(|(u, d)| (u.as_ref(), d.keys().map(|d| d.as_ref()).collect()))
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

                self.shared_with_set.write().entry(user_id).or_default().extend(info);
            }

            if self.to_share_with_set.read().is_empty() {
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
                self.to_share_with_set.read().keys().map(|k| k.to_string()).collect();

            error!(
                all_request_ids = ?request_ids,
                ?request_id,
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

    /// Encrypt an arbitrary event for the given room.
    ///
    /// Beware that a room key needs to be shared before this method
    /// can be called using the `share_room_key()` method.
    ///
    /// # Arguments
    ///
    /// * `payload` - The plaintext content of the event that should be
    ///   serialized to JSON and encrypted.
    ///
    /// # Panics
    ///
    /// Panics if the content can't be serialized.
    async fn encrypt_inner<T: Serialize>(
        &self,
        payload: &T,
        relates_to: Option<serde_json::Value>,
    ) -> OutboundGroupSessionEncryptionResult {
        let ciphertext = self
            .encrypt_helper(
                serde_json::to_string(payload).expect("payload serialization never fails"),
            )
            .await;
        let scheme: RoomEventEncryptionScheme = match self.settings.algorithm {
            EventEncryptionAlgorithm::MegolmV1AesSha2 => MegolmV1AesSha2Content {
                ciphertext,
                sender_key: Some(self.account_identity_keys.curve25519),
                session_id: self.session_id().to_owned(),
                device_id: Some(self.device_id.clone()),
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

        OutboundGroupSessionEncryptionResult {
            content: Raw::new(&content)
                .expect("m.room.encrypted event content can always be serialized"),
            algorithm: self.settings.algorithm.to_owned(),
            session_id: self.session_id.clone(),
        }
    }

    /// Encrypt a room message for the given room.
    ///
    /// Beware that a room key needs to be shared before this method
    /// can be called using the `share_room_key()` method.
    ///
    /// # Arguments
    ///
    /// * `event_type` - The plaintext type of the event, the outer type of the
    ///   event will become `m.room.encrypted`.
    ///
    /// * `content` - The plaintext content of the message that should be
    ///   encrypted in raw JSON form.
    ///
    /// # Panics
    ///
    /// Panics if the content can't be serialized.
    pub async fn encrypt(
        &self,
        event_type: &str,
        content: &Raw<AnyMessageLikeEventContent>,
    ) -> OutboundGroupSessionEncryptionResult {
        #[derive(Serialize)]
        struct Payload<'a> {
            #[serde(rename = "type")]
            event_type: &'a str,
            content: &'a Raw<AnyMessageLikeEventContent>,
            room_id: &'a RoomId,
        }

        let payload = Payload { event_type, content, room_id: &self.room_id };

        let relates_to = content
            .get_field::<serde_json::Value>("m.relates_to")
            .expect("serde_json::Value deserialization with valid JSON input never fails");

        self.encrypt_inner(&payload, relates_to).await
    }

    /// Encrypt a room state event for the given room.
    ///
    /// Beware that a room key needs to be shared before this method
    /// can be called using the `share_room_key()` method.
    ///
    /// # Arguments
    ///
    /// * `event_type` - The plaintext type of the event, the outer type of the
    ///   event will become `m.room.encrypted`.
    ///
    /// * `state_key` - The plaintext state key of the event, the outer state
    ///   key will be derived from this and the event type.
    ///
    /// * `content` - The plaintext content of the message that should be
    ///   encrypted in raw JSON form.
    ///
    /// # Panics
    ///
    /// Panics if the content can't be serialized.
    #[cfg(feature = "experimental-encrypted-state-events")]
    pub async fn encrypt_state(
        &self,
        event_type: &str,
        state_key: &str,
        content: &Raw<AnyStateEventContent>,
    ) -> Raw<RoomEncryptedEventContent> {
        #[derive(Serialize)]
        struct Payload<'a> {
            #[serde(rename = "type")]
            event_type: &'a str,
            state_key: &'a str,
            content: &'a Raw<AnyStateEventContent>,
            room_id: &'a RoomId,
        }

        let payload = Payload { event_type, state_key, content, room_id: &self.room_id };
        self.encrypt_inner(&payload, None).await.content
    }

    fn elapsed(&self) -> bool {
        let creation_time = Duration::from_secs(self.creation_time.get().into());
        let now = Duration::from_secs(SecondsSinceUnixEpoch::now().get().into());
        now.checked_sub(creation_time)
            .map(|elapsed| elapsed >= self.safe_rotation_period())
            .unwrap_or(true)
    }

    /// Returns the rotation_period_ms that was set for this session, clamped
    /// to be no less than one hour.
    ///
    /// This is to prevent a malicious or careless user causing sessions to be
    /// rotated very frequently.
    ///
    /// The feature flag `_disable-minimum-rotation-period-ms` can
    /// be used to prevent this behaviour (which can be useful for tests).
    fn safe_rotation_period(&self) -> Duration {
        if cfg!(feature = "_disable-minimum-rotation-period-ms") {
            self.settings.rotation_period
        } else {
            max(self.settings.rotation_period, ONE_HOUR)
        }
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
        let shared_history =
            shared_history_from_history_visibility(&self.settings.history_visibility);

        RoomKeyContent::MegolmV1AesSha2(
            MegolmV1AesSha2RoomKeyContent::new(
                self.room_id().to_owned(),
                self.session_id().to_owned(),
                session_key,
                shared_history,
            )
            .into(),
        )
    }

    /// Create a read-only view into the device sharing state of this session.
    /// This view includes pending requests, so it is not guaranteed that the
    /// represented state has been fully propagated yet.
    pub(crate) fn sharing_view(&self) -> SharingView<'_> {
        SharingView {
            shared_with_set: self.shared_with_set.read(),
            to_share_with_set: self.to_share_with_set.read(),
        }
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
        self.shared_with_set.write().entry(user_id.to_owned()).or_default().insert(
            device_id.to_owned(),
            ShareInfo::new_shared(sender_key, index, Default::default()),
        );
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
        let share_info =
            ShareInfo::new_shared(sender_key, self.message_index().await, Default::default());
        self.shared_with_set
            .write()
            .entry(user_id.to_owned())
            .or_default()
            .insert(device_id.to_owned(), share_info);
    }

    /// Get the list of requests that need to be sent out for this session to be
    /// marked as shared.
    pub(crate) fn pending_requests(&self) -> Vec<Arc<ToDeviceRequest>> {
        self.to_share_with_set.read().values().map(|(req, _)| req.clone()).collect()
    }

    /// Get the list of request ids this session is waiting for to be sent out.
    pub(crate) fn pending_request_ids(&self) -> Vec<OwnedTransactionId> {
        self.to_share_with_set.read().keys().cloned().collect()
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
    ///   an unencrypted mode or an encrypted using passphrase.
    pub fn from_pickle(
        device_id: OwnedDeviceId,
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
            shared_with_set: Arc::new(StdRwLock::new(pickle.shared_with_set)),
            to_share_with_set: Arc::new(StdRwLock::new(pickle.requests)),
        })
    }

    /// Store the group session as a base64 encoded string and associated data
    /// belonging to the session.
    ///
    /// # Arguments
    ///
    /// * `pickle_mode` - The mode that should be used to pickle the group
    ///   session, either an unencrypted mode or an encrypted using passphrase.
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
            shared_with_set: self.shared_with_set.read().clone(),
            requests: self.to_share_with_set.read().clone(),
        }
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for OutboundGroupSession {
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
    pub room_id: OwnedRoomId,
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

    use ruma::{
        EventEncryptionAlgorithm,
        events::room::{
            encryption::RoomEncryptionEventContent, history_visibility::HistoryVisibility,
        },
        uint,
    };

    use super::{EncryptionSettings, ROTATION_MESSAGES, ROTATION_PERIOD, ShareState};
    use crate::CollectStrategy;

    #[test]
    fn test_encryption_settings_conversion() {
        let mut content =
            RoomEncryptionEventContent::new(EventEncryptionAlgorithm::MegolmV1AesSha2);
        let settings = EncryptionSettings::new(
            content.clone(),
            HistoryVisibility::Joined,
            CollectStrategy::AllDevices,
        );

        assert_eq!(settings.rotation_period, ROTATION_PERIOD);
        assert_eq!(settings.rotation_period_msgs, ROTATION_MESSAGES);

        content.rotation_period_ms = Some(uint!(3600));
        content.rotation_period_msgs = Some(uint!(500));

        let settings = EncryptionSettings::new(
            content,
            HistoryVisibility::Shared,
            CollectStrategy::AllDevices,
        );

        assert_eq!(settings.rotation_period, Duration::from_millis(3600));
        assert_eq!(settings.rotation_period_msgs, 500);
    }

    /// Ensure that the `ShareState` PartialOrd instance orders according to
    /// specificity of the value.
    #[test]
    fn test_share_state_ordering() {
        let values = [
            ShareState::NotShared,
            ShareState::SharedButChangedSenderKey,
            ShareState::Shared { message_index: 1, olm_wedging_index: Default::default() },
        ];
        // Make sure our test case of possible variants is exhaustive
        match values[0] {
            ShareState::NotShared
            | ShareState::SharedButChangedSenderKey
            | ShareState::Shared { .. } => {}
        }
        assert!(values.is_sorted());
    }

    #[cfg(any(target_os = "linux", target_os = "macos", target_family = "wasm"))]
    mod expiration {
        use std::{sync::atomic::Ordering, time::Duration};

        use matrix_sdk_test::async_test;
        use ruma::{
            SecondsSinceUnixEpoch, device_id, events::room::message::RoomMessageEventContent,
            room_id, serde::Raw, uint, user_id,
        };

        use crate::{
            Account, EncryptionSettings, MegolmError,
            olm::{OutboundGroupSession, SenderData},
        };

        const TWO_HOURS: Duration = Duration::from_secs(60 * 60 * 2);

        #[async_test]
        async fn test_session_is_not_expired_if_no_messages_sent_and_no_time_passed() {
            // Given a session that expires after one message
            let session = create_session(EncryptionSettings {
                rotation_period_msgs: 1,
                ..Default::default()
            })
            .await;

            // When we send no messages at all

            // Then it is not expired
            assert!(!session.expired());
        }

        #[async_test]
        async fn test_session_is_expired_if_we_rotate_every_message_and_one_was_sent()
        -> Result<(), MegolmError> {
            // Given a session that expires after one message
            let session = create_session(EncryptionSettings {
                rotation_period_msgs: 1,
                ..Default::default()
            })
            .await;

            // When we send a message
            let _ = session
                .encrypt(
                    "m.room.message",
                    &Raw::new(&RoomMessageEventContent::text_plain("Test message"))?.cast(),
                )
                .await;

            // Then the session is expired
            assert!(session.expired());

            Ok(())
        }

        #[async_test]
        async fn test_session_with_rotation_period_is_not_expired_after_no_time() {
            // Given a session with a 2h expiration
            let session = create_session(EncryptionSettings {
                rotation_period: TWO_HOURS,
                ..Default::default()
            })
            .await;

            // When we don't allow any time to pass

            // Then it is not expired
            assert!(!session.expired());
        }

        #[async_test]
        async fn test_session_is_expired_after_rotation_period() {
            // Given a session with a 2h expiration
            let mut session = create_session(EncryptionSettings {
                rotation_period: TWO_HOURS,
                ..Default::default()
            })
            .await;

            // When 3 hours have passed
            let now = SecondsSinceUnixEpoch::now();
            session.creation_time = SecondsSinceUnixEpoch(now.get() - uint!(10800));

            // Then the session is expired
            assert!(session.expired());
        }

        #[async_test]
        #[cfg(not(feature = "_disable-minimum-rotation-period-ms"))]
        async fn test_session_does_not_expire_under_one_hour_even_if_we_ask_for_shorter() {
            // Given a session with a 100ms expiration
            let mut session = create_session(EncryptionSettings {
                rotation_period: Duration::from_millis(100),
                ..Default::default()
            })
            .await;

            // When less than an hour has passed
            let now = SecondsSinceUnixEpoch::now();
            session.creation_time = SecondsSinceUnixEpoch(now.get() - uint!(1800));

            // Then the session is not expired: we enforce a minimum of 1 hour
            assert!(!session.expired());

            // But when more than an hour has passed
            session.creation_time = SecondsSinceUnixEpoch(now.get() - uint!(3601));

            // Then the session is expired
            assert!(session.expired());
        }

        #[async_test]
        #[cfg(feature = "_disable-minimum-rotation-period-ms")]
        async fn test_with_disable_minrotperiod_feature_sessions_can_expire_quickly() {
            // Given a session with a 100ms expiration
            let mut session = create_session(EncryptionSettings {
                rotation_period: Duration::from_millis(100),
                ..Default::default()
            })
            .await;

            // When less than an hour has passed
            let now = SecondsSinceUnixEpoch::now();
            session.creation_time = SecondsSinceUnixEpoch(now.get() - uint!(1800));

            // Then the session is expired: the feature flag has prevented us enforcing a
            // minimum
            assert!(session.expired());
        }

        #[async_test]
        async fn test_session_with_zero_msgs_rotation_is_not_expired_initially() {
            // Given a session that is supposed to expire after zero messages
            let session = create_session(EncryptionSettings {
                rotation_period_msgs: 0,
                ..Default::default()
            })
            .await;

            // When we send no messages

            // Then the session is not expired: we are protected against this nonsensical
            // setup
            assert!(!session.expired());
        }

        #[async_test]
        async fn test_session_with_zero_msgs_rotation_expires_after_one_message()
        -> Result<(), MegolmError> {
            // Given a session that is supposed to expire after zero messages
            let session = create_session(EncryptionSettings {
                rotation_period_msgs: 0,
                ..Default::default()
            })
            .await;

            // When we send a message
            let _ = session
                .encrypt(
                    "m.room.message",
                    &Raw::new(&RoomMessageEventContent::text_plain("Test message"))?.cast(),
                )
                .await;

            // Then the session is expired: we treated rotation_period_msgs=0 as if it were
            // =1
            assert!(session.expired());

            Ok(())
        }

        #[async_test]
        async fn test_session_expires_after_10k_messages_even_if_we_ask_for_more() {
            // Given we asked to expire after 100K messages
            let session = create_session(EncryptionSettings {
                rotation_period_msgs: 100_000,
                ..Default::default()
            })
            .await;

            // Sanity: it does not expire after <10K messages
            assert!(!session.expired());
            session.message_count.store(1000, Ordering::SeqCst);
            assert!(!session.expired());
            session.message_count.store(9999, Ordering::SeqCst);
            assert!(!session.expired());

            // When we have sent >= 10K messages
            session.message_count.store(10_000, Ordering::SeqCst);

            // Then it is considered expired: we enforce a maximum of 10K messages before
            // rotation.
            assert!(session.expired());
        }

        async fn create_session(settings: EncryptionSettings) -> OutboundGroupSession {
            let account =
                Account::with_device_id(user_id!("@alice:example.org"), device_id!("DEVICEID"))
                    .static_data;
            let (session, _) = account
                .create_group_session_pair(
                    room_id!("!test_room:example.org"),
                    settings,
                    SenderData::unknown(),
                )
                .await
                .unwrap();
            session
        }
    }
}
