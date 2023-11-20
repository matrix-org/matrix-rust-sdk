use std::time::Duration;

use anyhow::Result;
use futures_util::{pin_mut, StreamExt};
use matrix_sdk::{
    config::SyncSettings,
    ruma::{
        api::client::room::create_room::v3::Request as CreateRoomRequest, assign,
        events::room::message::RoomMessageEventContent, mxc_uri,
    },
    RoomState, SlidingSyncList, SlidingSyncMode,
};
use tokio::time::sleep;
use tracing::{error, warn};

use crate::helpers::TestClientBuilder;

#[tokio::test]
async fn test_room_avatar_group_conversation() -> Result<()> {
    let alice = TestClientBuilder::new("alice".to_owned())
        .randomize_username()
        .use_sqlite()
        .build()
        .await?;
    let bob =
        TestClientBuilder::new("bob".to_owned()).randomize_username().use_sqlite().build().await?;
    let celine = TestClientBuilder::new("celine".to_owned())
        .randomize_username()
        .use_sqlite()
        .build()
        .await?;

    // Bob and Celine set their avatars.
    bob.account().set_avatar_url(Some(mxc_uri!("mxc://localhost/bob"))).await?;
    celine.account().set_avatar_url(Some(mxc_uri!("mxc://localhost/celine"))).await?;

    // Set up sliding sync for alice.
    let sliding_alice = alice
        .sliding_sync("main")?
        .with_all_extensions()
        .poll_timeout(Duration::from_secs(2))
        .network_timeout(Duration::from_secs(2))
        .add_list(
            SlidingSyncList::builder("all")
                .sync_mode(SlidingSyncMode::new_selective().add_range(0..=20)),
        )
        .build()
        .await?;

    let s = sliding_alice.clone();
    tokio::task::spawn(async move {
        let stream = s.sync();
        pin_mut!(stream);
        while let Some(up) = stream.next().await {
            warn!("received update: {up:?}");
        }
    });

    // alice creates a room and invites bob and celine.
    let alice_room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            invite: vec![bob.user_id().unwrap().to_owned(), celine.user_id().unwrap().to_owned()],
            is_direct: true,
        }))
        .await?;

    sleep(Duration::from_secs(1)).await;

    let alice_room = alice.get_room(alice_room.room_id()).unwrap();
    assert_eq!(alice_room.state(), RoomState::Joined);

    let sliding_room = sliding_alice
        .get_room(alice_room.room_id())
        .await
        .expect("sliding sync finds alice's own room");

    // Here, there should be no avatar (group conversation and no avatar has been
    // set in the room).
    for _ in 0..3 {
        sleep(Duration::from_secs(1)).await;
        assert_eq!(alice_room.avatar_url(), None);
        assert_eq!(sliding_room.avatar_url(), None);

        // Force a new server response.
        alice_room.send(RoomMessageEventContent::text_plain("hello world")).await?;
    }

    // Alice sets an avatar for the room.
    let group_avatar_uri = mxc_uri!("mxc://localhost/group");
    alice_room.set_avatar_url(group_avatar_uri, None).await?;

    for _ in 0..3 {
        sleep(Duration::from_secs(1)).await;
        assert_eq!(alice_room.avatar_url().as_deref(), Some(group_avatar_uri));
        assert_eq!(sliding_room.avatar_url().as_deref(), Some(group_avatar_uri));

        // Force a new server response.
        alice_room.send(RoomMessageEventContent::text_plain("hello world")).await?;
    }

    // And eventually Alice unsets it.
    alice_room.remove_avatar().await?;

    for _ in 0..3 {
        sleep(Duration::from_secs(1)).await;
        assert_eq!(alice_room.avatar_url(), None);
        assert_eq!(sliding_room.avatar_url(), None);

        // Force a new server response.
        alice_room.send(RoomMessageEventContent::text_plain("hello world")).await?;
    }

    Ok(())
}
