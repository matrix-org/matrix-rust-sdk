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
    collections::{BTreeMap, HashMap},
    fmt,
    ops::{Deref, Not as _},
    sync::Arc,
    time::Duration,
};

use hkdf::Hkdf;
use js_option::JsOption;
use matrix_sdk_common::deserialized_responses::{
    AlgorithmInfo, DeviceLinkProblem, EncryptionInfo, VerificationLevel, VerificationState,
};
#[cfg(test)]
use ruma::api::client::dehydrated_device::DehydratedDeviceV1;
use ruma::{
    CanonicalJsonValue, DeviceId, DeviceKeyAlgorithm, DeviceKeyId, MilliSecondsSinceUnixEpoch,
    OneTimeKeyAlgorithm, OneTimeKeyId, OwnedDeviceId, OwnedDeviceKeyId, OwnedOneTimeKeyId,
    OwnedUserId, RoomId, SecondsSinceUnixEpoch, UInt, UserId,
    api::client::{
        dehydrated_device::{DehydratedDeviceData, DehydratedDeviceV2},
        keys::{
            upload_keys,
            upload_signatures::v3::{Request as SignatureUploadRequest, SignedKeys},
        },
    },
    canonical_json::to_canonical_value,
    events::{AnyToDeviceEvent, room::history_visibility::HistoryVisibility},
    serde::Raw,
};
use serde::{Deserialize, Serialize, de::Error};
use serde_json::value::{RawValue as RawJsonValue, to_raw_value};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::{Span, debug, field::debug, info, instrument, trace, warn};
use vodozemac::{
    Curve25519PublicKey, Ed25519Signature, KeyId, PickleError, base64_encode,
    olm::{
        Account as InnerAccount, AccountPickle, IdentityKeys, OlmMessage,
        OneTimeKeyGenerationResult, PreKeyMessage, SessionConfig,
    },
};

use super::{
    EncryptionSettings, InboundGroupSession, OutboundGroupSession, PrivateCrossSigningIdentity,
    Session, SessionCreationError as MegolmSessionCreationError, utility::SignJson,
};
#[cfg(feature = "experimental-algorithms")]
use crate::types::events::room::encrypted::OlmV2Curve25519AesSha2Content;
use crate::{
    DecryptionSettings, Device, OlmError, SignatureError, TrustRequirement,
    dehydrated_devices::DehydrationError,
    error::{EventError, OlmResult, SessionCreationError},
    identities::DeviceData,
    olm::SenderData,
    store::{
        Store,
        types::{Changes, DeviceChanges},
    },
    types::{
        CrossSigningKey, DeviceKeys, EventEncryptionAlgorithm, MasterPubkey, OneTimeKey, SignedKey,
        events::{
            olm_v1::AnyDecryptedOlmEvent,
            room::encrypted::{
                EncryptedToDeviceEvent, OlmV1Curve25519AesSha2Content,
                ToDeviceEncryptedEventContent,
            },
        },
        requests::UploadSigningKeysRequest,
    },
};

#[derive(Debug)]
enum PrekeyBundle {
    Olm3DH { key: SignedKey },
}

#[derive(Debug, Clone)]
pub(crate) enum SessionType {
    New(Session),
    Existing(Session),
}

#[derive(Debug)]
pub struct InboundCreationResult {
    pub session: Session,
    pub plaintext: String,
}

impl SessionType {
    #[cfg(test)]
    pub fn session(self) -> Session {
        match self {
            SessionType::New(s) => s,
            SessionType::Existing(s) => s,
        }
    }
}

/// A struct witnessing a successful decryption of an Olm-encrypted to-device
/// event.
///
/// Contains the decrypted event plaintext along with some associated metadata,
/// such as the identity (Curve25519) key of the to-device event sender.
#[derive(Debug)]
pub(crate) struct OlmDecryptionInfo {
    pub session: SessionType,
    pub message_hash: OlmMessageHash,
    pub inbound_group_session: Option<InboundGroupSession>,
    pub result: DecryptionResult,
}

#[derive(Debug)]
pub(crate) struct DecryptionResult {
    // AnyDecryptedOlmEvent is pretty big at 512 bytes, box it to reduce stack size
    pub event: Box<AnyDecryptedOlmEvent>,
    pub raw_event: Raw<AnyToDeviceEvent>,
    pub sender_key: Curve25519PublicKey,
    pub encryption_info: EncryptionInfo,
}

/// A hash of a successfully decrypted Olm message.
///
/// Can be used to check if a message has been replayed to us.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OlmMessageHash {
    /// The curve25519 key of the sender that sent us the Olm message.
    pub sender_key: String,
    /// The hash of the message.
    pub hash: String,
}

impl OlmMessageHash {
    fn new(sender_key: Curve25519PublicKey, ciphertext: &OlmMessage) -> Self {
        let (message_type, ciphertext) = ciphertext.clone().to_parts();
        let sender_key = sender_key.to_base64();

        let sha = Sha256::new()
            .chain_update(sender_key.as_bytes())
            .chain_update([message_type as u8])
            .chain_update(ciphertext)
            .finalize();

        Self { sender_key, hash: base64_encode(sha.as_slice()) }
    }
}

/// Account data that's static for the lifetime of a Client.
///
/// This data never changes once it's set, so it can be freely passed and cloned
/// everywhere.
#[derive(Clone)]
#[cfg_attr(not(tarpaulin_include), derive(Debug))]
pub struct StaticAccountData {
    /// The user_id this account belongs to.
    pub user_id: OwnedUserId,
    /// The device_id of this entry.
    pub device_id: OwnedDeviceId,
    /// The associated identity keys.
    pub identity_keys: Arc<IdentityKeys>,
    /// Whether the account is for a dehydrated device.
    pub dehydrated: bool,
    // The creation time of the account in milliseconds since epoch.
    creation_local_time: MilliSecondsSinceUnixEpoch,
}

impl StaticAccountData {
    const ALGORITHMS: &'static [&'static EventEncryptionAlgorithm] = &[
        &EventEncryptionAlgorithm::OlmV1Curve25519AesSha2,
        #[cfg(feature = "experimental-algorithms")]
        &EventEncryptionAlgorithm::OlmV2Curve25519AesSha2,
        &EventEncryptionAlgorithm::MegolmV1AesSha2,
        #[cfg(feature = "experimental-algorithms")]
        &EventEncryptionAlgorithm::MegolmV2AesSha2,
    ];

    /// Create a group session pair.
    ///
    /// This session pair can be used to encrypt and decrypt messages meant for
    /// a large group of participants.
    ///
    /// The outbound session is used to encrypt messages while the inbound one
    /// is used to decrypt messages encrypted by the outbound one.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The ID of the room where the group session will be used.
    ///
    /// * `settings` - Settings determining the algorithm and rotation period of
    ///   the outbound group session.
    pub async fn create_group_session_pair(
        &self,
        room_id: &RoomId,
        settings: EncryptionSettings,
        own_sender_data: SenderData,
    ) -> Result<(OutboundGroupSession, InboundGroupSession), MegolmSessionCreationError> {
        trace!(?room_id, algorithm = settings.algorithm.as_str(), "Creating a new room key");

        let visibility = settings.history_visibility.clone();
        let algorithm = settings.algorithm.to_owned();

        let outbound = OutboundGroupSession::new(
            self.device_id.clone(),
            self.identity_keys.clone(),
            room_id,
            settings,
        )?;

        let identity_keys = &self.identity_keys;

        let sender_key = identity_keys.curve25519;
        let signing_key = identity_keys.ed25519;
        let shared_history = shared_history_from_history_visibility(&visibility);

        let inbound = InboundGroupSession::new(
            sender_key,
            signing_key,
            room_id,
            &outbound.session_key().await,
            own_sender_data,
            None,
            algorithm,
            Some(visibility),
            shared_history,
        )?;

        Ok((outbound, inbound))
    }

    #[cfg(any(test, feature = "testing"))]
    #[allow(dead_code)]
    /// Testing only facility to create a group session pair with default
    /// settings.
    pub async fn create_group_session_pair_with_defaults(
        &self,
        room_id: &RoomId,
    ) -> (OutboundGroupSession, InboundGroupSession) {
        self.create_group_session_pair(
            room_id,
            EncryptionSettings::default(),
            SenderData::unknown(),
        )
        .await
        .expect("Can't create default group session pair")
    }

    /// Get the key ID of our Ed25519 signing key.
    pub fn signing_key_id(&self) -> OwnedDeviceKeyId {
        DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, self.device_id())
    }

    /// Check if the given JSON is signed by this Account key.
    ///
    /// This method should only be used if an object's signature needs to be
    /// checked multiple times, and you'd like to avoid performing the
    /// canonicalization step each time.
    ///
    /// **Note**: Use this method with caution, the `canonical_json` needs to be
    /// correctly canonicalized and make sure that the object you are checking
    /// the signature for is allowed to be signed by our own device.
    pub fn has_signed_raw(
        &self,
        signatures: &crate::types::Signatures,
        canonical_json: &str,
    ) -> Result<(), SignatureError> {
        use crate::olm::utility::VerifyJson;

        let signing_key = self.identity_keys.ed25519;

        signing_key.verify_canonicalized_json(
            &self.user_id,
            &DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, self.device_id()),
            signatures,
            canonical_json,
        )
    }

    /// Generate the unsigned `DeviceKeys` from this `StaticAccountData`.
    pub fn unsigned_device_keys(&self) -> DeviceKeys {
        let identity_keys = self.identity_keys();
        let keys = BTreeMap::from([
            (
                DeviceKeyId::from_parts(DeviceKeyAlgorithm::Curve25519, &self.device_id),
                identity_keys.curve25519.into(),
            ),
            (
                DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, &self.device_id),
                identity_keys.ed25519.into(),
            ),
        ]);

        let mut ret = DeviceKeys::new(
            (*self.user_id).to_owned(),
            (*self.device_id).to_owned(),
            Self::ALGORITHMS.iter().map(|a| (**a).clone()).collect(),
            keys,
            Default::default(),
        );
        if self.dehydrated {
            ret.dehydrated = JsOption::Some(true);
        }
        ret
    }

    /// Get the user id of the owner of the account.
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    /// Get the device ID that owns this account.
    pub fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    /// Get the public parts of the identity keys for the account.
    pub fn identity_keys(&self) -> IdentityKeys {
        *self.identity_keys
    }

    /// Get the local timestamp creation of the account in secs since epoch.
    pub fn creation_local_time(&self) -> MilliSecondsSinceUnixEpoch {
        self.creation_local_time
    }
}

