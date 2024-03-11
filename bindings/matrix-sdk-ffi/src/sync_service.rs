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
use matrix_sdk_ui::sync_service::{
    State as MatrixSyncServiceState, SyncService as MatrixSyncService,
    SyncServiceBuilder as MatrixSyncServiceBuilder,
};

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

#[uniffi::export(callback_interface)]
pub trait SyncServiceStateObserver: Send + Sync + Debug {
    fn on_update(&self, state: SyncServiceState);
}

#[derive(uniffi::Object)]
pub struct SyncService {
    pub(crate) inner: Arc<MatrixSyncService>,
}

#[uniffi::export(async_runtime = "tokio")]
impl SyncService {
    pub fn room_list_service(&self) -> Arc<RoomListService> {
        Arc::new(RoomListService { inner: self.inner.room_list_service() })
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
    builder: MatrixSyncServiceBuilder,
}

impl SyncServiceBuilder {
    pub(crate) fn new(client: Client) -> Arc<Self> {
        Arc::new(Self { builder: MatrixSyncService::builder(client) })
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl SyncServiceBuilder {
    pub fn with_unified_invites_in_room_list(
        self: Arc<Self>,
        with_unified_invites: bool,
    ) -> Arc<Self> {
        let this = unwrap_or_clone_arc(self);
        let builder = this.builder.with_unified_invites_in_room_list(with_unified_invites);
        Arc::new(Self { builder })
    }

    pub fn with_cross_process_lock(self: Arc<Self>, app_identifier: Option<String>) -> Arc<Self> {
        let this = unwrap_or_clone_arc(self);
        let builder = this.builder.with_cross_process_lock(app_identifier);
        Arc::new(Self { builder })
    }

    pub async fn finish(self: Arc<Self>) -> Result<Arc<SyncService>, ClientError> {
        let this = unwrap_or_clone_arc(self);
        Ok(Arc::new(SyncService { inner: Arc::new(this.builder.build().await?) }))
    }
}
