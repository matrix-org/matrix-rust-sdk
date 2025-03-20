use std::time::Duration;

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk::test_utils::mocks::MatrixMockServer;
use matrix_sdk_base::timeout::timeout;
use matrix_sdk_test::{
    async_test, event_factory::EventFactory, JoinedRoomBuilder, ALICE, BOB, CAROL,
};
use matrix_sdk_ui::timeline::{
    EnforceThread, Error as TimelineError, EventSendState, RoomExt, TimelineDetails, TimelineItemContent
};
use ruma::{
    event_id,
    events::{
        reaction::RedactedReactionEventContent,
        relation::InReplyTo,
        room::{
            encrypted::{
                EncryptedEventScheme, MegolmV1AesSha2ContentInit, RoomEncryptedEventContent,
            },
            message::{
                ForwardThread, Relation, ReplyWithinThread, RoomMessageEventContentWithoutRelation,
            },
            ImageInfo,
        },
        sticker::{StickerEventContent, StickerMediaSource},
        Mentions,
    },
    owned_event_id, owned_mxc_uri, room_id,
};
use serde_json::json;
use stream_assert::{assert_next_matches, assert_pending};
use tokio::task::yield_now;
use wiremock::{
    matchers::{header, method, path_regex},
    Mock, Request, ResponseTemplate,
};

#[async_test]
async fn test_in_reply_to_details_when_event_doesnt_exist() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    let timeline = room.timeline().await.unwrap();

    // The event doesn't exist.
    assert_matches!(
        timeline.fetch_details_for_event(event_id!("$fakeevent")).await,
        Err(TimelineError::EventNotInTimeline(_))
    );
}

#[async_test]
async fn test_in_reply_to_details() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    // Add an event and a reply to that event to the timeline
    let eid1 = event_id!("$event1");
    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("hello").sender(*ALICE).event_id(eid1))
                .add_timeline_event(f.text_msg("hello to you too").reply_to(eid1).sender(*BOB)),
        )
        .await;

    {
        assert_let!(Some(timeline_updates) = timeline_stream.next().await);
        assert_eq!(timeline_updates.len(), 3);

        // We get the original message.
        assert_let!(VectorDiff::PushBack { value: first } = &timeline_updates[0]);
        assert_matches!(first.as_event().unwrap().content(), TimelineItemContent::Message(_));

        // We get the reply.
        assert_let!(VectorDiff::PushBack { value: second } = &timeline_updates[1]);
        let second_event = second.as_event().unwrap();
        assert_let!(TimelineItemContent::Message(message) = second_event.content());
        let in_reply_to = message.in_reply_to().unwrap();
        assert_eq!(in_reply_to.event_id, eid1);
        assert_matches!(in_reply_to.event, TimelineDetails::Ready(_));

        // Good old date divider.
        assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[2]);
        assert!(date_divider.is_date_divider());
    }

    // Add an reply to an unknown event to the timeline.
    let eid2 = event_id!("$event2");
    let eid3 = event_id!("$event3");
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("you were right").reply_to(eid2).sender(*BOB).event_id(eid3),
            ),
        )
        .await;

    let third_unique_id = {
        assert_let!(Some(timeline_updates) = timeline_stream.next().await);
        assert_eq!(timeline_updates.len(), 2);

        assert_let!(VectorDiff::Set { value: _read_receipt_update, .. } = &timeline_updates[0]);

        assert_let!(VectorDiff::PushBack { value: third } = &timeline_updates[1]);
        let third_event = third.as_event().unwrap();
        assert_let!(TimelineItemContent::Message(message) = third_event.content());
        let in_reply_to = message.in_reply_to().unwrap();
        assert_eq!(in_reply_to.event_id, eid2);
        assert_matches!(in_reply_to.event, TimelineDetails::Unavailable);

        third.unique_id().clone()
    };

    // Set up fetching the replied-to event to fail
    let scoped_room_event = Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/v3/rooms/.*/event/\$event2"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "errcode": "M_NOT_FOUND",
            "error": "Event not found.",
        })))
        .expect(1)
        .mount_as_scoped(server.server())
        .await;

    // Fetch details remotely if we can't find them locally.
    timeline.fetch_details_for_event(eid3).await.unwrap();

    {
        assert_let!(Some(timeline_updates) = timeline_stream.next().await);
        assert_eq!(timeline_updates.len(), 2);

        // First it's set to pending, because we're starting the request…
        assert_let!(VectorDiff::Set { index: 3, value: third } = &timeline_updates[0]);
        assert_let!(TimelineItemContent::Message(message) = third.as_event().unwrap().content());
        assert_matches!(message.in_reply_to().unwrap().event, TimelineDetails::Pending);
        assert_eq!(*third.unique_id(), third_unique_id);

        // …then it's marked as an error when the request fails.
        assert_let!(VectorDiff::Set { index: 3, value: third } = &timeline_updates[1]);
        assert_let!(TimelineItemContent::Message(message) = third.as_event().unwrap().content());
        assert_matches!(message.in_reply_to().unwrap().event, TimelineDetails::Error(_));
        assert_eq!(*third.unique_id(), third_unique_id);
    }

    // Set up fetching the replied-to event to succeed.
    drop(scoped_room_event);
    server
        .mock_room_event()
        .match_event_id()
        .ok(f
            .text_msg("Alice is gonna arrive soon")
            .sender(*CAROL)
            .room(room_id)
            .event_id(eid2)
            .into())
        .mock_once()
        .mount()
        .await;

    timeline.fetch_details_for_event(eid3).await.unwrap();

    {
        assert_let!(Some(timeline_updates) = timeline_stream.next().await);
        assert_eq!(timeline_updates.len(), 2);

        // First it's set to pending, because we're starting the request…
        assert_let!(VectorDiff::Set { index: 3, value: third } = &timeline_updates[0]);
        assert_let!(TimelineItemContent::Message(message) = third.as_event().unwrap().content());
        assert_matches!(message.in_reply_to().unwrap().event, TimelineDetails::Pending);
        assert_eq!(*third.unique_id(), third_unique_id);

        // …then it's filled when the request succeeds.
        assert_let!(VectorDiff::Set { index: 3, value: third } = &timeline_updates[1]);
        assert_let!(TimelineItemContent::Message(message) = third.as_event().unwrap().content());
        assert_matches!(message.in_reply_to().unwrap().event, TimelineDetails::Ready(_));
        assert_eq!(*third.unique_id(), third_unique_id);

        assert_pending!(timeline_stream);
    }
}

