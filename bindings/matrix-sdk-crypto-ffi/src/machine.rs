use std::{
    collections::{BTreeMap, HashMap},
    io::Cursor,
    mem::ManuallyDrop,
    ops::Deref,
    sync::Arc,
    time::Duration,
};

use js_int::UInt;
use matrix_sdk_common::deserialized_responses::AlgorithmInfo;
use matrix_sdk_crypto::{
    backups::{
        MegolmV1BackupKey as RustBackupKey, SignatureState,
        SignatureVerification as RustSignatureCheckResult,
    },
    decrypt_room_key_export, encrypt_room_key_export,
    olm::ExportedRoomKey,
    store::{BackupDecryptionKey, Changes},
    LocalTrust, OlmMachine as InnerMachine, UserIdentities,
};
use ruma::{
    api::{
        client::{
            backup::add_backup_keys::v3::Response as KeysBackupResponse,
            keys::{
                claim_keys::v3::Response as KeysClaimResponse,
                get_keys::v3::Response as KeysQueryResponse,
                upload_keys::v3::Response as KeysUploadResponse,
                upload_signatures::v3::Response as SignatureUploadResponse,
            },
            message::send_message_event::v3::Response as RoomMessageResponse,
            sync::sync_events::{v3::ToDevice, DeviceLists as RumaDeviceLists},
            to_device::send_event_to_device::v3::Response as ToDeviceResponse,
        },
        IncomingResponse,
    },
    events::{
        key::verification::VerificationMethod, room::message::MessageType, AnyMessageLikeEvent,
        AnySyncMessageLikeEvent, AnyTimelineEvent, MessageLikeEvent,
    },
    serde::Raw,
    DeviceKeyAlgorithm, EventId, OwnedTransactionId, OwnedUserId, RoomId, UserId,
};
use serde::{Deserialize, Serialize};
use serde_json::{value::RawValue, Value};
use tokio::runtime::Runtime;
use zeroize::Zeroize;

use crate::{
    error::{CryptoStoreError, DecryptionError, SecretImportError, SignatureError},
    parse_user_id,
    responses::{response_from_string, OwnedResponse},
    BackupKeys, BackupRecoveryKey, BootstrapCrossSigningResult, CrossSigningKeyExport,
    CrossSigningStatus, DecodeError, DecryptedEvent, Device, DeviceLists, EncryptionSettings,
    EventEncryptionAlgorithm, KeyImportError, KeysImportResult, MegolmV1BackupKey,
    ProgressListener, Request, RequestType, RequestVerificationResult, RoomKeyCounts, RoomSettings,
    Sas, SignatureUploadRequest, StartSasResult, UserIdentity, Verification, VerificationRequest,
};

/// The return value for the [`OlmMachine::receive_sync_changes()`] method.
///
/// Will contain various information about the `/sync` changes the
/// [`OlmMachine`] processed.
#[derive(uniffi::Record)]
pub struct SyncChangesResult {
    /// The, now possibly decrypted, to-device events the [`OlmMachine`]
    /// received, decrypted, and processed.
    to_device_events: Vec<String>,

    /// Information about the room keys that were extracted out of the to-device
    /// events.
    room_key_infos: Vec<RoomKeyInfo>,
}

/// Information on a room key that has been received or imported.
#[derive(uniffi::Record)]
pub struct RoomKeyInfo {
    /// The [messaging algorithm] that this key is used for. Will be one of the
    /// `m.megolm.*` algorithms.
    ///
    /// [messaging algorithm]: https://spec.matrix.org/v1.6/client-server-api/#messaging-algorithms
    pub algorithm: String,

    /// The room where the key is used.
    pub room_id: String,

    /// The Curve25519 key of the device which initiated the session originally.
    pub sender_key: String,

    /// The ID of the session that the key is for.
    pub session_id: String,
}

impl From<matrix_sdk_crypto::store::RoomKeyInfo> for RoomKeyInfo {
    fn from(value: matrix_sdk_crypto::store::RoomKeyInfo) -> Self {
        Self {
            algorithm: value.algorithm.to_string(),
            room_id: value.room_id.to_string(),
            sender_key: value.sender_key.to_base64(),
            session_id: value.session_id,
        }
    }
}

/// A high level state machine that handles E2EE for Matrix.
#[derive(uniffi::Object)]
pub struct OlmMachine {
    pub(crate) inner: ManuallyDrop<InnerMachine>,
    pub(crate) runtime: Runtime,
}

impl Drop for OlmMachine {
    fn drop(&mut self) {
        // SAFETY: self.inner is never used again, which is the only requirement
        //         for ManuallyDrop::take to be used safely.
        let inner = unsafe { ManuallyDrop::take(&mut self.inner) };
        let _guard = self.runtime.enter();
        // Dropping the inner OlmMachine must happen within a tokio context
        // because deadpool drops sqlite connections in the DB pool on tokio's
        // blocking threadpool to avoid blocking async worker threads.
        drop(inner);
    }
}

/// A pair of outgoing room key requests, both of those are sendToDevice
/// requests.
#[derive(uniffi::Record)]
pub struct KeyRequestPair {
    /// The optional cancellation, this is None if no previous key request was
    /// sent out for this key, thus it doesn't need to be cancelled.
    pub cancellation: Option<Request>,
    /// The actual key request.
    pub key_request: Request,
}

/// The result of a signature verification of a signed JSON object.
#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SignatureVerification {
    /// The result of the signature verification using the public key of our own
    /// device.
    pub device_signature: SignatureState,
    /// The result of the signature verification using the public key of our own
    /// user identity.
    pub user_identity_signature: SignatureState,
    /// The result of the signature verification using public keys of other
    /// devices we own.
    pub other_devices_signatures: HashMap<String, SignatureState>,
    /// Is the signed JSON object trusted.
    ///
    /// This flag tells us if the result has a valid signature from any of the
    /// following:
    ///
    /// * Our own device
    /// * Our own user identity, provided the identity is trusted as well
    /// * Any of our own devices, provided the device is trusted as well
    pub trusted: bool,
}

