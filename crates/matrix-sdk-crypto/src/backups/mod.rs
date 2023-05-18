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
    api::client::backup::RoomKeyBackup, serde::Raw, DeviceId, DeviceKeyAlgorithm, OwnedDeviceId,
    OwnedRoomId, OwnedTransactionId, TransactionId,
};
use tokio::sync::RwLock;
use tracing::{debug, info, instrument, trace, warn};

use crate::{
    olm::{Account, InboundGroupSession, SignedJsonObject},
    store::{BackupKeys, Changes, RecoveryKey, RoomKeyCounts, Store},
    types::{MegolmV1AuthData, RoomKeyBackupInfo, Signatures},
    CryptoStoreError, Device, KeysBackupRequest, OutgoingRequest,
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
    account: Account,
    store: Store,
    backup_key: Arc<RwLock<Option<MegolmV1BackupKey>>>,
    pending_backup: Arc<RwLock<Option<PendingBackup>>>,
}

#[derive(Debug, Clone)]
struct PendingBackup {
    request_id: OwnedTransactionId,
    request: KeysBackupRequest,
    sessions: BTreeMap<OwnedRoomId, BTreeMap<String, BTreeSet<String>>>,
}

impl PendingBackup {
    fn session_was_part_of_the_backup(&self, session: &InboundGroupSession) -> bool {
        self.sessions
            .get(session.room_id())
            .and_then(|r| {
                r.get(&session.sender_key().to_base64()).map(|s| s.contains(session.session_id()))
            })
            .unwrap_or(false)
    }
}

impl From<PendingBackup> for OutgoingRequest {
    fn from(b: PendingBackup) -> Self {
        OutgoingRequest { request_id: b.request_id, request: Arc::new(b.request.into()) }
    }
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

    pub(crate) fn new(
        account: Account,
        store: Store,
        backup_key: Option<MegolmV1BackupKey>,
    ) -> Self {
        Self {
            account,
            store,
            backup_key: RwLock::new(backup_key).into(),
            pending_backup: RwLock::new(None).into(),
        }
    }

    /// Are we able to back up room keys to the server?
    pub async fn enabled(&self) -> bool {
        self.backup_key.read().await.as_ref().map(|b| b.backup_version().is_some()).unwrap_or(false)
    }

