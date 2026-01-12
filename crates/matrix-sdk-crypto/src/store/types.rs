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

//! Data types for persistent storage.
//!
//! This module defines the data structures used by the crypto store to
//! represent objects that are persisted in the database.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::Duration,
};

use ruma::{OwnedDeviceId, OwnedRoomId, OwnedUserId};
use serde::{Deserialize, Serialize};
use vodozemac::{Curve25519PublicKey, base64_encode};
use zeroize::{Zeroize, ZeroizeOnDrop};

use super::{DehydrationError, GossipRequest};
use crate::{
    Account, Device, DeviceData, GossippedSecret, Session, UserIdentity, UserIdentityData,
    olm::{
        InboundGroupSession, OlmMessageHash, OutboundGroupSession, PrivateCrossSigningIdentity,
        SenderData,
    },
    types::{
        EventEncryptionAlgorithm,
        events::{
            room_key_bundle::RoomKeyBundleContent,
            room_key_withheld::{RoomKeyWithheldContent, RoomKeyWithheldEvent},
        },
    },
};

/// Aggregated changes to be saved in the database.
///
/// This is an update version of `Changes` that will replace it as #2624
/// progresses.
// If you ever add a field here, make sure to update `Changes::is_empty` too.
#[derive(Default, Debug)]
#[allow(missing_docs)]
pub struct PendingChanges {
    pub account: Option<Account>,
}

impl PendingChanges {
    /// Are there any changes stored or is this an empty `Changes` struct?
    pub fn is_empty(&self) -> bool {
        self.account.is_none()
    }
}

/// Aggregated changes to be saved in the database.
// If you ever add a field here, make sure to update `Changes::is_empty` too.
#[derive(Default, Debug)]
#[allow(missing_docs)]
pub struct Changes {
    pub private_identity: Option<PrivateCrossSigningIdentity>,
    pub backup_version: Option<String>,
    pub backup_decryption_key: Option<BackupDecryptionKey>,
    pub dehydrated_device_pickle_key: Option<DehydratedDeviceKey>,
    pub sessions: Vec<Session>,
    pub message_hashes: Vec<OlmMessageHash>,
    pub inbound_group_sessions: Vec<InboundGroupSession>,
    pub outbound_group_sessions: Vec<OutboundGroupSession>,
    pub key_requests: Vec<GossipRequest>,
    pub identities: IdentityChanges,
    pub devices: DeviceChanges,
    /// Stores when a `m.room_key.withheld` is received
    pub withheld_session_info: BTreeMap<OwnedRoomId, BTreeMap<String, RoomKeyWithheldEntry>>,
    pub room_settings: HashMap<OwnedRoomId, RoomSettings>,
    pub secrets: Vec<GossippedSecret>,
    pub next_batch_token: Option<String>,

    /// Historical room key history bundles that we have received and should
    /// store.
    pub received_room_key_bundles: Vec<StoredRoomKeyBundleData>,

    /// The set of rooms for which we have requested all room keys from the
    /// backup in advance of constructing a room key bundle.
    pub room_key_backups_fully_downloaded: HashSet<OwnedRoomId>,
}

/// Information about an [MSC4268] room key bundle.
///
/// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredRoomKeyBundleData {
    /// The user that sent us this data.
    pub sender_user: OwnedUserId,

    /// The [`Curve25519PublicKey`] of the device that sent us this data.
    pub sender_key: Curve25519PublicKey,

    /// Information about the sender of this data and how much we trust that
    /// information.
    pub sender_data: SenderData,

    /// The room key bundle data itself.
    pub bundle_data: RoomKeyBundleContent,
}

/// A user for which we are tracking the list of devices.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackedUser {
    /// The user ID of the user.
    pub user_id: OwnedUserId,
    /// The outdate/dirty flag of the user, remembers if the list of devices for
    /// the user is considered to be out of date. If the list of devices is
    /// out of date, a `/keys/query` request should be sent out for this
    /// user.
    pub dirty: bool,
}

impl Changes {
    /// Are there any changes stored or is this an empty `Changes` struct?
    pub fn is_empty(&self) -> bool {
        self.private_identity.is_none()
            && self.backup_version.is_none()
            && self.backup_decryption_key.is_none()
            && self.dehydrated_device_pickle_key.is_none()
            && self.sessions.is_empty()
            && self.message_hashes.is_empty()
            && self.inbound_group_sessions.is_empty()
            && self.outbound_group_sessions.is_empty()
            && self.key_requests.is_empty()
            && self.identities.is_empty()
            && self.devices.is_empty()
            && self.withheld_session_info.is_empty()
            && self.room_settings.is_empty()
            && self.secrets.is_empty()
            && self.next_batch_token.is_none()
            && self.received_room_key_bundles.is_empty()
    }
}

