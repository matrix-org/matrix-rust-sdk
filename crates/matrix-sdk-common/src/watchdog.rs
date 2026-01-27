// Copyright 2026 The Matrix.org Foundation C.I.C.
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
// See the License for the specific language governing permissions and
// limitations under the License.

//! This module provides a [`TaskMonitor`] for spawning and monitoring
//! long-running background tasks. Tasks spawned through the monitor are
//! monitored for panics, errors, and unexpected termination.
//!
//! ```no_run
//! use matrix_sdk_common::watchdog::TaskMonitor;
//!
//! let monitor = TaskMonitor::new();
//!
//! // Subscribe to failure notifications
//! let mut failures = monitor.subscribe();
//!
//! // Spawn a monitored background task
//! let handle = monitor.spawn_background_task("my_task", async {
//!     loop {
//!         // Do background work...
//!         matrix_sdk_common::sleep::sleep(std::time::Duration::from_secs(1))
//!             .await;
//!     }
//! });
//!
//! // Listen for failures in another task
//! // while let Ok(failure) = failures.recv().await {
//! //     eprintln!("Task {} failed: {:?}", failure.task.name, failure.reason);
//! // }
//! ```
//!
//! ## WebAssembly (WASM) support
//!
//! Unfortunately, safe unwinding isn't supported on most WASM targets, as of
//! 2026-01-28, so panics in monitored tasks cannot be caught and reported.
//! Instead, a panic in a monitored task may throw a JS exception. The rest of
//! the monitoring features (error reporting, early termination)
//! is still functional, though.

use std::{
    any::Any,
    collections::HashMap,
    future::Future,
    panic::AssertUnwindSafe,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use futures_util::FutureExt;
use tokio::sync::broadcast;

use crate::{
    SendOutsideWasm,
    executor::{AbortHandle, spawn},
    locks::RwLock,
};

/// Unique identifier for a background task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskId(u64);

impl TaskId {
    /// Create a new unique task ID, by incrementing a global counter.
    fn new() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        Self(NEXT_ID.fetch_add(1, Ordering::SeqCst))
    }
}

impl std::fmt::Display for TaskId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "TaskId({})", self.0)
    }
}

/// Metadata about a spawned background task.
#[derive(Debug, Clone)]
pub struct BackgroundTaskInfo {
    /// Unique identifier for this task.
    pub id: TaskId,

    /// Human-readable name for the task, as defined when spawning it.
    pub name: String,
}

/// Reason why a background task failed.
#[derive(Debug, Clone)]
pub enum BackgroundTaskFailureReason {
    /// The task panicked.
    Panic {
        /// The panic message, if it could be extracted.
        message: Option<String>,
        /// Backtrace captured after the panic (if available).
        panic_backtrace: Option<String>,
    },

    /// The task returned an error.
    Error {
        /// String representation of the error.
        // TODO(bnjbvr): consider storing a boxed error instead?
        error: String,
    },

    /// The task ended unexpectedly (for tasks expected to run forever).
    EarlyTermination,
}

/// A report of a background task failure.
///
/// This is sent through the broadcast channel when a monitored task fails.
#[derive(Debug, Clone)]
pub struct BackgroundTaskFailure {
    /// Information about the task that failed.
    pub task: BackgroundTaskInfo,

    /// Why the task failed.
    pub reason: BackgroundTaskFailureReason,
}

/// Internal entry for tracking an active task.
#[derive(Debug)]
struct ActiveTask {
    /// The tokio's handle to preemptively abort the task.
    // TODO: might be useful to abort on drop?
    _abort_handle: AbortHandle,
}

/// Default capacity for the failure broadcast channel.
///
/// It doesn't have to be large, because it's expected that consumers of such a
/// failure report would likely stop execution of the SDK or take immediate
/// corrective action, and that failures should be rare.
const FAILURE_CHANNEL_CAPACITY: usize = 8;

