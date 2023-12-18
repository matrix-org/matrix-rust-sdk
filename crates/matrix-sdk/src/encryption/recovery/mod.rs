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
//! and may chose to let your client automatically create a backup, if it
//! doesn't exist, as well:
//!
//! ```no_run
//! use matrix_sdk::{encryption::EncryptionSettings, Client};
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

use futures_core::Stream;
use ruma::{
    api::client::keys::get_keys,
    events::{
        secret::send::ToDeviceSecretSendEvent,
        secret_storage::default_key::SecretStorageDefaultKeyEvent,
    },
};
use tracing::{error, info, instrument};

#[cfg(doc)]
use crate::encryption::{
    backups::Backups,
    secret_storage::{SecretStorage, SecretStore},
};
use crate::Client;

pub mod futures;
mod types;
pub use self::types::{EnableProgress, RecoveryError, RecoveryState, Result};
use self::{
    futures::{Enable, RecoverAndReset, Reset},
    types::{BackupDisabledContent, SecretStorageDisabledContent},
};

/// The recovery manager for the [`Client`].
#[derive(Debug)]
pub struct Recovery {
    pub(super) client: Client,
}

impl Recovery {
    /// Get the current [`RecoveryState`] for this [`Client`].
    pub fn state(&self) -> RecoveryState {
        self.client.inner.recovery_state.get()
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
    pub fn state_stream(&self) -> impl Stream<Item = RecoveryState> {
        self.client.inner.recovery_state.subscribe_reset()
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
        if !self.client.encryption().backups().exists_on_server().await? {
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
        self.client.account().set_account_data(SecretStorageDisabledContent {}).await?;
        self.client.account().set_account_data(BackupDisabledContent { disabled: true }).await?;
        self.update_recovery_state().await?;
        // TODO: Do we want to "delete" the known secrets as well?

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
    pub fn recover_and_reset<'a>(&'a self, old_key: &'a str) -> RecoverAndReset<'_> {
        RecoverAndReset::new(self, old_key)
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
    pub async fn are_we_the_last_man_standing(&self) -> Result<bool> {
        let olm_machine = self.client.olm_machine().await;
        let olm_machine = olm_machine.as_ref().ok_or(crate::Error::NoOlmMachine)?;
        let user_id = olm_machine.user_id();

        self.client.encryption().ensure_initial_key_query().await?;

        let devices = self.client.encryption().get_user_devices(user_id).await?;

        Ok(devices.devices().count() == 1)
    }

    async fn all_known_secrets_available(&self) -> Result<bool> {
        let cross_signing_complete = self
            .client
            .encryption()
            .cross_signing_status()
            .await
            .map(|status| status.is_complete())
            .unwrap_or_default();

        // The backup state is fine if we have backups enabled locally, or if backups
        // have been marked as disabled.
        let backup_state_ok = if self.client.encryption().backups().are_enabled().await {
            true
        } else {
            self.are_backups_marked_as_disabled().await?
        };

        Ok(cross_signing_complete && backup_state_ok)
    }

    async fn should_auto_enable_backups(&self) -> Result<bool> {
        // If we didn't already enable backups, we don't see a backup version on the
        // server, and finally if backups have not been marked to be explicitly
        // disabled, then we can automatically enable them.
        Ok(self.client.inner.encryption_settings.auto_enable_backups
            && !self.client.encryption().backups().are_enabled().await
            && !self.client.encryption().backups().exists_on_server().await?
            && !self.are_backups_marked_as_disabled().await?)
    }

    pub(crate) async fn setup(&self) -> Result<()> {
        info!("Setting up account data listeners and trying to setup recovery");

        self.update_recovery_state().await?;

        if self.should_auto_enable_backups().await? {
            self.enable_backup().await?;
        }

        self.client.add_event_handler(Self::default_key_event_handler);
        self.client.add_event_handler(Self::secret_send_event_handler);

        Ok(())
    }

    async fn are_backups_marked_as_disabled(&self) -> Result<bool> {
        Ok(self
            .client
            .account()
            .fetch_account_data(BackupDisabledContent::event_type())
            .await?
            .map(|event| {
                event
                    .deserialize_as::<BackupDisabledContent>()
                    .map(|event| event.disabled)
                    .unwrap_or(false)
            })
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
        self.client.inner.recovery_state.set(new_state);

        Ok(())
    }

    async fn update_recovery_state_no_fail(&self) {
        if let Err(e) = self.update_recovery_state().await {
            error!("Coulnd't update the recovery state: {e:?}");
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

    #[instrument]
    pub(crate) async fn update_state_after_backup_disabling(&self) {
        // TODO: This is quite ugly, this method is called by the backups subsystem.
        // Backups shouldn't depend on recovery, recovery should listen to the
        // backup state change.
        self.update_recovery_state_no_fail().await;
    }

    #[instrument]
    pub(crate) async fn update_state_after_keys_query(&self, response: &get_keys::v3::Response) {
        if let Some(user_id) = self.client.user_id() {
            if response.master_keys.contains_key(user_id) {
                // TODO: This is unnecessarily expensive, we could let the crypto crate notify
                // us that our private keys got erased... But, the OlmMachine
                // gets recreated and... You know the drill by now...
                self.update_recovery_state_no_fail().await;
            }
        }
    }
}
