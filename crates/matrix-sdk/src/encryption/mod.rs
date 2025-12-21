// Copyright 2021 The Matrix.org Foundation C.I.C.
// Copyright 2021 Damir Jelić
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

#![doc = include_str!("../docs/encryption.md")]
#![cfg_attr(target_family = "wasm", allow(unused_imports))]

#[cfg(feature = "experimental-send-custom-to-device")]
use std::ops::Deref;
use std::{
    collections::{BTreeMap, HashSet},
    io::{Cursor, Read, Write},
    iter,
    path::PathBuf,
    str::FromStr,
    sync::Arc,
};

use eyeball::{SharedObservable, Subscriber};
use futures_core::Stream;
use futures_util::{
    future::try_join,
    stream::{self, StreamExt},
};
#[cfg(feature = "experimental-send-custom-to-device")]
use matrix_sdk_base::crypto::CollectStrategy;
use matrix_sdk_base::{
    StateStoreDataKey, StateStoreDataValue,
    cross_process_lock::CrossProcessLockError,
    crypto::{
        CrossSigningBootstrapRequests, OlmMachine,
        store::types::{RoomKeyBundleInfo, RoomKeyInfo},
        types::{
            SignedKey,
            requests::{
                OutgoingRequest, OutgoingVerificationRequest, RoomMessageRequest, ToDeviceRequest,
            },
        },
    },
};
use matrix_sdk_common::{executor::spawn, locks::Mutex as StdMutex};
use ruma::{
    DeviceId, MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedUserId, TransactionId, UserId,
    api::client::{
        error::{ErrorBody, StandardErrorBody},
        keys::{
            get_keys, upload_keys, upload_signatures::v3::Request as UploadSignaturesRequest,
            upload_signing_keys::v3::Request as UploadSigningKeysRequest,
        },
        message::send_message_event,
        to_device::send_event_to_device::v3::{
            Request as RumaToDeviceRequest, Response as ToDeviceResponse,
        },
        uiaa::{AuthData, AuthType, OAuthParams, UiaaInfo},
    },
    assign,
    events::room::{MediaSource, ThumbnailInfo},
};
#[cfg(feature = "experimental-send-custom-to-device")]
use ruma::{events::AnyToDeviceEventContent, serde::Raw, to_device::DeviceIdOrAllDevices};
use serde::{Deserialize, de::Error as _};
use tasks::BundleReceiverTask;
use tokio::sync::{Mutex, RwLockReadGuard};
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tracing::{debug, error, instrument, warn};
use url::Url;
use vodozemac::Curve25519PublicKey;

use self::{
    backups::{Backups, types::BackupClientState},
    futures::UploadEncryptedFile,
    identities::{Device, DeviceUpdates, IdentityUpdates, UserDevices, UserIdentity},
    recovery::{Recovery, RecoveryState},
    secret_storage::SecretStorage,
    tasks::{BackupDownloadTask, BackupUploadingTask, ClientTasks},
    verification::{SasVerification, Verification, VerificationRequest},
};
use crate::{
    Client, Error, HttpError, Result, RumaApiError, TransmissionProgress,
    attachment::Thumbnail,
    client::{ClientInner, WeakClient},
    cross_process_lock::CrossProcessLockGuard,
    error::HttpResult,
};

pub mod backups;
pub mod futures;
pub mod identities;
pub mod recovery;
pub mod secret_storage;
pub(crate) mod tasks;
pub mod verification;

pub use matrix_sdk_base::crypto::{
    CrossSigningStatus, CryptoStoreError, DecryptorError, EventError, KeyExportError, LocalTrust,
    MediaEncryptionInfo, MegolmError, OlmError, RoomKeyImportResult, SecretImportError,
    SessionCreationError, SignatureError, VERSION,
    olm::{
        SessionCreationError as MegolmSessionCreationError,
        SessionExportError as OlmSessionExportError,
    },
    vodozemac,
};

#[cfg(feature = "experimental-send-custom-to-device")]
use crate::config::RequestConfig;
pub use crate::error::RoomKeyImportError;

/// All the data related to the encryption state.
pub(crate) struct EncryptionData {
    /// Background tasks related to encryption (key backup, initialization
    /// tasks, etc.).
    pub tasks: StdMutex<ClientTasks>,

    /// End-to-end encryption settings.
    pub encryption_settings: EncryptionSettings,

    /// All state related to key backup.
    pub backup_state: BackupClientState,

    /// All state related to secret storage recovery.
    pub recovery_state: SharedObservable<RecoveryState>,
}

impl EncryptionData {
    pub fn new(encryption_settings: EncryptionSettings) -> Self {
        Self {
            encryption_settings,

            tasks: StdMutex::new(Default::default()),
            backup_state: Default::default(),
            recovery_state: Default::default(),
        }
    }

    pub fn initialize_tasks(&self, client: &Arc<ClientInner>) {
        let weak_client = WeakClient::from_inner(client);

        let mut tasks = self.tasks.lock();
        tasks.upload_room_keys = Some(BackupUploadingTask::new(weak_client.clone()));

        if self.encryption_settings.backup_download_strategy
            == BackupDownloadStrategy::AfterDecryptionFailure
        {
            tasks.download_room_keys = Some(BackupDownloadTask::new(weak_client));
        }
    }

    /// Initialize the background task which listens for changes in the
    /// [`backups::BackupState`] and updataes the [`recovery::RecoveryState`].
    ///
    /// This should happen after the usual tasks have been set up and after the
    /// E2EE initialization tasks have been set up.
    pub fn initialize_recovery_state_update_task(&self, client: &Client) {
        let mut guard = self.tasks.lock();

        let future = Recovery::update_state_after_backup_state_change(client);
        let join_handle = spawn(future);

        guard.update_recovery_state_after_backup = Some(join_handle);
    }
}

/// Settings for end-to-end encryption features.
#[derive(Clone, Copy, Debug, Default)]
pub struct EncryptionSettings {
    /// Automatically bootstrap cross-signing for a user once they're logged, in
    /// case it's not already done yet.
    ///
    /// This requires to login with a username and password, or that MSC3967 is
    /// enabled on the server, as of 2023-10-20.
    pub auto_enable_cross_signing: bool,

    /// Select a strategy to download room keys from the backup, by default room
    /// keys won't be downloaded from the backup automatically.
    ///
    /// Take a look at the [`BackupDownloadStrategy`] enum for more options.
    pub backup_download_strategy: BackupDownloadStrategy,

    /// Automatically create a backup version if no backup exists.
    pub auto_enable_backups: bool,
}

/// Settings for end-to-end encryption features.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum BackupDownloadStrategy {
    /// Automatically download all room keys from the backup when the backup
    /// recovery key has been received. The backup recovery key can be received
    /// in two ways:
    ///
    /// 1. Received as a `m.secret.send` to-device event, after a successful
    ///    interactive verification.
    /// 2. Imported from secret storage (4S) using the
    ///    [`SecretStore::import_secrets()`] method.
    ///
    /// [`SecretStore::import_secrets()`]: crate::encryption::secret_storage::SecretStore::import_secrets
    OneShot,

    /// Attempt to download a single room key if an event fails to be decrypted.
    AfterDecryptionFailure,

    /// Don't download any room keys automatically. The user can manually
    /// download room keys using the [`Backups::download_room_key()`] methods.
    ///
    /// This is the default option.
    #[default]
    Manual,
}

/// The verification state of our own device
///
/// This enum tells us if our own user identity trusts these devices, in other
/// words it tells us if the user identity has signed the device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerificationState {
    /// The verification state is unknown for now.
    Unknown,
    /// The device is considered to be verified, it has been signed by its user
    /// identity.
    Verified,
    /// The device is unverified.
    Unverified,
}

/// Wraps together a `CrossProcessLockStoreGuard` and a generation number.
#[derive(Debug)]
pub struct CrossProcessLockStoreGuardWithGeneration {
    _guard: CrossProcessLockGuard,
    generation: u64,
}

impl CrossProcessLockStoreGuardWithGeneration {
    /// Return the Crypto Store generation associated with this store lock.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// A stateful struct remembering the cross-signing keys we need to upload.
///
/// Since the `/_matrix/client/v3/keys/device_signing/upload` might require
/// additional authentication, this struct will contain information on the type
/// of authentication the user needs to complete before the upload might be
/// continued.
///
/// More info can be found in the [spec].
///
/// [spec]: https://spec.matrix.org/v1.11/client-server-api/#post_matrixclientv3keysdevice_signingupload
#[derive(Debug)]
pub struct CrossSigningResetHandle {
    client: Client,
    upload_request: UploadSigningKeysRequest,
    signatures_request: UploadSignaturesRequest,
    auth_type: CrossSigningResetAuthType,
    is_cancelled: Mutex<bool>,
}

impl CrossSigningResetHandle {
    /// Set up a new `CrossSigningResetHandle`.
    pub fn new(
        client: Client,
        upload_request: UploadSigningKeysRequest,
        signatures_request: UploadSignaturesRequest,
        auth_type: CrossSigningResetAuthType,
    ) -> Self {
        Self {
            client,
            upload_request,
            signatures_request,
            auth_type,
            is_cancelled: Mutex::new(false),
        }
    }

    /// Get the [`CrossSigningResetAuthType`] this cross-signing reset process
    /// is using.
    pub fn auth_type(&self) -> &CrossSigningResetAuthType {
        &self.auth_type
    }

    /// Continue the cross-signing reset by either waiting for the
    /// authentication to be done on the side of the OAuth 2.0 server or by
    /// providing additional [`AuthData`] the homeserver requires.
    pub async fn auth(&self, auth: Option<AuthData>) -> Result<()> {
        let mut upload_request = self.upload_request.clone();
        upload_request.auth = auth;

        while let Err(e) = self.client.send(upload_request.clone()).await {
            if *self.is_cancelled.lock().await {
                return Ok(());
            }

            match e.as_uiaa_response() {
                Some(uiaa_info) => {
                    if uiaa_info.auth_error.is_some() {
                        return Err(e.into());
                    }
                }
                None => return Err(e.into()),
            }
        }

        self.client.send(self.signatures_request.clone()).await?;

        Ok(())
    }

