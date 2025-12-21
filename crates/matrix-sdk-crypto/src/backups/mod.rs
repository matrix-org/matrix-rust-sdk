// Copyright 2021, 2022 The Matrix.org Foundation C.I.C.
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

//! Server-side backup support for room keys
//!
//! This module largely implements support for server-side backups using the
//! `m.megolm_backup.v1.curve25519-aes-sha2` backup algorithm.
//!
//! Due to various flaws in this backup algorithm it is **not** recommended to
//! use this module or any of its functionality. The module is only provided for
//! backwards compatibility.
//!
//! [spec]: https://spec.matrix.org/unstable/client-server-api/#server-side-key-backups

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use ruma::{
    DeviceId, DeviceKeyAlgorithm, OwnedDeviceId, OwnedRoomId, OwnedTransactionId, RoomId,
    TransactionId, api::client::backup::RoomKeyBackup, serde::Raw,
};
use tokio::sync::RwLock;
use tracing::{debug, info, instrument, trace, warn};

use crate::{
    CryptoStoreError, Device, RoomKeyImportResult, SignatureError,
    olm::{BackedUpRoomKey, ExportedRoomKey, InboundGroupSession, SignedJsonObject},
    store::{
        Store,
        types::{BackupDecryptionKey, BackupKeys, Changes, RoomKeyCounts},
    },
    types::{MegolmV1AuthData, RoomKeyBackupInfo, Signatures, requests::KeysBackupRequest},
};

mod keys;

pub use keys::{DecodeError, DecryptionError, MegolmV1BackupKey};

/// A state machine that handles backing up room keys.
///
/// The state machine can be activated using the
/// [`BackupMachine::enable_backup_v1`] method. After the state machine has been
/// enabled a request that will upload encrypted room keys can be generated
/// using the [`BackupMachine::backup`] method.
#[derive(Debug, Clone)]
pub struct BackupMachine {
    store: Store,
    backup_key: Arc<RwLock<Option<MegolmV1BackupKey>>>,
    pending_backup: Arc<RwLock<Option<PendingBackup>>>,
}

type SenderKey = String;
type SessionId = String;

#[derive(Debug, Clone)]
struct PendingBackup {
    request_id: OwnedTransactionId,
    request: KeysBackupRequest,
    sessions: BTreeMap<OwnedRoomId, BTreeMap<SenderKey, BTreeSet<SessionId>>>,
}

/// The result of a signature verification of a signed JSON object.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SignatureVerification {
    /// The result of the signature verification using the public key of our own
    /// device.
    pub device_signature: SignatureState,
    /// The result of the signature verification using the public key of our own
    /// user identity.
    pub user_identity_signature: SignatureState,
    /// The result of the signature verification using public keys of other
    /// devices we own.
    pub other_signatures: BTreeMap<OwnedDeviceId, SignatureState>,
}

impl SignatureVerification {
    /// Is the result considered to be trusted?
    ///
    /// This tells us if the result has a valid signature from any of the
    /// following:
    ///
    /// * Our own device
    /// * Our own user identity, provided the identity is trusted as well
    /// * Any of our own devices, provided the device is trusted as well
    pub fn trusted(&self) -> bool {
        self.device_signature.trusted()
            || self.user_identity_signature.trusted()
            || self.other_signatures.values().any(|s| s.trusted())
    }
}

/// The result of a signature check.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum SignatureState {
    /// The signature is missing.
    #[default]
    Missing,
    /// The signature is invalid.
    Invalid,
    /// The signature is valid but the device or user identity that created the
    /// signature is not trusted.
    ValidButNotTrusted,
    /// The signature is valid and the device or user identity that created the
    /// signature is trusted.
    ValidAndTrusted,
}

impl SignatureState {
    /// Is the state considered to be trusted?
    pub fn trusted(self) -> bool {
        self == SignatureState::ValidAndTrusted
    }