/// Account holding identity keys for which sessions can be created.
///
/// An account is the central identity for encrypted communication between two
/// devices.
pub struct Account {
    pub(crate) static_data: StaticAccountData,
    /// `vodozemac` account.
    inner: Box<InnerAccount>,
    /// Is this account ready to encrypt messages? (i.e. has it shared keys with
    /// a homeserver)
    shared: bool,
    /// The number of signed one-time keys we have uploaded to the server. If
    /// this is None, no action will be taken. After a sync request the client
    /// needs to set this for us, depending on the count we will suggest the
    /// client to upload new keys.
    uploaded_signed_key_count: u64,
    /// The timestamp of the last time we generated a fallback key. Fallback
    /// keys are rotated in a time-based manner. This field records when we
    /// either generated our first fallback key or rotated one.
    ///
    /// Will be `None` if we never created a fallback key, or if we're migrating
    /// from a `AccountPickle` that didn't use time-based fallback key
    /// rotation.
    fallback_creation_timestamp: Option<MilliSecondsSinceUnixEpoch>,
}

impl Deref for Account {
    type Target = StaticAccountData;

    fn deref(&self) -> &Self::Target {
        &self.static_data
    }
}

/// A pickled version of an `Account`.
///
/// Holds all the information that needs to be stored in a database to restore
/// an account.
#[derive(Serialize, Deserialize)]
#[allow(missing_debug_implementations)]
pub struct PickledAccount {
    /// The user id of the account owner.
    pub user_id: OwnedUserId,
    /// The device ID of the account owner.
    pub device_id: OwnedDeviceId,
    /// The pickled version of the Olm account.
    pub pickle: AccountPickle,
    /// Was the account shared.
    pub shared: bool,
    /// Whether this is for a dehydrated device
    #[serde(default)]
    pub dehydrated: bool,
    /// The number of uploaded one-time keys we have on the server.
    pub uploaded_signed_key_count: u64,
    /// The local time creation of this account (milliseconds since epoch), used
    /// as creation time of own device
    #[serde(default = "default_account_creation_time")]
    pub creation_local_time: MilliSecondsSinceUnixEpoch,
    /// The timestamp of the last time we generated a fallback key.
    #[serde(default)]
    pub fallback_key_creation_timestamp: Option<MilliSecondsSinceUnixEpoch>,
}

fn default_account_creation_time() -> MilliSecondsSinceUnixEpoch {
    MilliSecondsSinceUnixEpoch(UInt::default())
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Account")
            .field("identity_keys", &self.identity_keys())
            .field("shared", &self.shared())
            .finish()
    }
}

pub type OneTimeKeys = BTreeMap<OwnedOneTimeKeyId, Raw<ruma::encryption::OneTimeKey>>;
pub type FallbackKeys = OneTimeKeys;