    /// Cancel the ongoing identity reset process
    pub async fn cancel(&self) {
        *self.is_cancelled.lock().await = true;
    }
}

/// information about the additional authentication that is required before the
/// cross-signing keys can be uploaded.
#[derive(Debug, Clone)]
pub enum CrossSigningResetAuthType {
    /// The homeserver requires user-interactive authentication.
    Uiaa(UiaaInfo),
    /// OAuth 2.0 is used for authentication and the user needs to open a URL to
    /// approve the upload of cross-signing keys.
    OAuth(OAuthCrossSigningResetInfo),
}

impl CrossSigningResetAuthType {
    fn new(error: &HttpError) -> Result<Option<Self>> {
        if let Some(auth_info) = error.as_uiaa_response() {
            if let Ok(Some(auth_info)) = OAuthCrossSigningResetInfo::from_auth_info(auth_info) {
                Ok(Some(CrossSigningResetAuthType::OAuth(auth_info)))
            } else {
                Ok(Some(CrossSigningResetAuthType::Uiaa(auth_info.clone())))
            }
        } else {
            Ok(None)
        }
    }
}

/// OAuth 2.0 specific information about the required authentication for the
/// upload of cross-signing keys.
#[derive(Debug, Clone, Deserialize)]
pub struct OAuthCrossSigningResetInfo {
    /// The URL where the user can approve the reset of the cross-signing keys.
    pub approval_url: Url,
}

impl OAuthCrossSigningResetInfo {
    fn from_auth_info(auth_info: &UiaaInfo) -> Result<Option<Self>> {
        let Some(parameters) = auth_info.params::<OAuthParams>(&AuthType::OAuth)? else {
            return Ok(None);
        };

        Ok(Some(OAuthCrossSigningResetInfo { approval_url: parameters.url.as_str().try_into()? }))
    }
}

/// A struct that helps to parse the custom error message Synapse posts if a
/// duplicate one-time key is uploaded.
#[derive(Debug)]
struct DuplicateOneTimeKeyErrorMessage {
    /// The previously uploaded one-time key.
    old_key: Curve25519PublicKey,
    /// The one-time key we're attempting to upload right now.
    new_key: Curve25519PublicKey,
}

impl FromStr for DuplicateOneTimeKeyErrorMessage {
    type Err = serde_json::Error;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        // First we split the string into two parts, the part containing the old key and
        // the part containing the new key. The parts are conveniently separated
        // by a `;` character.
        let mut split = s.split_terminator(';');

        let old_key = split
            .next()
            .ok_or(serde_json::Error::custom("Old key is missing in the error message"))?;
        let new_key = split
            .next()
            .ok_or(serde_json::Error::custom("New key is missing in the error message"))?;

        // Now we remove the lengthy prefix from the part containing the old key, we
        // should be left with just the JSON of the signed key.
        let old_key_index = old_key
            .find("Old key:")
            .ok_or(serde_json::Error::custom("Old key is missing the prefix"))?;

        let old_key = old_key[old_key_index..]
            .trim()
            .strip_prefix("Old key:")
            .ok_or(serde_json::Error::custom("Old key is missing the prefix"))?;

        // The part containing the new key is much simpler, we just remove a static
        // prefix.
        let new_key = new_key
            .trim()
            .strip_prefix("new key:")
            .ok_or(serde_json::Error::custom("New key is missing the prefix"))?;

        // The JSON containing the new key is for some reason quoted using single
        // quotes, so let's replace them with normal double quotes.
        let new_key = new_key.replace("'", "\"");

        // Let's deserialize now.
        let old_key: SignedKey = serde_json::from_str(old_key)?;
        let new_key: SignedKey = serde_json::from_str(&new_key)?;

        // Pick out the Curve keys, we don't care about the rest that much.
        let old_key = old_key.key();
        let new_key = new_key.key();

        Ok(Self { old_key, new_key })
    }
}

impl Client {
    pub(crate) async fn olm_machine(&self) -> RwLockReadGuard<'_, Option<OlmMachine>> {
        self.base_client().olm_machine().await
    }

    pub(crate) async fn mark_request_as_sent(
        &self,
        request_id: &TransactionId,
        response: impl Into<matrix_sdk_base::crypto::types::requests::AnyIncomingResponse<'_>>,
    ) -> Result<(), matrix_sdk_base::Error> {
        Ok(self
            .olm_machine()
            .await
            .as_ref()
            .expect(
                "We should have an olm machine once we try to mark E2EE related requests as sent",
            )
            .mark_request_as_sent(request_id, response)
            .await?)
    }

    /// Query the server for users device keys.
    ///
    /// # Panics
    ///
    /// Panics if no key query needs to be done.
    #[instrument(skip(self, device_keys))]
    pub(crate) async fn keys_query(
        &self,
        request_id: &TransactionId,
        device_keys: BTreeMap<OwnedUserId, Vec<OwnedDeviceId>>,
    ) -> Result<get_keys::v3::Response> {
        let request = assign!(get_keys::v3::Request::new(), { device_keys });

        let response = self.send(request).await?;
        self.mark_request_as_sent(request_id, &response).await?;
        self.encryption().update_state_after_keys_query(&response).await;

        Ok(response)
    }

    /// Construct a [`EncryptedFile`][ruma::events::room::EncryptedFile] by
    /// encrypting and uploading a provided reader.
    ///
    /// # Arguments
    ///
    /// * `content_type` - The content type of the file.
    /// * `reader` - The reader that should be encrypted and uploaded.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # use matrix_sdk::ruma::{room_id, OwnedRoomId};
    /// use serde::{Deserialize, Serialize};
    /// use matrix_sdk::ruma::events::{macros::EventContent, room::EncryptedFile};
    ///
    /// #[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
    /// #[ruma_event(type = "com.example.custom", kind = MessageLike)]
    /// struct CustomEventContent {
    ///     encrypted_file: EncryptedFile,
    /// }
    ///
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// # let room = client.get_room(&room_id!("!test:example.com")).unwrap();
    /// let mut reader = std::io::Cursor::new(b"Hello, world!");
    /// let encrypted_file = client.upload_encrypted_file(&mut reader).await?;
    ///
    /// room.send(CustomEventContent { encrypted_file }).await?;
    /// # anyhow::Ok(()) };
    /// ```
    pub fn upload_encrypted_file<'a, R: Read + ?Sized + 'a>(
        &'a self,
        reader: &'a mut R,
    ) -> UploadEncryptedFile<'a, R> {
        UploadEncryptedFile::new(self, reader)
    }

    /// Encrypt and upload the file and thumbnails, and return the source
    /// information.
    pub(crate) async fn upload_encrypted_media_and_thumbnail(
        &self,
        data: &[u8],
        thumbnail: Option<Thumbnail>,
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> Result<(MediaSource, Option<(MediaSource, Box<ThumbnailInfo>)>)> {
        let upload_thumbnail = self.upload_encrypted_thumbnail(thumbnail, send_progress.clone());

        let upload_attachment = async {
            let mut cursor = Cursor::new(data);
            self.upload_encrypted_file(&mut cursor)
                .with_send_progress_observable(send_progress)
                .await
        };

        let (thumbnail, file) = try_join(upload_thumbnail, upload_attachment).await?;

        Ok((MediaSource::Encrypted(Box::new(file)), thumbnail))
    }

    /// Uploads an encrypted thumbnail to the media repository, and returns
    /// its source and extra information.
    async fn upload_encrypted_thumbnail(
        &self,
        thumbnail: Option<Thumbnail>,
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> Result<Option<(MediaSource, Box<ThumbnailInfo>)>> {
        let Some(thumbnail) = thumbnail else {
            return Ok(None);
        };

        let (data, _, thumbnail_info) = thumbnail.into_parts();
        let mut cursor = Cursor::new(data);

        let file = self
            .upload_encrypted_file(&mut cursor)
            .with_send_progress_observable(send_progress)
            .await?;

        Ok(Some((MediaSource::Encrypted(Box::new(file)), thumbnail_info)))
    }

    /// Claim one-time keys creating new Olm sessions.
    ///
    /// # Arguments
    ///
    /// * `users` - The list of user/device pairs that we should claim keys for.
    pub(crate) async fn claim_one_time_keys(
        &self,
        users: impl Iterator<Item = &UserId>,
    ) -> Result<()> {
        let _lock = self.locks().key_claim_lock.lock().await;

        if let Some((request_id, request)) = self
            .olm_machine()
            .await
            .as_ref()
            .ok_or(Error::NoOlmMachine)?
            .get_missing_sessions(users)
            .await?
        {
            let response = self.send(request).await?;
            self.mark_request_as_sent(&request_id, &response).await?;
        }

        Ok(())
    }

    /// Upload the E2E encryption keys.
    ///
    /// This uploads the long lived device keys as well as the required amount
    /// of one-time keys.
    ///
    /// # Panics
    ///
    /// Panics if the client isn't logged in, or if no encryption keys need to
    /// be uploaded.
    #[instrument(skip(self, request))]
    pub(crate) async fn keys_upload(
        &self,
        request_id: &TransactionId,
        request: &upload_keys::v3::Request,
    ) -> Result<upload_keys::v3::Response> {
        debug!(
            device_keys = request.device_keys.is_some(),
            one_time_key_count = request.one_time_keys.len(),
            "Uploading public encryption keys",
        );

        let response = self.send(request.clone()).await?;
        self.mark_request_as_sent(request_id, &response).await?;

        Ok(response)
    }

    pub(crate) async fn room_send_helper(
        &self,
        request: &RoomMessageRequest,
    ) -> Result<send_message_event::v3::Response> {
        let content = request.content.clone();
        let txn_id = request.txn_id.clone();
        let room_id = &request.room_id;

        self.get_room(room_id)
            .expect("Can't send a message to a room that isn't known to the store")
            .send(*content)
            .with_transaction_id(txn_id)
            .await
            .map(|result| result.response)
    }

    pub(crate) async fn send_to_device(
        &self,
        request: &ToDeviceRequest,
    ) -> HttpResult<ToDeviceResponse> {
        let request = RumaToDeviceRequest::new_raw(
            request.event_type.clone(),
            request.txn_id.clone(),
            request.messages.clone(),
        );

        self.send(request).await
    }

    pub(crate) async fn send_verification_request(
        &self,
        request: OutgoingVerificationRequest,
    ) -> Result<()> {
        use matrix_sdk_base::crypto::types::requests::OutgoingVerificationRequest::*;

        match request {
            ToDevice(t) => {
                self.send_to_device(&t).await?;
            }
            InRoom(r) => {
                self.room_send_helper(&r).await?;
            }
        }

        Ok(())
    }

    async fn send_outgoing_request(&self, r: OutgoingRequest) -> Result<()> {
        use matrix_sdk_base::crypto::types::requests::AnyOutgoingRequest;

        match r.request() {
            AnyOutgoingRequest::KeysQuery(request) => {
                self.keys_query(r.request_id(), request.device_keys.clone()).await?;
            }
            AnyOutgoingRequest::KeysUpload(request) => {
                let response = self.keys_upload(r.request_id(), request).await;

                if let Err(e) = &response {
                    match e.as_ruma_api_error() {
                        Some(RumaApiError::ClientApi(e)) if e.status_code == 400 => {
                            if let ErrorBody::Standard(StandardErrorBody { message, .. }) = &e.body
                            {
                                // This is one of the nastiest errors we can have. The server
                                // telling us that we already have a one-time key uploaded means
                                // that we forgot about some of our one-time keys. This will lead to
                                // UTDs.
                                {
                                    let already_reported = self
                                        .state_store()
                                        .get_kv_data(StateStoreDataKey::OneTimeKeyAlreadyUploaded)
                                        .await?
                                        .is_some();

                                    if message.starts_with("One time key") && !already_reported {
                                        if let Ok(message) =
                                            DuplicateOneTimeKeyErrorMessage::from_str(message)
                                        {
                                            error!(
                                                sentry = true,
                                                old_key = %message.old_key,
                                                new_key = %message.new_key,
                                                "Duplicate one-time keys have been uploaded"
                                            );
                                        } else {
                                            error!(
                                                sentry = true,
                                                "Duplicate one-time keys have been uploaded"
                                            );
                                        }

                                        self.state_store()
                                            .set_kv_data(
                                                StateStoreDataKey::OneTimeKeyAlreadyUploaded,
                                                StateStoreDataValue::OneTimeKeyAlreadyUploaded,
                                            )
                                            .await?;
                                    }
                                }
                            }
                        }
                        _ => {}
                    }

                    response?;
                }
            }
            AnyOutgoingRequest::ToDeviceRequest(request) => {
                let response = self.send_to_device(request).await?;
                self.mark_request_as_sent(r.request_id(), &response).await?;
            }
            AnyOutgoingRequest::SignatureUpload(request) => {
                let response = self.send(request.clone()).await?;
                self.mark_request_as_sent(r.request_id(), &response).await?;
            }
            AnyOutgoingRequest::RoomMessage(request) => {
                let response = self.room_send_helper(request).await?;
                self.mark_request_as_sent(r.request_id(), &response).await?;
            }
            AnyOutgoingRequest::KeysClaim(request) => {
                let response = self.send(request.clone()).await?;
                self.mark_request_as_sent(r.request_id(), &response).await?;
            }
        }

        Ok(())
    }

    #[instrument(skip_all)]
    pub(crate) async fn send_outgoing_requests(&self) -> Result<()> {
        const MAX_CONCURRENT_REQUESTS: usize = 20;

        // This is needed because sometimes we need to automatically
        // claim some one-time keys to unwedge an existing Olm session.
        if let Err(e) = self.claim_one_time_keys(iter::empty()).await {
            warn!("Error while claiming one-time keys {:?}", e);
        }

        let outgoing_requests = stream::iter(
            self.olm_machine()
                .await
                .as_ref()
                .ok_or(Error::NoOlmMachine)?
                .outgoing_requests()
                .await?,
        )
        .map(|r| self.send_outgoing_request(r));

        let requests = outgoing_requests.buffer_unordered(MAX_CONCURRENT_REQUESTS);

        requests
            .for_each(|r| async move {
                match r {
                    Ok(_) => (),
                    Err(e) => warn!(error = ?e, "Error when sending out an outgoing E2EE request"),
                }
            })
            .await;

        Ok(())
    }
}

#[cfg(any(feature = "testing", test))]
impl Client {
    /// Get the olm machine, for testing purposes only.
    pub async fn olm_machine_for_testing(&self) -> RwLockReadGuard<'_, Option<OlmMachine>> {
        self.olm_machine().await
    }
}