    /// Did we find a valid signature?
    pub fn signed(self) -> bool {
        self == SignatureState::ValidButNotTrusted && self == SignatureState::ValidAndTrusted
    }
}

impl BackupMachine {
    const BACKUP_BATCH_SIZE: usize = 100;

    pub(crate) fn new(store: Store, backup_key: Option<MegolmV1BackupKey>) -> Self {
        Self {
            store,
            backup_key: RwLock::new(backup_key).into(),
            pending_backup: RwLock::new(None).into(),
        }
    }

    /// Are we able to back up room keys to the server?
    pub async fn enabled(&self) -> bool {
        self.backup_key.read().await.as_ref().is_some_and(|b| b.backup_version().is_some())
    }

    /// Check if our own device has signed the given signed JSON payload.
    fn check_own_device_signature(
        &self,
        signatures: &Signatures,
        auth_data: &str,
    ) -> SignatureState {
        match self.store.static_account().has_signed_raw(signatures, auth_data) {
            Ok(_) => SignatureState::ValidAndTrusted,
            Err(e) => match e {
                SignatureError::NoSignatureFound => SignatureState::Missing,
                _ => SignatureState::Invalid,
            },
        }
    }

    /// Check if our own cross-signing user identity has signed the given signed
    /// JSON payload.
    async fn check_own_identity_signature(
        &self,
        signatures: &Signatures,
        auth_data: &str,
    ) -> Result<SignatureState, CryptoStoreError> {
        let user_id = &self.store.static_account().user_id;
        let identity = self.store.get_identity(user_id).await?;

        let ret = if let Some(identity) = identity.and_then(|i| i.own()) {
            match identity.master_key().has_signed_raw(signatures, auth_data) {
                Ok(_) => {
                    if identity.is_verified() {
                        SignatureState::ValidAndTrusted
                    } else {
                        SignatureState::ValidButNotTrusted
                    }
                }
                Err(e) => match e {
                    SignatureError::NoSignatureFound => SignatureState::Missing,
                    _ => SignatureState::Invalid,
                },
            }
        } else {
            SignatureState::Missing
        };

        Ok(ret)
    }

    /// Check if the signed JSON payload `auth_data` has been signed by the
    /// `device`.
    fn backup_signed_by_device(
        &self,
        device: Device,
        signatures: &Signatures,
        auth_data: &str,
    ) -> SignatureState {
        if device.has_signed_raw(signatures, auth_data).is_ok() {
            if device.is_verified() {
                SignatureState::ValidAndTrusted
            } else {
                SignatureState::ValidButNotTrusted
            }
        } else {
            SignatureState::Invalid
        }
    }

    /// Check if the signed JSON payload `auth_data` has been signed by any of
    /// our devices.
    async fn test_device_signatures(
        &self,
        signatures: &Signatures,
        auth_data: &str,
        compute_all_signatures: bool,
    ) -> Result<BTreeMap<OwnedDeviceId, SignatureState>, CryptoStoreError> {
        let mut result = BTreeMap::new();

        if let Some(user_signatures) = signatures.get(&self.store.static_account().user_id) {
            for device_key_id in user_signatures.keys() {
                if device_key_id.algorithm() == DeviceKeyAlgorithm::Ed25519 {
                    // No need to check our own device here, we're doing that using
                    // the check_own_device_signature().
                    if device_key_id.key_name() == self.store.static_account().device_id {
                        continue;
                    }

                    let state = self
                        .test_ed25519_device_signature(
                            device_key_id.key_name(),
                            signatures,
                            auth_data,
                        )
                        .await?;

                    result.insert(device_key_id.key_name().to_owned(), state);

                    // Abort the loop if we found a trusted and valid signature,
                    // unless we should check all of them.
                    if state.trusted() && !compute_all_signatures {
                        break;
                    }
                }
            }
        }

        Ok(result)
    }

