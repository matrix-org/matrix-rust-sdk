use std::{ops::Not as _, time::Duration};

use anyhow::Result;
use assert_matches2::{assert_let, assert_matches};
use matrix_sdk::{
    RoomState,
    encryption::{BackupDownloadStrategy, EncryptionSettings, recovery::RecoveryState},
    event_cache::RoomEventCacheUpdate,
    latest_events::LatestEventValue,
    room::{MessagesOptions, edit::EditedContent::RoomMessage},
    ruma::{
        api::client::room::create_room::v3::{Request as CreateRoomRequest, RoomPreset},
        assign, event_id,
        events::{
            self, AnyRoomAccountDataEventContent, AnySyncStateEvent, AnySyncTimelineEvent,
            Mentions, RoomAccountDataEventContent,
            room::message::{
                OriginalSyncRoomMessageEvent, RoomMessageEventContent,
                RoomMessageEventContentWithoutRelation,
            },
        },
        serde::Raw,
        uint,
    },
    test_utils::assert_event_matches_msg,
};
use matrix_sdk_test::TestResult;
use matrix_sdk_ui::sync_service::SyncService;
use tokio::{spawn, time::sleep};
use tracing::{debug, error};

use crate::helpers::{TestClientBuilder, wait_for_room};

#[tokio::test]
async fn test_event_with_context() -> Result<()> {
    let bob = TestClientBuilder::new("bob").use_sqlite().build().await?;

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

    let alice = TestClientBuilder::new("alice").use_sqlite().build().await?;

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

    let alice_room = wait_for_room(&alice, &room_id).await;

    // Bob joins it.
    let mut bob_joined = false;
    for i in 1..=5 {
        if let Some(room) = bob.get_room(&room_id) {
            room.join().await?;
            bob_joined = true;
            break;
        }
        sleep(Duration::from_millis(500 * i)).await;
    }
    anyhow::ensure!(bob_joined, "bob couldn't join after ~8 seconds");

    assert_eq!(alice_room.state(), RoomState::Joined);

    alice_room.enable_encryption().await?;

    for i in 0..10 {
        alice_room.send(RoomMessageEventContent::text_plain(i.to_string())).await?;
    }

    let send_event_result =
        alice_room.send(RoomMessageEventContent::text_plain("hello there!")).await?;
    let event_id = send_event_result.response.event_id;

    for i in 0..10 {
        alice_room.send(RoomMessageEventContent::text_plain((i + 10).to_string())).await?;
    }

    let room = bob.get_room(alice_room.room_id()).expect("bob has joined the room");

    {
        // First /context query: only the target event, no context around it.
        let response = room.event_with_context(&event_id, false, uint!(0), None).await?;

        let target = response
            .event
            .expect("there should be an event")
            .raw()
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
        let response =
            room.event_with_context(event_id!("$lolololol"), false, uint!(0), None).await;

        // Servers answers with 404.
        assert_let!(Err(err) = response);
        assert_eq!(err.as_client_api_error().unwrap().status_code.as_u16(), 404);
    }

    {
        // Next query: target event with a context of 3 events. There
        // should be some previous and next tokens.
        let response = room.event_with_context(&event_id, false, uint!(3), None).await?;

        let target = response
            .event
            .expect("there should be an event")
            .raw()
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
        let event = prev_events[9].raw().deserialize().unwrap();
        assert_matches!(event, AnySyncTimelineEvent::State(AnySyncStateEvent::RoomEncryption(_)));

        // There are other events before that (room creation, alice joining).
        assert!(prev_messages.end.is_some());
    }

    Ok(())
}

