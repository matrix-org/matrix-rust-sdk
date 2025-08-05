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
//!
//! On non Wasm platforms, this re-exports parts of tokio directly.  For Wasm,
//! we provide a single-threaded solution that matches the interface that tokio
//! provides as a drop in replacement.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

#[cfg(not(target_family = "wasm"))]
mod sys {
    pub use tokio::{
        runtime::{Handle, Runtime},
        task::{AbortHandle, JoinError, JoinHandle, spawn},
    };
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
        FutureExt,
        future::{Abortable, RemoteHandle},
    };

    /// A Wasm specific version of `tokio::task::JoinError` designed to work
    /// in the single-threaded environment available in Wasm environments.
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

    /// A Wasm specific version of `tokio::task::JoinHandle` that
    /// holds handles to locally executing futures.
    #[derive(Debug)]
    pub struct JoinHandle<T> {
        remote_handle: Option<RemoteHandle<T>>,
        abort_handle: AbortHandle,
    }

    impl<T> JoinHandle<T> {
        /// Aborts the spawned future, preventing it from being polled again.
        pub fn abort(&self) {
            self.abort_handle.abort();
        }

        /// Returns the handle to the `AbortHandle` that can be used to
        /// abort the spawned future.
        pub fn abort_handle(&self) -> AbortHandle {
            self.abort_handle.clone()
        }

        /// Returns true if the spawned future has been aborted.
        pub fn is_finished(&self) -> bool {
            self.abort_handle.is_aborted()
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
            if self.abort_handle.is_aborted() {
                // The future has been aborted. It is not possible to poll it again.
                Poll::Ready(Err(JoinError::Cancelled))
            } else if let Some(handle) = self.remote_handle.as_mut() {
                Pin::new(handle).poll(cx).map(Ok)
            } else {
                Poll::Ready(Err(JoinError::Panic))
            }
        }
    }

    /// A Wasm specific version of `tokio::task::spawn` that utilizes
    /// wasm_bindgen_futures to spawn futures on the local executor.
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

        JoinHandle { remote_handle: Some(remote_handle), abort_handle }
    }
}

pub use sys::*;

/// A type ensuring a task is aborted on drop.
#[derive(Debug)]
pub struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> AbortOnDrop<T> {
    pub fn new(join_handle: JoinHandle<T>) -> Self {
        Self(join_handle)
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl<T: 'static> Future for AbortOnDrop<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(context)
    }
}

/// Trait to create an [`AbortOnDrop`] from a [`JoinHandle`].
pub trait JoinHandleExt<T> {
    fn abort_on_drop(self) -> AbortOnDrop<T>;
}

impl<T> JoinHandleExt<T> for JoinHandle<T> {
    fn abort_on_drop(self) -> AbortOnDrop<T> {
        AbortOnDrop::new(self)
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
