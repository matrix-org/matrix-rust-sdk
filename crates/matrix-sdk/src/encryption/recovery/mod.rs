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
//! Dear spec connosieuers, recovery here is not the same as the [`Recovery
//! key`] mentioned in the spec. The recovery key from the spec is solely about
//! backups, while the term recovery in this file includes both the backups
//! and the secret storage mechanism. The recovery key mentioned in this file is
//! the secret storage key.
//!
//! [`Recovery key`]: https://spec.matrix.org/v1.8/client-server-api/#recovery-key

#![allow(missing_docs)]

use futures_core::Stream;
use ruma::{
    api::client::keys::get_keys,
    events::{
        secret::send::ToDeviceSecretSendEvent,
        secret_storage::default_key::SecretStorageDefaultKeyEvent, EventContent,
        GlobalAccountDataEventType,
    },
    exports::ruma_macros::EventContent,
};
use serde::{Deserialize, Serialize};
use tracing::{error, info};

use crate::{encryption::secret_storage::Result, Client, Error};

mod futures;

pub use futures::{Enable, EnableProgress, RecoverAndReset, Reset};

#[derive(Clone, Copy, Debug, Default)]
pub enum RecoveryState {
    /// We didn't yet inform ourselves about the state of things.
    #[default]
    Unknown,
    /// Secret storage is setup and we have all the secrets locally.
    Enabled,
    /// No default secret storage key exists or it is disabled explicitly using
    /// the account data event.
    Disabled,
    /// Secret storage is setup but we're missing some secrets.
    Incomplete,
}

// TODO: This should likely be in the matrix-sdk-ui crate.

/// A hack to allow the `m.secret_storage.default_key` event to be "deleted".
#[derive(Clone, Debug, Default, Deserialize, Serialize, EventContent)]
#[ruma_event(type = "m.secret_storage.default_key", kind = GlobalAccountData)]
struct SecretStorageDisabledContent {}

#[derive(Clone, Debug, Default, Deserialize, Serialize, EventContent)]
#[ruma_event(type = "m.org.matrix.custom.backup_disabled", kind = GlobalAccountData)]
struct BackupDisabledContent {
    disabled: bool,
}

impl BackupDisabledContent {
    fn event_type() -> GlobalAccountDataEventType {
        // This is dumb, there's got to be a better way to get to the event type?
        Self { disabled: false }.event_type()
    }
}

#[derive(Debug)]
pub struct Recovery {
    pub(super) client: Client,
}

impl Recovery {
    pub fn state(&self) -> RecoveryState {
        self.client.inner.recovery_state.get()
    }

    pub fn state_stream(&self) -> impl Stream<Item = RecoveryState> {
        self.client.inner.recovery_state.subscribe_reset()
    }

    /// Enable secret storage *and* backups.
    pub fn enable(&self) -> Enable<'_> {
        Enable::new(self)
    }

    /// Enable backups only.
    pub async fn enable_backup(&self) -> Result<()> {
        if !self.client.encryption().backups().exists_on_server().await? {
            self.mark_backup_as_enabled().await?;

            self.client.encryption().backups().create().await?;
            self.client.encryption().backups().maybe_trigger_backup();

            Ok(())
        } else {
            // TODO: Pick a better error.
            Err(Error::InconsistentState.into())
        }
    }

    /// Disable recovery completely.
    ///
    /// This method will disable the uploading of room keys to a backup, delete
    /// the currently active backup version, and remove the default secret
    /// storage key. It will not delete the `m.secret_storage.default_key`
    /// global account data even since that's not possible, but it will set it
    /// to an invalid event. Can someone explain me why a key/value store
    /// doesn't have a `DELETE` method?
    pub async fn disable(&self) -> Result<()> {
        self.client.encryption().backups().disable().await?;
        // Why oh why, can't we delete account data events?
        self.client.account().set_account_data(SecretStorageDisabledContent {}).await?;
        self.client.account().set_account_data(BackupDisabledContent { disabled: true }).await?;
        // TODO: Do we want to "delete" the known secrets as well?

        Ok(())
    }

    /// Reset the recovery key.
    ///
    /// This will rotate the secret storage key and re-upload all the secrets to
    /// the [`SecretStore`].
    ///
    /// [`SecretStore`]: crate::encryption::secret_storage::SecretStore
    pub fn reset_key(&self) -> Reset<'_> {
        Reset::new(self)
    }

    /// Reset the recovery key but first import all the secrets from foobar.
    pub fn recover_and_reset<'a>(&'a self, old_key: &'a str) -> RecoverAndReset<'_> {
        RecoverAndReset::new(self, old_key)
    }

    /// What the fuck is this supposed to do if not fetch the secrets from
    /// secret storage? How is that different from the initial restore stuff
    /// from secret storage flow?
    pub async fn recover(&self, recovery_key: &str) -> Result<()> {
        let store =
            self.client.encryption().secret_storage().open_secret_store(recovery_key).await?;

        store.import_secrets().await?;
        self.update_recovery_state().await?;

        Ok(())
    }

    pub async fn is_recovery_enabled(&self) -> Result<bool> {
        // TODO: Should this take backups into consideration?
        Ok(self.client.encryption().secret_storage().is_enabled().await?)
    }

    /// Is this device the last device the user has.
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

        if self.are_backups_disabled().await? {
            Ok(cross_signing_complete)
        } else {
            Ok(self.client.encryption().backups().are_enabled().await && cross_signing_complete)
        }
    }

    async fn should_auto_enable_backups(&self) -> Result<bool> {
        // If we didn't already enable backups, we don't see a backup version on the
        // server and finally if backups have not been marked to be explicitly
        // disabled, then we can automatically enable them.
        Ok(self.client.inner.encryption_settings.auto_enable_backups
            && !self.client.encryption().backups().are_enabled().await
            && !self.client.encryption().backups().exists_on_server().await?
            && !self.are_backups_disabled().await?)
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

    async fn are_backups_disabled(&self) -> Result<bool> {
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
        Ok(if self.is_recovery_enabled().await? {
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

    async fn secret_send_event_handler(_: ToDeviceSecretSendEvent, client: Client) {
        if let Err(e) = client.encryption().recovery().update_recovery_state().await {
            error!(
                "Coulnd't update the recovery state after the receival of a secret send \
                 event {e:?}"
            );
        }
    }

    async fn default_key_event_handler(_: SecretStorageDefaultKeyEvent, client: Client) {
        if let Err(e) = client.encryption().recovery().update_recovery_state().await {
            error!(
                "Coulnd't update the recovery state after the receival of a new \
                 default_key event {e:?}"
            );
        }
    }

    pub(crate) async fn update_state_after_backup_disabling(&self) {
        // TODO: This is quite ugly, backups shouldn't depend on recovery,
        // recovery should listen to the backup state change.
        if let Err(e) = self.update_recovery_state().await {
            error!("Coulnd't update the recovery state after backups were disabled {e:?}");
        }
    }

    pub(crate) async fn update_state_after_keys_query(&self, response: &get_keys::v3::Response) {
        if let Some(user_id) = self.client.user_id() {
            if response.master_keys.contains_key(user_id) {
                // TODO: This is unnecessarily expensive, we could let the crypto crate notify
                // us that our private keys got erased... Butt, the OlmMachine
                // gets recreated and... You know the drill by now...
                if let Err(e) = self.update_recovery_state().await {
                    error!(
                        "Couldn't update the recovery state after a /keys/query updated our \
                         user identity {e:?}"
                    );
                }
            }
        }
    }
}
