// Copyright 2026 The Matrix.org Foundation C.I.C.
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

//! Tests for the reconciliation of gap virtual items against the event
//! cache's gap reports.

use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::{FutureExt, StreamExt as _};
use matrix_sdk::event_cache::TimelineGap;
use matrix_sdk_test::{ALICE, async_test};
use ruma::event_id;
use stream_assert::assert_next_matches;

use super::{TestTimeline, TestTimelineBuilder};
use crate::timeline::{VirtualTimelineItem, controller::TimelineSettings};

/// A timeline with storage-only pagination enabled, i.e. one that renders gap
/// items.
async fn gappy_timeline() -> TestTimeline {
    TestTimelineBuilder::new()
        .settings(TimelineSettings { storage_only_pagination: true, ..Default::default() })
        .build()
        .await
}

fn gap(prev_token: &str, following_event_id: &ruma::EventId) -> TimelineGap {
    TimelineGap {
        prev_token: prev_token.to_owned(),
        following_event_id: Some(following_event_id.to_owned()),
    }
}

#[async_test]
async fn test_gap_item_is_anchored_before_its_following_event() {
    let timeline = gappy_timeline().await;
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;
    timeline.handle_live_event(f.text_msg("A").sender(*ALICE).event_id(event_id!("$a"))).await;
    timeline.handle_live_event(f.text_msg("B").sender(*ALICE).event_id(event_id!("$b"))).await;

    // Timeline: [date-divider, A, B].
    let _ = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let _ = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    let _ = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

    // The event cache reports a gap before B.
    timeline.controller.handle_timeline_gaps(vec![gap("g1", event_id!("$b"))]).await;

    // Timeline: [date-divider, A, gap, B].
    let item = assert_next_matches!(stream, VectorDiff::Insert { index: 2, value } => value);
    assert_let!(VirtualTimelineItem::Gap { prev_token } = item.as_virtual().unwrap());
    assert_eq!(prev_token, "g1");

    assert!(stream.next().now_or_never().is_none());
}

#[async_test]
async fn test_gap_item_moves_and_keeps_its_identity_when_reanchored() {
    let timeline = gappy_timeline().await;
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;
    timeline.handle_live_event(f.text_msg("A").sender(*ALICE).event_id(event_id!("$a"))).await;
    timeline.handle_live_event(f.text_msg("B").sender(*ALICE).event_id(event_id!("$b"))).await;

    let _ = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let _ = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    let _ = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

    // Timeline: [date-divider, A, gap, B].
    timeline.controller.handle_timeline_gaps(vec![gap("g1", event_id!("$b"))]).await;
    let item = assert_next_matches!(stream, VectorDiff::Insert { index: 2, value } => value);
    let gap_id = item.unique_id().clone();

    // The gap now precedes A (e.g. after a partial resolution shuffled the
    // events around): the item moves, but keeps its identity.
    timeline.controller.handle_timeline_gaps(vec![gap("g1", event_id!("$a"))]).await;

    // Timeline: [date-divider, gap, A, B].
    assert_next_matches!(stream, VectorDiff::Remove { index: 2 });
    let item = assert_next_matches!(stream, VectorDiff::Insert { index: 1, value } => value);
    assert!(item.is_gap());
    assert_eq!(*item.unique_id(), gap_id);

    assert!(stream.next().now_or_never().is_none());
}

#[async_test]
async fn test_adjacent_gaps_collapse_to_the_newest_one() {
    let timeline = gappy_timeline().await;
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;
    timeline.handle_live_event(f.text_msg("A").sender(*ALICE).event_id(event_id!("$a"))).await;
    timeline.handle_live_event(f.text_msg("B").sender(*ALICE).event_id(event_id!("$b"))).await;

    // Timeline: [date-divider, A, B].
    let _ = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let _ = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    let _ = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

    // Two gaps with nothing rendered between them: both anchor before B.
    // Only the newest one is rendered.
    timeline
        .controller
        .handle_timeline_gaps(vec![gap("g1", event_id!("$b")), gap("g2", event_id!("$b"))])
        .await;

    // Timeline: [date-divider, A, gap(g2), B].
    let item = assert_next_matches!(stream, VectorDiff::Insert { index: 2, value } => value);
    assert_let!(VirtualTimelineItem::Gap { prev_token } = item.as_virtual().unwrap());
    assert_eq!(prev_token, "g2");

    assert!(stream.next().now_or_never().is_none());

    // Resolving g2 lands an event between the gaps: g1 now has its own
    // anchor and gets rendered in its place.
    timeline.handle_live_event(f.text_msg("M").sender(*ALICE).event_id(event_id!("$m"))).await;
    let _ = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

    timeline.controller.handle_timeline_gaps(vec![gap("g1", event_id!("$m"))]).await;

    // The g2 item goes away, and g1 is anchored before M, wherever the
    // timeline placed it.
    let mut saw_g1 = false;
    while let Some(Some(diff)) = stream.next().now_or_never() {
        if let VectorDiff::Insert { value, .. } = &diff
            && let Some(VirtualTimelineItem::Gap { prev_token }) = value.as_virtual()
        {
            assert_eq!(prev_token, "g1");
            saw_g1 = true;
        }
    }
    assert!(saw_g1, "g1 should be rendered once it has its own anchor");
}

