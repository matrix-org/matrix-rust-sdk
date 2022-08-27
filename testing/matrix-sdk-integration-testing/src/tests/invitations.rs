use std::time::Duration;

use anyhow::{bail, Result};
use assign::assign;
use matrix_sdk::{
    room::Room, ruma::api::client::room::create_room::v3::Request as CreateRoomRequest,
};

use super::{get_client_for_user, Store};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_invitation_details() -> Result<()> {
    let tamatoa = get_client_for_user(Store::Sled, "tamatoa".to_owned()).await?;
    let sebastian = get_client_for_user(Store::Sled, "sebastian".to_owned()).await?;

    let invites = [sebastian.user_id().expect("sebastian has a userid!").to_owned()];
    // create a room and invite sebastian;
    let request = assign!(CreateRoomRequest::new(), {
        invite: &invites,
        is_direct: true,
    });

    let tamatoa_clone = tamatoa.clone();

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        tamatoa_clone.sync_once(Default::default()).await
    });

    let created_room = tamatoa.create_room(request).await?;

    // the actual test
    sebastian.sync_once(Default::default()).await?;
    let room =
        sebastian.get_room(created_room.room_id()).expect("Sebstian doesn't know about the room");

    if let Room::Invited(iv) = room {
        let details = iv.invite_details().await?;
        let sender = details.inviter.expect("invite details doesn't have inviter");
        assert_eq!(sender.user_id(), tamatoa.user_id().expect("tamatoa has a user_id"));
    } else {
        bail!("The room tamatoa invited sebastian in isn't an invite: {:?}", room);
    }
    Ok(())
}
