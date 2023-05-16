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
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk_test::async_test;
use ruma::events::{
    receipt::{ReceiptThread, ReceiptType},
    room::message::RoomMessageEventContent,
};

use super::{TestTimeline, ALICE, BOB};

#[async_test]
async fn read_receipts_updates() {
    let timeline = TestTimeline::new().with_read_receipt_tracking();
    let mut stream = timeline.subscribe().await;

    timeline.handle_live_message_event(*ALICE, RoomMessageEventContent::text_plain("A")).await;
    timeline.handle_live_message_event(*BOB, RoomMessageEventContent::text_plain("B")).await;

    let _day_divider =
        assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);

    // No read receipt for our own user.
    let item_a =
        assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let event_a = item_a.as_event().unwrap();
    assert!(event_a.read_receipts().is_empty());

    // Implicit read receipt of Bob.
    let item_b =
        assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let event_b = item_b.as_event().unwrap();
    assert_eq!(event_b.read_receipts().len(), 1);
    assert!(event_b.read_receipts().get(*BOB).is_some());

    // Implicit read receipt of Bob is updated.
    timeline.handle_live_message_event(*BOB, RoomMessageEventContent::text_plain("C")).await;

    let item_a =
        assert_matches!(stream.next().await, Some(VectorDiff::Set { index: 2, value }) => value);
    let event_a = item_a.as_event().unwrap();
    assert!(event_a.read_receipts().is_empty());

    let item_c =
        assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
    let event_c = item_c.as_event().unwrap();
    assert_eq!(event_c.read_receipts().len(), 1);
    assert!(event_c.read_receipts().get(*BOB).is_some());

    timeline.handle_live_message_event(*ALICE, RoomMessageEventContent::text_plain("D")).await;

    let item_d =
        assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
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

    let item_c =
        assert_matches!(stream.next().await, Some(VectorDiff::Set { index: 3, value }) => value);
    let event_c = item_c.as_event().unwrap();
    assert!(event_c.read_receipts().is_empty());

    let item_d =
        assert_matches!(stream.next().await, Some(VectorDiff::Set { index: 4, value }) => value);
    let event_d = item_d.as_event().unwrap();
    assert_eq!(event_d.read_receipts().len(), 1);
    assert!(event_d.read_receipts().get(*BOB).is_some());
}
