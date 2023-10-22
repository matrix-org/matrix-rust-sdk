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

#![allow(missing_docs)]

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, RwLock},
    time::Duration,
};

use futures_core::Stream;
use futures_util::StreamExt;
use matrix_sdk_base::crypto::{
    backups::MegolmV1BackupKey,
    store::{BackupDecryptionKey, RoomKeyCounts},
    types::RoomKeyBackupInfo,
    GossippedSecret, KeysBackupRequest, OlmMachine, RoomKeyImportResult,
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
use tokio::sync::broadcast;
use tokio_stream::wrappers::{errors::BroadcastStreamRecvError, BroadcastStream};
use tracing::{info, instrument, trace, warn, Span};

mod futures;

pub use self::futures::{SteadyStateError, WaitForSteadyState};
use crate::{Client, Error};

#[derive(Clone, Debug)]
pub struct ChannelObservable<T: Clone + Send> {
    value: Arc<RwLock<T>>,
    channel: broadcast::Sender<T>,
}

impl<T: Default + Clone + Send + 'static> Default for ChannelObservable<T> {
    fn default() -> Self {
        let value = Default::default();
        Self::new(value)
    }
}

impl<T: 'static + Send + Clone> ChannelObservable<T> {
    pub fn new(value: T) -> Self {
        let channel = broadcast::Sender::new(100);
        Self { value: RwLock::new(value).into(), channel }
    }

    pub fn subscribe(&self) -> impl Stream<Item = Result<T, BroadcastStreamRecvError>> {
        let current_value = self.value.read().unwrap().to_owned();
        let initial_stream = tokio_stream::once(Ok(current_value));
        let broadcast_stream = BroadcastStream::new(self.channel.subscribe());

        let combined = initial_stream.chain(broadcast_stream);

        combined
    }

    pub fn set(&self, new_value: T) {
        *self.value.write().unwrap() = new_value.to_owned();
        // We're ignoring the error case where no receivers exist.
        let _ = self.channel.send(new_value);
    }

    pub fn get(&self) -> T {
        self.value.read().unwrap().to_owned()
    }
}

#[derive(Debug, Clone)]
pub struct Backups {
    pub(super) client: Client,
}

#[derive(Clone, Debug)]
pub enum UploadState {
    Idle,
    CheckingIfUploadNeeded(RoomKeyCounts),
    Uploading(RoomKeyCounts),
    Error,
    Done,
}

pub(crate) struct BackupClientState {
    upload_delay: Arc<RwLock<Duration>>,
    pub(crate) upload_progress: ChannelObservable<UploadState>,
    global_state: ChannelObservable<BackupState>,
    room_keys_broadcaster: broadcast::Sender<RoomKeyImportResult>,
}

