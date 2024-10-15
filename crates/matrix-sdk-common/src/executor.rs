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
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

#[cfg(target_arch = "wasm32")]
pub use futures_util::future::Aborted as JoinError;
#[cfg(target_arch = "wasm32")]
use futures_util::future::{AbortHandle, Abortable, RemoteHandle};
use futures_util::FutureExt;
#[cfg(not(target_arch = "wasm32"))]
pub use tokio::task::{spawn, JoinError, JoinHandle};

/// A `Box::pin` future that is `Send` on non-wasm, and without `Send` on wasm.
#[cfg(target_arch = "wasm32")]
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;
#[cfg(not(target_arch = "wasm32"))]
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

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

pub trait BoxFutureExt<'a, T: 'a> {
    fn box_future(self) -> BoxFuture<'a, T>;
}

#[cfg(not(target_arch = "wasm32"))]
impl<'a, F, T> BoxFutureExt<'a, T> for F
where
    F: Future<Output = T> + 'a + Send,
    T: 'a,
{
    fn box_future(self) -> BoxFuture<'a, T> {
        self.boxed()
    }
}

#[cfg(target_arch = "wasm32")]
impl<'a, F, T> BoxFutureExt<'a, T> for F
where
    F: Future<Output = T> + 'a,
    T: 'a,
{
    fn box_future(self) -> BoxFuture<'a, T> {
        self.boxed_local()
    }
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

#[cfg(not(target_arch = "wasm32"))]
impl<T> Future for AbortOnDrop<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

#[cfg(target_arch = "wasm32")]
impl<T: 'static> Future for AbortOnDrop<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if self.0.abort_handle.is_aborted() {
            // The future has been aborted. It is not possible to poll it again.
            Poll::Ready(Err(JoinError))
        } else {
            Pin::new(&mut self.0.remote_handle).poll(cx).map(Ok)
        }
    }
}

/// Trait to create a `AbortOnDrop` from a `JoinHandle`.
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
