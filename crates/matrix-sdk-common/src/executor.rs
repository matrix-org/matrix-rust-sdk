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

//! Abstraction over an executor so we can spawn tasks under Wasm the same way
//! we do usually.

#[cfg(not(target_family = "wasm"))]
mod sys {
    pub use tokio::task::{spawn, AbortHandle, JoinError, JoinHandle};
}

#[cfg(target_family = "wasm")]
mod sys {
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll},
    };

    pub use futures_util::future::AbortHandle;
    use futures_util::{
        future::{Abortable, RemoteHandle},
        FutureExt,
    };

    #[derive(Debug)]
    pub enum JoinError {
        Cancelled,
        Panic,
    }

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

    impl std::fmt::Display for JoinError {
        fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match &self {
                JoinError::Cancelled => write!(fmt, "task was cancelled"),
                JoinError::Panic => write!(fmt, "task panicked"),
            }
        }
    }

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

    #[derive(Debug)]
    pub struct JoinHandle<T> {
        remote_handle: Option<RemoteHandle<T>>,
        the_abort_handle: AbortHandle,
    }

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

    impl<T> Drop for JoinHandle<T> {
        fn drop(&mut self) {
            // don't abort the spawned future
            if let Some(h) = self.remote_handle.take() {
                h.forget();
            }
        }
    }

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
}

#[cfg(not(target_family = "wasm"))]
mod runtime_types {
    /// A handle to a runtime for executing async tasks and futures.
    pub type Handle = tokio::runtime::Handle;
    pub type Runtime = tokio::runtime::Runtime;
}

#[cfg(target_family = "wasm")]
mod runtime_types {
    use super::*;
    use std::future::Future;

    /// A handle to a runtime for executing async tasks and futures.
    pub type Handle = WasmRuntimeHandle;
    pub type Runtime = WasmRuntimeHandle;

    #[derive(Debug)]
    /// A dummy guard that does nothing when dropped.
    /// This is used for the Wasm implementation to match tokio::runtime::EnterGuard.
    pub struct WasmRuntimeGuard;

    impl Drop for WasmRuntimeGuard {
        fn drop(&mut self) {
            // No-op, as there's no special context to exit in Wasm
        }
    }

    #[derive(Default, Debug)]
    /// A runtime handle implementation for WebAssembly targets.
    ///
    /// This implements a minimal subset of the tokio::runtime::Handle API
    /// that is needed for the matrix-rust-sdk to function on Wasm.
    pub struct WasmRuntimeHandle;

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
}

/// Get a runtime handle appropriate for the current target platform.
///
/// This function returns a unified `Handle` type that works across both
/// Wasm and non-Wasm platforms, allowing code to be written that is
/// agnostic to the platform-specific runtime implementation.
///
/// Returns:
/// - A `tokio::runtime::Handle` on non-Wasm platforms
/// - A `WasmRuntimeHandle` on Wasm platforms
pub fn get_runtime_handle() -> Handle {
    #[cfg(target_family = "wasm")]
    return Handle::default();

    #[cfg(not(target_family = "wasm"))]
    async_compat::get_runtime_handle()
}

pub use runtime_types::*;
pub use sys::*;

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