    /// Check if our own device has signed the given signed JSON payload.
    async fn check_own_device_signature(
        &self,
        signatures: &Signatures,
        auth_data: &str,
    ) -> SignatureState {
        match self.account.has_signed_raw(signatures, auth_data) {
            Ok(_) => SignatureState::ValidAndTrusted,
            Err(e) => match e {
                crate::SignatureError::NoSignatureFound => SignatureState::Missing,
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
        let user_id = self.account.user_id();
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
                    crate::SignatureError::NoSignatureFound => SignatureState::Missing,
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

        if let Some(user_signatures) = signatures.get(self.account.user_id()) {
            for device_key_id in user_signatures.keys() {
                if device_key_id.algorithm() == DeviceKeyAlgorithm::Ed25519 {
                    // No need to check our own device here, we're doing that using
                    // the check_own_device_signature().
                    if device_key_id.device_id() == self.account.device_id() {
                        continue;
                    }

                    let state = self
                        .test_ed25519_device_signature(
                            device_key_id.device_id(),
                            signatures,
                            auth_data,
                        )
                        .await?;

                    result.insert(device_key_id.device_id().to_owned(), state);

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
        trace!(?auth_data, "Verifying backup auth data");

        let serialized_auth_data = match auth_data.to_canonical_json() {
            Ok(s) => s,
            Err(e) => {
                warn!(error =? e, "Error while verifying backup, can't canonicalize auth data");
                return Ok(Default::default());
            }
        };

        // Check if there's a signature from our own device.
        let device_signature =
            self.check_own_device_signature(&auth_data.signatures, &serialized_auth_data).await;
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
    /// * `backup_version`: The backup version that should be verified. Should
    /// be fetched from the server using the [`/room_keys/version`] endpoint.
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
        self.store.inbound_group_session_counts().await
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

    /// Store the recovery key in the crypto store.
    ///
    /// This is useful if the client wants to support gossiping of the backup
    /// key.
    pub async fn save_recovery_key(
        &self,
        recovery_key: Option<RecoveryKey>,
        version: Option<String>,
    ) -> Result<(), CryptoStoreError> {
        let changes = Changes { recovery_key, backup_version: version, ..Default::default() };
        self.store.save_changes(changes).await
    }

    /// Get the backup keys we have saved in our crypto store.
    pub async fn get_backup_keys(&self) -> Result<BackupKeys, CryptoStoreError> {
        self.store.load_backup_keys().await
    }

    /// Encrypt a batch of room keys and return a request that needs to be sent
    /// out to backup the room keys.
    pub async fn backup(&self) -> Result<Option<OutgoingRequest>, CryptoStoreError> {
        let mut request = self.pending_backup.write().await;

        if let Some(request) = &*request {
            trace!("Backing up, returning an existing request");

            Ok(Some(request.clone().into()))
        } else {
            trace!("Backing up, creating a new request");

            let new_request = self.backup_helper().await?;
            *request = new_request.clone();

            Ok(new_request.map(|r| r.into()))
        }
    }

    pub(crate) async fn mark_request_as_sent(
        &self,
        request_id: &TransactionId,
    ) -> Result<(), CryptoStoreError> {
        let mut request = self.pending_backup.write().await;

        if let Some(r) = &*request {
            if r.request_id == request_id {
                let sessions: Vec<_> = self
                    .store
                    .get_inbound_group_sessions()
                    .await?
                    .into_iter()
                    .filter(|s| r.session_was_part_of_the_backup(s))
                    .collect();

                for session in &sessions {
                    session.mark_as_backed_up();
                }

                trace!(request_id = ?r.request_id, keys = ?r.sessions, "Marking room keys as backed up");

                let changes = Changes { inbound_group_sessions: sessions, ..Default::default() };
                self.store.save_changes(changes).await?;

                let counts = self.store.inbound_group_session_counts().await?;

                trace!(
                    room_key_counts = ?counts,
                    request_id = ?r.request_id, keys = ?r.sessions, "Marked room keys as backed up"
                );

                *request = None;
            } else {
                warn!(
                    expected = r.request_id.to_string().as_str(),
                    got = request_id.to_string().as_str(),
                    "Tried to mark a pending backup as sent but the request id didn't match"
                );
            }
        } else {
            warn!(
                request_id = request_id.to_string().as_str(),
                "Tried to mark a pending backup as sent but there isn't a backup pending"
            );
        };

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
            self.store.inbound_group_sessions_for_backup(Self::BACKUP_BATCH_SIZE).await?;

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
        BTreeMap<OwnedRoomId, BTreeMap<String, BTreeSet<String>>>,
    ) {
        let mut backup: BTreeMap<OwnedRoomId, RoomKeyBackup> = BTreeMap::new();
        let mut session_record: BTreeMap<OwnedRoomId, BTreeMap<String, BTreeSet<String>>> =
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
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::async_test;
    use ruma::{device_id, room_id, user_id, CanonicalJsonValue, DeviceId, RoomId, UserId};
    use serde_json::json;

    use crate::{store::RecoveryKey, types::RoomKeyBackupInfo, OlmError, OlmMachine};

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
        let counts = backup_machine.store.inbound_group_session_counts().await?;

        assert_eq!(counts.total, 0, "Initially no keys exist");
        assert_eq!(counts.backed_up, 0, "Initially no backed up keys exist");

        machine.create_outbound_group_session_with_defaults(room_id()).await?;
        machine.create_outbound_group_session_with_defaults(room_id2()).await?;

        let counts = backup_machine.store.inbound_group_session_counts().await?;
        assert_eq!(counts.total, 2, "Two room keys need to exist in the store");
        assert_eq!(counts.backed_up, 0, "No room keys have been backed up yet");

        let recovery_key = RecoveryKey::new().expect("Can't create new recovery key");
        let backup_key = recovery_key.megolm_v1_public_key();
        backup_key.set_version("1".to_owned());

        backup_machine.enable_backup_v1(backup_key).await?;

        let request =
            backup_machine.backup().await?.expect("Created a backup request successfully");
        assert_eq!(
            Some(&*request.request_id),
            backup_machine.backup().await?.as_ref().map(|r| &*r.request_id),
            "Calling backup again without uploading creates the same backup request"
        );

        backup_machine.mark_request_as_sent(&request.request_id).await?;

        let counts = backup_machine.store.inbound_group_session_counts().await?;
        assert_eq!(counts.total, 2);
        assert_eq!(counts.backed_up, 2, "All room keys have been backed up");

        assert!(
            backup_machine.backup().await?.is_none(),
            "No room keys need to be backed up, no request needs to be created"
        );

        backup_machine.disable_backup().await?;

        let counts = backup_machine.store.inbound_group_session_counts().await?;
        assert_eq!(counts.total, 2);
        assert_eq!(
            counts.backed_up, 0,
            "Disabling the backup resets the backup flag on the room keys"
        );

        Ok(())
    }

    #[async_test]
    async fn memory_store_backups() -> Result<(), OlmError> {
        let machine = OlmMachine::new(alice_id(), alice_device_id()).await;

        backup_flow(machine).await
    }

    #[async_test]
    async fn verify_auth_data() -> Result<(), OlmError> {
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

        let signatures = machine.sign(&serialized).await;

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

        let signatures = machine.sign(&serialized).await;

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
}
