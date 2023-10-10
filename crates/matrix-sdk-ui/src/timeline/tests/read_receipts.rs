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

use eyeball_im::VectorDiff;
use matrix_sdk_test::{async_test, ALICE, BOB, CAROL};
use ruma::{
    event_id,
    events::{
        receipt::{ReceiptThread, ReceiptType},
        room::message::{MessageType, RoomMessageEventContent, SyncRoomMessageEvent},
        AnySyncMessageLikeEvent, AnySyncTimelineEvent,
    },
    owned_event_id, room_id,
};
use stream_assert::{assert_next_matches, assert_pending};

use super::TestTimeline;
use crate::timeline::inner::TimelineInnerSettings;

fn filter_notice(ev: &AnySyncTimelineEvent) -> bool {
    match ev {
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(
            SyncRoomMessageEvent::Original(msg),
        )) => !matches!(msg.content.msgtype, MessageType::Notice(_)),
        _ => true,
    }
}

#[async_test]
async fn read_receipts_updates_on_live_events() {
    let timeline = TestTimeline::new()
        .with_settings(TimelineInnerSettings { track_read_receipts: true, ..Default::default() });
    let mut stream = timeline.subscribe().await;

    timeline.handle_live_message_event(*ALICE, RoomMessageEventContent::text_plain("A")).await;
    timeline.handle_live_message_event(*BOB, RoomMessageEventContent::text_plain("B")).await;

    let _day_divider = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

    // No read receipt for our own user.
    let item_a = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_a = item_a.as_event().unwrap();
    assert!(event_a.read_receipts().is_empty());

    // Implicit read receipt of Bob.
    let item_b = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_b = item_b.as_event().unwrap();
    assert_eq!(event_b.read_receipts().len(), 1);
    assert!(event_b.read_receipts().get(*BOB).is_some());

    // Implicit read receipt of Bob is updated.
    timeline.handle_live_message_event(*BOB, RoomMessageEventContent::text_plain("C")).await;

    let item_a = assert_next_matches!(stream, VectorDiff::Set { index: 2, value } => value);
    let event_a = item_a.as_event().unwrap();
    assert!(event_a.read_receipts().is_empty());

    let item_c = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_c = item_c.as_event().unwrap();
    assert_eq!(event_c.read_receipts().len(), 1);
    assert!(event_c.read_receipts().get(*BOB).is_some());

    timeline.handle_live_message_event(*ALICE, RoomMessageEventContent::text_plain("D")).await;

    let item_d = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_d = item_d.as_event().unwrap();
    assert!(event_d.read_receipts().is_empty());

    // Explicit read receipt is updated.
    timeline
        .handle_read_receipts([(
            event_d.event_id().unwrap().to_owned(),
            ReceiptType::Read,
            BOB.to_owned(),
            ReceiptThread::Unthreaded,
        )])
        .await;

    let item_c = assert_next_matches!(stream, VectorDiff::Set { index: 3, value } => value);
    let event_c = item_c.as_event().unwrap();
    assert!(event_c.read_receipts().is_empty());

    let item_d = assert_next_matches!(stream, VectorDiff::Set { index: 4, value } => value);
    let event_d = item_d.as_event().unwrap();
    assert_eq!(event_d.read_receipts().len(), 1);
    assert!(event_d.read_receipts().get(*BOB).is_some());
}

#[async_test]
async fn read_receipts_updates_on_back_paginated_events() {
    let timeline = TestTimeline::new()
        .with_settings(TimelineInnerSettings { track_read_receipts: true, ..Default::default() });
    let room_id = room_id!("!room:localhost");

    timeline
        .handle_back_paginated_message_event_with_id(
            *BOB,
            room_id,
            event_id!("$event_a"),
            RoomMessageEventContent::text_plain("A"),
        )
        .await;
    timeline
        .handle_back_paginated_message_event_with_id(
            *CAROL,
            room_id,
            event_id!("$event_with_bob_receipt"),
            RoomMessageEventContent::text_plain("B"),
        )
        .await;

    let items = timeline.inner.items().await;
    assert_eq!(items.len(), 3);
    assert!(items[0].is_day_divider());

    // Implicit read receipt of Bob.
    let event_a = items[2].as_event().unwrap();
    assert_eq!(event_a.read_receipts().len(), 1);
    assert!(event_a.read_receipts().get(*BOB).is_some());

    // Implicit read receipt of Carol, explicit read receipt of Bob ignored.
    let event_b = items[1].as_event().unwrap();
    assert_eq!(event_b.read_receipts().len(), 1);
    assert!(event_b.read_receipts().get(*CAROL).is_some());
}

