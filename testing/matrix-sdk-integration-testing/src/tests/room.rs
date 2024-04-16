use std::time::Duration;

use anyhow::Result;
use assert_matches2::{assert_let, assert_matches};
use matrix_sdk::{
    room::MessagesOptions,
    ruma::{
        api::client::room::create_room::v3::Request as CreateRoomRequest,
        assign, event_id,
        events::{room::message::RoomMessageEventContent, AnyStateEvent, AnyTimelineEvent},
        uint,
    },
    test_utils::assert_event_matches_msg,
    RoomState,
};
use tokio::{spawn, time::sleep};
use tracing::error;

use crate::helpers::TestClientBuilder;

#[tokio::test]
async fn test_event_with_context() -> Result<()> {
    let bob =
        TestClientBuilder::new("bob".to_owned()).randomize_username().use_sqlite().build().await?;

    // Spawn sync for bob.
    let b = bob.clone();
    spawn(async move {
        let bob = b;
        loop {
            if let Err(err) = bob.sync(Default::default()).await {
                error!("bob sync error: {err}");
            }
        }
    });

    let alice = TestClientBuilder::new("alice".to_owned())
        .randomize_username()
        .use_sqlite()
        .build()
        .await?;

    // Spawn sync for alice too.
    let a = alice.clone();
    spawn(async move {
        let alice = a;
        loop {
            if let Err(err) = alice.sync(Default::default()).await {
                error!("alice sync error: {err}");
            }
        }
    });

    // alice creates a room and invites bob.
    let room_id = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            invite: vec![bob.user_id().unwrap().to_owned()],
            is_direct: true,
        }))
        .await?
        .room_id()
        .to_owned();

    let mut alice_room = None;
    for i in 1..=4 {
        alice_room = alice.get_room(&room_id);
        if alice_room.is_some() {
            break;
        }
        sleep(Duration::from_millis(30 * i)).await;
    }

    // Bob joins it.
    let mut bob_joined = false;
    for i in 1..=4 {
        if let Some(room) = bob.get_room(&room_id) {
            room.join().await?;
            bob_joined = true;
            break;
        }
        sleep(Duration::from_millis(30 * i)).await;
    }
    anyhow::ensure!(bob_joined, "bob couldn't join after 3 seconds");

    let alice_room = alice_room.unwrap();
    assert_eq!(alice_room.state(), RoomState::Joined);

    alice_room.enable_encryption().await?;

    for i in 0..10 {
        alice_room.send(RoomMessageEventContent::text_plain(i.to_string())).await?;
    }

    let send_event_response =
        alice_room.send(RoomMessageEventContent::text_plain("hello there!")).await?;
    let event_id = send_event_response.event_id;

    for i in 0..10 {
        alice_room.send(RoomMessageEventContent::text_plain((i + 10).to_string())).await?;
    }

    let room = bob.get_room(alice_room.room_id()).expect("bob has joined the room");

    {
        // First /context query: only the target event, no context around it.
        let response = room.event_with_context(&event_id, false, uint!(0)).await?;

        let target = response
            .event
            .expect("there should be an event")
            .event
            .deserialize()
            .expect("it should be deserializable");
        assert_eq!(target.event_id(), &event_id);

        assert!(response.events_after.is_empty());
        assert!(response.events_before.is_empty());

        assert!(response.next_batch_token.is_some());
        assert!(response.prev_batch_token.is_some());
    }

    {
        // Next query: an event that doesn't exist (hopefully!).
        let response = room.event_with_context(event_id!("$lolololol"), false, uint!(0)).await;

        // Servers answers with 404.
        assert_let!(Err(err) = response);
        assert_eq!(err.as_client_api_error().unwrap().status_code.as_u16(), 404);
    }

    {
        // Next query: target event with a context of 3 events. There
        // should be some previous and next tokens.
        let response = room.event_with_context(&event_id, false, uint!(3)).await?;

        let target = response
            .event
            .expect("there should be an event")
            .event
            .deserialize()
            .expect("it should be deserializable");
        assert_eq!(target.event_id(), &event_id);

        let after = response.events_after;
        assert_eq!(after.len(), 2);
        assert_event_matches_msg(&after[0], "10");
        assert_event_matches_msg(&after[1], "11");

        let before = response.events_before;
        assert_eq!(before.len(), 1);
        assert_event_matches_msg(&before[0], "9");

        // Paginate forwards.
        let next_batch = response.next_batch_token.unwrap();

        let next_messages =
            room.messages(MessagesOptions::forward().from(Some(next_batch.as_str()))).await?;

        let next_events = next_messages.chunk;
        assert_eq!(next_events.len(), 8);
        assert_event_matches_msg(&next_events[0], "12");
        assert_event_matches_msg(&next_events[7], "19");

        {
            // Synapse is pranking us here, pretending there might be other events
            // afterwards.
            let next_messages = room
                .messages(
                    MessagesOptions::forward().from(Some(next_messages.end.unwrap().as_str())),
                )
                .await?;

            assert!(next_messages.chunk.is_empty());
            assert!(next_messages.end.is_none());
        }

        // Paginate backwards.
        let prev_batch = response.prev_batch_token.unwrap();

        let prev_messages =
            room.messages(MessagesOptions::backward().from(Some(prev_batch.as_str()))).await?;

        let prev_events = prev_messages.chunk;
        assert_eq!(prev_events.len(), 10);
        assert_event_matches_msg(&prev_events[0], "8");
        assert_event_matches_msg(&prev_events[8], "0");

        // Last event is the m.room.encryption event.
        let event = prev_events[9].event.deserialize().unwrap();
        assert_matches!(event, AnyTimelineEvent::State(AnyStateEvent::RoomEncryption(_)));

        // There are other events before that (room creation, alice joining).
        assert!(prev_messages.end.is_some());
    }

    Ok(())
}
