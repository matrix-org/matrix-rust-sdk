use std::sync::Arc;

use futures_util::StreamExt;
use matrix_sdk::{
    encryption,
    encryption::{backups, recovery},
};
use thiserror::Error;
use zeroize::Zeroize;

use super::RUNTIME;
use crate::{client::Client, error::ClientError, ruma::AuthData, task_handle::TaskHandle};

#[derive(uniffi::Object)]
pub struct Encryption {
    pub(crate) inner: matrix_sdk::encryption::Encryption,

    /// A reference to the FFI client.
    ///
    /// Note: we do this to make it so that the FFI `NotificationClient` keeps
    /// the FFI `Client` and thus the SDK `Client` alive. Otherwise, we
    /// would need to repeat the hack done in the FFI `Client::drop` method.
    pub(crate) _client: Arc<Client>,
}

#[uniffi::export(callback_interface)]
pub trait BackupStateListener: Sync + Send {
    fn on_update(&self, status: BackupState);
}

#[uniffi::export(callback_interface)]
pub trait BackupSteadyStateListener: Sync + Send {
    fn on_update(&self, status: BackupUploadState);
}

#[uniffi::export(callback_interface)]
pub trait RecoveryStateListener: Sync + Send {
    fn on_update(&self, status: RecoveryState);
}

#[uniffi::export(callback_interface)]
pub trait VerificationStateListener: Sync + Send {
    fn on_update(&self, status: VerificationState);
}

#[derive(uniffi::Enum)]
pub enum BackupUploadState {
    Waiting,
    Uploading { backed_up_count: u32, total_count: u32 },
    Error,
    Done,
}

#[derive(Debug, Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum SteadyStateError {
    #[error("The backup got disabled while waiting for the room keys to be uploaded.")]
    BackupDisabled,
    #[error("There was a connection error.")]
    Connection,
    #[error("We couldn't read status updates from the upload task quickly enough.")]
    Lagged,
}

#[derive(Debug, Error, uniffi::Error)]
pub enum RecoveryError {
    /// A backup already exists on the homeserver, the recovery subsystem does
    /// not allow backups to be overwritten, disable recovery first.
    #[error(
        "A backup already exists on the homeserver and the method does not allow to overwrite it"
    )]
    BackupExistsOnServer,

    /// A typical SDK error.
    #[error(transparent)]
    Client { source: crate::ClientError },

    /// Error in the secret storage subsystem.
    #[error("Error in the secret-storage subsystem: {error_message}")]
    SecretStorage { error_message: String },
}

impl From<matrix_sdk::encryption::recovery::RecoveryError> for RecoveryError {
    fn from(value: matrix_sdk::encryption::recovery::RecoveryError) -> Self {
        match value {
            recovery::RecoveryError::BackupExistsOnServer => Self::BackupExistsOnServer,
            recovery::RecoveryError::Sdk(e) => Self::Client { source: ClientError::from(e) },
            recovery::RecoveryError::SecretStorage(e) => {
                Self::SecretStorage { error_message: e.to_string() }
            }
        }
    }
}

pub type Result<A, E = RecoveryError> = std::result::Result<A, E>;

impl From<matrix_sdk::encryption::backups::futures::SteadyStateError> for SteadyStateError {
    fn from(value: matrix_sdk::encryption::backups::futures::SteadyStateError) -> Self {
        match value {
            backups::futures::SteadyStateError::BackupDisabled => Self::BackupDisabled,
            backups::futures::SteadyStateError::Connection => Self::Connection,
            backups::futures::SteadyStateError::Lagged => Self::Lagged,
        }
    }
}

#[derive(uniffi::Enum)]
pub enum BackupState {
    Unknown,
    Creating,
    Enabling,
    Resuming,
    Enabled,
    Downloading,
    Disabling,
}