impl Default for BackupClientState {
    fn default() -> Self {
        Self {
            upload_delay: RwLock::new(Duration::from_millis(100)).into(),
            upload_progress: ChannelObservable::new(UploadState::Idle),
            global_state: Default::default(),
            room_keys_broadcaster: broadcast::Sender::new(100),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackupState {
    Unknown,
    Creating,
    Enabling,
    Resuming,
    Enabled,
    Downloading,
    Disabling,
    Disabled,
}

impl Default for BackupState {
    fn default() -> Self {
        Self::Unknown
    }
}

impl Backups {
    fn set_state(&self, state: BackupState) {
        self.client.inner.backups_state.global_state.set(state);
    }

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

    pub fn wait_for_steady_state(&self) -> WaitForSteadyState<'_> {
        WaitForSteadyState {
            backups: self,
            progress: self.client.inner.backups_state.upload_progress.clone(),
            timeout: None,
        }
    }

    pub async fn create(&self) -> Result<(), Error> {
        let _guard = self.client.locks().backup_modify_lock.lock().await;

        self.set_state(BackupState::Creating);

        let future = async {
            let decryption_key = BackupDecryptionKey::new().expect(
                "We should be able to generate enough randomness to \
                 create a new backup recovery key",
            );

            let olm_machine = self.client.olm_machine().await;
            let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;

            let backup_key = decryption_key.megolm_v1_public_key();

            let mut backup_info = decryption_key.as_room_key_backup_info();
            olm_machine.backup_machine().sign_backup(&mut backup_info).await?;

            let algorithm = Raw::new(&backup_info)?.cast();
            let request = create_backup_version::v3::Request::new(algorithm);
            let response = self.client.send(request, Default::default()).await?;
            let version = response.version;

            // TODO: This should remove the old stored key and version.
            olm_machine.backup_machine().disable_backup().await?;

            olm_machine
                .backup_machine()
                .save_decryption_key(Some(decryption_key), Some(version.to_owned()))
                .await?;

            self.enable(olm_machine, backup_key, version).await?;

            Ok(())
        };

        let result = future.await;

        if result.is_err() {
            self.set_state(BackupState::Unknown)
        }

        result
    }

    #[instrument(skip_all, fields(version))]
    pub async fn disable(&self) -> Result<(), Error> {
        let _guard = self.client.locks().backup_modify_lock.lock().await;

        self.set_state(BackupState::Disabling);

        let future = async {
            let olm_machine = self.client.olm_machine().await;
            let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;

            // TODO: We don't delete from the store so this runs multiple times.
            let backup_keys = olm_machine.backup_machine().get_backup_keys().await?;

            if let Some(version) = backup_keys.backup_version {
                Span::current().record("version", &version);

                info!("Disabling and deleting backup");

                olm_machine.backup_machine().disable_backup().await?;

                info!("Backup successfully disabled");

                let request =
                    ruma::api::client::backup::delete_backup_version::v3::Request::new(version);

                self.client.send(request, Default::default()).await?;
                self.set_state(BackupState::Disabled);

                info!("Backup successfully disabled and deleted");
            } else {
                info!("Backup is not enabled, can't disable it");
            }

            Ok(())
        };

        let result = future.await;

        // TODO: Is this the right state for this, we could had a storage error or a
        // network error for the delete call.
        if result.is_err() {
            self.set_state(BackupState::Unknown);
        }

        result
    }

    pub(crate) async fn setup_and_resume(&self) -> Result<(), Error> {
        info!("Setting up secret listeners and trying to resume backups");

        // TODO: We have a nice [`OlmMachine::store()::secrets_stream()`] which we could
        // use instead of a event handler.
        self.client.add_event_handler(Self::secret_send_event_handler);
        self.maybe_resume_backups().await?;

        Ok(())
    }

    #[instrument(skip_all)]
    pub(crate) async fn maybe_enable_backups(
        &self,
        maybe_recovery_key: &str,
    ) -> Result<bool, Error> {
        let _guard = self.client.locks().backup_modify_lock.lock().await;

        let future = async {
            self.set_state(BackupState::Enabling);

            let olm_machine = self.client.olm_machine().await;
            let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;

            let decryption_key =
                BackupDecryptionKey::from_base64(maybe_recovery_key).map_err(|e| {
                    <serde_json::Error as serde::de::Error>::custom(format!(
                        "Couldn't deserialize the backup recovery key: {e:?}"
                    ))
                })?;

            let current_version = self.get_current_version().await?;

            let Some(current_version) = current_version else {
                warn!("Tried to enable backups, but no backup version was found on the server.");
                return Ok(false);
            };

            Span::current().record("backup_version", &current_version.version);

            let backup_info: RoomKeyBackupInfo = current_version.algorithm.deserialize_as()?;
            let stored_keys = olm_machine.backup_machine().get_backup_keys().await?;

            if stored_keys.backup_version.as_ref() == Some(&current_version.version)
                && self.are_enabled().await
            {
                Ok(true)
            } else if decryption_key.backup_key_matches(&backup_info) {
                info!(
                "We have found the correct backup recovery key, storing the backup recovery key \
                 and enabling backups"
            );

                let backup_key = decryption_key.megolm_v1_public_key();
                backup_key.set_version(current_version.version.to_owned());

                olm_machine
                    .backup_machine()
                    .save_decryption_key(
                        Some(decryption_key.to_owned()),
                        Some(current_version.version.to_owned()),
                    )
                    .await?;
                olm_machine.backup_machine().enable_backup_v1(backup_key).await?;

                if self.client.inner.encryption_settings.auto_download_from_backup {
                    self.set_state(BackupState::Downloading);

                    if let Err(e) =
                        self.download_all_room_keys(decryption_key, current_version.version).await
                    {
                        warn!("Couldn't automatically download all room keys from backup: {e:?}");
                    }
                }

                self.maybe_trigger_backup();

                Ok(true)
            } else {
                let derived_key = decryption_key.megolm_v1_public_key();
                let downloaded_key = current_version.algorithm;

                warn!(
                ?derived_key,
                ?downloaded_key,
                "Found an active backup but the recovery key we received isn't the one used in \
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
                    self.set_state(BackupState::Disabled);
                }

                Ok(enabled)
            }
            Err(e) => {
                self.set_state(BackupState::Disabled);

                Err(e)
            }
        }
    }

    async fn maybe_resume_backup_from_decryption_key(
        &self,
        decryption_key: BackupDecryptionKey,
        version: Option<String>,
    ) -> Result<bool, Error> {
        let olm_machine = self.client.olm_machine().await;
        let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;

        if let Some(version) = version {
            let backup_key = decryption_key.megolm_v1_public_key();

            self.enable(olm_machine, backup_key, version).await?;

            Ok(true)
        } else {
            Ok(false)
        }
    }

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

    async fn resume_backup_from_stored_backup_key(
        &self,
        olm_machine: &OlmMachine,
    ) -> Result<bool, Error> {
        let backup_keys = olm_machine.store().load_backup_keys().await?;

        if let Some(decryption_key) = backup_keys.decryption_key {
            self.maybe_resume_backup_from_decryption_key(decryption_key, backup_keys.backup_version)
                .await
        } else {
            Ok(false)
        }
    }

    async fn maybe_resume_from_secret_inbox(&self, olm_machine: &OlmMachine) -> Result<(), Error> {
        let secrets = olm_machine.store().get_secrets_from_inbox(&SecretName::RecoveryKey).await?;

        for secret in secrets {
            if self.handle_received_secret(secret).await? {
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

    pub(crate) async fn secret_send_event_handler(_: ToDeviceSecretSendEvent, client: Client) {
        let olm_machine = client.olm_machine().await;

        // TODO: Because of our crude multi-process support, which reloads the whole
        // [`OlmMachine`] the `secrets_stream` might stop giving you updates. Once
        // that's fixed, stop listening to individual secret send events and
        // listen to the secrets stream.
        if let Some(olm_machine) = olm_machine.as_ref() {
            if let Err(_e) =
                client.encryption().backups().maybe_resume_from_secret_inbox(olm_machine).await
            {
                // TODO: Log a warning here.
            }
        }
    }

    pub(crate) async fn handle_received_secret(
        &self,
        secret: GossippedSecret,
    ) -> Result<bool, Error> {
        if secret.secret_name == SecretName::RecoveryKey {
            if self.maybe_enable_backups(&secret.event.content.secret).await? {
                let olm_machine = self.client.olm_machine().await;
                let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;
                olm_machine.store().delete_secrets_from_inbox(&secret.secret_name).await?;

                Ok(true)
            } else {
                Ok(false)
            }
        } else {
            Ok(false)
        }
    }

    pub(crate) fn maybe_trigger_backup(&self) {
        let tasks = self.client.inner.tasks.lock().unwrap();

        if let Some(tasks) = tasks.as_ref() {
            tasks.upload_room_keys.trigger_upload();
        }
    }

    async fn handle_deleted_backup_version(&self, olm_machine: &OlmMachine) -> Result<(), Error> {
        // TODO: Do we want to check if auto enabling via the account data event is
        // turned off? Turn it off ourselves if it isn't?
        olm_machine.backup_machine().disable_backup().await?;
        self.set_state(BackupState::Disabled);
        // TODO: The backup module should not depend on the recovery module.
        self.client.encryption().recovery().update_state_after_backup_disabling().await;

        Ok(())
    }

    #[instrument(skip(self, olm_machine, request))]
    async fn send_backup_request(
        &self,
        olm_machine: &OlmMachine,
        request_id: &TransactionId,
        request: KeysBackupRequest,
    ) -> Result<(), Error> {
        trace!("Uploading some room keys");

        let request = add_backup_keys::v3::Request::new(request.version, request.rooms);

        match self.client.send(request, Default::default()).await {
            Ok(response) => {
                olm_machine.mark_request_as_sent(&request_id, &response).await?;

                let new_counts = olm_machine.backup_machine().room_key_counts().await?;

                self.client
                    .inner
                    .backups_state
                    .upload_progress
                    .set(UploadState::Uploading(new_counts));

                #[cfg(not(target_arch = "wasm32"))]
                {
                    let delay =
                        self.client.inner.backups_state.upload_delay.read().unwrap().to_owned();
                    tokio::time::sleep(delay).await;
                }

                Ok(())
            }
            Err(error) => {
                if let Some(kind) = error.client_api_error_kind() {
                    match kind {
                        ErrorKind::NotFound => {
                            warn!("No backup found on the server, the backup got likely deleted, disabling backups.");

                            self.handle_deleted_backup_version(olm_machine).await?;
                        }
                        ErrorKind::WrongRoomKeysVersion { current_version } => {
                            warn!(
                                new_version = current_version,
                                "A new backup version was found on the server, disabling backups"
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

    pub(crate) async fn backup_room_keys(&self) -> Result<(), Error> {
        let _guard = self.client.locks().backup_upload_lock.lock().await;

        let olm_machine = self.client.olm_machine().await;
        let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;

        let old_counts = olm_machine.backup_machine().room_key_counts().await?;

        self.client
            .inner
            .backups_state
            .upload_progress
            .set(UploadState::CheckingIfUploadNeeded(old_counts));

        while let Some((request_id, request)) = olm_machine.backup_machine().backup().await? {
            self.send_backup_request(olm_machine, &request_id, request).await?;
        }

        self.client.inner.backups_state.upload_progress.set(UploadState::Done);

        Ok(())
    }

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
                let Ok(room_key) = room_key.deserialize() else { continue };

                let Ok(room_key) =
                    backup_decryption_key.decrypt_session_data(room_key.session_data)
                else {
                    continue;
                };
                let Ok(room_key) = serde_json::from_slice(&room_key) else { continue };

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
        let _ = self.client.inner.backups_state.room_keys_broadcaster.send(result);

        Ok(())
    }

    fn room_keys_stream(
        &self,
    ) -> impl Stream<Item = Result<RoomKeyImportResult, BroadcastStreamRecvError>> {
        BroadcastStream::new(self.client.inner.backups_state.room_keys_broadcaster.subscribe())
    }

    pub fn room_keys_for_room_stream(
        &self,
        room_id: &RoomId,
    ) -> impl Stream<Item = Result<BTreeMap<String, BTreeSet<String>>, BroadcastStreamRecvError>>
    {
        let room_id = room_id.to_owned();
        // TODO: This is a bit crap to say the least, the type is non descriptive and
        // doesn't even contain all the important data, it should be a stream of
        // `RoomKeyInfo` like the OlmMachine has... But on the other hand we
        // should just used the OlmMachine stream and remove this.

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

    pub async fn download_room_keys_for_room(&self, room_id: &RoomId) -> Result<(), Error> {
        let olm_machine = self.client.olm_machine().await;
        let olm_machine = olm_machine.as_ref().ok_or(Error::NoOlmMachine)?;

        let backup_keys = olm_machine.store().load_backup_keys().await?;

        if let Some(decryption_key) = backup_keys.decryption_key {
            if let Some(version) = backup_keys.backup_version {
                let request =
                    get_backup_keys_for_room::v3::Request::new(version, room_id.to_owned());
                let response = self.client.send(request, Default::default()).await?;
                let response = get_backup_keys::v3::Response::new(BTreeMap::from([(
                    room_id.to_owned(),
                    RoomKeyBackup::new(response.sessions),
                )]));

                self.handle_downloaded_room_keys(response, decryption_key, olm_machine).await?;
            }
        }

        Ok(())
    }

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

    pub async fn download_all_room_keys(
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

    pub fn state_stream(
        &self,
    ) -> impl Stream<Item = Result<BackupState, BroadcastStreamRecvError>> {
        self.client.inner.backups_state.global_state.subscribe()
    }

    pub fn state(&self) -> BackupState {
        self.client.inner.backups_state.global_state.get()
    }

    pub async fn are_enabled(&self) -> bool {
        let olm_machine = self.client.olm_machine().await;

        if let Some(machine) = olm_machine.as_ref() {
            machine.backup_machine().enabled().await
        } else {
            false
        }
    }

    pub(crate) async fn exists_on_server(&self) -> Result<bool, Error> {
        Ok(self.get_current_version().await?.is_some())
    }
}

#[cfg(test)]
mod test {
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
            .and(path(format!("_matrix/client/unstable/room_keys/version")))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
              "version": "1"
            })))
            .expect(1)
            .named("POST for the backup creation")
            .mount_as_scoped(&server)
            .await;

        let _put_scope = Mock::given(method("PUT"))
            .and(path(format!("_matrix/client/unstable/room_keys/keys")))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(put_response)
            .expect(1)
            .named("POST for the backup creation")
            .mount_as_scoped(&server)
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

        assert_eq!(client.encryption().backups().state(), BackupState::Disabled);
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
                .and(path(format!("_matrix/client/r0/room_keys/version")))
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
                .and(path(format!("_matrix/client/r0/room_keys/version")))
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
                .and(path(format!("_matrix/client/r0/room_keys/version")))
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
                .and(path(format!("_matrix/client/r0/room_keys/version")))
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
            .and(path(format!("_matrix/client/unstable/room_keys/version")))
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

        let old_duration = { client.inner.backups_state.upload_delay.read().unwrap().to_owned() };

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
                        UploadState::CheckingIfUploadNeeded(_) => (),
                        UploadState::Done => {
                            let current_delay = {
                                client.inner.backups_state.upload_delay.read().unwrap().to_owned()
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
            { client.inner.backups_state.upload_delay.read().unwrap().to_owned() };

        assert_eq!(old_duration, current_duration);

        server.verify().await;
    }
}
