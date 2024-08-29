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

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use chrono::{Datelike, Local, TimeZone};
use eyeball_im::VectorDiff;
use matrix_sdk_test::{async_test, ALICE, BOB};
use ruma::{
    event_id,
    events::{room::message::RoomMessageEventContent, AnyMessageLikeEventContent},
};
use stream_assert::assert_next_matches;

use super::TestTimeline;
use crate::timeline::{TimelineItemKind, VirtualTimelineItem};

#[async_test]
async fn test_day_divider() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;

    timeline
        .handle_live_event(f.text_msg("This is a first message on the first day").sender(*ALICE))
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    item.as_event().unwrap();

    let day_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert_let!(VirtualTimelineItem::DayDivider(ts) = day_divider.as_virtual().unwrap());
    let date = Local.timestamp_millis_opt(ts.0.into()).single().unwrap();
    assert_eq!(date.year(), 1970);
    assert_eq!(date.month(), 1);
    assert_eq!(date.day(), 1);

    timeline
        .handle_live_event(f.text_msg("This is a second message on the first day").sender(*ALICE))
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    item.as_event().unwrap();

    // Timestamps start at unix epoch, advance to one day later
    f.set_next_ts(24 * 60 * 60 * 1000);

    timeline
        .handle_live_event(f.text_msg("This is a first message on the next day").sender(*ALICE))
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    item.as_event().unwrap();

    let day_divider = assert_next_matches!(stream, VectorDiff::Insert { index: 3, value } => value);
    assert_let!(VirtualTimelineItem::DayDivider(ts) = day_divider.as_virtual().unwrap());
    let date = Local.timestamp_millis_opt(ts.0.into()).single().unwrap();
    assert_eq!(date.year(), 1970);
    assert_eq!(date.month(), 1);
    assert_eq!(date.day(), 2);

    let _ = timeline
        .handle_local_event(AnyMessageLikeEventContent::RoomMessage(
            RoomMessageEventContent::text_plain("A message I'm sending just now"),
        ))
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    item.as_event().unwrap();

    // The other events are in the past so a local event always creates a new day
    // divider.
    let day_divider = assert_next_matches!(stream, VectorDiff::Insert { index: 5, value } => value);
    assert_matches!(day_divider.as_virtual().unwrap(), VirtualTimelineItem::DayDivider { .. });
}

#[async_test]
async fn test_update_read_marker() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;

    timeline.handle_live_event(f.text_msg("A").sender(&ALICE)).await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let first_event_id = item.as_event().unwrap().event_id().unwrap().to_owned();

    let day_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(day_divider.is_day_divider());

    timeline.controller.handle_fully_read_marker(first_event_id.clone()).await;

    // Nothing should happen, the marker cannot be added at the end.

    timeline.handle_live_event(f.text_msg("B").sender(&BOB)).await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let second_event_id = item.as_event().unwrap().event_id().unwrap().to_owned();

    // Now the read marker appears after the first event.
    let item = assert_next_matches!(stream, VectorDiff::Insert { index: 2, value } => value);
    assert_matches!(item.as_virtual(), Some(VirtualTimelineItem::ReadMarker));

    timeline.controller.handle_fully_read_marker(second_event_id.clone()).await;

    // The read marker is removed but not reinserted, because it cannot be added at
    // the end.
    assert_next_matches!(stream, VectorDiff::Remove { index: 2 });

    timeline.handle_live_event(f.text_msg("C").sender(&ALICE)).await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let third_event_id = item.as_event().unwrap().event_id().unwrap().to_owned();

    // Now the read marker is reinserted after the second event.
    let marker = assert_next_matches!(stream, VectorDiff::Insert { index: 3, value } => value);
    assert_matches!(marker.kind, TimelineItemKind::Virtual(VirtualTimelineItem::ReadMarker));

    // Nothing should happen if the fully read event is set back to an older event.
    timeline.controller.handle_fully_read_marker(first_event_id).await;

    // Nothing should happen if the fully read event isn't found.
    timeline.controller.handle_fully_read_marker(event_id!("$fake_event_id").to_owned()).await;

    // Nothing should happen if the fully read event is referring to an event
    // that has already been marked as fully read.
    timeline.controller.handle_fully_read_marker(second_event_id).await;

    timeline.handle_live_event(f.text_msg("D").sender(&ALICE)).await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    item.as_event().unwrap();

    timeline.controller.handle_fully_read_marker(third_event_id).await;

    // The read marker is moved after the third event.
    assert_next_matches!(stream, VectorDiff::Remove { index: 3 });
    let marker = assert_next_matches!(stream, VectorDiff::Insert { index: 4, value } => value);
    assert_matches!(marker.kind, TimelineItemKind::Virtual(VirtualTimelineItem::ReadMarker));
}