impl From<RustSignatureCheckResult> for SignatureVerification {
    fn from(r: RustSignatureCheckResult) -> Self {
        let trusted = r.trusted();

        Self {
            device_signature: r.device_signature,
            user_identity_signature: r.user_identity_signature,
            other_devices_signatures: r
                .other_signatures
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            trusted,
        }
    }
}

#[uniffi::export]
impl OlmMachine {
    /// Create a new `OlmMachine`
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique ID of the user that owns this machine.
    ///
    /// * `device_id` - The unique ID of the device that owns this machine.
    ///
    /// * `path` - The path where the state of the machine should be persisted.
    ///
    /// * `passphrase` - The passphrase that should be used to encrypt the data
    ///   at rest in the crypto store. **Warning**, if no passphrase is given,
    ///   the store and all its data will remain unencrypted.
    #[uniffi::constructor]
    pub fn new(
        user_id: String,
        device_id: String,
        path: String,
        mut passphrase: Option<String>,
    ) -> Result<Arc<Self>, CryptoStoreError> {
        let user_id = parse_user_id(&user_id)?;
        let device_id = device_id.as_str().into();
        let runtime = Runtime::new().expect("Couldn't create a tokio runtime");

        let store = runtime
            .block_on(matrix_sdk_sqlite::SqliteCryptoStore::open(path, passphrase.as_deref()))?;

        passphrase.zeroize();

        let inner =
            runtime.block_on(InnerMachine::with_store(&user_id, device_id, Arc::new(store)))?;

        Ok(Arc::new(OlmMachine { inner: ManuallyDrop::new(inner), runtime }))
    }

    /// Get the user ID of the owner of this `OlmMachine`.
    pub fn user_id(&self) -> String {
        self.inner.user_id().to_string()
    }

    /// Get the device ID of the device of this `OlmMachine`.
    pub fn device_id(&self) -> String {
        self.inner.device_id().to_string()
    }

    /// Get our own identity keys.
    pub fn identity_keys(&self) -> HashMap<String, String> {
        let identity_keys = self.inner.identity_keys();
        let curve_key = identity_keys.curve25519.to_base64();
        let ed25519_key = identity_keys.ed25519.to_base64();

        HashMap::from([("ed25519".to_owned(), ed25519_key), ("curve25519".to_owned(), curve_key)])
    }

    /// Get the status of the private cross signing keys.
    ///
    /// This can be used to check which private cross signing keys we have
    /// stored locally.
    pub fn cross_signing_status(&self) -> CrossSigningStatus {
        self.runtime.block_on(self.inner.cross_signing_status()).into()
    }

    /// Get a cross signing user identity for the given user ID.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the identity belongs to
    ///
    /// * `timeout` - The time in seconds we should wait before returning if
    /// the user's device list has been marked as stale. Passing a 0 as the
    /// timeout means that we won't wait at all. **Note**, this assumes that
    /// the requests from [`OlmMachine::outgoing_requests`] are being processed
    /// and sent out. Namely, this waits for a `/keys/query` response to be
    /// received.
    pub fn get_identity(
        &self,
        user_id: String,
        timeout: u32,
    ) -> Result<Option<UserIdentity>, CryptoStoreError> {
        let user_id = parse_user_id(&user_id)?;

        let timeout = if timeout == 0 { None } else { Some(Duration::from_secs(timeout.into())) };

        Ok(
            if let Some(identity) =
                self.runtime.block_on(self.inner.get_identity(&user_id, timeout))?
            {
                Some(self.runtime.block_on(UserIdentity::from_rust(identity))?)
            } else {
                None
            },
        )
    }

    /// Check if a user identity is considered to be verified by us.
    pub fn is_identity_verified(&self, user_id: String) -> Result<bool, CryptoStoreError> {
        let user_id = parse_user_id(&user_id)?;

        Ok(
            if let Some(identity) =
                self.runtime.block_on(self.inner.get_identity(&user_id, None))?
            {
                match identity {
                    UserIdentities::Own(i) => i.is_verified(),
                    UserIdentities::Other(i) => i.is_verified(),
                }
            } else {
                false
            },
        )
    }

    /// Manually the user with the given user ID.
    ///
    /// This method will attempt to sign the user identity using either our
    /// private cross signing key, for other user identities, or our device keys
    /// for our own user identity.
    ///
    /// This method can fail if we don't have the private part of our
    /// user-signing key.
    ///
    /// Returns a request that needs to be sent out for the user identity to be
    /// marked as verified.
    pub fn verify_identity(
        &self,
        user_id: String,
    ) -> Result<SignatureUploadRequest, SignatureError> {
        let user_id = UserId::parse(user_id)?;

        let user_identity = self.runtime.block_on(self.inner.get_identity(&user_id, None))?;

        if let Some(user_identity) = user_identity {
            Ok(match user_identity {
                UserIdentities::Own(i) => self.runtime.block_on(i.verify())?,
                UserIdentities::Other(i) => self.runtime.block_on(i.verify())?,
            }
            .into())
        } else {
            Err(SignatureError::UnknownUserIdentity(user_id.to_string()))
        }
    }

    /// Get a `Device` from the store.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The id of the device owner.
    ///
    /// * `device_id` - The id of the device itself.
    ///
    /// * `timeout` - The time in seconds we should wait before returning if
    /// the user's device list has been marked as stale. Passing a 0 as the
    /// timeout means that we won't wait at all. **Note**, this assumes that
    /// the requests from [`OlmMachine::outgoing_requests`] are being processed
    /// and sent out. Namely, this waits for a `/keys/query` response to be
    /// received.
    pub fn get_device(
        &self,
        user_id: String,
        device_id: String,
        timeout: u32,
    ) -> Result<Option<Device>, CryptoStoreError> {
        let user_id = parse_user_id(&user_id)?;

        let timeout = if timeout == 0 { None } else { Some(Duration::from_secs(timeout.into())) };

        Ok(self
            .runtime
            .block_on(self.inner.get_device(&user_id, device_id.as_str().into(), timeout))?
            .map(|d| d.into()))
    }

