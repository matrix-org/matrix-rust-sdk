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

use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use futures_core::Stream;
use pin_project_lite::pin_project;

use super::TimelineDropHandle;

pin_project! {
    /// A stream that wraps a [`TimelineDropHandle`] so that the `Timeline`
    /// isn't dropped until the `Stream` is dropped.
    pub(super) struct TimelineWithDropHandle<S> {
        #[pin]
        inner: S,
        drop_handle: Arc<TimelineDropHandle>,
    }
}

impl<S> TimelineWithDropHandle<S> {
    /// Create a new [`WithTimelineDropHandle`].
    pub(super) fn new(inner: S, drop_handle: Arc<TimelineDropHandle>) -> Self {
        Self { inner, drop_handle }
    }
}

impl<S> Stream for TimelineWithDropHandle<S>
where
    S: Stream,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.project().inner.poll_next(cx)
    }
}
