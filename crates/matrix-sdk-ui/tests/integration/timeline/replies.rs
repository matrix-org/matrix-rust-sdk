use std::time::Duration;

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk::{
    config::SyncSettings,
    test_utils::{events::EventFactory, logged_in_client_with_server},
};
use matrix_sdk_base::timeout::timeout;
use matrix_sdk_test::{
    async_test, mocks::mock_encryption_state, EventBuilder, JoinedRoomBuilder, SyncResponseBuilder,
    ALICE, BOB, CAROL,
};
use matrix_sdk_ui::timeline::{
    Error as TimelineError, EventSendState, RoomExt, TimelineDetails, TimelineItemContent,
};
use ruma::{
    event_id,
    events::{
        reaction::RedactedReactionEventContent,
        relation::InReplyTo,
        room::message::{
            AddMentions, ForwardThread, OriginalRoomMessageEvent, Relation, ReplyWithinThread,
            RoomMessageEventContent, RoomMessageEventContentWithoutRelation,
        },
        Mentions, MessageLikeUnsigned,
    },
    owned_event_id, owned_room_id, owned_user_id, room_id, uint, MilliSecondsSinceUnixEpoch,
};
use serde_json::json;
use stream_assert::assert_next_matches;
use tokio::task::yield_now;
use wiremock::{
    matchers::{header, method, path_regex},
    Mock, Request, ResponseTemplate,
};

use crate::mock_sync;

#[async_test]
async fn test_in_reply_to_details() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    // The event doesn't exist.
    assert_matches!(
        timeline.fetch_details_for_event(event_id!("$fakeevent")).await,
        Err(TimelineError::RemoteEventNotInTimeline)
    );

    // Add an event and a reply to that event to the timeline
    let event_id_1 = event_id!("$event1");
    let f = EventFactory::new();
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(f.text_msg("hello").sender(*ALICE).event_id(event_id_1))
            .add_timeline_event(f.text_msg("hello to you too").reply_to(event_id_1).sender(*BOB)),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::PushBack { value: first }) = timeline_stream.next().await);
    assert_matches!(first.as_event().unwrap().content(), TimelineItemContent::Message(_));

    assert_let!(Some(VectorDiff::PushBack { value: second }) = timeline_stream.next().await);
    let second_event = second.as_event().unwrap();
    assert_let!(TimelineItemContent::Message(message) = second_event.content());
    let in_reply_to = message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, event_id!("$event1"));
    assert_matches!(in_reply_to.event, TimelineDetails::Ready(_));

    assert_let!(Some(VectorDiff::PushFront { value: day_divider }) = timeline_stream.next().await);
    assert!(day_divider.is_day_divider());

    // Add an reply to an unknown event to the timeline
    let event_id_2 = event_id!("$event2");
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(f.text_msg("you were right").reply_to(event_id_2).sender(*BOB)),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(
        Some(VectorDiff::Set { value: _read_receipt_update, .. }) = timeline_stream.next().await
    );

    assert_let!(Some(VectorDiff::PushBack { value: third }) = timeline_stream.next().await);
    let third_event = third.as_event().unwrap();
    assert_let!(TimelineItemContent::Message(message) = third_event.content());
    let in_reply_to = message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, event_id_2);
    assert_matches!(in_reply_to.event, TimelineDetails::Unavailable);

    let unique_id = third.unique_id().to_owned();

    // Set up fetching the replied-to event to fail
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/event/\$event2"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "Event not found.",
        })))
        .expect(1)
        .mount(&server)
        .await;

    // Fetch details remotely if we can't find them locally.
    timeline.fetch_details_for_event(third_event.event_id().unwrap()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::Set { index: 3, value: third }) = timeline_stream.next().await);
    assert_let!(TimelineItemContent::Message(message) = third.as_event().unwrap().content());
    assert_matches!(message.in_reply_to().unwrap().event, TimelineDetails::Pending);
    assert_eq!(third.unique_id(), unique_id);

    assert_let!(Some(VectorDiff::Set { index: 3, value: third }) = timeline_stream.next().await);
    assert_let!(TimelineItemContent::Message(message) = third.as_event().unwrap().content());
    assert_matches!(message.in_reply_to().unwrap().event, TimelineDetails::Error(_));
    assert_eq!(third.unique_id(), unique_id);

    // Set up fetching the replied-to event to succeed
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/event/\$event2"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                f.text_msg("Alice is gonna arrive soon")
                    .sender(*CAROL)
                    .room(room_id)
                    .event_id(event_id_2)
                    .into_raw_timeline(),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;

    timeline.fetch_details_for_event(third_event.event_id().unwrap()).await.unwrap();

    assert_let!(Some(VectorDiff::Set { index: 3, value: third }) = timeline_stream.next().await);
    assert_let!(TimelineItemContent::Message(message) = third.as_event().unwrap().content());
    assert_matches!(message.in_reply_to().unwrap().event, TimelineDetails::Pending);
    assert_eq!(third.unique_id(), unique_id);

    assert_let!(Some(VectorDiff::Set { index: 3, value: third }) = timeline_stream.next().await);
    assert_let!(TimelineItemContent::Message(message) = third.as_event().unwrap().content());
    assert_matches!(message.in_reply_to().unwrap().event, TimelineDetails::Ready(_));
    assert_eq!(third.unique_id(), unique_id);
}

