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

//! The recovery module
//!
//! The recovery module attempts to provide a unified and simplified view over
//! the secret storage and backup subsystems.
//!
//! **Note**: If you are using this module, do not use the [`SecretStorage`] and
//! [`Backups`] subsystems directly. This module makes assumptions that might be
//! broken by the direct usage of the respective lower level modules.
//!
//! **Note**: The term Recovery used in this submodule is not the same as the
//! [`Recovery key`] mentioned in the spec. The recovery key from the spec is
//! solely about backups, while the term recovery in this file includes both the
//! backups and the secret storage subsystems. The recovery key mentioned in
//! this file is the secret storage key.
//!
//! You should configure your client to bootstrap cross-signing automatically
//! and may choose to let your client automatically create a backup, if it
//! doesn't exist, as well:
//!
//! ```no_run
//! use matrix_sdk::{Client, encryption::EncryptionSettings};
//!
//! # async {
//! # let homeserver = "http://example.org";
//! let client = Client::builder()
//!     .homeserver_url(homeserver)
//!     .with_encryption_settings(EncryptionSettings {
//!         auto_enable_cross_signing: true,
//!         auto_enable_backups: true,
//!         ..Default::default()
//!     })
//!     .build()
//!     .await?;
//! # anyhow::Ok(()) };
//! ```
//!
//! # Examples
//!
//! For a newly registered user you will want to enable recovery, either
//! immediately or before the user logs out.
//!
//! ```no_run
//! # use matrix_sdk::{Client, encryption::recovery::EnableProgress};
//! # use url::Url;
//! # async {
//! # let homeserver = Url::parse("http://example.com")?;
//! # let client = Client::new(homeserver).await?;
//! let recovery = client.encryption().recovery();
//!
//! // Create a new recovery key, you can use the provided passphrase, or the returned recovery key
//! // to recover.
//! let recovery_key = recovery
//!     .enable()
//!     .wait_for_backups_to_upload()
//!     .with_passphrase("my passphrase")
//!     .await;
//! # anyhow::Ok(()) };
//! ```
//!
//! If the user logs in with another device, you'll want to let the user recover
//! its secrets by entering the recovery key or recovery passphrase.
//!
//! ```no_run
//! # use matrix_sdk::{Client, encryption::recovery::EnableProgress};
//! # use url::Url;
//! # async {
//! # let homeserver = Url::parse("http://example.com")?;
//! # let client = Client::new(homeserver).await?;
//! let recovery = client.encryption().recovery();
//!
//! // Create a new recovery key, you can use the provided passphrase, or the returned recovery key
//! // to recover.
//! recovery.recover("my recovery key or passphrase").await;
//! # anyhow::Ok(()) };
//! ```
//!
//! [`Recovery key`]: https://spec.matrix.org/v1.8/client-server-api/#recovery-key

use futures_core::{Future, Stream};
use futures_util::StreamExt as _;
use ruma::{
    api::client::keys::get_keys,
    events::{
        GlobalAccountDataEventType,
        secret::{request::SecretName, send::ToDeviceSecretSendEvent},
        secret_storage::{default_key::SecretStorageDefaultKeyEvent, secret::SecretEventContent},
    },
    serde::Raw,
};
use serde_json::{json, value::to_raw_value};
use tracing::{error, info, instrument, warn};

#[cfg(doc)]
use crate::encryption::{
    backups::Backups,
    secret_storage::{SecretStorage, SecretStore},
};
use crate::{Client, client::WeakClient, encryption::backups::BackupState};

pub mod futures;
mod types;
pub use self::types::{EnableProgress, RecoveryError, RecoveryState, Result};
use self::{
    futures::{Enable, RecoverAndReset, Reset},
    types::{BackupDisabledContent, SecretStorageDisabledContent},
};
use crate::encryption::{AuthData, CrossSigningResetAuthType, CrossSigningResetHandle};

/// The recovery manager for the [`Client`].
#[derive(Debug)]
pub struct Recovery {
    pub(super) client: Client,
}

