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

//! Unified API for both the Room List API and the Encryption Sync API, that
//! takes care of all the underlying details. This is an opiniated way to run
//! both APIs, with high-level callbacks that should be called in reaction to
//! user actions and/or system events.

use std::sync::{Arc, Mutex};

use eyeball::SharedObservable;
use futures_core::Stream;
use futures_util::{pin_mut, StreamExt as _};
use matrix_sdk::Client;
use thiserror::Error;
use tokio::task::{spawn, JoinHandle};

use crate::{
    encryption_sync::{self, EncryptionSync, WithLocking},
    room_list_service::{self, RoomListService},
};

/// Current state of the application.
///
/// This is a high-level state indicating what's the status of the underlying
/// syncs. The application starts in `Running` mode, and then hits a terminal
/// state `Terminated` (if it gracefully exited) or `Error` (in case any of the
/// underlying syncs ran into an error).
///
/// It is the responsibility of the caller to restart the application using the
/// [`App::start`] method, in case it terminated, gracefully or not.
///
/// This can be observed with [`App::observe_state`].
#[derive(Clone)]
pub enum AppState {
    /// The underlying syncs are properly running in the background.
    Running,
    /// Any of the underlying syncs has terminated gracefully (i.e. be stopped).
    Terminated,
    /// Any of the underlying syncs has ran into an error.
    Error,
}

pub struct App {
    room_list: Arc<RoomListService>,
    encryption_sync: Option<Arc<EncryptionSync>>,
    task_handle: Arc<Mutex<Option<JoinHandle<()>>>>,
    state_observer: SharedObservable<AppState>,
}

impl App {
    /// Create a new builder for configuring an `App`.
    pub fn builder(client: Client) -> AppBuilder {
        AppBuilder::new(client)
    }

    /// Get the underlying `RoomListService` instance for easier access to its
    /// methods.
    pub fn room_list_service(&self) -> &RoomListService {
        &*self.room_list
    }

    /// Observe the current state of the application.
    ///
    /// See also [`AppState`].
    pub fn observe_state(&self) -> impl Stream<Item = AppState> {
        self.state_observer.subscribe()
    }

    /// Start (or restart) the underlying sliding syncs.
    ///
    /// This can be called multiple times safely: if a previous task had been
    /// spawned to run the syncs, then it will be properly aborted and
    /// restarted.
    pub async fn start(&self) -> Result<(), Error> {
        let room_list = self.room_list.clone();
        let encryption_sync = self.encryption_sync.clone();
        let state_observer = self.state_observer.clone();

        let mut task_handle_lock = self.task_handle.lock().unwrap();

        // If there was a task running already with the streams, stop it gently. In the
        // case it was already terminated, that's fine as it won't cause any
        // harm to abort it.
        if let Some(task) = task_handle_lock.take() {
            task.abort();
            drop(task);
        }

        *task_handle_lock = Some(spawn(async move {
            let room_list_stream = room_list.sync();

            pin_mut!(room_list_stream);

            if let Some(encryption_sync) = encryption_sync {
                let encryption_sync_stream = encryption_sync.sync();

                pin_mut!(encryption_sync_stream);

                // Note: any error on one of the two syncs will cause the overall stream to
                // error and the loop to terminate.

                loop {
                    tokio::select! {
                        encryption_sync_result = encryption_sync_stream.next() => {
                            if let Some(encryption_sync_result) = encryption_sync_result {
                                if let Err(err) = encryption_sync_result {
                                    tracing::error!("Encryption sync returned an error: {err:#}");
                                    state_observer.set(AppState::Error);
                                    break;
                                }
                            } else {
                                state_observer.set(AppState::Terminated);
                                break;
                            }
                        }

                        room_list_result = room_list_stream.next() => {
                            if let Some(room_list_result) = room_list_result {
                                if let Err(err) = room_list_result {
                                    tracing::error!("Room list sync returned an error: {err:#}");
                                    state_observer.set(AppState::Error);
                                    break;
                                }
                            } else {
                                state_observer.set(AppState::Terminated);
                                break;
                            }
                        }
                    }
                }
            } else {
                while let Some(res) = room_list_stream.next().await {
                    if let Err(err) = res {
                        tracing::error!("Error while processing app (room list) state: {err:#}");
                        state_observer.set(AppState::Error);
                        break;
                    }
                }

                state_observer.set(AppState::Terminated);
            }
        }));

        Ok(())
    }

