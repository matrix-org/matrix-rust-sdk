use anyhow::Result;
use assign::assign;
use matrix_sdk::ruma::{
    api::client::room::create_room::v3::Request as CreateRoomRequest,
    events::{
        room::avatar::{RoomAvatarEventContent, SyncRoomAvatarEvent},
        StateEventType,
    },
    mxc_uri,
};

use super::get_client_for_user;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_redaction() -> Result<()> {
    let tamatoa = get_client_for_user("tamatoa".to_owned()).await?;
    // create a room
    let request = assign!(CreateRoomRequest::new(), {
        is_direct: true,
    });

    // we need a background sync for `create_room`
    let bg_sync = tamatoa.clone();
    let bg_syncer = tokio::spawn(async move { bg_sync.sync(Default::default()).await });

    let room = tamatoa.create_room(request).await?;
    bg_syncer.abort();
    // let's send a specific state event

    let avatar_url = mxc_uri!("mxc://example.org/avatar").to_owned();
    let content = assign!(RoomAvatarEventContent::new(), {
        url: Some(avatar_url),
    });

    room.send_state_event(content, "").await?;
    // sync up.
    tamatoa.sync_once(Default::default()).await?;

    // check state event.

    let raw_event =
        room.get_state_event(StateEventType::RoomAvatar, "").await?.expect("Room Avatar not found");
    let room_avatar_event: SyncRoomAvatarEvent = raw_event.deserialize_as()?;
    assert!(
        room_avatar_event.as_original().expect("event exists").content.url.is_some(),
        "Event not found"
    );

    room.redact(room_avatar_event.event_id(), None, None).await?;
    // sync up.
    tamatoa.sync_once(Default::default()).await?;

    let raw_event =
        room.get_state_event(StateEventType::RoomAvatar, "").await?.expect("Room Avatar not found");
    let room_avatar_event: SyncRoomAvatarEvent = raw_event.deserialize_as()?;
    // Avatar content has been redacted
    assert!(room_avatar_event.as_original().is_none(), "Event still found");

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_redaction_static() -> Result<()> {
    let tamatoa = get_client_for_user("tamatoa".to_owned()).await?;
    // create a room
    let request = assign!(CreateRoomRequest::new(), {
        is_direct: true,
    });

    // we need a background sync for `create_room`
    let bg_sync = tamatoa.clone();
    let bg_syncer = tokio::spawn(async move { bg_sync.sync(Default::default()).await });

    let room = tamatoa.create_room(request).await?;
    bg_syncer.abort();

    // let's send a specific state event
    let avatar_url = mxc_uri!("mxc://example.org/avatar").to_owned();
    let content = assign!(RoomAvatarEventContent::new(), {
        url: Some(avatar_url),
    });

    room.send_state_event(content, "").await?;
    // sync up.
    tamatoa.sync_once(Default::default()).await?;

    // check state event.

    let room_avatar_event: SyncRoomAvatarEvent =
        room.get_state_event_static("").await?.expect("Room Avatar not found").deserialize()?;
    assert!(
        room_avatar_event.as_original().expect("event exists").content.url.is_some(),
        "Event not found"
    );

    room.redact(room_avatar_event.event_id(), None, None).await?;
    // we don't sync up.
    tamatoa.sync_once(Default::default()).await?;

    let room_avatar_event: SyncRoomAvatarEvent =
        room.get_state_event_static("").await?.expect("Room Avatar not found").deserialize()?;
    // Avatar content has been redacted
    assert!(room_avatar_event.as_original().is_none(), "Event still found");

    Ok(())
}