#[async_test]
async fn test_fetch_details_utd() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().encrypted().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    // Add an encrypted event and a reply to that event to the timeline.
    let f = EventFactory::new();

    let replied_to_event_id = event_id!("$original");
    let response_event_id = event_id!("$response");
    let encrypted = EncryptedEventScheme::MegolmV1AesSha2(
        MegolmV1AesSha2ContentInit {
            ciphertext: "\
                AwgAEtABWuWeRLintqVP5ez5kki8sDsX7zSq++9AJo9lELGTDjNKzbF8sowUgg0DaGoP\
                dgWyBmuUxT2bMggwM0fAevtu4XcFtWUx1c/sj1vhekrng9snmXpz4a30N8jhQ7N4WoIg\
                /G5wsPKtOITjUHeon7EKjTPFU7xoYXmxbjDL/9R4hGQdRqogs1hj0ZnWRxNCvr3ahq24\
                E0j8WyBrQXOb2PIHVNfV/9eW8AB744UQXn8FJpmQO8c0Us3YorXtIFrwAtvI3FknD7Lj\
                eeYFpR9oeyZKuzo2Wzp7eiEZt0Lm+xb7Lfp9yY52RhAO7JLlCM4oPff2yXHpUmcjdGsi\
                9Zc9Z92hiILkZoKOSGccYQoLjYlfL8rVsIVvl4tDDQ"
                .to_owned(),
            sender_key: "sKSGv2uD9zUncgL6GiLedvuky3fjVcEz9qVKZkpzN14".to_owned(),
            device_id: "PNQBRWYIJL".into(),
            session_id: "HSRlM67FgLYl0J0l1luflfGwpnFcLKHnNoRqUuIhQ5Q".into(),
        }
        .into(),
    );

    server
        .mock_room_event()
        .match_event_id()
        .ok(f
            .event(RoomEncryptedEventContent::new(encrypted, None))
            .sender(*ALICE)
            .event_id(replied_to_event_id)
            .into_utd_sync_timeline_event())
        .mock_once()
        .mount()
        .await;

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("UTD, can you rageshake?")
                    .reply_to(replied_to_event_id)
                    .sender(*BOB)
                    .event_id(response_event_id),
            ),
        )
        .await;

    {
        assert_let!(Some(timeline_updates) = timeline_stream.next().await);
        assert_eq!(timeline_updates.len(), 2);

        // We get the reply, but with no details.
        assert_let!(VectorDiff::PushBack { value: second } = &timeline_updates[0]);
        let second_event = second.as_event().unwrap();
        assert_let!(TimelineItemContent::Message(message) = second_event.content());
        let in_reply_to = message.in_reply_to().unwrap();
        assert_eq!(in_reply_to.event_id, replied_to_event_id);
        assert_matches!(in_reply_to.event, TimelineDetails::Unavailable);

        // Good old date divider.
        assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[1]);
        assert!(date_divider.is_date_divider());
    }

    // Now fetch details.
    timeline.fetch_details_for_event(response_event_id).await.unwrap();

    {
        assert_let!(Some(timeline_updates) = timeline_stream.next().await);
        assert_eq!(timeline_updates.len(), 2);

        // First it's set to pending, because we're starting the request…
        assert_let!(VectorDiff::Set { index: 1, value } = &timeline_updates[0]);
        assert_let!(TimelineItemContent::Message(message) = value.as_event().unwrap().content());
        assert_matches!(message.in_reply_to().unwrap().event, TimelineDetails::Pending);

        // …then it's filled as the request succeeds.
        assert_let!(VectorDiff::Set { index: 1, value } = &timeline_updates[1]);
        assert_let!(TimelineItemContent::Message(message) = value.as_event().unwrap().content());

        // The replied-to event is available, and is a UTD.
        let in_reply_to = message.in_reply_to().unwrap();
        assert_let!(TimelineDetails::Ready(replied_to) = &in_reply_to.event);
        assert_eq!(replied_to.sender(), *ALICE);
        assert_matches!(replied_to.content(), TimelineItemContent::UnableToDecrypt(_));
    }
}