    /// Manually the device of the given user with the given device ID.
    ///
    /// This method will attempt to sign the device using our private cross
    /// signing key.
    ///
    /// This method will always fail if the device belongs to someone else, we
    /// can only sign our own devices.
    ///
    /// It can also fail if we don't have the private part of our self-signing
    /// key.
    ///
    /// Returns a request that needs to be sent out for the device to be marked
    /// as verified.
    pub fn verify_device(
        &self,
        user_id: String,
        device_id: String,
    ) -> Result<SignatureUploadRequest, SignatureError> {
        let user_id = UserId::parse(user_id)?;
        let device = self.runtime.block_on(self.inner.get_device(
            &user_id,
            device_id.as_str().into(),
            None,
        ))?;

        if let Some(device) = device {
            Ok(self.runtime.block_on(device.verify())?.into())
        } else {
            Err(SignatureError::UnknownDevice(user_id, device_id))
        }
    }

    /// Set local trust state for the device of the given user without creating
    /// or uploading any signatures if verified
    pub fn set_local_trust(
        &self,
        user_id: String,
        device_id: String,
        trust_state: LocalTrust,
    ) -> Result<(), CryptoStoreError> {
        let user_id = parse_user_id(&user_id)?;

        let device = self.runtime.block_on(self.inner.get_device(
            &user_id,
            device_id.as_str().into(),
            None,
        ))?;

        if let Some(device) = device {
            self.runtime.block_on(device.set_local_trust(trust_state))?;
        }

        Ok(())
    }

    /// Get all devices of an user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The id of the device owner.
    ///
    /// * `timeout` - The time in seconds we should wait before returning if
    /// the user's device list has been marked as stale. Passing a 0 as the
    /// timeout means that we won't wait at all. **Note**, this assumes that
    /// the requests from [`OlmMachine::outgoing_requests`] are being processed
    /// and sent out. Namely, this waits for a `/keys/query` response to be
    /// received.
    pub fn get_user_devices(
        &self,
        user_id: String,
        timeout: u32,
    ) -> Result<Vec<Device>, CryptoStoreError> {
        let user_id = parse_user_id(&user_id)?;

        let timeout = if timeout == 0 { None } else { Some(Duration::from_secs(timeout.into())) };
        Ok(self
            .runtime
            .block_on(self.inner.get_user_devices(&user_id, timeout))?
            .devices()
            .map(|d| d.into())
            .collect())
    }

    /// Get the list of outgoing requests that need to be sent to the
    /// homeserver.
    ///
    /// After the request was sent out and a successful response was received
    /// the response body should be passed back to the state machine using the
    /// [mark_request_as_sent()](Self::mark_request_as_sent) method.
    ///
    /// **Note**: This method call should be locked per call.
    pub fn outgoing_requests(&self) -> Result<Vec<Request>, CryptoStoreError> {
        Ok(self
            .runtime
            .block_on(self.inner.outgoing_requests())?
            .into_iter()
            .map(|r| r.into())
            .collect())
    }

    /// Mark a request that was sent to the server as sent.
    ///
    /// # Arguments
    ///
    /// * `request_id` - The unique ID of the request that was sent out. This
    ///   needs to be an UUID.
    ///
    /// * `request_type` - The type of the request that was sent out.
    ///
    /// * `response_body` - The body of the response that was received.
    pub fn mark_request_as_sent(
        &self,
        request_id: String,
        request_type: RequestType,
        response_body: String,
    ) -> Result<(), CryptoStoreError> {
        let id: OwnedTransactionId = request_id.into();

        let response = response_from_string(&response_body);

        let response: OwnedResponse = match request_type {
            RequestType::KeysUpload => {
                KeysUploadResponse::try_from_http_response(response).map(Into::into)
            }
            RequestType::KeysQuery => {
                KeysQueryResponse::try_from_http_response(response).map(Into::into)
            }
            RequestType::ToDevice => {
                ToDeviceResponse::try_from_http_response(response).map(Into::into)
            }
            RequestType::KeysClaim => {
                KeysClaimResponse::try_from_http_response(response).map(Into::into)
            }
            RequestType::SignatureUpload => {
                SignatureUploadResponse::try_from_http_response(response).map(Into::into)
            }
            RequestType::KeysBackup => {
                KeysBackupResponse::try_from_http_response(response).map(Into::into)
            }
            RequestType::RoomMessage => {
                RoomMessageResponse::try_from_http_response(response).map(Into::into)
            }
        }
        .expect("Can't convert json string to response");

        self.runtime.block_on(self.inner.mark_request_as_sent(&id, &response))?;

        Ok(())
    }

    /// Let the state machine know about E2EE related sync changes that we
    /// received from the server.
    ///
    /// This needs to be called after every sync, ideally before processing
    /// any other sync changes.
    ///
    /// # Arguments
    ///
    /// * `events` - A serialized array of to-device events we received in the
    ///   current sync response.
    ///
    /// * `device_changes` - The list of devices that have changed in some way
    /// since the previous sync.
    ///
    /// * `key_counts` - The map of uploaded one-time key types and counts.
    pub fn receive_sync_changes(
        &self,
        events: String,
        device_changes: DeviceLists,
        key_counts: HashMap<String, i32>,
        unused_fallback_keys: Option<Vec<String>>,
    ) -> Result<SyncChangesResult, CryptoStoreError> {
        let to_device: ToDevice = serde_json::from_str(&events)?;
        let device_changes: RumaDeviceLists = device_changes.into();
        let key_counts: BTreeMap<DeviceKeyAlgorithm, UInt> = key_counts
            .into_iter()
            .map(|(k, v)| {
                (
                    DeviceKeyAlgorithm::from(k),
                    v.clamp(0, i32::MAX)
                        .try_into()
                        .expect("Couldn't convert key counts into an UInt"),
                )
            })
            .collect();

        let unused_fallback_keys: Option<Vec<DeviceKeyAlgorithm>> =
            unused_fallback_keys.map(|u| u.into_iter().map(DeviceKeyAlgorithm::from).collect());

        let (to_device_events, room_key_infos) = self.runtime.block_on(
            self.inner.receive_sync_changes(matrix_sdk_crypto::EncryptionSyncChanges {
                to_device_events: to_device.events,
                changed_devices: &device_changes,
                one_time_keys_counts: &key_counts,
                unused_fallback_keys: unused_fallback_keys.as_deref(),
            }),
        )?;

        let to_device_events =
            to_device_events.into_iter().map(|event| event.json().get().to_owned()).collect();
        let room_key_infos = room_key_infos.into_iter().map(|info| info.into()).collect();

        Ok(SyncChangesResult { to_device_events, room_key_infos })
    }

