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
//! takes care of all the underlying details.
//!
//! This is an opiniated way to run both APIs, with high-level callbacks that
//! should be called in reaction to user actions and/or system events.
//!
//! The room list sync will signal errors via its
//! [`state`](RoomListService::state) that the user
//! MUST observe. Whenever an error/termination is observed, the user MUST call
//! [`SyncService::start()`] again to restart the room list sync.
//!
//! The encryption sync is handled separately, and it is the responsibility of
//! the `SyncService` to manage its errors. Hence, no specific actions are
//! required from the user if the encryption sync runs into errors, as it is
//! automatically restarted (unless the user explicitly asked to
//! [`SyncService::pause()`]).
//!
//! This service can be in one of three states:
//!
//! - idle: neither the encryption sync nor the room list sync are
//! running. Nothing is happening until the user asks to do something. That's
//! the initial state. Calling [`SyncService::start()`] will lead to the next
//! state. Calling [`SyncService::pause()`] is a no-op.
//! - both syncs running: both the encryption sync and the room list sync are
//!   running at the same
//! time. Calling
//! [`SyncService::start()`] is a no-op. Calling [`SyncService::pause()`] will
//! get back to the first state. If the room list sync
//! runs into an error, it stops, and the sync service runs into the third
//! state:
//! - only the encryption sync is running: in that state, the room list sync
//!   isn't running. Calling
//! [`SyncService::start()`] will lead to the second state, while
//! [`SyncService::pause()`] will lead to the first state.

use std::sync::{Arc, Mutex};

use futures_util::{pin_mut, StreamExt as _};
use matrix_sdk::Client;
use thiserror::Error;
use tokio::task::{spawn, JoinHandle};
use tracing::{debug, error, trace, warn};

use crate::{
    encryption_sync::{self, EncryptionSync, WithLocking},
    room_list_service::{self, RoomListService},
};

// Trick: use an internal module for the state, so we get to make it public for
// testing purposes only by having a single `pub use` statement guarded by a cfg
// line.
mod internals {
    #[derive(Clone, Copy, Debug, PartialEq)]
    pub enum State {
        Idle,
        OnlyEncryptionSync,
        BothRunning,
    }
}

#[cfg(any(test, feature = "testing"))]
pub use internals::*;
#[cfg(not(any(test, feature = "testing")))]
use internals::*;

pub struct SyncService {
    /// Room list service used to synchronize the rooms state.
    room_list_service: Arc<RoomListService>,

    /// Encryption sync taking care of e2ee events.
    encryption_sync: Option<Arc<EncryptionSync>>,

    /// What's the state of this sync service?
    ///
    /// This is using a mutex that's acquired for the entirety of the
    /// [`Self::start`] and [`Self::pause`] methods' bodies, so as to avoid
    /// race conditions when these functions are called multiple times.
    state: Arc<Mutex<State>>,

    /// Task running the room list service.
    room_list_task: Arc<Mutex<Option<JoinHandle<()>>>>,

    /// Task running the encryption sync.
    encryption_sync_task: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl SyncService {
    /// Create a new builder for configuring an `SyncService`.
    pub fn builder(client: Client) -> SyncServiceBuilder {
        SyncServiceBuilder::new(client)
    }

    /// Get the underlying `RoomListService` instance for easier access to its
    /// methods.
    pub fn room_list_service(&self) -> Arc<RoomListService> {
        self.room_list_service.clone()
    }

    /// Start (or restart) the underlying sliding syncs.
    ///
    /// This can be called multiple times safely:
    /// - if the stream is still properly running, it won't be restarted.
    /// - if the stream has been aborted before, it will be properly cleaned up
    ///   and restarted.
    pub async fn start(&self) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap();

        // Only (re)start the tasks if any was stopped.
        if matches!(*state, State::BothRunning) {
            // It was already true, so we can skip the restart.
            return Ok(());
        }

        trace!("starting sync service");

        // First, take care of the room list.

        let mut room_list_task_guard = self.room_list_task.lock().unwrap();

        if let Some(task) = room_list_task_guard.take() {
            warn!("active room list service task when start()ing sync service");
            task.abort();
            // task is dropped here.
        }

        let room_list_task = self.room_list_service.clone();
        let state_task = self.state.clone();

        *room_list_task_guard = Some(spawn(async move {
            let room_list_stream = room_list_task.sync();
            pin_mut!(room_list_stream);
            while let Some(res) = room_list_stream.next().await {
                if let Err(err) = res {
                    error!("Error while processing room list (sync service): {err:#}");
                    break;
                }
            }
            // Put the whole sync service in the not-running state, so that subsequent
            // calls to `start()` will restart the room list service.
            *state_task.lock().unwrap() = State::OnlyEncryptionSync;
        }));

        drop(room_list_task_guard);

        if let Some(encryption_sync) = self.encryption_sync.clone() {
            // Then, only restart the encryption sync if it was explicitly not running
            // before.
            let mut task_guard = self.encryption_sync_task.lock().unwrap();
            if task_guard.is_none() {
                *task_guard = Some(spawn(async move {
                    loop {
                        // The encryption sync automatically restarts in case of errors, unless it
                        // is explicitly paused.
                        let encryption_sync_stream = encryption_sync.sync();

                        pin_mut!(encryption_sync_stream);

                        while let Some(res) = encryption_sync_stream.next().await {
                            if let Err(err) = res {
                                // Errors may include "real" errors, but also benign M_UNKNOWN_POS
                                // errors, so just log at the debug level since the latter will
                                // likely be more frequent.
                                debug!("Error while processing encryption sync (sync service): {err}. Restarting…");
                                break;
                            }
                        }
                    }
                }));
            }
        }

        *state = State::BothRunning;

        Ok(())
    }