#[async_test]
async fn test_fetch_details_poll() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    // Add an encrypted event and a reply to that event to the timeline.
    let f = EventFactory::new();

    let replied_to_event_id = event_id!("$original");
    let response_event_id = event_id!("$response");

    server
        .mock_room_event()
        .match_event_id()
        .ok(f
            .poll_start(
                "What is the best color? A. Red, B. Blue, C. Green",
                "What is the best color?",
                vec!["Red", "Blue", "Green"],
            )
            .sender(*ALICE)
            .event_id(replied_to_event_id)
            .into())
        .mock_once()
        .mount()
        .await;

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("wow, that's a tough one!")
                    .reply_to(replied_to_event_id)
                    .sender(*BOB)
                    .event_id(response_event_id),
            ),
        )
        .await;

    {
        assert_let!(Some(timeline_updates) = timeline_stream.next().await);
        assert_eq!(timeline_updates.len(), 2);

        // We get the reply, but with no details.
        assert_let!(VectorDiff::PushBack { value: second } = &timeline_updates[0]);
        let second_event = second.as_event().unwrap();
        assert_let!(TimelineItemContent::Message(message) = second_event.content());
        let in_reply_to = message.in_reply_to().unwrap();
        assert_eq!(in_reply_to.event_id, replied_to_event_id);
        assert_matches!(in_reply_to.event, TimelineDetails::Unavailable);

        // Good old date divider.
        assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[1]);
        assert!(date_divider.is_date_divider());
    }

    // Now fetch details.
    timeline.fetch_details_for_event(response_event_id).await.unwrap();

    {
        assert_let!(Some(timeline_updates) = timeline_stream.next().await);
        assert_eq!(timeline_updates.len(), 2);

        // First it's set to pending, because we're starting the request…
        assert_let!(VectorDiff::Set { index: 1, value } = &timeline_updates[0]);
        assert_let!(TimelineItemContent::Message(message) = value.as_event().unwrap().content());
        assert_matches!(message.in_reply_to().unwrap().event, TimelineDetails::Pending);

        // …then it's filled as the request succeeds.
        assert_let!(VectorDiff::Set { index: 1, value } = &timeline_updates[1]);
        assert_let!(TimelineItemContent::Message(message) = value.as_event().unwrap().content());

        // The replied-to event is available, and is the poll.
        let in_reply_to = message.in_reply_to().unwrap();
        assert_let!(TimelineDetails::Ready(replied_to) = &in_reply_to.event);
        assert_eq!(replied_to.sender(), *ALICE);
        assert_let!(TimelineItemContent::Poll(poll_state) = replied_to.content());
        assert_eq!(
            poll_state.fallback_text().unwrap(),
            "What is the best color? A. Red, B. Blue, C. Green"
        );
    }
}

