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
use matrix_sdk::{assert_next_matches_with_timeout, send_queue::RoomSendQueueUpdate};
use matrix_sdk_base::store::QueueWedgeError;
use matrix_sdk_test::{ALICE, BOB, async_test};
use ruma::{
    MilliSecondsSinceUnixEpoch, event_id,
    events::{AnyMessageLikeEventContent, room::message::RoomMessageEventContent},
    user_id,
};
use stream_assert::{assert_next_matches, assert_pending};

use super::TestTimeline;
use crate::timeline::{
    TimelineReadReceiptTracking,
    controller::TimelineSettings,
    event_item::{EventSendState, RemoteEventOrigin},
    tests::{TestRoomDataProvider, TestTimelineBuilder},
};

#[async_test]
async fn test_remote_echo_full_trip() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    // Given a local event…
    let txn_id = timeline
        .handle_local_event(AnyMessageLikeEventContent::RoomMessage(
            RoomMessageEventContent::text_plain("echo"),
        ))
        .await;

    // Scenario 1: The local event has not been sent yet to the server.
    let id = {
        let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
        let event_item = item.as_event().unwrap();
        assert!(event_item.is_local_echo());
        assert_matches!(
            event_item.send_state(),
            Some(EventSendState::NotSentYet { progress: None })
        );
        assert!(!event_item.can_be_replied_to());
        item.unique_id().to_owned()
    };

    {
        // The date divider comes in late.
        let date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
        assert!(date_divider.is_date_divider());
    }

    // Scenario 2: The local event has not been sent to the server successfully, it
    // has failed. In this case, there is no event ID.
    {
        let error = Arc::new(matrix_sdk::Error::SendQueueWedgeError(Box::new(
            QueueWedgeError::GenericApiError { msg: "this is a test".to_owned() },
        )));
        timeline
            .controller
            .update_event_send_state(
                &txn_id,
                EventSendState::SendingFailed { error, is_recoverable: true },
            )
            .await;

        let item = assert_next_matches!(stream, VectorDiff::Set { value, index: 1 } => value);
        let event_item = item.as_event().unwrap();
        assert!(event_item.is_local_echo());
        assert_matches!(
            event_item.send_state(),
            Some(EventSendState::SendingFailed { is_recoverable: true, .. })
        );
        assert_eq!(*item.unique_id(), id);
    }

    // Scenario 3: The local event has been sent successfully to the server and an
    // event ID has been received as part of the server's response.
    let event_id = event_id!("$W6mZSLWMmfuQQ9jhZWeTxFIM");
    let timestamp = {
        timeline
            .controller
            .update_event_send_state(
                &txn_id,
                EventSendState::Sent { event_id: event_id.to_owned() },
            )
            .await;

        let item = assert_next_matches!(stream, VectorDiff::Set { value, index: 1 } => value);
        let event_item = item.as_event().unwrap();
        assert!(event_item.is_local_echo());
        assert_matches!(event_item.send_state(), Some(EventSendState::Sent { .. }));
        assert_eq!(*item.unique_id(), id);

        event_item.timestamp()
    };

    // Now, a sync has been run against the server, and an event with the same ID
    // comes in.
    timeline
        .handle_live_event(
            timeline
                .factory
                .text_msg("echo")
                .sender(*ALICE)
                .event_id(event_id)
                .server_ts(timestamp),
        )
        .await;

    // The local echo is replaced with the remote echo.
    assert_next_matches!(stream, VectorDiff::Remove { index: 1 });
    let item = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(!item.as_event().unwrap().is_local_echo());
    assert_eq!(*item.unique_id(), id);

    // The date divider is adjusted.
    // A new date divider is inserted, and the older one is removed.
    let date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(date_divider.is_date_divider());
    assert_next_matches!(stream, VectorDiff::Remove { index: 2 });

    assert_pending!(stream);
}

#[async_test]
async fn test_remote_echo_new_position() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;
    let f = &timeline.factory;

    // Given a local event…
    let txn_id = timeline
        .handle_local_event(AnyMessageLikeEventContent::RoomMessage(
            RoomMessageEventContent::text_plain("echo"),
        ))
        .await;

    let item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);
    let txn_id_from_event = item.as_event().unwrap();
    assert_eq!(txn_id, txn_id_from_event.transaction_id().unwrap());

    let date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(date_divider.is_date_divider());

    // … and another event that comes back before the remote echo
    timeline.handle_live_event(f.text_msg("test").sender(&BOB)).await;

    // … and is inserted before the local echo item
    let bob_message = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(bob_message.is_remote_event());

    let date_divider = assert_next_matches!(stream, VectorDiff::PushFront { value } => value);
    assert!(date_divider.is_date_divider());

    // When the remote echo comes in…
    timeline
        .handle_live_event(
            f.text_msg("echo")
                .sender(*ALICE)
                .event_id(event_id!("$eeG0HA0FAZ37wP8kXlNkxx3I"))
                .server_ts(6)
                .unsigned_transaction_id(&txn_id),
        )
        .await;

    // … the remote echo replaces the previous event.
    assert_next_matches!(stream, VectorDiff::Remove { index: 3 });
    let item = assert_next_matches!(stream, VectorDiff::Insert { index: 2, value} => value);
    assert!(!item.as_event().unwrap().is_local_echo());

    // Date divider is updated.
    assert_next_matches!(stream, VectorDiff::Remove { index: 3 });

    assert_pending!(stream);
}

