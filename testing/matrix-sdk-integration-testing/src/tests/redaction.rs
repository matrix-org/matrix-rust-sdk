use anyhow::Result;
use assign::assign;
use matrix_sdk::{
    config::SyncSettings,
    ruma::{
        api::client::room::create_room::v3::Request as CreateRoomRequest,
        events::{room::name::RoomNameEventContent, StateEventType},
    },
    Client,
};

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
        if tamatoa.get_room(&room_id).is_some() {
            break;
        }
    }

    let room = tamatoa.get_room(&room_id).unwrap();
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
        if tamatoa.get_room(&room_id).is_some() {
            break;
        }
    }

    let room = tamatoa.get_room(&room_id).unwrap();

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
