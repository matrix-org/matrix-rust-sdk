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
//! The sync service will signal errors via its
//! [`state`](SyncService::state) that the user
//! MUST observe. Whenever an error/termination is observed, the user MUST call
//! [`SyncService::start()`] again to restart the room list sync.

use std::sync::{Arc, Mutex};

use eyeball::{SharedObservable, Subscriber};
use futures_core::Future;
use futures_util::{pin_mut, StreamExt as _};
use matrix_sdk::Client;
use thiserror::Error;
use tokio::{
    sync::{
        mpsc::{Receiver, Sender},
        Mutex as AsyncMutex, OwnedMutexGuard,
    },
    task::{spawn, JoinHandle},
};
use tracing::{error, info, instrument, trace, warn, Instrument, Level};

use crate::{
    encryption_sync_service::{self, EncryptionSyncPermit, EncryptionSyncService, WithLocking},
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
pub enum State {
    /// The service hasn't ever been started yet, or has been stopped.
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
    encryption_sync_service: Option<Arc<EncryptionSyncService>>,

    /// What's the state of this sync service?
    state: SharedObservable<State>,

    /// Use a mutex everytime to modify the `state` value, otherwise it would be
    /// possible to have race conditions when starting or pausing the
    /// service multiple times really quickly.
    modifying_state: AsyncMutex<()>,

    /// Task running the room list service.
    room_list_task: Arc<Mutex<Option<JoinHandle<()>>>>,

    /// Task running the encryption sync.
    encryption_sync_task: Arc<Mutex<Option<JoinHandle<()>>>>,

    /// Global lock to allow using at most one `EncryptionSyncService` at all
    /// times.
    ///
    /// This ensures that there's only one ever existing in the application's
    /// lifetime (under the assumption that there is at most one
    /// `SyncService` per application).
    encryption_sync_permit: Arc<AsyncMutex<EncryptionSyncPermit>>,

    /// Scheduler task ensuring proper termination.
    ///
    /// This task is waiting for a `TerminationReport` from any of the other two
    /// tasks, or from a user request via [`Self::stop()`]. It makes sure
    /// that the two services are properly shut up and just interrupted.
    ///
    /// This is set at the same time as the other two tasks.
    scheduler_task: Arc<Mutex<Option<JoinHandle<()>>>>,

    /// `TerminationReport` sender for the [`Self::stop()`] function.
    ///
    /// This is set at the same time as all the tasks in [`Self::start()`].
    scheduler_sender: Mutex<Option<Sender<TerminationReport>>>,
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
    pub fn state(&self) -> Subscriber<State> {
        self.state.subscribe()
    }

    /// The role of the scheduler task is to wait for a termination message
    /// (`TerminationReport`), sent either because we wanted to stop both
    /// syncs, or because one of the syncs failed (in which case we'll stop
    /// the other one too).
    fn spawn_scheduler_task(
        &self,
        mut receiver: Receiver<TerminationReport>,
    ) -> impl Future<Output = ()> {
        let encryption_sync_task = self.encryption_sync_task.clone();
        let encryption_sync = self.encryption_sync_service.clone();
        let room_list_service = self.room_list_service.clone();
        let room_list_task = self.room_list_task.clone();
        let state = self.state.clone();

        async move {
            let Some(report) = receiver.recv().await else {
                info!("internal channel has been closed?");
                return;
            };

            // If one service failed, make sure to request stopping the other one.
            let (stop_room_list, stop_encryption) = match &report.origin {
                TerminationOrigin::EncryptionSync => (true, false),
                TerminationOrigin::RoomList => (false, true),
                TerminationOrigin::Scheduler => (true, true),
            };

            // Stop both services, and wait for the streams to properly finish: at some
            // point they'll return `None` and will exit their infinite loops,
            // and their tasks will gracefully terminate.

            if stop_room_list {
                if let Err(err) = room_list_service.stop_sync() {
                    error!("unable to stop room list service: {err:#}");
                }
            }

            {
                let task = room_list_task.lock().unwrap().take();
                if let Some(task) = task {
                    if let Err(err) = task.await {
                        error!("when awaiting room list service: {err:#}");
                    }
                }
            }

            if let Some(encryption_sync) = &encryption_sync {
                if stop_encryption {
                    if let Err(err) = encryption_sync.stop_sync() {
                        warn!("unable to stop encryption sync: {err:#}");
                    }
                }

                let task = encryption_sync_task.lock().unwrap().take();
                if let Some(task) = task {
                    if let Err(err) = task.await {
                        error!("when awaiting encryption sync: {err:#}");
                    }
                }
            }

            if report.is_error {
                if report.has_expired {
                    if stop_room_list {
                        room_list_service.expire_sync_session().await;
                    }
                    if stop_encryption {
                        // Expire the encryption sync too.
                        if let Some(encryption_sync) = encryption_sync {
                            encryption_sync.expire_sync_session().await;
                        }
                    }
                }

                state.set(State::Error);
            } else if matches!(report.origin, TerminationOrigin::Scheduler) {
                state.set(State::Idle);
            } else {
                state.set(State::Terminated);
            }
        }
        .instrument(tracing::span!(Level::WARN, "scheduler task"))
    }

    fn spawn_encryption_sync(
        &self,
        encryption_sync: Arc<EncryptionSyncService>,
        sender: Sender<TerminationReport>,
        sync_permit_guard: OwnedMutexGuard<EncryptionSyncPermit>,
    ) -> impl Future<Output = ()> {
        async move {
            let encryption_sync_stream = encryption_sync.sync(sync_permit_guard);
            pin_mut!(encryption_sync_stream);

            let (is_error, has_expired) = loop {
                let res = encryption_sync_stream.next().await;
                match res {
                    Some(Ok(())) => {
                        // Carry on.
                    }
                    Some(Err(err)) => {
                        // If the encryption sync error was an expired session, also expire the
                        // room list sync.
                        let has_expired =
                            if let encryption_sync_service::Error::SlidingSync(err) = &err {
                                err.client_api_error_kind()
                                    == Some(&ruma::api::client::error::ErrorKind::UnknownPos)
                            } else {
                                false
                            };
                        error!("Error while processing encryption in sync service: {err:#}");
                        break (true, has_expired);
                    }
                    None => {
                        // The stream has ended.
                        break (false, false);
                    }
                }
            };

            if let Err(err) = sender
                .send(TerminationReport {
                    is_error,
                    has_expired,
                    origin: TerminationOrigin::EncryptionSync,
                })
                .await
            {
                error!("Error while sending termination report: {err:#}");
            }
        }
    }

    fn spawn_room_list_sync(&self, sender: Sender<TerminationReport>) -> impl Future<Output = ()> {
        let room_list_service = self.room_list_service.clone();

        async move {
            let room_list_stream = room_list_service.sync();
            pin_mut!(room_list_stream);

            let (is_error, has_expired) = loop {
                let res = room_list_stream.next().await;
                match res {
                    Some(Ok(())) => {
                        // Carry on.
                    }
                    Some(Err(err)) => {
                        // If the room list error was an expired session, also expire the
                        // encryption sync.
                        let has_expired = if let room_list_service::Error::SlidingSync(err) = &err {
                            err.client_api_error_kind()
                                == Some(&ruma::api::client::error::ErrorKind::UnknownPos)
                        } else {
                            false
                        };
                        error!("Error while processing room list in sync service: {err:#}");
                        break (true, has_expired);
                    }
                    None => {
                        // The stream has ended.
                        break (false, false);
                    }
                }
            };

            if let Err(err) = sender
                .send(TerminationReport {
                    is_error,
                    has_expired,
                    origin: TerminationOrigin::RoomList,
                })
                .await
            {
                error!("Error while sending termination report: {err:#}");
            }
        }
    }

    /// Start (or restart) the underlying sliding syncs.
    ///
    /// This can be called multiple times safely:
    /// - if the stream is still properly running, it won't be restarted.
    /// - if the stream has been aborted before, it will be properly cleaned up
    ///   and restarted.
    pub async fn start(&self) {
        let _guard = self.modifying_state.lock().await;

        // Only (re)start the tasks if any was stopped.
        if matches!(self.state.get(), State::Running) {
            // It was already true, so we can skip the restart.
            return;
        }

        trace!("starting sync service");

        let (sender, receiver) = tokio::sync::mpsc::channel(16);

        // First, take care of the room list.
        *self.room_list_task.lock().unwrap() =
            Some(spawn(self.spawn_room_list_sync(sender.clone())));

        // Then, take care of the encryption sync.
        if let Some(encryption_sync) = self.encryption_sync_service.clone() {
            let sync_permit_guard = self.encryption_sync_permit.clone().lock_owned().await;
            *self.encryption_sync_task.lock().unwrap() = Some(spawn(self.spawn_encryption_sync(
                encryption_sync,
                sender.clone(),
                sync_permit_guard,
            )));
        }

        // Spawn the scheduler task.
        *self.scheduler_sender.lock().unwrap() = Some(sender);
        *self.scheduler_task.lock().unwrap() = Some(spawn(self.spawn_scheduler_task(receiver)));

        self.state.set(State::Running);
    }

    /// Stop the underlying sliding syncs.
    ///
    /// This must be called when the app goes into the background. It's better
    /// to call this API when the application exits, although not strictly
    /// necessary.
    #[instrument(skip_all)]
    pub async fn stop(&self) -> Result<(), Error> {
        let _guard = self.modifying_state.lock().await;

        match self.state.get() {
            State::Idle | State::Terminated | State::Error => {
                // No need to stop if we were not running.
                return Ok(());
            }
            State::Running => {}
        };

        trace!("pausing sync service");

        // First, request to stop the two underlying syncs; we'll look at the results
        // later, so that we're in a clean state independently of the request to
        // stop.

        let sender = self.scheduler_sender.lock().unwrap().clone();
        sender
            .ok_or_else(|| {
                error!("missing sender");
                Error::InternalSchedulerError
            })?
            .send(TerminationReport {
                is_error: false,
                has_expired: false,
                origin: TerminationOrigin::Scheduler,
            })
            .await
            .map_err(|err| {
                error!("when sending termination report: {err}");
                Error::InternalSchedulerError
            })?;

        let scheduler_task = self.scheduler_task.lock().unwrap().take();
        scheduler_task
            .ok_or_else(|| {
                error!("missing scheduler task");
                Error::InternalSchedulerError
            })?
            .await
            .map_err(|err| {
                error!("couldn't finish scheduler task: {err}");
                Error::InternalSchedulerError
            })?;

        Ok(())
    }

    /// Attempt to get a permit to use an `EncryptionSyncService` at a given
    /// time.
    ///
    /// This ensures there is at most one [`EncryptionSyncService`] active at
    /// any time, per application.
    pub fn try_get_encryption_sync_permit(&self) -> Option<OwnedMutexGuard<EncryptionSyncPermit>> {
        self.encryption_sync_permit.clone().try_lock_owned().ok()
    }
}

enum TerminationOrigin {
    EncryptionSync,
    RoomList,
    Scheduler,
}

struct TerminationReport {
    is_error: bool,
    has_expired: bool,
    origin: TerminationOrigin,
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
        let encryption_sync_permit = Arc::new(AsyncMutex::new(EncryptionSyncPermit::new()));

        let (room_list, encryption_sync) = if self.with_encryption_sync {
            let room_list = RoomListService::new(self.client.clone()).await?;

            let encryption_sync = EncryptionSyncService::new(
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
            encryption_sync_service: encryption_sync,
            encryption_sync_task: Arc::new(Mutex::new(None)),
            room_list_task: Arc::new(Mutex::new(None)),
            scheduler_task: Arc::new(Mutex::new(None)),
            scheduler_sender: Mutex::new(None),
            state: SharedObservable::new(State::Idle),
            modifying_state: AsyncMutex::new(()),
            encryption_sync_permit,
        })
    }
}

/// Errors for the `SyncService` API.
#[derive(Debug, Error)]
pub enum Error {
    /// An error received from the `RoomListService` API.
    #[error(transparent)]
    RoomList(#[from] room_list_service::Error),

    /// An error received from the `EncryptionSyncService` API.
    #[error(transparent)]
    EncryptionSync(#[from] encryption_sync_service::Error),

    #[error("the scheduler channel has run into an unexpected error")]
    InternalSchedulerError,
}