/// A monitor for spawning and monitoring background tasks.
///
/// The [`TaskMonitor`] allows you to spawn background tasks that are
/// automatically monitored for panics, errors, and unexpected termination.
/// In such cases, a [`BackgroundTaskFailure`] is sent through a broadcast
/// channel that subscribers can listen to.
///
/// # Example
///
/// ```no_run
/// use matrix_sdk_common::watchdog::TaskMonitor;
///
/// let monitor = TaskMonitor::new();
///
/// // Subscribe to failures
/// let mut failures = monitor.subscribe();
///
/// // Spawn a task that runs indefinitely
/// let _handle = monitor.spawn_background_task("worker", async {
///     loop {
///         // Do work...
///         matrix_sdk_common::sleep::sleep(std::time::Duration::from_secs(1))
///             .await;
///     }
/// });
/// ```
#[derive(Debug)]
pub struct TaskMonitor {
    /// Sender for failure notifications.
    failure_sender: broadcast::Sender<BackgroundTaskFailure>,

    /// Map of active tasks by ID.
    active_tasks: Arc<RwLock<HashMap<TaskId, ActiveTask>>>,
}

impl Default for TaskMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl TaskMonitor {
    /// Create a new task monitor.
    pub fn new() -> Self {
        let (failure_sender, _) = broadcast::channel(FAILURE_CHANNEL_CAPACITY);
        Self { failure_sender, active_tasks: Default::default() }
    }

    /// Subscribe to failure notifications.
    ///
    /// Returns a broadcast receiver that will receive [`BackgroundTaskFailure`]
    /// messages whenever a monitored task fails.
    ///
    /// Note: If the receiver falls behind, older messages may be dropped.
    pub fn subscribe(&self) -> broadcast::Receiver<BackgroundTaskFailure> {
        self.failure_sender.subscribe()
    }

    /// Spawn a background task that is expected to run indefinitely.
    ///
    /// If the task completes (whether successfully or by panicking), it will be
    /// reported as a [`BackgroundTaskFailure`] report through the broadcast
    /// channel.
    ///
    /// Use this for long-running tasks like event loops, sync tasks, or
    /// background workers that should never complete under normal
    /// operation.
    ///
    /// # Arguments
    ///
    /// * `name` - A human-readable name for the task (for debugging purposes).
    /// * `future` - The async task to run.
    ///
    /// # Returns
    ///
    /// A [`BackgroundTaskHandle`] that can be used to abort the task or check
    /// if it has finished. This is the equivalent of tokio's `JoinHandle`.
    pub fn spawn_background_task<F>(
        &self,
        name: impl Into<String>,
        future: F,
    ) -> BackgroundTaskHandle
    where
        F: Future<Output = ()> + SendOutsideWasm + 'static,
    {
        let name = name.into();
        let task_id = TaskId::new();
        let task_info = BackgroundTaskInfo { id: task_id, name };

        let intentionally_aborted = Arc::new(AtomicBool::new(false));

        let active_tasks = self.active_tasks.clone();
        let failure_sender = self.failure_sender.clone();
        let aborted_flag = intentionally_aborted.clone();

        let wrapped = async move {
            let result = AssertUnwindSafe(future).catch_unwind().await;

            // Remove the task from the list of active ones.
            active_tasks.write().remove(&task_id);

            // Don't report if intentionally aborted.
            if aborted_flag.load(Ordering::Acquire) {
                return;
            }

            let failure_reason = match result {
                Ok(()) => {
                    // The task ended, this is considered an early termination.
                    BackgroundTaskFailureReason::EarlyTermination
                }

                Err(panic_payload) => BackgroundTaskFailureReason::Panic {
                    message: extract_panic_message(&panic_payload),
                    panic_backtrace: capture_backtrace(),
                },
            };

            let failure = BackgroundTaskFailure { task: task_info, reason: failure_reason };

            // Forward failure to observers (ignore if there's none).
            let _ = failure_sender.send(failure);
        };

        let join_handle = spawn(wrapped);
        let abort_handle = join_handle.abort_handle();

        // Register the task.
        self.active_tasks
            .write()
            .insert(task_id, ActiveTask { _abort_handle: abort_handle.clone() });

        BackgroundTaskHandle { abort_handle, intentionally_aborted }
    }