    async fn test_ed25519_device_signature(
        &self,
        device_id: &DeviceId,
        signatures: &Signatures,
        auth_data: &str,
    ) -> Result<SignatureState, CryptoStoreError> {
        // We might iterate over some non-device signatures as well, but in this
        // case there's no corresponding device and we get `Ok(None)` here, so
        // things still work out.
        let device = self.store.get_device(self.store.user_id(), device_id).await?;
        trace!(?device_id, "Checking backup auth data for device");

        if let Some(device) = device {
            Ok(self.backup_signed_by_device(device, signatures, auth_data))
        } else {
            trace!(?device_id, "Device not found, can't check signature");
            Ok(SignatureState::Missing)
        }
    }

    async fn verify_auth_data_v1(
        &self,
        auth_data: MegolmV1AuthData,
        compute_all_signatures: bool,
    ) -> Result<SignatureVerification, CryptoStoreError> {
        let serialized_auth_data = match auth_data.to_canonical_json() {
            Ok(s) => s,
            Err(e) => {
                warn!(error =? e, "Error while verifying backup, can't canonicalize auth data");
                return Ok(Default::default());
            }
        };

        // Check if there's a signature from our own device.
        let device_signature =
            self.check_own_device_signature(&auth_data.signatures, &serialized_auth_data);
        // Check if there's a signature from our own user identity.
        let user_identity_signature =
            self.check_own_identity_signature(&auth_data.signatures, &serialized_auth_data).await?;

        // Collect all the other signatures if there isn't already a valid one,
        // or if we're told to collect all of them anyways.
        let other_signatures = if !(device_signature.trusted() || user_identity_signature.trusted())
            || compute_all_signatures
        {
            self.test_device_signatures(
                &auth_data.signatures,
                &serialized_auth_data,
                compute_all_signatures,
            )
            .await?
        } else {
            Default::default()
        };

        Ok(SignatureVerification { device_signature, user_identity_signature, other_signatures })
    }

    /// Verify some backup info that we downloaded from the server.
    ///
    /// # Arguments
    ///
    /// * `backup_info`: The backup info that should be verified. Should be
    ///   fetched from the server using the [`/room_keys/version`] endpoint.
    ///
    /// * `compute_all_signatures`: *Useful for debugging only*. If this
    ///   parameter is `true`, the internal machinery will compute the trust
    ///   state for all signatures before returning, instead of short-circuiting
    ///   on the first trusted signature. Has no impact on whether the backup
    ///   will be considered verified.
    ///
    /// [`/room_keys/version`]: https://spec.matrix.org/unstable/client-server-api/#get_matrixclientv3room_keysversion
    pub async fn verify_backup(
        &self,
        backup_info: RoomKeyBackupInfo,
        compute_all_signatures: bool,
    ) -> Result<SignatureVerification, CryptoStoreError> {
        trace!(?backup_info, "Verifying backup auth data");

        if let RoomKeyBackupInfo::MegolmBackupV1Curve25519AesSha2(data) = backup_info {
            self.verify_auth_data_v1(data, compute_all_signatures).await
        } else {
            Ok(Default::default())
        }
    }

    /// Sign a [`RoomKeyBackupInfo`] using the device's identity key and, if
    /// available, the cross-signing master key.
    ///
    /// # Arguments
    ///
    /// * `backup_info`: The backup version that should be verified. Should be
    ///   created from the [`BackupDecryptionKey`] using the
    ///   [`BackupDecryptionKey::to_backup_info()`] method.
    pub async fn sign_backup(
        &self,
        backup_info: &mut RoomKeyBackupInfo,
    ) -> Result<(), SignatureError> {
        if let RoomKeyBackupInfo::MegolmBackupV1Curve25519AesSha2(data) = backup_info {
            let canonical_json = data.to_canonical_json()?;

            let private_identity = self.store.private_identity();
            let identity = private_identity.lock().await;

            if let Some(key_id) = identity.master_key_id().await
                && let Ok(signature) = identity.sign(&canonical_json).await
            {
                data.signatures.add_signature(self.store.user_id().to_owned(), key_id, signature);
            }

            let cache = self.store.cache().await?;
            let account = cache.account().await?;
            let key_id = account.signing_key_id();
            let signature = account.sign(&canonical_json);
            data.signatures.add_signature(self.store.user_id().to_owned(), key_id, signature);

            Ok(())
        } else {
            Err(SignatureError::UnsupportedAlgorithm)
        }
    }