#[tokio::test]
async fn test_room_account_data() -> Result<()> {
    let alice = TestClientBuilder::new("alice").use_sqlite().build().await?;

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
    let room_id = alice.create_room(CreateRoomRequest::new()).await?.room_id().to_owned();

    let alice_room = wait_for_room(&alice, &room_id).await;

    // ensure clean

    let tag =
        alice_room.account_data_static::<events::marked_unread::MarkedUnreadEventContent>().await?;
    assert!(tag.is_none());

    let tag = alice_room.account_data_static::<events::tag::TagEventContent>().await?;
    assert!(tag.is_none());

    // set a raw one
    let marked_unread_content = events::marked_unread::MarkedUnreadEventContent::new(true);
    let full_event: AnyRoomAccountDataEventContent = marked_unread_content.clone().into();
    alice_room
        .set_account_data_raw(marked_unread_content.event_type(), Raw::new(&full_event).unwrap())
        .await?;

    let mut tags = events::tag::Tags::new();
    tags.insert(events::tag::TagName::from("u.custom_name"), events::tag::TagInfo::new());

    let new_tag = events::tag::TagEventContent::new(tags);

    // let's set this
    alice_room.set_account_data(new_tag.clone()).await?;

    let mut countdown = 30;
    let mut found = false;
    while countdown > 0 {
        if let Some(tag) = alice_room.account_data_static::<events::tag::TagEventContent>().await? {
            let _content = tag.deserialize().unwrap().content;
            assert_matches!(new_tag.clone(), _content);
            found = true;
            break;
        }
        sleep(Duration::from_millis(100)).await;
        countdown -= 1;
    }

    assert!(found, "Even after 3 seconds the tag was not found");

    // test the non-static method works, too
    let tag = alice_room.account_data(new_tag.event_type()).await?.unwrap();
    let _content = tag.deserialize().unwrap().content();
    assert_matches!(new_tag, _content);

    let new_marked_unread_content =
        alice_room.account_data(marked_unread_content.event_type()).await?.unwrap();
    let _content = new_marked_unread_content.deserialize().unwrap().content();
    assert_matches!(marked_unread_content, _content);

    Ok(())
}

