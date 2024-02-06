use std::{sync::Arc, time::Duration};

use anyhow::Result;
use futures_util::{pin_mut, StreamExt as _};
use matrix_sdk::{
    config::SyncSettings,
    ruma::{
        api::client::{
            receipt::create_receipt::v3::ReceiptType,
            room::create_room::v3::Request as CreateRoomRequest,
            sync::sync_events::v4::{
                AccountDataConfig, E2EEConfig, ReceiptsConfig, ToDeviceConfig,
            },
        },
        assign,
        events::{
            receipt::ReceiptThread, room::message::RoomMessageEventContent,
            AnySyncMessageLikeEvent, Mentions, StateEventType,
        },
        mxc_uri,
    },
    Client, RoomListEntry, RoomMemberships, RoomState, SlidingSyncList, SlidingSyncMode,
};
use matrix_sdk_ui::RoomListService;
use stream_assert::assert_pending;
use tokio::{spawn, sync::Mutex, time::sleep};
use tracing::{error, warn};

use crate::helpers::TestClientBuilder;

#[tokio::test]
async fn test_left_room() -> Result<()> {
    let peter = TestClientBuilder::new("peter".to_owned())
        .randomize_username()
        .use_sqlite()
        .build()
        .await?;
    let steven = TestClientBuilder::new("steven".to_owned())
        .randomize_username()
        .use_sqlite()
        .build()
        .await?;

    // Set up sliding sync for Peter.
    let sliding_peter = peter
        .sliding_sync("main")?
        .with_all_extensions()
        .poll_timeout(Duration::from_secs(3))
        .network_timeout(Duration::from_secs(3))
        .add_list(
            SlidingSyncList::builder("all")
                .sync_mode(SlidingSyncMode::new_selective().add_range(0..=20)),
        )
        .build()
        .await?;

    let s = sliding_peter.clone();
    spawn(async move {
        let stream = s.sync();
        pin_mut!(stream);
        while let Some(up) = stream.next().await {
            warn!("received update: {up:?}");
        }
    });

    // Set up regular sync for Steven.
    let steven2 = steven.clone();
    spawn(async move {
        if let Err(err) = steven2.sync(SyncSettings::default()).await {
            error!("steven couldn't sync: {err}");
        }
    });

    // Peter creates a room and invites Steven.
    let peter_room = peter
        .create_room(assign!(CreateRoomRequest::new(), {
            invite: vec![steven.user_id().unwrap().to_owned()],
            is_direct: true,
        }))
        .await?;

    // Steven joins it.
    let mut joined = false;
    for _ in 0..3 {
        sleep(Duration::from_secs(1)).await;
        if let Some(room) = steven.get_room(peter_room.room_id()) {
            room.join().await?;
            joined = true;
            break;
        }
    }
    anyhow::ensure!(joined, "steven couldn't join after 3 seconds");

    // Now Peter is just being rude.
    peter_room.leave().await?;

    sleep(Duration::from_secs(1)).await;
    let peter_room = peter.get_room(peter_room.room_id()).unwrap();
    assert_eq!(peter_room.state(), RoomState::Left);

    let list = sliding_peter
        .on_list("all", |l| {
            let list = l.clone();
            async { list }
        })
        .await
        .expect("must found room list");

    // Even though we left the room, the server still includes the room in the list,
    // so the SDK doesn't receive a DELETE sync op for this room entry.
    // See also https://github.com/vector-im/element-x-ios/issues/2005.
    assert_eq!(list.room_list::<RoomListEntry>().len(), 1);

    Ok(())
}

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
    spawn(async move {
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

    // Here, there should be no avatar (group conversation and no avatar has been
    // set in the room).
    for _ in 0..3 {
        sleep(Duration::from_secs(1)).await;
        assert_eq!(alice_room.avatar_url(), None);

        // Force a new server response.
        alice_room.send(RoomMessageEventContent::text_plain("hello world")).await?;
    }

    // Alice sets an avatar for the room.
    let group_avatar_uri = mxc_uri!("mxc://localhost/group");
    alice_room.set_avatar_url(group_avatar_uri, None).await?;

    for _ in 0..3 {
        sleep(Duration::from_secs(1)).await;
        assert_eq!(alice_room.avatar_url().as_deref(), Some(group_avatar_uri));

        // Force a new server response.
        alice_room.send(RoomMessageEventContent::text_plain("hello world")).await?;
    }

    // And eventually Alice unsets it.
    alice_room.remove_avatar().await?;

    for _ in 0..3 {
        sleep(Duration::from_secs(1)).await;
        assert_eq!(alice_room.avatar_url(), None);

        // Force a new server response.
        alice_room.send(RoomMessageEventContent::text_plain("hello world")).await?;
    }

    Ok(())
}

#[tokio::test]
async fn test_joined_user_can_create_push_context_with_room_list_service() -> Result<()> {
    // This regression test for #3031 checks that we properly get the logged-in room
    // member information, when connecting on the first time using the room
    // list service.
    //
    // Now the conditions to trigger this bug were quite precise:
    // - obviously we shouldn't have had any previous info about the logged-in
    //   user's room membership (which can be queried and cached for different
    //   reasons, at different times),
    // - we need to get information about rooms through the room list service,
    // - and since we enabled room members lazy-loading already, we shouldn't
    //   receive any events sent by the logged-in user.
    //
    // One way to trigger this is to have a room we've joined, but in which we
    // haven't sent the last event.
    //
    // Set this up, by creating a room, inviting another user, have the other user
    // send a message, and fake a new "device" by creating another client for
    // the same user.

    let bob =
        TestClientBuilder::new("bob".to_owned()).randomize_username().use_sqlite().build().await?;

    let alice = TestClientBuilder::new("alice".to_owned())
        .randomize_username()
        .use_sqlite()
        .build()
        .await?;

    // Set up regular sync for Alice to start with.
    let a = alice.clone();
    let alice_handle = spawn(async move {
        if let Err(err) = a.sync(Default::default()).await {
            warn!("alice sync errored: {err}");
        }
    });

    // Set up standard sync for Bob.
    let b = bob.clone();
    let bob_handle = spawn(async move {
        if let Err(err) = b.sync(Default::default()).await {
            warn!("bob sync errored: {err}");
        }
    });

    // Alice creates a room and invites Bob.
    let alice_room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            invite: vec![bob.user_id().unwrap().to_owned()],
            is_direct: false,
        }))
        .await?;

    sleep(Duration::from_secs(1)).await;

    // Bob joins the room.
    bob.join_room_by_id(alice_room.room_id()).await?;

    let alice_room = alice.get_room(alice_room.room_id()).unwrap();
    assert_eq!(alice_room.state(), RoomState::Joined);

    // Stop previous sync for Alice.
    alice_handle.abort();

    // Test setup is done, start with the actual testing.

    // Given a last event sent by Bob to the room,
    bob.get_room(alice_room.room_id())
        .unwrap()
        .send(RoomMessageEventContent::text_plain("hello world"))
        .await?;

    // And a new device for Alice that uses sliding sync,
    let hs = alice.homeserver();
    let sliding_sync_url = alice.sliding_sync_proxy();
    let alice_id = alice.user_id().unwrap().localpart().to_owned();

    let alice = Client::new(hs).await?;
    alice.set_sliding_sync_proxy(sliding_sync_url);
    alice.matrix_auth().login_username(&alice_id, &alice_id).await?;

    let room_list_service = Arc::new(RoomListService::new(alice.clone()).await?);

    // After starting sliding sync,
    let rls = room_list_service.clone();
    let alice_handle = spawn(async move {
        let sync = rls.sync();
        pin_mut!(sync);
        while let Some(update) = sync.next().await {
            warn!("Update from the room list service: {update:?}");
        }
    });

    sleep(Duration::from_secs(3)).await;

    // Alice room membership for that room will be known.
    let members = alice
        .get_room(alice_room.room_id())
        .unwrap()
        .members_no_sync(RoomMemberships::ACTIVE)
        .await?;
    let alice_user_id = alice.user_id().unwrap();
    assert!(members.iter().any(|member| member.user_id() == alice_user_id));

    room_list_service.stop_sync()?;
    alice_handle.abort();
    bob_handle.abort();

    Ok(())
}