/// This struct is used to remember whether an identity has undergone a change
/// or remains the same as the one we already know about.
///
/// When the homeserver informs us of a potential change in a user's identity or
/// device during a `/sync` response, it triggers a `/keys/query` request from
/// our side. In response to this query, the server provides a comprehensive
/// snapshot of all the user's devices and identities.
///
/// Our responsibility is to discern whether a device or identity is new,
/// changed, or unchanged.
#[derive(Debug, Clone, Default)]
#[allow(missing_docs)]
pub struct IdentityChanges {
    pub new: Vec<UserIdentityData>,
    pub changed: Vec<UserIdentityData>,
    pub unchanged: Vec<UserIdentityData>,
}

impl IdentityChanges {
    pub(super) fn is_empty(&self) -> bool {
        self.new.is_empty() && self.changed.is_empty()
    }

    /// Convert the vectors contained in the [`IdentityChanges`] into
    /// three maps from user id to user identity (new, updated, unchanged).
    pub(super) fn into_maps(
        self,
    ) -> (
        BTreeMap<OwnedUserId, UserIdentityData>,
        BTreeMap<OwnedUserId, UserIdentityData>,
        BTreeMap<OwnedUserId, UserIdentityData>,
    ) {
        let new: BTreeMap<_, _> = self
            .new
            .into_iter()
            .map(|identity| (identity.user_id().to_owned(), identity))
            .collect();

        let changed: BTreeMap<_, _> = self
            .changed
            .into_iter()
            .map(|identity| (identity.user_id().to_owned(), identity))
            .collect();

        let unchanged: BTreeMap<_, _> = self
            .unchanged
            .into_iter()
            .map(|identity| (identity.user_id().to_owned(), identity))
            .collect();

        (new, changed, unchanged)
    }
}

#[derive(Debug, Clone, Default)]
#[allow(missing_docs)]
pub struct DeviceChanges {
    pub new: Vec<DeviceData>,
    pub changed: Vec<DeviceData>,
    pub deleted: Vec<DeviceData>,
}

/// Updates about [`Device`]s which got received over the `/keys/query`
/// endpoint.
#[derive(Clone, Debug, Default)]
pub struct DeviceUpdates {
    /// The list of newly discovered devices.
    ///
    /// A device being in this list does not necessarily mean that the device
    /// was just created, it just means that it's the first time we're
    /// seeing this device.
    pub new: BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceId, Device>>,
    /// The list of changed devices.
    pub changed: BTreeMap<OwnedUserId, BTreeMap<OwnedDeviceId, Device>>,
}

/// Updates about [`UserIdentity`]s which got received over the `/keys/query`
/// endpoint.
#[derive(Clone, Debug, Default)]
pub struct IdentityUpdates {
    /// The list of newly discovered user identities .
    ///
    /// A identity being in this list does not necessarily mean that the
    /// identity was just created, it just means that it's the first time
    /// we're seeing this identity.
    pub new: BTreeMap<OwnedUserId, UserIdentity>,
    /// The list of changed identities.
    pub changed: BTreeMap<OwnedUserId, UserIdentity>,
    /// The list of unchanged identities.
    pub unchanged: BTreeMap<OwnedUserId, UserIdentity>,
}

/// The private part of a backup key.
///
/// The private part of the key is not used on a regular basis. Rather, it is
/// used only when we need to *recover* the backup.
///
/// Typically, this private key is itself encrypted and stored in server-side
/// secret storage (SSSS), whence it can be retrieved when it is needed for a
/// recovery operation. Alternatively, the key can be "gossiped" between devices
/// via "secret sharing".
#[derive(Clone, Zeroize, ZeroizeOnDrop, Deserialize, Serialize)]
#[serde(transparent)]
pub struct BackupDecryptionKey {
    pub(crate) inner: Box<[u8; BackupDecryptionKey::KEY_SIZE]>,
}

impl BackupDecryptionKey {
    /// The number of bytes the decryption key will hold.
    pub const KEY_SIZE: usize = 32;

    /// Create a new random decryption key.
    pub fn new() -> Result<Self, rand::Error> {
        let mut rng = rand::thread_rng();

        let mut key = Box::new([0u8; Self::KEY_SIZE]);
        rand::Fill::try_fill(key.as_mut_slice(), &mut rng)?;

        Ok(Self { inner: key })
    }

