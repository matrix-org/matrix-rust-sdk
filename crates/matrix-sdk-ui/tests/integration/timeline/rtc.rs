use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use matrix_sdk::{assert_let_timeout, test_utils::mocks::MatrixMockServer};
use matrix_sdk_test::{
    ALICE, BOB, CAROL, JoinedRoomBuilder, async_test, event_factory::EventFactory,
};
use matrix_sdk_ui::timeline::{RoomExt, TimelineItemContent};
use ruma::{
    MilliSecondsSinceUnixEpoch, event_id,
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
    assert!(matches!(event_item.content(), TimelineItemContent::RtcNotification { .. }));
    assert_let!(
        TimelineItemContent::RtcNotification { call_intent: _, declined_by, .. } =
            event_item.content()
    );
    assert_eq!(declined_by.len(), 0);

    // Ignore update 1 (implicit read receipt following the declination)
    // Then the decline is taken into account.
    assert_let!(VectorDiff::Set { index: 0, value: updated_message } = &timeline_updates[2]);
    let event_item = updated_message.as_event().unwrap();

    assert_let!(
        TimelineItemContent::RtcNotification { call_intent, declined_by, .. } =
            event_item.content()
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
    assert!(matches!(event_item.content(), TimelineItemContent::RtcNotification { .. }));
    assert_let!(
        TimelineItemContent::RtcNotification { call_intent: _, declined_by, .. } =
            event_item.content()
    );
    assert_eq!(declined_by.len(), 0);

    // Ignore update 1 (implicit read receipt following the declination)
    // Then the first decline is taken into account.
    assert_let!(VectorDiff::Set { index: 0, value: updated_message } = &timeline_updates[2]);
    let event_item = updated_message.as_event().unwrap();

    assert_let!(
        TimelineItemContent::RtcNotification { call_intent: _, declined_by, .. } =
            event_item.content()
    );
    assert_eq!(declined_by.len(), 1);
    assert_eq!(declined_by[0], *BOB);

    // Ignore update 3 (implicit read receipt following the declination)
    // Then the second decline is taken into account.
    assert_let!(VectorDiff::Set { index: 0, value: updated_message } = &timeline_updates[4]);
    let event_item = updated_message.as_event().unwrap();

    assert_let!(
        TimelineItemContent::RtcNotification { call_intent: _, declined_by, .. } =
            event_item.content()
    );
    assert_eq!(declined_by.len(), 2);
    assert_eq!(declined_by[0], *BOB);
    assert_eq!(declined_by[1], *CAROL);
}

#[async_test]
async fn test_active_call_info() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let notification_event_id = event_id!("$notify");
    let bob_membership = event_id!("$bob-joined");
    let carol_membership = event_id!("$carol-joined");
    let alice_membership = event_id!("$alice-joined");
    let notification_timestamp = MilliSecondsSinceUnixEpoch::now();

    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.rtc_notification(NotificationType::Ring)
                        .sender(&ALICE)
                        .server_ts(notification_timestamp)
                        .event_id(notification_event_id),
                )
                .add_state_event(
                    f.call_membership_state(BOB.to_owned(), "BOB_DEVICE".to_owned())
                        .lk_focus(room_id.into(), "https://livekit.localhow".to_owned())
                        .event_id(bob_membership),
                )
                .add_state_event(
                    f.call_membership_state(CAROL.to_owned(), "CAROL_DEVICE".to_owned())
                        .lk_focus(room_id.into(), "https://livekit.localhow".to_owned())
                        .event_id(carol_membership),
                ),
        )
        .await;

    assert_let_timeout!(Some(timeline_updates) = timeline_stream.next());
    assert_eq!(timeline_updates.len(), 3);

    // First is adding the notification event
    assert_let!(VectorDiff::PushBack { value: message } = &timeline_updates[0]);
    let event_item = message.as_event().unwrap();
    assert!(matches!(event_item.content(), TimelineItemContent::RtcNotification { .. }));

    //Second update is updating it to have active call info
    assert_let!(VectorDiff::Set { index: 0, value: message } = &timeline_updates[1]);
    let event_item = message.as_event().unwrap();
    assert!(matches!(event_item.content(), TimelineItemContent::RtcNotification { .. }));
    assert_let!(
        TimelineItemContent::RtcNotification { active_call_info: Some(active_call_info), .. } =
            event_item.content()
    );
    let active_members = &active_call_info.active_members;
    assert_eq!(active_members.len(), 2);
    assert!(active_members.contains(&BOB.to_owned()));
    assert!(active_members.contains(&CAROL.to_owned()));
    assert_eq!(active_call_info.call_started_ts_millis.unwrap(), notification_timestamp);

    let room_info = room.clone_info();
    assert_eq!(room_info.active_room_call_participants().len(), 2);

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_event(
                f.call_membership_state(ALICE.to_owned(), "ALICE_DEVICE".to_owned())
                    .lk_focus(room_id.into(), "https://livekit.localhow".to_owned())
                    .event_id(alice_membership),
            ),
        )
        .await;

    assert_let_timeout!(Some(timeline_updates) = timeline_stream.next());

    assert_let!(VectorDiff::Set { index: 1, value: updated_message } = &timeline_updates[0]);
    let event_item = updated_message.as_event().unwrap();
    assert_let!(
        TimelineItemContent::RtcNotification { active_call_info: Some(active_call_info), .. } =
            event_item.content()
    );
    let active_members = &active_call_info.active_members;
    assert_eq!(active_members.len(), 3);
    assert!(active_members.contains(&BOB.to_owned()));
    assert!(active_members.contains(&CAROL.to_owned()));
    assert!(active_members.contains(&ALICE.to_owned()));

    // Per spec, if there is now a new call notification event, then the active
    // call info should now be attached to it and remove from the previous one
    let new_notification_event_id = event_id!("$notify_new");
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.rtc_notification(NotificationType::Ring)
                    .sender(&ALICE)
                    .event_id(new_notification_event_id),
            ),
        )
        .await;

    assert_let_timeout!(Some(timeline_updates) = timeline_stream.next());

    // timeline_updates[0] is read receipt related
    assert_let!(VectorDiff::Set { index: 1, .. } = &timeline_updates[0]);
    // Update 1 is adding the new notification
    assert_let!(VectorDiff::PushBack { .. } = &timeline_updates[1]);

    // Should update the previous notification to clear it and add a new one
    assert_let!(VectorDiff::Set { index: 1, value: old_notification } = &timeline_updates[2]);
    let event_item = old_notification.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), notification_event_id);
    // Active call info should be None
    assert_let!(
        TimelineItemContent::RtcNotification { active_call_info: None, .. } = event_item.content()
    );

    assert_let!(VectorDiff::Set { index: 2, value: new_notification } = &timeline_updates[3]);
    let event_item = new_notification.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), new_notification_event_id);
    assert_let!(
        TimelineItemContent::RtcNotification { active_call_info: Some(active_call_info), .. } =
            event_item.content()
    );
    let active_members = &active_call_info.active_members;
    assert_eq!(active_members.len(), 3);
    assert!(active_members.contains(&BOB.to_owned()));
    assert!(active_members.contains(&CAROL.to_owned()));
    assert!(active_members.contains(&ALICE.to_owned()));
}

