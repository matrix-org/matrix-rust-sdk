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
pub use futures_util::future::Aborted as JoinError;
#[cfg(target_arch = "wasm32")]
use futures_util::{
    future::{AbortHandle, Abortable, RemoteHandle},
    FutureExt,
};
#[cfg(not(target_arch = "wasm32"))]
pub use tokio::task::{spawn, JoinError, JoinHandle};

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

    JoinHandle { remote_handle, abort_handle }
}

#[cfg(target_arch = "wasm32")]
#[derive(Debug)]
pub struct JoinHandle<T> {
    remote_handle: RemoteHandle<T>,
    abort_handle: AbortHandle,
}

#[cfg(target_arch = "wasm32")]
impl<T> JoinHandle<T> {
    pub fn abort(&self) {
        self.abort_handle.abort();
    }
}

#[cfg(target_arch = "wasm32")]
impl<T: 'static> Future for JoinHandle<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.abort_handle.is_aborted() {
            // The future has been aborted. It is not possible to poll it again.
            Poll::Ready(Err(JoinError))
        } else {
            Pin::new(&mut self.remote_handle).poll(cx).map(Ok)
        }
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use matrix_sdk_test::async_test;

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