    /// Stop the underlying sliding syncs.
    ///
    /// This must be called when the app goes into the background. It's better
    /// to call this API when the application exits, although not strictly
    /// necessary.
    pub fn pause(&self) -> Result<(), Error> {
        let mut state = self.state.lock().unwrap();

        trace!("pausing sync service");

        // First, request to stop the two underlying syncs; we'll look at the results
        // later, so that we're in a clean state independently of the request to
        // stop.

        let stop_room_list_result = match *state {
            State::Idle => {
                // No need to pause if we were not running.
                return Ok(());
            }

            State::BothRunning => {
                // Stop the room list sync.
                self.room_list_service.stop_sync()
            }

            State::OnlyEncryptionSync => Ok(()),
        };

        let stop_encryption_sync_result = if let Some(ref encryption_sync) = self.encryption_sync {
            encryption_sync.stop()
        } else {
            Ok(())
        };

        // Clean up both tasks: abort and drop both.

        {
            let mut room_list_task_guard = self.room_list_task.lock().unwrap();
            if let Some(task) = room_list_task_guard.take() {
                task.abort();
            }
        }

        {
            let mut encryption_task_guard = self.encryption_sync_task.lock().unwrap();
            if let Some(task) = encryption_task_guard.take() {
                task.abort();
            }
        }

        *state = State::Idle;

        // Now that we've updated the internal state, possibly report stop errors.
        stop_room_list_result?;
        stop_encryption_sync_result?;

        Ok(())
    }
}

#[cfg(any(test, feature = "testing"))]
impl SyncService {
    /// Is the sync service running?
    pub fn state(&self) -> State {
        *self.state.lock().unwrap()
    }
}

#[derive(Clone)]
pub struct SyncServiceBuilder {
    /// SDK client.
    client: Client,

    /// Is the encryption sync running as a separate instance of sliding sync
    /// (true), or is it fused in the main `RoomList` sliding sync (false)?
    with_encryption_sync: bool,

    /// Is the cross-process lock for the crypto store enabled?
    with_cross_process_lock: bool,

    /// Application identifier, used as the cross-process lock value, if
    /// applicable.
    identifier: String,
}

impl SyncServiceBuilder {
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

    /// Finish setting up the `SyncService`.
    ///
    /// This creates the underlying sliding syncs, and will *not* start them in
    /// the background. The resulting `SyncService` must be kept alive as
    /// long as the sliding syncs are supposed to run.
    pub async fn build(self) -> Result<SyncService, Error> {
        let (room_list, encryption_sync) = if self.with_encryption_sync {
            let room_list = RoomListService::new(self.client.clone()).await?;
            let encryption_sync = EncryptionSync::new(
                self.identifier,
                self.client,
                None,
                WithLocking::from(self.with_cross_process_lock),
            )
            .await?;
            (room_list, Some(Arc::new(encryption_sync)))
        } else {
            let room_list = RoomListService::new_with_encryption(self.client.clone()).await?;
            (room_list, None)
        };

        Ok(SyncService {
            room_list_service: Arc::new(room_list),
            encryption_sync,
            encryption_sync_task: Arc::new(Mutex::new(None)),
            room_list_task: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(State::Idle)),
        })
    }
}

/// Errors for the `SyncService` API.
#[derive(Debug, Error)]
pub enum Error {
    /// An error received from the `RoomList` API.
    #[error(transparent)]
    RoomList(#[from] room_list_service::Error),

    /// An error received from the `EncryptionSync` API.
    #[error(transparent)]
    EncryptionSync(#[from] encryption_sync::Error),
}
