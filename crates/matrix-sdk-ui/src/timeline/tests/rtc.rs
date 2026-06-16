//! Tests for RTC notification timeline items.

use std::ops::Add;

use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use matrix_sdk_base::CallIntentConsensus;
use matrix_sdk_test::{ALICE, BOB, CAROL};
use ruma::{
    event_id,
    events::rtc::notification::{CallIntent, NotificationType},
};
use stream_assert::{assert_next_matches, assert_pending};

use super::*;
use crate::timeline::{TimelineItemContent, controller::ActiveCallInfo};

/// Test that `handle_rtc_members_update` correctly updates the active_members
/// of the RtcNotification timeline item.
#[tokio::test]
async fn test_rtc_members_update() {
    let timeline = TestTimeline::new();

    let mut stream = timeline.subscribe_events().await;

    let f = &timeline.factory;

    let notification_event = f
        .rtc_notification(NotificationType::Ring)
        .sender(*ALICE)
        .call_intent(CallIntent::Audio)
        .event_id(event_id!("$notification_event"));

    timeline.handle_live_event(notification_event).await;

    let notification_item = assert_next_matches!(stream, VectorDiff::PushBack { value } => value);

    assert_matches!(
        notification_item.content,
        TimelineItemContent::RtcNotification { active_call_info: None, .. }
    );

    // Simulate two members joined the call
    set_active_call_members(&timeline, &[BOB.to_owned(), CAROL.to_owned()]).await;

    let notification_item = assert_next_matches!(stream, VectorDiff::Set { value, .. } => value);
    assert_rtc_notification_active_members(
        notification_item,
        vec![BOB.to_owned(), CAROL.to_owned()],
    );

    // Simulate CAROL left
    set_active_call_members(&timeline, &[BOB.to_owned()]).await;

    let notification_item = assert_next_matches!(stream, VectorDiff::Set { value, .. } => value);
    assert_rtc_notification_active_members(notification_item, vec![BOB.to_owned()]);
}

/// Test that `handle_rtc_members_update` correctly updates the active_members
/// of the last notification event
#[tokio::test]
async fn test_rtc_members_update_last_only() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.subscribe_events().await;

    let f = &timeline.factory;

    let some_timestamp = SystemTime::now();

    // ===========
    // ACT: Insert a first notification event
    // ===========
    timeline
        .handle_live_event(
            f.rtc_notification(NotificationType::Ring)
                .sender(*ALICE)
                .call_intent(CallIntent::Audio)
                .event_id(event_id!("$notification_event_old"))
                .server_ts(MilliSecondsSinceUnixEpoch::from_system_time(some_timestamp).unwrap()),
        )
        .await;

    assert_next_matches!(stream, VectorDiff::PushBack { .. });

    // ===========
    // ACT: Simulate active members in the call
    // ===========
    set_active_call_members(&timeline, &[BOB.to_owned()]).await;

    // ===========
    // ASSERT: The notification item should be updated with the active members
    // ===========

    let notification_item = assert_next_matches!(stream, VectorDiff::Set { value, .. } => value);
    let info = assert_rtc_notification_active_members(notification_item, vec![BOB.to_owned()]);
    assert_eq!(
        info.call_started_ts_millis,
        Some(MilliSecondsSinceUnixEpoch::from_system_time(some_timestamp).unwrap())
    );

    // ===========
    // ACT: Insert a new notification event with a newer timestamp.
    // ===========
    let new_notification_ts = some_timestamp.add(Duration::from_secs(60));

    timeline
        .handle_live_event(
            f.rtc_notification(NotificationType::Ring)
                .sender(*ALICE)
                .call_intent(CallIntent::Audio)
                .event_id(event_id!("$notification_event_new"))
                .server_ts(
                    MilliSecondsSinceUnixEpoch::from_system_time(new_notification_ts).unwrap(),
                ),
        )
        .await;

    // ===========
    // ASSERT: As per requirement only the latest notification item in the timeline
    // should have the active call info. So ensure the oldest notification item
    // is cleared and that the new one has the previously known info
    // ===========

    // The notification is first added
    assert_next_matches!(
        stream,
        VectorDiff::PushBack { value } => value
    );

    // The old one is cleared
    let old_notif_updated = assert_next_matches!(
        stream,
        VectorDiff::Set { index: 0, value } => value
    );
    assert_eq!(
        old_notif_updated.event_id().expect("eventId should be set"),
        event_id!("$notification_event_old")
    );
    assert_matches!(old_notif_updated.content, TimelineItemContent::RtcNotification { active_call_info: None, .. } => {});

    // The new one is updated with the previously known info
    let item = assert_next_matches!(
        stream,
        VectorDiff::Set { index: 1, value } => value
    );
    let info = assert_rtc_notification_active_members(item, vec![BOB.to_owned()]);
    assert_eq!(
        info.call_started_ts_millis,
        Some(MilliSecondsSinceUnixEpoch::from_system_time(new_notification_ts).unwrap())
    );

    // ===========
    // ACT: Simulate new active members in the call
    // ===========

    set_active_call_members(&timeline, &[BOB.to_owned(), CAROL.to_owned()]).await;

    // ===========
    // ASSERT: Should only update the latest notification item.
    // ===========

    let notification_item = assert_next_matches!(stream, VectorDiff::Set { value, .. } => value);
    assert_eq!(
        notification_item.event_id().expect("eventId should be set"),
        event_id!("$notification_event_new")
    );
    assert_rtc_notification_active_members(
        notification_item,
        vec![BOB.to_owned(), CAROL.to_owned()],
    );

    // Simulate back paginated rtc notification

    let old_notification_ts = some_timestamp.sub(Duration::from_secs(60));

    timeline
        .controller
        .handle_remote_events_with_diffs(
            vec![VectorDiff::PushFront {
                value: f
                    .rtc_notification(NotificationType::Ring)
                    .sender(*ALICE)
                    .call_intent(CallIntent::Audio)
                    .event_id(event_id!("$notification_event_old"))
                    .server_ts(
                        MilliSecondsSinceUnixEpoch::from_system_time(old_notification_ts).unwrap(),
                    )
                    .into_event(),
            }],
            RemoteEventOrigin::Pagination,
        )
        .await;

    // Should only add this notification, and not modify the most recent one
    let notification_item =
        assert_next_matches!(stream, VectorDiff::PushFront { value, .. } => value);
    assert_eq!(
        notification_item.event_id().expect("eventId should be set"),
        event_id!("$notification_event_old")
    );

    assert_pending!(stream);
}

fn assert_rtc_notification_active_members(
    item: EventTimelineItem,
    members: Vec<OwnedUserId>,
) -> ActiveCallInfo {
    assert_matches!(item.content, TimelineItemContent::RtcNotification { active_call_info: Some(active_call_info), .. } => {
        let active_members = &active_call_info.active_members;
        assert_eq!(active_members.len(), members.len());
        for member in members {
            assert!(active_members.contains(&member));
        }
        active_call_info
    })
}

async fn set_active_call_members(timeline: &TestTimeline, members: &[OwnedUserId]) {
    let active_call = ActiveCallInfo {
        active_members: members.to_owned().into(),
        call_intent: CallIntentConsensus::None,
        is_joined: false,
        call_started_ts_millis: MilliSecondsSinceUnixEpoch::now().into(),
    };
    timeline.controller.handle_active_call_update(Some(active_call)).await;
}