impl Recovery {
    /// The list of known secrets that are contained in secret storage once
    /// recover is enabled.
    pub const KNOWN_SECRETS: &[SecretName] = &[
        SecretName::CrossSigningMasterKey,
        SecretName::CrossSigningUserSigningKey,
        SecretName::CrossSigningSelfSigningKey,
        SecretName::RecoveryKey,
    ];

    /// Get the current [`RecoveryState`] for this [`Client`].
    pub fn state(&self) -> RecoveryState {
        self.client.inner.e2ee.recovery_state.get()
    }

    /// Get a stream of updates to the [`RecoveryState`].
    ///
    /// This method will send out the current state as the first update.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, encryption::recovery::RecoveryState};
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// use futures_util::StreamExt;
    ///
    /// let recovery = client.encryption().recovery();
    ///
    /// let mut state_stream = recovery.state_stream();
    ///
    /// while let Some(update) = state_stream.next().await {
    ///     match update {
    ///         RecoveryState::Enabled => {
    ///             println!("Recovery has been enabled");
    ///         }
    ///         _ => (),
    ///     }
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub fn state_stream(&self) -> impl Stream<Item = RecoveryState> + use<> {
        self.client.inner.e2ee.recovery_state.subscribe_reset()
    }

    /// Enable secret storage *and* backups.
    ///
    /// This method will create a new secret storage key and a new backup if one
    /// doesn't already exist. It will then upload all the locally cached
    /// secrets, including the backup recovery key, to the new secret store.
    ///
    /// This method will throw an error if a backup already exists on the
    /// homeserver but this [`Client`] isn't connected to the existing backup.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, encryption::recovery::EnableProgress};
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// use futures_util::StreamExt;
    ///
    /// let recovery = client.encryption().recovery();
    ///
    /// let enable = recovery
    ///     .enable()
    ///     .wait_for_backups_to_upload()
    ///     .with_passphrase("my passphrase");
    ///
    /// let mut progress_stream = enable.subscribe_to_progress();
    ///
    /// tokio::spawn(async move {
    ///     while let Some(update) = progress_stream.next().await {
    ///         let Ok(update) = update else {
    ///             panic!("Update to the enable progress lagged")
    ///         };
    ///
    ///         match update {
    ///             EnableProgress::CreatingBackup => {
    ///                 println!("Creating a new backup");
    ///             }
    ///             EnableProgress::CreatingRecoveryKey => {
    ///                 println!("Creating a new recovery key");
    ///             }
    ///             EnableProgress::Done { .. } => {
    ///                 println!("Recovery has been enabled");
    ///                 break;
    ///             }
    ///             _ => (),
    ///         }
    ///     }
    /// });
    ///
    /// let recovery_key = enable.await?;
    ///
    /// # anyhow::Ok(()) };
    /// ```
    #[instrument(skip_all)]
    pub fn enable(&self) -> Enable<'_> {
        Enable::new(self)
    }

    /// Create a new backup if one does not exist yet.
    ///
    /// This method will throw an error if a backup already exists on the
    /// homeserver but this [`Client`] isn't connected to the existing backup.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, encryption::backups::BackupState};
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// let recovery = client.encryption().recovery();
    ///
    /// recovery.enable_backup().await?;
    ///
    /// assert_eq!(client.encryption().backups().state(), BackupState::Enabled);
    ///
    /// # anyhow::Ok(()) };
    /// ```
    #[instrument(skip_all)]
    pub async fn enable_backup(&self) -> Result<()> {
        if !self.client.encryption().backups().fetch_exists_on_server().await? {
            self.mark_backup_as_enabled().await?;

            self.client.encryption().backups().create().await?;
            self.client.encryption().backups().maybe_trigger_backup();

            Ok(())
        } else {
            Err(RecoveryError::BackupExistsOnServer)
        }
    }

    /// Disable recovery completely.
    ///
    /// This method will do the following steps:
    ///
    /// 1. Disable the uploading of room keys to a currently active backup.
    /// 2. Delete the currently active backup.
    /// 3. Set the `m.secret_storage.default_key` global account data event to
    ///    an empty JSON content.
    /// 4. Set a global account data event so clients won't attempt to
    ///    automatically re-enable a backup.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, encryption::recovery::RecoveryState};
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// let recovery = client.encryption().recovery();
    ///
    /// recovery.disable().await?;
    ///
    /// assert_eq!(recovery.state(), RecoveryState::Disabled);
    ///
    /// # anyhow::Ok(()) };
    /// ```
    #[instrument(skip_all)]
    pub async fn disable(&self) -> Result<()> {
        self.client.encryption().backups().disable().await?;

        // Why oh why, can't we delete account data events?
        //
        // Alright, let's attempt to "delete" the content of our current default key,
        // for this we first need to check if there is a default key, then
        // deserialize the content and find out the key ID.
        //
        // Then we finally set the event to an empty JSON content.
        if let Ok(Some(default_event)) =
            self.client.encryption().secret_storage().fetch_default_key_id().await
            && let Ok(default_event) = default_event.deserialize()
        {
            let key_id = default_event.key_id;
            let event_type = GlobalAccountDataEventType::SecretStorageKey(key_id);

            self.client
                .account()
                .set_account_data_raw(event_type, Raw::new(&json!({})).expect("").cast_unchecked())
                .await?;
        }

        // Now let's "delete" the actual `m.secret.storage.default_key` event.
        self.client.account().set_account_data(SecretStorageDisabledContent {}).await?;
        // Make sure that we don't re-enable backups automatically.
        self.client.account().set_account_data(BackupDisabledContent { disabled: true }).await?;
        // Finally, "delete" all the known secrets we have in the account data.
        self.delete_all_known_secrets().await?;

        self.update_recovery_state().await?;

        Ok(())
    }

    /// Reset the recovery key.
    ///
    /// This will rotate the secret storage key and re-upload all the secrets to
    /// the [`SecretStore`].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, encryption::recovery::RecoveryState};
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// let recovery = client.encryption().recovery();
    ///
    /// let new_recovery_key =
    ///     recovery.reset_key().with_passphrase("my passphrase").await;
    /// # anyhow::Ok(()) };
    /// ```
    #[instrument(skip_all)]
    pub fn reset_key(&self) -> Reset<'_> {
        // TODO: Should this only be possible if we're in the RecoveryState::Enabled
        // state? Otherwise we'll create a new secret store but won't be able to
        // upload all the secrets.
        Reset::new(self)
    }

    /// Reset the recovery key but first import all the secrets from secret
    /// storage.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, encryption::recovery::RecoveryState};
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// let recovery = client.encryption().recovery();
    ///
    /// let new_recovery_key = recovery
    ///     .recover_and_reset("my old passphrase or key")
    ///     .with_passphrase("my new passphrase")
    ///     .await?;
    /// # anyhow::Ok(()) };
    /// ```
    #[instrument(skip_all)]
    pub fn recover_and_reset<'a>(&'a self, old_key: &'a str) -> RecoverAndReset<'a> {
        RecoverAndReset::new(self, old_key)
    }

    /// Completely reset the current user's crypto identity.
    /// This method will go through the following steps:
    ///
    /// 1. Disable backing up room keys and delete the active backup
    /// 2. Disable recovery and delete secret storage
    /// 3. Go through the cross-signing key reset flow
    /// 4. Finally, re-enable key backups (only if they were already enabled)
    ///
    /// Disclaimer: failures in this flow will potentially leave the user in
    /// an inconsistent state but they're expected to just run the reset flow
    /// again as presumably the reason they started it to begin with was
    /// that they no longer had access to any of their data.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{
    ///     encryption::recovery, encryption::CrossSigningResetAuthType, ruma::api::client::uiaa,
    ///     Client,
    ///   };
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// # let user_id = unimplemented!();
    /// let encryption = client.encryption();
    ///
    /// if let Some(handle) = encryption.recovery().reset_identity().await? {
    ///     match handle.auth_type() {
    ///         CrossSigningResetAuthType::Uiaa(uiaa) => {
    ///             let password = "1234".to_owned();
    ///             let mut password = uiaa::Password::new(user_id, password);
    ///             password.session = uiaa.session;
    ///
    ///             handle.reset(Some(uiaa::AuthData::Password(password))).await?;
    ///         }
    ///         CrossSigningResetAuthType::OAuth(o) => {
    ///             println!(
    ///                 "To reset your end-to-end encryption cross-signing identity, \
    ///                 you first need to approve it at {}",
    ///                 o.approval_url
    ///             );
    ///             handle.reset(None).await?;
    ///         }
    ///     }
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn reset_identity(&self) -> Result<Option<IdentityResetHandle>> {
        self.client.encryption().backups().disable_and_delete().await?; // 1.

        // 2. (We can't delete account data events)
        self.client.account().set_account_data(SecretStorageDisabledContent {}).await?;
        self.client.encryption().recovery().update_recovery_state().await?;

        let cross_signing_reset_handle = self.client.encryption().reset_cross_signing().await?;

        if let Some(handle) = cross_signing_reset_handle {
            // Authentication required, backups will be re-enabled after the reset
            Ok(Some(IdentityResetHandle {
                client: self.client.clone(),
                cross_signing_reset_handle: handle,
            }))
        } else {
            // No authentication required, re-enable backups
            if self.client.encryption().recovery().should_auto_enable_backups().await? {
                self.client.encryption().recovery().enable_backup().await?; // 4.
            }

            Ok(None)
        }
    }

    /// Recover all the secrets from the homeserver.
    ///
    /// This method is a convenience method around the
    /// [`SecretStore::import_secrets()`] method, please read the documentation
    /// of this method for more information about what happens if you call
    /// this method.
    ///
    /// In short, this method will turn a newly created [`Client`] into a fully
    /// end-to-end encryption enabled client.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{Client, encryption::recovery::RecoveryState};
    /// # use url::Url;
    /// # async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let client = Client::new(homeserver).await?;
    /// let recovery = client.encryption().recovery();
    ///
    /// recovery.recover("my recovery key or passphrase").await;
    ///
    /// assert_eq!(recovery.state(), RecoveryState::Enabled);
    /// # anyhow::Ok(()) };
    /// ```
    #[instrument(skip_all)]
    pub async fn recover(&self, recovery_key: &str) -> Result<()> {
        let store =
            self.client.encryption().secret_storage().open_secret_store(recovery_key).await?;

        store.import_secrets().await?;
        self.update_recovery_state().await?;

        Ok(())
    }

    /// Is this device the last device the user has?
    ///
    /// This method is useful to check if we should recommend to the user that
    /// they should enable recovery, typically done before logging out.
    ///
    /// If the user does not enable recovery before logging out of their last
    /// device, they will not be able to decrypt historic messages once they
    /// create a new device.
    pub async fn is_last_device(&self) -> Result<bool> {
        let olm_machine = self.client.olm_machine().await;
        let olm_machine = olm_machine.as_ref().ok_or(crate::Error::NoOlmMachine)?;
        let user_id = olm_machine.user_id();

        self.client.encryption().ensure_initial_key_query().await?;

        let devices = self.client.encryption().get_user_devices(user_id).await?;

        Ok(devices.devices().count() == 1)
    }

    /// Did we correctly set up cross-signing and backups?
    async fn all_known_secrets_available(&self) -> Result<bool> {
        // Cross-signing state is fine if we have all the private cross-signing keys, as
        // indicated in the status.
        let cross_signing_complete = self
            .client
            .encryption()
            .cross_signing_status()
            .await
            .map(|status| status.is_complete());
        if !cross_signing_complete.unwrap_or_default() {
            return Ok(false);
        }

        // The backup state is fine if we have backups enabled locally, or if backups
        // have been marked as disabled.
        if self.client.encryption().backups().are_enabled().await {
            Ok(true)
        } else {
            self.are_backups_marked_as_disabled().await
        }
    }

    async fn should_auto_enable_backups(&self) -> Result<bool> {
        // If we didn't already enable backups, we don't see a backup version on the
        // server, and finally if backups have not been marked to be explicitly
        // disabled, then we can automatically enable them.
        Ok(self.client.inner.e2ee.encryption_settings.auto_enable_backups
            && !self.client.encryption().backups().are_enabled().await
            && !self.client.encryption().backups().fetch_exists_on_server().await?
            && !self.are_backups_marked_as_disabled().await?)
    }

    pub(crate) async fn setup(&self) -> Result<()> {
        info!("Setting up account data listeners and trying to setup recovery");

        self.client.add_event_handler(Self::default_key_event_handler);
        self.client.add_event_handler(Self::secret_send_event_handler);
        self.client.inner.e2ee.initialize_recovery_state_update_task(&self.client);

        self.update_recovery_state().await?;

        if self.should_auto_enable_backups().await? {
            info!("Trying to automatically enable backups");

            if let Err(e) = self.enable_backup().await {
                warn!("Could not automatically enable backups: {e:?}");
            }
        }

        Ok(())
    }

    /// Delete all the known secrets we are keeping in secret storage.
    ///
    /// The exact list of secrets is defined in [`Recovery::KNOWN_SECRETS`] and
    /// might change over time.
    ///
    /// Since account data events can't actually be deleted, due to a missing
    /// DELETE API, we're replacing the events with an empty
    /// [`SecretEventContent`].
    async fn delete_all_known_secrets(&self) -> Result<()> {
        for secret_name in Self::KNOWN_SECRETS {
            let event_type = GlobalAccountDataEventType::from(secret_name.to_owned());
            let content = SecretEventContent::new(Default::default());
            let secret_content = Raw::from_json(
                to_raw_value(&content)
                    .expect("We should be able to serialize a raw empty secret event content"),
            );
            self.client.account().set_account_data_raw(event_type, secret_content).await?;
        }

        Ok(())
    }

    /// Run a network request to figure whether backups have been disabled at
    /// the account level.
    async fn are_backups_marked_as_disabled(&self) -> Result<bool> {
        Ok(self
            .client
            .account()
            .fetch_account_data_static::<BackupDisabledContent>()
            .await?
            .map(|event| event.deserialize().map(|event| event.disabled).unwrap_or(false))
            .unwrap_or(false))
    }

    async fn mark_backup_as_enabled(&self) -> Result<()> {
        self.client.account().set_account_data(BackupDisabledContent { disabled: false }).await?;

        Ok(())
    }

    async fn check_recovery_state(&self) -> Result<RecoveryState> {
        Ok(if self.client.encryption().secret_storage().is_enabled().await? {
            if self.all_known_secrets_available().await? {
                RecoveryState::Enabled
            } else {
                RecoveryState::Incomplete
            }
        } else {
            RecoveryState::Disabled
        })
    }

    async fn update_recovery_state(&self) -> Result<()> {
        let new_state = self.check_recovery_state().await?;
        let old_state = self.client.inner.e2ee.recovery_state.set(new_state);

        if new_state != old_state {
            info!("Recovery state changed from {old_state:?} to {new_state:?}");
        }

        Ok(())
    }

    async fn update_recovery_state_no_fail(&self) {
        if let Err(e) = self.update_recovery_state().await {
            error!("Couldn't update the recovery state: {e:?}");
        }
    }

    #[instrument]
    async fn secret_send_event_handler(_: ToDeviceSecretSendEvent, client: Client) {
        client.encryption().recovery().update_recovery_state_no_fail().await;
    }

    #[instrument]
    async fn default_key_event_handler(_: SecretStorageDefaultKeyEvent, client: Client) {
        client.encryption().recovery().update_recovery_state_no_fail().await;
    }

    /// Listen for changes in the [`BackupState`] and, if necessary, update the
    /// [`RecoveryState`] accordingly.
    ///
    /// This should not be called directly, this method is put into a background
    /// task which is always listening for updates in the [`BackupState`].
    pub(crate) fn update_state_after_backup_state_change(
        client: &Client,
    ) -> impl Future<Output = ()> + use<> {
        let mut stream = client.encryption().backups().state_stream();
        let weak = WeakClient::from_client(client);

        async move {
            while let Some(update) = stream.next().await {
                if let Some(client) = weak.get() {
                    match update {
                        Ok(update) => {
                            // The recovery state only cares about these two states, the
                            // intermediate states that tell us that
                            // we're creating a backup are not interesting.
                            if matches!(update, BackupState::Unknown | BackupState::Enabled) {
                                client
                                    .encryption()
                                    .recovery()
                                    .update_recovery_state_no_fail()
                                    .await;
                            }
                        }
                        Err(_) => {
                            // We missed some updates, let's update our state in case something
                            // changed.
                            client.encryption().recovery().update_recovery_state_no_fail().await;
                        }
                    }
                } else {
                    break;
                }
            }
        }
    }

    #[instrument(skip_all)]
    pub(crate) async fn update_state_after_keys_query(&self, response: &get_keys::v3::Response) {
        if let Some(user_id) = self.client.user_id()
            && response.master_keys.contains_key(user_id)
        {
            // TODO: This is unnecessarily expensive, we could let the crypto crate notify
            // us that our private keys got erased... But, the OlmMachine
            // gets recreated and... You know the drill by now...
            self.update_recovery_state_no_fail().await;
        }
    }
}

