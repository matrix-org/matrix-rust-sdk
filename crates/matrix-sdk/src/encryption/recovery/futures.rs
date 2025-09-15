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

//! Named futures for the recovery support.

use std::future::IntoFuture;

use futures_core::Stream;
use futures_util::{StreamExt, pin_mut};
use matrix_sdk_common::boxed_into_future;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tracing::{Instrument, Span, warn};

use super::{EnableProgress, Recovery, RecoveryError, Result};
use crate::{
    encryption::{backups::UploadState, secret_storage::SecretStore},
    utils::ChannelObservable,
};

/// Named future for the [`Recovery::enable()`] method.
#[derive(Debug)]
pub struct Enable<'a> {
    pub(super) recovery: &'a Recovery,
    pub(super) progress: ChannelObservable<EnableProgress>,
    pub(super) wait_for_backups_upload: bool,
    pub(super) passphrase: Option<&'a str>,
    tracing_span: Span,
}

impl<'a> Enable<'a> {
    pub(super) fn new(recovery: &'a Recovery) -> Self {
        Self {
            recovery,
            progress: Default::default(),
            wait_for_backups_upload: false,
            passphrase: None,
            tracing_span: Span::current(),
        }
    }

    /// Subscribe to updates to the recovery enabling progress.
    pub fn subscribe_to_progress(
        &self,
    ) -> impl Stream<Item = Result<EnableProgress, BroadcastStreamRecvError>> + use<> {
        self.progress.subscribe()
    }

    /// Should the enabling of the recovery also wait for *all* room keys to be
    /// uploaded to the server-side key backup?
    ///
    /// This is useful if the user is enabling recovery and the room key backup
    /// just before logging out. Otherwise the logout might finish before
    /// all room keys have been backed up and thus historic messages will
    /// fail to decrypt once the user logs back in again.
    pub fn wait_for_backups_to_upload(mut self) -> Self {
        self.wait_for_backups_upload = true;

        self
    }

    /// In addition to the recovery key the [`Recovery::enable()`] method
    /// returns, allow this passphrase to be used for the
    /// [`Recovery::recover()`] method.
    pub fn with_passphrase(mut self, passphrase: &'a str) -> Self {
        self.passphrase = Some(passphrase);

        self
    }
}

impl<'a> IntoFuture for Enable<'a> {
    type Output = Result<String>;
    boxed_into_future!(extra_bounds: 'a);

    fn into_future(self) -> Self::IntoFuture {
        let Self { recovery, progress, wait_for_backups_upload, passphrase, tracing_span } = self;

        let future = async move {
            if !recovery.client.encryption().backups().are_enabled().await {
                if recovery.client.encryption().backups().fetch_exists_on_server().await? {
                    return Err(RecoveryError::BackupExistsOnServer);
                } else {
                    progress.set(EnableProgress::CreatingBackup);
                    recovery.mark_backup_as_enabled().await?;
                    recovery.client.encryption().backups().create().await?;
                }
            }

            progress.set(EnableProgress::CreatingRecoveryKey);

            let secret_storage = recovery.client.encryption().secret_storage();

            let create_store = if let Some(passphrase) = passphrase {
                secret_storage.create_secret_store().with_passphrase(passphrase)
            } else {
                secret_storage.create_secret_store()
            };

            let store: SecretStore = create_store.await?;

            if wait_for_backups_upload {
                let backups = recovery.client.encryption().backups();
                let upload_future = backups.wait_for_steady_state();
                let upload_progress = upload_future.subscribe_to_progress();

                #[allow(unused_variables)]
                let progress_task = matrix_sdk_common::executor::spawn({
                    let progress = progress.clone();
                    async move {
                        pin_mut!(upload_progress);

                        while let Some(update) = upload_progress.next().await {
                            match update {
                                Ok(UploadState::Uploading(count)) => {
                                    progress.set(EnableProgress::BackingUp(count));
                                }
                                Ok(UploadState::Done | UploadState::Error) | Err(_) => break,
                                _ => (),
                            }
                        }
                    }
                });

                if let Err(e) = upload_future.await {
                    warn!("Couldn't upload all the room keys to the backup: {e:?}");
                    progress.set(EnableProgress::RoomKeyUploadError);
                }

                #[cfg(not(target_family = "wasm"))]
                progress_task.abort();
            } else {
                recovery.client.encryption().backups().maybe_trigger_backup();
            }

            let key = store.secret_storage_key();

            progress.set(EnableProgress::Done { recovery_key: key });
            recovery.update_recovery_state().await?;

            Ok(store.secret_storage_key())
        };

        Box::pin(future.instrument(tracing_span))
    }
}

/// Named future for the [`Recovery::reset_key()`] method.
#[derive(Debug)]
pub struct Reset<'a> {
    pub(super) recovery: &'a Recovery,
    pub(super) passphrase: Option<&'a str>,
    tracing_span: Span,
}

impl<'a> Reset<'a> {
    pub(super) fn new(recovery: &'a Recovery) -> Self {
        Self { recovery, passphrase: None, tracing_span: Span::current() }
    }

    /// In addition to the recovery key the [`Recovery::reset_key()`] method
    /// returns, allow this passphrase to be used for the
    /// [`Recovery::recover()`] method.
    pub fn with_passphrase(mut self, passphrase: &'a str) -> Self {
        self.passphrase = Some(passphrase);

        self
    }
}

impl<'a> IntoFuture for Reset<'a> {
    type Output = Result<String>;
    boxed_into_future!(extra_bounds: 'a);

    fn into_future(self) -> Self::IntoFuture {
        let Self { recovery, passphrase, tracing_span } = self;

        let future = async move {
            let secret_storage = recovery.client.encryption().secret_storage();

            let create_store = if let Some(passphrase) = passphrase {
                secret_storage.create_secret_store().with_passphrase(passphrase)
            } else {
                secret_storage.create_secret_store()
            };

            let store: SecretStore = create_store.await?;
            recovery.update_recovery_state().await?;

            Ok(store.secret_storage_key())
        };

        Box::pin(future.instrument(tracing_span))
    }
}

/// Named future for the [`Recovery::recover_and_reset()`] method.
#[derive(Debug)]
pub struct RecoverAndReset<'a> {
    pub(super) recovery: &'a Recovery,
    pub(super) old_recovery_key: &'a str,
    pub(super) passphrase: Option<&'a str>,
    tracing_span: Span,
}

impl<'a> RecoverAndReset<'a> {
    pub(super) fn new(recovery: &'a Recovery, old_recovery_key: &'a str) -> Self {
        Self { recovery, old_recovery_key, passphrase: None, tracing_span: Span::current() }
    }

    /// In addition to the new recovery key the
    /// [`Recovery::recover_and_reset()`] method returns, allow this
    /// passphrase to be used for the [`Recovery::recover()`] method.
    pub fn with_passphrase(mut self, passphrase: &'a str) -> Self {
        self.passphrase = Some(passphrase);

        self
    }
}

impl<'a> IntoFuture for RecoverAndReset<'a> {
    type Output = Result<String>;
    boxed_into_future!(extra_bounds: 'a);

    fn into_future(self) -> Self::IntoFuture {
        let Self { recovery, old_recovery_key, passphrase, tracing_span } = self;

        let future = async move {
            recovery.recover(old_recovery_key).await?;

            let reset = if let Some(passphrase) = passphrase {
                recovery.reset_key().with_passphrase(passphrase)
            } else {
                recovery.reset_key()
            };

            reset.await
        };

        Box::pin(future.instrument(tracing_span))
    }
}