    /// Add the given list of users to be tracked, triggering a key query
    /// request for them.
    ///
    /// The OlmMachine maintains a list of users whose devices we are keeping
    /// track of: these are known as "tracked users". These must be users
    /// that we share a room with, so that the server sends us updates for
    /// their device lists.
    ///
    /// *Note*: Only users that aren't already tracked will be considered for an
    /// update. It's safe to call this with already tracked users, it won't
    /// result in excessive keys query requests.
    ///
    /// # Arguments
    ///
    /// `users` - The users that should be queued up for a key query.
    pub fn update_tracked_users(&self, users: Vec<String>) -> Result<(), CryptoStoreError> {
        let users: Vec<OwnedUserId> =
            users.into_iter().filter_map(|u| UserId::parse(u).ok()).collect();

        self.runtime.block_on(self.inner.update_tracked_users(users.iter().map(Deref::deref)))?;

        Ok(())
    }

    /// Check if the given user is considered to be tracked.
    ///
    /// A user can be marked for tracking using the
    /// [`OlmMachine::update_tracked_users()`] method.
    pub fn is_user_tracked(&self, user_id: String) -> Result<bool, CryptoStoreError> {
        let user_id = parse_user_id(&user_id)?;
        Ok(self.runtime.block_on(self.inner.tracked_users())?.contains(&user_id))
    }

    /// Generate one-time key claiming requests for all the users we are missing
    /// sessions for.
    ///
    /// After the request was sent out and a successful response was received
    /// the response body should be passed back to the state machine using the
    /// [mark_request_as_sent()](Self::mark_request_as_sent) method.
    ///
    /// This method should be called every time before a call to
    /// [`share_room_key()`](Self::share_room_key) is made.
    ///
    /// # Arguments
    ///
    /// * `users` - The list of users for which we would like to establish 1:1
    /// Olm sessions for.
    pub fn get_missing_sessions(
        &self,
        users: Vec<String>,
    ) -> Result<Option<Request>, CryptoStoreError> {
        let users: Vec<OwnedUserId> =
            users.into_iter().filter_map(|u| UserId::parse(u).ok()).collect();

        Ok(self
            .runtime
            .block_on(self.inner.get_missing_sessions(users.iter().map(Deref::deref)))?
            .map(|r| r.into()))
    }

    /// Get the stored room settings, such as the encryption algorithm or
    /// whether to encrypt only for trusted devices.
    ///
    /// These settings can be modified via
    /// [set_room_algorithm()](Self::set_room_algorithm) and
    /// [set_room_only_allow_trusted_devices()](Self::set_room_only_allow_trusted_devices)
    /// methods.
    pub fn get_room_settings(
        &self,
        room_id: String,
    ) -> Result<Option<RoomSettings>, CryptoStoreError> {
        let room_id = RoomId::parse(room_id)?;
        let settings = self
            .runtime
            .block_on(self.inner.store().get_room_settings(&room_id))?
            .map(|v| v.try_into())
            .transpose()?;
        Ok(settings)
    }

    /// Set the room algorithm used for encrypting messages to one of the
    /// available variants
    pub fn set_room_algorithm(
        &self,
        room_id: String,
        algorithm: EventEncryptionAlgorithm,
    ) -> Result<(), CryptoStoreError> {
        let room_id = RoomId::parse(room_id)?;
        self.runtime.block_on(async move {
            let mut settings =
                self.inner.store().get_room_settings(&room_id).await?.unwrap_or_default();
            settings.algorithm = algorithm.into();
            self.inner
                .store()
                .save_changes(Changes {
                    room_settings: HashMap::from([(room_id, settings)]),
                    ..Default::default()
                })
                .await?;
            Ok(())
        })
    }

    /// Set flag whether this room should encrypt messages for untrusted
    /// devices, or whether they should be excluded from the conversation.
    ///
    /// Note that per-room setting may be overridden by a global
    /// [set_only_allow_trusted_devices()](Self::set_only_allow_trusted_devices)
    /// method.
    pub fn set_room_only_allow_trusted_devices(
        &self,
        room_id: String,
        only_allow_trusted_devices: bool,
    ) -> Result<(), CryptoStoreError> {
        let room_id = RoomId::parse(room_id)?;
        self.runtime.block_on(async move {
            let mut settings =
                self.inner.store().get_room_settings(&room_id).await?.unwrap_or_default();
            settings.only_allow_trusted_devices = only_allow_trusted_devices;
            self.inner
                .store()
                .save_changes(Changes {
                    room_settings: HashMap::from([(room_id, settings)]),
                    ..Default::default()
                })
                .await?;
            Ok(())
        })
    }

    /// Check whether there is a global flag to only encrypt messages for
    /// trusted devices or for everyone.
    ///
    /// Note that if the global flag is false, individual rooms may still be
    /// encrypting only for trusted devices, depending on the per-room
    /// `only_allow_trusted_devices` flag.
    pub fn get_only_allow_trusted_devices(&self) -> Result<bool, CryptoStoreError> {
        let block = self.runtime.block_on(self.inner.store().get_only_allow_trusted_devices())?;
        Ok(block)
    }

