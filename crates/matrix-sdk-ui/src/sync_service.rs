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
//! user should call [`SyncService::start()`] again to restart the room list
//! sync, if that is not desirable, the offline support for the [`SyncService`]
//! may be enabled using the [`SyncServiceBuilder::with_offline_mode`] setting.

use std::{sync::Arc, time::Duration};

use eyeball::{SharedObservable, Subscriber};
use futures_util::{
    StreamExt as _,
    future::{Either, select},
    pin_mut,
};
use matrix_sdk::{
    Client,
    config::RequestConfig,
    executor::{JoinHandle, spawn},
    sleep::sleep,
};
use thiserror::Error;
use tokio::sync::{
    Mutex as AsyncMutex, OwnedMutexGuard,
    mpsc::{Receiver, Sender},
};
use tracing::{Instrument, Level, Span, error, info, instrument, trace, warn};

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
/// This can be observed with [`SyncService::state`].
#[derive(Clone, Debug)]
pub enum State {
    /// The service hasn't ever been started yet, or has been stopped.
    Idle,

    /// The underlying syncs are properly running in the background.
    Running,

    /// Any of the underlying syncs has terminated gracefully (i.e. be stopped).
    Terminated,

    /// Any of the underlying syncs has ran into an error.
    ///
    /// The associated [`enum@Error`] is inside an [`Arc`] to (i) make [`State`]
    /// cloneable, and to (ii) not make it heavier.
    Error(Arc<Error>),

    /// The service has entered offline mode. This state will only be entered if
    /// the [`SyncService`] has been built with the
    /// [`SyncServiceBuilder::with_offline_mode`] setting.
    ///
    /// The [`SyncService`] will enter the offline mode if syncing with the
    /// server fails, it will then periodically check if the server is
    /// available using the `/_matrix/client/versions` endpoint.
    ///
    /// Once the [`SyncService`] receives a 200 response from the
    /// `/_matrix/client/versions` endpoint, it will go back into the
    /// [`State::Running`] mode and attempt to sync again.
    ///
    /// Calling [`SyncService::start()`] while in this state will abort the
    /// `/_matrix/client/versions` checks and attempt to sync immediately.
    ///
    /// Calling [`SyncService::stop()`] will abort the offline mode and the
    /// [`SyncService`] will go into the [`State::Idle`] mode.
    Offline,
}

enum MaybeAcquiredPermit {
    Acquired(OwnedMutexGuard<EncryptionSyncPermit>),
    Unacquired(Arc<AsyncMutex<EncryptionSyncPermit>>),
}

impl MaybeAcquiredPermit {
    async fn acquire(self) -> OwnedMutexGuard<EncryptionSyncPermit> {
        match self {
            MaybeAcquiredPermit::Acquired(owned_mutex_guard) => owned_mutex_guard,
            MaybeAcquiredPermit::Unacquired(lock) => lock.lock_owned().await,
        }
    }
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
        let (task, termination_sender) =
            Self::spawn_supervisor_task(inner, room_list_service, encryption_sync_permit).await;