    /// Stop the underlying sliding syncs.
    ///
    /// This must be called when the app goes into the background. It's better
    /// to call this API when the application exits, although not strictly
    /// necessary.
    pub fn pause(&self) -> Result<(), Error> {
        self.room_list.stop_sync()?;
        if let Some(ref encryption_sync) = self.encryption_sync {
            encryption_sync.stop()?;
        }
        Ok(())
    }
}

pub struct AppBuilder {
    /// SDK client.
    client: Client,

    /// Is the encryption sync running as a separate instance of sliding sync
    /// (true), or is it fused in the main `RoomList` sliding sync (false)?
    with_encryption_sync: bool,

    /// Is the cross-process lock for the crypto store enabled?
    with_cross_process_lock: bool,

    /// Application identifier, used the cross-process lock value, if
    /// applicable.
    identifier: String,
}

impl AppBuilder {
    fn new(client: Client) -> Self {
        Self {
            client,
            with_cross_process_lock: false,
            with_encryption_sync: false,
            identifier: "app".to_owned(),
        }
    }

    /// Enables the encryption sync for this application.
    ///
    /// This will run a second sliding sync instance, that can independently
    /// process encryption events, which can speed up some use cases.
    ///
    /// It's also a prerequisite if another process can *also* process
    /// encryption events; in that case, the `with_cross_process_lock`
    /// boolean must be set to `true` to enable the cross-process crypto
    /// store lock. This is only applicable to very specific use cases, like
    /// an external process attempting to decrypt notifications. In general,
    /// `with_cross_process_lock` can remain `false`.
    ///
    /// If the cross-process lock is enabled, then an app identifier can be
    /// provided too, to identify the current process; if it's not provided,
    /// a default value of "app" is used as the application identifier.
    pub fn with_encryption_sync(
        mut self,
        with_cross_process_lock: bool,
        app_identifier: Option<String>,
    ) -> Self {
        self.with_encryption_sync = true;
        self.with_cross_process_lock = with_cross_process_lock;
        if let Some(app_identifier) = app_identifier {
            self.identifier = app_identifier;
        }
        self
    }

    /// Finish setting up the `App`.
    ///
    /// This creates the underlying sliding syncs, and will start them in the
    /// background. The resulting `App` must be kept alive as long as the
    /// sliding syncs are supposed to run.
    pub async fn build(self) -> Result<App, Error> {
        let (room_list, encryption_sync) = if self.with_encryption_sync {
            let room_list = RoomListService::new(self.client.clone()).await?;
            let with_lock =
                if self.with_cross_process_lock { WithLocking::Yes } else { WithLocking::No };
            let encryption_sync =
                EncryptionSync::new(self.identifier, self.client, None, with_lock).await?;
            (room_list, Some(Arc::new(encryption_sync)))
        } else {
            let room_list = RoomListService::new_with_encryption(self.client.clone()).await?;
            (room_list, None)
        };

        let app = App {
            room_list: Arc::new(room_list),
            encryption_sync,
            state_observer: SharedObservable::new(AppState::Running),
            task_handle: Default::default(),
        };

        app.start().await?;

        Ok(app)
    }
}

/// Errors for the `App` API.
#[derive(Debug, Error)]
pub enum Error {
    /// An error received from the `RoomList` API.
    #[error(transparent)]
    RoomList(#[from] room_list_service::Error),

    /// An error received from the `EncryptionSync` API.
    #[error(transparent)]
    EncryptionSync(#[from] encryption_sync::Error),
}
