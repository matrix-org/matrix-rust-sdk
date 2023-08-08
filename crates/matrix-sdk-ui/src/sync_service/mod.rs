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

use eyeball::{SharedObservable, Subscriber};
use futures_util::{pin_mut, StreamExt as _};
use matrix_sdk::Client;
use thiserror::Error;
use tokio::sync::Mutex as AsyncMutex;
use tokio::task::{spawn, JoinHandle};
use tracing::{error, trace, warn};

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
/// [`SyncService::start`] method, in case it terminated, gracefully or not.
///
/// This can be observed with [`SyncService::state`].
#[derive(Clone, Debug, PartialEq)]
pub enum SyncServiceState {
    /// The service hasn't ever been started yet.
    Idle,
    /// The underlying syncs are properly running in the background.
    Running,
    /// Any of the underlying syncs has terminated gracefully (i.e. be stopped).
    Terminated,
    /// Any of the underlying syncs has ran into an error.
    Error,
}

pub struct SyncService {
    /// Room list service used to synchronize the rooms state.
    room_list_service: Arc<RoomListService>,

    /// Encryption sync taking care of e2ee events.
    encryption_sync: Option<Arc<EncryptionSync>>,

    /// What's the state of this sync service?
    state: SharedObservable<SyncServiceState>,

    /// Use a mutex everytime to modify the `state` value, otherwise it would be possible to have
    /// race conditions when starting or pausing the service multiple times really quickly.
    modifying_state: AsyncMutex<()>,

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

    /// Returns the state of the sync service.
    pub fn state(&self) -> Subscriber<SyncServiceState> {
        self.state.subscribe()
    }

    /// Start (or restart) the underlying sliding syncs.
    ///
    /// This can be called multiple times safely:
    /// - if the stream is still properly running, it won't be restarted.
    /// - if the stream has been aborted before, it will be properly cleaned up
    ///   and restarted.
    pub async fn start(&self) -> Result<(), Error> {
        let _guard = self.modifying_state.lock().await;

        let state = self.state.get();

        // Only (re)start the tasks if any was stopped.
        if matches!(state, SyncServiceState::Running) {
            // It was already true, so we can skip the restart.
            return Ok(());
        }

        trace!("starting sync service");

        // Set up a sender/receiver pair so that each service can notify the other when it's
        // expired, avoiding a double restart.

        let (sender, mut receiver) = tokio::sync::broadcast::channel(16);

        // First, take care of the room list.

        let mut room_list_task_guard = self.room_list_task.lock().unwrap();

        if let Some(task) = room_list_task_guard.take() {
            warn!("active room list service task when start()ing sync service");
            task.abort();
            // task is dropped here.
        }

        let room_list_task = self.room_list_service.clone();
        let sender_clone = sender.clone();
        let mut receiver_clone = sender.subscribe();

        let state_task = self.state.clone();

        *room_list_task_guard = Some(spawn(async move {
            let room_list_stream = room_list_task.sync();
            pin_mut!(room_list_stream);

            loop {
                tokio::select! {
                    biased;

                    internal_message = receiver_clone.recv() => {
                        // Note: don't update the overall state here: it's been written to by the
                        // other task, so we don't have to do it ourselves.
                        if let Err(err) = internal_message {
                            warn!("Sync service broadcast channel error when receiving (room list): {err:#}");
                        }
                        room_list_task.expire_session().await;
                        break;
                    }

                    res = room_list_stream.next() => {
                        match res {
                            Some(Ok(())) => {
                                // Carry on.
                            }
                            Some(Err(err)) => {
                                let mut was_expired = false;
                                if let room_list_service::Error::SlidingSync(err) = &err {
                                    if err.client_api_error_kind() == Some(&ruma::api::client::error::ErrorKind::UnknownPos) {
                                        was_expired = true;
                                        if let Err(err) = sender_clone.send(()) {
                                            warn!("Sync service broadcast channel when writing (room list): {err:#}");
                                        }
                                    }
                                }
                                if !was_expired {
                                    error!("Error while processing room list (sync service): {err:#}");
                                }
                                state_task.set(SyncServiceState::Error);
                                break;
                            }
                            None => {
                                // The stream has ended.
                                state_task.set(SyncServiceState::Terminated);
                                break;
                            }
                        }
                    }
                }
            }
        }));

        drop(room_list_task_guard);

        if let Some(encryption_sync) = self.encryption_sync.clone() {
            let mut task_guard = self.encryption_sync_task.lock().unwrap();

            if let Some(task) = task_guard.take() {
                warn!("active encryption sync service service task when start()ing sync service");
                task.abort();
                // task is dropped here.
            }

            let state_task = self.state.clone();

            *task_guard = Some(spawn(async move {
                let encryption_sync_stream = encryption_sync.sync();

                pin_mut!(encryption_sync_stream);

                loop {
                    tokio::select! {
                        biased;

                        internal_message = receiver.recv() => {
                            if let Err(err) = internal_message {
                                warn!("Sync service broadcast channel error when receiving (encryption sync): {err:#}");
                            }
                            encryption_sync.expire_session().await;
                            break;
                        }

                        res = encryption_sync_stream.next() => {
                            match res {
                                Some(Ok(())) => {
                                    // Carry on.
                                }
                                Some(Err(err)) => {
                                    let mut was_expired = false;
                                    if let encryption_sync::Error::SlidingSync(err) = &err {
                                        if err.client_api_error_kind() == Some(&ruma::api::client::error::ErrorKind::UnknownPos) {
                                            was_expired = true;
                                            if let Err(err) = sender.send(()) {
                                                warn!("Sync service broadcast channel when writing (encryption sync): {err:#}");
                                            }
                                        }
                                    }
                                    if !was_expired {
                                        error!("Error while processing encryption sync (sync service): {err:#}");
                                    }
                                    state_task.set(SyncServiceState::Error);
                                    break;
                                }
                                None => {
                                    // The stream has ended.
                                    state_task.set(SyncServiceState::Terminated);
                                    break;
                                }
                            }
                        }
                    }
                }
            }));
        }

        self.state.set(SyncServiceState::Running);

        Ok(())
    }