        Self { task, termination_sender }
    }

    /// Check if a homeserver is reachable.
    ///
    /// This function handles the offline mode by waiting for either a
    /// termination report or a successful `/_matrix/client/versions` response.
    ///
    /// This function waits for two conditions:
    ///
    /// 1. Waiting for a termination report: This ensures that the user can exit
    ///    offline mode and attempt to restart the [`SyncService`] manually.
    ///
    /// 2. Waiting to come back online: This continuously checks server
    ///    availability.
    ///
    /// If the `/_matrix/client/versions` request succeeds, the function exits
    /// without a termination report. If we receive a [`TerminationReport`] from
    /// the user, we exit immediately and return the termination report.
    async fn offline_check(
        client: &Client,
        receiver: &mut Receiver<TerminationReport>,
    ) -> Option<TerminationReport> {
        info!("Entering the offline mode");

        let wait_for_termination_report = async {
            loop {
                // Since we didn't empty the channel when entering the offline mode in fear that
                // we might miss a report with the
                // `TerminationOrigin::Supervisor` origin and the channel might contain stale
                // reports from one of the sync services, in case both of them have sent a
                // report, let's ignore all reports we receive from the sync
                // services.
                let report =
                    receiver.recv().await.unwrap_or_else(TerminationReport::supervisor_error);

                match report.origin {
                    TerminationOrigin::EncryptionSync | TerminationOrigin::RoomList => {}
                    // Since the sync service aren't running anymore, we can only receive a report
                    // from the supervisor. It would have probably made sense to have separate
                    // channels for reports the sync services send and the user can send using the
                    // `SyncService::stop()` method.
                    TerminationOrigin::Supervisor => break report,
                }
            }
        };

        let wait_to_be_online = async move {
            loop {
                // Encountering network failures when sending a request which has with no retry
                // limit set in the `RequestConfig` are treated as permanent failures and our
                // exponential backoff doesn't kick in.
                //
                // Let's set a retry limit so network failures are retried as well.
                let request_config = RequestConfig::default().retry_limit(5);

                // We're in an infinite loop, but our request sending already has an exponential
                // backoff set up. This will kick in for any request errors that we consider to
                // be transient. Common network errors (timeouts, DNS failures) or any server
                // error in the 5xx range of HTTP errors are considered to be transient.
                //
                // Still, as a precaution, we're going to sleep here for a while in the Error
                // case.
                match client.fetch_server_versions(Some(request_config)).await {
                    Ok(_) => break,
                    Err(_) => sleep(Duration::from_millis(100)).await,
                }
            }
        };

        pin_mut!(wait_for_termination_report);
        pin_mut!(wait_to_be_online);

        let maybe_termination_report = select(wait_for_termination_report, wait_to_be_online).await;

        let report = match maybe_termination_report {
            Either::Left((termination_report, _)) => Some(termination_report),
            Either::Right((_, _)) => None,
        };

        info!("Exiting offline mode: {report:?}");

        report
    }

    /// The role of the supervisor task is to wait for a termination message
    /// ([`TerminationReport`]), sent either because we wanted to stop both
    /// syncs, or because one of the syncs failed (in which case we'll stop the
    /// other one too).
    async fn spawn_supervisor_task(
        inner: &SyncServiceInner,
        room_list_service: Arc<RoomListService>,
        encryption_sync_permit: Arc<AsyncMutex<EncryptionSyncPermit>>,
    ) -> (JoinHandle<()>, Sender<TerminationReport>) {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);

        let encryption_sync = inner.encryption_sync_service.clone();
        let state = inner.state.clone();
        let termination_sender = sender.clone();

        // When we first start, and don't use offline mode, we want to acquire the sync
        // permit before we enter a future that might be polled at a later time,
        // this means that the permit will be acquired as soon as this future,
        // the one the `spawn_supervisor_task` function creates, is awaited.
        //
        // In other words, once `sync_service.start().await` is finished, the permit
        // will be in the acquired state.
        let mut sync_permit_guard =
            MaybeAcquiredPermit::Acquired(encryption_sync_permit.clone().lock_owned().await);

        let offline_mode = inner.with_offline_mode;
        let parent_span = inner.parent_span.clone();

        let future = async move {
            loop {
                let (room_list_task, encryption_sync_task) = Self::spawn_child_tasks(
                    room_list_service.clone(),
                    encryption_sync.clone(),
                    sync_permit_guard,
                    sender.clone(),
                    parent_span.clone(),
                )
                .await;

                sync_permit_guard = MaybeAcquiredPermit::Unacquired(encryption_sync_permit.clone());

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

                    if report.has_expired() {
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

                    if report.has_expired() {
                        encryption_sync.expire_sync_session().await;
                    }
                }

                if let Err(err) = encryption_sync_task.await {
                    error!("when awaiting encryption sync: {err:#}");
                }

                if let Some(error) = report.error {
                    if offline_mode {
                        state.set(State::Offline);

                        let client = room_list_service.client();

                        if let Some(report) = Self::offline_check(client, &mut receiver).await {
                            if let Some(error) = report.error {
                                state.set(State::Error(Arc::new(error)));
                            } else {
                                state.set(State::Idle);
                            }
                            break;
                        }

                        state.set(State::Running);
                    } else {
                        state.set(State::Error(Arc::new(error)));
                        break;
                    }
                } else if matches!(report.origin, TerminationOrigin::Supervisor) {
                    state.set(State::Idle);
                    break;
                } else {
                    state.set(State::Terminated);
                    break;
                }
            }
        }
        .instrument(tracing::span!(Level::WARN, "supervisor task"));

        let task = spawn(future);

        (task, termination_sender)
    }

    async fn spawn_child_tasks(
        room_list_service: Arc<RoomListService>,
        encryption_sync_service: Arc<EncryptionSyncService>,
        sync_permit_guard: MaybeAcquiredPermit,
        sender: Sender<TerminationReport>,
        parent_span: Span,
    ) -> (JoinHandle<()>, JoinHandle<()>) {
        // First, take care of the room list.
        let room_list_task = spawn(
            Self::room_list_sync_task(room_list_service, sender.clone())
                .instrument(parent_span.clone()),
        );

        // Then, take care of the encryption sync.
        let encryption_sync_task = spawn(
            Self::encryption_sync_task(
                encryption_sync_service,
                sender.clone(),
                sync_permit_guard.acquire().await,
            )
            .instrument(parent_span),
        );

        (room_list_task, encryption_sync_task)
    }

    async fn encryption_sync_task(
        encryption_sync: Arc<EncryptionSyncService>,
        sender: Sender<TerminationReport>,
        sync_permit_guard: OwnedMutexGuard<EncryptionSyncPermit>,
    ) {
        let encryption_sync_stream = encryption_sync.sync(sync_permit_guard);
        pin_mut!(encryption_sync_stream);

        let termination_report = loop {
            match encryption_sync_stream.next().await {
                Some(Ok(())) => {
                    // Carry on.
                }
                Some(Err(error)) => {
                    let termination_report = TerminationReport::encryption_sync(Some(error));

                    if !termination_report.has_expired() {
                        error!(
                            "Error while processing encryption in sync service: {:#?}",
                            termination_report.error
                        );
                    }

                    break termination_report;
                }
                None => {
                    // The stream has ended.
                    break TerminationReport::encryption_sync(None);
                }
            }
        };

        if let Err(err) = sender.send(termination_report).await {
            error!("Error while sending termination report: {err:#}");
        }
    }

    async fn room_list_sync_task(
        room_list_service: Arc<RoomListService>,
        sender: Sender<TerminationReport>,
    ) {
        let room_list_stream = room_list_service.sync();
        pin_mut!(room_list_stream);

        let termination_report = loop {
            match room_list_stream.next().await {
                Some(Ok(())) => {
                    // Carry on.
                }
                Some(Err(error)) => {
                    let termination_report = TerminationReport::room_list(Some(error));

                    if !termination_report.has_expired() {
                        error!(
                            "Error while processing room list in sync service: {:#?}",
                            termination_report.error
                        );
                    }

                    break termination_report;
                }
                None => {
                    // The stream has ended.
                    break TerminationReport::room_list(None);
                }
            }
        };

        if let Err(err) = sender.send(termination_report).await {
            error!("Error while sending termination report: {err:#}");
        }
    }

    async fn shutdown(self) {
        match self.termination_sender.send(TerminationReport::supervisor()).await {
            Ok(_) => {
                let _ = self.task.await.inspect_err(|err| {
                    // A `JoinError` indicates that the task was already dead, either because it got
                    // cancelled or because it panicked. We only cancel the task in the Err branch
                    // below and the task shouldn't be able to panic.
                    //
                    // So let's log an error and return.
                    error!("The supervisor task has stopped unexpectedly: {err:?}");
                });
            }
            Err(err) => {
                error!("Couldn't send the termination report to the supervisor task: {err}");
                // Let's abort the task if it won't shut down properly, otherwise we would have
                // left it as a detached task.
                self.task.abort();
            }
        }
    }
}