/// A helper struct that handles continues resetting a user's crypto identity
/// after authentication was required and re-enabling backups (if necessary) at
/// the end of it
#[derive(Debug)]
pub struct IdentityResetHandle {
    client: Client,
    cross_signing_reset_handle: CrossSigningResetHandle,
}

impl IdentityResetHandle {
    /// Get the underlying [`CrossSigningResetAuthType`] this identity reset
    /// process is using.
    pub fn auth_type(&self) -> &CrossSigningResetAuthType {
        &self.cross_signing_reset_handle.auth_type
    }

    /// This method will retry to upload the device keys after the previous try
    /// failed due to required authentication
    pub async fn reset(&self, auth: Option<AuthData>) -> Result<()> {
        self.cross_signing_reset_handle.auth(auth).await?;

        if self.client.encryption().recovery().should_auto_enable_backups().await? {
            self.client.encryption().recovery().enable_backup().await?;
        }

        Ok(())
    }

    /// Cancel the ongoing identity reset process
    pub async fn cancel(&self) {
        self.cross_signing_reset_handle.cancel().await;
    }
}

// The http mocking library is not supported for wasm32
#[cfg(all(test, not(target_family = "wasm")))]
pub(crate) mod tests {
    use assert_matches::assert_matches;
    use matrix_sdk_test::async_test;
    use ruma::{
        events::{secret::request::SecretName, secret_storage::key},
        serde::Base64,
    };
    use serde_json::json;

