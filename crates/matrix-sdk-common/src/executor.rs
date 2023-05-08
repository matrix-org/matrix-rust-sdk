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
use futures_util::{future::RemoteHandle, FutureExt};
#[cfg(not(target_arch = "wasm32"))]
pub use tokio::spawn;

#[cfg(target_arch = "wasm32")]
pub fn spawn<F, T>(future: F) -> JoinHandle<T>
where
    F: Future<Output = T> + 'static,
{
    let fut = future.unit_error();
    let (fut, handle) = fut.remote_handle();
    wasm_bindgen_futures::spawn_local(fut);

    JoinHandle { handle }
}

#[cfg(target_arch = "wasm32")]
#[derive(Debug)]
pub struct JoinHandle<T> {
    handle: RemoteHandle<Result<T, ()>>,
}

#[cfg(target_arch = "wasm32")]
impl<T: 'static> Future for JoinHandle<T> {
    type Output = Result<T, ()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.handle).poll(cx)
    }
}