struct SyncServiceInner {
    encryption_sync_service: Arc<EncryptionSyncService>,

    /// Is the offline mode for the [`SyncService`] enabled?
    ///
    /// The offline mode is described in the [`State::Offline`] enum variant.
    with_offline_mode: bool,

    /// What's the state of this sync service?
    state: SharedObservable<State>,

    /// The parent tracing span to use for the tasks within this service.
    ///
    /// Normally this will be [`Span::none`], but it may be useful to assign a
    /// defined span, for example if there is more than one active sync
    /// service.
    parent_span: Span,

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

impl SyncServiceInner {
    async fn start(
        &mut self,
        room_list_service: Arc<RoomListService>,
        encryption_sync_permit: Arc<AsyncMutex<EncryptionSyncPermit>>,
    ) {
        trace!("starting sync service");

        self.supervisor =
            Some(SyncTaskSupervisor::new(self, room_list_service, encryption_sync_permit).await);
        self.state.set(State::Running);
    }

    async fn stop(&mut self) {
        trace!("pausing sync service");

        // Remove the supervisor from our state and request the tasks to be shutdown.
        if let Some(supervisor) = self.supervisor.take() {
            supervisor.shutdown().await;
        } else {
            error!("The sync service was not properly started, the supervisor task doesn't exist");
        }
    }

