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
#![cfg_attr(target_arch = "wasm32", allow(unused_imports))]

use std::{
    collections::{BTreeMap, HashSet},
    io::{Cursor, Read, Write},
    iter,
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex},
};

use eyeball::{SharedObservable, Subscriber};
use futures_core::Stream;
use futures_util::{
    future::try_join,
    stream::{self, StreamExt},
};
use matrix_sdk_base::crypto::{
    CrossSigningBootstrapRequests, OlmMachine, OutgoingRequest, RoomMessageRequest, ToDeviceRequest,
};
use matrix_sdk_common::executor::spawn;
use ruma::{
    api::client::{
        keys::{
            get_keys, upload_keys, upload_signing_keys::v3::Request as UploadSigningKeysRequest,
        },
        message::send_message_event,
        to_device::send_event_to_device::v3::{
            Request as RumaToDeviceRequest, Response as ToDeviceResponse,
        },
        uiaa::AuthData,
    },
    assign,
    events::room::{
        message::{
            AudioMessageEventContent, FileInfo, FileMessageEventContent, ImageMessageEventContent,
            MessageType, VideoInfo, VideoMessageEventContent,
        },
        ImageInfo, MediaSource, ThumbnailInfo,
    },
    DeviceId, OwnedDeviceId, OwnedUserId, TransactionId, UserId,
};
use tokio::sync::RwLockReadGuard;
use tracing::{debug, error, instrument, trace, warn};

use self::{
    backups::{types::BackupClientState, Backups},
    futures::PrepareEncryptedFile,
    identities::{DeviceUpdates, IdentityUpdates},
    recovery::{Recovery, RecoveryState},
    secret_storage::SecretStorage,
    tasks::{BackupDownloadTask, BackupUploadingTask, ClientTasks},
};
use crate::{
    attachment::{AttachmentConfig, Thumbnail},
    client::ClientInner,
    encryption::{
        identities::{Device, UserDevices},
        verification::{SasVerification, Verification, VerificationRequest},
    },
    error::HttpResult,
    store_locks::CrossProcessStoreLockGuard,
    Client, Error, Result, Room, TransmissionProgress,
};

pub mod backups;
pub mod futures;
pub mod identities;
pub mod recovery;
pub mod secret_storage;
pub(crate) mod tasks;
pub mod verification;

pub use matrix_sdk_base::crypto::{
    olm::{
        SessionCreationError as MegolmSessionCreationError,
        SessionExportError as OlmSessionExportError,
    },
    vodozemac, CrossSigningStatus, CryptoStoreError, DecryptorError, EventError, KeyExportError,
    LocalTrust, MediaEncryptionInfo, MegolmError, OlmError, RoomKeyImportResult, SecretImportError,
    SessionCreationError, SignatureError, VERSION,
};

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

    pub fn initialize_room_key_tasks(&self, client: &Arc<ClientInner>) {
        let weak_client = Arc::downgrade(client);

        let mut tasks = self.tasks.lock().unwrap();
        tasks.upload_room_keys = Some(BackupUploadingTask::new(weak_client.clone()));

        if self.encryption_settings.backup_download_strategy
            == BackupDownloadStrategy::AfterDecryptionFailure
        {
            tasks.download_room_keys = Some(BackupDownloadTask::new(weak_client));
        }
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
    _guard: CrossProcessStoreLockGuard,
    generation: u64,
}

impl CrossProcessLockStoreGuardWithGeneration {
    /// Return the Crypto Store generation associated with this store lock.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

impl Client {
    pub(crate) async fn olm_machine(&self) -> RwLockReadGuard<'_, Option<OlmMachine>> {
        self.base_client().olm_machine().await
    }

