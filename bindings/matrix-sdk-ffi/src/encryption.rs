use std::sync::Arc;

use futures_util::StreamExt;
use matrix_sdk::encryption::{backups, recovery};
use zeroize::Zeroize;

use super::RUNTIME;
use crate::{error::ClientError, task_handle::TaskHandle};

#[derive(uniffi::Object)]
pub struct Encryption {
    inner: matrix_sdk::encryption::Encryption,
}

impl From<matrix_sdk::encryption::Encryption> for Encryption {
    fn from(value: matrix_sdk::encryption::Encryption) -> Self {
        Self { inner: value }
    }
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

#[derive(uniffi::Enum)]
pub enum BackupUploadState {
    Waiting,
    CheckingIfUploadNeeded { backed_up_count: u32, total_count: u32 },
    Uploading { backed_up_count: u32, total_count: u32 },
    Error,
    Done,
}

#[derive(Debug, thiserror::Error)]
pub enum SteadyStateError {
    #[error("The backup got disabled while waiting for the room keys to be uploaded.")]
    BackupDisabled,
    #[error("There was a connection error.")]
    Connection,
    #[error("We couldn't read status updates from the upload task quickly enough.")]
    Laged,
}

impl From<matrix_sdk::encryption::backups::SteadyStateError> for SteadyStateError {
    fn from(value: matrix_sdk::encryption::backups::SteadyStateError) -> Self {
        match value {
            backups::SteadyStateError::BackupDisabled => Self::BackupDisabled,
            backups::SteadyStateError::Connection => Self::Connection,
            backups::SteadyStateError::Laged => Self::Laged,
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
            backups::BackupState::Unknown => BackupState::Unknown,
            backups::BackupState::Creating => BackupState::Creating,
            backups::BackupState::Enabling => BackupState::Enabling,
            backups::BackupState::Resuming => BackupState::Resuming,
            backups::BackupState::Enabled => BackupState::Enabled,
            backups::BackupState::Downloading => BackupState::Downloading,
            backups::BackupState::Disabling => BackupState::Disabling,
        }
    }
}

impl From<backups::UploadState> for BackupUploadState {
    fn from(value: backups::UploadState) -> Self {
        match value {
            backups::UploadState::Idle => Self::Waiting,
            backups::UploadState::CheckingIfUploadNeeded(count) => Self::CheckingIfUploadNeeded {
                backed_up_count: count.backed_up.try_into().unwrap_or(u32::MAX),
                total_count: count.total.try_into().unwrap_or(u32::MAX),
            },
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
    CreatingBackup,
    CreatingRecoveryKey,
    BackingUp { backed_up_count: u32, total_count: u32 },
    Done { recovery_key: String },
}

impl From<recovery::EnableProgress> for EnableRecoveryProgress {
    fn from(value: recovery::EnableProgress) -> Self {
        match &value {
            recovery::EnableProgress::CreatingBackup => Self::CreatingBackup,
            recovery::EnableProgress::CreatingRecoveryKey => Self::CreatingRecoveryKey,
            recovery::EnableProgress::BackingUp(counts) => Self::BackingUp {
                backed_up_count: counts.backed_up.try_into().unwrap_or(u32::MAX),
                total_count: counts.backed_up.try_into().unwrap_or(u32::MAX),
            },
            recovery::EnableProgress::Done { recovery_key } => {
                Self::Done { recovery_key: recovery_key.to_owned() }
            }
        }
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl Encryption {
    pub fn backup_state_listener(&self, listener: Box<dyn BackupStateListener>) -> Arc<TaskHandle> {
        let mut stream = self.inner.backups().state_stream();

        let stream_task = TaskHandle::new(RUNTIME.spawn(async move {
            while let Some(state) = stream.next().await {
                // TODO: Do we want to abort if theres's an error here?
                let Ok(state) = state else { continue };
                listener.on_update(state.into());
            }
        }));

        stream_task.into()
    }

    pub fn backup_state(&self) -> BackupState {
        self.inner.backups().state().into()
    }

    /// Does a backup exist on the server.
    ///
    /// Because the homeserver doesn't notify us about changes to the backup
    /// version the [`BackupState`] and its listener are a bit crippled.
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

    pub async fn enable_backups(&self) -> Result<(), ClientError> {
        Ok(self.inner.recovery().enable_backup().await?)
    }

    pub async fn is_last_device(&self) -> Result<bool, ClientError> {
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
                    // TODO: Do we want to abort if theres's an error here?
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
    ) -> Result<String, ClientError> {
        let recovery = self.inner.recovery();

        let enable = if wait_for_backups_to_upload {
            recovery.enable().wait_for_backups_to_upload()
        } else {
            recovery.enable()
        };

        let mut progress_stream = enable.subscribe_to_progress();

        let task = RUNTIME.spawn(async move {
            while let Some(progress) = progress_stream.next().await {
                // TODO: Do we want to abort if theres's an error here?
                let Ok(progress) = progress else { continue };
                progress_listener.on_update(progress.into());
            }
        });

        let ret = enable.await?;

        // TODO: Do we need to abort the task manually?
        task.abort();

        Ok(ret)
    }

    pub async fn disable_recovery(&self) -> Result<(), ClientError> {
        Ok(self.inner.recovery().disable().await?)
    }

    pub async fn reset_recovery_key(&self) -> Result<String, ClientError> {
        // TODO: This works even if we don't have all secrets on this device. Add
        // another method which resets the key but requires the old key?
        //
        // What does the user even do if they don't remember the old key 🫠
        Ok(self.inner.recovery().reset_key().await?)
    }

    pub async fn recover_and_reset(
        &self,
        mut old_recovery_key: String,
    ) -> Result<String, ClientError> {
        // TODO: Is this even useful? I'm not sure how it is.
        let result = self.inner.recovery().recover_and_reset(&old_recovery_key).await;

        old_recovery_key.zeroize();

        Ok(result?)
    }

    pub async fn fix_recovery_issues(&self, mut recovery_key: String) -> Result<(), ClientError> {
        let result = self.inner.recovery().recover(&recovery_key).await;

        recovery_key.zeroize();

        Ok(result?)
    }
}
