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

use matrix_sdk_common::instant::Instant;
use std::{
    convert::{TryFrom, TryInto},
    fmt,
    sync::{
        atomic::{AtomicBool, AtomicI64, Ordering},
        Arc,
    },
};

use matrix_sdk_common::locks::Mutex;
use serde_json::{json, Value};
use std::collections::BTreeMap;

pub use olm_rs::account::IdentityKeys;
use olm_rs::{
    account::{OlmAccount, OneTimeKeys},
    errors::{OlmAccountError, OlmSessionError},
    PicklingMode,
};

use crate::{device::Device, error::SessionCreationError};
pub use olm_rs::{
    session::{OlmMessage, PreKeyMessage},
    utility::OlmUtility,
};

use matrix_sdk_common::{
    api::r0::keys::{AlgorithmAndDeviceId, DeviceKeys, KeyAlgorithm, OneTimeKey, SignedKey},
    events::Algorithm,
    identifiers::{DeviceId, RoomId, UserId},
};

use super::{InboundGroupSession, OutboundGroupSession, Session};

/// Account holding identity keys for which sessions can be created.
///
/// An account is the central identity for encrypted communication between two
/// devices.
#[derive(Clone)]
pub struct Account {
    pub(crate) user_id: Arc<UserId>,
    pub(crate) device_id: Arc<Box<DeviceId>>,
    inner: Arc<Mutex<OlmAccount>>,
    pub(crate) identity_keys: Arc<IdentityKeys>,
    shared: Arc<AtomicBool>,
    /// The number of signed one-time keys we have uploaded to the server. If
    /// this is None, no action will be taken. After a sync request the client
    /// needs to set this for us, depending on the count we will suggest the
    /// client to upload new keys.
    uploaded_signed_key_count: Arc<AtomicI64>,
}

// #[cfg_attr(tarpaulin, skip)]
impl fmt::Debug for Account {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Account")
            .field("identity_keys", self.identity_keys())
            .field("shared", &self.shared())
            .finish()
    }
}

