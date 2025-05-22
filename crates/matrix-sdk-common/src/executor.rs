// Copyright 2021 The Matrix.org Foundation C.I.C.
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

//! Abstraction over an executor so we can spawn tasks under WASM the same way
//! we do usually.

#[cfg(target_arch = "wasm32")]
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

#[cfg(target_arch = "wasm32")]
pub use futures_util::future::AbortHandle;
#[cfg(target_arch = "wasm32")]
use futures_util::{
    future::{Abortable, RemoteHandle},
    FutureExt,
};
#[cfg(not(target_arch = "wasm32"))]
pub use tokio::task::{spawn, AbortHandle, JoinError, JoinHandle};

#[cfg(target_arch = "wasm32")]
#[derive(Debug)]
pub enum JoinError {
    Cancelled,
    Panic,
}
#[cfg(target_arch = "wasm32")]
impl JoinError {
    /// Returns true if the error was caused by the task being cancelled.
    ///
    /// See [the module level docs] for more information on cancellation.
    ///
    /// [the module level docs]: crate::task#cancellation
    pub fn is_cancelled(&self) -> bool {
        matches!(self, JoinError::Cancelled)
    }
}
#[cfg(target_arch = "wasm32")]
impl std::fmt::Display for JoinError {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self {
            JoinError::Cancelled => write!(fmt, "task was cancelled"),
            JoinError::Panic => write!(fmt, "task panicked"),
        }
    }
}
#[cfg(target_arch = "wasm32")]
pub fn spawn<F, T>(future: F) -> JoinHandle<T>
where
    F: Future<Output = T> + 'static,
{
    let (future, remote_handle) = future.remote_handle();
    let (abort_handle, abort_registration) = AbortHandle::new_pair();
    let future = Abortable::new(future, abort_registration);

    wasm_bindgen_futures::spawn_local(async {
        // Poll the future, and ignore the result (either it's `Ok(())`, or it's
        // `Err(Aborted)`).
        let _ = future.await;
    });

    JoinHandle { remote_handle: Some(remote_handle), the_abort_handle: abort_handle }
}

#[cfg(target_arch = "wasm32")]
#[derive(Debug)]
pub struct JoinHandle<T> {
    remote_handle: Option<RemoteHandle<T>>,
    the_abort_handle: AbortHandle,
}

#[cfg(target_arch = "wasm32")]
impl<T> JoinHandle<T> {
    pub fn abort(&self) {
        self.the_abort_handle.abort();
    }

    pub fn abort_handle(&self) -> AbortHandle {
        self.the_abort_handle.clone()
    }

    pub fn is_finished(&self) -> bool {
        self.the_abort_handle.is_aborted()
    }
}

#[cfg(target_arch = "wasm32")]
impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        // don't abort the spawned future
        if let Some(h) = self.remote_handle.take() {
            h.forget();
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl<T: 'static> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.the_abort_handle.is_aborted() {
            // The future has been aborted. It is not possible to poll it again.
            Poll::Ready(Err(JoinError::Cancelled))
        } else if let Some(handle) = self.remote_handle.as_mut() {
            Pin::new(handle).poll(cx).map(Ok)
        } else {
            Poll::Ready(Err(JoinError::Panic))
        }
    }
}

#[cfg(target_arch = "wasm32")]
use futures_executor;

/// A handle to a runtime for executing async tasks and futures.
///
/// This is a unified type that represents either:
/// - A `tokio::runtime::Handle` on non-WASM platforms
/// - A `WasmRuntimeHandle` on WASM platforms
///
/// This abstraction allows code to run on both WASM and non-WASM platforms
/// without conditional compilation.
#[cfg(not(target_arch = "wasm32"))]
pub type Handle = tokio::runtime::Handle;
#[cfg(not(target_arch = "wasm32"))]
pub type Runtime = tokio::runtime::Runtime;

#[cfg(target_arch = "wasm32")]
pub type Handle = WasmRuntimeHandle;
#[cfg(target_arch = "wasm32")]
pub type Runtime = WasmRuntimeHandle;

#[cfg(target_arch = "wasm32")]
#[derive(Debug)]
/// A dummy guard that does nothing when dropped.
/// This is used for the WASM implementation to match
/// tokio::runtime::EnterGuard.
pub struct WasmRuntimeGuard;

#[cfg(target_arch = "wasm32")]
impl Drop for WasmRuntimeGuard {
    fn drop(&mut self) {
        // No-op, as there's no special context to exit in WASM
    }
}

#[cfg(target_arch = "wasm32")]
#[derive(Default, Debug)]
/// A runtime handle implementation for WebAssembly targets.
///
/// This implements a minimal subset of the tokio::runtime::Handle API
/// that is needed for the matrix-rust-sdk to function on WASM.
pub struct WasmRuntimeHandle;

#[cfg(target_arch = "wasm32")]
impl WasmRuntimeHandle {
    /// Spawns a future in the wasm32 bindgen runtime.
    #[track_caller]
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + 'static,
        F::Output: 'static,
    {
        spawn(future)
    }

    /// Runs the provided function on an executor dedicated to blocking
    /// operations.
    #[track_caller]
    pub fn spawn_blocking<F, R>(&self, func: F) -> JoinHandle<R>
    where
        F: FnOnce() -> R + 'static,
        R: 'static,
    {
        spawn(async move { func() })
    }

    /// Runs a future to completion on the current thread.
    pub fn block_on<F, T>(&self, future: F) -> T
    where
        F: Future<Output = T>,
    {
        futures_executor::block_on(future)
    }

    /// Enters the runtime context.
    ///
    /// For WebAssembly, this is a no-op that returns a dummy guard.
    pub fn enter(&self) -> WasmRuntimeGuard {
        WasmRuntimeGuard
    }
}

/// Get a runtime handle appropriate for the current target platform.
///
/// This function returns a unified `Handle` type that works across both
/// WASM and non-WASM platforms, allowing code to be written that is
/// agnostic to the platform-specific runtime implementation.
///
/// Returns:
/// - A `tokio::runtime::Handle` on non-WASM platforms
/// - A `WasmRuntimeHandle` on WASM platforms
pub fn get_runtime_handle() -> Handle {
    #[cfg(target_arch = "wasm32")]
    {
        WasmRuntimeHandle
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        async_compat::get_runtime_handle()
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use matrix_sdk_test_macros::async_test;

    use super::spawn;

    #[async_test]
    async fn test_spawn() {
        let future = async { 42 };
        let join_handle = spawn(future);

        assert_matches!(join_handle.await, Ok(42));
    }

    #[async_test]
    async fn test_abort() {
        let future = async { 42 };
        let join_handle = spawn(future);

        join_handle.abort();

        assert!(join_handle.await.is_err());
    }
}