    /// Set global flag whether to encrypt messages for untrusted devices, or
    /// whether they should be excluded from the conversation.
    ///
    /// Note that if enabled, it will override any per-room settings.
    pub fn set_only_allow_trusted_devices(
        &self,
        only_allow_trusted_devices: bool,
    ) -> Result<(), CryptoStoreError> {
        self.runtime.block_on(
            self.inner.store().set_only_allow_trusted_devices(only_allow_trusted_devices),
        )?;
        Ok(())
    }

    /// Share a room key with the given list of users for the given room.
    ///
    /// After the request was sent out and a successful response was received
    /// the response body should be passed back to the state machine using the
    /// [mark_request_as_sent()](Self::mark_request_as_sent) method.
    ///
    /// This method should be called every time before a call to
    /// [`encrypt()`](Self::encrypt) with the given `room_id` is made.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The unique id of the room, note that this doesn't strictly
    /// need to be a Matrix room, it just needs to be an unique identifier for
    /// the group that will participate in the conversation.
    ///
    /// * `users` - The list of users which are considered to be members of the
    /// room and should receive the room key.
    ///
    /// * `settings` - The settings that should be used for the room key.
    pub fn share_room_key(
        &self,
        room_id: String,
        users: Vec<String>,
        settings: EncryptionSettings,
    ) -> Result<Vec<Request>, CryptoStoreError> {
        let users: Vec<OwnedUserId> =
            users.into_iter().filter_map(|u| UserId::parse(u).ok()).collect();

        let room_id = RoomId::parse(room_id)?;
        let requests = self.runtime.block_on(self.inner.share_room_key(
            &room_id,
            users.iter().map(Deref::deref),
            settings,
        ))?;

        Ok(requests.into_iter().map(|r| r.as_ref().into()).collect())
    }

    /// Encrypt the given event with the given type and content for the given
    /// room.
    ///
    /// **Note**: A room key needs to be shared with the group of users that are
    /// members in the given room. If this is not done this method will panic.
    ///
    /// The usual flow to encrypt an event using this state machine is as
    /// follows:
    ///
    /// 1. Get the one-time key claim request to establish 1:1 Olm sessions for
    ///    the room members of the room we wish to participate in. This is done
    ///    using the [`get_missing_sessions()`](Self::get_missing_sessions)
    ///    method. This method call should be locked per call.
    ///
    /// 2. Share a room key with all the room members using the
    ///    [`share_room_key()`](Self::share_room_key). This method call should
    ///    be locked per room.
    ///
    /// 3. Encrypt the event using this method.
    ///
    /// 4. Send the encrypted event to the server.
    ///
    /// After the room key is shared steps 1 and 2 will become noops, unless
    /// there's some changes in the room membership or in the list of devices a
    /// member has.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The unique id of the room where the event will be sent to.
    ///
    /// * `even_type` - The type of the event.
    ///
    /// * `content` - The serialized content of the event.
    pub fn encrypt(
        &self,
        room_id: String,
        event_type: String,
        content: String,
    ) -> Result<String, CryptoStoreError> {
        let room_id = RoomId::parse(room_id)?;
        let content: Value = serde_json::from_str(&content)?;

        let encrypted_content = self
            .runtime
            .block_on(self.inner.encrypt_room_event_raw(&room_id, content, &event_type))
            .expect("Encrypting an event produced an error");

        Ok(serde_json::to_string(&encrypted_content)?)
    }

    /// Decrypt the given event that was sent in the given room.
    ///
    /// # Arguments
    ///
    /// * `event` - The serialized encrypted version of the event.
    ///
    /// * `room_id` - The unique id of the room where the event was sent to.
    ///
    /// * `strict_shields` - If `true`, messages will be decorated with strict
    ///   warnings (use `false` to match legacy behaviour where unsafe keys have
    ///   lower severity warnings and unverified identities are not decorated).
    pub fn decrypt_room_event(
        &self,
        event: String,
        room_id: String,
        handle_verification_events: bool,
        strict_shields: bool,
    ) -> Result<DecryptedEvent, DecryptionError> {
        // Element Android wants only the content and the type and will create a
        // decrypted event with those two itself, this struct makes sure we
        // throw away all the other fields.
        #[derive(Deserialize, Serialize)]
        struct Event<'a> {
            #[serde(rename = "type")]
            event_type: String,
            #[serde(borrow)]
            content: &'a RawValue,
        }

        let event: Raw<_> = serde_json::from_str(&event)?;
        let room_id = RoomId::parse(room_id)?;

        let decrypted = self.runtime.block_on(self.inner.decrypt_room_event(&event, &room_id))?;

        if handle_verification_events {
            if let Ok(AnyTimelineEvent::MessageLike(e)) = decrypted.event.deserialize() {
                match &e {
                    AnyMessageLikeEvent::RoomMessage(MessageLikeEvent::Original(
                        original_event,
                    )) => {
                        if let MessageType::VerificationRequest(_) = &original_event.content.msgtype
                        {
                            self.runtime.block_on(self.inner.receive_verification_event(&e))?;
                        }
                    }
                    _ if e.event_type().to_string().starts_with("m.key.verification") => {
                        self.runtime.block_on(self.inner.receive_verification_event(&e))?;
                    }
                    _ => (),
                }
            }
        }

        let encryption_info =
            decrypted.encryption_info.expect("Decrypted event didn't contain any encryption info");

        let event_json: Event<'_> = serde_json::from_str(decrypted.event.json().get())?;