impl Account {
    pub(crate) fn new_helper(
        mut account: InnerAccount,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Self {
        let identity_keys = account.identity_keys();

        // Let's generate some initial one-time keys while we're here. Since we know
        // that this is a completely new [`Account`] we're certain that the
        // server does not yet have any one-time keys of ours.
        //
        // This ensures we upload one-time keys along with our device keys right
        // away, rather than waiting for the key counts to be echoed back to us
        // from the server.
        //
        // It would be nice to do this for the fallback key as well but we can't assume
        // that the server supports fallback keys. Maybe one of these days we
        // will be able to do so.
        account.generate_one_time_keys(account.max_number_of_one_time_keys());

        Self {
            static_data: StaticAccountData {
                user_id: user_id.into(),
                device_id: device_id.into(),
                identity_keys: Arc::new(identity_keys),
                dehydrated: false,
                creation_local_time: MilliSecondsSinceUnixEpoch::now(),
            },
            inner: Box::new(account),
            shared: false,
            uploaded_signed_key_count: 0,
            fallback_creation_timestamp: None,
        }
    }

    /// Create a fresh new account, this will generate the identity key-pair.
    pub fn with_device_id(user_id: &UserId, device_id: &DeviceId) -> Self {
        let account = InnerAccount::new();

        Self::new_helper(account, user_id, device_id)
    }

    /// Create a new random Olm Account, the long-term Curve25519 identity key
    /// encoded as base64 will be used for the device ID.
    pub fn new(user_id: &UserId) -> Self {
        let account = InnerAccount::new();
        let device_id: OwnedDeviceId =
            base64_encode(account.identity_keys().curve25519.as_bytes()).into();

        Self::new_helper(account, user_id, &device_id)
    }

    /// Create a new random Olm Account for a dehydrated device
    pub fn new_dehydrated(user_id: &UserId) -> Self {
        let account = InnerAccount::new();
        let device_id: OwnedDeviceId =
            base64_encode(account.identity_keys().curve25519.as_bytes()).into();

        let mut ret = Self::new_helper(account, user_id, &device_id);
        ret.static_data.dehydrated = true;
        ret
    }

    /// Get the immutable data for this account.
    pub fn static_data(&self) -> &StaticAccountData {
        &self.static_data
    }

    /// Update the uploaded key count.
    ///
    /// # Arguments
    ///
    /// * `new_count` - The new count that was reported by the server.
    pub fn update_uploaded_key_count(&mut self, new_count: u64) {
        self.uploaded_signed_key_count = new_count;
    }

    /// Get the currently known uploaded key count.
    pub fn uploaded_key_count(&self) -> u64 {
        self.uploaded_signed_key_count
    }

    /// Has the account been shared with the server.
    pub fn shared(&self) -> bool {
        self.shared
    }

    /// Mark the account as shared.
    ///
    /// Messages shouldn't be encrypted with the session before it has been
    /// shared.
    pub fn mark_as_shared(&mut self) {
        self.shared = true;
    }

    /// Get the one-time keys of the account.
    ///
    /// This can be empty, keys need to be generated first.
    pub fn one_time_keys(&self) -> HashMap<KeyId, Curve25519PublicKey> {
        self.inner.one_time_keys()
    }

    /// Generate count number of one-time keys.
    pub fn generate_one_time_keys(&mut self, count: usize) -> OneTimeKeyGenerationResult {
        self.inner.generate_one_time_keys(count)
    }

    /// Get the maximum number of one-time keys the account can hold.
    pub fn max_one_time_keys(&self) -> usize {
        self.inner.max_number_of_one_time_keys()
    }

    pub(crate) fn update_key_counts(
        &mut self,
        one_time_key_counts: &BTreeMap<OneTimeKeyAlgorithm, UInt>,
        unused_fallback_keys: Option<&[OneTimeKeyAlgorithm]>,
    ) {
        if let Some(count) = one_time_key_counts.get(&OneTimeKeyAlgorithm::SignedCurve25519) {
            let count: u64 = (*count).into();
            let old_count = self.uploaded_key_count();

            // Some servers might always return the key counts in the sync
            // response, we don't want to the logs with noop changes if they do
            // so.
            if count != old_count {
                debug!(
                    "Updated uploaded one-time key count {} -> {count}.",
                    self.uploaded_key_count(),
                );
            }

            self.update_uploaded_key_count(count);
            self.generate_one_time_keys_if_needed();
        }

        // If the server supports fallback keys or if it did so in the past, shown by
        // the existence of a fallback creation timestamp, generate a new one if
        // we don't have one, or if the current fallback key expired.
        if unused_fallback_keys.is_some() || self.fallback_creation_timestamp.is_some() {
            self.generate_fallback_key_if_needed();
        }
    }

    /// Generate new one-time keys that need to be uploaded to the server.
    ///
    /// Returns None if no keys need to be uploaded, otherwise the number of
    /// newly generated one-time keys. May return 0 if some one-time keys are
    /// already generated but weren't uploaded.
    ///
    /// Generally `Some` means that keys should be uploaded, while `None` means
    /// that keys should not be uploaded.
    #[instrument(skip_all)]
    pub fn generate_one_time_keys_if_needed(&mut self) -> Option<u64> {
        // Only generate one-time keys if there aren't any, otherwise the caller
        // might have failed to upload them the last time this method was
        // called.
        if !self.one_time_keys().is_empty() {
            return Some(0);
        }

        let count = self.uploaded_key_count();
        let max_keys = self.max_one_time_keys();

        if count >= max_keys as u64 {
            return None;
        }

        let key_count = (max_keys as u64) - count;
        let key_count: usize = key_count.try_into().unwrap_or(max_keys);

        let result = self.generate_one_time_keys(key_count);

        debug!(
            count = key_count,
            discarded_keys = ?result.removed,
            created_keys = ?result.created,
            "Generated new one-time keys"
        );

        Some(key_count as u64)
    }

    /// Generate a new fallback key iff a unpublished one isn't already inside
    /// of vodozemac and if the currently active one expired.
    ///
    /// The former is checked using [`Account::fallback_key().is_empty()`],
    /// which is a hashmap that gets cleared by the
    /// [`Account::mark_keys_as_published()`] call.
    pub(crate) fn generate_fallback_key_if_needed(&mut self) {
        if self.inner.fallback_key().is_empty() && self.fallback_key_expired() {
            let removed_fallback_key = self.inner.generate_fallback_key();
            self.fallback_creation_timestamp = Some(MilliSecondsSinceUnixEpoch::now());

            debug!(
                ?removed_fallback_key,
                "The fallback key either expired or we didn't have one: generated a new fallback key.",
            );
        }
    }

    /// Check if our most recent fallback key has expired.
    ///
    /// We consider the fallback key to be expired if it's older than a week.
    /// This is the lower bound for the recommended signed pre-key bundle
    /// rotation interval in the X3DH spec[1].
    ///
    /// [1]: https://signal.org/docs/specifications/x3dh/#publishing-keys
    fn fallback_key_expired(&self) -> bool {
        const FALLBACK_KEY_MAX_AGE: Duration = Duration::from_secs(3600 * 24 * 7);

        if let Some(time) = self.fallback_creation_timestamp {
            // `to_system_time()` returns `None` if the the UNIX_EPOCH + `time` doesn't fit
            // into a i64. This will likely never happen, but let's rotate the
            // key in case the values are messed up for some other reason.
            let Some(system_time) = time.to_system_time() else {
                return true;
            };

            // `elapsed()` errors if the `system_time` is in the future, this should mean
            // that our clock has changed to the past, let's rotate just in case
            // and then we'll get to a normal time.
            let Ok(elapsed) = system_time.elapsed() else {
                return true;
            };

            // Alright, our times are normal and we know how much time elapsed since the
            // last time we created/rotated a fallback key.
            //
            // If the key is older than a week, then we rotate it.
            elapsed > FALLBACK_KEY_MAX_AGE
        } else {
            // We never created a fallback key, or we're migrating to the time-based
            // fallback key rotation, so let's generate a new fallback key.
            true
        }
    }

    fn fallback_key(&self) -> HashMap<KeyId, Curve25519PublicKey> {
        self.inner.fallback_key()
    }

    /// Get a tuple of device, one-time, and fallback keys that need to be
    /// uploaded.
    ///
    /// If no keys need to be uploaded the `DeviceKeys` will be `None` and the
    /// one-time and fallback keys maps will be empty.
    pub fn keys_for_upload(&self) -> (Option<DeviceKeys>, OneTimeKeys, FallbackKeys) {
        let device_keys = self.shared().not().then(|| self.device_keys());

        let one_time_keys = self.signed_one_time_keys();
        let fallback_keys = self.signed_fallback_keys();

        (device_keys, one_time_keys, fallback_keys)
    }

    /// Mark the current set of one-time keys as being published.
    pub fn mark_keys_as_published(&mut self) {
        self.inner.mark_keys_as_published();
    }

    /// Sign the given string using the accounts signing key.
    ///
    /// Returns the signature as a base64 encoded string.
    pub fn sign(&self, string: &str) -> Ed25519Signature {
        self.inner.sign(string)
    }

    /// Get a serializable version of the `Account` so it can be persisted.
    pub fn pickle(&self) -> PickledAccount {
        let pickle = self.inner.pickle();

        PickledAccount {
            user_id: self.user_id().to_owned(),
            device_id: self.device_id().to_owned(),
            pickle,
            shared: self.shared(),
            dehydrated: self.static_data.dehydrated,
            uploaded_signed_key_count: self.uploaded_key_count(),
            creation_local_time: self.static_data.creation_local_time,
            fallback_key_creation_timestamp: self.fallback_creation_timestamp,
        }
    }

    pub(crate) fn dehydrate(&self, pickle_key: &[u8; 32]) -> Raw<DehydratedDeviceData> {
        let dehydration_result = self
            .inner
            .to_dehydrated_device(pickle_key)
            .expect("We should be able to convert a freshly created Account into a libolm pickle");

        let data = DehydratedDeviceData::V2(DehydratedDeviceV2::new(
            dehydration_result.ciphertext,
            dehydration_result.nonce,
        ));
        Raw::from_json(to_raw_value(&data).expect("Couldn't serialize our dehydrated device data"))
    }

    pub(crate) fn rehydrate(
        pickle_key: &[u8; 32],
        user_id: &UserId,
        device_id: &DeviceId,
        device_data: Raw<DehydratedDeviceData>,
    ) -> Result<Self, DehydrationError> {
        let data = device_data.deserialize()?;

        match data {
            DehydratedDeviceData::V1(d) => {
                let pickle_key = expand_legacy_pickle_key(pickle_key, device_id);
                let account =
                    InnerAccount::from_libolm_pickle(&d.device_pickle, pickle_key.as_ref())?;
                Ok(Self::new_helper(account, user_id, device_id))
            }
            DehydratedDeviceData::V2(d) => {
                let account =
                    InnerAccount::from_dehydrated_device(&d.device_pickle, &d.nonce, pickle_key)?;
                Ok(Self::new_helper(account, user_id, device_id))
            }
            _ => Err(DehydrationError::Json(serde_json::Error::custom(format!(
                "Unsupported dehydrated device algorithm {:?}",
                data.algorithm()
            )))),
        }
    }

    /// Produce a dehydrated device using a format described in an older version
    /// of MSC3814.
    #[cfg(test)]
    pub(crate) fn legacy_dehydrate(&self, pickle_key: &[u8; 32]) -> Raw<DehydratedDeviceData> {
        let pickle_key = expand_legacy_pickle_key(pickle_key, &self.device_id);
        let device_pickle = self
            .inner
            .to_libolm_pickle(pickle_key.as_ref())
            .expect("We should be able to convert a freshly created Account into a libolm pickle");

        let data = DehydratedDeviceData::V1(DehydratedDeviceV1::new(device_pickle));
        Raw::from_json(to_raw_value(&data).expect("Couldn't serialize our dehydrated device data"))
    }

    /// Restore an account from a previously pickled one.
    ///
    /// # Arguments
    ///
    /// * `pickle` - The pickled version of the Account.
    ///
    /// * `pickle_mode` - The mode that was used to pickle the account, either
    ///   an unencrypted mode or an encrypted using passphrase.
    pub fn from_pickle(pickle: PickledAccount) -> Result<Self, PickleError> {
        let account: vodozemac::olm::Account = pickle.pickle.into();
        let identity_keys = account.identity_keys();

        Ok(Self {
            static_data: StaticAccountData {
                user_id: (*pickle.user_id).into(),
                device_id: (*pickle.device_id).into(),
                identity_keys: Arc::new(identity_keys),
                dehydrated: pickle.dehydrated,
                creation_local_time: pickle.creation_local_time,
            },
            inner: Box::new(account),
            shared: pickle.shared,
            uploaded_signed_key_count: pickle.uploaded_signed_key_count,
            fallback_creation_timestamp: pickle.fallback_key_creation_timestamp,
        })
    }

    /// Sign the device keys of the account and return them so they can be
    /// uploaded.
    pub fn device_keys(&self) -> DeviceKeys {
        let mut device_keys = self.unsigned_device_keys();

        // Create a copy of the device keys containing only fields that will
        // get signed.
        let json_device_keys =
            to_canonical_value(&device_keys).expect("device key is always safe to serialize");
        let signature = self
            .sign_json(json_device_keys)
            .expect("Newly created device keys can always be signed");

        device_keys.signatures.add_signature(
            self.user_id().to_owned(),
            DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, &self.static_data.device_id),
            signature,
        );

        device_keys
    }

    /// Bootstraps cross-signing, generating new cross-signing keys and creating
    /// the necessary upload and signature requests.
    ///
    /// # Returns
    /// A tuple containing:
    /// - [`PrivateCrossSigningIdentity`]: The newly-generated cross-signing
    ///   identity (including a signature from this device).
    /// - [`UploadSigningKeysRequest`]: The request to upload the
    ///   newly-generated cross-signing keys to the server.
    /// - [`SignatureUploadRequest`]: The request to upload the signature of
    ///   this device to the server.
    pub async fn bootstrap_cross_signing(
        &self,
    ) -> (PrivateCrossSigningIdentity, UploadSigningKeysRequest, SignatureUploadRequest) {
        let identity = PrivateCrossSigningIdentity::for_account(self);

        let signature_request = identity
            .sign_account(self.static_data())
            .await
            .expect("Can't sign own device with new cross signing keys");

        let upload_request = identity.as_upload_request().await;

        (identity, upload_request, signature_request)
    }

