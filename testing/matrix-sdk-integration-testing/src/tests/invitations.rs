use anyhow::{ensure, Result};
use assign::assign;
use matrix_sdk::{
    ruma::api::client::room::create_room::v3::Request as CreateRoomRequest, RoomState,
};

use crate::helpers::TestClientBuilder;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_invitation_details() -> Result<()> {
    let tamatoa = TestClientBuilder::new("tamatoa".to_owned()).use_sqlite().build().await?;
    let sebastian = TestClientBuilder::new("sebastian".to_owned()).use_sqlite().build().await?;

    let invite = vec![sebastian.user_id().expect("sebastian has a userid!").to_owned()];
    // create a room and invite sebastian;
    let request = assign!(CreateRoomRequest::new(), {
        invite,
        is_direct: true,
    });

    let room = tamatoa.create_room(request).await?;
    let room_id = room.room_id().to_owned();

    // the actual test
    sebastian.sync_once(Default::default()).await?;
    let room = sebastian.get_room(&room_id).expect("Sebstian doesn't know about the room");

    ensure!(
        room.state() == RoomState::Invited,
        "The room tamatoa invited sebastian in isn't an invite: {room:?}"
    );

    let details = room.invite_details().await?;
    let sender = details.inviter.expect("invite details doesn't have inviter");
    assert_eq!(sender.user_id(), tamatoa.user_id().expect("tamatoa has a user_id"));

    Ok(())
}