impl Account {
    const ALGORITHMS: &'static [&'static Algorithm] = &[
        &Algorithm::OlmV1Curve25519AesSha2,
        &Algorithm::MegolmV1AesSha2,
    ];

    /// Create a fresh new account, this will generate the identity key-pair.
    #[allow(clippy::ptr_arg)]
    pub fn new(user_id: &UserId, device_id: &DeviceId) -> Self {
        let account = OlmAccount::new();
        let identity_keys = account.parsed_identity_keys();

        Account {
            user_id: Arc::new(user_id.to_owned()),
            device_id: Arc::new(device_id.into()),
            inner: Arc::new(Mutex::new(account)),
            identity_keys: Arc::new(identity_keys),
            shared: Arc::new(AtomicBool::new(false)),
            uploaded_signed_key_count: Arc::new(AtomicI64::new(0)),
        }
    }

    /// Get the public parts of the identity keys for the account.
    pub fn identity_keys(&self) -> &IdentityKeys {
        &self.identity_keys
    }

    /// Update the uploaded key count.
    ///
    /// # Arguments
    ///
    /// * `new_count` - The new count that was reported by the server.
    pub(crate) fn update_uploaded_key_count(&self, new_count: u64) {
        let key_count = i64::try_from(new_count).unwrap_or(i64::MAX);
        self.uploaded_signed_key_count
            .store(key_count, Ordering::Relaxed);
    }

    /// Get the currently known uploaded key count.
    pub fn uploaded_key_count(&self) -> i64 {
        self.uploaded_signed_key_count.load(Ordering::Relaxed)
    }

    /// Has the account been shared with the server.
    pub fn shared(&self) -> bool {
        self.shared.load(Ordering::Relaxed)
    }

    /// Mark the account as shared.
    ///
    /// Messages shouldn't be encrypted with the session before it has been
    /// shared.
    pub(crate) fn mark_as_shared(&self) {
        self.shared.store(true, Ordering::Relaxed);
    }

    /// Get the one-time keys of the account.
    ///
    /// This can be empty, keys need to be generated first.
    pub(crate) async fn one_time_keys(&self) -> OneTimeKeys {
        self.inner.lock().await.parsed_one_time_keys()
    }

    /// Generate count number of one-time keys.
    pub(crate) async fn generate_one_time_keys_helper(&self, count: usize) {
        self.inner.lock().await.generate_one_time_keys(count);
    }

    /// Get the maximum number of one-time keys the account can hold.
    pub(crate) async fn max_one_time_keys(&self) -> usize {
        self.inner.lock().await.max_number_of_one_time_keys()
    }

    /// Get a tuple of device and one-time keys that need to be uploaded.
    ///
    /// Returns an empty error if no keys need to be uploaded.
    pub(crate) async fn generate_one_time_keys(&self) -> Result<u64, ()> {
        let count = self.uploaded_key_count() as u64;
        let max_keys = self.max_one_time_keys().await;
        let max_on_server = (max_keys as u64) / 2;

        if count >= (max_on_server) {
            return Err(());
        }

        let key_count = (max_on_server) - count;
        let key_count: usize = key_count.try_into().unwrap_or(max_keys);

        self.generate_one_time_keys_helper(key_count).await;
        Ok(key_count as u64)
    }

    /// Should account or one-time keys be uploaded to the server.
    pub(crate) async fn should_upload_keys(&self) -> bool {
        if !self.shared() {
            return true;
        }

        let count = self.uploaded_key_count() as u64;

        // If we have a known key count, check that we have more than
        // max_one_time_Keys() / 2, otherwise tell the client to upload more.
        let max_keys = self.max_one_time_keys().await as u64;
        // If there are more keys already uploaded than max_key / 2
        // bail out returning false, this also avoids overflow.
        if count > (max_keys / 2) {
            return false;
        }

        let key_count = (max_keys / 2) - count;
        key_count > 0
    }

    /// Get a tuple of device and one-time keys that need to be uploaded.
    ///
    /// Returns an empty error if no keys need to be uploaded.
    pub(crate) async fn keys_for_upload(
        &self,
    ) -> Result<
        (
            Option<DeviceKeys>,
            Option<BTreeMap<AlgorithmAndDeviceId, OneTimeKey>>,
        ),
        (),
    > {
        if !self.should_upload_keys().await {
            return Err(());
        }

        let device_keys = if !self.shared() {
            Some(self.device_keys().await)
        } else {
            None
        };

        let one_time_keys = self.signed_one_time_keys().await.ok();

        Ok((device_keys, one_time_keys))
    }

    /// Mark the current set of one-time keys as being published.
    pub(crate) async fn mark_keys_as_published(&self) {
        self.inner.lock().await.mark_keys_as_published();
    }

    /// Sign the given string using the accounts signing key.
    ///
    /// Returns the signature as a base64 encoded string.
    pub async fn sign(&self, string: &str) -> String {
        self.inner.lock().await.sign(string)
    }

    /// Store the account as a base64 encoded string.
    ///
    /// # Arguments
    ///
    /// * `pickle_mode` - The mode that was used to pickle the account, either an
    /// unencrypted mode or an encrypted using passphrase.
    pub async fn pickle(&self, pickle_mode: PicklingMode) -> String {
        self.inner.lock().await.pickle(pickle_mode)
    }

    /// Restore an account from a previously pickled string.
    ///
    /// # Arguments
    ///
    /// * `pickle` - The pickled string of the account.
    ///
    /// * `pickle_mode` - The mode that was used to pickle the account, either an
    /// unencrypted mode or an encrypted using passphrase.
    ///
    /// * `shared` - Boolean determining if the account was uploaded to the
    /// server.
    #[allow(clippy::ptr_arg)]
    pub fn from_pickle(
        pickle: String,
        pickle_mode: PicklingMode,
        shared: bool,
        uploaded_signed_key_count: i64,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Self, OlmAccountError> {
        let account = OlmAccount::unpickle(pickle, pickle_mode)?;
        let identity_keys = account.parsed_identity_keys();

        Ok(Account {
            user_id: Arc::new(user_id.to_owned()),
            device_id: Arc::new(device_id.into()),
            inner: Arc::new(Mutex::new(account)),
            identity_keys: Arc::new(identity_keys),
            shared: Arc::new(AtomicBool::from(shared)),
            uploaded_signed_key_count: Arc::new(AtomicI64::new(uploaded_signed_key_count)),
        })
    }

    /// Sign the device keys of the account and return them so they can be
    /// uploaded.
    pub(crate) async fn device_keys(&self) -> DeviceKeys {
        let identity_keys = self.identity_keys();

        let mut keys = BTreeMap::new();

        keys.insert(
            AlgorithmAndDeviceId(KeyAlgorithm::Curve25519, (*self.device_id).clone()),
            identity_keys.curve25519().to_owned(),
        );
        keys.insert(
            AlgorithmAndDeviceId(KeyAlgorithm::Ed25519, (*self.device_id).clone()),
            identity_keys.ed25519().to_owned(),
        );

        let device_keys = json!({
            "user_id": (*self.user_id).clone(),
            "device_id": (*self.device_id).clone(),
            "algorithms": Account::ALGORITHMS,
            "keys": keys,
        });

        let mut signatures = BTreeMap::new();

        let mut signature = BTreeMap::new();
        signature.insert(
            AlgorithmAndDeviceId(KeyAlgorithm::Ed25519, (*self.device_id).clone()),
            self.sign_json(&device_keys).await,
        );
        signatures.insert((*self.user_id).clone(), signature);

        DeviceKeys {
            user_id: (*self.user_id).clone(),
            device_id: (*self.device_id).clone(),
            algorithms: vec![
                Algorithm::OlmV1Curve25519AesSha2,
                Algorithm::MegolmV1AesSha2,
            ],
            keys,
            signatures,
            unsigned: None,
        }
    }

    /// Convert a JSON value to the canonical representation and sign the JSON
    /// string.
    ///
    /// # Arguments
    ///
    /// * `json` - The value that should be converted into a canonical JSON
    /// string.
    ///
    /// # Panic
    ///
    /// Panics if the json value can't be serialized.
    pub async fn sign_json(&self, json: &Value) -> String {
        let canonical_json = cjson::to_string(json)
            .unwrap_or_else(|_| panic!(format!("Can't serialize {} to canonical JSON", json)));
        self.sign(&canonical_json).await
    }

    /// Generate, sign and prepare one-time keys to be uploaded.
    ///
    /// If no one-time keys need to be uploaded returns an empty error.
    pub(crate) async fn signed_one_time_keys(
        &self,
    ) -> Result<BTreeMap<AlgorithmAndDeviceId, OneTimeKey>, ()> {
        let _ = self.generate_one_time_keys().await?;

        let one_time_keys = self.one_time_keys().await;
        let mut one_time_key_map = BTreeMap::new();

        for (key_id, key) in one_time_keys.curve25519().iter() {
            let key_json = json!({
                "key": key,
            });

            let signature = self.sign_json(&key_json).await;

            let mut signature_map = BTreeMap::new();

            signature_map.insert(
                AlgorithmAndDeviceId(KeyAlgorithm::Ed25519, (*self.device_id).clone()),
                signature,
            );

            let mut signatures = BTreeMap::new();
            signatures.insert((*self.user_id).clone(), signature_map);

            let signed_key = SignedKey {
                key: key.to_owned(),
                signatures,
            };

            one_time_key_map.insert(
                AlgorithmAndDeviceId(KeyAlgorithm::SignedCurve25519, key_id.as_str().into()),
                OneTimeKey::SignedKey(signed_key),
            );
        }

        Ok(one_time_key_map)
    }

    /// Create a new session with another account given a one-time key.
    ///
    /// Returns the newly created session or a `OlmSessionError` if creating a
    /// session failed.
    ///
    /// # Arguments
    /// * `their_identity_key` - The other account's identity/curve25519 key.
    ///
    /// * `their_one_time_key` - A signed one-time key that the other account
    /// created and shared with us.
    pub(crate) async fn create_outbound_session_helper(
        &self,
        their_identity_key: &str,
        their_one_time_key: &SignedKey,
    ) -> Result<Session, OlmSessionError> {
        let session = self
            .inner
            .lock()
            .await
            .create_outbound_session(their_identity_key, &their_one_time_key.key)?;

        let now = Instant::now();
        let session_id = session.session_id();

        Ok(Session {
            user_id: self.user_id.clone(),
            device_id: self.device_id.clone(),
            our_identity_keys: self.identity_keys.clone(),
            inner: Arc::new(Mutex::new(session)),
            session_id: Arc::new(session_id),
            sender_key: Arc::new(their_identity_key.to_owned()),
            creation_time: Arc::new(now),
            last_use_time: Arc::new(now),
        })
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
    /// * `key_map` - A map from the algorithm and device id to the one-time
    ///     key that the other account created and shared with us.
    pub(crate) async fn create_outbound_session(
        &self,
        device: Device,
        key_map: &BTreeMap<AlgorithmAndDeviceId, OneTimeKey>,
    ) -> Result<Session, SessionCreationError> {
        let one_time_key = key_map.values().next().ok_or_else(|| {
            SessionCreationError::OneTimeKeyMissing(
                device.user_id().to_owned(),
                device.device_id().into(),
            )
        })?;

        let one_time_key = match one_time_key {
            OneTimeKey::SignedKey(k) => k,
            OneTimeKey::Key(_) => {
                return Err(SessionCreationError::OneTimeKeyNotSigned(
                    device.user_id().to_owned(),
                    device.device_id().into(),
                ));
            }
        };

        device.verify_one_time_key(&one_time_key).map_err(|e| {
            SessionCreationError::InvalidSignature(
                device.user_id().to_owned(),
                device.device_id().into(),
                e,
            )
        })?;

        let curve_key = device.get_key(KeyAlgorithm::Curve25519).ok_or_else(|| {
            SessionCreationError::DeviceMissingCurveKey(
                device.user_id().to_owned(),
                device.device_id().into(),
            )
        })?;

        self.create_outbound_session_helper(curve_key, &one_time_key)
            .await
            .map_err(|e| {
                SessionCreationError::OlmError(
                    device.user_id().to_owned(),
                    device.device_id().into(),
                    e,
                )
            })
    }

    /// Create a new session with another account given a pre-key Olm message.
    ///
    /// Returns the newly created session or a `OlmSessionError` if creating a
    /// session failed.
    ///
    /// # Arguments
    /// * `their_identity_key` - The other account's identitiy/curve25519 key.
    ///
    /// * `message` - A pre-key Olm message that was sent to us by the other
    /// account.
    pub(crate) async fn create_inbound_session(
        &self,
        their_identity_key: &str,
        message: PreKeyMessage,
    ) -> Result<Session, OlmSessionError> {
        let session = self
            .inner
            .lock()
            .await
            .create_inbound_session_from(their_identity_key, message)?;

        self.inner
            .lock()
            .await
            .remove_one_time_keys(&session)
            .expect(
            "Session was successfully created but the account doesn't hold a matching one-time key",
        );

        let now = Instant::now();
        let session_id = session.session_id();

        Ok(Session {
            user_id: self.user_id.clone(),
            device_id: self.device_id.clone(),
            our_identity_keys: self.identity_keys.clone(),
            inner: Arc::new(Mutex::new(session)),
            session_id: Arc::new(session_id),
            sender_key: Arc::new(their_identity_key.to_owned()),
            creation_time: Arc::new(now),
            last_use_time: Arc::new(now),
        })
    }

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
    pub(crate) async fn create_group_session_pair(
        &self,
        room_id: &RoomId,
    ) -> (OutboundGroupSession, InboundGroupSession) {
        let outbound =
            OutboundGroupSession::new(self.device_id.clone(), self.identity_keys.clone(), room_id);
        let identity_keys = self.identity_keys();

        let sender_key = identity_keys.curve25519();
        let signing_key = identity_keys.ed25519();

        let inbound = InboundGroupSession::new(
            sender_key,
            signing_key,
            &room_id,
            outbound.session_key().await,
        )
        .expect("Can't create inbound group session from a newly created outbound group session");

        (outbound, inbound)
    }
}

impl PartialEq for Account {
    fn eq(&self, other: &Self) -> bool {
        self.identity_keys() == other.identity_keys() && self.shared() == other.shared()
    }
}