    /// Spawn a background task that returns a `Result`.
    ///
    /// The task is monitored for panics and errors; see also
    /// [`BackgroundTaskFailure`].
    ///
    /// If the task returns `Ok(())`, it is considered successful and no failure
    /// is reported.
    ///
    /// # Arguments
    ///
    /// * `name` - A human-readable name for the task (for debugging purposes).
    /// * `future` - The async task to run.
    ///
    /// # Returns
    ///
    /// A [`BackgroundTaskHandle`] that can be used to abort the task or check
    /// if it has finished. This is the equivalent of tokio's `JoinHandle`.
    pub fn spawn_fallible_task<F, E>(
        &self,
        name: impl Into<String>,
        future: F,
    ) -> BackgroundTaskHandle
    where
        F: Future<Output = Result<(), E>> + SendOutsideWasm + 'static,
        E: std::error::Error + SendOutsideWasm + 'static,
    {
        let name = name.into();
        let task_id = TaskId::new();
        let task_info = BackgroundTaskInfo { id: task_id, name };

        let intentionally_aborted = Arc::new(AtomicBool::new(false));

        let active_tasks = self.active_tasks.clone();
        let failure_sender = self.failure_sender.clone();
        let aborted_flag = intentionally_aborted.clone();

        let wrapped = async move {
            let result = AssertUnwindSafe(future).catch_unwind().await;

            active_tasks.write().remove(&task_id);

            // Don't report if intentionally aborted.
            if aborted_flag.load(Ordering::Acquire) {
                return;
            }

            let failure_reason = match result {
                Ok(Ok(())) => {
                    // The task ended successfully, no failure to report.
                    return;
                }

                Ok(Err(e)) => BackgroundTaskFailureReason::Error { error: e.to_string() },

                Err(panic_payload) => BackgroundTaskFailureReason::Panic {
                    message: extract_panic_message(&panic_payload),
                    panic_backtrace: capture_backtrace(),
                },
            };

            // Send failure (ignore if no receivers).
            let _ = failure_sender
                .send(BackgroundTaskFailure { task: task_info, reason: failure_reason });
        };

        let join_handle = spawn(wrapped);
        let abort_handle = join_handle.abort_handle();

        // Register the task.
        self.active_tasks
            .write()
            .insert(task_id, ActiveTask { _abort_handle: abort_handle.clone() });

        BackgroundTaskHandle { abort_handle, intentionally_aborted }
    }
}

/// A handle to a spawned background task.
///
/// This handle can be used to abort the task or check if it has finished.
/// When aborted through this handle, the task will NOT be reported as a
/// failure.
#[derive(Debug)]
pub struct BackgroundTaskHandle {
    /// The underlying tokio's [`AbortHandle`].
    abort_handle: AbortHandle,

    /// An additional flag to indicate if the task was intentionally aborted, so
    /// we don't report it as a failure when that happens.
    intentionally_aborted: Arc<AtomicBool>,
}

impl BackgroundTaskHandle {
    /// Abort the task.
    ///
    /// The task will be stopped and will NOT be reported as a failure
    /// (this is considered intentional termination).
    pub fn abort(&self) {
        // Note: ordering matters here, we set the flag before aborting otherwise
        // there's a possible race condition where the abort() is observed
        // before the flag is set, and the task monitor would consider this an
        // unexpected termination.
        self.intentionally_aborted.store(true, Ordering::Release);
        self.abort_handle.abort();
    }

    /// Check if the task has finished.
    ///
    /// Returns `true` if the task completed, panicked, or was aborted on
    /// non-wasm; on wasm, returns whether the task has been aborted only
    /// (due to lack of better APIs).
    pub fn is_finished(&self) -> bool {
        #[cfg(not(target_family = "wasm"))]
        {
            self.abort_handle.is_finished()
        }
        #[cfg(target_family = "wasm")]
        {
            self.abort_handle.is_aborted()
        }
    }
}

/// Capture a backtrace at the current location.
///
/// Returns `None` if backtraces are not enabled or not available.
#[cfg(not(target_family = "wasm"))]
fn capture_backtrace() -> Option<String> {
    use std::backtrace::{Backtrace, BacktraceStatus};

    let bt = Backtrace::capture();
    if bt.status() == BacktraceStatus::Captured { Some(bt.to_string()) } else { None }
}

