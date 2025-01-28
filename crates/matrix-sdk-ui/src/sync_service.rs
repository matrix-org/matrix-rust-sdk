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
//! The sync service will signal errors via its [`state`](SyncService::state)
//! that the user MUST observe. Whenever an error/termination is observed, the
//! user MUST call [`SyncService::start()`] again to restart the room list sync.

use std::sync::Arc;

use eyeball::{SharedObservable, Subscriber};
use futures_core::Future;
use futures_util::{pin_mut, StreamExt as _};
use matrix_sdk::{
    executor::{spawn, JoinHandle},
    Client,
};
use thiserror::Error;
use tokio::sync::{
    mpsc::{Receiver, Sender},
    Mutex as AsyncMutex, OwnedMutexGuard,
};
use tracing::{error, info, instrument, trace, warn, Instrument, Level};

use crate::{
    encryption_sync_service::{self, EncryptionSyncPermit, EncryptionSyncService, WithLocking},
    room_list_service::{self, RoomListService},
};

/// Current state of the application.
///
/// This is a high-level state indicating what's the status of the underlying
/// syncs. The application starts in [`State::Running`] mode, and then hits a
/// terminal state [`State::Terminated`] (if it gracefully exited) or
/// [`State::Error`] (in case any of the underlying syncs ran into an error).
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

/// A supervisor responsible for managing two sync tasks: one for handling the
/// room list and another for supporting end-to-end encryption.
///
/// The two sync tasks are spawned as child tasks and are contained within the
/// supervising task, which is stored in the [`SyncTaskSupervisor::task`] field.
///
/// The supervisor ensures the two child tasks are managed as a single unit,
/// allowing for them to be shutdown in unison.
struct SyncTaskSupervisor {
    /// The supervising task that manages and contains the two sync child tasks.
    task: JoinHandle<()>,
    /// [`TerminationReport`] sender for the [`SyncTaskSupervisor::shutdown()`]
    /// function.
    termination_sender: Sender<TerminationReport>,
}

impl SyncTaskSupervisor {
    async fn new(
        inner: &SyncServiceInner,
        room_list_service: Arc<RoomListService>,
        encryption_sync_permit: Arc<AsyncMutex<EncryptionSyncPermit>>,
    ) -> Self {
        let (sender, receiver) = tokio::sync::mpsc::channel(16);
        let (room_list_task, encryption_sync_task) = Self::spawn_child_tasks(
            inner,
            room_list_service.clone(),
            encryption_sync_permit,
            sender.clone(),
        )
        .await;

        let task = spawn(Self::spawn_supervisor_task(
            inner,
            room_list_service,
            room_list_task,
            encryption_sync_task,
            receiver,
        ));

        Self { task, termination_sender: sender }
    }

    /// The role of the supervisor task is to wait for a termination message
    /// ([`TerminationReport`]), sent either because we wanted to stop both
    /// syncs, or because one of the syncs failed (in which case we'll stop the
    /// other one too).
    fn spawn_supervisor_task(
        inner: &SyncServiceInner,
        room_list_service: Arc<RoomListService>,
        room_list_task: JoinHandle<()>,
        encryption_sync_task: JoinHandle<()>,
        mut receiver: Receiver<TerminationReport>,
    ) -> impl Future<Output = ()> {
        let encryption_sync = inner.encryption_sync_service.clone();
        let state = inner.state.clone();

        async move {
            let report = if let Some(report) = receiver.recv().await {
                report
            } else {
                info!("internal channel has been closed?");
                // We should still stop the child tasks in the unlikely scenario that our
                // receiver died.
                TerminationReport::supervisor_error()
            };

            // If one service failed, make sure to request stopping the other one.
            let (stop_room_list, stop_encryption) = match &report.origin {
                TerminationOrigin::EncryptionSync => (true, false),
                TerminationOrigin::RoomList => (false, true),
                TerminationOrigin::Supervisor => (true, true),
            };

            // Stop both services, and wait for the streams to properly finish: at some
            // point they'll return `None` and will exit their infinite loops, and their
            // tasks will gracefully terminate.

            if stop_room_list {
                if let Err(err) = room_list_service.stop_sync() {
                    warn!(?report, "unable to stop room list service: {err:#}");
                }

                if report.has_expired {
                    room_list_service.expire_sync_session().await;
                }
            }

            if let Err(err) = room_list_task.await {
                error!("when awaiting room list service: {err:#}");
            }

            if stop_encryption {
                if let Err(err) = encryption_sync.stop_sync() {
                    warn!(?report, "unable to stop encryption sync: {err:#}");
                }

                if report.has_expired {
                    encryption_sync.expire_sync_session().await;
                }
            }

            if let Err(err) = encryption_sync_task.await {
                error!("when awaiting encryption sync: {err:#}");
            }

            if report.is_error {
                state.set(State::Error);
            } else if matches!(report.origin, TerminationOrigin::Supervisor) {
                state.set(State::Idle);
            } else {
                state.set(State::Terminated);
            }
        }
        .instrument(tracing::span!(Level::WARN, "supervisor task"))
    }