/// A high-level API to manage the client's encryption.
///
/// To get this, use [`Client::encryption()`].
#[derive(Debug, Clone)]
pub struct Encryption {
    /// The underlying client.
    client: Client,
}

impl Encryption {
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    /// Returns the current encryption settings for this client.
    pub(crate) fn settings(&self) -> EncryptionSettings {
        self.client.inner.e2ee.encryption_settings
    }

    /// Get the public ed25519 key of our own device. This is usually what is
    /// called the fingerprint of the device.
    pub async fn ed25519_key(&self) -> Option<String> {
        self.client.olm_machine().await.as_ref().map(|o| o.identity_keys().ed25519.to_base64())
    }

    /// Get the public Curve25519 key of our own device.
    pub async fn curve25519_key(&self) -> Option<Curve25519PublicKey> {
        self.client.olm_machine().await.as_ref().map(|o| o.identity_keys().curve25519)
    }

    /// Get the current device creation timestamp.
    pub async fn device_creation_timestamp(&self) -> MilliSecondsSinceUnixEpoch {
        match self.get_own_device().await {
            Ok(Some(device)) => device.first_time_seen_ts(),
            // Should not happen, there should always be an own device
            _ => MilliSecondsSinceUnixEpoch::now(),
        }
    }

    pub(crate) async fn import_secrets_bundle(
        &self,
        bundle: &matrix_sdk_base::crypto::types::SecretsBundle,
    ) -> Result<(), SecretImportError> {
        let olm_machine = self.client.olm_machine().await;
        let olm_machine =
            olm_machine.as_ref().expect("This should only be called once we have an OlmMachine");

        olm_machine.store().import_secrets_bundle(bundle).await
    }

    /// Get the status of the private cross signing keys.
    ///
    /// This can be used to check which private cross signing keys we have
    /// stored locally.
    pub async fn cross_signing_status(&self) -> Option<CrossSigningStatus> {
        let olm = self.client.olm_machine().await;
        let machine = olm.as_ref()?;
        Some(machine.cross_signing_status().await)
    }

    /// Does the user have other devices that the current device can verify
    /// against?
    ///
    /// The device must be signed by the user's cross-signing key, must have an
    /// identity, and must not be a dehydrated device.
    pub async fn has_devices_to_verify_against(&self) -> Result<bool> {
        let olm_machine = self.client.olm_machine().await;
        let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;
        let user_id = olm_machine.user_id();

        self.ensure_initial_key_query().await?;

        let devices = self.get_user_devices(user_id).await?;

        let ret = devices.devices().any(|device| {
            device.is_cross_signed_by_owner()
                && device.curve25519_key().is_some()
                && !device.is_dehydrated()
        });

        Ok(ret)
    }

    /// Get all the tracked users we know about
    ///
    /// Tracked users are users for which we keep the device list of E2EE
    /// capable devices up to date.
    pub async fn tracked_users(&self) -> Result<HashSet<OwnedUserId>, CryptoStoreError> {
        if let Some(machine) = self.client.olm_machine().await.as_ref() {
            machine.tracked_users().await
        } else {
            Ok(HashSet::new())
        }
    }

    /// Get a [`Subscriber`] for the [`VerificationState`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use matrix_sdk::{Client, encryption};
    /// use url::Url;
    ///
    /// # async {
    /// let homeserver = Url::parse("http://example.com")?;
    /// let client = Client::new(homeserver).await?;
    /// let mut subscriber = client.encryption().verification_state();
    ///
    /// let current_value = subscriber.get();
    ///
    /// println!("The current verification state is: {current_value:?}");
    ///
    /// if let Some(verification_state) = subscriber.next().await {
    ///     println!("Received verification state update {:?}", verification_state)
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub fn verification_state(&self) -> Subscriber<VerificationState> {
        self.client.inner.verification_state.subscribe_reset()
    }

    /// Get a verification object with the given flow id.
    pub async fn get_verification(&self, user_id: &UserId, flow_id: &str) -> Option<Verification> {
        let olm = self.client.olm_machine().await;
        let olm = olm.as_ref()?;
        #[allow(clippy::bind_instead_of_map)]
        olm.get_verification(user_id, flow_id).and_then(|v| match v {
            matrix_sdk_base::crypto::Verification::SasV1(sas) => {
                Some(SasVerification { inner: sas, client: self.client.clone() }.into())
            }
            #[cfg(feature = "qrcode")]
            matrix_sdk_base::crypto::Verification::QrV1(qr) => {
                Some(verification::QrVerification { inner: qr, client: self.client.clone() }.into())
            }
            _ => None,
        })
    }

    /// Get a `VerificationRequest` object for the given user with the given
    /// flow id.
    pub async fn get_verification_request(
        &self,
        user_id: &UserId,
        flow_id: impl AsRef<str>,
    ) -> Option<VerificationRequest> {
        let olm = self.client.olm_machine().await;
        let olm = olm.as_ref()?;

        olm.get_verification_request(user_id, flow_id)
            .map(|r| VerificationRequest { inner: r, client: self.client.clone() })
    }

    /// Get a specific device of a user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the device belongs to.
    ///
    /// * `device_id` - The unique id of the device.
    ///
    /// Returns a `Device` if one is found and the crypto store didn't throw an
    /// error.
    ///
    /// This will always return None if the client hasn't been logged in.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, ruma::{device_id, user_id}};
    /// # use url::Url;
    /// # async {
    /// # let alice = user_id!("@alice:example.org");
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// if let Some(device) =
    ///     client.encryption().get_device(alice, device_id!("DEVICEID")).await?
    /// {
    ///     println!("{:?}", device.is_verified());
    ///
    ///     if !device.is_verified() {
    ///         let verification = device.request_verification().await?;
    ///     }
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn get_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<Device>, CryptoStoreError> {
        let olm = self.client.olm_machine().await;
        let Some(machine) = olm.as_ref() else { return Ok(None) };
        let device = machine.get_device(user_id, device_id, None).await?;
        Ok(device.map(|d| Device { inner: d, client: self.client.clone() }))
    }

    /// A convenience method to retrieve your own device from the store.
    ///
    /// This is the same as calling [`Encryption::get_device()`] with your own
    /// user and device ID.
    ///
    /// This will always return a device, unless you are not logged in.
    pub async fn get_own_device(&self) -> Result<Option<Device>, CryptoStoreError> {
        let olm = self.client.olm_machine().await;
        let Some(machine) = olm.as_ref() else { return Ok(None) };
        let device = machine.get_device(machine.user_id(), machine.device_id(), None).await?;
        Ok(device.map(|d| Device { inner: d, client: self.client.clone() }))
    }

