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

//! Named futures for the backup support.

use std::{future::IntoFuture, time::Duration};

use futures_core::Stream;
use futures_util::StreamExt;
use matrix_sdk_common::boxed_into_future;
use thiserror::Error;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tracing::trace;

use super::{Backups, UploadState};
use crate::utils::ChannelObservable;

/// Error describing the ways that waiting for the backup upload to settle down
/// can fail.
#[derive(Clone, Copy, Debug, Error)]
pub enum SteadyStateError {
    /// The currently active backup got either deleted or a new one was created.
    ///
    /// No further room keys will be uploaded to the currently active
    /// backup.
    #[error("The backup got disabled while waiting for the room keys to be uploaded.")]
    BackupDisabled,
    /// Uploading the room keys to the homeserver failed due to a network error.
    ///
    /// Uploading will be retried again at a later point in time, or
    /// immediately if you wait for the steady state again.
    #[error("There was a network connection error.")]
    Connection,
    /// We missed some updates to the [`UploadState`] from the upload task.
    ///
    /// This error doesn't imply that there was an error with the uploading of
    /// room keys, it just means that we didn't receive all the transitions
    /// in the [`UploadState`]. You might want to retry waiting for the
    /// steady state.
    #[error("We couldn't read status updates from the upload task quickly enough.")]
    Lagged,
}

/// Named future for the [`Backups::wait_for_steady_state()`] method.
#[derive(Debug)]
pub struct WaitForSteadyState<'a> {
    pub(super) backups: &'a Backups,
    pub(super) progress: ChannelObservable<UploadState>,
    pub(super) timeout: Option<Duration>,
}

impl WaitForSteadyState<'_> {
    /// Subscribe to the progress of the backup upload step while waiting for it
    /// to settle down.
    pub fn subscribe_to_progress(
        &self,
    ) -> impl Stream<Item = Result<UploadState, BroadcastStreamRecvError>> + use<> {
        self.progress.subscribe()
    }

    /// Set the delay between each upload request.
    ///
    /// Uploading room keys might require multiple requests to be sent out. The
    /// [`Client`] waits for a while before it sends the next request out.
    ///
    /// This method allows you to override how long the [`Client`] will wait.
    /// The default value is 100 ms.
    ///
    /// [`Client`]: crate::Client
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.timeout = Some(delay);

        self
    }
}

impl<'a> IntoFuture for WaitForSteadyState<'a> {
    type Output = Result<(), SteadyStateError>;
    boxed_into_future!(extra_bounds: 'a);

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let Self { backups, timeout, progress } = self;

            trace!("Creating a stream to wait for the steady state");

            let mut progress_stream = progress.subscribe();

            let old_delay = if let Some(delay) = timeout {
                let mut lock = backups.client.inner.e2ee.backup_state.upload_delay.write().unwrap();
                let old_delay = Some(lock.to_owned());

                *lock = delay;

                old_delay
            } else {
                None
            };

            trace!("Waiting for the upload steady state");

            let ret = if backups.are_enabled().await {
                backups.maybe_trigger_backup();

                let mut ret = Ok(());

                // TODO: Do we want to be smart here and remember the count when we started
                // waiting and prevent the total from increasing, in case new room
                // keys arrive after we started waiting.
                while let Some(state) = progress_stream.next().await {
                    trace!(?state, "Update state while waiting for the backup steady state");

                    match state {
                        Ok(UploadState::Done) => {
                            ret = Ok(());
                            break;
                        }
                        Ok(UploadState::Error) => {
                            if backups.are_enabled().await {
                                ret = Err(SteadyStateError::Connection);
                            } else {
                                ret = Err(SteadyStateError::BackupDisabled);
                            }

                            break;
                        }
                        Err(_) => {
                            ret = Err(SteadyStateError::Lagged);
                            break;
                        }
                        _ => (),
                    }
                }

                ret
            } else {
                Err(SteadyStateError::BackupDisabled)
            };

            if let Some(old_delay) = old_delay {
                let mut lock = backups.client.inner.e2ee.backup_state.upload_delay.write().unwrap();
                *lock = old_delay;
            }

            ret
        })
    }
}