#[async_test]
async fn test_fetch_details_sticker() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    // Add a sticker event and a reply to that event to the timeline.
    let f = EventFactory::new();

    let replied_to_event_id = event_id!("$original");
    let response_event_id = event_id!("$response");
    let media_src = owned_mxc_uri!("mxc://example.com/1");
    server
        .mock_room_event()
        .match_event_id()
        .ok(f
            .event(StickerEventContent::new(
                "sticker!".to_owned(),
                ImageInfo::new(),
                media_src.clone(),
            ))
            .sender(*ALICE)
            .event_id(replied_to_event_id)
            .into())
        .mock_once()
        .mount()
        .await;

    let media_src = owned_mxc_uri!("mxc://example.com/1");
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("gorgious")
                    .reply_to(replied_to_event_id)
                    .sender(*BOB)
                    .event_id(response_event_id),
            ),
        )
        .await;

    {
        assert_let!(Some(timeline_updates) = timeline_stream.next().await);
        assert_eq!(timeline_updates.len(), 2);

        // We get the reply.
        assert_let!(VectorDiff::PushBack { value: second } = &timeline_updates[0]);
        let second_event = second.as_event().unwrap();
        assert_let!(TimelineItemContent::Message(message) = second_event.content());
        let in_reply_to = message.in_reply_to().unwrap();
        assert_eq!(in_reply_to.event_id, replied_to_event_id);
        assert_matches!(in_reply_to.event, TimelineDetails::Unavailable);

        // Good old date divider.
        assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[1]);
        assert!(date_divider.is_date_divider());
    }

    // Now fetch details.
    timeline.fetch_details_for_event(response_event_id).await.unwrap();

    {
        assert_let!(Some(timeline_updates) = timeline_stream.next().await);
        assert_eq!(timeline_updates.len(), 2);

        // First it's set to pending, because we're starting the request…
        assert_let!(VectorDiff::Set { index: 1, value } = &timeline_updates[0]);
        assert_let!(TimelineItemContent::Message(message) = value.as_event().unwrap().content());
        assert_matches!(message.in_reply_to().unwrap().event, TimelineDetails::Pending);

        // …then it's filled as the request succeeds.
        assert_let!(VectorDiff::Set { index: 1, value } = &timeline_updates[1]);
        assert_let!(TimelineItemContent::Message(message) = value.as_event().unwrap().content());

        // The replied-to event is available, and is a sticker.
        let in_reply_to = message.in_reply_to().unwrap();
        assert_let!(TimelineDetails::Ready(replied_to) = &in_reply_to.event);
        assert_eq!(replied_to.sender(), *ALICE);
        assert_let!(TimelineItemContent::Sticker(sticker) = replied_to.content());
        assert_eq!(sticker.content().body, "sticker!");
        assert_matches!(&sticker.content().source, StickerMediaSource::Plain(src) => {
            assert_eq!(*src, media_src);
        });
    }
}

