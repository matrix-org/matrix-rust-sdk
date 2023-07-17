use std::{sync::Arc, time::Duration};

use anyhow::Result;
use assign::assign;
use matrix_sdk::{
    event_handler::Ctx,
    room::Room,
    ruma::{
        api::client::room::create_room::v3::Request as CreateRoomRequest,
        events::room::member::{MembershipState, StrippedRoomMemberEvent},
    },
    Client, RoomMemberships, RoomState, StateStoreExt,
};
use tokio::sync::Notify;

use crate::helpers::get_client_for_user;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_repeated_join_leave() -> Result<()> {
    let peter = get_client_for_user("peter".to_owned(), true).await?;
    // FIXME: Run once with memory, once with SQLite
    let karl = get_client_for_user("karl".to_owned(), false).await?;
    let karl_id = karl.user_id().expect("karl has a userid!").to_owned();

    // Create a room and invite karl.
    let invite = vec![karl_id.clone()];
    let request = assign!(CreateRoomRequest::new(), {
        invite,
        is_direct: true,
    });

    // Sync after 1 second to so that create_room receives the event it is waiting
    // for.
    let peter_clone = peter.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(1)).await;
        peter_clone.sync_once(Default::default()).await
    });

    let created_room = peter.create_room(request).await?;
    let room_id = created_room.room_id();

    // Sync karl once to ensure he got the invite.
    karl.sync_once(Default::default()).await?;

    // Continuously sync karl from now on.
    let karl_clone = karl.clone();
    let join_handle = tokio::spawn(async move {
        karl_clone.sync(Default::default()).await.unwrap();
    });
    let invite_signal = Arc::new(Notify::new());
    karl.add_event_handler_context(invite_signal.clone());
    karl.add_event_handler(signal_on_invite);

    for i in 0..3 {
        println!("Iteration {i}");

        let room = karl.get_room(room_id).expect("karl has the room");

        // Test that karl has the expected state in its client.
        assert_eq!(room.state(), RoomState::Invited);

        let membership = room.get_member_no_sync(&karl_id).await?.expect("karl was invited");
        assert_eq!(*membership.membership(), MembershipState::Invite);

        // Join the room
        println!("Joining..");
        let room = room.join().await?;
        println!("Done");
        let membership = room.get_member(&karl_id).await?.expect("karl joined");
        assert_eq!(*membership.membership(), MembershipState::Join);
        assert_eq!(room.state(), RoomState::Joined);

        // Syncs can overwrite the internal state. If the sync lags behind because we
        // change so often so fast, we can get errors in the asserts here. So we have to
        // wait here a bit till the sync happened.
        room.sync_up().await;

        // Leave the room
        println!("Leaving..");
        room.leave().await?;
        println!("Done");
        let membership = room.get_member(&karl_id).await?.expect("karl left");
        assert_eq!(*membership.membership(), MembershipState::Leave);

        assert_eq!(room.state(), RoomState::Left);

        // Invite karl again and wait for karl to receive the invite.
        println!("Inviting..");
        let room = peter.get_joined_room(room_id).expect("peter created the room!");
        room.invite_user_by_id(&karl_id).await?;
        println!("Waiting to receive invite..");
        invite_signal.notified().await;
    }

    // Stop the sync.
    join_handle.abort();

    // Now check the underlying state store that it also has the correct information
    // (for when the client restarts).
    let invited = karl.store().get_user_ids(room_id, RoomMemberships::INVITE).await?;
    assert_eq!(invited.len(), 1);
    assert_eq!(invited[0], karl_id);

    let joined = karl.store().get_user_ids(room_id, RoomMemberships::JOIN).await?;
    assert!(!joined.contains(&karl_id));

    let event = karl
        .store()
        .get_member_event(room_id, &karl_id)
        .await?
        .expect("member event should exist")
        .deserialize()
        .unwrap();
    assert_eq!(*event.membership(), MembershipState::Invite);

    // Yay, test succeeded
    Ok(())
}

async fn signal_on_invite(
    event: StrippedRoomMemberEvent,
    room: Room,
    client: Client,
    sender: Ctx<Arc<Notify>>,
) {
    let own_id = client.user_id().expect("client is logged in");
    if event.sender == own_id {
        return;
    }

    if room.state() != RoomState::Invited {
        return;
    }

    if event.content.membership != MembershipState::Invite {
        return;
    }

    let invited = &event.state_key;
    if invited != own_id {
        return;
    }

    // Send signal that we received an invite.
    sender.notify_one();
}