    /// Sign the given CrossSigning Key in place
    pub fn sign_cross_signing_key(
        &self,
        cross_signing_key: &mut CrossSigningKey,
    ) -> Result<(), SignatureError> {
        let signature = self.sign_json(to_canonical_value(&cross_signing_key)?)?;

        cross_signing_key.signatures.add_signature(
            self.user_id().to_owned(),
            DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, self.device_id()),
            signature,
        );

        Ok(())
    }

    /// Sign the given Master Key
    pub fn sign_master_key(
        &self,
        master_key: &MasterPubkey,
    ) -> Result<SignatureUploadRequest, SignatureError> {
        let public_key =
            master_key.get_first_key().ok_or(SignatureError::MissingSigningKey)?.to_base64().into();

        let mut cross_signing_key: CrossSigningKey = master_key.as_ref().clone();
        cross_signing_key.signatures.clear();
        self.sign_cross_signing_key(&mut cross_signing_key)?;

        let mut user_signed_keys = SignedKeys::new();
        user_signed_keys.add_cross_signing_keys(public_key, cross_signing_key.to_raw());

        let signed_keys = [(self.user_id().to_owned(), user_signed_keys)].into();
        Ok(SignatureUploadRequest::new(signed_keys))
    }

    /// Convert a JSON value to the canonical representation and sign the JSON
    /// string.
    ///
    /// # Arguments
    ///
    /// * `json` - The value that should be converted into a canonical JSON
    ///   string.
    pub fn sign_json(&self, json: CanonicalJsonValue) -> Result<Ed25519Signature, SignatureError> {
        self.inner.sign_json(json)
    }

    /// Sign and prepare one-time keys to be uploaded.
    ///
    /// If no one-time keys need to be uploaded, returns an empty `BTreeMap`.
    pub fn signed_one_time_keys(&self) -> OneTimeKeys {
        let one_time_keys = self.one_time_keys();

        if one_time_keys.is_empty() {
            BTreeMap::new()
        } else {
            self.signed_keys(one_time_keys, false)
        }
    }

    /// Sign and prepare fallback keys to be uploaded.
    ///
    /// If no fallback keys need to be uploaded returns an empty BTreeMap.
    pub fn signed_fallback_keys(&self) -> FallbackKeys {
        let fallback_key = self.fallback_key();

        if fallback_key.is_empty() { BTreeMap::new() } else { self.signed_keys(fallback_key, true) }
    }

    fn signed_keys(
        &self,
        keys: HashMap<KeyId, Curve25519PublicKey>,
        fallback: bool,
    ) -> OneTimeKeys {
        let mut keys_map = BTreeMap::new();

        for (key_id, key) in keys {
            let signed_key = self.sign_key(key, fallback);

            keys_map.insert(
                OneTimeKeyId::from_parts(
                    OneTimeKeyAlgorithm::SignedCurve25519,
                    key_id.to_base64().as_str().into(),
                ),
                signed_key.into_raw(),
            );
        }

        keys_map
    }

    fn sign_key(&self, key: Curve25519PublicKey, fallback: bool) -> SignedKey {
        let mut key = if fallback {
            SignedKey::new_fallback(key.to_owned())
        } else {
            SignedKey::new(key.to_owned())
        };

        let signature = self
            .sign_json(to_canonical_value(&key).expect("Can't serialize a signed key"))
            .expect("Newly created one-time keys can always be signed");

        key.signatures_mut().add_signature(
            self.user_id().to_owned(),
            DeviceKeyId::from_parts(DeviceKeyAlgorithm::Ed25519, self.device_id()),
            signature,
        );

        key
    }

    /// Create a new session with another account given a one-time key.
    ///
    /// Returns the newly created session or a `OlmSessionError` if creating a
    /// session failed.
    ///
    /// # Arguments
    ///
    /// * `config` - The session config that should be used when creating the
    ///   Session.
    ///
    /// * `identity_key` - The other account's identity/curve25519 key.
    ///
    /// * `one_time_key` - A signed one-time key that the other account created
    ///   and shared with us.
    ///
    /// * `fallback_used` - Was the one-time key a fallback key.
    ///
    /// * `our_device_keys` - Our own `DeviceKeys`, including cross-signing
    ///   signatures if applicable, for embedding in encrypted messages.
    pub fn create_outbound_session_helper(
        &self,
        config: SessionConfig,
        identity_key: Curve25519PublicKey,
        one_time_key: Curve25519PublicKey,
        fallback_used: bool,
        our_device_keys: DeviceKeys,
    ) -> Session {
        let session = self.inner.create_outbound_session(config, identity_key, one_time_key);

        let now = SecondsSinceUnixEpoch::now();
        let session_id = session.session_id();

        Session {
            inner: Arc::new(Mutex::new(session)),
            session_id: session_id.into(),
            sender_key: identity_key,
            our_device_keys,
            created_using_fallback_key: fallback_used,
            creation_time: now,
            last_use_time: now,
        }
    }

    #[instrument(
        skip_all,
        fields(
            user_id = ?device.user_id(),
            device_id = ?device.device_id(),
            algorithms = ?device.algorithms()
        )
    )]
    fn find_pre_key_bundle(
        device: &DeviceData,
        key_map: &OneTimeKeys,
    ) -> Result<PrekeyBundle, SessionCreationError> {
        let mut keys = key_map.iter();

        let first_key = keys.next().ok_or_else(|| {
            SessionCreationError::OneTimeKeyMissing(
                device.user_id().to_owned(),
                device.device_id().into(),
            )
        })?;

        let first_key_id = first_key.0.to_owned();
        let first_key = OneTimeKey::deserialize(first_key_id.algorithm(), first_key.1)?;

        let result = match first_key {
            OneTimeKey::SignedKey(key) => Ok(PrekeyBundle::Olm3DH { key }),
        };

        trace!(?result, "Finished searching for a valid pre-key bundle");

        result
    }

    /// Create a new session with another account given a one-time key and a
    /// device.
    ///
    /// Returns the newly created session or a `OlmSessionError` if creating a
    /// session failed.
    ///
    /// # Arguments
    /// * `device` - The other account's device.
    ///
    /// * `key_map` - A map from the algorithm and device ID to the one-time key
    ///   that the other account created and shared with us.
    ///
    /// * `our_device_keys` - Our own `DeviceKeys`, including cross-signing
    ///   signatures if applicable, for embedding in encrypted messages.
    #[allow(clippy::result_large_err)]
    pub fn create_outbound_session(
        &self,
        device: &DeviceData,
        key_map: &OneTimeKeys,
        our_device_keys: DeviceKeys,
    ) -> Result<Session, SessionCreationError> {
        let pre_key_bundle = Self::find_pre_key_bundle(device, key_map)?;

        match pre_key_bundle {
            PrekeyBundle::Olm3DH { key } => {
                device.verify_one_time_key(&key).map_err(|error| {
                    SessionCreationError::InvalidSignature {
                        signing_key: device.ed25519_key().map(Box::new),
                        one_time_key: key.clone().into(),
                        error: error.into(),
                    }
                })?;

                let identity_key = device.curve25519_key().ok_or_else(|| {
                    SessionCreationError::DeviceMissingCurveKey(
                        device.user_id().to_owned(),
                        device.device_id().into(),
                    )
                })?;

                let is_fallback = key.fallback();
                let one_time_key = key.key();
                let config = device.olm_session_config();

                Ok(self.create_outbound_session_helper(
                    config,
                    identity_key,
                    one_time_key,
                    is_fallback,
                    our_device_keys,
                ))
            }
        }
    }

    /// Create a new session with another account given a pre-key Olm message.
    ///
    /// Returns the newly created session or a `OlmSessionError` if creating a
    /// session failed.
    ///
    /// # Arguments
    ///
    /// * `their_identity_key` - The other account's identity/curve25519 key.
    ///
    /// * `our_device_keys` - Our own `DeviceKeys`, including cross-signing
    ///   signatures if applicable, for embedding in encrypted messages.
    ///
    /// * `message` - A pre-key Olm message that was sent to us by the other
    ///   account.
    pub fn create_inbound_session(
        &mut self,
        their_identity_key: Curve25519PublicKey,
        our_device_keys: DeviceKeys,
        message: &PreKeyMessage,
    ) -> Result<InboundCreationResult, SessionCreationError> {
        Span::current().record("session_id", debug(message.session_id()));
        trace!("Creating a new Olm session from a pre-key message");

        let result = self.inner.create_inbound_session(their_identity_key, message)?;
        let now = SecondsSinceUnixEpoch::now();
        let session_id = result.session.session_id();

        debug!(session=?result.session, "Decrypted an Olm message from a new Olm session");

        let session = Session {
            inner: Arc::new(Mutex::new(result.session)),
            session_id: session_id.into(),
            sender_key: their_identity_key,
            our_device_keys,
            created_using_fallback_key: false,
            creation_time: now,
            last_use_time: now,
        };

        let plaintext = String::from_utf8_lossy(&result.plaintext).to_string();

        Ok(InboundCreationResult { session, plaintext })
    }

    #[cfg(any(test, feature = "testing"))]
    #[allow(dead_code)]
    /// Testing only helper to create a session for the given Account
    pub async fn create_session_for_test_helper(
        &mut self,
        other: &mut Account,
    ) -> (Session, Session) {
        use ruma::events::dummy::ToDeviceDummyEventContent;

        other.generate_one_time_keys(1);
        let one_time_map = other.signed_one_time_keys();
        let device = DeviceData::from_account(other);

        let mut our_session =
            self.create_outbound_session(&device, &one_time_map, self.device_keys()).unwrap();

        other.mark_keys_as_published();

        let message = our_session
            .encrypt(&device, "m.dummy", ToDeviceDummyEventContent::new(), None)
            .await
            .unwrap()
            .deserialize()
            .unwrap();

        #[cfg(feature = "experimental-algorithms")]
        let content = if let ToDeviceEncryptedEventContent::OlmV2Curve25519AesSha2(c) = message {
            c
        } else {
            panic!("Invalid encrypted event algorithm {}", message.algorithm());
        };

        #[cfg(not(feature = "experimental-algorithms"))]
        let ToDeviceEncryptedEventContent::OlmV1Curve25519AesSha2(content) = message else {
            panic!("Invalid encrypted event algorithm {}", message.algorithm());
        };

        let OlmMessage::PreKey(prekey) = content.ciphertext else {
            panic!("Wrong Olm message type");
        };

        let our_device = DeviceData::from_account(self);
        let other_session = other
            .create_inbound_session(
                our_device.curve25519_key().unwrap(),
                other.device_keys(),
                &prekey,
            )
            .unwrap();

        (our_session, other_session.session)
    }

    async fn decrypt_olm_helper(
        &mut self,
        store: &Store,
        sender: &UserId,
        sender_key: Curve25519PublicKey,
        ciphertext: &OlmMessage,
        decryption_settings: &DecryptionSettings,
    ) -> OlmResult<OlmDecryptionInfo> {
        let message_hash = OlmMessageHash::new(sender_key, ciphertext);

        match self
            .decrypt_and_parse_olm_message(
                store,
                sender,
                sender_key,
                ciphertext,
                decryption_settings,
            )
            .await
        {
            Ok((session, result)) => {
                Ok(OlmDecryptionInfo { session, message_hash, result, inbound_group_session: None })
            }
            Err(OlmError::SessionWedged(user_id, sender_key)) => {
                if store.is_message_known(&message_hash).await? {
                    info!(?sender_key, "An Olm message got replayed, decryption failed");
                    Err(OlmError::ReplayedMessage(user_id, sender_key))
                } else {
                    Err(OlmError::SessionWedged(user_id, sender_key))
                }
            }
            Err(e) => Err(e),
        }
    }

    #[cfg(feature = "experimental-algorithms")]
    async fn decrypt_olm_v2(
        &mut self,
        store: &Store,
        sender: &UserId,
        content: &OlmV2Curve25519AesSha2Content,
        decryption_settings: &DecryptionSettings,
    ) -> OlmResult<OlmDecryptionInfo> {
        self.decrypt_olm_helper(
            store,
            sender,
            content.sender_key,
            &content.ciphertext,
            decryption_settings,
        )
        .await
    }

    #[instrument(skip_all, fields(sender, sender_key = ?content.sender_key))]
    async fn decrypt_olm_v1(
        &mut self,
        store: &Store,
        sender: &UserId,
        content: &OlmV1Curve25519AesSha2Content,
        decryption_settings: &DecryptionSettings,
    ) -> OlmResult<OlmDecryptionInfo> {
        if content.recipient_key != self.static_data.identity_keys.curve25519 {
            warn!("Olm event doesn't contain a ciphertext for our key");

            Err(EventError::MissingCiphertext.into())
        } else {
            Box::pin(self.decrypt_olm_helper(
                store,
                sender,
                content.sender_key,
                &content.ciphertext,
                decryption_settings,
            ))
            .await
        }
    }

    #[instrument(skip_all, fields(algorithm = ?event.content.algorithm()))]
    pub(crate) async fn decrypt_to_device_event(
        &mut self,
        store: &Store,
        event: &EncryptedToDeviceEvent,
        decryption_settings: &DecryptionSettings,
    ) -> OlmResult<OlmDecryptionInfo> {
        trace!("Decrypting a to-device event");

        match &event.content {
            ToDeviceEncryptedEventContent::OlmV1Curve25519AesSha2(c) => {
                self.decrypt_olm_v1(store, &event.sender, c, decryption_settings).await
            }
            #[cfg(feature = "experimental-algorithms")]
            ToDeviceEncryptedEventContent::OlmV2Curve25519AesSha2(c) => {
                self.decrypt_olm_v2(store, &event.sender, c, decryption_settings).await
            }
            ToDeviceEncryptedEventContent::Unknown(_) => {
                warn!(
                    "Error decrypting an to-device event, unsupported \
                    encryption algorithm"
                );

                Err(EventError::UnsupportedAlgorithm.into())
            }
        }
    }

    /// Handles a response to a /keys/upload request.
    pub fn receive_keys_upload_response(
        &mut self,
        response: &upload_keys::v3::Response,
    ) -> OlmResult<()> {
        if !self.shared() {
            debug!("Marking account as shared");
        }
        self.mark_as_shared();

        debug!("Marking one-time keys as published");
        // First mark the current keys as published, as updating the key counts might
        // generate some new keys if we're still below the limit.
        self.mark_keys_as_published();
        self.update_key_counts(&response.one_time_key_counts, None);

        Ok(())
    }

    /// Try to decrypt an olm message, creating a new session if necessary.
    async fn decrypt_olm_message(
        &mut self,
        store: &Store,
        sender: &UserId,
        sender_key: Curve25519PublicKey,
        message: &OlmMessage,
    ) -> Result<(SessionType, String), OlmError> {
        let existing_sessions = store.get_sessions(&sender_key.to_base64()).await?;

        match message {
            OlmMessage::Normal(_) => {
                let mut errors_by_olm_session = Vec::new();

                if let Some(sessions) = existing_sessions {
                    // Try to decrypt the message using each Session we share with the
                    // given curve25519 sender key.
                    for session in sessions.lock().await.iter_mut() {
                        match session.decrypt(message).await {
                            Ok(p) => {
                                // success!
                                return Ok((SessionType::Existing(session.clone()), p));
                            }

                            Err(e) => {
                                // An error here is completely normal, after all we don't know
                                // which session was used to encrypt a message.
                                // We keep hold of the error, so that if *all* sessions fail to
                                // decrypt, we can log something useful.
                                errors_by_olm_session.push((session.session_id().to_owned(), e));
                            }
                        }
                    }
                }

                warn!(
                    ?errors_by_olm_session,
                    "Failed to decrypt a non-pre-key message with all available sessions"
                );
                Err(OlmError::SessionWedged(sender.to_owned(), sender_key))
            }

            OlmMessage::PreKey(prekey_message) => {
                // First try to decrypt using an existing session.
                if let Some(sessions) = existing_sessions {
                    for session in sessions.lock().await.iter_mut() {
                        if prekey_message.session_id() != session.session_id() {
                            // wrong session
                            continue;
                        }

                        if let Ok(p) = session.decrypt(message).await {
                            // success!
                            return Ok((SessionType::Existing(session.clone()), p));
                        }

                        // The message was intended for this session, but we weren't able to
                        // decrypt it.
                        //
                        // There's no point trying any other sessions, nor should we try to
                        // create a new one since we have already previously created a `Session`
                        // with the same keys.
                        //
                        // (Attempts to create a new session would likely fail anyway since the
                        // corresponding one-time key would've been already used up in the
                        // previous session creation operation. The one exception where this
                        // would not be so is if the fallback key was used for creating the
                        // session in lieu of an OTK.)

                        warn!(
                            session_id = session.session_id(),
                            "Failed to decrypt a pre-key message with the corresponding session"
                        );

                        return Err(OlmError::SessionWedged(
                            session.our_device_keys.user_id.to_owned(),
                            session.sender_key(),
                        ));
                    }
                }

                let device_keys = store.get_own_device().await?.as_device_keys().clone();
                let result =
                    match self.create_inbound_session(sender_key, device_keys, prekey_message) {
                        Ok(r) => r,
                        Err(e) => {
                            warn!(
                                "Failed to create a new Olm session from a pre-key message: {e:?}"
                            );
                            return Err(OlmError::SessionWedged(sender.to_owned(), sender_key));
                        }
                    };

                // We need to add the new session to the session cache, otherwise
                // we might try to create the same session again.
                // TODO: separate the session cache from the storage so we only add
                // it to the cache but don't store it.
                let mut changes =
                    Changes { sessions: vec![result.session.clone()], ..Default::default() };

                // Any new Olm session will bump the Olm wedging index for the
                // sender's device, if we have their device, which will cause us
                // to re-send existing Megolm sessions to them the next time we
                // use the session.  If we don't have their device, this means
                // that we haven't tried to send them any Megolm sessions yet,
                // so we don't need to worry about it.
                if let Some(device) = store.get_device_from_curve_key(sender, sender_key).await? {
                    let mut device_data = device.inner;
                    device_data.olm_wedging_index.increment();

                    changes.devices =
                        DeviceChanges { changed: vec![device_data], ..Default::default() };
                }

                store.save_changes(changes).await?;

                Ok((SessionType::New(result.session), result.plaintext))
            }
        }
    }

    /// Decrypt an Olm message, creating a new Olm session if necessary, and
    /// parse the result.
    #[instrument(skip(self, store), fields(session, session_id))]
    async fn decrypt_and_parse_olm_message(
        &mut self,
        store: &Store,
        sender: &UserId,
        sender_key: Curve25519PublicKey,
        message: &OlmMessage,
        decryption_settings: &DecryptionSettings,
    ) -> OlmResult<(SessionType, DecryptionResult)> {
        let (session, plaintext) =
            self.decrypt_olm_message(store, sender, sender_key, message).await?;

        trace!("Successfully decrypted an Olm message");

        match self
            .parse_decrypted_to_device_event(
                store,
                sender,
                sender_key,
                plaintext,
                decryption_settings,
            )
            .await
        {
            Ok(result) => Ok((session, result)),
            Err(e) => {
                // We might have created a new session but decryption might still
                // have failed, store it for the error case here, this is fine
                // since we don't expect this to happen often or at all.
                match session {
                    SessionType::New(s) | SessionType::Existing(s) => {
                        store.save_sessions(&[s]).await?;
                    }
                }

                warn!(
                    error = ?e,
                    "A to-device message was successfully decrypted but \
                    parsing and checking the event fields failed"
                );

                Err(e)
            }
        }
    }

    /// Parse the decrypted plaintext as JSON and verify that it wasn't
    /// forwarded by a third party.
    ///
    /// These checks are mandated by the spec[1]:
    ///
    /// > Other properties are included in order to prevent an attacker from
    /// > publishing someone else's Curve25519 keys as their own and
    /// > subsequently claiming to have sent messages which they didn't.
    /// > sender must correspond to the user who sent the event, recipient to
    /// > the local user, and recipient_keys to the local Ed25519 key.
    ///
    /// # Arguments
    ///
    /// * `sender` -  The `sender` field from the top level of the received
    ///   event.
    /// * `sender_key` - The `sender_key` from the cleartext `content` of the
    ///   received event (which should also have been used to find or establish
    ///   the Olm session that was used to decrypt the event -- so it is
    ///   guaranteed to be correct).
    /// * `plaintext` - The decrypted content of the event.
    async fn parse_decrypted_to_device_event(
        &self,
        store: &Store,
        sender: &UserId,
        sender_key: Curve25519PublicKey,
        plaintext: String,
        decryption_settings: &DecryptionSettings,
    ) -> OlmResult<DecryptionResult> {
        let event: Box<AnyDecryptedOlmEvent> = serde_json::from_str(&plaintext)?;
        let identity_keys = &self.static_data.identity_keys;

        if event.recipient() != self.static_data.user_id {
            Err(EventError::MismatchedSender(
                event.recipient().to_owned(),
                self.static_data.user_id.clone(),
            )
            .into())
        }
        // Check that the `sender` in the decrypted to-device event matches that at the
        // top level of the encrypted event.
        else if event.sender() != sender {
            Err(EventError::MismatchedSender(event.sender().to_owned(), sender.to_owned()).into())
        } else if identity_keys.ed25519 != event.recipient_keys().ed25519 {
            Err(EventError::MismatchedKeys(
                identity_keys.ed25519.into(),
                event.recipient_keys().ed25519.into(),
            )
            .into())
        } else {
            let sender_device = Self::get_event_sender_device(store, sender_key, &event).await?;
            let encryption_info = Self::get_olm_encryption_info(sender_key, sender, &sender_device);

            let result = DecryptionResult {
                event,
                raw_event: Raw::from_json(RawJsonValue::from_string(plaintext)?),
                sender_key,
                encryption_info,
            };

            // Return an error if the sender is unverified (and we care)
            if !self.is_from_verified_device_or_allowed_type(decryption_settings, &result) {
                Err(OlmError::UnverifiedSenderDevice)
            } else {
                // Sender is ok - return the decrypted event
                Ok(result)
            }
        }
    }

    /// Look up the [`Device`] that sent us a successfully-decrypted event.
    ///
    /// We first look for the sender device in our store; if it is found then we
    /// return that (having checked that the keys match). If the device is
    /// not found in the store, we return the details
    /// from `sender_device_keys`, if present. If the device is not in the
    /// store, and the event lacks `sender_device_keys`, an error is returned.
    ///
    /// Also validates the `sender_device_keys` field, if present, regardless of
    /// whether it is used.
    ///
    /// `m.room_key` events are special-cased and return `None`: we look up
    /// their devices later on.
    async fn get_event_sender_device(
        store: &Store,
        sender_key: Curve25519PublicKey,
        event: &AnyDecryptedOlmEvent,
    ) -> OlmResult<Option<Device>> {
        // If the event contained sender_device_keys, check them now.
        // WARN: If you move or modify this check, ensure that the code below is still
        // valid. The processing of the historic room key bundle depends on this being
        // here.
        let sender_device_keys = Self::check_sender_device_keys(event, sender_key)?;
        if let AnyDecryptedOlmEvent::RoomKey(_) = event {
            // If this event is an `m.room_key` event, defer the check for
            // the Ed25519 key of the sender until we decrypt room events.
            // This ensures that we receive the room key even if we don't
            // have access to the device.
            return Ok(None);
        }

        // MSC4268 requires room key bundle events to have a `sender_device_keys` field.
        // Enforce that now.
        if let AnyDecryptedOlmEvent::RoomKeyBundle(_) = event {
            sender_device_keys.ok_or(EventError::MissingSigningKey).inspect_err(|_| {
                warn!("The room key bundle was missing the sender device keys in the event")
            })?;
        }

        // For event types other than `m.room_key`, we need to look up the device in the
        // database irrespective of whether the `sender_device_keys` field is
        // present in the event, because it may have been marked as "locally
        // trusted" in the database.
        let store_device = store.get_device_from_curve_key(event.sender(), sender_key).await?;

        match (store_device, sender_device_keys) {
            // If the device is in the database, it had better have an Ed25519 key which
            // matches that in the event.
            (Some(device), _) => {
                let key = device.ed25519_key().ok_or(EventError::MissingSigningKey)?;
                if key != event.keys().ed25519 {
                    return Err(EventError::MismatchedKeys(
                        key.into(),
                        event.keys().ed25519.into(),
                    )
                    .into());
                }
                Ok(Some(device))
            }

            (None, Some(sender_device_keys)) => {
                // We have already validated the signature on `sender_device_keys`, so this
                // try_into cannot fail.
                let sender_device_data = sender_device_keys.try_into().expect("Conversion of DeviceKeys to DeviceData failed despite the signature already having been checked");
                Ok(Some(store.wrap_device_data(sender_device_data).await?))
            }

            (None, None) => Err(OlmError::EventError(EventError::MissingSigningKey)),
        }
    }

    /// Return true if:
    ///
    /// * the sending device is verified, or
    /// * the event type is one of those we allow to be sent from unverified
    ///   devices, or
    /// * we are not in "exclude_insecure_devices" mode, so everything is
    ///   allowed.
    ///
    /// Return false if:
    ///
    /// * we are in "exclude_insecure_devices" mode AND the sending device is
    ///   unverified.
    fn is_from_verified_device_or_allowed_type(
        &self,
        decryption_settings: &DecryptionSettings,
        result: &DecryptionResult,
    ) -> bool {
        let event_type = result.event.event_type();

        // If we're in "exclude insecure devices" mode, we prevent most
        // to-device events with unverified senders from being allowed
        // through here, but there are some exceptions:
        //
        // * m.room_key - we hold on to these until later, so if the sender becomes
        //   verified later we can still use the key.
        //
        // * m.room_key_request, m.room_key.withheld, m.key.verification.*,
        //   m.secret.request - these are allowed as plaintext events, so we also allow
        //   them encrypted from insecure devices. Note: the list of allowed types here
        //   should match with what is allowed in handle_to_device_event.
        match event_type {
            "m.room_key"
            | "m.room_key.withheld"
            | "m.room_key_request"
            | "m.secret.request"
            | "m.key.verification.key"
            | "m.key.verification.mac"
            | "m.key.verification.done"
            | "m.key.verification.ready"
            | "m.key.verification.start"
            | "m.key.verification.accept"
            | "m.key.verification.cancel"
            | "m.key.verification.request" => {
                // This is one of the exception types - we allow it even if the sender device is
                // not verified.
                true
            }
            _ => {
                // This is not an exception type - check for "exclude insecure devices" mode,
                // and whether the sender is verified.
                satisfies_sender_trust_requirement(
                    &result.encryption_info,
                    &decryption_settings.sender_device_trust_requirement,
                )
            }
        }
    }

    /// Gets the EncryptionInfo for a successfully decrypted to-device message
    /// that have passed the mismatched sender_key/user_id validation.
    ///
    /// `sender_device` is optional because for some to-device messages we defer
    /// the check for the ed25519 key, in that case the
    /// `verification_state` will have a `MissingDevice` link problem.
    fn get_olm_encryption_info(
        sender_key: Curve25519PublicKey,
        sender_id: &UserId,
        sender_device: &Option<Device>,
    ) -> EncryptionInfo {
        let verification_state = sender_device
            .as_ref()
            .map(|device| {
                if device.is_verified() {
                    // The device is locally verified or signed by a verified user
                    VerificationState::Verified
                } else if device.is_cross_signed_by_owner() {
                    // The device is not verified, but it is signed by its owner
                    if device
                        .device_owner_identity
                        .as_ref()
                        .expect("A device cross-signed by the owner must have an owner identity")
                        .was_previously_verified()
                    {
                        VerificationState::Unverified(VerificationLevel::VerificationViolation)
                    } else {
                        VerificationState::Unverified(VerificationLevel::UnverifiedIdentity)
                    }
                } else {
                    // No identity or not signed
                    VerificationState::Unverified(VerificationLevel::UnsignedDevice)
                }
            })
            .unwrap_or(VerificationState::Unverified(VerificationLevel::None(
                DeviceLinkProblem::MissingDevice,
            )));

        EncryptionInfo {
            sender: sender_id.to_owned(),
            sender_device: sender_device.as_ref().map(|d| d.device_id().to_owned()),
            forwarder: None,
            algorithm_info: AlgorithmInfo::OlmV1Curve25519AesSha2 {
                curve25519_public_key_base64: sender_key.to_base64(),
            },
            verification_state,
        }
    }

    /// If the plaintext of the decrypted message includes a
    /// `sender_device_keys` property per [MSC4147], check that it is valid.
    ///
    /// In particular, we check that:
    ///
    ///  * The Curve25519 key in the `sender_device_keys` matches that used to
    ///    establish the Olm session that was used to decrypt the event.
    ///
    ///  * The `sender_device_keys` contains a valid self-signature by the
    ///    Ed25519 key in the device data.
    ///
    ///  * The Ed25519 key in the device data matches that in the `keys` field
    ///    in the event, for consistency and sanity.
    ///
    /// The first two checks are sufficient to bind together the Ed25519 and
    /// Curve25519 keys:
    ///
    ///  * Only the holder of the secret part of the Curve25519 key that was
    ///    used to construct the Olm session (the 'owner' of that key) can
    ///    encrypt the device data in that Olm session. By including the Ed25519
    ///    key in the device data, the owner of the Curve25519 key is claiming
    ///    ownership of the Ed25519 key.
    ///
    ///  * Only the owner of the Ed25519 key can construct the self-signature on
    ///    the device data. By including the Curve25519 key in the device data
    ///    and then signing it, the owner of the Ed25519 key is claiming
    ///    ownership of the Curve25519 key.
    ///
    ///  * Since we now have claims in both directions, the two key owners must
    ///    either be the same entity, or working in sufficiently close
    ///    collaboration that they can be treated as such.
    ///
    /// # Arguments
    ///
    /// * `event` - The decrypted and deserialized plaintext of the event.
    /// * `sender_key` - The Curve25519 key that the sender used to establish
    ///   the Olm session that was used to decrypt the event.
    ///
    /// # Returns
    ///
    /// A reference to the `sender_device_keys` in the event, if it exists and
    /// is valid.
    ///
    /// [MSC4147]: https://github.com/matrix-org/matrix-spec-proposals/pull/4147
    fn check_sender_device_keys(
        event: &AnyDecryptedOlmEvent,
        sender_key: Curve25519PublicKey,
    ) -> OlmResult<Option<&DeviceKeys>> {
        let Some(sender_device_keys) = event.sender_device_keys() else {
            return Ok(None);
        };

        // Check the signature within the device_keys structure
        sender_device_keys.check_self_signature().map_err(|err| {
            warn!(
                "Received a to-device message with sender_device_keys with \
                 invalid signature: {err:?}",
            );
            OlmError::EventError(EventError::InvalidSenderDeviceKeys)
        })?;

        // Check that the Ed25519 key in the sender_device_keys matches the `ed25519`
        // key in the `keys` field in the event.
        if sender_device_keys.ed25519_key() != Some(event.keys().ed25519) {
            warn!(
                "Received a to-device message with sender_device_keys with incorrect \
                 ed25519 key: expected {:?}, got {:?}",
                event.keys().ed25519,
                sender_device_keys.ed25519_key(),
            );
            return Err(OlmError::EventError(EventError::InvalidSenderDeviceKeys));
        }

        // Check that the Curve25519 key in the sender_device_keys matches the key that
        // was used for the Olm session.
        if sender_device_keys.curve25519_key() != Some(sender_key) {
            warn!(
                "Received a to-device message with sender_device_keys with incorrect \
                 curve25519 key: expected {sender_key:?}, got {:?}",
                sender_device_keys.curve25519_key(),
            );
            return Err(OlmError::EventError(EventError::InvalidSenderDeviceKeys));
        }

        Ok(Some(sender_device_keys))
    }

    /// Internal use only.
    ///
    /// Cloning should only be done for testing purposes or when we are certain
    /// that we don't want the inner state to be shared.
    #[doc(hidden)]
    pub fn deep_clone(&self) -> Self {
        // `vodozemac::Account` isn't really cloneable, but... Don't tell anyone.
        Self::from_pickle(self.pickle()).unwrap()
    }
}

