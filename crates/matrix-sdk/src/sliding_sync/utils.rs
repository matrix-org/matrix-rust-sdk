//! Moaaar features for Sliding Sync.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};

use tokio::task::{JoinError, JoinHandle};

/// Private type to ensure a task is aborted on drop.
pub(crate) struct AbortOnDrop<T>(JoinHandle<T>);

impl<T> AbortOnDrop<T> {
    fn new(join_handle: JoinHandle<T>) -> Self {
        Self(join_handle)
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl<T> Future for AbortOnDrop<T> {
    type Output = Result<T, JoinError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.0).poll(cx)
    }
}

/// Private trait to create a `AbortOnDrop` from a `JoinHandle`.
pub(crate) trait JoinHandleExt<T> {
    fn abort_on_drop(self) -> AbortOnDrop<T>;
}

impl<T> JoinHandleExt<T> for JoinHandle<T> {
    fn abort_on_drop(self) -> AbortOnDrop<T> {
        AbortOnDrop::new(self)
    }
}
