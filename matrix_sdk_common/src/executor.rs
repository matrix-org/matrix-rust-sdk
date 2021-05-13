//! Abstraction over an executor so we can spawn tasks under WASM the same way
//! we do usually.
#[cfg(target_arch = "wasm32")]
use std::{
    pin::Pin,
    task::{Context, Poll},
};

#[cfg(target_arch = "wasm32")]
use futures::{future::RemoteHandle, Future, FutureExt};
#[cfg(not(target_arch = "wasm32"))]
pub use tokio::spawn;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen_futures::spawn_local;

#[cfg(target_arch = "wasm32")]
pub fn spawn<F, T>(future: F) -> JoinHandle<T>
where
    F: Future<Output = T> + 'static,
{
    let fut = future.unit_error();
    let (fut, handle) = fut.remote_handle();
    spawn_local(fut);

    JoinHandle { handle }
}

#[cfg(target_arch = "wasm32")]
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
