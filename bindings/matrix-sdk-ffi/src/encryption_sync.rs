use std::sync::Arc;

use futures_util::{pin_mut, StreamExt as _};
use matrix_sdk_ui::encryption_sync::{EncryptionSync as MatrixEncryptionSync, EncryptionSyncMode};
use tracing::{error, warn};

use crate::{client::Client, error::ClientError, task_handle::TaskHandle, RUNTIME};

#[derive(uniffi::Enum)]
pub enum EncryptionSyncTerminationReason {
    Done,
    Error { msg: String },
}

#[uniffi::export(callback_interface)]
pub trait EncryptionSyncListener: Sync + Send {
    /// Called whenever the notification sync loop terminates, and must be
    /// restarted.
    fn did_terminate(&self, reason: EncryptionSyncTerminationReason);
}

/// Full context for the notification sync loop.
#[derive(uniffi::Object)]
pub struct EncryptionSync {
    /// Unused field, maintains the sliding sync loop alive.
    _handle: TaskHandle,

    sync: Arc<MatrixEncryptionSync>,
}

impl EncryptionSync {
    fn start(
        encryption_sync: Arc<MatrixEncryptionSync>,
        listener: Box<dyn EncryptionSyncListener>,
    ) -> TaskHandle {
        TaskHandle::new(RUNTIME.spawn(async move {
            let stream = encryption_sync.sync();
            pin_mut!(stream);

            let error = loop {
                let streamed = stream.next().await;
                match streamed {
                    Some(Ok(())) => {
                        // Yay.
                    }

                    None => {
                        break None;
                    }

                    Some(Err(err)) => {
                        // The internal sliding sync instance already handles retries for us, so if
                        // we get an error here, it means the maximum number of retries has been
                        // reached, and there's not much we can do anymore.
                        warn!("Error in encryption sync: {err:#}");
                        break Some(err);
                    }
                }
            };

            listener.did_terminate(match error {
                Some(err) => EncryptionSyncTerminationReason::Error { msg: err.to_string() },
                None => EncryptionSyncTerminationReason::Done,
            });
        }))
    }
}

#[uniffi::export]
impl EncryptionSync {
    pub fn stop(&self) {
        if let Err(err) = self.sync.stop() {
            error!("Error when stopping the encryption sync: {err}");
        }
    }
}

impl Client {
    fn encryption_sync(
        &self,
        id: String,
        listener: Box<dyn EncryptionSyncListener>,
        mode: EncryptionSyncMode,
    ) -> Result<Arc<EncryptionSync>, ClientError> {
        RUNTIME.block_on(async move {
            let inner = Arc::new(MatrixEncryptionSync::new(id, self.inner.clone(), mode).await?);

            let handle = EncryptionSync::start(inner.clone(), listener);

            Ok(Arc::new(EncryptionSync { _handle: handle, sync: inner }))
        })
    }
}

#[uniffi::export]
impl Client {
    /// Must be called to get the encryption loop running.
    ///
    /// `id` must be a unique identifier, less than 16 chars long, for the
    /// current process. It must not change over time, as it's used as a key
    /// for caching.
    ///
    /// This should be avoided, whenever possible, and be used only in
    /// situations where the encryption sync loop needs to run from multiple
    /// processes at the same time (on iOS for instance, where notifications
    /// are handled in a separate process). If you aren't in such a
    /// situation, prefer using `Client::room_list(true)`.
    pub fn main_encryption_sync(
        &self,
        id: String,
        listener: Box<dyn EncryptionSyncListener>,
    ) -> Result<Arc<EncryptionSync>, ClientError> {
        self.encryption_sync(id, listener, EncryptionSyncMode::NeverStop)
    }

    /// Encryption loop for a notification process.
    ///
    /// A fixed number of iterations can be given, to limit the time spent in
    /// that loop.
    ///
    /// This should be avoided, whenever possible, and be used only in
    /// situations where the encryption sync loop needs to run from multiple
    /// processes at the same time (on iOS for instance, where notifications
    /// are handled in a separate process). If you aren't in such a
    /// situation, prefer using `Client::room_list(true)`.
    pub fn notification_encryption_sync(
        &self,
        id: String,
        listener: Box<dyn EncryptionSyncListener>,
        num_iters: u8,
    ) -> Result<Arc<EncryptionSync>, ClientError> {
        self.encryption_sync(id, listener, EncryptionSyncMode::RunFixedIterations(num_iters))
    }
}