#[tokio::test]
async fn test_room_notification_count() -> Result<()> {
    use tokio::time::timeout;

    let bob =
        TestClientBuilder::new("bob".to_owned()).randomize_username().use_sqlite().build().await?;

    // Spawn sync for bob.
    let b = bob.clone();
    spawn(async move {
        let bob = b;
        loop {
            if let Err(err) = bob.sync(Default::default()).await {
                tracing::error!("bob sync error: {err}");
            }
        }
    });

    // Set up sliding sync for alice.
    let alice = TestClientBuilder::new("alice".to_owned())
        .randomize_username()
        .use_sqlite()
        .build()
        .await?;

    spawn({
        let sync = alice
            .sliding_sync("main")?
            .with_receipt_extension(assign!(ReceiptsConfig::default(), { enabled: Some(true) }))
            .with_account_data_extension(
                assign!(AccountDataConfig::default(), { enabled: Some(true) }),
            )
            .with_e2ee_extension(assign!(E2EEConfig::default(), { enabled: Some(true) }))
            .with_to_device_extension(assign!(ToDeviceConfig::default(), { enabled: Some(true) }))
            .add_list(
                SlidingSyncList::builder("all")
                    .sync_mode(SlidingSyncMode::new_selective().add_range(0..=20))
                    .required_state(vec![(StateEventType::RoomMember, "$ME".to_owned())]),
            )
            .build()
            .await?;

        async move {
            let stream = sync.sync();
            pin_mut!(stream);
            while let Some(up) = stream.next().await {
                warn!("received update: {up:?}");
            }
        }
    });

    let latest_event = Arc::new(Mutex::new(None));
    let l = latest_event.clone();
    alice.add_event_handler(|ev: AnySyncMessageLikeEvent| async move {
        let mut latest_event = l.lock().await;
        *latest_event = Some(ev);
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
        sleep(Duration::from_millis(30 * i)).await;
        alice_room = alice.get_room(&room_id);
        if alice_room.is_some() {
            break;
        }
    }

    let alice_room = alice_room.unwrap();
    assert_eq!(alice_room.state(), RoomState::Joined);

    alice_room.enable_encryption().await?;

    let mut info_updates = alice_room.subscribe_info();

    // At first, nothing has happened, so we shouldn't have any notifications.
    assert_eq!(alice_room.num_unread_messages(), 0);
    assert_eq!(alice_room.num_unread_mentions(), 0);
    assert_eq!(alice_room.num_unread_notifications(), 0);

    assert_pending!(info_updates);

    // Bob joins, nothing happens.
    bob.join_room_by_id(&room_id).await?;

    assert!(info_updates.next().await.is_some());

    assert_eq!(alice_room.num_unread_messages(), 0);
    assert_eq!(alice_room.num_unread_mentions(), 0);
    assert_eq!(alice_room.num_unread_notifications(), 0);
    assert!(alice_room.latest_event().is_none());

    assert_pending!(info_updates);

    // Bob sends a non-mention message.
    let bob_room = bob.get_room(&room_id).expect("bob knows about alice's room");

    bob_room.send(RoomMessageEventContent::text_plain("hello world")).await?;

    assert!(info_updates.next().await.is_some());

    assert_eq!(alice_room.num_unread_messages(), 1);
    assert_eq!(alice_room.num_unread_notifications(), 1);
    assert_eq!(alice_room.num_unread_mentions(), 0);

    assert_pending!(info_updates);

    // Bob sends a mention message.
    bob_room
        .send(
            RoomMessageEventContent::text_plain("Hello my dear friend Alice!")
                .set_mentions(Mentions::with_user_ids([alice.user_id().unwrap().to_owned()])),
        )
        .await?;

    assert!(info_updates.next().await.is_some());

    // The highlight also counts as a notification.
    assert_eq!(alice_room.num_unread_messages(), 2);
    assert_eq!(alice_room.num_unread_notifications(), 2);
    assert_eq!(alice_room.num_unread_mentions(), 1);

    assert_pending!(info_updates);

    // Alice marks the room as read.
    let event_id = latest_event.lock().await.take().unwrap().event_id().to_owned();
    alice_room.send_single_receipt(ReceiptType::Read, ReceiptThread::Unthreaded, event_id).await?;

    // Remote echo of marking the room as read.
    assert!(info_updates.next().await.is_some());

    // Sometimes, we get a spurious update quickly.
    let _ = timeout(Duration::from_secs(2), info_updates.next()).await;

    assert_eq!(alice_room.num_unread_messages(), 0);
    assert_eq!(alice_room.num_unread_notifications(), 0);
    assert_eq!(alice_room.num_unread_mentions(), 0);

    assert_pending!(info_updates);

    // Alice sends a message.
    alice_room.send(RoomMessageEventContent::text_plain("hello bob")).await?;

    // Local echo for our own message.
    assert!(info_updates.next().await.is_some());

    assert_eq!(alice_room.num_unread_messages(), 0);
    assert_eq!(alice_room.num_unread_notifications(), 0);
    assert_eq!(alice_room.num_unread_mentions(), 0);

    // Remote echo for our own message.
    assert!(info_updates.next().await.is_some());

    assert_eq!(alice_room.num_unread_messages(), 0);
    assert_eq!(alice_room.num_unread_notifications(), 0);
    assert_eq!(alice_room.num_unread_mentions(), 0);

    assert_pending!(info_updates);

    // Now Alice is only interesting in mentions of their name.
    let settings = alice.notification_settings().await;

    tracing::warn!("Updating room notification mode to mentions and keywords only...");
    settings
        .set_room_notification_mode(
            alice_room.room_id(),
            matrix_sdk::notification_settings::RoomNotificationMode::MentionsAndKeywordsOnly,
        )
        .await?;
    tracing::warn!("Done!");

    // Wait for remote echo.
    let _ = settings.subscribe_to_changes().recv().await;

    bob_room.send(RoomMessageEventContent::text_plain("I said hello!")).await?;

    assert!(info_updates.next().await.is_some());

    // The message doesn't contain a mention, so it doesn't notify Alice. But it
    // exists.
    assert_eq!(alice_room.num_unread_messages(), 1);
    assert_eq!(alice_room.num_unread_notifications(), 0);
    assert_eq!(alice_room.num_unread_mentions(), 0);

    assert_pending!(info_updates);

    // Bob sends a mention message.
    bob_room
        .send(
            RoomMessageEventContent::text_plain("Why, hello there Alice!")
                .set_mentions(Mentions::with_user_ids([alice.user_id().unwrap().to_owned()])),
        )
        .await?;

    assert!(info_updates.next().await.is_some());

    // The highlight also counts as a notification.
    assert_eq!(alice_room.num_unread_messages(), 2);
    assert_eq!(alice_room.num_unread_notifications(), 1);
    assert_eq!(alice_room.num_unread_mentions(), 1);

    assert_pending!(info_updates);

    Ok(())
}