#[async_test]
async fn test_transfer_in_reply_to_details_to_re_received_item() {
    let f = EventFactory::new();

    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();

    // Given a reply to an event that's not itself in the timeline...
    let event_id_1 = event_id!("$event1");
    let reply_event = f.text_msg("Reply").sender(&BOB).reply_to(event_id_1).into_raw_sync();
    sync_builder
        .add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(reply_event.clone()));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let items = timeline.items().await;
    assert_eq!(items.len(), 2); // day divider, reply
    let event_item = items[1].as_event().unwrap();
    let in_reply_to = event_item.content().as_message().unwrap().in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, event_id_1);
    assert_matches!(in_reply_to.event, TimelineDetails::Unavailable);

    // ... when we fetch the reply details for that item
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/event/\$event1"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                f.text_msg("Original Message")
                    .sender(&ALICE)
                    .room(room_id)
                    .event_id(event_id_1)
                    .into_raw_timeline()
                    .json(),
            ),
        )
        .expect(1)
        .mount(&server)
        .await;

    timeline.fetch_details_for_event(event_item.event_id().unwrap()).await.unwrap();

    // (which succeeds)
    let items = timeline.items().await;
    assert_eq!(items.len(), 2);
    let in_reply_to =
        items[1].as_event().unwrap().content().as_message().unwrap().in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, event_id_1);
    assert_matches!(in_reply_to.event, TimelineDetails::Ready(_));

    // ... and then we re-receive the reply event
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(reply_event));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();

    // ... the replied-to event details should remain from when we fetched them
    let items = timeline.items().await;
    assert_eq!(items.len(), 2);
    let in_reply_to =
        items[1].as_event().unwrap().content().as_message().unwrap().in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, event_id_1);
    assert_matches!(in_reply_to.event, TimelineDetails::Ready(_));
}

#[async_test]
async fn test_send_reply() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let event_builder = EventBuilder::new();
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let event_id_from_bob = event_id!("$event_from_bob");
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        event_builder.make_sync_message_event_with_id(
            &BOB,
            event_id_from_bob,
            RoomMessageEventContent::text_plain("Hello from Bob"),
        ),
    ));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let event_from_bob =
        assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);

    // Clear the timeline to make sure the old item does not need to be
    // available in it for the reply to work.
    timeline.clear().await;
    assert_next_matches!(timeline_stream, VectorDiff::Clear);

    mock_encryption_state(&server, false).await;

    // Now, let's reply to a message sent by `BOB`.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .respond_with(move |req: &Request| {
            use ruma::events::room::message::RoomMessageEventContent;

            let reply_event = req
                .body_json::<RoomMessageEventContent>()
                .expect("Failed to deserialize the event");

            assert_matches!(reply_event.relates_to, Some(Relation::Reply { in_reply_to: InReplyTo { event_id, .. } }) => {
                assert_eq!(event_id, event_id_from_bob);
            });
            assert_matches!(reply_event.mentions, Some(Mentions { user_ids, room: false, .. }) => {
                assert_eq!(user_ids.len(), 1);
                assert!(user_ids.contains(*BOB));
            });

            ResponseTemplate::new(200).set_body_json(json!({ "event_id": "$reply_event" }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let replied_to_info = event_from_bob.replied_to_info().unwrap();
    timeline
        .send_reply(
            RoomMessageEventContentWithoutRelation::text_plain("Replying to Bob"),
            replied_to_info,
            ForwardThread::Yes,
        )
        .await
        .unwrap();

    // Let the send queue handle the event.
    yield_now().await;

    let reply_item = assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);

    assert_matches!(reply_item.send_state(), Some(EventSendState::NotSentYet));
    let reply_message = reply_item.content().as_message().unwrap();
    assert_eq!(reply_message.body(), "Replying to Bob");
    let in_reply_to = reply_message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, event_id_from_bob);
    // Right now, we don't pass along the replied-to event to the event handler,
    // so it's not available if the timeline got cleared. Not critical, but
    // there's notable room for improvement here.
    //
    // let replied_to_event =
    // assert_matches!(&in_reply_to.event, TimelineDetails::Ready(ev) => ev);
    // assert_eq!(replied_to_event.sender(), *BOB);

    let diff = timeout(timeline_stream.next(), Duration::from_secs(1)).await.unwrap().unwrap();
    assert_let!(VectorDiff::Set { index: 0, value: reply_item_remote_echo } = diff);

    assert_matches!(reply_item_remote_echo.send_state(), Some(EventSendState::Sent { .. }));
    let reply_message = reply_item_remote_echo.content().as_message().unwrap();
    assert_eq!(reply_message.body(), "Replying to Bob");
    let in_reply_to = reply_message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, event_id_from_bob);
    // Same as above.
    //
    // let replied_to_event =
    // assert_matches!(&in_reply_to.event, TimelineDetails::Ready(ev) =>
    // ev); assert_eq!(replied_to_event.sender(), *BOB);

    server.verify().await;
}