    /// Export the [`BackupDecryptionKey`] as a base64 encoded string.
    pub fn to_base64(&self) -> String {
        base64_encode(self.inner.as_slice())
    }
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for BackupDecryptionKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("BackupDecryptionKey").field(&"...").finish()
    }
}

/// The pickle key used to safely store the dehydrated device pickle.
///
/// This input key material will be expanded using HKDF into an AES key, MAC
/// key, and an initialization vector (IV).
#[derive(Clone, Zeroize, ZeroizeOnDrop, Deserialize, Serialize)]
#[serde(transparent)]
pub struct DehydratedDeviceKey {
    pub(crate) inner: Box<[u8; DehydratedDeviceKey::KEY_SIZE]>,
}

impl DehydratedDeviceKey {
    /// The number of bytes the encryption key will hold.
    pub const KEY_SIZE: usize = 32;

    /// Generates a new random pickle key.
    pub fn new() -> Result<Self, rand::Error> {
        let mut rng = rand::thread_rng();

        let mut key = Box::new([0u8; Self::KEY_SIZE]);
        rand::Fill::try_fill(key.as_mut_slice(), &mut rng)?;

        Ok(Self { inner: key })
    }

    /// Creates a new dehydration pickle key from the given slice.
    ///
    /// Fail if the slice length is not 32.
    pub fn from_slice(slice: &[u8]) -> Result<Self, DehydrationError> {
        if slice.len() == 32 {
            let mut key = Box::new([0u8; 32]);
            key.copy_from_slice(slice);
            Ok(DehydratedDeviceKey { inner: key })
        } else {
            Err(DehydrationError::PickleKeyLength(slice.len()))
        }
    }

    /// Creates a dehydration pickle key from the given bytes.
    pub fn from_bytes(raw_key: &[u8; 32]) -> Self {
        let mut inner = Box::new([0u8; Self::KEY_SIZE]);
        inner.copy_from_slice(raw_key);

        Self { inner }
    }

    /// Export the [`DehydratedDeviceKey`] as a base64 encoded string.
    pub fn to_base64(&self) -> String {
        base64_encode(self.inner.as_slice())
    }
}

impl From<&[u8; 32]> for DehydratedDeviceKey {
    fn from(value: &[u8; 32]) -> Self {
        DehydratedDeviceKey { inner: Box::new(*value) }
    }
}

impl From<DehydratedDeviceKey> for Vec<u8> {
    fn from(key: DehydratedDeviceKey) -> Self {
        key.inner.to_vec()
    }
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for DehydratedDeviceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("DehydratedDeviceKey").field(&"...").finish()
    }
}

impl DeviceChanges {
    /// Merge the given `DeviceChanges` into this instance of `DeviceChanges`.
    pub fn extend(&mut self, other: DeviceChanges) {
        self.new.extend(other.new);
        self.changed.extend(other.changed);
        self.deleted.extend(other.deleted);
    }

    /// Are there any changes is this an empty [`DeviceChanges`] struct?
    pub fn is_empty(&self) -> bool {
        self.new.is_empty() && self.changed.is_empty() && self.deleted.is_empty()
    }
}

/// Struct holding info about how many room keys the store has.
#[derive(Debug, Clone, Default)]
pub struct RoomKeyCounts {
    /// The total number of room keys the store has.
    pub total: usize,
    /// The number of backed up room keys the store has.
    pub backed_up: usize,
}

/// Stored versions of the backup keys.
#[derive(Default, Clone, Debug)]
pub struct BackupKeys {
    /// The key used to decrypt backed up room keys.
    pub decryption_key: Option<BackupDecryptionKey>,
    /// The version that we are using for backups.
    pub backup_version: Option<String>,
}

/// A struct containing private cross signing keys that can be backed up or
/// uploaded to the secret store.
#[derive(Default, Zeroize, ZeroizeOnDrop)]
pub struct CrossSigningKeyExport {
    /// The seed of the master key encoded as unpadded base64.
    pub master_key: Option<String>,
    /// The seed of the self signing key encoded as unpadded base64.
    pub self_signing_key: Option<String>,
    /// The seed of the user signing key encoded as unpadded base64.
    pub user_signing_key: Option<String>,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for CrossSigningKeyExport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CrossSigningKeyExport")
            .field("master_key", &self.master_key.is_some())
            .field("self_signing_key", &self.self_signing_key.is_some())
            .field("user_signing_key", &self.user_signing_key.is_some())
            .finish_non_exhaustive()
    }
}