    /// Get a map holding all the devices of an user.
    ///
    /// This will always return an empty map if the client hasn't been logged
    /// in.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the devices belong to.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, ruma::user_id};
    /// # use url::Url;
    /// # async {
    /// # let alice = user_id!("@alice:example.org");
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// let devices = client.encryption().get_user_devices(alice).await?;
    ///
    /// for device in devices.devices() {
    ///     println!("{device:?}");
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn get_user_devices(&self, user_id: &UserId) -> Result<UserDevices, Error> {
        let devices = self
            .client
            .olm_machine()
            .await
            .as_ref()
            .ok_or(Error::NoOlmMachine)?
            .get_user_devices(user_id, None)
            .await?;

        Ok(UserDevices { inner: devices, client: self.client.clone() })
    }

    /// Get the E2EE identity of a user from the crypto store.
    ///
    /// Usually, we only have the E2EE identity of a user locally if the user
    /// is tracked, meaning that we are both members of the same encrypted room.
    ///
    /// To get the E2EE identity of a user even if it is not available locally
    /// use [`Encryption::request_user_identity()`].
    ///
    /// # Arguments
    ///
    /// * `user_id` - The unique id of the user that the identity belongs to.
    ///
    /// Returns a `UserIdentity` if one is found and the crypto store
    /// didn't throw an error.
    ///
    /// This will always return None if the client hasn't been logged in.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, ruma::user_id};
    /// # use url::Url;
    /// # async {
    /// # let alice = user_id!("@alice:example.org");
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// let user = client.encryption().get_user_identity(alice).await?;
    ///
    /// if let Some(user) = user {
    ///     println!("{:?}", user.is_verified());
    ///
    ///     let verification = user.request_verification().await?;
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn get_user_identity(
        &self,
        user_id: &UserId,
    ) -> Result<Option<UserIdentity>, CryptoStoreError> {
        let olm = self.client.olm_machine().await;
        let Some(olm) = olm.as_ref() else { return Ok(None) };
        let identity = olm.get_identity(user_id, None).await?;

        Ok(identity.map(|i| UserIdentity::new(self.client.clone(), i)))
    }

    /// Get the E2EE identity of a user from the homeserver.
    ///
    /// The E2EE identity returned is always guaranteed to be up-to-date. If the
    /// E2EE identity is not found, it should mean that the user did not set
    /// up cross-signing.
    ///
    /// If you want the E2EE identity of a user without making a request to the
    /// homeserver, use [`Encryption::get_user_identity()`] instead.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The ID of the user that the identity belongs to.
    ///
    /// Returns a [`UserIdentity`] if one is found. Returns an error if there
    /// was an issue with the crypto store or with the request to the
    /// homeserver.
    ///
    /// This will always return `None` if the client hasn't been logged in.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, ruma::user_id};
    /// # use url::Url;
    /// # async {
    /// # let alice = user_id!("@alice:example.org");
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// let user = client.encryption().request_user_identity(alice).await?;
    ///
    /// if let Some(user) = user {
    ///     println!("User is verified: {:?}", user.is_verified());
    ///
    ///     let verification = user.request_verification().await?;
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn request_user_identity(&self, user_id: &UserId) -> Result<Option<UserIdentity>> {
        let olm = self.client.olm_machine().await;
        let Some(olm) = olm.as_ref() else { return Ok(None) };

        let (request_id, request) = olm.query_keys_for_users(iter::once(user_id));
        self.client.keys_query(&request_id, request.device_keys).await?;

        let identity = olm.get_identity(user_id, None).await?;
        Ok(identity.map(|i| UserIdentity::new(self.client.clone(), i)))
    }

    /// Returns a stream of device updates, allowing users to listen for
    /// notifications about new or changed devices.
    ///
    /// The stream produced by this method emits updates whenever a new device
    /// is discovered or when an existing device's information is changed. Users
    /// can subscribe to this stream and receive updates in real-time.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use ruma::{device_id, user_id};
    /// # use futures_util::{pin_mut, StreamExt};
    /// # let client: Client = unimplemented!();
    /// # async {
    /// let devices_stream = client.encryption().devices_stream().await?;
    /// let user_id = client
    ///     .user_id()
    ///     .expect("We should know our user id after we have logged in");
    /// pin_mut!(devices_stream);
    ///
    /// for device_updates in devices_stream.next().await {
    ///     if let Some(user_devices) = device_updates.new.get(user_id) {
    ///         for device in user_devices.values() {
    ///             println!("A new device has been added {}", device.device_id());
    ///         }
    ///     }
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn devices_stream(&self) -> Result<impl Stream<Item = DeviceUpdates> + use<>> {
        let olm = self.client.olm_machine().await;
        let olm = olm.as_ref().ok_or(Error::NoOlmMachine)?;
        let client = self.client.to_owned();

        Ok(olm
            .store()
            .devices_stream()
            .map(move |updates| DeviceUpdates::new(client.to_owned(), updates)))
    }

    /// Returns a stream of user identity updates, allowing users to listen for
    /// notifications about new or changed user identities.
    ///
    /// The stream produced by this method emits updates whenever a new user
    /// identity is discovered or when an existing identities information is
    /// changed. Users can subscribe to this stream and receive updates in
    /// real-time.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use ruma::{device_id, user_id};
    /// # use futures_util::{pin_mut, StreamExt};
    /// # let client: Client = unimplemented!();
    /// # async {
    /// let identities_stream =
    ///     client.encryption().user_identities_stream().await?;
    /// pin_mut!(identities_stream);
    ///
    /// for identity_updates in identities_stream.next().await {
    ///     for (_, identity) in identity_updates.new {
    ///         println!("A new identity has been added {}", identity.user_id());
    ///     }
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn user_identities_stream(
        &self,
    ) -> Result<impl Stream<Item = IdentityUpdates> + use<>> {
        let olm = self.client.olm_machine().await;
        let olm = olm.as_ref().ok_or(Error::NoOlmMachine)?;
        let client = self.client.to_owned();

        Ok(olm
            .store()
            .user_identities_stream()
            .map(move |updates| IdentityUpdates::new(client.to_owned(), updates)))
    }

    /// Create and upload a new cross signing identity.
    ///
    /// # Arguments
    ///
    /// * `auth_data` - This request requires user interactive auth, the first
    ///   request needs to set this to `None` and will always fail with an
    ///   `UiaaResponse`. The response will contain information for the
    ///   interactive auth and the same request needs to be made but this time
    ///   with some `auth_data` provided.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::collections::BTreeMap;
    /// # use matrix_sdk::{ruma::api::client::uiaa, Client};
    /// # use url::Url;
    /// # use serde_json::json;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// if let Err(e) = client.encryption().bootstrap_cross_signing(None).await {
    ///     if let Some(response) = e.as_uiaa_response() {
    ///         let mut password = uiaa::Password::new(
    ///             uiaa::UserIdentifier::UserIdOrLocalpart("example".to_owned()),
    ///             "wordpass".to_owned(),
    ///         );
    ///         password.session = response.session.clone();
    ///
    ///         client
    ///             .encryption()
    ///             .bootstrap_cross_signing(Some(uiaa::AuthData::Password(password)))
    ///             .await
    ///             .expect("Couldn't bootstrap cross signing")
    ///     } else {
    ///         panic!("Error during cross signing bootstrap {:#?}", e);
    ///     }
    /// }
    /// # anyhow::Ok(()) };
    pub async fn bootstrap_cross_signing(&self, auth_data: Option<AuthData>) -> Result<()> {
        let olm = self.client.olm_machine().await;
        let olm = olm.as_ref().ok_or(Error::NoOlmMachine)?;

        let CrossSigningBootstrapRequests {
            upload_signing_keys_req,
            upload_keys_req,
            upload_signatures_req,
        } = olm.bootstrap_cross_signing(false).await?;

        let upload_signing_keys_req = assign!(UploadSigningKeysRequest::new(), {
            auth: auth_data,
            master_key: upload_signing_keys_req.master_key.map(|c| c.to_raw()),
            self_signing_key: upload_signing_keys_req.self_signing_key.map(|c| c.to_raw()),
            user_signing_key: upload_signing_keys_req.user_signing_key.map(|c| c.to_raw()),
        });

        if let Some(req) = upload_keys_req {
            self.client.send_outgoing_request(req).await?;
        }
        self.client.send(upload_signing_keys_req).await?;
        self.client.send(upload_signatures_req).await?;

        Ok(())
    }

    /// Reset the cross-signing keys.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use matrix_sdk::{ruma::api::client::uiaa, Client, encryption::CrossSigningResetAuthType};
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// # let user_id = unimplemented!();
    /// let encryption = client.encryption();
    ///
    /// if let Some(handle) = encryption.reset_cross_signing().await? {
    ///     match handle.auth_type() {
    ///         CrossSigningResetAuthType::Uiaa(uiaa) => {
    ///             use matrix_sdk::ruma::api::client::uiaa;
    ///
    ///             let password = "1234".to_owned();
    ///             let mut password = uiaa::Password::new(user_id, password);
    ///             password.session = uiaa.session;
    ///
    ///             handle.auth(Some(uiaa::AuthData::Password(password))).await?;
    ///         }
    ///         CrossSigningResetAuthType::OAuth(o) => {
    ///             println!(
    ///                 "To reset your end-to-end encryption cross-signing identity, \
    ///                 you first need to approve it at {}",
    ///                 o.approval_url
    ///             );
    ///             handle.auth(None).await?;
    ///         }
    ///     }
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn reset_cross_signing(&self) -> Result<Option<CrossSigningResetHandle>> {
        let olm = self.client.olm_machine().await;
        let olm = olm.as_ref().ok_or(Error::NoOlmMachine)?;

        let CrossSigningBootstrapRequests {
            upload_keys_req,
            upload_signing_keys_req,
            upload_signatures_req,
        } = olm.bootstrap_cross_signing(true).await?;

        let upload_signing_keys_req = assign!(UploadSigningKeysRequest::new(), {
            auth: None,
            master_key: upload_signing_keys_req.master_key.map(|c| c.to_raw()),
            self_signing_key: upload_signing_keys_req.self_signing_key.map(|c| c.to_raw()),
            user_signing_key: upload_signing_keys_req.user_signing_key.map(|c| c.to_raw()),
        });

        if let Some(req) = upload_keys_req {
            self.client.send_outgoing_request(req).await?;
        }

        if let Err(error) = self.client.send(upload_signing_keys_req.clone()).await {
            if let Ok(Some(auth_type)) = CrossSigningResetAuthType::new(&error) {
                let client = self.client.clone();

                Ok(Some(CrossSigningResetHandle::new(
                    client,
                    upload_signing_keys_req,
                    upload_signatures_req,
                    auth_type,
                )))
            } else {
                Err(error.into())
            }
        } else {
            self.client.send(upload_signatures_req).await?;

            Ok(None)
        }
    }