#[async_test]
async fn test_transfer_in_reply_to_details_to_re_received_item() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let f = EventFactory::new();

    let timeline = room.timeline().await.unwrap();

    // Given a reply to an event that's not itself in the timeline...
    let event_id_1 = event_id!("$event1");
    let reply_event = f.text_msg("Reply").sender(&BOB).reply_to(event_id_1).into_raw_sync();
    server
        .sync_room(&client, JoinedRoomBuilder::new(room_id).add_timeline_event(reply_event.clone()))
        .await;

    let items = timeline.items().await;
    assert_eq!(items.len(), 2); // date divider, reply
    let event_item = items[1].as_event().unwrap();
    let in_reply_to = event_item.content().as_message().unwrap().in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, event_id_1);
    assert_matches!(in_reply_to.event, TimelineDetails::Unavailable);

    // ... when we fetch the reply details for that item
    server
        .mock_room_event()
        .match_event_id()
        .ok(f.text_msg("Original Message").sender(&ALICE).room(room_id).event_id(event_id_1).into())
        .mock_once()
        .mount()
        .await;

    timeline.fetch_details_for_event(event_item.event_id().unwrap()).await.unwrap();

    // (which succeeds)
    let items = timeline.items().await;
    assert_eq!(items.len(), 2);
    let in_reply_to =
        items[1].as_event().unwrap().content().as_message().unwrap().in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, event_id_1);
    assert_matches!(in_reply_to.event, TimelineDetails::Ready(_));

    // ... and then we re-receive the reply event.
    server
        .sync_room(&client, JoinedRoomBuilder::new(room_id).add_timeline_event(reply_event))
        .await;

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
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let event_id_from_bob = event_id!("$event_from_bob");
    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("Hello from Bob").sender(&BOB).event_id(event_id_from_bob),
            ),
        )
        .await;

    let event_from_bob =
        assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);

    // Clear the timeline to make sure the old item does not need to be
    // available in it for the reply to work.
    timeline.clear().await;
    assert_next_matches!(timeline_stream, VectorDiff::Clear);

    // Now, let's reply to a message sent by `BOB`.
    server.mock_room_send()
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
        .mount()
        .await;

    let replied_to_info = event_from_bob.replied_to_info().unwrap();
    timeline
        .send_reply(
            RoomMessageEventContentWithoutRelation::text_plain("Replying to Bob"),
            replied_to_info,
            EnforceThread::No(ForwardThread::Yes),
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
}

#[async_test]
async fn test_send_reply_to_self() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let event_id_from_self = event_id!("$event_from_self");
    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("Hello from self")
                    .sender(client.user_id().expect("Client must have a user ID"))
                    .event_id(event_id_from_self),
            ),
        )
        .await;

    let event_from_self =
        assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);

    // Clear the timeline to make sure the old item does not need to be
    // available in it for the reply to work.
    timeline.clear().await;
    assert_next_matches!(timeline_stream, VectorDiff::Clear);

    // Now, let's reply to a message sent by the current user.
    server.mock_room_send()
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
        .mount()
        .await;

    let replied_to_info = event_from_self.replied_to_info().unwrap();
    timeline
        .send_reply(
            RoomMessageEventContentWithoutRelation::text_plain("Replying to self"),
            replied_to_info,
            EnforceThread::No(ForwardThread::Yes),
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
}

#[async_test]
async fn test_send_reply_to_threaded() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let thread_root = owned_event_id!("$thread_root");

    let event_id_1 = event_id!("$event1");
    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("Hello, World!")
                    .sender(&BOB)
                    .event_id(event_id_1)
                    .in_thread(&thread_root, &thread_root),
            ),
        )
        .await;

    let hello_world_item =
        assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);

    server.mock_room_send().ok(event_id!("$reply_event")).mock_once().mount().await;

    let replied_to_info = hello_world_item.replied_to_info().unwrap();
    timeline
        .send_reply(
            RoomMessageEventContentWithoutRelation::text_plain("Hello, Bob!"),
            replied_to_info,
            EnforceThread::No(ForwardThread::Yes),
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
}

#[async_test]
async fn test_send_reply_with_event_id() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let event_id_from_bob = event_id!("$event_from_bob");
    let f = EventFactory::new();
    let bob_event =
        f.text_msg("Hello from Bob").sender(&BOB).event_id(event_id_from_bob).into_event();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(bob_event.raw().clone()),
        )
        .await;

    let event_from_bob =
        assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);
    assert_eq!(event_from_bob.event_id().unwrap(), event_id_from_bob);

    // Clear the timeline to make sure the old item does not need to be
    // available in it for the reply to work.
    timeline.clear().await;
    assert_next_matches!(timeline_stream, VectorDiff::Clear);

    // Now, let's reply to a message sent by `BOB`.
    server.mock_room_send()
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
        .mount()
        .await;

    // Since we assume we can't use the timeline item directly in this use case, the
    // API will fetch the event from the server directly so we need to mock the
    // response.
    server.mock_room_event().ok(bob_event).named("event_1").mock_once().mount().await;

    let replied_to_info = timeline.replied_to_info_from_event_id(event_id_from_bob).await.unwrap();
    timeline
        .send_reply(
            RoomMessageEventContentWithoutRelation::text_plain("Replying to Bob"),
            replied_to_info,
            EnforceThread::No(ForwardThread::Yes),
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
}