impl PartialEq for Account {
    fn eq(&self, other: &Self) -> bool {
        self.identity_keys() == other.identity_keys() && self.shared() == other.shared()
    }
}

/// Calculate the shared history flag from the history visibility as defined in
/// [MSC3061]
///
/// The MSC defines that the shared history flag should be set to true when the
/// history visibility setting is set to `shared` or `world_readable`:
///
/// > A room key is flagged as having been used for shared history when it was
/// > used to encrypt a message while the room's history visibility setting
/// > was set to world_readable or shared.
///
/// In all other cases, even if we encounter a custom history visibility, we
/// should return false:
///
/// > If the client does not have an m.room.history_visibility state event for
/// > the room, or its value is not understood, the client should treat it as if
/// > its value is joined for the purposes of determining whether the key is
/// > used for shared history.
///
/// [MSC3061]: https://github.com/matrix-org/matrix-spec-proposals/pull/3061
pub(crate) fn shared_history_from_history_visibility(
    history_visibility: &HistoryVisibility,
) -> bool {
    match history_visibility {
        HistoryVisibility::Shared | HistoryVisibility::WorldReadable => true,
        HistoryVisibility::Invited | HistoryVisibility::Joined | _ => false,
    }
}

/// Expand the pickle key for an older version of dehydrated devices
///
/// The `org.matrix.msc3814.v1.olm` variant of dehydrated devices used the
/// libolm Account pickle format for the dehydrated device. The libolm pickle
/// encryption scheme uses HKDF to deterministically expand an input key
/// material, usually 32 bytes, into a AES key, MAC key, and the initialization
/// vector (IV).
///
/// This means that the same input key material will always end up producing the
/// same AES key, and IV.
///
/// This encryption scheme is used in the Olm double ratchet and was designed to
/// minimize the size of the ciphertext. As a tradeof, it requires a unique
/// input key material for each plaintext that gets encrypted, otherwise IV
/// reuse happens.
///
/// To combat the IV reuse, we're going to create a per-dehydrated-device unique
/// pickle key by expanding the key itself with the device ID used as the salt.
fn expand_legacy_pickle_key(key: &[u8; 32], device_id: &DeviceId) -> Box<[u8; 32]> {
    let kdf: Hkdf<Sha256> = Hkdf::new(Some(device_id.as_bytes()), key);
    let mut key = Box::new([0u8; 32]);

    kdf.expand(b"dehydrated-device-pickle-key", key.as_mut_slice())
        .expect("We should be able to expand the 32 byte pickle key");

    key
}