    /// Query the user's own device keys, if, and only if, we didn't have their
    /// identity in the first place.
    async fn ensure_initial_key_query(&self) -> Result<()> {
        let olm_machine = self.client.olm_machine().await;
        let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;

        let user_id = olm_machine.user_id();

        if self.client.encryption().get_user_identity(user_id).await?.is_none() {
            let (request_id, request) = olm_machine.query_keys_for_users([olm_machine.user_id()]);
            self.client.keys_query(&request_id, request.device_keys).await?;
        }

        Ok(())
    }

    /// Create and upload a new cross signing identity, if that has not been
    /// done yet.
    ///
    /// This will only create a new cross-signing identity if the user had never
    /// done it before. If the user did it before, then this is a no-op.
    ///
    /// See also the documentation of [`Self::bootstrap_cross_signing`] for the
    /// behavior of this function.
    ///
    /// # Arguments
    ///
    /// * `auth_data` - This request requires user interactive auth, the first
    ///   request needs to set this to `None` and will always fail with an
    ///   `UiaaResponse`. The response will contain information for the
    ///   interactive auth and the same request needs to be made but this time
    ///   with some `auth_data` provided.
    ///
    /// # Examples
    /// ```no_run
    /// # use std::collections::BTreeMap;
    /// # use matrix_sdk::{ruma::api::client::uiaa, Client};
    /// # use url::Url;
    /// # use serde_json::json;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// if let Err(e) = client.encryption().bootstrap_cross_signing_if_needed(None).await {
    ///     if let Some(response) = e.as_uiaa_response() {
    ///         let mut password = uiaa::Password::new(
    ///             uiaa::UserIdentifier::UserIdOrLocalpart("example".to_owned()),
    ///             "wordpass".to_owned(),
    ///         );
    ///         password.session = response.session.clone();
    ///
    ///         // Note, on the failed attempt we can use `bootstrap_cross_signing` immediately, to
    ///         // avoid checks.
    ///         client
    ///             .encryption()
    ///             .bootstrap_cross_signing(Some(uiaa::AuthData::Password(password)))
    ///             .await
    ///             .expect("Couldn't bootstrap cross signing")
    ///     } else {
    ///         panic!("Error during cross signing bootstrap {:#?}", e);
    ///     }
    /// }
    /// # anyhow::Ok(()) };
    pub async fn bootstrap_cross_signing_if_needed(
        &self,
        auth_data: Option<AuthData>,
    ) -> Result<()> {
        let olm_machine = self.client.olm_machine().await;
        let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;
        let user_id = olm_machine.user_id();

        self.ensure_initial_key_query().await?;

        if self.client.encryption().get_user_identity(user_id).await?.is_none() {
            self.bootstrap_cross_signing(auth_data).await?;
        }

        Ok(())
    }

    /// Export E2EE keys that match the given predicate encrypting them with the
    /// given passphrase.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path where the exported key file will be saved.
    ///
    /// * `passphrase` - The passphrase that will be used to encrypt the
    ///   exported room keys.
    ///
    /// * `predicate` - A closure that will be called for every known
    ///   `InboundGroupSession`, which represents a room key. If the closure
    ///   returns `true` the `InboundGroupSessoin` will be included in the
    ///   export, if the closure returns `false` it will not be included.
    ///
    /// # Panics
    ///
    /// This method will panic if it isn't run on a Tokio runtime.
    ///
    /// This method will panic if it can't get enough randomness from the OS to
    /// encrypt the exported keys securely.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::{path::PathBuf, time::Duration};
    /// # use matrix_sdk::{
    /// #     Client, config::SyncSettings,
    /// #     ruma::room_id,
    /// # };
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// let path = PathBuf::from("/home/example/e2e-keys.txt");
    /// // Export all room keys.
    /// client
    ///     .encryption()
    ///     .export_room_keys(path, "secret-passphrase", |_| true)
    ///     .await?;
    ///
    /// // Export only the room keys for a certain room.
    /// let path = PathBuf::from("/home/example/e2e-room-keys.txt");
    /// let room_id = room_id!("!test:localhost");
    ///
    /// client
    ///     .encryption()
    ///     .export_room_keys(path, "secret-passphrase", |s| s.room_id() == room_id)
    ///     .await?;
    /// # anyhow::Ok(()) };
    /// ```
    #[cfg(not(target_family = "wasm"))]
    pub async fn export_room_keys(
        &self,
        path: PathBuf,
        passphrase: &str,
        predicate: impl FnMut(&matrix_sdk_base::crypto::olm::InboundGroupSession) -> bool,
    ) -> Result<()> {
        let olm = self.client.olm_machine().await;
        let olm = olm.as_ref().ok_or(Error::NoOlmMachine)?;

        let keys = olm.store().export_room_keys(predicate).await?;
        let passphrase = zeroize::Zeroizing::new(passphrase.to_owned());

        let encrypt = move || -> Result<()> {
            let export: String =
                matrix_sdk_base::crypto::encrypt_room_key_export(&keys, &passphrase, 500_000)?;
            let mut file = std::fs::File::create(path)?;
            file.write_all(&export.into_bytes())?;
            Ok(())
        };

        let task = tokio::task::spawn_blocking(encrypt);
        task.await.expect("Task join error")
    }

    /// Import E2EE keys from the given file path.
    ///
    /// # Arguments
    ///
    /// * `path` - The file path where the exported key file will can be found.
    ///
    /// * `passphrase` - The passphrase that should be used to decrypt the
    ///   exported room keys.
    ///
    /// Returns a tuple of numbers that represent the number of sessions that
    /// were imported and the total number of sessions that were found in the
    /// key export.
    ///
    /// # Panics
    ///
    /// This method will panic if it isn't run on a Tokio runtime.
    ///
    /// ```no_run
    /// # use std::{path::PathBuf, time::Duration};
    /// # use matrix_sdk::{
    /// #     Client, config::SyncSettings,
    /// #     ruma::room_id,
    /// # };
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// let path = PathBuf::from("/home/example/e2e-keys.txt");
    /// let result =
    ///     client.encryption().import_room_keys(path, "secret-passphrase").await?;
    ///
    /// println!(
    ///     "Imported {} room keys out of {}",
    ///     result.imported_count, result.total_count
    /// );
    /// # anyhow::Ok(()) };
    /// ```
    #[cfg(not(target_family = "wasm"))]
    pub async fn import_room_keys(
        &self,
        path: PathBuf,
        passphrase: &str,
    ) -> Result<RoomKeyImportResult, RoomKeyImportError> {
        let olm = self.client.olm_machine().await;
        let olm = olm.as_ref().ok_or(RoomKeyImportError::StoreClosed)?;
        let passphrase = zeroize::Zeroizing::new(passphrase.to_owned());

        let decrypt = move || {
            let file = std::fs::File::open(path)?;
            matrix_sdk_base::crypto::decrypt_room_key_export(file, &passphrase)
        };

        let task = tokio::task::spawn_blocking(decrypt);
        let import = task.await.expect("Task join error")?;

        let ret = olm.store().import_exported_room_keys(import, |_, _| {}).await?;

        self.backups().maybe_trigger_backup();

        Ok(ret)
    }

    /// Receive notifications of room keys being received as a [`Stream`].
    ///
    /// Each time a room key is updated in any way, an update will be sent to
    /// the stream. Updates that happen at the same time are batched into a
    /// [`Vec`].
    ///
    /// If the reader of the stream lags too far behind, an error is broadcast
    /// containing the number of skipped items.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// use futures_util::StreamExt;
    ///
    /// let Some(mut room_keys_stream) =
    ///     client.encryption().room_keys_received_stream().await
    /// else {
    ///     return Ok(());
    /// };
    ///
    /// while let Some(update) = room_keys_stream.next().await {
    ///     println!("Received room keys {update:?}");
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn room_keys_received_stream(
        &self,
    ) -> Option<impl Stream<Item = Result<Vec<RoomKeyInfo>, BroadcastStreamRecvError>> + use<>>
    {
        let olm = self.client.olm_machine().await;
        let olm = olm.as_ref()?;

        Some(olm.store().room_keys_received_stream())
    }

    /// Receive notifications of historic room key bundles as a [`Stream`].
    ///
    /// Historic room key bundles are defined in [MSC4268](https://github.com/matrix-org/matrix-spec-proposals/pull/4268).
    ///
    /// Each time a historic room key bundle was received, an update will be
    /// sent to the stream. This stream is useful for informative purposes
    /// exclusively, historic room key bundles are handled by the SDK
    /// automatically.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// use futures_util::StreamExt;
    ///
    /// let Some(mut bundle_stream) =
    ///     client.encryption().historic_room_key_stream().await
    /// else {
    ///     return Ok(());
    /// };
    ///
    /// while let Some(bundle_info) = bundle_stream.next().await {
    ///     println!("Received a historic room key bundle {bundle_info:?}");
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn historic_room_key_stream(
        &self,
    ) -> Option<impl Stream<Item = RoomKeyBundleInfo> + use<>> {
        let olm = self.client.olm_machine().await;
        let olm = olm.as_ref()?;

        Some(olm.store().historic_room_key_stream())
    }

    /// Get the secret storage manager of the client.
    pub fn secret_storage(&self) -> SecretStorage {
        SecretStorage { client: self.client.to_owned() }
    }

    /// Get the backups manager of the client.
    pub fn backups(&self) -> Backups {
        Backups { client: self.client.to_owned() }
    }

    /// Get the recovery manager of the client.
    pub fn recovery(&self) -> Recovery {
        Recovery { client: self.client.to_owned() }
    }