#[async_test]
async fn test_send_reply_to_self() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let event_builder = EventBuilder::new();
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let event_id_from_self = event_id!("$event_from_self");
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        event_builder.make_sync_message_event_with_id(
            client.user_id().expect("Client must have a user ID"),
            event_id_from_self,
            RoomMessageEventContent::text_plain("Hello from self"),
        ),
    ));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let event_from_self =
        assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);

    // Clear the timeline to make sure the old item does not need to be
    // available in it for the reply to work.
    timeline.clear().await;
    assert_next_matches!(timeline_stream, VectorDiff::Clear);

    mock_encryption_state(&server, false).await;

    // Now, let's reply to a message sent by the current user.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .respond_with(move |req: &Request| {
            use ruma::events::room::message::RoomMessageEventContent;

            let reply_event = req
                .body_json::<RoomMessageEventContent>()
                .expect("Failed to deserialize the event");

            assert_matches!(reply_event.relates_to, Some(Relation::Reply { in_reply_to: InReplyTo { event_id, .. } }) => {
                assert_eq!(event_id, event_id_from_self);
            });
            assert!(reply_event.mentions.is_none());

            ResponseTemplate::new(200).set_body_json(json!({ "event_id": "$reply_event" }))
        })
        .expect(1)
        .mount(&server)
        .await;

    let replied_to_info = event_from_self.replied_to_info().unwrap();
    timeline
        .send_reply(
            RoomMessageEventContentWithoutRelation::text_plain("Replying to self"),
            replied_to_info,
            ForwardThread::Yes,
        )
        .await
        .unwrap();

    // Let the send queue handle the event.
    yield_now().await;

    let reply_item = assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);

    assert_matches!(reply_item.send_state(), Some(EventSendState::NotSentYet));
    let reply_message = reply_item.content().as_message().unwrap();
    assert_eq!(reply_message.body(), "Replying to self");
    let in_reply_to = reply_message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, event_id_from_self);

    let diff = timeout(timeline_stream.next(), Duration::from_secs(1)).await.unwrap().unwrap();
    assert_let!(VectorDiff::Set { index: 0, value: reply_item_remote_echo } = diff);

    assert_matches!(reply_item_remote_echo.send_state(), Some(EventSendState::Sent { .. }));
    let reply_message = reply_item_remote_echo.content().as_message().unwrap();
    assert_eq!(reply_message.body(), "Replying to self");
    let in_reply_to = reply_message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, event_id_from_self);

    server.verify().await;
}