    use super::Recovery;
    use crate::{
        encryption::{recovery::types::RecoveryError, secret_storage::SecretStorageError},
        test_utils::mocks::MatrixMockServer,
    };

    // If recovery fails due when importing a secret from secret storage, we
    // should get the `ImportError` variant of `SecretStorageError`.  The
    // following tests test different import failures.
    #[async_test]
    async fn test_recover_with_no_cross_signing_key() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server
            .mock_get_secret_storage_key()
            .ok(
                client.user_id().unwrap(),
                &key::SecretStorageKeyEventContent::new(
                    "abc".into(),
                    key::SecretStorageEncryptionAlgorithm::V1AesHmacSha2(
                        key::SecretStorageV1AesHmacSha2Properties::new(
                            Some(Base64::parse("xv5b6/p3ExEw++wTyfSHEg==").unwrap()),
                            Some(
                                Base64::parse("ujBBbXahnTAMkmPUX2/0+VTfUh63pGyVRuBcDMgmJC8=")
                                    .unwrap(),
                            ),
                        ),
                    ),
                ),
            )
            .mount()
            .await;
        server
            .mock_get_default_secret_storage_key()
            .ok(client.user_id().unwrap(), "abc")
            .mount()
            .await;

        let recovery = Recovery { client };

        let ret =
            recovery.recover("EsTj 3yST y93F SLpB jJsz eAXc 2XzA ygD3 w69H fGaN TKBj jXEd").await;