    /// Activate the given backup key to be used to encrypt and backup room
    /// keys.
    ///
    /// This will use the [`m.megolm_backup.v1.curve25519-aes-sha2`] algorithm
    /// to encrypt the room keys.
    ///
    /// [`m.megolm_backup.v1.curve25519-aes-sha2`]:
    /// https://spec.matrix.org/unstable/client-server-api/#backup-algorithm-mmegolm_backupv1curve25519-aes-sha2
    pub async fn enable_backup_v1(&self, key: MegolmV1BackupKey) -> Result<(), CryptoStoreError> {
        if key.backup_version().is_some() {
            *self.backup_key.write().await = Some(key.clone());
            info!(backup_key = ?key, "Activated a backup");
        } else {
            warn!(backup_key = ?key, "Tried to activate a backup without having the backup key uploaded");
        }

        Ok(())
    }

    /// Get the number of backed up room keys and the total number of room keys.
    pub async fn room_key_counts(&self) -> Result<RoomKeyCounts, CryptoStoreError> {
        let backup_version = self.backup_key.read().await.as_ref().and_then(|k| k.backup_version());
        self.store.inbound_group_session_counts(backup_version.as_deref()).await
    }

    /// Disable and reset our backup state.
    ///
    /// This will remove any pending backup request, remove the backup key and
    /// reset the backup state of each room key we have.
    #[instrument(skip(self))]
    pub async fn disable_backup(&self) -> Result<(), CryptoStoreError> {
        debug!("Disabling key backup and resetting backup state for room keys");

        self.backup_key.write().await.take();
        self.pending_backup.write().await.take();

        self.store.reset_backup_state().await?;

        debug!("Done disabling backup");

        Ok(())
    }

    /// Provide the `backup_version` of the current `backup_key`, or None if
    /// there is no current key, or the key is not used with any backup
    /// version.
    pub async fn backup_version(&self) -> Option<String> {
        self.backup_key.read().await.as_ref().and_then(|k| k.backup_version())
    }

    /// Store the backup decryption key in the crypto store.
    ///
    /// This is useful if the client wants to support gossiping of the backup
    /// key.
    pub async fn save_decryption_key(
        &self,
        backup_decryption_key: Option<BackupDecryptionKey>,
        version: Option<String>,
    ) -> Result<(), CryptoStoreError> {
        let changes =
            Changes { backup_decryption_key, backup_version: version, ..Default::default() };
        self.store.save_changes(changes).await
    }

    /// Get the backup keys we have saved in our crypto store.
    pub async fn get_backup_keys(&self) -> Result<BackupKeys, CryptoStoreError> {
        self.store.load_backup_keys().await
    }

    /// Encrypt a batch of room keys and return a request that needs to be sent
    /// out to backup the room keys.
    pub async fn backup(
        &self,
    ) -> Result<Option<(OwnedTransactionId, KeysBackupRequest)>, CryptoStoreError> {
        let mut request = self.pending_backup.write().await;

        if let Some(request) = &*request {
            trace!("Backing up, returning an existing request");

            Ok(Some((request.request_id.clone(), request.request.clone())))
        } else {
            trace!("Backing up, creating a new request");

            let new_request = self.backup_helper().await?;
            *request = new_request.clone();

            Ok(new_request.map(|r| (r.request_id, r.request)))
        }
    }