    pub(crate) async fn mark_request_as_sent(
        &self,
        request_id: &TransactionId,
        response: impl Into<matrix_sdk_base::crypto::IncomingResponse<'_>>,
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
    #[instrument(skip(self))]
    pub(crate) async fn keys_query(
        &self,
        request_id: &TransactionId,
        device_keys: BTreeMap<OwnedUserId, Vec<OwnedDeviceId>>,
    ) -> Result<get_keys::v3::Response> {
        let request = assign!(get_keys::v3::Request::new(), { device_keys });

        let response = self.send(request, None).await?;
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
    /// let encrypted_file = client.prepare_encrypted_file(&mime::TEXT_PLAIN, &mut reader).await?;
    ///
    /// room.send(CustomEventContent { encrypted_file }).await?;
    /// # anyhow::Ok(()) };
    /// ```
    pub fn prepare_encrypted_file<'a, R: Read + ?Sized + 'a>(
        &'a self,
        content_type: &'a mime::Mime,
        reader: &'a mut R,
    ) -> PrepareEncryptedFile<'a, R> {
        PrepareEncryptedFile::new(self, content_type, reader)
    }

    /// Encrypt and upload the file to be read from `reader` and construct an
    /// attachment message.
    pub(crate) async fn prepare_encrypted_attachment_message(
        &self,
        filename: &str,
        content_type: &mime::Mime,
        data: Vec<u8>,
        config: AttachmentConfig,
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> Result<MessageType> {
        let upload_thumbnail =
            self.upload_encrypted_thumbnail(config.thumbnail, content_type, send_progress.clone());

        let upload_attachment = async {
            let mut cursor = Cursor::new(data);
            self.prepare_encrypted_file(content_type, &mut cursor)
                .with_send_progress_observable(send_progress)
                .await
        };

        let ((thumbnail_source, thumbnail_info), file) =
            try_join(upload_thumbnail, upload_attachment).await?;

        // if config.caption is set, use it as body, and filename as the file name
        // otherwise, body is the filename, and the filename is not set
        // https://github.com/tulir/matrix-spec-proposals/blob/body-as-caption/proposals/2530-body-as-caption.md
        let (body, filename) = match config.caption {
            Some(caption) => (caption, Some(filename.to_owned())),
            None => (filename.to_owned(), None),
        };

        Ok(match content_type.type_() {
            mime::IMAGE => {
                let info = assign!(config.info.map(ImageInfo::from).unwrap_or_default(), {
                    mimetype: Some(content_type.as_ref().to_owned()),
                    thumbnail_source,
                    thumbnail_info
                });
                let content = assign!(ImageMessageEventContent::encrypted(body.to_owned(), file), {
                    info: Some(Box::new(info)),
                    formatted: config.formatted_caption,
                    filename
                });
                MessageType::Image(content)
            }
            mime::AUDIO => {
                let audio_message_event_content =
                    AudioMessageEventContent::encrypted(body.to_owned(), file);
                MessageType::Audio(crate::media::update_audio_message_event(
                    audio_message_event_content,
                    content_type,
                    config.info,
                ))
            }
            mime::VIDEO => {
                let info = assign!(config.info.map(VideoInfo::from).unwrap_or_default(), {
                    mimetype: Some(content_type.as_ref().to_owned()),
                    thumbnail_source,
                    thumbnail_info
                });
                let content = assign!(VideoMessageEventContent::encrypted(body.to_owned(), file), {
                    info: Some(Box::new(info)),
                    formatted: config.formatted_caption,
                    filename
                });
                MessageType::Video(content)
            }
            _ => {
                let info = assign!(config.info.map(FileInfo::from).unwrap_or_default(), {
                    mimetype: Some(content_type.as_ref().to_owned()),
                    thumbnail_source,
                    thumbnail_info
                });
                let content = assign!(FileMessageEventContent::encrypted(body.to_owned(), file), {
                    info: Some(Box::new(info))
                });
                MessageType::File(content)
            }
        })
    }

    async fn upload_encrypted_thumbnail(
        &self,
        thumbnail: Option<Thumbnail>,
        content_type: &mime::Mime,
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> Result<(Option<MediaSource>, Option<Box<ThumbnailInfo>>)> {
        if let Some(thumbnail) = thumbnail {
            let mut cursor = Cursor::new(thumbnail.data);

            let file = self
                .prepare_encrypted_file(content_type, &mut cursor)
                .with_send_progress_observable(send_progress)
                .await?;

            #[rustfmt::skip]
            let thumbnail_info =
                assign!(thumbnail.info.map(ThumbnailInfo::from).unwrap_or_default(), {
                    mimetype: Some(thumbnail.content_type.as_ref().to_owned())
                });

            Ok((Some(MediaSource::Encrypted(Box::new(file))), Some(Box::new(thumbnail_info))))
        } else {
            Ok((None, None))
        }
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
            let response = self.send(request, None).await?;
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

        let response = self.send(request.clone(), None).await?;
        self.mark_request_as_sent(request_id, &response).await?;

        Ok(response)
    }

    pub(crate) async fn room_send_helper(
        &self,
        request: &RoomMessageRequest,
    ) -> Result<send_message_event::v3::Response> {
        let content = request.content.clone();
        let txn_id = &request.txn_id;
        let room_id = &request.room_id;

        self.get_room(room_id)
            .expect("Can't send a message to a room that isn't known to the store")
            .send(content)
            .with_transaction_id(txn_id)
            .await
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

        self.send(request, None).await
    }

    pub(crate) async fn send_verification_request(
        &self,
        request: matrix_sdk_base::crypto::OutgoingVerificationRequest,
    ) -> Result<()> {
        match request {
            matrix_sdk_base::crypto::OutgoingVerificationRequest::ToDevice(t) => {
                self.send_to_device(&t).await?;
            }
            matrix_sdk_base::crypto::OutgoingVerificationRequest::InRoom(r) => {
                self.room_send_helper(&r).await?;
            }
        }

        Ok(())
    }

    /// Get the existing DM room with the given user, if any.
    pub fn get_dm_room(&self, user_id: &UserId) -> Option<Room> {
        let rooms = self.joined_rooms();

        // Find the room we share with the `user_id` and only with `user_id`
        let room = rooms.into_iter().find(|r| {
            let targets = r.direct_targets();
            targets.len() == 1 && targets.contains(user_id)
        });

        trace!(?room, "Found room");
        room
    }

    async fn send_outgoing_request(&self, r: OutgoingRequest) -> Result<()> {
        use matrix_sdk_base::crypto::OutgoingRequests;

        match r.request() {
            OutgoingRequests::KeysQuery(request) => {
                self.keys_query(r.request_id(), request.device_keys.clone()).await?;
            }
            OutgoingRequests::KeysUpload(request) => {
                self.keys_upload(r.request_id(), request).await?;
            }
            OutgoingRequests::ToDeviceRequest(request) => {
                let response = self.send_to_device(request).await?;
                self.mark_request_as_sent(r.request_id(), &response).await?;
            }
            OutgoingRequests::SignatureUpload(request) => {
                let response = self.send(request.clone(), None).await?;
                self.mark_request_as_sent(r.request_id(), &response).await?;
            }
            OutgoingRequests::RoomMessage(request) => {
                let response = self.room_send_helper(request).await?;
                self.mark_request_as_sent(r.request_id(), &response).await?;
            }
            OutgoingRequests::KeysClaim(request) => {
                let response = self.send(request.clone(), None).await?;
                self.mark_request_as_sent(r.request_id(), &response).await?;
            }
        }

        Ok(())
    }

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

    /// Get the status of the private cross signing keys.
    ///
    /// This can be used to check which private cross signing keys we have
    /// stored locally.
    pub async fn cross_signing_status(&self) -> Option<CrossSigningStatus> {
        let olm = self.client.olm_machine().await;
        let machine = olm.as_ref()?;
        Some(machine.cross_signing_status().await)
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
    /// use matrix_sdk::{encryption, Client};
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
        self.client.inner.verification_state.subscribe()
    }

    /// Get a verification object with the given flow id.
    pub async fn get_verification(&self, user_id: &UserId, flow_id: &str) -> Option<Verification> {
        let olm = self.client.olm_machine().await;
        let olm = olm.as_ref()?;
        #[allow(clippy::bind_instead_of_map)]
        olm.get_verification(user_id, flow_id).and_then(|v| match v {
            matrix_sdk_base::crypto::Verification::SasV1(s) => {
                Some(SasVerification { inner: s, client: self.client.clone() }.into())
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

    /// Get a E2EE identity of an user.
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
    ) -> Result<Option<crate::encryption::identities::UserIdentity>, CryptoStoreError> {
        use crate::encryption::identities::UserIdentity;

        let olm = self.client.olm_machine().await;
        let Some(olm) = olm.as_ref() else { return Ok(None) };
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
    pub async fn devices_stream(&self) -> Result<impl Stream<Item = DeviceUpdates>> {
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
    pub async fn user_identities_stream(&self) -> Result<impl Stream<Item = IdentityUpdates>> {
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
    /// request needs to set this to `None` and will always fail with an
    /// `UiaaResponse`. The response will contain information for the
    /// interactive auth and the same request needs to be made but this time
    /// with some `auth_data` provided.
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
        self.client.send(upload_signing_keys_req, None).await?;
        self.client.send(upload_signatures_req, None).await?;

        Ok(())
    }

    /// Query the user's own device keys, if, and only if, we didn't have their
    /// identity in the first place.
    async fn ensure_initial_key_query(&self) -> Result<()> {
        let olm_machine = self.client.olm_machine().await;
        let olm_machine = olm_machine.as_ref().ok_or(crate::Error::NoOlmMachine)?;

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
    /// request needs to set this to `None` and will always fail with an
    /// `UiaaResponse`. The response will contain information for the
    /// interactive auth and the same request needs to be made but this time
    /// with some `auth_data` provided.
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
        let olm_machine = olm_machine.as_ref().ok_or(crate::Error::NoOlmMachine)?;
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
    ///   exported
    /// room keys.
    ///
    /// * `predicate` - A closure that will be called for every known
    /// `InboundGroupSession`, which represents a room key. If the closure
    /// returns `true` the `InboundGroupSessoin` will be included in the export,
    /// if the closure returns `false` it will not be included.
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
    #[cfg(not(target_arch = "wasm32"))]
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
    /// exported room keys.
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
    #[cfg(not(target_arch = "wasm32"))]
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
    pub async fn enable_cross_process_store_lock(&self, lock_value: String) -> Result<(), Error> {
        // If the lock has already been created, don't recreate it from scratch.
        if let Some(prev_lock) = self.client.locks().cross_process_crypto_store_lock.get() {
            let prev_holder = prev_lock.lock_holder();
            if prev_holder == lock_value {
                return Ok(());
            }
            warn!("Recreating cross-process store lock with a different holder value: prev was {prev_holder}, new is {lock_value}");
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
            let guard = lock.try_lock_once().await?;
            if guard.is_some() {
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
                self.client.base_client().regenerate_olm().await?;
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
            let guard = lock.spin_lock(max_backoff).await?;

            let generation = self.on_lock_newly_acquired().await?;

            Ok(Some(CrossProcessLockStoreGuardWithGeneration { _guard: guard, generation }))
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
            let maybe_guard = lock.try_lock_once().await?;

            let Some(guard) = maybe_guard else {
                return Ok(None);
            };

            let generation = self.on_lock_newly_acquired().await?;

            Ok(Some(CrossProcessLockStoreGuardWithGeneration { _guard: guard, generation }))
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
    /// the user from having to re-enter their password (or use other methods),
    /// we can provide the authentication data here. This is necessary for
    /// uploading cross-signing keys. However, please note that there is a
    /// proposal (MSC3967) to remove this requirement, which would allow for
    /// the initial upload of cross-signing keys without authentication,
    /// rendering this parameter obsolete.
    pub(crate) async fn run_initialization_tasks(&self, auth_data: Option<AuthData>) -> Result<()> {
        let mut tasks = self.client.inner.e2ee.tasks.lock().unwrap();

        let this = self.clone();
        tasks.setup_e2ee = Some(spawn(async move {
            if this.settings().auto_enable_cross_signing {
                if let Err(e) = this.bootstrap_cross_signing_if_needed(auth_data).await {
                    error!("Couldn't bootstrap cross signing {e:?}");
                }
            }

            if let Err(e) = this.backups().setup_and_resume().await {
                error!("Couldn't setup and resume backups {e:?}");
            }
            if let Err(e) = this.recovery().setup().await {
                error!("Couldn't setup and resume recovery {e:?}");
            }

            this.update_verification_state().await;
        }));

        Ok(())
    }

    /// Waits for end-to-end encryption initialization tasks to finish, if any
    /// was running in the background.
    pub async fn wait_for_e2ee_initialization_tasks(&self) {
        let task = self.client.inner.e2ee.tasks.lock().unwrap().setup_e2ee.take();

        if let Some(task) = task {
            if let Err(err) = task.await {
                warn!("Error when initializing backups: {err}");
            }
        }
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
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::time::Duration;

    use matrix_sdk_base::SessionMeta;
    use matrix_sdk_test::{
        async_test, test_json, GlobalAccountDataTestEvent, JoinedRoomBuilder, StateTestEvent,
        SyncResponseBuilder, DEFAULT_TEST_ROOM_ID,
    };
    use ruma::{
        device_id, event_id,
        events::{reaction::ReactionEventContent, relation::Annotation},
        user_id,
    };
    use serde_json::json;
    use wiremock::{
        matchers::{header, method, path_regex},
        Mock, MockServer, ResponseTemplate,
    };

    use crate::{
        config::RequestConfig,
        matrix_auth::{MatrixSession, MatrixSessionTokens},
        test_utils::logged_in_client,
        Client,
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
        assert!(room.is_encrypted().await.expect("Getting encryption state"));

        let event_id = event_id!("$1:example.org");
        let reaction = ReactionEventContent::new(Annotation::new(event_id.into(), "🐈".to_owned()));
        room.send(reaction).await.expect("Sending the reaction should not fail");

        room.send_raw("m.reaction", json!({})).await.expect("Sending the reaction should not fail");
    }

    #[async_test]
    async fn test_get_dm_room_returns_the_room_we_have_with_this_user() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;
        // This is the user ID that is inside MemberAdditional.
        // Note the confusing username, so we can share
        // GlobalAccountDataTestEvent::Direct with the invited test.
        let user_id = user_id!("@invited:localhost");

        // When we receive a sync response saying "invited" is invited to a DM
        let response = SyncResponseBuilder::default()
            .add_joined_room(
                JoinedRoomBuilder::default().add_state_event(StateTestEvent::MemberAdditional),
            )
            .add_global_account_data_event(GlobalAccountDataTestEvent::Direct)
            .build_sync_response();
        client.base_client().receive_sync_response(response).await.unwrap();

        // Then get_dm_room finds this room
        let found_room = client.get_dm_room(user_id).expect("DM not found!");
        assert!(found_room.get_member_no_sync(user_id).await.unwrap().is_some());
    }

    #[async_test]
    async fn test_get_dm_room_still_finds_room_where_participant_is_only_invited() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;
        // This is the user ID that is inside MemberInvite
        let user_id = user_id!("@invited:localhost");

        // When we receive a sync response saying "invited" is invited to a DM
        let response = SyncResponseBuilder::default()
            .add_joined_room(
                JoinedRoomBuilder::default().add_state_event(StateTestEvent::MemberInvite),
            )
            .add_global_account_data_event(GlobalAccountDataTestEvent::Direct)
            .build_sync_response();
        client.base_client().receive_sync_response(response).await.unwrap();

        // Then get_dm_room finds this room
        let found_room = client.get_dm_room(user_id).expect("DM not found!");
        assert!(found_room.get_member_no_sync(user_id).await.unwrap().is_some());
    }

    #[async_test]
    async fn test_get_dm_room_still_finds_left_room() {
        // See the discussion in https://github.com/matrix-org/matrix-rust-sdk/issues/2017
        // and the high-level issue at https://github.com/vector-im/element-x-ios/issues/1077

        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;
        // This is the user ID that is inside MemberAdditional.
        // Note the confusing username, so we can share
        // GlobalAccountDataTestEvent::Direct with the invited test.
        let user_id = user_id!("@invited:localhost");

        // When we receive a sync response saying "invited" is invited to a DM
        let response = SyncResponseBuilder::default()
            .add_joined_room(
                JoinedRoomBuilder::default().add_state_event(StateTestEvent::MemberLeave),
            )
            .add_global_account_data_event(GlobalAccountDataTestEvent::Direct)
            .build_sync_response();
        client.base_client().receive_sync_response(response).await.unwrap();

        // Then get_dm_room finds this room
        let found_room = client.get_dm_room(user_id).expect("DM not found!");
        assert!(found_room.get_member_no_sync(user_id).await.unwrap().is_some());
    }

    #[cfg(feature = "sqlite")]
    #[async_test]
    async fn test_generation_counter_invalidates_olm_machine() {
        // Create two clients using the same sqlite database.
        let sqlite_path = std::env::temp_dir().join("generation_counter_sqlite.db");
        let session = MatrixSession {
            meta: SessionMeta {
                user_id: user_id!("@example:localhost").to_owned(),
                device_id: device_id!("DEVICEID").to_owned(),
            },
            tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
        };

        let client1 = Client::builder()
            .homeserver_url("http://localhost:1234")
            .request_config(RequestConfig::new().disable_retry())
            .sqlite_store(&sqlite_path, None)
            .build()
            .await
            .unwrap();
        client1.matrix_auth().restore_session(session.clone()).await.unwrap();

        let client2 = Client::builder()
            .homeserver_url("http://localhost:1234")
            .request_config(RequestConfig::new().disable_retry())
            .sqlite_store(sqlite_path, None)
            .build()
            .await
            .unwrap();
        client2.matrix_auth().restore_session(session).await.unwrap();

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
        let decryption_key = matrix_sdk_base::crypto::store::BackupDecryptionKey::new()
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
        let sqlite_path =
            std::env::temp_dir().join("generation_counter_no_spurious_invalidations.db");
        let session = MatrixSession {
            meta: SessionMeta {
                user_id: user_id!("@example:localhost").to_owned(),
                device_id: device_id!("DEVICEID").to_owned(),
            },
            tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
        };

        let client = Client::builder()
            .homeserver_url("http://localhost:1234")
            .request_config(RequestConfig::new().disable_retry())
            .sqlite_store(&sqlite_path, None)
            .build()
            .await
            .unwrap();
        client.matrix_auth().restore_session(session.clone()).await.unwrap();

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
            client2.matrix_auth().restore_session(session).await.unwrap();

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
}