    async fn restart(
        &mut self,
        room_list_service: Arc<RoomListService>,
        encryption_sync_permit: Arc<AsyncMutex<EncryptionSyncPermit>>,
    ) {
        self.stop().await;
        self.start(room_list_service, encryption_sync_permit).await;
    }
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
///         State::Offline => eprintln!(
///             "We have entered the offline mode, the server seems to be
///              unavailable"
///         ),
///         State::Terminated => {
///             eprintln!("The sync service has been gracefully terminated");
///             break;
///         }
///         State::Error(_) => {
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
    /// - if the [`SyncService`] is in the offline mode we will exit the offline
    ///   mode and immediately attempt to sync again.
    /// - if the stream has been aborted before, it will be properly cleaned up
    ///   and restarted.
    pub async fn start(&self) {
        let mut inner = self.inner.lock().await;

        // Only (re)start the tasks if it's stopped or if we're in the offline mode.
        match inner.state.get() {
            // If we're already running, there's nothing to do.
            State::Running => {}
            // If we're in the offline mode, first stop the service and then start it again.
            State::Offline => {
                inner
                    .restart(self.room_list_service.clone(), self.encryption_sync_permit.clone())
                    .await
            }
            // Otherwise just start.
            State::Idle | State::Terminated | State::Error(_) => {
                inner
                    .start(self.room_list_service.clone(), self.encryption_sync_permit.clone())
                    .await
            }
        }
    }

    /// Stop the underlying sliding syncs.
    ///
    /// This must be called when the app goes into the background. It's better
    /// to call this API when the application exits, although not strictly
    /// necessary.
    #[instrument(skip_all)]
    pub async fn stop(&self) {
        let mut inner = self.inner.lock().await;

        match inner.state.get() {
            State::Idle | State::Terminated | State::Error(_) => {
                // No need to stop if we were not running.
                return;
            }
            State::Running | State::Offline => {}
        }

        inner.stop().await;
    }

    /// Force expiring both sessions.
    ///
    /// This ensures that the sync service is stopped before expiring both
    /// sessions. It should be used sparingly, as it will cause a restart of
    /// the sessions on the server as well.
    #[instrument(skip_all)]
    pub async fn expire_sessions(&self) {
        // First, stop the sync service if it was running; it's a no-op if it was
        // already stopped.
        self.stop().await;

        // Expire the room list sync session.
        self.room_list_service.expire_sync_session().await;

        // Expire the encryption sync session.
        self.inner.lock().await.encryption_sync_service.expire_sync_session().await;
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
    /// The origin of the termination.
    origin: TerminationOrigin,

    /// If the termination is due to an error, this is the cause.
    error: Option<Error>,
}

impl TerminationReport {
    /// Create a new [`TerminationReport`] with `origin` set to
    /// [`TerminationOrigin::EncryptionSync`] and `error` set to
    /// [`Error::EncryptionSync`].
    fn encryption_sync(error: Option<encryption_sync_service::Error>) -> Self {
        Self { origin: TerminationOrigin::EncryptionSync, error: error.map(Error::EncryptionSync) }
    }