    pub(crate) async fn mark_request_as_sent(
        &self,
        request_id: &TransactionId,
    ) -> Result<(), CryptoStoreError> {
        let mut request = self.pending_backup.write().await;
        if let Some(r) = &*request {
            if r.request_id == request_id {
                let room_and_session_ids: Vec<(&RoomId, &str)> = r
                    .sessions
                    .iter()
                    .flat_map(|(room_id, sender_key_to_session_ids)| {
                        std::iter::repeat(room_id).zip(sender_key_to_session_ids.values().flatten())
                    })
                    .map(|(room_id, session_id)| (room_id.as_ref(), session_id.as_str()))
                    .collect();

                trace!(request_id = ?r.request_id, keys = ?r.sessions, "Marking room keys as backed up");

                self.store
                    .mark_inbound_group_sessions_as_backed_up(
                        &r.request.version,
                        &room_and_session_ids,
                    )
                    .await?;

                trace!(
                    request_id = ?r.request_id,
                    keys = ?r.sessions,
                    "Marked room keys as backed up"
                );

                *request = None;
            } else {
                warn!(
                    expected = ?r.request_id,
                    got = ?request_id,
                    "Tried to mark a pending backup as sent but the request id didn't match"
                );
            }
        } else {
            warn!(
                ?request_id,
                "Tried to mark a pending backup as sent but there isn't a backup pending"
            );
        }

        Ok(())
    }

    async fn backup_helper(&self) -> Result<Option<PendingBackup>, CryptoStoreError> {
        let Some(backup_key) = &*self.backup_key.read().await else {
            warn!("Trying to backup room keys but no backup key was found");
            return Ok(None);
        };

        let Some(version) = backup_key.backup_version() else {
            warn!("Trying to backup room keys but the backup key wasn't uploaded");
            return Ok(None);
        };

        let sessions =
            self.store.inbound_group_sessions_for_backup(&version, Self::BACKUP_BATCH_SIZE).await?;

        if sessions.is_empty() {
            trace!(?backup_key, "No room keys need to be backed up");
            return Ok(None);
        }

        let key_count = sessions.len();
        let (backup, session_record) = Self::backup_keys(sessions, backup_key).await;

        info!(
            key_count = key_count,
            keys = ?session_record,
            ?backup_key,
            "Successfully created a room keys backup request"
        );

        let request = PendingBackup {
            request_id: TransactionId::new(),
            request: KeysBackupRequest { version, rooms: backup },
            sessions: session_record,
        };

        Ok(Some(request))
    }

    /// Backup all the non-backed up room keys we know about
    async fn backup_keys(
        sessions: Vec<InboundGroupSession>,
        backup_key: &MegolmV1BackupKey,
    ) -> (
        BTreeMap<OwnedRoomId, RoomKeyBackup>,
        BTreeMap<OwnedRoomId, BTreeMap<SenderKey, BTreeSet<SessionId>>>,
    ) {
        let mut backup: BTreeMap<OwnedRoomId, RoomKeyBackup> = BTreeMap::new();
        let mut session_record: BTreeMap<OwnedRoomId, BTreeMap<SenderKey, BTreeSet<SessionId>>> =
            BTreeMap::new();

        for session in sessions {
            let room_id = session.room_id().to_owned();
            let session_id = session.session_id().to_owned();
            let sender_key = session.sender_key().to_owned();
            let session = backup_key.encrypt(session).await;

            session_record
                .entry(room_id.to_owned())
                .or_default()
                .entry(sender_key.to_base64())
                .or_default()
                .insert(session_id.clone());

            let session = Raw::new(&session).expect("Can't serialize a backed up room key");

            backup
                .entry(room_id)
                .or_insert_with(|| RoomKeyBackup::new(BTreeMap::new()))
                .sessions
                .insert(session_id, session);
        }

        (backup, session_record)
    }