/// Test that UTDs, after having been decrypted, update the unread and
/// notification counts for a given room.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_unread_counts_get_updated_after_decryption() -> TestResult {
    const RECOVERY_PASSPHRASE: &str = "I am error";

    let encryption_settings = EncryptionSettings {
        auto_enable_cross_signing: true,
        auto_enable_backups: true,
        backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
    };

    // Set up sync for user Alice, and create a room.
    let alice1 = TestClientBuilder::new("alice")
        .encryption_settings(encryption_settings)
        .use_sqlite()
        .build()
        .await?;
    let alice_user_id = alice1.user_id().unwrap().to_owned();

    let sync_service1 = SyncService::builder(alice1.clone()).build().await?;
    sync_service1.start().await;

    alice1.encryption().wait_for_e2ee_initialization_tasks().await;
    alice1.encryption().recovery().enable().with_passphrase(RECOVERY_PASSPHRASE).await?;

    debug!("Creating room…");
    let room1 = alice1
        .create_room(assign!(CreateRoomRequest::new(), {
            is_direct: true,
            initial_state: vec![],
            preset: Some(RoomPreset::PrivateChat)
        }))
        .await?;

    let room_id = room1.room_id().to_owned();

    room1.enable_encryption().await?;

    let (original_event_id, edit_event_id) = {
        let bob = TestClientBuilder::new("bob").use_sqlite().build().await?;
        bob.encryption().wait_for_e2ee_initialization_tasks().await;

        let bob_sync_service = SyncService::builder(bob.clone()).build().await?;
        bob_sync_service.start().await;

        // Alice sends an invite to Bob.
        room1.invite_user_by_id(bob.user_id().unwrap()).await?;

        // Bob receives and accepts the invite.
        let bob_room;
        loop {
            if let Some(invited_room) = bob.invited_rooms().first() {
                if invited_room.room_id() == room1.room_id() {
                    invited_room.join().await?;
                    bob_room = invited_room.clone();
                    break;
                }
            } else {
                sleep(Duration::from_millis(100)).await;
                continue;
            }
        }

        // Bob sends an intentional mention to Alice.
        while bob_room.state() != RoomState::Joined {
            sleep(Duration::from_millis(100)).await;
        }

        let send_result = bob_room.send(RoomMessageEventContent::text_plain("hi Alyx!")).await?;
        let original_event_id = send_result.response.event_id;

        // Bob edits that message they sent, and adds intentional mentions in the edit.
        let mentions = Mentions::with_user_ids([alice_user_id.to_owned()]);
        let send_edit_result = bob_room
            .send(
                bob_room
                    .make_edit_event(
                        &original_event_id,
                        RoomMessage(
                            RoomMessageEventContentWithoutRelation::text_plain("hi Alice!")
                                .add_mentions(mentions),
                        ),
                    )
                    .await?,
            )
            .await?;

        (original_event_id, send_edit_result.response.event_id)
    };

    // Alice waits to receive the edit event, which should be decrypted at this
    // point.
    loop {
        if let LatestEventValue::Remote(timeline_event) = room1.latest_event() {
            if timeline_event.event_id().as_ref() == Some(&edit_event_id) {
                let message_event = timeline_event
                    .raw()
                    .deserialize_as_unchecked::<OriginalSyncRoomMessageEvent>()?;
                assert_eq!(message_event.content.body(), "* hi Alice!");

                break;
            }

            sleep(Duration::from_millis(100)).await;
        }
    }

    // Alright, shutting down alice1 now.
    sync_service1.stop().await;
    drop(alice1);

    // Now alice2 comes into play.
    let alice2 = TestClientBuilder::with_exact_username(alice_user_id.localpart().to_owned())
        .encryption_settings(encryption_settings)
        .use_sqlite()
        .build()
        .await?;

    // No rooms as of yet, we have not synced with the server as of yet.
    assert!(alice2.rooms().is_empty());

    alice2.event_cache().subscribe()?;

    let sync_service2 = SyncService::builder(alice2.clone()).build().await?;

    sync_service2.room_list_service().subscribe_to_rooms(&[&room_id]).await;
    sync_service2.start().await;

    alice2.encryption().wait_for_e2ee_initialization_tasks().await;

    // Let's get the room from the new client.
    let room2 = wait_for_room(&alice2, &room_id).await;

    assert!(room2.latest_encryption_state().await?.is_encrypted(), "The room should be encrypted");

    // Paginate events backwards.
    let (event_cache, _drop_handles) = room2.event_cache().await.unwrap();

    let pagination_outcome = event_cache.pagination().run_backwards_until(100).await?;
    assert!(pagination_outcome.reached_start, "Pagination should have finished");

    let (events, mut room_updates) = event_cache.subscribe().await?;

    // Last two events in the room cache should be UTDs.
    let timeline_tail = events.iter().rev().take(2).collect::<Vec<_>>();
    // Note: events are reverted, because of .rev().
    assert_eq!(timeline_tail[0].event_id().as_ref(), Some(&edit_event_id));
    assert!(timeline_tail[0].kind.is_utd());
    assert_eq!(timeline_tail[1].event_id().as_ref(), Some(&original_event_id));
    assert!(timeline_tail[1].kind.is_utd());

    // The unread counts should be incorrect.

    // Only 1 message: the edit has clear info that it's an edit.
    // TODO: we should probably *not* trust it?
    assert_eq!(room2.num_unread_messages(), 1);
    assert_eq!(room2.num_unread_mentions(), 0); // This should be 1 (after decryption).
    // By default, all 1:1 messages are notifications.
    assert_eq!(room2.num_unread_notifications(), 1);

    // Let's now recover.
    alice2.encryption().recovery().recover(RECOVERY_PASSPHRASE).await?;
    assert_eq!(alice2.encryption().recovery().state(), RecoveryState::Enabled);

    assert_let!(
        Ok(RoomEventCacheUpdate::UpdateTimelineEvents(diff_update)) = room_updates.recv().await
    );

    let mut events = events.into();
    for diff in diff_update.diffs {
        diff.apply(&mut events);
    }

    // Last two events in the room cache should now be decrypted!
    let timeline_tail = events.iter().rev().take(2).collect::<Vec<_>>();
    // Note: events are reverted, because of .rev().
    assert_eq!(timeline_tail[0].event_id().as_ref(), Some(&edit_event_id));
    assert_eq!(timeline_tail[1].event_id().as_ref(), Some(&original_event_id));
    assert!(timeline_tail[0].kind.is_utd().not());
    assert!(timeline_tail[1].kind.is_utd().not());

    // And the unread count should now be correct.

    // 1 message (the edit still doesn't count).
    assert_eq!(room2.num_unread_messages(), 1);
    // 1 intentional mention \o/
    assert_eq!(room2.num_unread_mentions(), 1);
    // Both events count for notifications.
    // TODO: Either the original or the edit should count, but not both, as they
    // will often result in a single consolidated item in the UI (#6282).
    assert_eq!(room2.num_unread_notifications(), 2);

    Ok(())
}