        Ok(match &encryption_info.algorithm_info {
            AlgorithmInfo::MegolmV1AesSha2 { curve25519_key, sender_claimed_keys } => {
                DecryptedEvent {
                    clear_event: serde_json::to_string(&event_json)?,
                    sender_curve25519_key: curve25519_key.to_owned(),
                    claimed_ed25519_key: sender_claimed_keys
                        .get(&DeviceKeyAlgorithm::Ed25519)
                        .cloned(),
                    forwarding_curve25519_chain: vec![],
                    shield_state: if strict_shields {
                        encryption_info.verification_state.to_shield_state_strict().into()
                    } else {
                        encryption_info.verification_state.to_shield_state_lax().into()
                    },
                }
            }
        })
    }

    /// Request or re-request a room key that was used to encrypt the given
    /// event.
    ///
    /// # Arguments
    ///
    /// * `event` - The undecryptable event that we would wish to request a room
    /// key for.
    ///
    /// * `room_id` - The id of the room the event was sent to.
    pub fn request_room_key(
        &self,
        event: String,
        room_id: String,
    ) -> Result<KeyRequestPair, DecryptionError> {
        let event: Raw<_> = serde_json::from_str(&event)?;
        let room_id = RoomId::parse(room_id)?;

        let (cancel, request) =
            self.runtime.block_on(self.inner.request_room_key(&event, &room_id))?;

        let cancellation = cancel.map(|r| r.into());
        let key_request = request.into();

        Ok(KeyRequestPair { cancellation, key_request })
    }

    /// Export all of our room keys.
    ///
    /// # Arguments
    ///
    /// * `passphrase` - The passphrase that should be used to encrypt the key
    /// export.
    ///
    /// * `rounds` - The number of rounds that should be used when expanding the
    /// passphrase into an key.
    pub fn export_room_keys(
        &self,
        passphrase: String,
        rounds: i32,
    ) -> Result<String, CryptoStoreError> {
        let keys = self.runtime.block_on(self.inner.export_room_keys(|_| true))?;

        let encrypted = encrypt_room_key_export(&keys, &passphrase, rounds as u32)
            .map_err(CryptoStoreError::Serialization)?;

        Ok(encrypted)
    }

    /// Import room keys from the given serialized key export.
    ///
    /// # Arguments
    ///
    /// * `keys` - The serialized version of the key export.
    ///
    /// * `passphrase` - The passphrase that was used to encrypt the key export.
    ///
    /// * `progress_listener` - A callback that can be used to introspect the
    /// progress of the key import.
    pub fn import_room_keys(
        &self,
        keys: String,
        passphrase: String,
        progress_listener: Box<dyn ProgressListener>,
    ) -> Result<KeysImportResult, KeyImportError> {
        let keys = Cursor::new(keys);
        let keys = decrypt_room_key_export(keys, &passphrase)?;
        self.import_room_keys_helper(keys, false, progress_listener)
    }

    /// Import room keys from the given serialized unencrypted key export.
    ///
    /// This method is the same as [`OlmMachine::import_room_keys`] but the
    /// decryption step is skipped and should be performed by the caller. This
    /// should be used if the room keys are coming from the server-side backup,
    /// the method will mark all imported room keys as backed up.
    ///
    /// # Arguments
    ///
    /// * `keys` - The serialized version of the unencrypted key export.
    ///
    /// * `progress_listener` - A callback that can be used to introspect the
    /// progress of the key import.
    pub fn import_decrypted_room_keys(
        &self,
        keys: String,
        progress_listener: Box<dyn ProgressListener>,
    ) -> Result<KeysImportResult, KeyImportError> {
        let keys: Vec<Value> = serde_json::from_str(&keys)?;

        let keys = keys.into_iter().map(serde_json::from_value).filter_map(|k| k.ok()).collect();

        self.import_room_keys_helper(keys, true, progress_listener)
    }

    /// Discard the currently active room key for the given room if there is
    /// one.
    pub fn discard_room_key(&self, room_id: String) -> Result<(), CryptoStoreError> {
        let room_id = RoomId::parse(room_id)?;

        self.runtime.block_on(self.inner.invalidate_group_session(&room_id))?;

        Ok(())
    }

    /// Receive an unencrypted verification event.
    ///
    /// This method can be used to pass verification events that are happening
    /// in unencrypted rooms to the `OlmMachine`.
    ///
    /// **Note**: This has been deprecated.
    pub fn receive_unencrypted_verification_event(
        &self,
        event: String,
        room_id: String,
    ) -> Result<(), CryptoStoreError> {
        self.receive_verification_event(event, room_id)
    }

    /// Receive a verification event.
    ///
    /// This method can be used to pass verification events that are happening
    /// in rooms to the `OlmMachine`. The event should be in the decrypted form.
    pub fn receive_verification_event(
        &self,
        event: String,
        room_id: String,
    ) -> Result<(), CryptoStoreError> {
        let room_id = RoomId::parse(room_id)?;
        let event: AnySyncMessageLikeEvent = serde_json::from_str(&event)?;

        let event = event.into_full_event(room_id);

        self.runtime.block_on(self.inner.receive_verification_event(&event))?;

        Ok(())
    }

    /// Get all the verification requests that we share with the given user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The ID of the user for which we would like to fetch the
    /// verification requests.
    pub fn get_verification_requests(&self, user_id: String) -> Vec<Arc<VerificationRequest>> {
        let Ok(user_id) = UserId::parse(user_id) else {
            return vec![];
        };

        self.inner
            .get_verification_requests(&user_id)
            .into_iter()
            .map(|v| {
                VerificationRequest { inner: v, runtime: self.runtime.handle().to_owned() }.into()
            })
            .collect()
    }

    /// Get a verification requests that we share with the given user with the
    /// given flow id.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The ID of the user for which we would like to fetch the
    /// verification requests.
    ///
    /// * `flow_id` - The ID that uniquely identifies the verification flow.
    pub fn get_verification_request(
        &self,
        user_id: String,
        flow_id: String,
    ) -> Option<Arc<VerificationRequest>> {
        let user_id = UserId::parse(user_id).ok()?;

        self.inner.get_verification_request(&user_id, flow_id).map(|v| {
            VerificationRequest { inner: v, runtime: self.runtime.handle().to_owned() }.into()
        })
    }

    /// Get an m.key.verification.request content for the given user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The ID of the user which we would like to request to
    /// verify.
    ///
    /// * `methods` - The list of verification methods we want to advertise to
    /// support.
    pub fn verification_request_content(
        &self,
        user_id: String,
        methods: Vec<String>,
    ) -> Result<Option<String>, CryptoStoreError> {
        let user_id = parse_user_id(&user_id)?;

        let identity = self.runtime.block_on(self.inner.get_identity(&user_id, None))?;

        let methods = methods.into_iter().map(VerificationMethod::from).collect();

        Ok(if let Some(identity) = identity.and_then(|i| i.other()) {
            let content =
                self.runtime.block_on(identity.verification_request_content(Some(methods)));
            Some(serde_json::to_string(&content)?)
        } else {
            None
        })
    }

    /// Request a verification flow to begin with the given user in the given
    /// room.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The ID of the user which we would like to request to
    /// verify.
    ///
    /// * `room_id` - The ID of the room that represents a DM with the given
    /// user.
    ///
    /// * `event_id` - The event ID of the `m.key.verification.request` event
    /// that we sent out to request the verification to begin. The content for
    /// this request can be created using the [verification_request_content()]
    /// method.
    ///
    /// * `methods` - The list of verification methods we advertised as
    /// supported in the `m.key.verification.request` event.
    ///
    /// [verification_request_content()]: Self::verification_request_content
    pub fn request_verification(
        &self,
        user_id: String,
        room_id: String,
        event_id: String,
        methods: Vec<String>,
    ) -> Result<Option<Arc<VerificationRequest>>, CryptoStoreError> {
        let user_id = parse_user_id(&user_id)?;
        let event_id = EventId::parse(event_id)?;
        let room_id = RoomId::parse(room_id)?;

        let identity = self.runtime.block_on(self.inner.get_identity(&user_id, None))?;

        let methods = methods.into_iter().map(VerificationMethod::from).collect();

        Ok(if let Some(identity) = identity.and_then(|i| i.other()) {
            let request = self.runtime.block_on(identity.request_verification(
                &room_id,
                &event_id,
                Some(methods),
            ));

            Some(
                VerificationRequest { inner: request, runtime: self.runtime.handle().to_owned() }
                    .into(),
            )
        } else {
            None
        })
    }

    /// Request a verification flow to begin with the given user's device.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The ID of the user which we would like to request to
    /// verify.
    ///
    /// * `device_id` - The ID of the device that we wish to verify.
    ///
    /// * `methods` - The list of verification methods we advertised as
    /// supported in the `m.key.verification.request` event.
    pub fn request_verification_with_device(
        &self,
        user_id: String,
        device_id: String,
        methods: Vec<String>,
    ) -> Result<Option<RequestVerificationResult>, CryptoStoreError> {
        let user_id = parse_user_id(&user_id)?;
        let device_id = device_id.as_str().into();

        let methods = methods.into_iter().map(VerificationMethod::from).collect();

        Ok(
            if let Some(device) =
                self.runtime.block_on(self.inner.get_device(&user_id, device_id, None))?
            {
                let (verification, request) =
                    self.runtime.block_on(device.request_verification_with_methods(methods));

                Some(RequestVerificationResult {
                    verification: VerificationRequest {
                        inner: verification,
                        runtime: self.runtime.handle().to_owned(),
                    }
                    .into(),
                    request: request.into(),
                })
            } else {
                None
            },
        )
    }

    /// Request a verification flow to begin with our other devices.
    ///
    /// # Arguments
    ///
    /// `methods` - The list of verification methods we want to advertise to
    /// support.
    pub fn request_self_verification(
        &self,
        methods: Vec<String>,
    ) -> Result<Option<RequestVerificationResult>, CryptoStoreError> {
        let identity =
            self.runtime.block_on(self.inner.get_identity(self.inner.user_id(), None))?;

        let methods = methods.into_iter().map(VerificationMethod::from).collect();

        Ok(if let Some(identity) = identity.and_then(|i| i.own()) {
            let (verification, request) =
                self.runtime.block_on(identity.request_verification_with_methods(methods))?;
            Some(RequestVerificationResult {
                verification: VerificationRequest {
                    inner: verification,
                    runtime: self.runtime.handle().to_owned(),
                }
                .into(),
                request: request.into(),
            })
        } else {
            None
        })
    }

    /// Get a verification flow object for the given user with the given flow
    /// id.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The ID of the user for which we would like to fetch the
    /// verification.
    ///
    /// * `flow_id` - The ID that uniquely identifies the verification flow.
    pub fn get_verification(&self, user_id: String, flow_id: String) -> Option<Arc<Verification>> {
        let user_id = UserId::parse(user_id).ok()?;

        self.inner
            .get_verification(&user_id, &flow_id)
            .map(|v| Verification { inner: v, runtime: self.runtime.handle().to_owned() }.into())
    }

    /// Start short auth string verification with a device without going
    /// through a verification request first.
    ///
    /// **Note**: This has been largely deprecated and the
    /// [request_verification_with_device()] method should be used instead.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The ID of the user for which we would like to start the
    /// SAS verification.
    ///
    /// * `device_id` - The ID of device we would like to verify.
    ///
    /// [request_verification_with_device()]: Self::request_verification_with_device
    pub fn start_sas_with_device(
        &self,
        user_id: String,
        device_id: String,
    ) -> Result<Option<StartSasResult>, CryptoStoreError> {
        let user_id = parse_user_id(&user_id)?;
        let device_id = device_id.as_str().into();

        Ok(
            if let Some(device) =
                self.runtime.block_on(self.inner.get_device(&user_id, device_id, None))?
            {
                let (sas, request) = self.runtime.block_on(device.start_verification())?;

                Some(StartSasResult {
                    sas: Sas { inner: sas, runtime: self.runtime.handle().to_owned() }.into(),
                    request: request.into(),
                })
            } else {
                None
            },
        )
    }

    /// Create a new private cross signing identity and create a request to
    /// upload the public part of it to the server.
    pub fn bootstrap_cross_signing(&self) -> Result<BootstrapCrossSigningResult, CryptoStoreError> {
        Ok(self.runtime.block_on(self.inner.bootstrap_cross_signing(true))?.into())
    }

    /// Export all our private cross signing keys.
    ///
    /// The export will contain the seed for the ed25519 keys as a base64
    /// encoded string.
    ///
    /// This method returns `None` if we don't have any private cross signing
    /// keys.
    pub fn export_cross_signing_keys(
        &self,
    ) -> Result<Option<CrossSigningKeyExport>, CryptoStoreError> {
        Ok(self.runtime.block_on(self.inner.export_cross_signing_keys())?.map(|e| e.into()))
    }

    /// Import our private cross signing keys.
    ///
    /// The export needs to contain the seed for the ed25519 keys as a base64
    /// encoded string.
    pub fn import_cross_signing_keys(
        &self,
        export: CrossSigningKeyExport,
    ) -> Result<(), SecretImportError> {
        self.runtime.block_on(self.inner.import_cross_signing_keys(export.into()))?;

        Ok(())
    }

    /// Activate the given backup key to be used with the given backup version.
    ///
    /// **Warning**: The caller needs to make sure that the given `BackupKey` is
    /// trusted, otherwise we might be encrypting room keys that a malicious
    /// party could decrypt.
    ///
    /// The [`OlmMachine::verify_backup`] method can be used to so.
    pub fn enable_backup_v1(
        &self,
        key: MegolmV1BackupKey,
        version: String,
    ) -> Result<(), DecodeError> {
        let backup_key = RustBackupKey::from_base64(&key.public_key)?;
        backup_key.set_version(version);

        self.runtime.block_on(self.inner.backup_machine().enable_backup_v1(backup_key))?;

        Ok(())
    }

    /// Are we able to encrypt room keys.
    ///
    /// This returns true if we have an active `BackupKey` and backup version
    /// registered with the state machine.
    pub fn backup_enabled(&self) -> bool {
        self.runtime.block_on(self.inner.backup_machine().enabled())
    }

    /// Disable and reset our backup state.
    ///
    /// This will remove any pending backup request, remove the backup key and
    /// reset the backup state of each room key we have.
    pub fn disable_backup(&self) -> Result<(), CryptoStoreError> {
        Ok(self.runtime.block_on(self.inner.backup_machine().disable_backup())?)
    }

    /// Encrypt a batch of room keys and return a request that needs to be sent
    /// out to backup the room keys.
    pub fn backup_room_keys(&self) -> Result<Option<Request>, CryptoStoreError> {
        let request = self.runtime.block_on(self.inner.backup_machine().backup())?;

        let request = request.map(|r| r.into());

        Ok(request)
    }

    /// Get the number of backed up room keys and the total number of room keys.
    pub fn room_key_counts(&self) -> Result<RoomKeyCounts, CryptoStoreError> {
        Ok(self.runtime.block_on(self.inner.backup_machine().room_key_counts())?.into())
    }

    /// Store the recovery key in the crypto store.
    ///
    /// This is useful if the client wants to support gossiping of the backup
    /// key.
    pub fn save_recovery_key(
        &self,
        key: Option<Arc<BackupRecoveryKey>>,
        version: Option<String>,
    ) -> Result<(), CryptoStoreError> {
        let key = key.map(|k| {
            // We need to clone here due to FFI limitations but RecoveryKey does
            // not want to expose clone since it's private key material.
            let mut encoded = k.to_base64();
            let key = BackupDecryptionKey::from_base64(&encoded)
                .expect("Encoding and decoding from base64 should always work");
            encoded.zeroize();
            key
        });
        Ok(self.runtime.block_on(self.inner.backup_machine().save_decryption_key(key, version))?)
    }

    /// Get the backup keys we have saved in our crypto store.
    pub fn get_backup_keys(&self) -> Result<Option<Arc<BackupKeys>>, CryptoStoreError> {
        Ok(self
            .runtime
            .block_on(self.inner.backup_machine().get_backup_keys())?
            .try_into()
            .ok()
            .map(Arc::new))
    }

    /// Sign the given message using our device key and if available cross
    /// signing master key.
    pub fn sign(&self, message: String) -> HashMap<String, HashMap<String, String>> {
        self.runtime
            .block_on(self.inner.sign(&message))
            .into_iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    v.into_iter()
                        .map(|(k, v)| {
                            (
                                k.to_string(),
                                match v {
                                    Ok(s) => s.to_base64(),
                                    Err(i) => i.source,
                                },
                            )
                        })
                        .collect(),
                )
            })
            .collect()
    }

    /// Check if the given backup has been verified by us or by another of our
    /// devices that we trust.
    ///
    /// The `backup_info` should be a JSON encoded object with the following
    /// format:
    ///
    /// ```json
    /// {
    ///     "algorithm": "m.megolm_backup.v1.curve25519-aes-sha2",
    ///     "auth_data": {
    ///         "public_key":"XjhWTCjW7l59pbfx9tlCBQolfnIQWARoKOzjTOPSlWM",
    ///         "signatures": {}
    ///     }
    /// }
    /// ```
    pub fn verify_backup(
        &self,
        backup_info: String,
    ) -> Result<SignatureVerification, CryptoStoreError> {
        let backup_info = serde_json::from_str(&backup_info)?;

        Ok(self
            .runtime
            .block_on(self.inner.backup_machine().verify_backup(backup_info, false))?
            .into())
    }
}

impl OlmMachine {
    fn import_room_keys_helper(
        &self,
        keys: Vec<ExportedRoomKey>,
        from_backup: bool,
        progress_listener: Box<dyn ProgressListener>,
    ) -> Result<KeysImportResult, KeyImportError> {
        let listener = |progress: usize, total: usize| {
            progress_listener.on_progress(progress as i32, total as i32)
        };

        let result =
            self.runtime.block_on(self.inner.import_room_keys(keys, from_backup, listener))?;

        Ok(KeysImportResult {
            imported: result.imported_count as i64,
            total: result.total_count as i64,
            keys: result
                .keys
                .into_iter()
                .map(|(r, m)| {
                    (
                        r.to_string(),
                        m.into_iter().map(|(s, k)| (s, k.into_iter().collect())).collect(),
                    )
                })
                .collect(),
        })
    }
}