/// Capture a backtrace - WASM version (backtraces not typically available).
#[cfg(target_family = "wasm")]
fn capture_backtrace() -> Option<String> {
    None
}

/// Extract a message from a panic payload.
fn extract_panic_message(payload: &Box<dyn Any + Send>) -> Option<String> {
    if let Some(s) = payload.downcast_ref::<&str>() {
        Some((*s).to_owned())
    } else {
        payload.downcast_ref::<String>().cloned()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use assert_matches::assert_matches;
    use matrix_sdk_test_macros::async_test;

    use super::{BackgroundTaskFailureReason, TaskMonitor};
    use crate::{sleep::sleep, timeout::timeout};

    #[async_test]
    async fn test_early_termination_is_reported() {
        let monitor = TaskMonitor::new();
        let mut failures = monitor.subscribe();

        // Spawn a task that completes immediately.
        let _handle = monitor.spawn_background_task("test_task", async {
            // Completes immediately: this is an "early termination".
        });

        // Should receive an early termination failure.
        let failure = timeout(failures.recv(), Duration::from_secs(1))
            .await
            .expect("timeout waiting for failure")
            .expect("channel closed");

        assert_eq!(failure.task.name, "test_task");
        assert_matches!(failure.reason, BackgroundTaskFailureReason::EarlyTermination);
    }

    #[async_test]
    #[cfg(not(target_family = "wasm"))] // Unfortunately, safe unwinding doesn't work on wasm.
    async fn test_panic_is_captured() {
        let monitor = TaskMonitor::new();
        let mut failures = monitor.subscribe();

        // Spawn a task that panics.
        let _handle = monitor.spawn_background_task("panicking_task", async {
            panic!("test panic message");
        });

        // Should receive a panic failure.
        let failure = timeout(failures.recv(), Duration::from_secs(1))
            .await
            .expect("timeout waiting for failure")
            .expect("channel closed");

        assert_eq!(failure.task.name, "panicking_task");
        assert_matches!(
            failure.reason,
            BackgroundTaskFailureReason::Panic { message, .. } => {
                assert_eq!(message.as_deref(), Some("test panic message"));
            }
        );
    }

    #[async_test]
    async fn test_error_is_captured() {
        let monitor = TaskMonitor::new();
        let mut failures = monitor.subscribe();

        // Spawn a fallible task that returns an error.
        let _handle = monitor.spawn_fallible_task("fallible_task", async {
            Err::<(), _>(std::io::Error::other("test error message"))
        });

        // Should receive an error failure.
        let failure = timeout(failures.recv(), Duration::from_secs(1))
            .await
            .expect("timeout waiting for failure")
            .expect("channel closed");

        assert_eq!(failure.task.name, "fallible_task");
        assert_matches!(
            failure.reason,
            BackgroundTaskFailureReason::Error { error } => {
                assert!(error.contains("test error message"));
            }
        );
    }

    #[async_test]
    async fn test_successful_fallible_task_no_failure() {
        let monitor = TaskMonitor::new();
        let mut failures = monitor.subscribe();

        // Spawn a fallible task that succeeds.
        let _handle =
            monitor.spawn_fallible_task("success_task", async { Ok::<(), std::io::Error>(()) });

        // Should NOT receive any failure: use a short timeout.
        let result = timeout(failures.recv(), Duration::from_millis(100)).await;
        assert!(result.is_err(), "should timeout, no failure expected");
    }

    #[async_test]
    async fn test_abort_does_not_report_failure() {
        let monitor = TaskMonitor::new();
        let mut failures = monitor.subscribe();

        // Spawn a long-running task.
        let handle = monitor.spawn_background_task("aborted_task", async {
            loop {
                sleep(Duration::from_secs(10)).await;
            }
        });

        // Give the task time to start.
        sleep(Duration::from_millis(10)).await;

        // Abort it.
        handle.abort();

        // Should NOT receive a failure for intentional abort.
        let result = timeout(failures.recv(), Duration::from_millis(100)).await;
        assert!(result.is_err(), "should timeout, no failure expected for abort");

        assert!(handle.is_finished(), "task should be finished after abort");
    }
}