impl From<backups::BackupState> for BackupState {
    fn from(value: backups::BackupState) -> Self {
        match value {
            backups::BackupState::Unknown => Self::Unknown,
            backups::BackupState::Creating => Self::Creating,
            backups::BackupState::Enabling => Self::Enabling,
            backups::BackupState::Resuming => Self::Resuming,
            backups::BackupState::Enabled => Self::Enabled,
            backups::BackupState::Downloading => Self::Downloading,
            backups::BackupState::Disabling => Self::Disabling,
        }
    }
}

impl From<backups::UploadState> for BackupUploadState {
    fn from(value: backups::UploadState) -> Self {
        match value {
            backups::UploadState::Idle => Self::Waiting,
            backups::UploadState::Uploading(count) => Self::Uploading {
                backed_up_count: count.backed_up.try_into().unwrap_or(u32::MAX),
                total_count: count.total.try_into().unwrap_or(u32::MAX),
            },
            backups::UploadState::Error => Self::Error,
            backups::UploadState::Done => Self::Done,
        }
    }
}

#[derive(uniffi::Enum)]
pub enum RecoveryState {
    Unknown,
    Enabled,
    Disabled,
    Incomplete,
}

impl From<recovery::RecoveryState> for RecoveryState {
    fn from(value: recovery::RecoveryState) -> Self {
        match value {
            recovery::RecoveryState::Unknown => Self::Unknown,
            recovery::RecoveryState::Enabled => Self::Enabled,
            recovery::RecoveryState::Disabled => Self::Disabled,
            recovery::RecoveryState::Incomplete => Self::Incomplete,
        }
    }
}

#[uniffi::export(callback_interface)]
pub trait EnableRecoveryProgressListener: Sync + Send {
    fn on_update(&self, status: EnableRecoveryProgress);
}

#[derive(uniffi::Enum)]
pub enum EnableRecoveryProgress {
    Starting,
    CreatingBackup,
    CreatingRecoveryKey,
    BackingUp { backed_up_count: u32, total_count: u32 },
    RoomKeyUploadError,
    Done { recovery_key: String },
}

impl From<recovery::EnableProgress> for EnableRecoveryProgress {
    fn from(value: recovery::EnableProgress) -> Self {
        match &value {
            recovery::EnableProgress::Starting => Self::Starting,
            recovery::EnableProgress::CreatingBackup => Self::CreatingBackup,
            recovery::EnableProgress::CreatingRecoveryKey => Self::CreatingRecoveryKey,
            recovery::EnableProgress::BackingUp(counts) => Self::BackingUp {
                backed_up_count: counts.backed_up.try_into().unwrap_or(u32::MAX),
                total_count: counts.backed_up.try_into().unwrap_or(u32::MAX),
            },
            recovery::EnableProgress::RoomKeyUploadError => Self::RoomKeyUploadError,
            recovery::EnableProgress::Done { recovery_key } => {
                Self::Done { recovery_key: recovery_key.to_owned() }
            }
        }
    }
}

#[derive(uniffi::Enum)]
pub enum VerificationState {
    Unknown,
    Verified,
    Unverified,
}

