use assert_matches::assert_matches;
use std::sync::Arc;

use anyhow::{Ok, Result};
use assign::assign;
use eyeball_im::VectorDiff;
use futures_core::Stream;
use futures_util::{future::join_all, StreamExt};
use matrix_sdk::{
    config::SyncSettings,
    ruma::{
        api::client::room::create_room::v3::Request as CreateRoomRequest,
        events::{
            relation::Annotation,
            room::{message::RoomMessageEventContent, name::RoomNameEventContent},
            StateEventType,
        },
        EventId, OwnedEventId, RoomId,
    },
    Client, LoopCtrl,
};
use matrix_sdk_ui::timeline::{EventTimelineItem, RoomExt, TimelineItem};

use crate::helpers::get_client_for_user;

async fn sync_once(client: &Client, sync_token: Option<String>) -> Result<String> {
    let settings = match sync_token {
        Some(token) => SyncSettings::default().token(token),
        None => SyncSettings::default(),
    };
    let sync_token = client.sync_once(settings).await?.next_batch;
    Ok(sync_token)
}

#[ignore = "Broken since synapse update, see #1069"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_redacting_name() -> Result<()> {
    let tamatoa = get_client_for_user("tamatoa".to_owned(), true).await?;
    // create a room
    let request = assign!(CreateRoomRequest::new(), {
        is_direct: true,
    });

    let mut sync_token = None;
    let room = tamatoa.create_room(request).await?;
    let room_id = room.room_id().to_owned();
    for _ in 0..=10 {
        sync_token = Some(sync_once(&tamatoa, sync_token).await?);
        if tamatoa.get_joined_room(&room_id).is_some() {
            break;
        }
    }

    let room = tamatoa.get_joined_room(&room_id).unwrap();
    // let's send a specific state event

    let content = RoomNameEventContent::new(Some("Inappropriate text".to_owned()));

    room.send_state_event(content).await?;
    // sync up.
    for _ in 0..=10 {
        // we call sync up to ten times to give the server time to flush other
        // messages over and send us the new state event
        sync_token = Some(sync_once(&tamatoa, sync_token).await?);

        if room.name().is_some() {
            break;
        }
    }

    assert_eq!(room.name(), Some("Inappropriate text".to_owned()));
    // check state event.

    let raw_event =
        room.get_state_event(StateEventType::RoomName, "").await?.expect("Room Name not found");
    let room_name_event = raw_event.cast::<RoomNameEventContent>().deserialize()?;
    let sync_room_name_event = room_name_event.as_sync().expect("event is sync event");
    assert!(
        sync_room_name_event.as_original().expect("event exists").content.name.is_some(),
        "Event not found"
    );

    room.redact(sync_room_name_event.event_id(), None, None).await?;
    // sync up.
    for _ in 0..=10 {
        // we call sync up to ten times to give the server time to flush other
        // messages over and send us the new state ev
        sync_token = Some(sync_once(&tamatoa, sync_token).await?);

        if room.name().is_none() {
            break;
        }
    }

    let raw_event =
        room.get_state_event(StateEventType::RoomName, "").await?.expect("Room Name not found");
    let room_name_event = raw_event.cast::<RoomNameEventContent>().deserialize()?;
    // Name content has been redacted
    assert!(
        room_name_event
            .as_sync()
            .expect("event is sync event")
            .as_original()
            .expect("event exists")
            .content
            .name
            .is_none(),
        "Event hasn't been redacted"
    );

    Ok(())
}

