// Copyright 2023 The Matrix.org Foundation C.I.C.
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

//! Room key backup support
//!
//! This module implements support for server-side key backups[[1]]. The module
//! allows you to connect to an existing backup, create or delete backups from
//! the homeserver, and download room keys from a backup.
//!
//! [1]: https://spec.matrix.org/unstable/client-server-api/#server-side-key-backups

use std::collections::{BTreeMap, BTreeSet};

use futures_core::Stream;
use futures_util::StreamExt;
use matrix_sdk_base::crypto::{
    backups::MegolmV1BackupKey, store::BackupDecryptionKey, types::RoomKeyBackupInfo,
    KeysBackupRequest, OlmMachine, RoomKeyImportResult,
};
use ruma::{
    api::client::{
        backup::{
            add_backup_keys, create_backup_version, get_backup_keys, get_backup_keys_for_room,
            get_backup_keys_for_session, get_latest_backup_info, RoomKeyBackup,
        },
        error::ErrorKind,
    },
    events::secret::{request::SecretName, send::ToDeviceSecretSendEvent},
    serde::Raw,
    RoomId, TransactionId,
};
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tracing::{error, info, instrument, trace, warn, Span};

pub mod futures;
pub(crate) mod types;

pub use types::{BackupState, UploadState};

use self::futures::WaitForSteadyState;
use crate::{Client, Error};

/// The backups manager for the [`Client`].
#[derive(Debug, Clone)]
pub struct Backups {
    pub(super) client: Client,
}

impl Backups {
    /// Create a new backup version, encrypted with a new backup recovery key.
    ///
    /// The backup recovery key will be persisted locally and shared with
    /// trusted devices as `m.secret.send` to-device messages.
    ///
    /// After the backup has been created, all room keys will be uploaded to the
    /// homeserver.
    ///
    /// *Warning*: This will overwrite any existing backup.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, encryption::backups::BackupState};
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// let backups = client.encryption().backups();
    /// backups.create().await?;
    ///
    /// assert_eq!(backups.state(), BackupState::Enabled);
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn create(&self) -> Result<(), Error> {
        let _guard = self.client.locks().backup_modify_lock.lock().await;

        self.set_state(BackupState::Creating);

        // Create a future so we can catch errors and go back to the `Unknown`
        // state. This is a hack to get around the lack of `try` blocks in Rust.
        let future = async {
            let olm_machine = self.client.olm_machine().await;
            let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;

            // Create a new backup recovery key.
            let decryption_key = BackupDecryptionKey::new().expect(
                "We should be able to generate enough randomness to create a new backup recovery \
                 key",
            );

            // Get the info about the new backup key, this needs to be uploaded to the
            // homeserver[1].
            //
            // We need to sign the `RoomKeyBackupInfo` so other clients which might want
            // to start using the backup without having access to the
            // `BackupDecryptionKey` can do so, as per [spec]:
            //
            // Clients must only store keys in backups after they have ensured that the
            // `auth_data` has not been tampered with. This can be done either by:
            //
            //  * checking that it is signed by the user's master cross-signing key or by a
            //    verified device belonging to the same user, or
            //  * by deriving the public key from a private key that it obtained from a
            //    trusted source. Trusted sources for the private key include the user
            //    entering the key, retrieving the key stored in secret storage, or
            //    obtaining the key via secret sharing from a verified device belonging to
            //    the same user.
            //
            //
            // [1]: https://spec.matrix.org/v1.8/client-server-api/#post_matrixclientv3room_keysversion
            // [spec]: https://spec.matrix.org/v1.8/client-server-api/#server-side-key-backups
            let mut backup_info = decryption_key.to_backup_info();

            if let Err(e) = olm_machine.backup_machine().sign_backup(&mut backup_info).await {
                warn!("Unable to sign the newly created backup version: {e:?}");
            }

            let algorithm = Raw::new(&backup_info)?.cast();
            let request = create_backup_version::v3::Request::new(algorithm);
            let response = self.client.send(request, Default::default()).await?;
            let version = response.version;

            // Reset any state we might have had before the new backup was created.
            // TODO: This should remove the old stored key and version.
            olm_machine.backup_machine().disable_backup().await?;

            let backup_key = decryption_key.megolm_v1_public_key();

            // Save the newly created keys and the version we received from the server.
            olm_machine
                .backup_machine()
                .save_decryption_key(Some(decryption_key), Some(version.to_owned()))
                .await?;

            // Enable the backup and start the upload of room keys.
            self.enable(olm_machine, backup_key, version).await?;

            Ok(())
        };

        let result = future.await;

        if result.is_err() {
            self.set_state(BackupState::Unknown);
        }

        result
    }