impl From<encryption::VerificationState> for VerificationState {
    fn from(value: encryption::VerificationState) -> Self {
        match &value {
            encryption::VerificationState::Unknown => Self::Unknown,
            encryption::VerificationState::Verified => Self::Verified,
            encryption::VerificationState::Unverified => Self::Unverified,
        }
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl Encryption {
    /// Get the public ed25519 key of our own device. This is usually what is
    /// called the fingerprint of the device.
    pub async fn ed25519_key(&self) -> Option<String> {
        self.inner.ed25519_key().await
    }

    /// Get the public curve25519 key of our own device in base64. This is
    /// usually what is called the identity key of the device.
    pub async fn curve25519_key(&self) -> Option<String> {
        self.inner.curve25519_key().await.map(|k| k.to_base64())
    }

    pub fn backup_state_listener(&self, listener: Box<dyn BackupStateListener>) -> Arc<TaskHandle> {
        let mut stream = self.inner.backups().state_stream();

        let stream_task = TaskHandle::new(RUNTIME.spawn(async move {
            while let Some(state) = stream.next().await {
                let Ok(state) = state else { continue };
                listener.on_update(state.into());
            }
        }));

        stream_task.into()
    }

    pub fn backup_state(&self) -> BackupState {
        self.inner.backups().state().into()
    }

    /// Does a backup exist on the server?
    ///
    /// Because the homeserver doesn't notify us about changes to the backup
    /// version, the [`BackupState`] and its listener are a bit crippled.
    /// The `BackupState::Unknown` state might mean there is no backup at all or
    /// a backup exists but we don't have access to it.
    ///
    /// Therefore it is necessary to poll the server for an answer every time
    /// you want to differentiate between those two states.
    pub async fn backup_exists_on_server(&self) -> Result<bool, ClientError> {
        Ok(self.inner.backups().exists_on_server().await?)
    }

    pub fn recovery_state(&self) -> RecoveryState {
        self.inner.recovery().state().into()
    }

    pub fn recovery_state_listener(
        &self,
        listener: Box<dyn RecoveryStateListener>,
    ) -> Arc<TaskHandle> {
        let mut stream = self.inner.recovery().state_stream();

        let stream_task = TaskHandle::new(RUNTIME.spawn(async move {
            while let Some(state) = stream.next().await {
                listener.on_update(state.into());
            }
        }));

        stream_task.into()
    }

    pub async fn enable_backups(&self) -> Result<()> {
        Ok(self.inner.recovery().enable_backup().await?)
    }

    pub async fn is_last_device(&self) -> Result<bool> {
        Ok(self.inner.recovery().are_we_the_last_man_standing().await?)
    }

    pub async fn wait_for_backup_upload_steady_state(
        &self,
        progress_listener: Option<Box<dyn BackupSteadyStateListener>>,
    ) -> Result<(), SteadyStateError> {
        let backups = self.inner.backups();
        let wait_for_steady_state = backups.wait_for_steady_state();

        let task = if let Some(listener) = progress_listener {
            let mut progress_stream = wait_for_steady_state.subscribe_to_progress();

            Some(RUNTIME.spawn(async move {
                while let Some(progress) = progress_stream.next().await {
                    let Ok(progress) = progress else { continue };
                    listener.on_update(progress.into());
                }
            }))
        } else {
            None
        };

        let result = wait_for_steady_state.await;

        if let Some(task) = task {
            task.abort();
        }

        Ok(result?)
    }

    pub async fn enable_recovery(
        &self,
        wait_for_backups_to_upload: bool,
        progress_listener: Box<dyn EnableRecoveryProgressListener>,
    ) -> Result<String> {
        let recovery = self.inner.recovery();

        let enable = if wait_for_backups_to_upload {
            recovery.enable().wait_for_backups_to_upload()
        } else {
            recovery.enable()
        };

        let mut progress_stream = enable.subscribe_to_progress();

        let task = RUNTIME.spawn(async move {
            while let Some(progress) = progress_stream.next().await {
                let Ok(progress) = progress else { continue };
                progress_listener.on_update(progress.into());
            }
        });

        let ret = enable.await?;

        task.abort();

        Ok(ret)
    }

    pub async fn disable_recovery(&self) -> Result<()> {
        Ok(self.inner.recovery().disable().await?)
    }

    pub async fn reset_recovery_key(&self) -> Result<String> {
        Ok(self.inner.recovery().reset_key().await?)
    }

    pub async fn recover_and_reset(&self, mut old_recovery_key: String) -> Result<String> {
        let result = self.inner.recovery().recover_and_reset(&old_recovery_key).await;

        old_recovery_key.zeroize();

        Ok(result?)
    }

    /// Completely reset the current user's crypto identity: reset the cross
    /// signing keys, delete the existing backup and recovery key.
    pub async fn reset_identity(&self) -> Result<Option<Arc<IdentityResetHandle>>, ClientError> {
        if let Some(reset_handle) = self
            .inner
            .recovery()
            .reset_identity()
            .await
            .map_err(|e| ClientError::Generic { msg: e.to_string() })?
        {
            return Ok(Some(Arc::new(IdentityResetHandle { inner: reset_handle })));
        }

        Ok(None)
    }

    pub async fn recover(&self, mut recovery_key: String) -> Result<()> {
        let result = self.inner.recovery().recover(&recovery_key).await;

        recovery_key.zeroize();

        Ok(result?)
    }

    pub fn verification_state(&self) -> VerificationState {
        self.inner.verification_state().get().into()
    }

    pub fn verification_state_listener(
        self: Arc<Self>,
        listener: Box<dyn VerificationStateListener>,
    ) -> Arc<TaskHandle> {
        let mut subscriber = self.inner.verification_state();
        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            while let Some(verification_state) = subscriber.next().await {
                listener.on_update(verification_state.into());
            }
        })))
    }

    /// Waits for end-to-end encryption initialization tasks to finish, if any
    /// was running in the background.
    pub async fn wait_for_e2ee_initialization_tasks(&self) {
        self.inner.wait_for_e2ee_initialization_tasks().await;
    }
}

