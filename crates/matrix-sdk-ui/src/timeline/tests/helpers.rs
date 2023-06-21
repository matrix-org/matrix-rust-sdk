// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use std::sync::Arc;

use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use futures_core::Stream;
use futures_util::{FutureExt, StreamExt};
use ruma::EventId;

use crate::timeline::{EventTimelineItem, TimelineItem};

pub(super) async fn assert_event_is_updated(
    stream: &mut (impl Stream<Item = VectorDiff<Arc<TimelineItem>>> + Unpin),
    event_id: &EventId,
    _position: usize,
) -> EventTimelineItem {
    let event = assert_matches!(
        stream.next().await,
        Some(VectorDiff::Set { index: _position, value }) => value
    );
    let event = event.as_event().unwrap();
    assert_eq!(event.event_id().unwrap(), event_id);
    event.to_owned()
}

pub(super) async fn assert_no_more_updates(
    stream: &mut (impl Stream<Item = VectorDiff<Arc<TimelineItem>>> + Unpin),
) {
    assert!(stream.next().now_or_never().is_none())
}
