// Copyright 2025 The Matrix.org Foundation C.I.C.
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

//! Abstraction over a stream so we can use them in a Wasm or non-Wasm environment
//! interchangeably throughout the codebase.
#[cfg(not(target_family = "wasm"))]
mod sys {
    pub use futures_util::{stream::BoxStream, StreamExt};
}

#[cfg(target_family = "wasm")]
mod sys {
    use core::pin::Pin;
    use futures_core::{
        future::{FusedFuture, Future},
        task::{Context, Poll},
        FusedStream, Stream,
    };

    pub use futures_util::stream::LocalBoxStream as BoxStream;
    use futures_util::stream::StreamExt as _;

    /// Future for the [`next`](super::StreamExt::next) method.
    #[derive(Debug)]
    #[must_use = "futures do nothing unless you `.await` or poll them"]
    pub struct Next<'a, St: ?Sized> {
        stream: &'a mut St,
    }

    impl<St: ?Sized + Unpin> Unpin for Next<'_, St> {}

    impl<'a, St: ?Sized + Stream + Unpin> Next<'a, St> {
        pub(super) fn new(stream: &'a mut St) -> Self {
            Self { stream }
        }
    }

    impl<St: ?Sized + FusedStream + Unpin> FusedFuture for Next<'_, St> {
        fn is_terminated(&self) -> bool {
            self.stream.is_terminated()
        }
    }

    impl<St: ?Sized + Stream + Unpin> Future for Next<'_, St> {
        type Output = Option<St::Item>;

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.stream.poll_next_unpin(cx)
        }
    }

    // Just a helper function to ensure the futures we're returning all have the
    // right implementations.
    pub(crate) fn assert_future<T, F>(future: F) -> F
    where
        F: Future<Output = T>,
    {
        future
    }

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

    impl<T: ?Sized> StreamExt for T where T: Stream {}
}

pub use sys::*;