#[ignore = "Broken since synapse update, see #1069"]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_redacting_name_static() -> Result<()> {
    let tamatoa = get_client_for_user("tamatoa".to_owned(), true).await?;
    // create a room
    let request = assign!(CreateRoomRequest::new(), {
        is_direct: true,
    });

    let mut sync_token = None;
    let room = tamatoa.create_room(request).await?;
    let room_id = room.room_id().to_owned();
    for _ in 0..=10 {
        sync_token = Some(sync_once(&tamatoa, sync_token).await?);
        if tamatoa.get_joined_room(&room_id).is_some() {
            break;
        }
    }

    let room = tamatoa.get_joined_room(&room_id).unwrap();

    // let's send a specific state event
    let content = RoomNameEventContent::new(Some("Inappropriate text".to_owned()));

    room.send_state_event(content).await?;
    // sync up.
    for _ in 0..=10 {
        // we call sync up to ten times to give the server time to flush other
        // messages over and send us the new state event
        sync_token = Some(sync_once(&tamatoa, sync_token).await?);

        if room.name().is_some() {
            break;
        }
    }

    // check state event.

    let room_name_event = room
        .get_state_event_static::<RoomNameEventContent>()
        .await?
        .expect("Room Name not found")
        .deserialize()?;
    let sync_room_name_event = room_name_event.as_sync().expect("event is sync event");
    assert!(
        sync_room_name_event.as_original().expect("event exists").content.name.is_some(),
        "Event not found"
    );

    room.redact(sync_room_name_event.event_id(), None, None).await?;
    // we sync up.
    for _ in 0..=10 {
        // we call sync up to ten times to give the server time to flush other
        // messages over and send us the new state ev
        sync_token = Some(sync_once(&tamatoa, sync_token).await?);

        if room.name().is_none() {
            break;
        }
    }

    let room_name_event = room
        .get_state_event_static::<RoomNameEventContent>()
        .await?
        .expect("Room Name not found")
        .deserialize()?;
    // Name content has been redacted
    assert!(
        room_name_event
            .as_sync()
            .expect("event is sync event")
            .as_original()
            .expect("event exists")
            .content
            .name
            .is_none(),
        "Event hasn't been redacted"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_redacting_reaction() -> Result<()> {
    // Set up
    let alice = get_client_for_user("alice".to_owned(), true).await?;
    let user_id = alice.user_id().unwrap();
    let room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            is_direct: true,
        }))
        .await?;
    let room_id = room.room_id();
    let timeline = room.timeline().await;
    let reaction_key = "👍";
    let (_items, mut stream) = timeline.subscribe().await;

    // Send message
    timeline.send(RoomMessageEventContent::text_plain("hi!").into(), None).await;
    {
        let _day_divider = stream.next().await;
        let event =
            assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
        let event = event.as_event().unwrap();
        assert_matches!(event.content().as_message(), Some(_));
        assert!(event.is_local_echo());
        assert!(event.reactions().is_empty());
    }

    // Receive updated local echo with event ID
    let event_id = {
        let event = assert_matches!(stream.next().await, Some(VectorDiff::Set { index: 1, value }) => value);
        let event = event.as_event().unwrap();
        event.event_id().unwrap().to_owned()
    };

    // Sync remaining remote events
    sync_room(&alice, room_id).await?;
    {
        let _room_create = stream.next().await;
        let _membership_change = stream.next().await;
        let _power_levels = stream.next().await;
        let _join_rules = stream.next().await;
        let _history_visibility = stream.next().await;
        let _guest_access = stream.next().await;
        let _remove_local_event = stream.next().await;
    }

    // Check we have the remote echo
    {
        let event =
            assert_matches!(stream.next().await, Some(VectorDiff::PushBack { value }) => value);
        let event = event.as_event().unwrap();
        assert_eq!(event.event_id().unwrap(), &event_id);
        assert!(!event.is_local_echo());
    };

    // Toggle reaction multiple times
    for _ in 0..3 {
        // Send a reaction
        let reaction = Annotation::new(event_id.clone(), reaction_key.into());

        // Add the reaction many times
        join_all((0..100).map(|_| timeline.toggle_reaction(&reaction)).collect::<Vec<_>>()).await;

        let message_position = timeline.items().await.len() - 1;

        // Check we have a local echo of the reaction
        {
            let event = assert_event_is_updated(&mut stream, &event_id, message_position).await;
            let (reaction_tx_id, reaction_event_id) = {
                let reactions = event.reactions().get(reaction_key).unwrap();
                let reaction = reactions.by_sender(user_id.to_owned()).next().unwrap();
                reaction.to_owned()
            };
            assert_matches!(reaction_tx_id, Some(_));
            assert_matches!(reaction_event_id, None); // Event ID hasn't been received from homeserver yet
        }

        // Check we have the remote echo of the reaction
        let _reaction_event_id: OwnedEventId = {
            let event = assert_event_is_updated(&mut stream, &event_id, message_position).await;
            let reactions = event.reactions().get(reaction_key).unwrap();
            assert_eq!(reactions.senders().count(), 1);
            let reaction = reactions.by_sender(user_id.to_owned()).next().unwrap();
            let (reaction_tx_id, reaction_event_id) = reaction;
            assert_matches!(reaction_tx_id, None);
            let reaction_event_id = assert_matches!(reaction_event_id, Some(value) => value);
            reaction_event_id.to_owned()
        };

        // Redact the reaction many times
        join_all((0..100).map(|_| timeline.toggle_reaction(&reaction)).collect::<Vec<_>>()).await;

        // Check the message has no local reaction
        {
            let event = assert_event_is_updated(&mut stream, &event_id, message_position).await;
            assert_matches!(event.reactions().get(reaction_key), None);
        }
    }

    Ok(())
}

async fn sync_room(client: &Client, room_id: &RoomId) -> Result<()> {
    client
        .sync_with_callback(SyncSettings::default(), |response| async move {
            if response.rooms.join.iter().any(|(id, _)| id == room_id) {
                LoopCtrl::Break
            } else {
                LoopCtrl::Continue
            }
        })
        .await?;
    Ok(())
}

async fn assert_event_is_updated(
    stream: &mut (impl Stream<Item = VectorDiff<Arc<TimelineItem>>> + Unpin),
    event_id: &EventId,
    _position: usize,
) -> EventTimelineItem {
    let event = assert_matches!(
        stream.next().await,
        Some(VectorDiff::Set { index: _position, value }) => value
    );
    let event = event.as_event().unwrap();
    assert_eq!(event.event_id().unwrap(), event_id);
    event.to_owned()
}