    fn cancel_and_stop_encryption_sync(&self) -> Result<(), Error> {
        {
            let mut encryption_task_guard = self.encryption_sync_task.lock().unwrap();
            if let Some(task) = encryption_task_guard.take() {
                task.abort();
            }
        }
        if let Some(ref encryption_sync) = self.encryption_sync {
            encryption_sync.stop()?;
        }
        Ok(())
    }

    fn cancel_and_stop_room_list(&self) -> Result<(), Error> {
        {
            let mut room_list_task_guard = self.room_list_task.lock().unwrap();
            if let Some(task) = room_list_task_guard.take() {
                task.abort();
            }
        }
        self.room_list_service.stop_sync()?;
        Ok(())
    }

    /// Stop the underlying sliding syncs.
    ///
    /// This must be called when the app goes into the background. It's better
    /// to call this API when the application exits, although not strictly
    /// necessary.
    pub async fn pause(&self) -> Result<(), Error> {
        trace!("pausing sync service");

        let _guard = self.modifying_state.lock().await;

        match self.state.get() {
            SyncServiceState::Idle | SyncServiceState::Terminated | SyncServiceState::Error => {
                // No need to pause if we were not running.
                return Ok(());
            }
            SyncServiceState::Running => {}
        };

        // First, request to stop the two underlying syncs; we'll look at the results
        // later, so that we're in a clean state independently of the request to
        // stop.

        let encryption_sync_result = self.cancel_and_stop_encryption_sync();
        let room_list_result = self.cancel_and_stop_room_list();

        self.state.set(SyncServiceState::Idle);

        // Now that we've updated the internal state, possibly report stop errors.
        encryption_sync_result?;
        room_list_result?;

        Ok(())
    }
}

// Testing helpers, mostly.
#[doc(hidden)]
impl SyncService {
    /// Return the existential states of internal tasks.
    pub fn task_states(&self) -> (bool, bool) {
        (
            self.encryption_sync_task.lock().unwrap().is_some(),
            self.room_list_task.lock().unwrap().is_some(),
        )
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
            state: SharedObservable::new(SyncServiceState::Idle),
            modifying_state: AsyncMutex::new(()),
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
