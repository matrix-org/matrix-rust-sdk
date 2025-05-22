#[cfg(target_arch = "wasm32")]
use core::pin::Pin;

#[cfg(target_arch = "wasm32")]
use futures_core::{
    future::{FusedFuture, Future},
    task::{Context, Poll},
    FusedStream, Stream,
};
#[cfg(target_arch = "wasm32")]
pub use futures_util::stream::LocalBoxStream as BoxStream;
#[cfg(target_arch = "wasm32")]
use futures_util::stream::StreamExt as _;
#[cfg(not(target_arch = "wasm32"))]
pub use futures_util::{stream::BoxStream, StreamExt};

/// Future for the [`next`](super::StreamExt::next) method.
#[cfg(target_arch = "wasm32")]
#[derive(Debug)]
#[must_use = "futures do nothing unless you `.await` or poll them"]
pub struct Next<'a, St: ?Sized> {
    stream: &'a mut St,
}
#[cfg(target_arch = "wasm32")]
impl<St: ?Sized + Unpin> Unpin for Next<'_, St> {}

#[cfg(target_arch = "wasm32")]
impl<'a, St: ?Sized + Stream + Unpin> Next<'a, St> {
    pub(super) fn new(stream: &'a mut St) -> Self {
        Self { stream }
    }
}

#[cfg(target_arch = "wasm32")]
impl<St: ?Sized + FusedStream + Unpin> FusedFuture for Next<'_, St> {
    fn is_terminated(&self) -> bool {
        self.stream.is_terminated()
    }
}

#[cfg(target_arch = "wasm32")]
impl<St: ?Sized + Stream + Unpin> Future for Next<'_, St> {
    type Output = Option<St::Item>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.stream.poll_next_unpin(cx)
    }
}

// Just a helper function to ensure the futures we're returning all have the
// right implementations.
#[cfg(target_arch = "wasm32")]
pub(crate) fn assert_future<T, F>(future: F) -> F
where
    F: Future<Output = T>,
{
    future
}

#[cfg(target_arch = "wasm32")]
pub trait StreamExt: Stream {
    fn boxed<'a>(self) -> BoxStream<'a, Self::Item>
    where
        Self: Sized + 'a,
    {
        self.boxed_local()
    }

    fn next(&mut self) -> Next<'_, Self>
    where
        Self: Unpin,
    {
        assert_future::<Option<Self::Item>, _>(Next::new(self))
    }
}

#[cfg(target_arch = "wasm32")]
impl<T: ?Sized> StreamExt for T where T: Stream {}