    /// Import the given room keys into our store.
    ///
    /// # Arguments
    ///
    /// * `room_keys` - A list of previously exported keys that should be
    ///   imported into our store. If we already have a better version of a key
    ///   the key will *not* be imported.
    ///
    /// Returns a [`RoomKeyImportResult`] containing information about room keys
    /// which were imported.
    #[deprecated(note = "Use the OlmMachine::store::import_room_keys method instead")]
    pub async fn import_backed_up_room_keys(
        &self,
        room_keys: BTreeMap<OwnedRoomId, BTreeMap<String, BackedUpRoomKey>>,
        progress_listener: impl Fn(usize, usize),
    ) -> Result<RoomKeyImportResult, CryptoStoreError> {
        let mut decrypted_room_keys = vec![];

        for (room_id, room_keys) in room_keys {
            for (session_id, room_key) in room_keys {
                let room_key = ExportedRoomKey::from_backed_up_room_key(
                    room_id.to_owned(),
                    session_id,
                    room_key,
                );

                decrypted_room_keys.push(room_key);
            }
        }

        // FIXME: This method is a bit flawed: we have no real idea which backup version
        //   these keys came from. For example, we might have reset the backup
        //   since the keys were downloaded. For now, let's assume they came from
        //   the "current" backup version.
        let backup_version = self.backup_version().await;

        self.store
            .import_room_keys(decrypted_room_keys, backup_version.as_deref(), progress_listener)
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use assert_matches2::assert_let;
    use matrix_sdk_test::async_test;
    use ruma::{CanonicalJsonValue, DeviceId, RoomId, UserId, device_id, room_id, user_id};
    use serde_json::json;

    use super::BackupMachine;
    use crate::{
        OlmError, OlmMachine,
        olm::BackedUpRoomKey,
        store::{
            CryptoStore, MemoryStore,
            types::{BackupDecryptionKey, Changes},
        },
        types::RoomKeyBackupInfo,
    };

    fn room_key() -> BackedUpRoomKey {
        let json = json!({
            "algorithm": "m.megolm.v1.aes-sha2",
            "sender_key": "DeHIg4gwhClxzFYcmNntPNF9YtsdZbmMy8+3kzCMXHA",
            "session_key": "AQAAAABvWMNZjKFtebYIePKieQguozuoLgzeY6wKcyJjLJcJtQgy1dPqTBD12U+XrYLrRHn\
                            lKmxoozlhFqJl456+9hlHCL+yq+6ScFuBHtJepnY1l2bdLb4T0JMDkNsNErkiLiLnD6yp3J\
                            DSjIhkdHxmup/huygrmroq6/L5TaThEoqvW4DPIuO14btKudsS34FF82pwjKS4p6Mlch+0e\
                            fHAblQV",
            "sender_claimed_keys":{},
            "forwarding_curve25519_key_chain":[]
        });

        serde_json::from_value(json)
            .expect("We should be able to deserialize our backed up room key")
    }

    fn alice_id() -> &'static UserId {
        user_id!("@alice:example.org")
    }