    /// Disable and delete the currently active backup.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, encryption::backups::BackupState};
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// let backups = client.encryption().backups();
    /// backups.disable().await?;
    ///
    /// assert_eq!(backups.state(), BackupState::Unknown);
    /// # anyhow::Ok(()) };
    /// ```
    #[instrument(skip_all, fields(version))]
    pub async fn disable(&self) -> Result<(), Error> {
        let _guard = self.client.locks().backup_modify_lock.lock().await;

        self.set_state(BackupState::Disabling);

        // Create a future so we can catch errors and go back to the `Unknown` state.
        let future = async {
            let olm_machine = self.client.olm_machine().await;
            let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;

            let backup_keys = olm_machine.backup_machine().get_backup_keys().await?;

            if let Some(version) = backup_keys.backup_version {
                Span::current().record("version", &version);
                info!("Deleting and disabling backup");

                self.delete_backup_from_server(version).await?;
                info!("Backup successfully deleted");

                olm_machine.backup_machine().disable_backup().await?;
                self.set_state(BackupState::Unknown);

                info!("Backup successfully disabled and deleted");
            } else {
                info!("Backup is not enabled, can't disable it");
            }

            Ok(())
        };

        let result = future.await;

        if result.is_err() {
            self.set_state(BackupState::Unknown);
        }

        result
    }

