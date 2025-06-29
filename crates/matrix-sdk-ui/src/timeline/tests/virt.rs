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
use chrono::{Datelike, TimeZone, Utc};
use eyeball_im::VectorDiff;
use futures_util::{FutureExt, StreamExt as _};
use matrix_sdk_test::{ALICE, BOB, async_test};
use ruma::{
    event_id,
    events::{AnyMessageLikeEventContent, room::message::RoomMessageEventContent},
};
use stream_assert::assert_next_matches;

use super::TestTimeline;
use crate::timeline::{VirtualTimelineItem, traits::RoomDataProvider as _};

#[async_test]
async fn test_date_divider() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;

    timeline
        .handle_live_event(f.text_msg("This is a first message on the first day").sender(*ALICE))
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    item.as_event().unwrap();

    let date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert_let!(VirtualTimelineItem::DateDivider(ts) = date_divider.as_virtual().unwrap());
    let date = Utc.timestamp_millis_opt(ts.0.into()).single().unwrap();
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

    let date_divider =
        assert_next_matches!(stream, VectorDiff::Insert { index: 3, value } => value);
    assert_let!(VirtualTimelineItem::DateDivider(ts) = date_divider.as_virtual().unwrap());
    let date = Utc.timestamp_millis_opt(ts.0.into()).single().unwrap();
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

    // The other events are in the past so a local event always creates a new date
    // divider.
    let date_divider =
        assert_next_matches!(stream, VectorDiff::Insert { index: 5, value } => value);
    assert!(date_divider.is_date_divider());
}

#[async_test]
async fn test_update_read_marker() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    let own_user = timeline.controller.room_data_provider.own_user_id().to_owned();

    let f = &timeline.factory;
    timeline.handle_live_event(f.text_msg("A").sender(&own_user)).await;

    // Timeline: [A].
    // No read marker.
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_id1 = item.as_event().unwrap().event_id().unwrap().to_owned();

    // Timeline: [date-divider, A].
    let date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(date_divider.is_date_divider());

    timeline.controller.handle_fully_read_marker(event_id1.clone()).await;

    // Nothing should happen, the marker cannot be added at the end.
    // Timeline: [A].
    //            ^-- fully read
    assert!(stream.next().now_or_never().is_none());

    // Timeline: [date-divider, A, B].
    timeline.handle_live_event(f.text_msg("B").sender(&BOB)).await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_id2 = item.as_event().unwrap().event_id().unwrap().to_owned();

    // Now the read marker appears after the first event.
    // Timeline: [date-divider, A, read-marker, B].
    //            fully read --^
    let item = assert_next_matches!(stream, VectorDiff::Insert { index: 2, value } => value);
    assert_matches!(item.as_virtual(), Some(VirtualTimelineItem::ReadMarker));

    timeline.controller.handle_fully_read_marker(event_id2.clone()).await;

    // The read marker is removed but not reinserted, because it cannot be added at
    // the end.
    // Timeline: [date-divider, A, B].
    //                            ^-- fully read
    assert_next_matches!(stream, VectorDiff::Remove { index: 2 });

    // Timeline: [date-divider, A, B, C].
    //                            ^-- fully read
    timeline.handle_live_event(f.text_msg("C").sender(&BOB)).await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_id3 = item.as_event().unwrap().event_id().unwrap().to_owned();

    // Now the read marker is reinserted after the second event.
    // Timeline: [date-divider, A, B, read-marker, C].
    //                            ^-- fully read
    let marker = assert_next_matches!(stream, VectorDiff::Insert { index: 3, value } => value);
    assert!(marker.is_read_marker());

    // Nothing should happen if the fully read event is set back to an older event
    // sent by another user.
    timeline.controller.handle_fully_read_marker(event_id1).await;
    assert!(stream.next().now_or_never().is_none());

    // Nothing should happen if the fully read event isn't found.
    timeline.controller.handle_fully_read_marker(event_id!("$fake_event_id").to_owned()).await;
    assert!(stream.next().now_or_never().is_none());

    // Nothing should happen if the fully read event is referring to an event
    // that has already been marked as fully read.
    timeline.controller.handle_fully_read_marker(event_id2).await;
    assert!(stream.next().now_or_never().is_none());

    // Timeline: [date-divider, A, B, read-marker, C, D].
    //                            ^-- fully read
    timeline.handle_live_event(f.text_msg("D").sender(&BOB)).await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_id4 = item.as_event().unwrap().event_id().unwrap().to_owned();

    timeline.controller.handle_fully_read_marker(event_id3).await;

    // The read marker is moved after the third event (sent by another user).
    // Timeline: [date-divider, A, B, C, D].
    //                  fully read --^
    assert_next_matches!(stream, VectorDiff::Remove { index: 3 });

    // Timeline: [date-divider, A, B, C, read-marker, D].
    //                  fully read --^
    let marker = assert_next_matches!(stream, VectorDiff::Insert { index: 4, value } => value);
    assert!(marker.is_read_marker());

    // If the current user sends an event afterwards, the read marker doesn't move.
    // Timeline: [date-divider, A, B, C, read-marker, D, E].
    //                  fully read --^
    timeline.handle_live_event(f.text_msg("E").sender(&own_user)).await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    item.as_event().unwrap();

    assert!(stream.next().now_or_never().is_none());

    // If the marker moved forward to another user's event, and there's no other
    // event sent from another user, then it will be removed.
    // Timeline: [date-divider, A, B, C, D, E].
    //                     fully read --^
    timeline.controller.handle_fully_read_marker(event_id4).await;
    assert_next_matches!(stream, VectorDiff::Remove { index: 4 });

    assert!(stream.next().now_or_never().is_none());

    // When a last event is inserted by ourselves, still no read marker.
    // Timeline: [date-divider, A, B, C, D, E, F].
    //                     fully read --^
    timeline.handle_live_event(f.text_msg("F").sender(&own_user)).await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    item.as_event().unwrap();

    // Timeline: [date-divider, A, B, C, D, E, F, G].
    //                     fully read --^
    timeline.handle_live_event(f.text_msg("G").sender(&own_user)).await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    item.as_event().unwrap();

    assert!(stream.next().now_or_never().is_none());

    // But when it's another user who sent the event, then we get a read marker just
    // before their message. It is the first message that's both after the
    // fully-read event and not sent by us.
    //
    // Timeline: [date-divider, A, B, C, D, E, F, G, H].
    //                     fully read --^
    timeline.handle_live_event(f.text_msg("H").sender(&BOB)).await;
    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    item.as_event().unwrap();

    //                                     [our own]              v-- sent by Bob
    // Timeline: [date-divider, A, B, C, D,  E, F, G, read-marker, H].
    //                     fully read --^
    let marker = assert_next_matches!(stream, VectorDiff::Insert { index: 8, value } => value);
    assert!(marker.is_read_marker());

    assert!(stream.next().now_or_never().is_none());
}