// There is an order in which the initial events are sent, first the membership
// event is sent, then the notification event is sent.
// When the membership event is received this will trigger an active call info
// update and we don't want that this first update is attached to the current
// last notification in the timeline as it is not the correct one, the correct
// one will come after. If this is not done then there will be a visual glitch
// in the timeline where the last rtc_notification will be updated and then
// replaced by the correct one (it will "jump")
#[async_test]
async fn test_only_update_notification_after_it_has_been_marked_as_last() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.rtc_notification(NotificationType::Ring)
                    .sender(&ALICE)
                    .event_id(event_id!("$pre-existing-notification")),
            ),
        )
        .await;

    // A call is started, so first the membership is added
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_event(
                f.call_membership_state(BOB.to_owned(), "BOB_DEVICE".to_owned())
                    .lk_focus(room_id.into(), "https://livekit.localhow".to_owned())
                    .event_id(event_id!("$bob-joined")),
            ),
        )
        .await;

    // Ensure that the existing notification timeline item has no info about the new
    // call
    assert_let_timeout!(Some(_timeline_updates) = timeline_stream.next());

    let items = timeline.items().await;
    let event_items: Vec<_> = items.iter().filter_map(|item| item.as_event()).collect();
    let notification = event_items[0];

    // Assert that active_call_info is none
    assert_let!(
        TimelineItemContent::RtcNotification { active_call_info: None, .. } =
            notification.content()
    );

    // Now the relevant notification is added
    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.rtc_notification(NotificationType::Ring)
                    .sender(&ALICE)
                    .event_id(event_id!("$call-notification")),
            ),
        )
        .await;

    assert_let_timeout!(Some(_timeline_updates) = timeline_stream.next());

    let items = timeline.items().await;
    let event_items: Vec<_> = items.iter().filter_map(|item| item.as_event()).collect();
    let notification = event_items[1];
    assert_eq!(notification.event_id().unwrap(), event_id!("$call-notification"));

    // Assert that active_call_info is none
    assert_let!(
        TimelineItemContent::RtcNotification { active_call_info: Some(active_call_info), .. } =
            notification.content()
    );
    assert!(active_call_info.active_members.contains(&BOB.to_owned()));

    // Simulate call ending
    // A call is started, so first the membership is added
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_event(
                f.call_membership_state(BOB.to_owned(), "BOB_DEVICE".to_owned())
                    .leave()
                    .event_id(event_id!("$bob-joined")),
            ),
        )
        .await;

    let items = timeline.items().await;
    let event_items: Vec<_> = items.iter().filter_map(|item| item.as_event()).collect();
    let notification = event_items[1];

    assert_eq!(notification.event_id().unwrap(), event_id!("$call-notification"));
    assert_let!(
        TimelineItemContent::RtcNotification { active_call_info: None, .. } =
            notification.content()
    );

    // If there is a new membership, then this is a new call, should not update the
    // old notification

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_event(
                f.call_membership_state(CAROL.to_owned(), "CAROL_DEVICE".to_owned())
                    .lk_focus(room_id.into(), "https://livekit.localhow".to_owned())
                    .event_id(event_id!("$carol-joined")),
            ),
        )
        .await;

    let items = timeline.items().await;
    let event_items: Vec<_> = items.iter().filter_map(|item| item.as_event()).collect();
    let notification = event_items[1];

    assert_eq!(notification.event_id().unwrap(), event_id!("$call-notification"));
    assert_let!(
        TimelineItemContent::RtcNotification { active_call_info: None, .. } =
            notification.content()
    );
}
