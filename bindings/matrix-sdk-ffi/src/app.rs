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

use futures_util::{pin_mut, StreamExt as _};
use matrix_sdk::Client;
use matrix_sdk_ui::app::{
    App as MatrixApp, AppBuilder as MatrixAppBuilder, AppState as MatrixAppState,
};

use crate::{
    error::ClientError, helpers::unwrap_or_clone_arc, room_list::RoomListService,
    task_handle::TaskHandle, RUNTIME,
};

#[derive(uniffi::Enum)]
pub enum AppState {
    Running,
    Terminated,
    Error,
}

impl From<MatrixAppState> for AppState {
    fn from(value: MatrixAppState) -> Self {
        match value {
            MatrixAppState::Running => Self::Running,
            MatrixAppState::Terminated => Self::Terminated,
            MatrixAppState::Error => Self::Error,
        }
    }
}

#[uniffi::export(callback_interface)]
pub trait AppStateObserver: Send + Sync + Debug {
    fn on_update(&self, state: AppState);
}

#[derive(uniffi::Object)]
pub struct App {
    inner: MatrixApp,
}

#[uniffi::export]
impl App {
    pub fn room_list_service(&self) -> Arc<RoomListService> {
        Arc::new(RoomListService { inner: self.inner.room_list_service() })
    }

    fn state(&self, listener: Box<dyn AppStateObserver>) -> Arc<TaskHandle> {
        let state_stream = self.inner.observe_state();

        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            pin_mut!(state_stream);

            while let Some(state) = state_stream.next().await {
                listener.on_update(state.into());
            }
        })))
    }

    pub fn start(&self) -> Result<(), ClientError> {
        let start = self.inner.start();
        RUNTIME.block_on(async { Ok(start.await?) })
    }

    pub fn pause(&self) -> Result<(), ClientError> {
        Ok(self.inner.pause()?)
    }
}

#[derive(Clone, uniffi::Object)]
pub struct AppBuilder {
    builder: MatrixAppBuilder,
}

impl AppBuilder {
    pub(crate) fn new(client: Client) -> Arc<Self> {
        Arc::new(Self { builder: MatrixApp::builder(client) })
    }
}

#[uniffi::export]
impl AppBuilder {
    pub fn with_encryption_sync(
        self: Arc<Self>,
        with_cross_process_lock: bool,
        app_identifier: Option<String>,
    ) -> Arc<Self> {
        let this = unwrap_or_clone_arc(self);
        let builder = this.builder.with_encryption_sync(with_cross_process_lock, app_identifier);
        Arc::new(Self { builder })
    }

    pub fn finish(self: Arc<Self>) -> Result<Arc<App>, ClientError> {
        let this = unwrap_or_clone_arc(self);
        RUNTIME.block_on(async move { Ok(Arc::new(App { inner: this.builder.build().await? })) })
    }
}
