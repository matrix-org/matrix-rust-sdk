use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use matrix_sdk::{assert_let_timeout, test_utils::mocks::MatrixMockServer};
use matrix_sdk_test::{
    ALICE, BOB, CAROL, JoinedRoomBuilder, async_test, event_factory::EventFactory,
};
use matrix_sdk_ui::timeline::{RoomExt, TimelineItemContent};
use ruma::{
    event_id,
    events::rtc::notification::{CallIntent, NotificationType},
    room_id,
};
use tokio_stream::StreamExt;

#[async_test]
async fn test_decline_call() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let notification_event_id = event_id!("$notify");
    let declination_event_id = event_id!("$decline");

    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.rtc_notification(NotificationType::Ring)
                        .call_intent(CallIntent::Audio)
                        .sender(&ALICE)
                        .event_id(notification_event_id),
                )
                .add_timeline_event(
                    f.call_decline(notification_event_id)
                        .sender(&BOB)
                        .event_id(declination_event_id),
                ),
        )
        .await;

    assert_let_timeout!(Some(timeline_updates) = timeline_stream.next());
    assert_eq!(timeline_updates.len(), 4);

    assert_let!(VectorDiff::PushBack { value: message } = &timeline_updates[0]);
    let event_item = message.as_event().unwrap();
    assert!(event_item.content().is_rtc_notification());
    assert_let!(
        TimelineItemContent::RtcNotification { call_intent: _, declined_by } = event_item.content()
    );
    assert_eq!(declined_by.len(), 0);

    // Ignore update 1 (implicit read receipt following the declination)
    // Then the decline is taken into account.
    assert_let!(VectorDiff::Set { index: 0, value: updated_message } = &timeline_updates[2]);
    let event_item = updated_message.as_event().unwrap();

    assert_let!(
        TimelineItemContent::RtcNotification { call_intent, declined_by } = event_item.content()
    );
    assert_eq!(declined_by.len(), 1);
    assert_eq!(declined_by[0], *BOB);
    assert_eq!(call_intent.clone().unwrap(), CallIntent::Audio);
}

#[async_test]
async fn test_multiple_decline_call() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let notification_event_id = event_id!("$notify");
    let declination_event_id_bob = event_id!("$decline1");
    let declination_event_id_carol = event_id!("$decline2");

    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.rtc_notification(NotificationType::Ring)
                        .sender(&ALICE)
                        .event_id(notification_event_id),
                )
                .add_timeline_event(
                    f.call_decline(notification_event_id)
                        .sender(&BOB)
                        .event_id(declination_event_id_bob),
                )
                .add_timeline_event(
                    f.call_decline(notification_event_id)
                        .sender(&CAROL)
                        .event_id(declination_event_id_carol),
                ),
        )
        .await;

    assert_let_timeout!(Some(timeline_updates) = timeline_stream.next());
    assert_eq!(timeline_updates.len(), 6);

    assert_let!(VectorDiff::PushBack { value: message } = &timeline_updates[0]);
    let event_item = message.as_event().unwrap();
    assert!(event_item.content().is_rtc_notification());
    assert_let!(
        TimelineItemContent::RtcNotification { call_intent: _, declined_by } = event_item.content()
    );
    assert_eq!(declined_by.len(), 0);

    // Ignore update 1 (implicit read receipt following the declination)
    // Then the first decline is taken into account.
    assert_let!(VectorDiff::Set { index: 0, value: updated_message } = &timeline_updates[2]);
    let event_item = updated_message.as_event().unwrap();

    assert_let!(
        TimelineItemContent::RtcNotification { call_intent: _, declined_by } = event_item.content()
    );
    assert_eq!(declined_by.len(), 1);
    assert_eq!(declined_by[0], *BOB);

    // Ignore update 3 (implicit read receipt following the declination)
    // Then the second decline is taken into account.
    assert_let!(VectorDiff::Set { index: 0, value: updated_message } = &timeline_updates[4]);
    let event_item = updated_message.as_event().unwrap();

    assert_let!(
        TimelineItemContent::RtcNotification { call_intent: _, declined_by } = event_item.content()
    );
    assert_eq!(declined_by.len(), 2);
    assert_eq!(declined_by[0], *BOB);
    assert_eq!(declined_by[1], *CAROL);
}