    fn alice_device_id() -> &'static DeviceId {
        device_id!("JLAFKJWSCS")
    }

    fn room_id() -> &'static RoomId {
        room_id!("!test:localhost")
    }

    fn room_id2() -> &'static RoomId {
        room_id!("!test2:localhost")
    }

    async fn backup_flow(machine: OlmMachine) -> Result<(), OlmError> {
        let backup_machine = machine.backup_machine();
        let backup_version = current_backup_version(backup_machine).await;

        let counts =
            backup_machine.store.inbound_group_session_counts(backup_version.as_deref()).await?;

        assert_eq!(counts.total, 0, "Initially no keys exist");
        assert_eq!(counts.backed_up, 0, "Initially no backed up keys exist");

        machine.create_outbound_group_session_with_defaults_test_helper(room_id()).await?;
        machine.create_outbound_group_session_with_defaults_test_helper(room_id2()).await?;

        let counts =
            backup_machine.store.inbound_group_session_counts(backup_version.as_deref()).await?;
        assert_eq!(counts.total, 2, "Two room keys need to exist in the store");
        assert_eq!(counts.backed_up, 0, "No room keys have been backed up yet");

        let decryption_key = BackupDecryptionKey::new().expect("Can't create new recovery key");
        let backup_key = decryption_key.megolm_v1_public_key();
        backup_key.set_version("1".to_owned());

        backup_machine.enable_backup_v1(backup_key).await?;

        let (request_id, _) =
            backup_machine.backup().await?.expect("Created a backup request successfully");
        assert_eq!(
            Some(&request_id),
            backup_machine.backup().await?.as_ref().map(|(request_id, _)| request_id),
            "Calling backup again without uploading creates the same backup request"
        );

        backup_machine.mark_request_as_sent(&request_id).await?;
        let backup_version = current_backup_version(backup_machine).await;

        let counts =
            backup_machine.store.inbound_group_session_counts(backup_version.as_deref()).await?;
        assert_eq!(counts.total, 2);
        assert_eq!(counts.backed_up, 2, "All room keys have been backed up");

        assert!(
            backup_machine.backup().await?.is_none(),
            "No room keys need to be backed up, no request needs to be created"
        );

        backup_machine.disable_backup().await?;
        let backup_version = current_backup_version(backup_machine).await;

        let counts =
            backup_machine.store.inbound_group_session_counts(backup_version.as_deref()).await?;
        assert_eq!(counts.total, 2);
        assert_eq!(
            counts.backed_up, 0,
            "Disabling the backup resets the backup flag on the room keys"
        );

        Ok(())
    }

    async fn current_backup_version(backup_machine: &BackupMachine) -> Option<String> {
        backup_machine.backup_key.read().await.as_ref().and_then(|k| k.backup_version())
    }

    #[async_test]
    async fn test_memory_store_backups() -> Result<(), OlmError> {
        let machine = OlmMachine::new(alice_id(), alice_device_id()).await;

        backup_flow(machine).await
    }

    #[async_test]
    async fn test_verify_auth_data() -> Result<(), OlmError> {
        let machine = OlmMachine::new(alice_id(), alice_device_id()).await;
        let backup_machine = machine.backup_machine();

        let auth_data = json!({
            "public_key":"XjhWTCjW7l59pbfx9tlCBQolfnIQWARoKOzjTOPSlWM",
        });

        let backup_version = json!({
            "algorithm": "m.megolm_backup.v1.curve25519-aes-sha2",
            "auth_data": auth_data,
        });

        let canonical_json: CanonicalJsonValue =
            auth_data.clone().try_into().expect("Canonicalizing should always work");
        let serialized = canonical_json.to_string();

        let backup_version: RoomKeyBackupInfo = serde_json::from_value(backup_version).unwrap();

        let state = backup_machine
            .verify_backup(backup_version, false)
            .await
            .expect("Verifying should work");
        assert!(!state.trusted());
        assert!(!state.device_signature.trusted());
        assert!(!state.user_identity_signature.trusted());
        assert!(!state.other_signatures.values().any(|s| s.trusted()));

        let signatures = machine.sign(&serialized).await?;

        let backup_version = json!({
            "algorithm": "m.megolm_backup.v1.curve25519-aes-sha2",
            "auth_data": {
                "public_key":"XjhWTCjW7l59pbfx9tlCBQolfnIQWARoKOzjTOPSlWM",
                "signatures": signatures,
            }
        });
        let backup_version: RoomKeyBackupInfo = serde_json::from_value(backup_version).unwrap();

        let state = backup_machine
            .verify_backup(backup_version, false)
            .await
            .expect("Verifying should work");

        assert!(state.trusted());
        assert!(state.device_signature.trusted());
        assert!(!state.user_identity_signature.trusted());
        assert!(!state.other_signatures.values().any(|s| s.trusted()));

        machine
            .bootstrap_cross_signing(true)
            .await
            .expect("Bootstrapping a new identity always works");

        let signatures = machine.sign(&serialized).await?;

        let backup_version = json!({
            "algorithm": "m.megolm_backup.v1.curve25519-aes-sha2",
            "auth_data": {
                "public_key":"XjhWTCjW7l59pbfx9tlCBQolfnIQWARoKOzjTOPSlWM",
                "signatures": signatures,
            }
        });
        let backup_version: RoomKeyBackupInfo = serde_json::from_value(backup_version).unwrap();

        let state = backup_machine
            .verify_backup(backup_version, false)
            .await
            .expect("Verifying should work");

        assert!(state.trusted());
        assert!(state.device_signature.trusted());
        assert!(state.user_identity_signature.trusted());
        assert!(!state.other_signatures.values().any(|s| s.trusted()));

        Ok(())
    }

    #[async_test]
    async fn test_import_backed_up_room_keys() {
        let machine = OlmMachine::new(alice_id(), alice_device_id()).await;
        let backup_machine = machine.backup_machine();

        // We set up a backup key, so that we can test `backup_machine.backup()` later.
        let decryption_key = BackupDecryptionKey::new().expect("Couldn't create new recovery key");
        let backup_key = decryption_key.megolm_v1_public_key();
        backup_key.set_version("1".to_owned());
        backup_machine.enable_backup_v1(backup_key).await.expect("Couldn't enable backup");

        let room_id = room_id!("!DovneieKSTkdHKpIXy:morpheus.localhost");
        let session_id = "gM8i47Xhu0q52xLfgUXzanCMpLinoyVyH7R58cBuVBU";
        let room_key = room_key();

        let room_keys: BTreeMap<_, BTreeMap<_, _>> = BTreeMap::from([(
            room_id.to_owned(),
            BTreeMap::from([(session_id.to_owned(), room_key)]),
        )]);

        let session = machine.store().get_inbound_group_session(room_id, session_id).await.unwrap();

        assert!(session.is_none(), "Initially we should not have the session in the store");

        #[allow(deprecated)]
        backup_machine
            .import_backed_up_room_keys(room_keys, |_, _| {})
            .await
            .expect("We should be able to import a room key");

        // Now check that the session was correctly imported, and that it is marked as
        // backed up
        let session = machine.store().get_inbound_group_session(room_id, session_id).await.unwrap();
        assert_let!(Some(session) = session);
        assert!(
            session.backed_up(),
            "If a session was imported from a backup, it should be considered to be backed up"
        );
        assert!(session.has_been_imported());

        // Also check that it is not returned by a backup request.
        let backup_request =
            backup_machine.backup().await.expect("We should be able to create a backup request");
        assert!(
            backup_request.is_none(),
            "If a session was imported from backup, it should not be backed up again."
        );
    }

    #[async_test]
    async fn test_sign_backup_info() {
        let machine = OlmMachine::new(alice_id(), alice_device_id()).await;
        let backup_machine = machine.backup_machine();

        let decryption_key = BackupDecryptionKey::new().unwrap();
        let mut backup_info = decryption_key.to_backup_info();

        let result = backup_machine.verify_backup(backup_info.to_owned(), false).await.unwrap();

        assert!(!result.trusted());

        backup_machine.sign_backup(&mut backup_info).await.unwrap();

        let result = backup_machine.verify_backup(backup_info, false).await.unwrap();

        assert!(result.trusted());
    }

    #[async_test]
    async fn test_fix_backup_key_mismatch() {
        let store = MemoryStore::new();

        let backup_decryption_key = BackupDecryptionKey::new().unwrap();

        store
            .save_changes(Changes {
                backup_decryption_key: Some(backup_decryption_key.clone()),
                backup_version: Some("1".to_owned()),
                ..Default::default()
            })
            .await
            .unwrap();

        // Create the machine using `with_store` and without a call to enable_backup_v1,
        // like regenerate_olm would do
        let alice =
            OlmMachine::with_store(alice_id(), alice_device_id(), store, None).await.unwrap();

        let binding = alice.backup_machine().backup_key.read().await;
        let machine_backup_key = binding.as_ref().unwrap();

        assert_eq!(
            machine_backup_key.to_base64(),
            backup_decryption_key.megolm_v1_public_key().to_base64(),
            "The OlmMachine loaded the wrong backup key."
        );
    }
}