        assert_matches!(
            ret,
            Err(RecoveryError::SecretStorage(SecretStorageError::ImportError {
                name: SecretName::CrossSigningMasterKey,
                error: _
            }))
        );
    }

    #[async_test]
    async fn test_recover_with_invalid_cross_signing_key() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server
            .mock_get_secret_storage_key()
            .ok(
                client.user_id().unwrap(),
                &key::SecretStorageKeyEventContent::new(
                    "abc".into(),
                    key::SecretStorageEncryptionAlgorithm::V1AesHmacSha2(
                        key::SecretStorageV1AesHmacSha2Properties::new(
                            Some(Base64::parse("xv5b6/p3ExEw++wTyfSHEg==").unwrap()),
                            Some(
                                Base64::parse("ujBBbXahnTAMkmPUX2/0+VTfUh63pGyVRuBcDMgmJC8=")
                                    .unwrap(),
                            ),
                        ),
                    ),
                ),
            )
            .mount()
            .await;
        server
            .mock_get_default_secret_storage_key()
            .ok(client.user_id().unwrap(), "abc")
            .mount()
            .await;
        server.mock_get_master_signing_key().ok(client.user_id().unwrap(), json!({})).mount().await;

        let recovery = Recovery { client };

        let ret =
            recovery.recover("EsTj 3yST y93F SLpB jJsz eAXc 2XzA ygD3 w69H fGaN TKBj jXEd").await;

        assert_matches!(
            ret,
            Err(RecoveryError::SecretStorage(SecretStorageError::ImportError {
                name: SecretName::CrossSigningMasterKey,
                error: _
            }))
        );
    }

    #[async_test]
    async fn test_recover_with_undecryptable_cross_signing_key() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        server
            .mock_get_secret_storage_key()
            .ok(
                client.user_id().unwrap(),
                &key::SecretStorageKeyEventContent::new(
                    "abc".into(),
                    key::SecretStorageEncryptionAlgorithm::V1AesHmacSha2(
                        key::SecretStorageV1AesHmacSha2Properties::new(
                            Some(Base64::parse("xv5b6/p3ExEw++wTyfSHEg==").unwrap()),
                            Some(
                                Base64::parse("ujBBbXahnTAMkmPUX2/0+VTfUh63pGyVRuBcDMgmJC8=")
                                    .unwrap(),
                            ),
                        ),
                    ),
                ),
            )
            .mount()
            .await;
        server
            .mock_get_default_secret_storage_key()
            .ok(client.user_id().unwrap(), "abc")
            .mount()
            .await;
        server
            .mock_get_master_signing_key()
            .ok(
                client.user_id().unwrap(),
                json!({
                    "encrypted": {
                        "abc": {
                            "iv": "xv5b6/p3ExEw++wTyfSHEg==",
                            "mac": "ujBBbXahnTAMkmPUX2/0+VTfUh63pGyVRuBcDMgmJC8=",
                            "ciphertext": "abcd"
                        }
                    }
                }),
            )
            .mount()
            .await;

        let recovery = Recovery { client };

        let ret =
            recovery.recover("EsTj 3yST y93F SLpB jJsz eAXc 2XzA ygD3 w69H fGaN TKBj jXEd").await;

        assert_matches!(
            ret,
            Err(RecoveryError::SecretStorage(SecretStorageError::ImportError {
                name: SecretName::CrossSigningMasterKey,
                error: _
            }))
        );
    }
}