    /// Enables the crypto-store cross-process lock.
    ///
    /// This may be required if there are multiple processes that may do writes
    /// to the same crypto store. In that case, it's necessary to create a
    /// lock, so that only one process writes to it, otherwise this may
    /// cause confusing issues because of stale data contained in in-memory
    /// caches.
    ///
    /// The provided `lock_value` must be a unique identifier for this process.
    /// Check [`Client::cross_process_store_locks_holder_name`] to
    /// get the global value.
    pub async fn enable_cross_process_store_lock(&self, lock_value: String) -> Result<(), Error> {
        // If the lock has already been created, don't recreate it from scratch.
        if let Some(prev_lock) = self.client.locks().cross_process_crypto_store_lock.get() {
            let prev_holder = prev_lock.lock_holder();
            if prev_holder == lock_value {
                return Ok(());
            }
            warn!(
                "Recreating cross-process store lock with a different holder value: \
                 prev was {prev_holder}, new is {lock_value}"
            );
        }

        let olm_machine = self.client.base_client().olm_machine().await;
        let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;

        let lock =
            olm_machine.store().create_store_lock("cross_process_lock".to_owned(), lock_value);

        // Gently try to initialize the crypto store generation counter.
        //
        // If we don't get the lock immediately, then it is already acquired by another
        // process, and we'll get to reload next time we acquire the lock.
        {
            let lock_result = lock.try_lock_once().await?;

            if lock_result.is_ok() {
                olm_machine
                    .initialize_crypto_store_generation(
                        &self.client.locks().crypto_store_generation,
                    )
                    .await?;
            }
        }

        self.client
            .locks()
            .cross_process_crypto_store_lock
            .set(lock)
            .map_err(|_| Error::BadCryptoStoreState)?;

        Ok(())
    }

    /// Maybe reload the `OlmMachine` after acquiring the lock for the first
    /// time.
    ///
    /// Returns the current generation number.
    async fn on_lock_newly_acquired(&self) -> Result<u64, Error> {
        let olm_machine_guard = self.client.olm_machine().await;
        if let Some(olm_machine) = olm_machine_guard.as_ref() {
            let (new_gen, generation_number) = olm_machine
                .maintain_crypto_store_generation(&self.client.locks().crypto_store_generation)
                .await?;
            // If the crypto store generation has changed,
            if new_gen {
                // (get rid of the reference to the current crypto store first)
                drop(olm_machine_guard);
                // Recreate the OlmMachine.
                self.client.base_client().regenerate_olm(None).await?;
            }
            Ok(generation_number)
        } else {
            // XXX: not sure this is reachable. Seems like the OlmMachine should always have
            // been initialised by the time we get here. Ideally we'd panic, or return an
            // error, but for now I'm just adding some logging to check if it
            // happens, and returning the magic number 0.
            warn!("Encryption::on_lock_newly_acquired: called before OlmMachine initialised");
            Ok(0)
        }
    }

    /// If a lock was created with [`Self::enable_cross_process_store_lock`],
    /// spin-waits until the lock is available.
    ///
    /// May reload the `OlmMachine`, after obtaining the lock but not on the
    /// first time.
    pub async fn spin_lock_store(
        &self,
        max_backoff: Option<u32>,
    ) -> Result<Option<CrossProcessLockStoreGuardWithGeneration>, Error> {
        if let Some(lock) = self.client.locks().cross_process_crypto_store_lock.get() {
            let guard = lock
                .spin_lock(max_backoff)
                .await
                .map_err(|err| {
                    Error::CrossProcessLockError(Box::new(CrossProcessLockError::TryLock(
                        Box::new(err),
                    )))
                })?
                .map_err(|err| Error::CrossProcessLockError(Box::new(err.into())))?;

            let generation = self.on_lock_newly_acquired().await?;

            Ok(Some(CrossProcessLockStoreGuardWithGeneration {
                _guard: guard.into_guard(),
                generation,
            }))
        } else {
            Ok(None)
        }
    }

    /// If a lock was created with [`Self::enable_cross_process_store_lock`],
    /// attempts to lock it once.
    ///
    /// Returns a guard to the lock, if it was obtained.
    pub async fn try_lock_store_once(
        &self,
    ) -> Result<Option<CrossProcessLockStoreGuardWithGeneration>, Error> {
        if let Some(lock) = self.client.locks().cross_process_crypto_store_lock.get() {
            let lock_result = lock.try_lock_once().await?;

            let Some(guard) = lock_result.ok() else {
                return Ok(None);
            };

            let generation = self.on_lock_newly_acquired().await?;

            Ok(Some(CrossProcessLockStoreGuardWithGeneration {
                _guard: guard.into_guard(),
                generation,
            }))
        } else {
            Ok(None)
        }
    }

    /// Testing purposes only.
    #[cfg(any(test, feature = "testing"))]
    pub async fn uploaded_key_count(&self) -> Result<u64> {
        let olm_machine = self.client.olm_machine().await;
        let olm_machine = olm_machine.as_ref().ok_or(Error::AuthenticationRequired)?;
        Ok(olm_machine.uploaded_key_count().await?)
    }

    /// Bootstrap encryption and enables event listeners for the E2EE support.
    ///
    /// Based on the `EncryptionSettings`, this call might:
    /// - Bootstrap cross-signing if needed (POST `/device_signing/upload`)
    /// - Create a key backup if needed (POST `/room_keys/version`)
    /// - Create a secret storage if needed (PUT `/account_data/{type}`)
    ///
    /// As part of this process, and if needed, the current device keys would be
    /// uploaded to the server, new account data would be added, and cross
    /// signing keys and signatures might be uploaded.
    ///
    /// Should be called once we
    /// created a [`OlmMachine`], i.e. after logging in.
    ///
    /// # Arguments
    ///
    /// * `auth_data` - Some requests may require re-authentication. To prevent
    ///   the user from having to re-enter their password (or use other
    ///   methods), we can provide the authentication data here. This is
    ///   necessary for uploading cross-signing keys. However, please note that
    ///   there is a proposal (MSC3967) to remove this requirement, which would
    ///   allow for the initial upload of cross-signing keys without
    ///   authentication, rendering this parameter obsolete.
    pub(crate) async fn spawn_initialization_task(&self, auth_data: Option<AuthData>) {
        // It's fine to be async here as we're only getting the lock protecting the
        // `OlmMachine`. Since the lock shouldn't be that contested right after logging
        // in we won't delay the login or restoration of the Client.
        let bundle_receiver_task = if self.client.inner.enable_share_history_on_invite {
            Some(BundleReceiverTask::new(&self.client).await)
        } else {
            None
        };

        let mut tasks = self.client.inner.e2ee.tasks.lock();

        let this = self.clone();

        tasks.setup_e2ee = Some(spawn(async move {
            // Update the current state first, so we don't have to wait for the result of
            // network requests
            this.update_verification_state().await;

            if this.settings().auto_enable_cross_signing
                && let Err(e) = this.bootstrap_cross_signing_if_needed(auth_data).await
            {
                error!("Couldn't bootstrap cross signing {e:?}");
            }

            if let Err(e) = this.backups().setup_and_resume().await {
                error!("Couldn't setup and resume backups {e:?}");
            }
            if let Err(e) = this.recovery().setup().await {
                error!("Couldn't setup and resume recovery {e:?}");
            }
        }));

        tasks.receive_historic_room_key_bundles = bundle_receiver_task;
    }

    /// Waits for end-to-end encryption initialization tasks to finish, if any
    /// was running in the background.
    pub async fn wait_for_e2ee_initialization_tasks(&self) {
        let task = self.client.inner.e2ee.tasks.lock().setup_e2ee.take();

        if let Some(task) = task
            && let Err(err) = task.await
        {
            warn!("Error when initializing backups: {err}");
        }
    }

    /// Upload the device keys and initial set of one-time keys to the server.
    ///
    /// This should only be called when the user logs in for the first time,
    /// the method will ensure that other devices see our own device as an
    /// end-to-end encryption enabled one.
    ///
    /// **Warning**: Do not use this method if we're already calling
    /// [`Client::send_outgoing_request()`]. This method is intended for
    /// explicitly uploading the device keys before starting a sync.
    pub(crate) async fn ensure_device_keys_upload(&self) -> Result<()> {
        let olm = self.client.olm_machine().await;
        let olm = olm.as_ref().ok_or(Error::NoOlmMachine)?;

        if let Some((request_id, request)) = olm.upload_device_keys().await? {
            self.client.keys_upload(&request_id, &request).await?;

            let (request_id, request) = olm.query_keys_for_users([olm.user_id()]);
            self.client.keys_query(&request_id, request.device_keys).await?;
        }

        Ok(())
    }

    pub(crate) async fn update_state_after_keys_query(&self, response: &get_keys::v3::Response) {
        self.recovery().update_state_after_keys_query(response).await;

        // Only update the verification_state if our own devices changed
        if let Some(user_id) = self.client.user_id() {
            let contains_own_device = response.device_keys.contains_key(user_id);

            if contains_own_device {
                self.update_verification_state().await;
            }
        }
    }

    async fn update_verification_state(&self) {
        match self.get_own_device().await {
            Ok(device) => {
                if let Some(device) = device {
                    let is_verified = device.is_cross_signed_by_owner();

                    if is_verified {
                        self.client.inner.verification_state.set(VerificationState::Verified);
                    } else {
                        self.client.inner.verification_state.set(VerificationState::Unverified);
                    }
                } else {
                    warn!("Couldn't find out own device in the store.");
                    self.client.inner.verification_state.set(VerificationState::Unknown);
                }
            }
            Err(error) => {
                warn!("Failed retrieving own device: {error}");
                self.client.inner.verification_state.set(VerificationState::Unknown);
            }
        }
    }