#[async_test]
async fn test_send_reply_to_threaded() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let event_builder = EventBuilder::new();
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let thread_root = OriginalRoomMessageEvent {
        content: RoomMessageEventContent::text_plain("Thread root"),
        event_id: owned_event_id!("$thread_root"),
        origin_server_ts: MilliSecondsSinceUnixEpoch(uint!(10_000)),
        room_id: owned_room_id!("!testroomid:example.org"),
        sender: owned_user_id!("@user:example.org"),
        unsigned: MessageLikeUnsigned::default(),
    };

    let event_id_1 = event_id!("$event1");
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        event_builder.make_sync_message_event_with_id(
            &BOB,
            event_id_1,
            RoomMessageEventContent::text_plain("Hello, World!").make_for_thread(
                &thread_root,
                ReplyWithinThread::No,
                AddMentions::No,
            ),
        ),
    ));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let hello_world_item =
        assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);

    mock_encryption_state(&server, false).await;
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "event_id": "$reply_event" })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let replied_to_info = hello_world_item.replied_to_info().unwrap();
    timeline
        .send_reply(
            RoomMessageEventContentWithoutRelation::text_plain("Hello, Bob!"),
            replied_to_info,
            ForwardThread::Yes,
        )
        .await
        .unwrap();

    // Let the send queue handle the event.
    yield_now().await;

    let reply_item = assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);

    assert_matches!(reply_item.send_state(), Some(EventSendState::NotSentYet));
    let reply_message = reply_item.content().as_message().unwrap();

    // The reply should be considered part of the thread.
    assert!(reply_message.is_threaded());

    // Some extra assertions.
    assert_eq!(reply_message.body(), "Hello, Bob!");
    let in_reply_to = reply_message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, event_id_1);

    assert_let!(TimelineDetails::Ready(replied_to_event) = &in_reply_to.event);
    assert_eq!(replied_to_event.sender(), *BOB);

    // Wait for remote echo.
    let diff = timeout(timeline_stream.next(), Duration::from_secs(1)).await.unwrap().unwrap();
    assert_let!(VectorDiff::Set { index: 1, value: reply_item_remote_echo } = diff);

    assert_matches!(reply_item_remote_echo.send_state(), Some(EventSendState::Sent { .. }));

    // Same assertions as before still hold on the contained message.
    assert!(reply_message.is_threaded());

    assert_eq!(reply_message.body(), "Hello, Bob!");
    let in_reply_to = reply_message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, event_id_1);

    assert_let!(TimelineDetails::Ready(replied_to_event) = &in_reply_to.event);
    assert_eq!(replied_to_event.sender(), *BOB);

    server.verify().await;
}

#[async_test]
async fn test_send_reply_with_event_id() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let event_builder = EventBuilder::new();
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let event_id_from_bob = event_id!("$event_from_bob");
    let raw_event_from_bob = event_builder.make_sync_message_event_with_id(
        &BOB,
        event_id_from_bob,
        RoomMessageEventContent::text_plain("Hello from Bob"),
    );
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id).add_timeline_event(raw_event_from_bob.clone()),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let event_from_bob =
        assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);
    assert_eq!(event_from_bob.event_id().unwrap(), event_id_from_bob);

    // Clear the timeline to make sure the old item does not need to be
    // available in it for the reply to work.
    timeline.clear().await;
    assert_next_matches!(timeline_stream, VectorDiff::Clear);

    mock_encryption_state(&server, false).await;

    // Now, let's reply to a message sent by `BOB`.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .respond_with(move |req: &Request| {
            use ruma::events::room::message::RoomMessageEventContent;

            let reply_event = req
                .body_json::<RoomMessageEventContent>()
                .expect("Failed to deserialize the event");

            assert_matches!(reply_event.relates_to, Some(Relation::Reply { in_reply_to: InReplyTo { event_id, .. } }) => {
                assert_eq!(event_id, event_id_from_bob);
            });
            assert_matches!(reply_event.mentions, Some(Mentions { user_ids, room: false, .. }) => {
                assert_eq!(user_ids.len(), 1);
                assert!(user_ids.contains(*BOB));
            });

            ResponseTemplate::new(200).set_body_json(json!({ "event_id": "$reply_event" }))
        })
        .expect(1)
        .mount(&server)
        .await;

    // Since we assume we can't use the timeline item directly in this use case, the
    // API will fetch the event from the server directly so we need to mock the
    // response.
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/event/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(raw_event_from_bob.json()))
        .expect(1)
        .named("event_1")
        .mount(&server)
        .await;

    let replied_to_info = timeline.replied_to_info_from_event_id(event_id_from_bob).await.unwrap();
    timeline
        .send_reply(
            RoomMessageEventContentWithoutRelation::text_plain("Replying to Bob"),
            replied_to_info,
            ForwardThread::Yes,
        )
        .await
        .unwrap();

    // Let the sending queue handle the event.
    yield_now().await;

    let reply_item = assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);

    assert_matches!(reply_item.send_state(), Some(EventSendState::NotSentYet));
    let reply_message = reply_item.content().as_message().unwrap();
    assert_eq!(reply_message.body(), "Replying to Bob");
    let in_reply_to = reply_message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, event_id_from_bob);

    let diff = timeout(timeline_stream.next(), Duration::from_secs(1)).await.unwrap().unwrap();
    assert_let!(VectorDiff::Set { index: 0, value: reply_item_remote_echo } = diff);

    assert_matches!(reply_item_remote_echo.send_state(), Some(EventSendState::Sent { .. }));
    let reply_message = reply_item_remote_echo.content().as_message().unwrap();
    assert_eq!(reply_message.body(), "Replying to Bob");
    let in_reply_to = reply_message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, event_id_from_bob);

    server.verify().await;
}