    /// Returns a future to wait for room keys to be uploaded.
    ///
    /// Awaiting the future will wake up a task to upload room keys which have
    /// not yet been uploaded to the homeserver. It will then wait for the task
    /// to finish uploading.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, encryption::backups::UploadState};
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// use futures_util::StreamExt;
    ///
    /// let backups = client.encryption().backups();
    /// let wait_for_steady_state = backups.wait_for_steady_state();
    ///
    /// let mut progress_stream = wait_for_steady_state.subscribe_to_progress();
    ///
    /// tokio::spawn(async move {
    ///     while let Some(update) = progress_stream.next().await {
    ///         let Ok(update) = update else { break };
    ///
    ///         match update {
    ///             UploadState::Uploading(counts) => {
    ///                 println!(
    ///                     "Uploaded {} out of {} room keys.",
    ///                     counts.backed_up, counts.total
    ///                 );
    ///             }
    ///             UploadState::Error => break,
    ///             UploadState::Done => break,
    ///             _ => (),
    ///         }
    ///     }
    /// });
    ///
    /// wait_for_steady_state.await?;
    ///
    /// # anyhow::Ok(()) };
    /// ```
    pub fn wait_for_steady_state(&self) -> WaitForSteadyState<'_> {
        WaitForSteadyState {
            backups: self,
            progress: self.client.inner.backup_state.upload_progress.clone(),
            timeout: None,
        }
    }

    /// Get a stream of updates to the [`BackupState`].
    ///
    /// This method will send out the current state as the first update.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, encryption::backups::BackupState};
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// use futures_util::StreamExt;
    ///
    /// let backups = client.encryption().backups();
    ///
    /// let mut state_stream = backups.state_stream();
    ///
    /// while let Some(update) = state_stream.next().await {
    ///     let Ok(update) = update else { break };
    ///
    ///     match update {
    ///         BackupState::Enabled => {
    ///             println!("Backups have been enabled");
    ///         }
    ///         _ => (),
    ///     }
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub fn state_stream(
        &self,
    ) -> impl Stream<Item = Result<BackupState, BroadcastStreamRecvError>> {
        self.client.inner.backup_state.global_state.subscribe()
    }

    /// Get the current [`BackupState`] for this [`Client`].
    pub fn state(&self) -> BackupState {
        self.client.inner.backup_state.global_state.get()
    }

    /// Are backups enabled for the current [`Client`]?
    ///
    /// This method will check if we locally have an active backup key and
    /// backup version and are ready to upload room keys to a backup.
    pub async fn are_enabled(&self) -> bool {
        let olm_machine = self.client.olm_machine().await;

        if let Some(machine) = olm_machine.as_ref() {
            machine.backup_machine().enabled().await
        } else {
            false
        }
    }

    /// Does a backup exist on the server?
    ///
    /// This method will request info about the current backup from the
    /// homeserver and if a backup exits return `true`, otherwise `false`.
    pub async fn exists_on_server(&self) -> Result<bool, Error> {
        Ok(self.get_current_version().await?.is_some())
    }

    /// Subscribe to a stream that notifies when a room key for the specified
    /// room is downloaded from the key backup.
    pub fn room_keys_for_room_stream(
        &self,
        room_id: &RoomId,
    ) -> impl Stream<Item = Result<BTreeMap<String, BTreeSet<String>>, BroadcastStreamRecvError>>
    {
        let room_id = room_id.to_owned();

        // TODO: This is a bit crap to say the least. The type is
        // non-descriptive and doesn't even contain all the important data. It
        // should be a stream of `RoomKeyInfo` like the OlmMachine has... But on
        // the other hand we should just be able to use the corresponding
        // OlmMachine stream and remove this. Currently we can't do this because
        // the OlmMachine gets destroyed and recreated all the time to be able
        // to support the notifications-related multiprocessing on iOS.
        self.room_keys_stream().filter_map(move |import_result| {
            let room_id = room_id.to_owned();

            async move {
                match import_result {
                    Ok(mut import_result) => import_result.keys.remove(&room_id).map(Ok),
                    Err(e) => Some(Err(e)),
                }
            }
        })
    }

    /// Download all room keys for a certain room from the server-side key
    /// backup.
    pub async fn download_room_keys_for_room(&self, room_id: &RoomId) -> Result<(), Error> {
        let olm_machine = self.client.olm_machine().await;
        let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;

        let backup_keys = olm_machine.store().load_backup_keys().await?;

        if let Some(decryption_key) = backup_keys.decryption_key {
            if let Some(version) = backup_keys.backup_version {
                let request =
                    get_backup_keys_for_room::v3::Request::new(version, room_id.to_owned());
                let response = self.client.send(request, Default::default()).await?;

                // Transform response to standard format (map of room ID -> room key).
                let response = get_backup_keys::v3::Response::new(BTreeMap::from([(
                    room_id.to_owned(),
                    RoomKeyBackup::new(response.sessions),
                )]));

                self.handle_downloaded_room_keys(response, decryption_key, olm_machine).await?;
            }
        }

        Ok(())
    }

    /// Download a single room key from the server-side key backup.
    pub async fn download_room_key(&self, room_id: &RoomId, session_id: &str) -> Result<(), Error> {
        let olm_machine = self.client.olm_machine().await;
        let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;

        let backup_keys = olm_machine.store().load_backup_keys().await?;

        if let Some(decryption_key) = backup_keys.decryption_key {
            if let Some(version) = backup_keys.backup_version {
                let request = get_backup_keys_for_session::v3::Request::new(
                    version,
                    room_id.to_owned(),
                    session_id.to_owned(),
                );
                let response = self.client.send(request, Default::default()).await?;

                // Transform response to standard format (map of room ID -> room key).
                let response = get_backup_keys::v3::Response::new(BTreeMap::from([(
                    room_id.to_owned(),
                    RoomKeyBackup::new(BTreeMap::from([(
                        session_id.to_owned(),
                        response.key_data,
                    )])),
                )]));

                self.handle_downloaded_room_keys(response, decryption_key, olm_machine).await?;
            }
        }

        Ok(())
    }

    /// Set the state of the backup.
    fn set_state(&self, state: BackupState) {
        self.client.inner.backup_state.global_state.set(state);
    }

    /// Set the backup state to the `Enabled` variant and insert the backup key
    /// and version into the [`OlmMachine`].
    async fn enable(
        &self,
        olm_machine: &OlmMachine,
        backup_key: MegolmV1BackupKey,
        version: String,
    ) -> Result<(), Error> {
        backup_key.set_version(version);
        olm_machine.backup_machine().enable_backup_v1(backup_key).await?;

        self.set_state(BackupState::Enabled);

        Ok(())
    }

    /// Decrypt and forward a response containing backed up room keys to the
    /// [`OlmMachine`].
    async fn handle_downloaded_room_keys(
        &self,
        backed_up_keys: get_backup_keys::v3::Response,
        backup_decryption_key: BackupDecryptionKey,
        olm_machine: &OlmMachine,
    ) -> Result<(), Error> {
        let mut decrypted_room_keys: BTreeMap<_, BTreeMap<_, _>> = BTreeMap::new();

        for (room_id, room_keys) in backed_up_keys.rooms {
            for (session_id, room_key) in room_keys.sessions {
                // TODO: Log that we're skipping some keys here.
                let Ok(room_key) = room_key.deserialize() else {
                    warn!("Couldn't deserialize a room key we downloaded from backups, session ID: {session_id}");
                    continue;
                };

                let Ok(room_key) =
                    backup_decryption_key.decrypt_session_data(room_key.session_data)
                else {
                    continue;
                };

                decrypted_room_keys
                    .entry(room_id.to_owned())
                    .or_default()
                    .insert(session_id, room_key);
            }
        }

        let result = olm_machine
            .backup_machine()
            .import_backed_up_room_keys(decrypted_room_keys, |_, _| {})
            .await?;

        // Since we can't use the usual room keys stream from the `OlmMachine`
        // we're going to send things out in our own custom broadcaster.
        let _ = self.client.inner.backup_state.room_keys_broadcaster.send(result);

        Ok(())
    }

    /// Download all room keys from the backup on the homeserver.
    async fn download_all_room_keys(
        &self,
        decryption_key: BackupDecryptionKey,
        version: String,
    ) -> Result<(), Error> {
        let request = get_backup_keys::v3::Request::new(version);
        let response = self.client.send(request, Default::default()).await?;

        let olm_machine = self.client.olm_machine().await;
        let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;

        self.handle_downloaded_room_keys(response, decryption_key, olm_machine).await?;

        Ok(())
    }

    fn room_keys_stream(
        &self,
    ) -> impl Stream<Item = Result<RoomKeyImportResult, BroadcastStreamRecvError>> {
        BroadcastStream::new(self.client.inner.backup_state.room_keys_broadcaster.subscribe())
    }

    /// Get info about the currently active backup from the server.
    async fn get_current_version(
        &self,
    ) -> Result<Option<get_latest_backup_info::v3::Response>, Error> {
        let request = get_latest_backup_info::v3::Request::new();

        match self.client.send(request, None).await {
            Ok(r) => Ok(Some(r)),
            Err(e) => {
                if let Some(kind) = e.client_api_error_kind() {
                    if kind == &ErrorKind::NotFound {
                        Ok(None)
                    } else {
                        Err(e.into())
                    }
                } else {
                    Err(e.into())
                }
            }
        }
    }

    async fn delete_backup_from_server(&self, version: String) -> Result<(), Error> {
        let request = ruma::api::client::backup::delete_backup_version::v3::Request::new(version);

        match self.client.send(request, Default::default()).await {
            Ok(_) => Ok(()),
            Err(e) => {
                if let Some(kind) = e.client_api_error_kind() {
                    if kind == &ErrorKind::NotFound {
                        Ok(())
                    } else {
                        Err(e.into())
                    }
                } else {
                    Err(e.into())
                }
            }
        }
    }

    #[instrument(skip(self, olm_machine, request))]
    async fn send_backup_request(
        &self,
        olm_machine: &OlmMachine,
        request_id: &TransactionId,
        request: KeysBackupRequest,
    ) -> Result<(), Error> {
        trace!("Uploading some room keys");

        let add_backup_keys = add_backup_keys::v3::Request::new(request.version, request.rooms);

        match self.client.send(add_backup_keys, Default::default()).await {
            Ok(response) => {
                olm_machine.mark_request_as_sent(request_id, &response).await?;

                let new_counts = olm_machine.backup_machine().room_key_counts().await?;

                self.client
                    .inner
                    .backup_state
                    .upload_progress
                    .set(UploadState::Uploading(new_counts));

                #[cfg(not(target_arch = "wasm32"))]
                {
                    let delay =
                        self.client.inner.backup_state.upload_delay.read().unwrap().to_owned();
                    tokio::time::sleep(delay).await;
                }

                Ok(())
            }
            Err(error) => {
                if let Some(kind) = error.client_api_error_kind() {
                    match kind {
                        ErrorKind::NotFound => {
                            warn!("No backup found on the server, the backup likely got deleted, disabling backups.");

                            self.handle_deleted_backup_version(olm_machine).await?;
                        }
                        ErrorKind::WrongRoomKeysVersion { current_version } => {
                            warn!(
                                new_version = current_version,
                                "A new backup version was found on the server, disabling backups."
                            );

                            // TODO: If we're verified and there are other devices besides us,
                            // request the new backup key over `m.secret.send`.

                            self.handle_deleted_backup_version(olm_machine).await?;
                        }

                        _ => (),
                    }
                }

                Err(error.into())
            }
        }
    }

    /// Poll the [`OlmMachine`] for room keys which need to be backed up and
    /// send out the request to the homeserver.
    ///
    /// This should only be called by the [`BackupUploadingTask`].
    ///
    /// [`BackupUploadingTask`]: crate::client::tasks::BackupUploadingTask
    pub(crate) async fn backup_room_keys(&self) -> Result<(), Error> {
        let _guard = self.client.locks().backup_upload_lock.lock().await;

        let olm_machine = self.client.olm_machine().await;
        let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;

        while let Some((request_id, request)) = olm_machine.backup_machine().backup().await? {
            self.send_backup_request(olm_machine, &request_id, request).await?;
        }

        self.client.inner.backup_state.upload_progress.set(UploadState::Done);

        Ok(())
    }

    /// Set up a `m.secret.send` listener and re-enable backups if we have a
    /// backup recovery key stored.
    pub(crate) async fn setup_and_resume(&self) -> Result<(), Error> {
        info!("Setting up secret listeners and trying to resume backups");

        self.client.add_event_handler(Self::secret_send_event_handler);
        self.maybe_resume_backups().await?;

        Ok(())
    }

    /// Try to enable backups with the given backup recovery key.
    ///
    /// This should be called if we receive a backup recovery, either:
    ///
    /// * As an `m.secret.send` to-device message from a trusted device.
    /// * From 4S (i.e. from the `m.megolm_backup.v1` event global account
    ///   data).
    ///
    /// In both cases the method will compare the currently active backup
    /// version to the backup recovery key's version and, if there is a match,
    /// activate backups on this device and start uploading room keys to the
    /// backup.
    ///
    /// Returns true if backups were just enabled or were already enabled,
    /// otherwise false.
    #[instrument(skip_all)]
    pub(crate) async fn maybe_enable_backups(
        &self,
        maybe_recovery_key: &str,
    ) -> Result<bool, Error> {
        let _guard = self.client.locks().backup_modify_lock.lock().await;

        // Create a future here which allows us to catch any failure that might happen
        // so we can later on fall back to the correct `BackupState`.
        let future = async {
            self.set_state(BackupState::Enabling);

            let olm_machine = self.client.olm_machine().await;
            let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;
            let backup_machine = olm_machine.backup_machine();

            let decryption_key =
                BackupDecryptionKey::from_base64(maybe_recovery_key).map_err(|e| {
                    <serde_json::Error as serde::de::Error>::custom(format!(
                        "Couldn't deserialize the backup recovery key: {e:?}"
                    ))
                })?;

            // Let's try to see if there's a backup on the homeserver.
            let current_version = self.get_current_version().await?;

            let Some(current_version) = current_version else {
                warn!("Tried to enable backups, but no backup version was found on the server.");
                return Ok(false);
            };

            Span::current().record("backup_version", &current_version.version);

            let backup_info: RoomKeyBackupInfo = current_version.algorithm.deserialize_as()?;
            let stored_keys = backup_machine.get_backup_keys().await?;

            if stored_keys.backup_version.as_ref() == Some(&current_version.version)
                && self.are_enabled().await
            {
                // If we already have a backup enabled which is using the currently active
                // backup version, do nothing but tell the caller using the return value that
                // backups are enabled.
                Ok(true)
            } else if decryption_key.backup_key_matches(&backup_info) {
                info!(
                    "We have found the correct backup recovery key. Storing the backup recovery \
                     key and enabling backups."
                );

                // We're enabling a new backup, reset the `backed_up` flags on the room keys and
                // remove any key/version we might have.
                backup_machine.disable_backup().await?;

                let backup_key = decryption_key.megolm_v1_public_key();
                backup_key.set_version(current_version.version.to_owned());

                // Persist the new keys and enable the backup.
                backup_machine
                    .save_decryption_key(
                        Some(decryption_key.to_owned()),
                        Some(current_version.version.to_owned()),
                    )
                    .await?;
                backup_machine.enable_backup_v1(backup_key).await?;

                // If the user has set up the client to download any room keys, do so now. This
                // is not really useful in a real scenario since the API to
                // download room keys is not paginated.
                //
                // You need to download all room keys at once, parse a potentially huge JSON
                // response and decrypt all the room keys found in the backup.
                //
                // This doesn't work for any sizeable account.
                if self.client.inner.encryption_settings.auto_download_from_backup {
                    self.set_state(BackupState::Downloading);

                    if let Err(e) =
                        self.download_all_room_keys(decryption_key, current_version.version).await
                    {
                        warn!("Couldn't automatically download all room keys from backup: {e:?}");
                    }
                }

                // Trigger the upload of any room keys we might need to upload.
                self.maybe_trigger_backup();

                Ok(true)
            } else {
                let derived_key = decryption_key.megolm_v1_public_key();
                let downloaded_key = current_version.algorithm;

                warn!(
                    ?derived_key,
                    ?downloaded_key,
                    "Found an active backup but the recovery key we received isn't the one used for \
                     this backup version"
                );

                Ok(false)
            }
        };

        match future.await {
            Ok(enabled) => {
                if enabled {
                    self.set_state(BackupState::Enabled);
                } else {
                    self.set_state(BackupState::Unknown);
                }

                Ok(enabled)
            }
            Err(e) => {
                self.set_state(BackupState::Unknown);

                Err(e)
            }
        }
    }

    /// Try to resume backups from a backup recovery key we have found in the
    /// crypto store.
    ///
    /// Returns true if backups have been resumed, false otherwise.
    async fn resume_backup_from_stored_backup_key(
        &self,
        olm_machine: &OlmMachine,
    ) -> Result<bool, Error> {
        let backup_keys = olm_machine.store().load_backup_keys().await?;

        if let Some(decryption_key) = backup_keys.decryption_key {
            if let Some(version) = backup_keys.backup_version {
                let backup_key = decryption_key.megolm_v1_public_key();

                self.enable(olm_machine, backup_key, version).await?;

                Ok(true)
            } else {
                Ok(false)
            }
        } else {
            Ok(false)
        }
    }

    /// Try to resume backups by iterating through the `m.secret.send` to-device
    /// messages the [`OlmMachine`] has received and stored in the secret inbox.
    async fn maybe_resume_from_secret_inbox(&self, olm_machine: &OlmMachine) -> Result<(), Error> {
        let secrets = olm_machine.store().get_secrets_from_inbox(&SecretName::RecoveryKey).await?;

        for secret in secrets {
            if self.maybe_enable_backups(&secret.event.content.secret).await? {
                break;
            }
        }

        olm_machine.store().delete_secrets_from_inbox(&SecretName::RecoveryKey).await?;

        Ok(())
    }

    /// Check and re-enable a backup if we have a backup recovery key locally.
    async fn maybe_resume_backups(&self) -> Result<(), Error> {
        let olm_machine = self.client.olm_machine().await;
        let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;

        // Let us first check if we have a stored backup recovery key and a backup
        // version.
        if !self.resume_backup_from_stored_backup_key(olm_machine).await? {
            // We didn't manage to enable backups from a stored backup recovery key, let us
            // check our secret inbox. Perhaps we can find a valid key there.
            self.maybe_resume_from_secret_inbox(olm_machine).await?;
        }

        Ok(())
    }

    /// Listen for `m.secret.send` to-device messages and check the secret inbox
    /// if we do receive one.
    pub(crate) async fn secret_send_event_handler(_: ToDeviceSecretSendEvent, client: Client) {
        let olm_machine = client.olm_machine().await;

        // TODO: Because of our crude multi-process support, which reloads the whole
        // [`OlmMachine`] the `secrets_stream` might stop giving you updates. Once
        // that's fixed, stop listening to individual secret send events and
        // listen to the secrets stream.
        if let Some(olm_machine) = olm_machine.as_ref() {
            if let Err(e) =
                client.encryption().backups().maybe_resume_from_secret_inbox(olm_machine).await
            {
                error!("Could not handle `m.secret.send` event: {e:?}");
            }
        } else {
            error!("Tried to handle a `m.secret.send` event but no OlmMachine was initialized");
        }
    }

    /// Send a notification to the task which is responsible for uploading room
    /// keys to the backup that it might have new room keys to back up.
    pub(crate) fn maybe_trigger_backup(&self) {
        let tasks = self.client.inner.tasks.lock().unwrap();

        if let Some(tasks) = tasks.upload_room_keys.as_ref() {
            tasks.trigger_upload();
        }
    }

    /// Disable our backups locally if we notice that the backup has been
    /// removed on the homeserver.
    async fn handle_deleted_backup_version(&self, olm_machine: &OlmMachine) -> Result<(), Error> {
        olm_machine.backup_machine().disable_backup().await?;
        self.set_state(BackupState::Unknown);

        Ok(())
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod test {
    use std::time::Duration;

    use matrix_sdk_base::crypto::olm::ExportedRoomKey;
    use matrix_sdk_test::async_test;
    use serde_json::json;
    use wiremock::{
        matchers::{header, method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;
    use crate::test_utils::logged_in_client;

    fn room_key() -> ExportedRoomKey {
        let json = json!({
            "algorithm": "m.megolm.v1.aes-sha2",
            "room_id": "!DovneieKSTkdHKpIXy:morpheus.localhost",
            "sender_key": "DeHIg4gwhClxzFYcmNntPNF9YtsdZbmMy8+3kzCMXHA",
            "session_id": "gM8i47Xhu0q52xLfgUXzanCMpLinoyVyH7R58cBuVBU",
            "session_key": "AQAAAABvWMNZjKFtebYIePKieQguozuoLgzeY6wKcyJjLJcJtQgy1dPqTBD12U+XrYLrRHn\
                            lKmxoozlhFqJl456+9hlHCL+yq+6ScFuBHtJepnY1l2bdLb4T0JMDkNsNErkiLiLnD6yp3J\
                            DSjIhkdHxmup/huygrmroq6/L5TaThEoqvW4DPIuO14btKudsS34FF82pwjKS4p6Mlch+0e\
                            fHAblQV",
            "sender_claimed_keys":{},
            "forwarding_curve25519_key_chain":[]
        });

        serde_json::from_value(json)
            .expect("We should be able to deserialize our exported room key")
    }

    async fn backup_disabling_test_body(
        client: &Client,
        server: &MockServer,
        put_response: ResponseTemplate,
    ) {
        let _post_scope = Mock::given(method("POST"))
            .and(path("_matrix/client/unstable/room_keys/version"))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
              "version": "1"
            })))
            .expect(1)
            .named("POST for the backup creation")
            .mount_as_scoped(server)
            .await;

        let _put_scope = Mock::given(method("PUT"))
            .and(path("_matrix/client/unstable/room_keys/keys"))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(put_response)
            .expect(1)
            .named("POST for the backup creation")
            .mount_as_scoped(server)
            .await;

        client
            .encryption()
            .backups()
            .create()
            .await
            .expect("We should be able to create a new backup");

        assert_eq!(client.encryption().backups().state(), BackupState::Enabled);

        client
            .encryption()
            .backups()
            .backup_room_keys()
            .await
            .expect_err("Backups should be disabled");

        assert_eq!(client.encryption().backups().state(), BackupState::Unknown);
    }

    #[async_test]
    async fn backup_disabling_after_remote_deletion() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        {
            let machine = client.olm_machine().await;
            machine
                .as_ref()
                .unwrap()
                .store()
                .import_exported_room_keys(vec![room_key()], |_, _| {})
                .await
                .expect("We should be able to import a room key");
        }

        backup_disabling_test_body(
            &client,
            &server,
            ResponseTemplate::new(404).set_body_json(json!({
                "errcode": "M_NOT_FOUND",
                "error": "Unknown backup version"
            })),
        )
        .await;

        backup_disabling_test_body(
            &client,
            &server,
            ResponseTemplate::new(403).set_body_json(json!({
                "current_version": "42",
                "errcode": "M_WRONG_ROOM_KEYS_VERSION",
                "error": "Wrong backup version."
            })),
        )
        .await;

        server.verify().await;
    }

    #[async_test]
    async fn exists_on_server() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        {
            let _scope = Mock::given(method("GET"))
                .and(path("_matrix/client/r0/room_keys/version"))
                .and(header("authorization", "Bearer 1234"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "algorithm": "m.megolm_backup.v1.curve25519-aes-sha2",
                    "auth_data": {
                        "public_key": "abcdefg",
                        "signatures": {},
                    },
                    "count": 42,
                    "etag": "anopaquestring",
                    "version": "1",
                })))
                .expect(1)
                .mount_as_scoped(&server)
                .await;

            let exists = client
                .encryption()
                .backups()
                .exists_on_server()
                .await
                .expect("We should be able to check if backups exist on the server");

            assert!(exists, "We should deduce that a backup exist on the server");
        }

        {
            let _scope = Mock::given(method("GET"))
                .and(path("_matrix/client/r0/room_keys/version"))
                .and(header("authorization", "Bearer 1234"))
                .respond_with(ResponseTemplate::new(404).set_body_json(json!({
                    "errcode": "M_NOT_FOUND",
                    "error": "No current backup version"
                })))
                .expect(1)
                .mount_as_scoped(&server)
                .await;

            let exists = client
                .encryption()
                .backups()
                .exists_on_server()
                .await
                .expect("We should be able to check if backups exist on the server");

            assert!(!exists, "We should deduce that no backup exist on the server");
        }

        {
            let _scope = Mock::given(method("GET"))
                .and(path("_matrix/client/r0/room_keys/version"))
                .and(header("authorization", "Bearer 1234"))
                .respond_with(ResponseTemplate::new(429).set_body_json(json!({
                    "errcode": "M_LIMIT_EXCEEDED",
                    "error": "Too many requests",
                    "retry_after_ms": 2000
                })))
                .expect(1)
                .mount_as_scoped(&server)
                .await;

            client.encryption().backups().exists_on_server().await.expect_err(
                "If the /version endpoint returns a non 404 error we should throw an error",
            );
        }

        {
            let _scope = Mock::given(method("GET"))
                .and(path("_matrix/client/r0/room_keys/version"))
                .and(header("authorization", "Bearer 1234"))
                .respond_with(ResponseTemplate::new(404))
                .expect(1)
                .mount_as_scoped(&server)
                .await;

            client.encryption().backups().exists_on_server().await.expect_err(
                "If the /version endpoint returns a non-Matrix 404 error we should throw an error",
            );
        }

        server.verify().await;
    }

    #[async_test]
    async fn waiting_for_steady_state_resets_the_delay() {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        Mock::given(method("POST"))
            .and(path("_matrix/client/unstable/room_keys/version"))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
              "version": "1"
            })))
            .expect(1)
            .named("POST for the backup creation")
            .mount(&server)
            .await;

        client
            .encryption()
            .backups()
            .create()
            .await
            .expect("We should be able to create a new backup");

        let backups = client.encryption().backups();

        let old_duration = { client.inner.backup_state.upload_delay.read().unwrap().to_owned() };

        let wait_for_steady_state =
            backups.wait_for_steady_state().with_delay(Duration::from_nanos(100));

        let mut progress_stream = wait_for_steady_state.subscribe_to_progress();

        let task = matrix_sdk_common::executor::spawn({
            let client = client.to_owned();
            async move {
                while let Some(state) = progress_stream.next().await {
                    let Ok(state) = state else {
                        panic!("Error while waiting for the upload state")
                    };

                    match state {
                        UploadState::Idle => (),
                        UploadState::Done => {
                            let current_delay = {
                                client.inner.backup_state.upload_delay.read().unwrap().to_owned()
                            };

                            assert_ne!(current_delay, old_duration);
                            break;
                        }
                        _ => panic!("We should not have entered any other state"),
                    }
                }
            }
        });

        wait_for_steady_state.await.expect("We should be able to wait for the steady state");
        task.await.unwrap();

        let current_duration =
            { client.inner.backup_state.upload_delay.read().unwrap().to_owned() };

        assert_eq!(old_duration, current_duration);

        server.verify().await;
    }
}