#[async_test]
async fn test_gap_item_is_removed_when_no_longer_reported() {
    let timeline = gappy_timeline().await;
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;
    timeline.handle_live_event(f.text_msg("A").sender(*ALICE).event_id(event_id!("$a"))).await;

    let _ = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let _ = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);

    // Timeline: [date-divider, gap, A].
    timeline.controller.handle_timeline_gaps(vec![gap("g1", event_id!("$a"))]).await;
    assert_next_matches!(stream, VectorDiff::Insert { index: 1, value: _ });

    // The gap has been resolved: the item disappears.
    timeline.controller.handle_timeline_gaps(vec![]).await;
    assert_next_matches!(stream, VectorDiff::Remove { index: 1 });

    assert!(stream.next().now_or_never().is_none());
}

#[async_test]
async fn test_gap_item_waits_for_its_anchor() {
    let timeline = gappy_timeline().await;
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;
    timeline.handle_live_event(f.text_msg("A").sender(*ALICE).event_id(event_id!("$a"))).await;

    let _ = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let _ = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);

    // The gap's following event isn't known to the timeline: nothing is
    // rendered for it yet.
    timeline.controller.handle_timeline_gaps(vec![gap("g1", event_id!("$b"))]).await;
    assert!(stream.next().now_or_never().is_none());

    // Once the anchor event arrives, the gap is rendered before it.
    timeline.handle_live_event(f.text_msg("B").sender(*ALICE).event_id(event_id!("$b"))).await;

    // Timeline: [date-divider, A, gap, B].
    let _ = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let item = assert_next_matches!(stream, VectorDiff::Insert { index: 2, value } => value);
    assert!(item.is_gap());

    assert!(stream.next().now_or_never().is_none());
}

#[async_test]
async fn test_timeline_start_is_not_inserted_over_a_leading_gap() {
    let timeline = gappy_timeline().await;
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;
    timeline.handle_live_event(f.text_msg("A").sender(*ALICE).event_id(event_id!("$a"))).await;

    let _ = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let _ = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);

    // Timeline: [date-divider, gap, A]. The gap leads the remotes region, so
    // having exhausted the storage doesn't prove the room's start: no
    // timeline start item.
    timeline.controller.handle_timeline_gaps(vec![gap("g1", event_id!("$a"))]).await;
    assert_next_matches!(stream, VectorDiff::Insert { index: 1, value: _ });

    timeline.controller.insert_timeline_start_if_missing().await;
    assert!(stream.next().now_or_never().is_none());

    // Once the gap is resolved, the timeline start can be inserted.
    timeline.controller.handle_timeline_gaps(vec![]).await;
    assert_next_matches!(stream, VectorDiff::Remove { index: 1 });

    timeline.controller.insert_timeline_start_if_missing().await;
    let item = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(item.is_timeline_start());

    assert!(stream.next().now_or_never().is_none());
}

#[async_test]
async fn test_gap_with_no_rendered_follower_is_shown_at_the_newest_end() {
    // A filtered timeline, keeping only messages whose body contains "keep".
    let timeline = TestTimelineBuilder::new()
        .settings(TimelineSettings {
            storage_only_pagination: true,
            event_filter: std::sync::Arc::new(|event, _| {
                use ruma::events::{AnyMessageLikeEventContent, AnySyncMessageLikeEvent, AnySyncTimelineEvent};
                match event {
                    AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(msg)) => {
                        matches!(msg.as_original().map(|ev| AnyMessageLikeEventContent::RoomMessage(ev.content.clone())), Some(AnyMessageLikeEventContent::RoomMessage(content)) if content.body().contains("keep"))
                    }
                    _ => false,
                }
            }),
            ..Default::default()
        })
        .build()
        .await;
    let mut stream = timeline.subscribe().await;

    let f = &timeline.factory;
    timeline.handle_live_event(f.text_msg("keep A").sender(*ALICE).event_id(event_id!("$a"))).await;
    let _ = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let _ = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);

    // B is filtered out: known to the timeline, but has no item.
    timeline.handle_live_event(f.text_msg("drop B").sender(*ALICE).event_id(event_id!("$b"))).await;
    assert!(stream.next().now_or_never().is_none());

    // A gap before B: nothing after it is rendered, so it shows at the newest
    // end (there may be items in the gap): [date-divider, A, gap].
    timeline.controller.handle_timeline_gaps(vec![gap("g1", event_id!("$b"))]).await;
    let item = assert_next_matches!(stream, VectorDiff::Insert { index: 2, value } => value);
    assert!(item.is_gap());
    assert!(stream.next().now_or_never().is_none());

    // A rendered event after the gap re-anchors it: [date-divider, A, gap, C].
    timeline.handle_live_event(f.text_msg("keep C").sender(*ALICE).event_id(event_id!("$c"))).await;
    while stream.next().now_or_never().flatten().is_some() {}

    let items = timeline.controller.items().await;
    assert!(items[2].is_gap(), "{items:?}");
    assert_eq!(items[3].as_event().unwrap().event_id().unwrap(), event_id!("$c"));
}
