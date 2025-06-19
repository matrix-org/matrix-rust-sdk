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

//! Platform-specific stream utilities.
//!
//! This module provides a unified `BoxStream` + `StreamExt` class for working
//! with boxed streams across different platforms. On native platforms,
//! streams can be `Send`, but on Wasm they cannot. This module abstracts
//! over that difference.

#[cfg(not(target_family = "wasm"))]
mod sys {
    // On native platforms, just re-export everything from futures_util
    pub use futures_util::{StreamExt, stream::BoxStream};
}

#[cfg(target_family = "wasm")]
mod sys {
    use futures_core::Stream;
    // On Wasm, BoxStream is LocalBoxStream
    pub use futures_util::stream::LocalBoxStream as BoxStream;

    /// Custom `StreamExt` trait for Wasm that provides essential methods
    /// like `.boxed()` and `.next()` without `Send` requirements.
    pub trait StreamExt: Stream {
        /// Box this stream using `LocalBoxStream` (no `Send` requirement).
        fn boxed<'a>(self) -> BoxStream<'a, Self::Item>
        where
            Self: Sized + 'a,
        {
            futures_util::StreamExt::boxed_local(self)
        }

        /// Get the next item from this stream.
        fn next(&mut self) -> futures_util::stream::Next<'_, Self>
        where
            Self: Unpin,
        {
            futures_util::StreamExt::next(self)
        }
    }

    impl<S: Stream> StreamExt for S {}
}

pub use sys::*;