#[async_test]
async fn test_date_divider_removed_after_local_echo_disappeared() {
    let timeline = TestTimeline::new();

    let f = &timeline.factory;

    timeline
        .handle_live_event(f.text_msg("remote echo").sender(user_id!("@a:b.c")).server_ts(0))
        .await;

    let items = timeline.controller.items().await;

    assert_eq!(items.len(), 2);
    assert!(items[0].is_date_divider());
    assert!(items[1].is_remote_event());

    // Add a local echo.
    // It's not possible to synthesize `LocalEcho`s because they require forging a
    // `SendHandle`, which is a bit involved. Instead, use handle_local_event.
    let txn_id =
        timeline.handle_local_event(RoomMessageEventContent::text_plain("local echo").into()).await;

    let items = timeline.controller.items().await;

    assert_eq!(items.len(), 4);
    assert!(items[0].is_date_divider());
    assert!(items[1].is_remote_event());
    assert!(items[2].is_date_divider());
    assert!(items[3].is_local_echo());

    // Cancel the local echo.
    timeline
        .handle_room_send_queue_update(RoomSendQueueUpdate::CancelledLocalEvent {
            transaction_id: txn_id,
        })
        .await;

    let items = timeline.controller.items().await;

    assert_eq!(items.len(), 2);
    assert!(items[0].is_date_divider());
    assert!(items[1].is_remote_event());
}

#[async_test]
async fn test_no_read_marker_with_local_echo() {
    let event_id = event_id!("$1");

    let timeline = TestTimelineBuilder::new()
        .provider(TestRoomDataProvider::default().with_fully_read_marker(event_id.to_owned()))
        .settings(TimelineSettings {
            track_read_receipts: TimelineReadReceiptTracking::AllEvents,
            ..Default::default()
        })
        .build();

    let f = &timeline.factory;

    // Use `replace_with_initial_remote_events` which initializes the read marker;
    // other methods don't, by default.
    timeline
        .controller
        .replace_with_initial_remote_events(
            [f.text_msg("msg1")
                .sender(user_id!("@a:b.c"))
                .event_id(event_id)
                .server_ts(MilliSecondsSinceUnixEpoch::now())
                .into_event()],
            RemoteEventOrigin::Sync,
        )
        .await;

    let items = timeline.controller.items().await;

    assert_eq!(items.len(), 2);
    assert!(items[0].is_date_divider());
    assert!(items[1].is_remote_event());

    // Add a local echo.
    // It's not possible to synthesize `LocalEcho`s because they require forging a
    // `SendHandle`, which is a bit involved. Instead, use handle_local_event.
    let txn_id =
        timeline.handle_local_event(RoomMessageEventContent::text_plain("local echo").into()).await;

    let items = timeline.controller.items().await;

    assert_eq!(items.len(), 3);
    assert!(items[0].is_date_divider());
    assert!(items[1].is_remote_event());
    assert!(items[2].is_local_echo());

    // Cancel the local echo.
    timeline
        .handle_room_send_queue_update(RoomSendQueueUpdate::CancelledLocalEvent {
            transaction_id: txn_id,
        })
        .await;

    let items = timeline.controller.items().await;

    assert_eq!(items.len(), 2);
    assert!(items[0].is_date_divider());
    assert!(items[1].is_remote_event());
}

#[async_test]
async fn test_no_reuse_of_counters() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe().await;

    let now = MilliSecondsSinceUnixEpoch::now();

    // Given a local event…
    timeline
        .handle_local_event(AnyMessageLikeEventContent::RoomMessage(
            RoomMessageEventContent::text_plain("echo"),
        ))
        .await;

    // It gets added with a unique id
    // Timeline = [local]
    let local_id = assert_next_matches_with_timeout!(stream, VectorDiff::PushBack { value: item } => {
        let event_item = item.as_event().unwrap();
        assert!(event_item.is_local_echo());
        assert_matches!(event_item.send_state(), Some(EventSendState::NotSentYet { progress: None }));
        assert!(!event_item.can_be_replied_to());
        item.unique_id().to_owned()
    });

    // The date divider comes in late.
    // Timeline = [date-divider local]
    assert_next_matches_with_timeout!(stream, VectorDiff::PushFront { value } => {
        assert!(value.is_date_divider());
    });

    // Add a remote event now.
    timeline
        .handle_live_event(
            timeline
                .factory
                .text_msg("hey")
                .sender(&ALICE)
                .event_id(event_id!("$1"))
                .server_ts(now),
        )
        .await;

    // Timeline = [remote date-divider local]
    assert_next_matches_with_timeout!(stream, VectorDiff::PushFront { value: item } => {
        assert!(!item.as_event().unwrap().is_local_echo());
        // Both items have a different unique id.
        assert_ne!(local_id, item.unique_id().to_owned());
    });

    // Date divider shenanigans.
    // Timeline = [date-divider remote date-divider local]
    assert_next_matches_with_timeout!(stream, VectorDiff::PushFront { value } => {
        assert!(value.is_date_divider());
    });
    // Timeline = [date-divider remote local]
    assert_next_matches_with_timeout!(stream, VectorDiff::Remove { index: 2 });

    // When clearing the timeline, the local echo remains.
    timeline.controller.clear().await;

    assert_next_matches_with_timeout!(stream, VectorDiff::Remove { index: 1 });

    // The next timeline item comes with a different unique id.
    timeline
        .handle_live_event(
            timeline.factory.text_msg("yo").sender(&ALICE).event_id(event_id!("$2")).server_ts(now),
        )
        .await;

    let remote_id = assert_next_matches_with_timeout!(stream, VectorDiff::PushFront { value: item } => {
        assert!(!item.as_event().unwrap().is_local_echo());
        item.unique_id().to_owned()
    });

    // The remote id still isn't the same as the local id.
    assert_ne!(local_id, remote_id);
}