#[derive(uniffi::Object)]
pub struct IdentityResetHandle {
    pub(crate) inner: matrix_sdk::encryption::recovery::IdentityResetHandle,
}

#[uniffi::export(async_runtime = "tokio")]
impl IdentityResetHandle {
    /// Get the underlying [`CrossSigningResetAuthType`] this identity reset
    /// process is using.
    pub fn auth_type(&self) -> CrossSigningResetAuthType {
        self.inner.auth_type().into()
    }

    /// This method starts the identity reset process and
    /// will go through the following steps:
    ///
    /// 1. Disable backing up room keys and delete the active backup
    /// 2. Disable recovery and delete secret storage
    /// 3. Go through the cross-signing key reset flow
    /// 4. Finally, re-enable key backups only if they were enabled before
    pub async fn reset(&self, auth: Option<AuthData>) -> Result<(), ClientError> {
        if let Some(auth) = auth {
            self.inner
                .reset(Some(auth.into()))
                .await
                .map_err(|e| ClientError::Generic { msg: e.to_string() })
        } else {
            self.inner.reset(None).await.map_err(|e| ClientError::Generic { msg: e.to_string() })
        }
    }

    pub async fn cancel(&self) {
        self.inner.cancel().await;
    }
}

#[derive(uniffi::Enum)]
pub enum CrossSigningResetAuthType {
    /// The homeserver requires user-interactive authentication.
    Uiaa,
    // /// OIDC is used for authentication and the user needs to open a URL to
    // /// approve the upload of cross-signing keys.
    Oidc {
        info: OidcCrossSigningResetInfo,
    },
}

impl From<&matrix_sdk::encryption::CrossSigningResetAuthType> for CrossSigningResetAuthType {
    fn from(value: &matrix_sdk::encryption::CrossSigningResetAuthType) -> Self {
        match value {
            encryption::CrossSigningResetAuthType::Uiaa(_) => Self::Uiaa,
            encryption::CrossSigningResetAuthType::Oidc(info) => Self::Oidc { info: info.into() },
        }
    }
}

#[derive(uniffi::Record)]
pub struct OidcCrossSigningResetInfo {
    /// The error message we received from the homeserver after we attempted to
    /// reset the cross-signing keys.
    pub error: String,
    /// The URL where the user can approve the reset of the cross-signing keys.
    pub approval_url: String,
}

impl From<&matrix_sdk::encryption::OidcCrossSigningResetInfo> for OidcCrossSigningResetInfo {
    fn from(value: &matrix_sdk::encryption::OidcCrossSigningResetInfo) -> Self {
        Self { error: value.error.to_owned(), approval_url: value.approval_url.to_string() }
    }
}