/// Does the to-device event satisfy the sender trust requirement from the
/// decryption settings?
fn satisfies_sender_trust_requirement(
    encryption_info: &EncryptionInfo,
    trust_requirement: &TrustRequirement,
) -> bool {
    trace!(
        verification_state = ?encryption_info.verification_state,
        ?trust_requirement, "check_to_device_sender_trust_requirement",
    );

    match (&encryption_info.verification_state, trust_requirement) {
        // If we don't care, everything is OK.
        (_, TrustRequirement::Untrusted) => true,

        // Verified is OK whatever our requirements are.
        (VerificationState::Verified, _) => true,

        // We do care, and we are not fully verified: check more deeply.
        // (Note that for to-device messages the legacy trust requirement is not relevant.)
        (
            VerificationState::Unverified(verification_level),
            TrustRequirement::CrossSignedOrLegacy | TrustRequirement::CrossSigned,
        ) => match verification_level {
            // The device is signed but the identity is only pinned - this is fine.
            VerificationLevel::UnverifiedIdentity => true,

            // The device is unsigned or missing, or the user is in verification violation,
            // or the sender is mismatched: this is not fine.
            VerificationLevel::UnsignedDevice
            | VerificationLevel::None(_)
            | VerificationLevel::VerificationViolation
            | VerificationLevel::MismatchedSender => false,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        ops::Deref,
        time::Duration,
    };

    use anyhow::Result;
    use matrix_sdk_test::async_test;
    use ruma::{
        DeviceId, MilliSecondsSinceUnixEpoch, OneTimeKeyAlgorithm, OneTimeKeyId, UserId, device_id,
        events::room::history_visibility::HistoryVisibility, room_id, user_id,
    };
    use serde_json::json;

    use super::Account;
    use crate::{
        DeviceData, EncryptionSettings,
        olm::{SignedJsonObject, account::shared_history_from_history_visibility},
        types::{DeviceKeys, SignedKey},
    };

    fn user_id() -> &'static UserId {
        user_id!("@alice:localhost")
    }

    fn device_id() -> &'static DeviceId {
        device_id!("DEVICEID")
    }

    #[test]
    fn test_one_time_key_creation() -> Result<()> {
        let mut account = Account::with_device_id(user_id(), device_id());

        let (_, one_time_keys, _) = account.keys_for_upload();
        assert!(!one_time_keys.is_empty());

        let (_, second_one_time_keys, _) = account.keys_for_upload();
        assert!(!second_one_time_keys.is_empty());

        let one_time_key_ids: BTreeSet<&OneTimeKeyId> =
            one_time_keys.keys().map(Deref::deref).collect();
        let second_one_time_key_ids: BTreeSet<&OneTimeKeyId> =
            second_one_time_keys.keys().map(Deref::deref).collect();

        assert_eq!(one_time_key_ids, second_one_time_key_ids);

        account.mark_keys_as_published();
        account.update_uploaded_key_count(50);
        account.generate_one_time_keys_if_needed();

        let (_, third_one_time_keys, _) = account.keys_for_upload();
        assert!(third_one_time_keys.is_empty());

        account.update_uploaded_key_count(0);
        account.generate_one_time_keys_if_needed();

        let (_, fourth_one_time_keys, _) = account.keys_for_upload();
        assert!(!fourth_one_time_keys.is_empty());

        let fourth_one_time_key_ids: BTreeSet<&OneTimeKeyId> =
            fourth_one_time_keys.keys().map(Deref::deref).collect();

        assert_ne!(one_time_key_ids, fourth_one_time_key_ids);
        Ok(())
    }

    #[test]
    fn test_fallback_key_creation() -> Result<()> {
        let mut account = Account::with_device_id(user_id(), device_id());

        let (_, _, fallback_keys) = account.keys_for_upload();

        // We don't create fallback keys since we don't know if the server
        // supports them, we need to receive a sync response to decide if we're
        // going to create them or not.
        assert!(
            fallback_keys.is_empty(),
            "We should not upload fallback keys until we know if the server supports them."
        );

        let one_time_keys = BTreeMap::from([(OneTimeKeyAlgorithm::SignedCurve25519, 50u8.into())]);

        // A `None` here means that the server doesn't support fallback keys, no
        // fallback key gets uploaded.
        account.update_key_counts(&one_time_keys, None);
        let (_, _, fallback_keys) = account.keys_for_upload();
        assert!(
            fallback_keys.is_empty(),
            "We should not upload a fallback key if we're certain that the server doesn't support \
             them."
        );

        // The empty array means that the server supports fallback keys but
        // there isn't a unused fallback key on the server. This time we upload
        // a fallback key.
        let unused_fallback_keys = &[];
        account.update_key_counts(&one_time_keys, Some(unused_fallback_keys.as_ref()));
        let (_, _, fallback_keys) = account.keys_for_upload();
        assert!(
            !fallback_keys.is_empty(),
            "We should upload the initial fallback key if the server supports them."
        );
        account.mark_keys_as_published();

        // There's no unused fallback key on the server, but our initial fallback key
        // did not yet expire.
        let unused_fallback_keys = &[];
        account.update_key_counts(&one_time_keys, Some(unused_fallback_keys.as_ref()));
        let (_, _, fallback_keys) = account.keys_for_upload();
        assert!(
            fallback_keys.is_empty(),
            "We should not upload new fallback keys unless our current fallback key expires."
        );

        let fallback_key_timestamp =
            account.fallback_creation_timestamp.unwrap().to_system_time().unwrap()
                - Duration::from_secs(3600 * 24 * 30);

        account.fallback_creation_timestamp =
            Some(MilliSecondsSinceUnixEpoch::from_system_time(fallback_key_timestamp).unwrap());

        account.update_key_counts(&one_time_keys, None);
        let (_, _, fallback_keys) = account.keys_for_upload();
        assert!(
            !fallback_keys.is_empty(),
            "Now that our fallback key has expired, we should try to upload a new one, even if the \
             server supposedly doesn't support fallback keys anymore"
        );

        Ok(())
    }

    #[test]
    fn test_fallback_key_signing() -> Result<()> {
        let key = vodozemac::Curve25519PublicKey::from_base64(
            "7PUPP6Ijt5R8qLwK2c8uK5hqCNF9tOzWYgGaAay5JBs",
        )?;
        let account = Account::with_device_id(user_id(), device_id());

        let key = account.sign_key(key, true);

        let canonical_key = key.to_canonical_json()?;

        assert_eq!(
            canonical_key,
            "{\"fallback\":true,\"key\":\"7PUPP6Ijt5R8qLwK2c8uK5hqCNF9tOzWYgGaAay5JBs\"}"
        );

        account
            .has_signed_raw(key.signatures(), &canonical_key)
            .expect("Couldn't verify signature");

        let device = DeviceData::from_account(&account);
        device.verify_one_time_key(&key).expect("The device can verify its own signature");

        Ok(())
    }

    #[test]
    fn test_account_and_device_creation_timestamp() -> Result<()> {
        let now = MilliSecondsSinceUnixEpoch::now();
        let account = Account::with_device_id(user_id(), device_id());
        let then = MilliSecondsSinceUnixEpoch::now();

        assert!(account.creation_local_time() >= now);
        assert!(account.creation_local_time() <= then);

        let device = DeviceData::from_account(&account);
        assert_eq!(account.creation_local_time(), device.first_time_seen_ts());

        Ok(())
    }

    #[async_test]
    async fn test_fallback_key_signature_verification() -> Result<()> {
        let fallback_key = json!({
            "fallback": true,
            "key": "XPFqtLvBepBmW6jSAbBuJbhEpprBhQOX1IjUu+cnMF4",
            "signatures": {
                "@dkasak_c:matrix.org": {
                    "ed25519:EXPDYDPWZH": "RJCBMJPL5hvjxgq8rmLmqkNOuPsaan7JeL1wsE+gW6R39G894lb2sBmzapHeKCn/KFjmkonPLkICApRDS+zyDw"
                }
            }
        });

        let device_keys = json!({
            "algorithms": [
                "m.olm.v1.curve25519-aes-sha2",
                "m.megolm.v1.aes-sha2"
            ],
            "device_id": "EXPDYDPWZH",
            "keys": {
                "curve25519:EXPDYDPWZH": "k7f3igo0Vrdm88JSSA5d3OCuUfHYELChB2b57aOROB8",
                "ed25519:EXPDYDPWZH": "GdjYI8fxs175gSpYRJkyN6FRfvcyTsNOhJ2OR/Ggp+E"
            },
            "signatures": {
                "@dkasak_c:matrix.org": {
                    "ed25519:EXPDYDPWZH": "kzrtfQMbJXWXQ1uzhybtwFnGk0JJBS4Mg8VPMusMu6U8MPJccwoHVZKo5+owuHTzIodI+GZYqLmMSzvfvsChAA"
                }
            },
            "user_id": "@dkasak_c:matrix.org",
            "unsigned": {}
        });

        let device_keys: DeviceKeys = serde_json::from_value(device_keys).unwrap();
        let device = DeviceData::try_from(&device_keys).unwrap();
        let fallback_key: SignedKey = serde_json::from_value(fallback_key).unwrap();

        device
            .verify_one_time_key(&fallback_key)
            .expect("The fallback key should pass the signature verification");

        Ok(())
    }

    #[test]
    fn test_shared_history_flag_from_history_visibility() {
        assert!(
            shared_history_from_history_visibility(&HistoryVisibility::WorldReadable),
            "The world readable visibility should set the shared history flag to true"
        );

        assert!(
            shared_history_from_history_visibility(&HistoryVisibility::Shared),
            "The shared visibility should set the shared history flag to true"
        );

        assert!(
            !shared_history_from_history_visibility(&HistoryVisibility::Joined),
            "The joined visibility should set the shared history flag to false"
        );

        assert!(
            !shared_history_from_history_visibility(&HistoryVisibility::Invited),
            "The invited visibility should set the shared history flag to false"
        );

        let visibility = HistoryVisibility::from("custom_visibility");
        assert!(
            !shared_history_from_history_visibility(&visibility),
            "A custom visibility should set the shared history flag to false"
        );
    }

    #[async_test]
    async fn test_shared_history_set_when_creating_group_sessions() {
        let account = Account::new(user_id());
        let room_id = room_id!("!room:id");
        let settings = EncryptionSettings {
            history_visibility: HistoryVisibility::Shared,
            ..Default::default()
        };

        let (_, session) = account
            .create_group_session_pair(room_id, settings, Default::default())
            .await
            .expect("We should be able to create a group session pair");

        assert!(
            session.shared_history(),
            "The shared history flag should have been set when we created the new session"
        );
    }
}
