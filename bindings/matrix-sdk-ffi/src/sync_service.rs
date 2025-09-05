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

use std::{fmt::Debug, sync::Arc};

use futures_util::pin_mut;
use matrix_sdk::Client;
use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm};
use matrix_sdk_ui::{
    sync_service::{
        State as MatrixSyncServiceState, SyncService as MatrixSyncService,
        SyncServiceBuilder as MatrixSyncServiceBuilder,
    },
    unable_to_decrypt_hook::UtdHookManager,
};

use crate::{
    error::ClientError, helpers::unwrap_or_clone_arc, room_list::RoomListService,
    runtime::get_runtime_handle, TaskHandle,
};

#[derive(uniffi::Enum)]
pub enum SyncServiceState {
    Idle,
    Running,
    Terminated,
    Error,
    Offline,
}

impl From<MatrixSyncServiceState> for SyncServiceState {
    fn from(value: MatrixSyncServiceState) -> Self {
        match value {
            MatrixSyncServiceState::Idle => Self::Idle,
            MatrixSyncServiceState::Running => Self::Running,
            MatrixSyncServiceState::Terminated => Self::Terminated,
            MatrixSyncServiceState::Error(_error) => Self::Error,
            MatrixSyncServiceState::Offline => Self::Offline,
        }
    }
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait SyncServiceStateObserver: SendOutsideWasm + SyncOutsideWasm + Debug {
    fn on_update(&self, state: SyncServiceState);
}

#[derive(uniffi::Object)]
pub struct SyncService {
    pub(crate) inner: Arc<MatrixSyncService>,
    utd_hook: Option<Arc<UtdHookManager>>,
}

#[matrix_sdk_ffi_macros::export]
impl SyncService {
    pub fn room_list_service(&self) -> Arc<RoomListService> {
        Arc::new(RoomListService {
            inner: self.inner.room_list_service(),
            utd_hook: self.utd_hook.clone(),
        })
    }

    pub async fn start(&self) {
        self.inner.start().await
    }

    pub async fn stop(&self) {
        self.inner.stop().await
    }

    pub fn state(&self, listener: Box<dyn SyncServiceStateObserver>) -> Arc<TaskHandle> {
        let state_stream = self.inner.state();

        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            pin_mut!(state_stream);

            while let Some(state) = state_stream.next().await {
                listener.on_update(state.into());
            }
        })))
    }

    /// Force expiring both sliding sync sessions.
    ///
    /// This ensures that the sync service is stopped before expiring both
    /// sessions. It should be used sparingly, as it will cause a restart of
    /// the sessions on the server as well.
    pub async fn expire_sessions(&self) {
        self.inner.expire_sessions().await;
    }
}

#[derive(Clone, uniffi::Object)]
pub struct SyncServiceBuilder {
    builder: MatrixSyncServiceBuilder,
    utd_hook: Option<Arc<UtdHookManager>>,
}

impl SyncServiceBuilder {
    pub(crate) fn new(client: Client, utd_hook: Option<Arc<UtdHookManager>>) -> Arc<Self> {
        Arc::new(Self { builder: MatrixSyncService::builder(client), utd_hook })
    }
}

#[matrix_sdk_ffi_macros::export]
impl SyncServiceBuilder {
    pub fn with_cross_process_lock(self: Arc<Self>) -> Arc<Self> {
        let this = unwrap_or_clone_arc(self);
        let builder = this.builder.with_cross_process_lock();
        Arc::new(Self { builder, ..this })
    }

    /// Enable the "offline" mode for the [`SyncService`].
    pub fn with_offline_mode(self: Arc<Self>) -> Arc<Self> {
        let this = unwrap_or_clone_arc(self);
        let builder = this.builder.with_offline_mode();
        Arc::new(Self { builder, ..this })
    }

    pub fn with_share_pos(self: Arc<Self>, enable: bool) -> Arc<Self> {
        let this = unwrap_or_clone_arc(self);
        let builder = this.builder.with_share_pos(enable);
        Arc::new(Self { builder, ..this })
    }

    pub async fn finish(self: Arc<Self>) -> Result<Arc<SyncService>, ClientError> {
        let this = unwrap_or_clone_arc(self);
        Ok(Arc::new(SyncService {
            inner: Arc::new(this.builder.build().await?),
            utd_hook: this.utd_hook,
        }))
    }
}