    /// Encrypts then send the given content via the `/sendToDevice` end-point
    /// using Olm encryption.
    ///
    /// If there are a lot of recipient devices multiple `/sendToDevice`
    /// requests might be sent out.
    ///
    /// # Returns
    /// A list of failures. The list of devices that couldn't get the messages.
    #[cfg(feature = "experimental-send-custom-to-device")]
    pub async fn encrypt_and_send_raw_to_device(
        &self,
        recipient_devices: Vec<&Device>,
        event_type: &str,
        content: Raw<AnyToDeviceEventContent>,
        share_strategy: CollectStrategy,
    ) -> Result<Vec<(OwnedUserId, OwnedDeviceId)>> {
        let users = recipient_devices.iter().map(|device| device.user_id());

        // Will claim one-time-key for users that needs it
        // TODO: For later optimisation: This will establish missing olm sessions with
        // all this users devices, but we just want for some devices.
        self.client.claim_one_time_keys(users).await?;

        let olm = self.client.olm_machine().await;
        let olm = olm.as_ref().expect("Olm machine wasn't started");

        let (requests, withhelds) = olm
            .encrypt_content_for_devices(
                recipient_devices.into_iter().map(|d| d.deref().clone()).collect(),
                event_type,
                &content
                    .deserialize_as::<serde_json::Value>()
                    .expect("Deserialize as Value will always work"),
                share_strategy,
            )
            .await?;

        let mut failures: Vec<(OwnedUserId, OwnedDeviceId)> = Default::default();

        // Push the withhelds in the failures
        withhelds.iter().for_each(|(d, _)| {
            failures.push((d.user_id().to_owned(), d.device_id().to_owned()));
        });

        // TODO: parallelize that? it's already grouping 250 devices per chunk.
        for request in requests {
            let ruma_request = RumaToDeviceRequest::new_raw(
                request.event_type.clone(),
                request.txn_id.clone(),
                request.messages.clone(),
            );

            let send_result = self
                .client
                .send_inner(ruma_request, Some(RequestConfig::short_retry()), Default::default())
                .await;

            // If the sending failed we need to collect the failures to report them
            if send_result.is_err() {
                // Mark the sending as failed
                for (user_id, device_map) in request.messages {
                    for device_id in device_map.keys() {
                        match device_id {
                            DeviceIdOrAllDevices::DeviceId(device_id) => {
                                failures.push((user_id.clone(), device_id.to_owned()));
                            }
                            DeviceIdOrAllDevices::AllDevices => {
                                // Cannot happen in this case
                            }
                        }
                    }
                }
            }
        }

        Ok(failures)
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod tests {
    use std::{
        ops::Not,
        str::FromStr,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::Duration,
    };

    use matrix_sdk_test::{
        DEFAULT_TEST_ROOM_ID, JoinedRoomBuilder, StateTestEvent, SyncResponseBuilder, async_test,
        test_json,
    };
    use ruma::{
        event_id,
        events::{reaction::ReactionEventContent, relation::Annotation},
    };
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, Request, ResponseTemplate,
        matchers::{header, method, path_regex},
    };

    use crate::{
        Client, assert_next_matches_with_timeout,
        config::RequestConfig,
        encryption::{
            DuplicateOneTimeKeyErrorMessage, OAuthCrossSigningResetInfo, VerificationState,
        },
        test_utils::{
            client::mock_matrix_session, logged_in_client, no_retry_test_client, set_client_session,
        },
    };

    #[async_test]
    async fn test_reaction_sending() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let event_id = event_id!("$2:example.org");

        Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/m.*room.*encryption.?"))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(&*test_json::sync_events::ENCRYPTION_CONTENT),
            )
            .mount(&server)
            .await;

        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/m\.reaction/.*".to_owned()))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "event_id": event_id,
            })))
            .mount(&server)
            .await;

        let response = SyncResponseBuilder::default()
            .add_joined_room(
                JoinedRoomBuilder::default()
                    .add_state_event(StateTestEvent::Member)
                    .add_state_event(StateTestEvent::PowerLevels)
                    .add_state_event(StateTestEvent::Encryption),
            )
            .build_sync_response();

        client.base_client().receive_sync_response(response).await.unwrap();

        let room = client.get_room(&DEFAULT_TEST_ROOM_ID).expect("Room should exist");
        assert!(
            room.latest_encryption_state().await.expect("Getting encryption state").is_encrypted()
        );

        let event_id = event_id!("$1:example.org");
        let reaction = ReactionEventContent::new(Annotation::new(event_id.into(), "🐈".to_owned()));
        room.send(reaction).await.expect("Sending the reaction should not fail");

        room.send_raw("m.reaction", json!({})).await.expect("Sending the reaction should not fail");
    }

    #[cfg(feature = "sqlite")]
    #[async_test]
    async fn test_generation_counter_invalidates_olm_machine() {
        // Create two clients using the same sqlite database.

        use matrix_sdk_base::store::RoomLoadSettings;
        let sqlite_path = std::env::temp_dir().join("generation_counter_sqlite.db");
        let session = mock_matrix_session();

        let client1 = Client::builder()
            .homeserver_url("http://localhost:1234")
            .request_config(RequestConfig::new().disable_retry())
            .sqlite_store(&sqlite_path, None)
            .build()
            .await
            .unwrap();
        client1
            .matrix_auth()
            .restore_session(session.clone(), RoomLoadSettings::default())
            .await
            .unwrap();

        let client2 = Client::builder()
            .homeserver_url("http://localhost:1234")
            .request_config(RequestConfig::new().disable_retry())
            .sqlite_store(sqlite_path, None)
            .build()
            .await
            .unwrap();
        client2.matrix_auth().restore_session(session, RoomLoadSettings::default()).await.unwrap();

        // When the lock isn't enabled, any attempt at locking won't return a guard.
        let guard = client1.encryption().try_lock_store_once().await.unwrap();
        assert!(guard.is_none());

        client1.encryption().enable_cross_process_store_lock("client1".to_owned()).await.unwrap();
        client2.encryption().enable_cross_process_store_lock("client2".to_owned()).await.unwrap();

        // One client can take the lock.
        let acquired1 = client1.encryption().try_lock_store_once().await.unwrap();
        assert!(acquired1.is_some());

        // Keep the olm machine, so we can see if it's changed later, by comparing Arcs.
        let initial_olm_machine =
            client1.olm_machine().await.clone().expect("must have an olm machine");

        // Also enable backup to check that new machine has the same backup keys.
        let decryption_key = matrix_sdk_base::crypto::store::types::BackupDecryptionKey::new()
            .expect("Can't create new recovery key");
        let backup_key = decryption_key.megolm_v1_public_key();
        backup_key.set_version("1".to_owned());
        initial_olm_machine
            .backup_machine()
            .save_decryption_key(Some(decryption_key.to_owned()), Some("1".to_owned()))
            .await
            .expect("Should save");

        initial_olm_machine.backup_machine().enable_backup_v1(backup_key.clone()).await.unwrap();

        assert!(client1.encryption().backups().are_enabled().await);

        // The other client can't take the lock too.
        let acquired2 = client2.encryption().try_lock_store_once().await.unwrap();
        assert!(acquired2.is_none());

        // Now have the first client release the lock,
        drop(acquired1);
        tokio::time::sleep(Duration::from_millis(100)).await;

        // And re-take it.
        let acquired1 = client1.encryption().try_lock_store_once().await.unwrap();
        assert!(acquired1.is_some());

        // In that case, the Olm Machine shouldn't change.
        let olm_machine = client1.olm_machine().await.clone().expect("must have an olm machine");
        assert!(initial_olm_machine.same_as(&olm_machine));

        // Ok, release again.
        drop(acquired1);
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Client2 can acquire the lock.
        let acquired2 = client2.encryption().try_lock_store_once().await.unwrap();
        assert!(acquired2.is_some());

        // And then release it.
        drop(acquired2);
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Client1 can acquire it again,
        let acquired1 = client1.encryption().try_lock_store_once().await.unwrap();
        assert!(acquired1.is_some());

        // But now its olm machine has been invalidated and thus regenerated!
        let olm_machine = client1.olm_machine().await.clone().expect("must have an olm machine");

        assert!(!initial_olm_machine.same_as(&olm_machine));

        let backup_key_new = olm_machine.backup_machine().get_backup_keys().await.unwrap();
        assert!(backup_key_new.decryption_key.is_some());
        assert_eq!(
            backup_key_new.decryption_key.unwrap().megolm_v1_public_key().to_base64(),
            backup_key.to_base64()
        );
        assert!(client1.encryption().backups().are_enabled().await);
    }

    #[cfg(feature = "sqlite")]
    #[async_test]
    async fn test_generation_counter_no_spurious_invalidation() {
        // Create two clients using the same sqlite database.

        use matrix_sdk_base::store::RoomLoadSettings;
        let sqlite_path =
            std::env::temp_dir().join("generation_counter_no_spurious_invalidations.db");
        let session = mock_matrix_session();

        let client = Client::builder()
            .homeserver_url("http://localhost:1234")
            .request_config(RequestConfig::new().disable_retry())
            .sqlite_store(&sqlite_path, None)
            .build()
            .await
            .unwrap();
        client
            .matrix_auth()
            .restore_session(session.clone(), RoomLoadSettings::default())
            .await
            .unwrap();

        let initial_olm_machine = client.olm_machine().await.as_ref().unwrap().clone();

        client.encryption().enable_cross_process_store_lock("client1".to_owned()).await.unwrap();

        // Enabling the lock doesn't update the olm machine.
        let after_enabling_lock = client.olm_machine().await.as_ref().unwrap().clone();
        assert!(initial_olm_machine.same_as(&after_enabling_lock));

        {
            // Simulate that another client hold the lock before.
            let client2 = Client::builder()
                .homeserver_url("http://localhost:1234")
                .request_config(RequestConfig::new().disable_retry())
                .sqlite_store(sqlite_path, None)
                .build()
                .await
                .unwrap();
            client2
                .matrix_auth()
                .restore_session(session, RoomLoadSettings::default())
                .await
                .unwrap();

            client2
                .encryption()
                .enable_cross_process_store_lock("client2".to_owned())
                .await
                .unwrap();

            let guard = client2.encryption().spin_lock_store(None).await.unwrap();
            assert!(guard.is_some());

            drop(guard);
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        {
            let acquired = client.encryption().try_lock_store_once().await.unwrap();
            assert!(acquired.is_some());
        }

        // Taking the lock the first time will update the olm machine.
        let after_taking_lock_first_time = client.olm_machine().await.as_ref().unwrap().clone();
        assert!(!initial_olm_machine.same_as(&after_taking_lock_first_time));

        {
            let acquired = client.encryption().try_lock_store_once().await.unwrap();
            assert!(acquired.is_some());
        }

        // Re-taking the lock doesn't update the olm machine.
        let after_taking_lock_second_time = client.olm_machine().await.as_ref().unwrap().clone();
        assert!(after_taking_lock_first_time.same_as(&after_taking_lock_second_time));
    }

    #[async_test]
    async fn test_update_verification_state_is_updated_before_any_requests_happen() {
        // Given a client and a server
        let client = no_retry_test_client(None).await;
        let server = MockServer::start().await;

        // When we subscribe to its verification state
        let mut verification_state = client.encryption().verification_state();

        // We can get its initial value, and it's Unknown
        assert_next_matches_with_timeout!(verification_state, VerificationState::Unknown);

        // We set up a mocked request to check this endpoint is not called before
        // reading the new state
        let keys_requested = Arc::new(AtomicBool::new(false));
        let inner_bool = keys_requested.clone();

        Mock::given(method("GET"))
            .and(path_regex(
                r"/_matrix/client/r0/user/.*/account_data/m.secret_storage.default_key",
            ))
            .respond_with(move |_req: &Request| {
                inner_bool.fetch_or(true, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(json!({}))
            })
            .mount(&server)
            .await;

        // When the session is initialised and the encryption tasks spawn
        set_client_session(&client).await;

        // Then we can get an updated value without waiting for any network requests
        assert!(keys_requested.load(Ordering::SeqCst).not());
        assert_next_matches_with_timeout!(verification_state, VerificationState::Unverified);
    }

    #[test]
    fn test_oauth_reset_info_from_uiaa_info() {
        let auth_info = json!({
            "session": "dummy",
            "flows": [
                {
                    "stages": [
                        "org.matrix.cross_signing_reset"
                    ]
                }
            ],
            "params": {
                "org.matrix.cross_signing_reset": {
                    "url": "https://example.org/account/account?action=org.matrix.cross_signing_reset"
                }
            },
            "msg": "To reset..."
        });

        let auth_info = serde_json::from_value(auth_info)
            .expect("We should be able to deserialize the UiaaInfo");
        OAuthCrossSigningResetInfo::from_auth_info(&auth_info)
            .expect("We should be able to fetch the cross-signing reset info from the auth info");
    }

    #[test]
    fn test_duplicate_one_time_key_error_parsing() {
        let message = concat!(
            r#"One time key signed_curve25519:AAAAAAAAAAA already exists. "#,
            r#"Old key: {"key":"dBcZBzQaiQYWf6rBPh2QypIOB/dxSoTeyaFaxNNbeHs","#,
            r#""signatures":{"@example:matrix.org":{"ed25519:AAAAAAAAAA":""#,
            r#"Fk45zHAbrd+1j9wZXLjL2Y/+DU/Mnz9yuvlfYBOOT7qExN2Jdud+5BAuNs8nZ/caS4wTF39Kg3zQpzaGERoCBg"}}};"#,
            r#" new key: {'key': 'CY0TWVK1/Kj3ZADuBcGe3UKvpT+IKAPMUsMeJhSDqno', "#,
            r#"'signatures': {'@example:matrix.org': {'ed25519:AAAAAAAAAA': "#,
            r#"'BQ9Gp0p+6srF+c8OyruqKKd9R4yaub3THYAyyBB/7X/rG8BwcAqFynzl1aGyFYun4Q+087a5OSiglCXI+/kQAA'}}}"#
        );
        let message = DuplicateOneTimeKeyErrorMessage::from_str(message)
            .expect("We should be able to parse the error message");

        assert_eq!(message.old_key.to_base64(), "dBcZBzQaiQYWf6rBPh2QypIOB/dxSoTeyaFaxNNbeHs");
        assert_eq!(message.new_key.to_base64(), "CY0TWVK1/Kj3ZADuBcGe3UKvpT+IKAPMUsMeJhSDqno");

        DuplicateOneTimeKeyErrorMessage::from_str("One time key already exists.")
            .expect_err("We shouldn't be able to parse an incomplete error message");
    }

    // Helper function for the test_devices_to_verify_against_* tests.  Make a
    // response to a /keys/query request using the given device keys and a
    // pre-defined set of cross-signing keys.
    fn devices_to_verify_against_keys_query_response(
        devices: Vec<serde_json::Value>,
    ) -> serde_json::Value {
        let device_keys: serde_json::Map<String, serde_json::Value> = devices
            .into_iter()
            .map(|device| (device.get("device_id").unwrap().as_str().unwrap().to_owned(), device))
            .collect();
        json!({
            "device_keys": {
                "@example:localhost": device_keys,
            },
            "master_keys": {
                "@example:localhost": {
                    "keys": {
                        "ed25519:PJklDgml7Xtt1Wr8jsWvB+lC5YD/bVDpHL+fYuItNxU": "PJklDgml7Xtt1Wr8jsWvB+lC5YD/bVDpHL+fYuItNxU",
                    },
                    "usage": ["master"],
                    "user_id": "@example:localhost",
                },
            },
            "self_signing_keys": {
                "@example:localhost": {
                    "keys": {
                        "ed25519:jobZVcxG+PBLwZMsF4XEJSJTVqOgDxd0Ud3J/bw3HYM": "jobZVcxG+PBLwZMsF4XEJSJTVqOgDxd0Ud3J/bw3HYM",
                    },
                    "usage": ["self_signing"],
                    "user_id": "@example:localhost",
                    "signatures": {
                        "@example:localhost": {
                            "ed25519:PJklDgml7Xtt1Wr8jsWvB+lC5YD/bVDpHL+fYuItNxU": "etO1bB+rCk+TQ/FcjQ8eWu/RsRNQNNQ1Ek+PD6//j8yz6igRjfvuHZaMvr/quAFrirfgExph2TdOwlDgN5bFCQ",
                        },
                    },
                },
            },
            "user_signing_keys": {
                "@example:localhost": {
                    "keys": {
                        "ed25519:CBaovtekFxzf2Ijjhk4B49drOH0/qmhBbptFlVW7HC0": "CBaovtekFxzf2Ijjhk4B49drOH0/qmhBbptFlVW7HC0",
                    },
                    "usage": ["user_signing"],
                    "user_id": "@example:localhost",
                    "signatures": {
                        "@example:localhost": {
                            "ed25519:PJklDgml7Xtt1Wr8jsWvB+lC5YD/bVDpHL+fYuItNxU": "E/DFi/hQTIb/7eSB+HbCXeTLFaLjqWHzLO9GwjL1qdhfO7ew4p6YdtXSH3T2YYr1dKCPteH/4nMYVwOhww2CBg",
                        },
                    },
                },
            }
        })
    }

    // The following three tests test that we can detect whether the user has
    // other devices that they can verify against under different conditions.
    #[async_test]
    /// Test that we detect that can't verify against another device if we have
    /// no devices.
    async fn test_devices_to_verify_against_no_devices() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        Mock::given(method("POST"))
            .and(path_regex(r"^/_matrix/client/r0/keys/query".to_owned()))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(devices_to_verify_against_keys_query_response(vec![])),
            )
            .mount(&server)
            .await;

        assert!(!client.encryption().has_devices_to_verify_against().await.unwrap());
    }

    #[async_test]
    /// Test that we detect that we can verify against another cross-signed
    /// regular device.
    async fn test_devices_to_verify_against_cross_signed() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        Mock::given(method("POST"))
            .and(path_regex(r"^/_matrix/client/r0/keys/query".to_owned()))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                devices_to_verify_against_keys_query_response(vec![
                    json!({
                        "algorithms": [
                            "m.olm.v1.curve25519-aes-sha2",
                            "m.megolm.v1.aes-sha2",
                        ],
                        "user_id": "@example:localhost",
                        "device_id": "SIGNEDDEVICE",
                        "keys": {
                            "curve25519:SIGNEDDEVICE": "o1LqUtH/sqd3WF+BB2Qr77uw3sDmZhMOz68/IV9aHxs",
                            "ed25519:SIGNEDDEVICE": "iVoEfMOoUqxXVMLdpZCOgvQuCrT3/kQWkBmB3Phi/lo",
                        },
                        "signatures": {
                            "@example:localhost": {
                                "ed25519:SIGNEDDEVICE": "C7yRu1fNrdD2EobVdtANMqk3LBtWtTRWrIU22xVS8/Om1kmA/luzek64R3N6JsZhYczVmZYBKhUC9kRvHHwOBg",
                                "ed25519:jobZVcxG+PBLwZMsF4XEJSJTVqOgDxd0Ud3J/bw3HYM": "frfh2HP28GclmGvwTic00Fj4nZCvm4RlRA6U56mnD5920hOi04+L055ojzp6ybZXvC/GQYfyTHwQXlUN1nvxBA",
                            },
                        },
                    })
                ])
            ))
            .mount(&server)
            .await;

        assert!(client.encryption().has_devices_to_verify_against().await.unwrap());
    }

    #[async_test]
    /// Test that we detect that we can't verify against a dehydrated or
    /// unsigned device.
    async fn test_devices_to_verify_against_dehydrated_and_unsigned() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;
        let user_id = client.user_id().unwrap();
        let olm_machine = client.olm_machine().await;
        let olm_machine = olm_machine.as_ref().unwrap();

        Mock::given(method("POST"))
            .and(path_regex(r"^/_matrix/client/r0/keys/query".to_owned()))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                devices_to_verify_against_keys_query_response(vec![
                    json!({
                        "algorithms": [
                            "m.olm.v1.curve25519-aes-sha2",
                            "m.megolm.v1.aes-sha2",
                        ],
                        "user_id": "@example:localhost",
                        "device_id": "DEHYDRATEDDEVICE",
                        "keys": {
                            "curve25519:DEHYDRATEDDEVICE": "XOn5VguAgokZ3p9mBz2yOB395fn6j75G8jIPcXEWQGY",
                            "ed25519:DEHYDRATEDDEVICE": "4GG5xmBT7z4rgUgmWNlKZ+ABE3QlGgTorF+luCnKfYI",
                        },
                        "dehydrated": true,
                        "signatures": {
                            "@example:localhost": {
                                "ed25519:DEHYDRATEDDEVICE": "+OMasB7nzVlMV+zRDxkh4h8h/Q0bY42P1SPv7X2IURIelT5G+d+AYSmg30N4maphxEDBqt/vI8/lIr71exc3Dg",
                                "ed25519:jobZVcxG+PBLwZMsF4XEJSJTVqOgDxd0Ud3J/bw3HYM": "8DzynAgbYgXX1Md5d4Vw91Zstpoi4dpG7levFeVhi4psCAWuBnV76Qu1s2TGjQQ0CLDXEqcxxuX9X4eUK5TGCg",
                            },
                        },
                    }),
                    json!({
                        "algorithms": [
                            "m.olm.v1.curve25519-aes-sha2",
                            "m.megolm.v1.aes-sha2",
                        ],
                        "user_id": "@example:localhost",
                        "device_id": "UNSIGNEDDEVICE",
                        "keys": {
                            "curve25519:UNSIGNEDDEVICE": "mMby6NpprkHxj+ONfO9Z5lBqVUHJBMkrPFSNJhogBkg",
                            "ed25519:UNSIGNEDDEVICE": "Zifq39ZDrlIaSRf0Hh22owEqXCPE+1JSSgs6LDlubwQ",
                        },
                        "signatures": {
                            "@example:localhost": {
                                "ed25519:UNSIGNEDDEVICE": "+L29RoDKoTufPGm/Bae65KHno7Z1H7GYhxSKpB4RQZRS7NrR29AMW1PVhEsIozYuDVEFuMZ0L8H3dlcaHxagBA",
                            },
                        },
                    }),
                ])
            ))
            .mount(&server)
            .await;

        let (request_id, request) = olm_machine.query_keys_for_users([user_id]);
        client.keys_query(&request_id, request.device_keys).await.unwrap();

        assert!(!client.encryption().has_devices_to_verify_against().await.unwrap());
    }
}