    async fn spawn_child_tasks(
        inner: &SyncServiceInner,
        room_list_service: Arc<RoomListService>,
        encryption_sync_permit: Arc<AsyncMutex<EncryptionSyncPermit>>,
        sender: Sender<TerminationReport>,
    ) -> (JoinHandle<()>, JoinHandle<()>) {
        // First, take care of the room list.
        let room_list_task = spawn(Self::room_list_sync_task(room_list_service, sender.clone()));

        // Then, take care of the encryption sync.
        let sync_permit_guard = encryption_sync_permit.clone().lock_owned().await;
        let encryption_sync_task = spawn(Self::encryption_sync_task(
            inner.encryption_sync_service.clone(),
            sender.clone(),
            sync_permit_guard,
        ));

        (room_list_task, encryption_sync_task)
    }

    fn check_if_expired(err: &matrix_sdk::Error) -> bool {
        err.client_api_error_kind() == Some(&ruma::api::client::error::ErrorKind::UnknownPos)
    }

    async fn encryption_sync_task(
        encryption_sync: Arc<EncryptionSyncService>,
        sender: Sender<TerminationReport>,
        sync_permit_guard: OwnedMutexGuard<EncryptionSyncPermit>,
    ) {
        use encryption_sync_service::Error;

        let encryption_sync_stream = encryption_sync.sync(sync_permit_guard);
        pin_mut!(encryption_sync_stream);

        let (is_error, has_expired) = loop {
            match encryption_sync_stream.next().await {
                Some(Ok(())) => {
                    // Carry on.
                }
                Some(Err(err)) => {
                    // If the encryption sync error was an expired session, also expire the
                    // room list sync.
                    let has_expired = if let Error::SlidingSync(err) = &err {
                        Self::check_if_expired(err)
                    } else {
                        false
                    };

                    if !has_expired {
                        error!("Error while processing encryption in sync service: {err:#}");
                    }

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

    async fn room_list_sync_task(
        room_list_service: Arc<RoomListService>,
        sender: Sender<TerminationReport>,
    ) {
        use room_list_service::Error;

        let room_list_stream = room_list_service.sync();
        pin_mut!(room_list_stream);

        let (is_error, has_expired) = loop {
            match room_list_stream.next().await {
                Some(Ok(())) => {
                    // Carry on.
                }
                Some(Err(err)) => {
                    // If the room list error was an expired session, also expire the
                    // encryption sync.
                    let has_expired = if let Error::SlidingSync(err) = &err {
                        Self::check_if_expired(err)
                    } else {
                        false
                    };

                    if !has_expired {
                        error!("Error while processing room list in sync service: {err:#}");
                    }

                    break (true, has_expired);
                }
                None => {
                    // The stream has ended.
                    break (false, false);
                }
            }
        };

        if let Err(err) = sender
            .send(TerminationReport { is_error, has_expired, origin: TerminationOrigin::RoomList })
            .await
        {
            error!("Error while sending termination report: {err:#}");
        }
    }

    async fn shutdown(self) -> Result<(), Error> {
        match self
            .termination_sender
            .send(TerminationReport {
                is_error: false,
                has_expired: false,
                origin: TerminationOrigin::Supervisor,
            })
            .await
        {
            Ok(_) => self.task.await.map_err(|err| {
                error!("couldn't finish supervisor task: {err}");
                Error::InternalSupervisorError
            }),
            Err(err) => {
                error!("when sending termination report: {err}");
                // Let's abort the task if it won't shut down properly, otherwise we would have
                // left it as a detached task.
                self.task.abort();
                Err(Error::InternalSupervisorError)
            }
        }
    }
}

struct SyncServiceInner {
    encryption_sync_service: Arc<EncryptionSyncService>,
    state: SharedObservable<State>,
    /// Supervisor task ensuring proper termination.
    ///
    /// This task is waiting for a [`TerminationReport`] from any of the other
    /// two tasks, or from a user request via [`SyncService::stop()`]. It
    /// makes sure that the two services are properly shut up and just
    /// interrupted.
    ///
    /// This is set at the same time as the other two tasks.
    supervisor: Option<SyncTaskSupervisor>,
}

/// A high level manager for your Matrix syncing needs.
///
/// The [`SyncService`] is responsible for managing real-time synchronization
/// with a Matrix server. It can initiate and maintain the necessary
/// synchronization tasks for you.
///
/// **Note**: The [`SyncService`] requires a server with support for [MSC4186],
/// otherwise it will fail with an 404 `M_UNRECOGNIZED` request error.
///
/// [MSC4186]: https://github.com/matrix-org/matrix-spec-proposals/pull/4186/
///
/// # Example
///
/// ```no_run
/// use matrix_sdk::Client;
/// use matrix_sdk_ui::sync_service::{State, SyncService};
/// # use url::Url;
/// # async {
/// let homeserver = Url::parse("http://example.com")?;
/// let client = Client::new(homeserver).await?;
///
/// client
///     .matrix_auth()
///     .login_username("example", "wordpass")
///     .initial_device_display_name("My bot")
///     .await?;
///
/// let sync_service = SyncService::builder(client).build().await?;
/// let mut state = sync_service.state();
///
/// while let Some(state) = state.next().await {
///     match state {
///         State::Idle => eprintln!("The sync service is idle."),
///         State::Running => eprintln!("The sync has started to run."),
///         State::Terminated => {
///             eprintln!("The sync service has been gracefully terminated");
///             break;
///         }
///         State::Error => {
///             eprintln!("The sync service has run into an error");
///             break;
///         }
///     }
/// }
/// # anyhow::Ok(()) };
/// ```
pub struct SyncService {
    inner: Arc<AsyncMutex<SyncServiceInner>>,

    /// Room list service used to synchronize the rooms state.
    room_list_service: Arc<RoomListService>,

    /// What's the state of this sync service? This field is replicated from the
    /// [`SyncServiceInner`] struct, but it should not be modified in this
    /// struct. It's re-exposed here so we can subscribe to the state without
    /// taking the lock on the `inner` field.
    state: SharedObservable<State>,

    /// Global lock to allow using at most one [`EncryptionSyncService`] at all
    /// times.
    ///
    /// This ensures that there's only one ever existing in the application's
    /// lifetime (under the assumption that there is at most one [`SyncService`]
    /// per application).
    encryption_sync_permit: Arc<AsyncMutex<EncryptionSyncPermit>>,
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

    /// Start (or restart) the underlying sliding syncs.
    ///
    /// This can be called multiple times safely:
    /// - if the stream is still properly running, it won't be restarted.
    /// - if the stream has been aborted before, it will be properly cleaned up
    ///   and restarted.
    pub async fn start(&self) {
        let mut inner = self.inner.lock().await;

        // Only (re)start the tasks if any was stopped.
        match inner.state.get() {
            // If we're already running, there's nothing to do.
            State::Running => (),
            State::Idle | State::Terminated | State::Error => {
                trace!("starting sync service");

                inner.supervisor = Some(
                    SyncTaskSupervisor::new(
                        &inner,
                        self.room_list_service.clone(),
                        self.encryption_sync_permit.clone(),
                    )
                    .await,
                );
                inner.state.set(State::Running);
            }
        }
    }

    /// Stop the underlying sliding syncs.
    ///
    /// This must be called when the app goes into the background. It's better
    /// to call this API when the application exits, although not strictly
    /// necessary.
    #[instrument(skip_all)]
    pub async fn stop(&self) -> Result<(), Error> {
        let mut inner = self.inner.lock().await;

        match inner.state.get() {
            State::Idle | State::Terminated | State::Error => {
                // No need to stop if we were not running.
                return Ok(());
            }
            State::Running => (),
        }

        trace!("pausing sync service");

        // First, request to stop the two underlying syncs; we'll look at the results
        // later, so that we're in a clean state independently of the request to stop.

        // Remove the supervisor from our inner state and request the tasks to be
        // shutdown.
        let supervisor = inner.supervisor.take().ok_or_else(|| {
            error!("The supervisor was not properly started up");
            Error::InternalSupervisorError
        })?;

        supervisor.shutdown().await
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

#[derive(Debug)]
enum TerminationOrigin {
    EncryptionSync,
    RoomList,
    Supervisor,
}

#[derive(Debug)]
struct TerminationReport {
    is_error: bool,
    has_expired: bool,
    origin: TerminationOrigin,
}

impl TerminationReport {
    fn supervisor_error() -> Self {
        TerminationReport {
            is_error: true,
            has_expired: false,
            origin: TerminationOrigin::Supervisor,
        }
    }
}

// Testing helpers, mostly.
#[doc(hidden)]
impl SyncService {
    /// Is the task supervisor running?
    pub async fn is_supervisor_running(&self) -> bool {
        self.inner.lock().await.supervisor.is_some()
    }
}

#[derive(Clone)]
pub struct SyncServiceBuilder {
    /// SDK client.
    client: Client,

    /// Is the cross-process lock for the crypto store enabled?
    with_cross_process_lock: bool,
}

impl SyncServiceBuilder {
    fn new(client: Client) -> Self {
        Self { client, with_cross_process_lock: false }
    }

    /// Enables the cross-process lock, if the sync service is being built in a
    /// multi-process setup.
    ///
    /// It's a prerequisite if another process can *also* process encryption
    /// events. This is only applicable to very specific use cases, like an
    /// external process attempting to decrypt notifications. In general,
    /// `with_cross_process_lock` should not be called.
    ///
    /// Be sure to have configured
    /// [`Client::cross_process_store_locks_holder_name`] accordingly.
    pub fn with_cross_process_lock(mut self) -> Self {
        self.with_cross_process_lock = true;
        self
    }

    /// Finish setting up the [`SyncService`].
    ///
    /// This creates the underlying sliding syncs, and will *not* start them in
    /// the background. The resulting [`SyncService`] must be kept alive as long
    /// as the sliding syncs are supposed to run.
    pub async fn build(self) -> Result<SyncService, Error> {
        let encryption_sync_permit = Arc::new(AsyncMutex::new(EncryptionSyncPermit::new()));

        let room_list = RoomListService::new(self.client.clone()).await?;

        let encryption_sync = Arc::new(
            EncryptionSyncService::new(
                self.client,
                None,
                WithLocking::from(self.with_cross_process_lock),
            )
            .await?,
        );

        let room_list_service = Arc::new(room_list);
        let state = SharedObservable::new(State::Idle);

        Ok(SyncService {
            state: state.clone(),
            room_list_service,
            encryption_sync_permit,
            inner: Arc::new(AsyncMutex::new(SyncServiceInner {
                supervisor: None,
                encryption_sync_service: encryption_sync,
                state,
            })),
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

    /// An error had occurred in the sync task supervisor, likely due to a bug.
    #[error("the supervisor channel has run into an unexpected error")]
    InternalSupervisorError,
}
