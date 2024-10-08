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
// See the License for that specific language governing permissions and
// limitations under the License.

use std::{fmt::Debug, sync::Arc, time::Duration};

use futures_util::pin_mut;
use matrix_sdk::{crypto::types::events::UtdCause, Client};
use matrix_sdk_ui::{
    sync_service::{
        State as MatrixSyncServiceState, SyncService as MatrixSyncService,
        SyncServiceBuilder as MatrixSyncServiceBuilder,
    },
    unable_to_decrypt_hook::{
        UnableToDecryptHook, UnableToDecryptInfo as SdkUnableToDecryptInfo, UtdHookManager,
    },
};
use tracing::error;

use crate::{
    error::ClientError, helpers::unwrap_or_clone_arc, room_list::RoomListService, TaskHandle,
    RUNTIME,
};

#[derive(uniffi::Enum)]
pub enum SyncServiceState {
    Idle,
    Running,
    Terminated,
    Error,
}

impl From<MatrixSyncServiceState> for SyncServiceState {
    fn from(value: MatrixSyncServiceState) -> Self {
        match value {
            MatrixSyncServiceState::Idle => Self::Idle,
            MatrixSyncServiceState::Running => Self::Running,
            MatrixSyncServiceState::Terminated => Self::Terminated,
            MatrixSyncServiceState::Error => Self::Error,
        }
    }
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait SyncServiceStateObserver: Send + Sync + Debug {
    fn on_update(&self, state: SyncServiceState);
}

#[derive(uniffi::Object)]
pub struct SyncService {
    pub(crate) inner: Arc<MatrixSyncService>,
    utd_hook: Option<Arc<UtdHookManager>>,
}

#[matrix_sdk_ffi_macros::export_async]
impl SyncService {
    pub fn room_list_service(&self) -> Arc<RoomListService> {
        Arc::new(RoomListService {
            inner: self.inner.room_list_service(),
            utd_hook: self.utd_hook.clone(),
        })
    }

    pub async fn start(&self) {
        self.inner.start().await;
    }

    pub async fn stop(&self) -> Result<(), ClientError> {
        Ok(self.inner.stop().await?)
    }

    pub fn state(&self, listener: Box<dyn SyncServiceStateObserver>) -> Arc<TaskHandle> {
        let state_stream = self.inner.state();

        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            pin_mut!(state_stream);

            while let Some(state) = state_stream.next().await {
                listener.on_update(state.into());
            }
        })))
    }
}

#[derive(Clone, uniffi::Object)]
pub struct SyncServiceBuilder {
    client: Client,
    builder: MatrixSyncServiceBuilder,

    utd_hook: Option<Arc<UtdHookManager>>,
}

impl SyncServiceBuilder {
    pub(crate) fn new(client: Client) -> Arc<Self> {
        Arc::new(Self {
            client: client.clone(),
            builder: MatrixSyncService::builder(client),
            utd_hook: None,
        })
    }
}

#[matrix_sdk_ffi_macros::export_async]
impl SyncServiceBuilder {
    pub fn with_cross_process_lock(self: Arc<Self>, app_identifier: Option<String>) -> Arc<Self> {
        let this = unwrap_or_clone_arc(self);
        let builder = this.builder.with_cross_process_lock(app_identifier);
        Arc::new(Self { client: this.client, builder, utd_hook: this.utd_hook })
    }

    pub async fn with_utd_hook(
        self: Arc<Self>,
        delegate: Box<dyn UnableToDecryptDelegate>,
    ) -> Arc<Self> {
        // UTDs detected before this duration may be reclassified as "late decryption"
        // events (or discarded, if they get decrypted fast enough).
        const UTD_HOOK_GRACE_PERIOD: Duration = Duration::from_secs(60);

        let this = unwrap_or_clone_arc(self);

        let mut utd_hook = UtdHookManager::new(Arc::new(UtdHook { delegate }), this.client.clone())
            .with_max_delay(UTD_HOOK_GRACE_PERIOD);

        if let Err(e) = utd_hook.reload_from_store().await {
            error!("Unable to reload UTD hook data from data store: {}", e);
            // Carry on with the setup anyway; we shouldn't fail setup just
            // because the UTD hook failed to load its data.
        }

        Arc::new(Self {
            client: this.client,
            builder: this.builder,
            utd_hook: Some(Arc::new(utd_hook)),
        })
    }

    pub async fn finish(self: Arc<Self>) -> Result<Arc<SyncService>, ClientError> {
        let this = unwrap_or_clone_arc(self);
        Ok(Arc::new(SyncService {
            inner: Arc::new(this.builder.build().await?),
            utd_hook: this.utd_hook,
        }))
    }
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait UnableToDecryptDelegate: Sync + Send {
    fn on_utd(&self, info: UnableToDecryptInfo);
}

struct UtdHook {
    delegate: Box<dyn UnableToDecryptDelegate>,
}

impl std::fmt::Debug for UtdHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UtdHook").finish_non_exhaustive()
    }
}

impl UnableToDecryptHook for UtdHook {
    fn on_utd(&self, info: SdkUnableToDecryptInfo) {
        const IGNORE_UTD_PERIOD: Duration = Duration::from_secs(4);

        // UTDs that have been decrypted in the `IGNORE_UTD_PERIOD` are just ignored and
        // not considered UTDs.
        if let Some(duration) = &info.time_to_decrypt {
            if *duration < IGNORE_UTD_PERIOD {
                return;
            }
        }

        // Report the UTD to the client.
        self.delegate.on_utd(info.into());
    }
}

#[derive(uniffi::Record)]
pub struct UnableToDecryptInfo {
    /// The identifier of the event that couldn't get decrypted.
    event_id: String,

    /// If the event could be decrypted late (that is, the event was encrypted
    /// at first, but could be decrypted later on), then this indicates the
    /// time it took to decrypt the event. If it is not set, this is
    /// considered a definite UTD.
    ///
    /// If set, this is in milliseconds.
    pub time_to_decrypt_ms: Option<u64>,

    /// What we know about what caused this UTD. E.g. was this event sent when
    /// we were not a member of this room?
    pub cause: UtdCause,
}

impl From<SdkUnableToDecryptInfo> for UnableToDecryptInfo {
    fn from(value: SdkUnableToDecryptInfo) -> Self {
        Self {
            event_id: value.event_id.to_string(),
            time_to_decrypt_ms: value.time_to_decrypt.map(|ttd| ttd.as_millis() as u64),
            cause: value.cause,
        }
    }
}
