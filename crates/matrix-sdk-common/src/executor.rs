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

//! On non Wasm platforms, this re-exports parts of tokio directly.  For Wasm,
//! we wrap the n0-future package's AbortHandle + spawn method in order to
//! provide an interface that is similar to tokio's.

#[cfg(not(target_family = "wasm"))]
mod sys {
    pub use tokio::task::{spawn, AbortHandle, JoinError, JoinHandle};

    /// A handle to a runtime for executing async tasks and futures.
    pub type Handle = tokio::runtime::Handle;
    pub type Runtime = tokio::runtime::Runtime;
}

#[cfg(target_family = "wasm")]
mod sys {
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll},
    };

    pub use futures_util::future::AbortHandle;
    pub use n0_future::task::{spawn as n0_spawn, JoinError};

    /// Wrapper around n0-future's JoinHandle to provide tokio-compatible API
    /// The wrapper adds methods the abort, abort_handle, and is_finished
    /// methods, in order to have better conformity with the tokio equivalents.
    #[derive(Debug)]
    pub struct JoinHandle<T> {
        inner: n0_future::task::JoinHandle<Result<T, futures_util::future::Aborted>>,
        abort_handle: AbortHandle,
    }

    impl<T> JoinHandle<T> {
        pub fn abort(&self) {
            self.abort_handle.abort();
        }

        pub fn abort_handle(&self) -> AbortHandle {
            self.abort_handle.clone()
        }

        pub fn is_finished(&self) -> bool {
            self.abort_handle.is_aborted()
        }
    }

    impl<T> Future for JoinHandle<T> {
        type Output = Result<T, JoinError>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            match Pin::new(&mut self.inner).poll(cx) {
                Poll::Ready(Ok(Ok(value))) => Poll::Ready(Ok(value)),
                Poll::Ready(Ok(Err(_aborted))) => Poll::Ready(Err(JoinError::Cancelled)),
                Poll::Ready(Err(join_err)) => Poll::Ready(Err(join_err)),
                Poll::Pending => Poll::Pending,
            }
        }
    }

    pub fn spawn<F, T: 'static>(future: F) -> JoinHandle<T>
    where
        F: Future<Output = T> + 'static,
    {
        let (abortable_future, abort_handle) = futures_util::future::abortable(future);
        let inner = n0_spawn(abortable_future);
        JoinHandle { inner, abort_handle }
    }
}

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
