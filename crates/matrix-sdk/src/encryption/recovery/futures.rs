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

use std::{future::IntoFuture, pin::Pin};

use futures_core::{Future, Stream};
use futures_util::{pin_mut, StreamExt};
use matrix_sdk_base::crypto::store::RoomKeyCounts;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use zeroize::{Zeroize, ZeroizeOnDrop};

use super::Recovery;
use crate::encryption::{
    backups::{ChannelObservable, UploadState},
    secret_storage::{Result, SecretStore},
};

#[derive(Debug, Clone, Zeroize, ZeroizeOnDrop)]
pub enum EnableProgress {
    CreatingBackup,
    CreatingRecoveryKey,
    #[zeroize(skip)]
    BackingUp(RoomKeyCounts),
    Done {
        recovery_key: String,
    },
}

impl Default for EnableProgress {
    fn default() -> Self {
        Self::CreatingBackup
    }
}

#[derive(Debug)]
pub struct Enable<'a> {
    pub(super) recovery: &'a Recovery,
    pub(super) progress: ChannelObservable<EnableProgress>,
    pub(super) wait_for_backups_upload: bool,
    pub(super) passphrase: Option<&'a str>,
}

impl<'a> Enable<'a> {
    pub(super) fn new(recovery: &'a Recovery) -> Self {
        Self {
            recovery,
            progress: Default::default(),
            wait_for_backups_upload: false,
            passphrase: None,
        }
    }

    pub fn subscribe_to_progress(
        &self,
    ) -> impl Stream<Item = Result<EnableProgress, BroadcastStreamRecvError>> {
        self.progress.subscribe()
    }

    pub fn wait_for_backups_to_upload(mut self) -> Self {
        self.wait_for_backups_upload = true;

        self
    }

    pub fn with_passphrase(mut self, passphrase: &'a str) -> Self {
        self.passphrase = Some(passphrase);

        self
    }
}

impl<'a> IntoFuture for Enable<'a> {
    type Output = Result<String>;
    #[cfg(target_arch = "wasm32")]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + 'a>>;
    #[cfg(not(target_arch = "wasm32"))]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let Self { recovery, progress, wait_for_backups_upload, passphrase } = self;

            if !recovery.client.encryption().backups().are_enabled().await {
                progress.set(EnableProgress::CreatingBackup);
                recovery.mark_backup_as_enabled().await?;
                recovery.client.encryption().backups().create().await?;
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

                let progress_task = matrix_sdk_common::executor::spawn({
                    let progress = progress.clone();
                    async move {
                        pin_mut!(upload_progress);

                        while let Some(update) = upload_progress.next().await {
                            match update {
                                Ok(UploadState::Uploading(count)) => {
                                    progress.set(EnableProgress::BackingUp(count));
                                }
                                Ok(UploadState::Done) => break,
                                Ok(UploadState::Error) => break,
                                Err(_) => break,
                                _ => (),
                            }
                        }
                    }
                });

                if let Err(_) = upload_future.await {
                    // TODO: What do we want to do here?
                    todo!()
                }

                progress_task.abort();
            } else {
                recovery.client.encryption().backups().maybe_trigger_backup();
            }

            let key = store.secret_storage_key();

            progress.set(EnableProgress::Done { recovery_key: key });
            recovery.update_recovery_state().await?;

            Ok(store.secret_storage_key())
        })
    }
}

#[derive(Debug)]
pub struct Reset<'a> {
    pub(super) recovery: &'a Recovery,
    pub(super) passphrase: Option<&'a str>,
}

impl<'a> Reset<'a> {
    pub(super) fn new(recovery: &'a Recovery) -> Self {
        Self { recovery, passphrase: None }
    }

    pub fn with_passphrase(mut self, passphrase: &'a str) -> Self {
        self.passphrase = Some(passphrase);

        self
    }
}

impl<'a> IntoFuture for Reset<'a> {
    type Output = Result<String>;
    #[cfg(target_arch = "wasm32")]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + 'a>>;
    #[cfg(not(target_arch = "wasm32"))]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let Self { recovery, passphrase } = self;

            let secret_storage = recovery.client.encryption().secret_storage();

            let create_store = if let Some(passphrase) = passphrase {
                secret_storage.create_secret_store().with_passphrase(passphrase)
            } else {
                secret_storage.create_secret_store()
            };

            let store: SecretStore = create_store.await?;
            recovery.update_recovery_state().await?;

            Ok(store.secret_storage_key())
        })
    }
}

#[derive(Debug)]
pub struct RecoverAndReset<'a> {
    pub(super) recovery: &'a Recovery,
    pub(super) old_recovery_key: &'a str,
    pub(super) passphrase: Option<&'a str>,
}

impl<'a> RecoverAndReset<'a> {
    pub(super) fn new(recovery: &'a Recovery, old_recovery_key: &'a str) -> Self {
        Self { recovery, old_recovery_key, passphrase: None }
    }

    pub fn with_passphrase(mut self, passphrase: &'a str) -> Self {
        self.passphrase = Some(passphrase);

        self
    }
}

impl<'a> IntoFuture for RecoverAndReset<'a> {
    type Output = Result<String>;
    #[cfg(target_arch = "wasm32")]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + 'a>>;
    #[cfg(not(target_arch = "wasm32"))]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send + 'a>>;

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let Self { recovery, old_recovery_key, passphrase } = self;

            recovery.recover(old_recovery_key).await?;

            let reset = if let Some(passphrase) = passphrase {
                recovery.reset_key().with_passphrase(passphrase)
            } else {
                recovery.reset_key()
            };

            reset.await
        })
    }
}