#[async_test]
async fn test_send_reply_enforce_thread() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let event_id_from_bob = event_id!("$event_from_bob");
    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("Hello from Bob").sender(&BOB).event_id(event_id_from_bob),
            ),
        )
        .await;

    let event_from_bob =
        assert_next_matches!(timeline_stream, VectorDiff::PushBack { value } => value);

    // Clear the timeline to make sure the old item does not need to be
    // available in it for the reply to work.
    timeline.clear().await;
    assert_next_matches!(timeline_stream, VectorDiff::Clear);

    // Now, let's reply to a message sent by `BOB`.
    server.mock_room_send()
        .respond_with(move |req: &Request| {
            use ruma::events::room::message::RoomMessageEventContent;

            let reply_event = req
                .body_json::<RoomMessageEventContent>()
                .expect("Failed to deserialize the event");

            assert_matches!(reply_event.relates_to, Some(Relation::Thread(thread)) => {
                assert_eq!(thread.event_id, event_id_from_bob);
                assert_eq!(thread.in_reply_to.unwrap().event_id, event_id_from_bob);
            });
            assert_matches!(reply_event.mentions, None);

            ResponseTemplate::new(200).set_body_json(json!({ "event_id": "$reply_event" }))
        })
        .expect(1)
        .mount()
        .await;

    let replied_to_info = event_from_bob.replied_to_info().unwrap();
    timeline
        .send_reply(
            RoomMessageEventContentWithoutRelation::text_plain("Replying to Bob"),
            replied_to_info,
            EnforceThread::Yes(ReplyWithinThread::No),
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

    let diff = timeout(timeline_stream.next(), Duration::from_secs(1)).await.unwrap().unwrap();
    assert_let!(VectorDiff::Set { index: 0, value: reply_item_remote_echo } = diff);

    assert_matches!(reply_item_remote_echo.send_state(), Some(EventSendState::Sent { .. }));
    let reply_message = reply_item_remote_echo.content().as_message().unwrap();
    assert_eq!(reply_message.body(), "Replying to Bob");
    let in_reply_to = reply_message.in_reply_to().unwrap();
    assert_eq!(in_reply_to.event_id, event_id_from_bob);

}

#[async_test]
async fn test_send_reply_with_event_id_that_is_redacted() {
    // This test checks if is possible to reply to a redacted event that is not in
    // the timeline. The event id will go through a process where the event is
    // fetched and the content will be extracted and deserialised to be used in
    // the reply.
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) =
        timeline.subscribe_filter_map(|item| item.as_event().cloned()).await;

    let redacted_event_id_from_bob = event_id!("$event_from_bob");
    let f = EventFactory::new();
    let event_from_bob = f
        .redacted(&BOB, RedactedReactionEventContent::new())
        .event_id(redacted_event_id_from_bob)
        .into_event();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(event_from_bob.raw().clone()),
        )
        .await;

    // Clear the timeline to make sure the old item does not need to be
    // available in it for the reply to work.
    timeline.clear().await;
    assert_next_matches!(timeline_stream, VectorDiff::Clear);

    // Now, let's reply to a message sent by `BOB`.
    server.mock_room_send()
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
        .mount()
        .await;

    // Since we assume we can't use the timeline item directly in this use case, the
    // API will fetch the event from the server directly so we need to mock the
    // response.
    server.mock_room_event().ok(event_from_bob).mock_once().named("event_1").mount().await;

    let replied_to_info =
        timeline.replied_to_info_from_event_id(redacted_event_id_from_bob).await.unwrap();
    timeline
        .send_reply(
            RoomMessageEventContentWithoutRelation::text_plain("Replying to Bob"),
            replied_to_info,
            EnforceThread::No(ForwardThread::Yes),
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
}