/// Result type telling us if a `/keys/query` response was expected for a given
/// user.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UserKeyQueryResult {
    WasPending,
    WasNotPending,

    /// A query was pending, but we gave up waiting
    TimeoutExpired,
}

/// Room encryption settings which are modified by state events or user options
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RoomSettings {
    /// The encryption algorithm that should be used in the room.
    pub algorithm: EventEncryptionAlgorithm,

    /// Whether state event encryption is enabled.
    #[cfg(feature = "experimental-encrypted-state-events")]
    #[serde(default)]
    pub encrypt_state_events: bool,

    /// Should untrusted devices receive the room key, or should they be
    /// excluded from the conversation.
    pub only_allow_trusted_devices: bool,

    /// The maximum time an encryption session should be used for, before it is
    /// rotated.
    pub session_rotation_period: Option<Duration>,

    /// The maximum number of messages an encryption session should be used for,
    /// before it is rotated.
    pub session_rotation_period_messages: Option<usize>,
}

impl Default for RoomSettings {
    fn default() -> Self {
        Self {
            algorithm: EventEncryptionAlgorithm::MegolmV1AesSha2,
            #[cfg(feature = "experimental-encrypted-state-events")]
            encrypt_state_events: false,
            only_allow_trusted_devices: false,
            session_rotation_period: None,
            session_rotation_period_messages: None,
        }
    }
}

/// Information on a room key that has been received or imported.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RoomKeyInfo {
    /// The [messaging algorithm] that this key is used for. Will be one of the
    /// `m.megolm.*` algorithms.
    ///
    /// [messaging algorithm]: https://spec.matrix.org/v1.6/client-server-api/#messaging-algorithms
    pub algorithm: EventEncryptionAlgorithm,

    /// The room where the key is used.
    pub room_id: OwnedRoomId,

    /// The Curve25519 key of the device which initiated the session originally.
    pub sender_key: Curve25519PublicKey,

    /// The ID of the session that the key is for.
    pub session_id: String,
}

impl From<&InboundGroupSession> for RoomKeyInfo {
    fn from(group_session: &InboundGroupSession) -> Self {
        RoomKeyInfo {
            algorithm: group_session.algorithm().clone(),
            room_id: group_session.room_id().to_owned(),
            sender_key: group_session.sender_key(),
            session_id: group_session.session_id().to_owned(),
        }
    }
}

/// Information on a room key that has been withheld
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RoomKeyWithheldInfo {
    /// The room where the key is used.
    pub room_id: OwnedRoomId,

    /// The ID of the session that the key is for.
    pub session_id: String,

    /// The withheld entry from a `m.room_key.withheld` event or [MSC4268] room
    /// key bundle.
    ///
    /// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
    pub withheld_event: RoomKeyWithheldEntry,
}

/// Represents an entry for a withheld room key event, which can be either a
/// to-device event or a bundle entry.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RoomKeyWithheldEntry {
    /// The user ID responsible for this entry, either from a
    /// `m.room_key.withheld` to-device event or an [MSC4268] room key bundle.
    ///
    /// [MSC4268]: https://github.com/matrix-org/matrix-spec-proposals/pull/4268
    pub sender: OwnedUserId,
    /// The content of the entry, which provides details about the reason the
    /// key was withheld.
    pub content: RoomKeyWithheldContent,
}

impl From<RoomKeyWithheldEvent> for RoomKeyWithheldEntry {
    fn from(value: RoomKeyWithheldEvent) -> Self {
        Self { sender: value.sender, content: value.content }
    }
}

/// Information about a received historic room key bundle.
///
/// This struct contains information needed to uniquely identify a room key
/// bundle. Only a single bundle per sender for a given room is persisted at a
/// time.
///
/// It is used to notify listeners about received room key bundles.
#[derive(Debug, Clone)]
pub struct RoomKeyBundleInfo {
    /// The user ID of the person that sent us the historic room key bundle.
    pub sender: OwnedUserId,

    /// The [`Curve25519PublicKey`] of the device that sent us this data.
    pub sender_key: Curve25519PublicKey,

    /// The ID of the room the bundle should be used in.
    pub room_id: OwnedRoomId,
}

impl From<&StoredRoomKeyBundleData> for RoomKeyBundleInfo {
    fn from(value: &StoredRoomKeyBundleData) -> Self {
        let StoredRoomKeyBundleData { sender_user, sender_data: _, bundle_data, sender_key } =
            value;
        let sender_key = *sender_key;

        Self { sender: sender_user.clone(), room_id: bundle_data.room_id.clone(), sender_key }
    }
}