    /// Create a new [`TerminationReport`] with `origin` set to
    /// [`TerminationOrigin::RoomList`] and `error` set to [`Error::RoomList`].
    fn room_list(error: Option<room_list_service::Error>) -> Self {
        Self { origin: TerminationOrigin::RoomList, error: error.map(Error::RoomList) }
    }

    /// Create a new [`TerminationReport`] with `origin` set to
    /// [`TerminationOrigin::Supervisor`] and `error` set to
    /// [`Error::Supervisor`].
    fn supervisor_error() -> Self {
        Self { origin: TerminationOrigin::Supervisor, error: Some(Error::Supervisor) }
    }

    /// Create a new [`TerminationReport`] with `origin` set to
    /// [`TerminationOrigin::Supervisor`] and `error` set to `None`.
    fn supervisor() -> Self {
        Self { origin: TerminationOrigin::Supervisor, error: None }
    }

    /// Check whether the termination is due to an expired sliding sync session.
    fn has_expired(&self) -> bool {
        match &self.error {
            Some(Error::RoomList(room_list_service::Error::SlidingSync(error)))
            | Some(Error::EncryptionSync(encryption_sync_service::Error::SlidingSync(error))) => {
                error.client_api_error_kind()
                    == Some(&ruma::api::client::error::ErrorKind::UnknownPos)
            }
            _ => false,
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

    /// Is the offline mode for the [`SyncService`] enabled?
    ///
    /// The offline mode is described in the [`State::Offline`] enum variant.
    with_offline_mode: bool,

    /// Whether to turn [`SlidingSyncBuilder::share_pos`] on or off.
    ///
    /// [`SlidingSyncBuilder::share_pos`]: matrix_sdk::sliding_sync::SlidingSyncBuilder::share_pos
    with_share_pos: bool,

    /// The parent tracing span to use for the tasks within this service.
    ///
    /// Normally this will be [`Span::none`], but it may be useful to assign a
    /// defined span, for example if there is more than one active sync
    /// service.
    parent_span: Span,
}

impl SyncServiceBuilder {
    fn new(client: Client) -> Self {
        Self {
            client,
            with_cross_process_lock: false,
            with_offline_mode: false,
            with_share_pos: true,
            parent_span: Span::none(),
        }
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

    /// Enable the "offline" mode for the [`SyncService`].
    ///
    /// To learn more about the "offline" mode read the documentation for the
    /// [`State::Offline`] enum variant.
    pub fn with_offline_mode(mut self) -> Self {
        self.with_offline_mode = true;
        self
    }

    /// Whether to turn [`SlidingSyncBuilder::share_pos`] on or off.
    ///
    /// [`SlidingSyncBuilder::share_pos`]: matrix_sdk::sliding_sync::SlidingSyncBuilder::share_pos
    pub fn with_share_pos(mut self, enable: bool) -> Self {
        self.with_share_pos = enable;
        self
    }

    /// Set the parent tracing span to be used for the tasks within this
    /// service.
    pub fn with_parent_span(mut self, parent_span: Span) -> Self {
        self.parent_span = parent_span;
        self
    }

    /// Finish setting up the [`SyncService`].
    ///
    /// This creates the underlying sliding syncs, and will *not* start them in
    /// the background. The resulting [`SyncService`] must be kept alive as long
    /// as the sliding syncs are supposed to run.
    pub async fn build(self) -> Result<SyncService, Error> {
        let Self {
            client,
            with_cross_process_lock,
            with_offline_mode,
            with_share_pos,
            parent_span,
        } = self;

        let encryption_sync_permit = Arc::new(AsyncMutex::new(EncryptionSyncPermit::new()));

        let room_list = RoomListService::new_with_share_pos(client.clone(), with_share_pos).await?;

        let encryption_sync = Arc::new(
            EncryptionSyncService::new(client, None, WithLocking::from(with_cross_process_lock))
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
                with_offline_mode,
                parent_span,
            })),
        })
    }
}

/// Errors for the [`SyncService`] API.
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
    Supervisor,
}