#[async_test]
async fn read_receipts_updates_on_filtered_events() {
    let timeline = TestTimeline::new().with_settings(TimelineInnerSettings {
        track_read_receipts: true,
        event_filter: Arc::new(filter_notice),
        ..Default::default()
    });
    let mut stream = timeline.subscribe().await;

    timeline.handle_live_message_event(*ALICE, RoomMessageEventContent::text_plain("A")).await;
    timeline.handle_live_message_event(*BOB, RoomMessageEventContent::notice_plain("B")).await;

    let _day_divider = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

    // No read receipt for our own user.
    let item_a = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_a = item_a.as_event().unwrap();
    assert!(event_a.read_receipts().is_empty());

    // Implicit read receipt of Bob.
    let item_a = assert_next_matches!(stream, VectorDiff::Set { index: 1, value } => value);
    let event_a = item_a.as_event().unwrap();
    assert_eq!(event_a.read_receipts().len(), 1);
    assert!(event_a.read_receipts().get(*BOB).is_some());

    // Implicit read receipt of Bob is updated.
    timeline.handle_live_message_event(*BOB, RoomMessageEventContent::text_plain("C")).await;

    let item_a = assert_next_matches!(stream, VectorDiff::Set { index: 1, value } => value);
    let event_a = item_a.as_event().unwrap();
    assert!(event_a.read_receipts().is_empty());

    let item_c = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_c = item_c.as_event().unwrap();
    assert_eq!(event_c.read_receipts().len(), 1);
    assert!(event_c.read_receipts().get(*BOB).is_some());

    // Populate more events.
    let event_d_id = owned_event_id!("$event_d");
    timeline
        .handle_live_message_event_with_id(
            *ALICE,
            &event_d_id,
            RoomMessageEventContent::notice_plain("D"),
        )
        .await;

    timeline.handle_live_message_event(*ALICE, RoomMessageEventContent::text_plain("E")).await;

    let item_e = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_e = item_e.as_event().unwrap();
    assert!(event_e.read_receipts().is_empty());

    // Explicit read receipt is updated but its visible event doesn't change.
    timeline
        .handle_read_receipts([(
            event_d_id,
            ReceiptType::Read,
            BOB.to_owned(),
            ReceiptThread::Unthreaded,
        )])
        .await;

    // Explicit read receipt is updated and its visible event changes.
    timeline
        .handle_read_receipts([(
            event_e.event_id().unwrap().to_owned(),
            ReceiptType::Read,
            BOB.to_owned(),
            ReceiptThread::Unthreaded,
        )])
        .await;

    let item_c = assert_next_matches!(stream, VectorDiff::Set { index: 2, value } => value);
    let event_c = item_c.as_event().unwrap();
    assert!(event_c.read_receipts().is_empty());

    let item_e = assert_next_matches!(stream, VectorDiff::Set { index: 3, value } => value);
    let event_e = item_e.as_event().unwrap();
    assert_eq!(event_e.read_receipts().len(), 1);
    assert!(event_e.read_receipts().get(*BOB).is_some());

    assert_pending!(stream);
}

#[async_test]
async fn read_receipts_updates_on_filtered_events_with_stored() {
    let timeline = TestTimeline::new().with_settings(TimelineInnerSettings {
        track_read_receipts: true,
        event_filter: Arc::new(filter_notice),
        ..Default::default()
    });
    let mut stream = timeline.subscribe().await;

    timeline.handle_live_message_event(*ALICE, RoomMessageEventContent::text_plain("A")).await;
    timeline
        .handle_live_message_event_with_id(
            *CAROL,
            event_id!("$event_with_bob_receipt"),
            RoomMessageEventContent::notice_plain("B"),
        )
        .await;

    let _day_divider = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

    // No read receipt for our own user.
    let item_a = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_a = item_a.as_event().unwrap();
    assert!(event_a.read_receipts().is_empty());

    // Stored read receipt of Bob.
    let item_a = assert_next_matches!(stream, VectorDiff::Set { index: 1, value } => value);
    let event_a = item_a.as_event().unwrap();
    assert_eq!(event_a.read_receipts().len(), 1);
    assert!(event_a.read_receipts().get(*BOB).is_some());

    // Implicit read receipt of Carol.
    let item_a = assert_next_matches!(stream, VectorDiff::Set { index: 1, value } => value);
    let event_a = item_a.as_event().unwrap();
    assert_eq!(event_a.read_receipts().len(), 2);
    assert!(event_a.read_receipts().get(*BOB).is_some());
    assert!(event_a.read_receipts().get(*CAROL).is_some());

    // Implicit read receipt of Bob is updated.
    timeline.handle_live_message_event(*BOB, RoomMessageEventContent::text_plain("C")).await;

    let item_a = assert_next_matches!(stream, VectorDiff::Set { index: 1, value } => value);
    let event_a = item_a.as_event().unwrap();
    assert_eq!(event_a.read_receipts().len(), 1);

    let item_c = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let event_c = item_c.as_event().unwrap();
    assert_eq!(event_c.read_receipts().len(), 1);
    assert!(event_c.read_receipts().get(*BOB).is_some());

    assert_pending!(stream);
}

#[async_test]
async fn read_receipts_updates_on_back_paginated_filtered_events() {
    let timeline = TestTimeline::new().with_settings(TimelineInnerSettings {
        track_read_receipts: true,
        event_filter: Arc::new(filter_notice),
        ..Default::default()
    });
    let mut stream = timeline.subscribe().await;
    let room_id = room_id!("!room:localhost");

    timeline
        .handle_back_paginated_message_event_with_id(
            *ALICE,
            room_id,
            event_id!("$event_a"),
            RoomMessageEventContent::text_plain("A"),
        )
        .await;
    timeline
        .handle_back_paginated_message_event_with_id(
            *CAROL,
            room_id,
            event_id!("$event_with_bob_receipt"),
            RoomMessageEventContent::notice_plain("B"),
        )
        .await;

    let _day_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);

    // No read receipt for our own user.
    let item_a = assert_next_matches!(stream, VectorDiff::Insert { index: 1, value } => value);
    let event_a = item_a.as_event().unwrap();
    assert!(event_a.read_receipts().is_empty());

    // Add non-filtered event to show read receipts.
    timeline
        .handle_back_paginated_message_event_with_id(
            *CAROL,
            room_id,
            event_id!("$event_c"),
            RoomMessageEventContent::text_plain("C"),
        )
        .await;

    // Implicit read receipt of Carol.
    let item_c = assert_next_matches!(stream, VectorDiff::Insert { index: 1, value } => value);
    let event_c = item_c.as_event().unwrap();
    assert_eq!(event_c.read_receipts().len(), 2);
    assert!(event_c.read_receipts().get(*BOB).is_some());
    assert!(event_c.read_receipts().get(*CAROL).is_some());

    assert_pending!(stream);
}