#[async_test]
async fn test_send_reply_with_event_id_that_is_redacted() {
    // This test checks if is possible to reply to a redacted event that is not in
    // the timeline. The event id will go through a process where the event is
    // fetched and the content will be extracted and deserialised to be used in
    // the reply.
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let event_builder = EventBuilder::new();
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let redacted_event_id_from_bob = event_id!("$event_from_bob");
    let raw_redacted_event_from_bob = event_builder.make_sync_redacted_message_event_with_id(
        &BOB,
        redacted_event_id_from_bob,
        RedactedReactionEventContent::new(),
    );
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id).add_timeline_event(raw_redacted_event_from_bob.clone()),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Clear the timeline to make sure the old item does not need to be
    // available in it for the reply to work.
    timeline.clear().await;
    assert_next_matches!(timeline_stream, VectorDiff::Clear);

    mock_encryption_state(&server, false).await;

    // Now, let's reply to a message sent by `BOB`.
    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .respond_with(move |req: &Request| {
            use ruma::events::room::message::RoomMessageEventContent;

            let reply_event = req
                .body_json::<RoomMessageEventContent>()
                .expect("Failed to deserialize the event");

            assert_matches!(reply_event.relates_to, Some(Relation::Reply { in_reply_to: InReplyTo { event_id, .. } }) => {
                assert_eq!(event_id, redacted_event_id_from_bob);
            });
            assert_matches!(reply_event.mentions, Some(Mentions { user_ids, room: false, .. }) => {
                assert_eq!(user_ids.len(), 1);
                assert!(user_ids.contains(*BOB));
            });

            ResponseTemplate::new(200).set_body_json(json!({ "event_id": "$reply_event" }))
        })
        .expect(1)
        .mount(&server)
        .await;

    // Since we assume we can't use the timeline item directly in this use case, the
    // API will fetch the event from the server directly so we need to mock the
    // response.
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/event/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(raw_redacted_event_from_bob.json()))
        .expect(1)
        .named("event_1")
        .mount(&server)
        .await;

    let replied_to_info =
        timeline.replied_to_info_from_event_id(redacted_event_id_from_bob).await.unwrap();
    timeline
        .send_reply(
            RoomMessageEventContentWithoutRelation::text_plain("Replying to Bob"),
            replied_to_info,
            ForwardThread::Yes,
        )
        .await
        .unwrap();

    // Let the sending queue handle the event.
    yield_now().await;

    let reply_item = assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);

    assert_matches!(reply_item.send_state(), Some(EventSendState::NotSentYet));
    let reply_message = reply_item.content().as_message().unwrap();
    assert_eq!(reply_message.body(), "Replying to Bob");
    let in_reply_to = reply_message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, redacted_event_id_from_bob);

    let diff = timeout(timeline_stream.next(), Duration::from_secs(1)).await.unwrap().unwrap();
    assert_let!(VectorDiff::Set { index: 0, value: reply_item_remote_echo } = diff);

    assert_matches!(reply_item_remote_echo.send_state(), Some(EventSendState::Sent { .. }));
    let reply_message = reply_item_remote_echo.content().as_message().unwrap();
    assert_eq!(reply_message.body(), "Replying to Bob");
    let in_reply_to = reply_message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, redacted_event_id_from_bob);

    server.verify().await;
}
