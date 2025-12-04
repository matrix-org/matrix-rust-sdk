// TODO: Remove this once all tests are re-enabled.
#![allow(unused)]

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use anyhow::Result;
use assert_matches::assert_matches;
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::{StreamExt as _, pin_mut};
use matrix_sdk::{
    Client, Room, RoomInfo, RoomMemberships, RoomState, SlidingSyncList, SlidingSyncMode,
    bytes::Bytes,
    config::SyncSettings,
    room_preview::RoomPreview,
    ruma::{
        RoomId,
        api::client::{
            receipt::create_receipt::v3::ReceiptType,
            room::create_room::v3::{Request as CreateRoomRequest, RoomPreset},
        },
        assign,
        directory::PublicRoomsChunkInit,
        events::{
            AnySyncMessageLikeEvent, InitialStateEvent, Mentions, StateEventType,
            receipt::ReceiptThread,
            room::{
                history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
                join_rules::{JoinRule, RoomJoinRulesEventContent},
                message::RoomMessageEventContent,
            },
        },
        mxc_uri, owned_server_name,
        room::JoinRuleSummary,
        room_id, uint,
    },
    sliding_sync::VersionBuilder,
    test_utils::{logged_in_client_with_server, mocks::MatrixMockServer},
};
use matrix_sdk_base::ruma::{
    api::client::sync::sync_events::v5 as http, owned_room_id, room_alias_id,
};
use matrix_sdk_test::async_test;
use matrix_sdk_ui::{
    RoomListService, room_list_service::filters::new_filter_all, sync_service::SyncService,
    timeline::RoomExt,
};
use once_cell::sync::Lazy;
use rand::Rng as _;
use serde_json::Value;
use stream_assert::assert_pending;
use tokio::{
    spawn,
    sync::Mutex,
    time::{sleep, timeout},
};
use tracing::{debug, error, info, trace, warn};
use wiremock::{Mock, MockServer, matchers::AnyMatcher};

use crate::helpers::{TestClientBuilder, wait_for_room};

#[tokio::test]
async fn test_left_room() -> Result<()> {
    let peter = TestClientBuilder::new("peter").use_sqlite().build().await?;
    let steven = TestClientBuilder::new("steven").use_sqlite().build().await?;

    // Set up sliding sync for Peter.
    let sliding_peter = peter
        .sliding_sync("main")?
        .with_all_extensions()
        .poll_timeout(Duration::from_secs(3))
        .network_timeout(Duration::from_secs(3))
        .add_list(
            SlidingSyncList::builder("all")
                .sync_mode(SlidingSyncMode::new_selective().add_range(0..=20))
                .required_state(vec![(StateEventType::RoomMember, "$ME".to_owned())]),
        )
        .build()
        .await?;

    spawn({
        let peter_sliding = sliding_peter.clone();

        async move {
            let stream = peter_sliding.sync();
            pin_mut!(stream);

            while let Some(up) = stream.next().await {
                let up = up.expect("sync should not fail");

                warn!("received update: {up:?}");
            }
        }
    });

    // Set up regular sync for Steven.
    spawn({
        let steven = steven.clone();

        async move {
            if let Err(err) = steven.sync(SyncSettings::default()).await {
                error!("steven couldn't sync: {err}");
            }
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
    for _ in 0..10 {
        sleep(Duration::from_secs(1)).await;

        if let Some(room) = steven.get_room(peter_room.room_id()) {
            room.join().await?;
            joined = true;
            break;
        }
    }

    assert!(joined, "steven couldn't join after 10 seconds");

    // Now Peter is just being rude.
    peter_room.leave().await?;

    let mut left = false;

    for _ in 0..10 {
        sleep(Duration::from_secs(1)).await;

        let peter_room = peter.get_room(peter_room.room_id()).unwrap();

        if peter_room.state() == RoomState::Left {
            left = true;
            break;
        }
    }

    assert!(left, "peter couldn't leave after 10 seconds");

    Ok(())
}

#[tokio::test]
async fn test_room_avatar_group_conversation() -> Result<()> {
    let alice = TestClientBuilder::new("alice").use_sqlite().build().await?;
    let bob = TestClientBuilder::new("bob").use_sqlite().build().await?;
    let celine = TestClientBuilder::new("celine").use_sqlite().build().await?;

    // Bob and Celine set their avatars.
    bob.account().set_avatar_url(Some(mxc_uri!("mxc://localhost/bob"))).await?;
    celine.account().set_avatar_url(Some(mxc_uri!("mxc://localhost/celine"))).await?;

    // Set up sliding sync for alice.
    let alice_sliding = alice
        .sliding_sync("main")?
        .with_all_extensions()
        .poll_timeout(Duration::from_secs(2))
        .network_timeout(Duration::from_secs(2))
        .add_list(
            SlidingSyncList::builder("all")
                .sync_mode(SlidingSyncMode::new_selective().add_range(0..=20))
                .required_state(vec![(StateEventType::RoomAvatar, "".to_owned())]),
        )
        .build()
        .await?;

    spawn({
        let alice_sliding = alice_sliding.clone();

        async move {
            let stream = alice_sliding.sync();
            pin_mut!(stream);

            while let Some(up) = stream.next().await {
                let up = up.expect("update must not fail");

                warn!("received update: {up:?}");
            }
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

    let bob = TestClientBuilder::new("bob").use_sqlite().build().await?;

    let alice = TestClientBuilder::new("alice").use_sqlite().build().await?;

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
    let alice_id = alice.user_id().unwrap().localpart().to_owned();

    let alice = Client::builder()
        .homeserver_url(hs)
        .sliding_sync_version_builder(VersionBuilder::Native)
        .build()
        .await
        .unwrap();
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

struct UpdateObserver {
    subscriber: eyeball::Subscriber<RoomInfo>,
    prev_update_json: Value,
}

impl UpdateObserver {
    fn new(subscriber: eyeball::Subscriber<RoomInfo>) -> Self {
        Self { subscriber, prev_update_json: Value::Null }
    }

    /// Retrieves the next update, and shows the (JSON) diff with the previous
    /// one, if any.
    ///
    /// Returns `None` when the diff was empty, aka it was a spurious update.
    async fn next(&mut self) -> Option<RoomInfo> {
        // Wait for the room info updates to stabilize.
        let mut update = None;

        while let Ok(Some(up)) = timeout(Duration::from_secs(2), self.subscriber.next()).await {
            update = Some(up);
        }
        let update = update.expect("there should have been an update");

        let update_json = serde_json::to_value(&update).unwrap();
        let update_diff = json_structural_diff::JsonDiff::diff_string(
            &self.prev_update_json,
            &update_json,
            false,
        );
        if let Some(update_diff) = update_diff {
            debug!("Received update:\n{update_diff}");
            self.prev_update_json = update_json;
            Some(update)
        } else {
            debug!("Update was spurious (diff is empty)");
            None
        }
    }

    #[track_caller]
    fn assert_is_pending(&mut self) {
        assert_pending!(self.subscriber);
    }
}

#[tokio::test]
async fn test_room_notification_count() -> Result<()> {
    use tokio::time::timeout;

    let bob = TestClientBuilder::new("bob").use_sqlite().build().await?;
    let alice = TestClientBuilder::new("alice").use_sqlite().build().await?;

    alice.event_cache().subscribe().unwrap();

    // Spawn sync for Bob.
    spawn({
        let bob = bob.clone();

        async move {
            loop {
                if let Err(err) = bob.sync(Default::default()).await {
                    tracing::error!("bob sync error: {err}");
                }
            }
        }
    });

    // Spawn sync for Alice (with sliding sync).
    spawn({
        let alice_sync = alice
            .sliding_sync("main")?
            .with_receipt_extension(
                assign!(http::request::Receipts::default(), { enabled: Some(true) }),
            )
            .with_account_data_extension(
                assign!(http::request::AccountData::default(), { enabled: Some(true) }),
            )
            .with_e2ee_extension(assign!(http::request::E2EE::default(), { enabled: Some(true) }))
            .with_to_device_extension(
                assign!(http::request::ToDevice::default(), { enabled: Some(true) }),
            )
            .add_list(
                SlidingSyncList::builder("all")
                    .sync_mode(SlidingSyncMode::new_selective().add_range(0..=20))
                    .required_state(vec![(StateEventType::RoomMember, "$ME".to_owned())]),
            )
            .build()
            .await?;

        async move {
            let stream = alice_sync.sync();
            pin_mut!(stream);

            while let Some(update) = stream.next().await {
                warn!(?update, "Alice sliding sync received an update");

                assert!(update.is_ok(), "Syncing Alice via sliding sync has failed");
            }
        }
    });

    let latest_event = Arc::new(Mutex::new(None));

    // Handle new event to update the `latest_event` for Alice.
    alice.add_event_handler({
        let latest_event = latest_event.clone();

        |ev: AnySyncMessageLikeEvent| async move {
            let mut latest_event = latest_event.lock().await;
            *latest_event = Some(ev);
        }
    });

    // Alice creates a room and invites Bob.
    let room_id = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            invite: vec![bob.user_id().unwrap().to_owned()],
            is_direct: true,
        }))
        .await?
        .room_id()
        .to_owned();

    let alice_room = alice.get_room(&room_id).unwrap();
    assert_eq!(alice_room.state(), RoomState::Joined);

    alice_room.enable_encryption().await?;

    let mut update_observer = UpdateObserver::new(alice_room.subscribe_info());

    {
        // At first, nothing has happened, so we shouldn't have any notifications.
        assert_eq!(alice_room.num_unread_messages(), 0);
        assert_eq!(alice_room.num_unread_mentions(), 0);
        assert_eq!(alice_room.num_unread_notifications(), 0);

        update_observer.assert_is_pending();
    }

    // Bob joins, nothing happens.
    bob.join_room_by_id(&room_id).await?;

    {
        debug!("Bob joined the room");

        let update =
            update_observer.next().await.expect("we should get an update when Bob joins the room");

        assert_eq!(update.joined_members_count(), 2);

        assert_eq!(alice_room.num_unread_messages(), 0);
        assert_eq!(alice_room.num_unread_mentions(), 0);
        assert_eq!(alice_room.num_unread_notifications(), 0);

        update_observer.assert_is_pending();
    }

    // Bob sends a non-mention message.
    let bob_room = bob.get_room(&room_id).expect("bob knows about alice's room");
    bob_room.send(RoomMessageEventContent::text_plain("hello world")).await?;

    {
        debug!("Bob sent a non-mention message");
        let update = update_observer
            .next()
            .await
            .expect("we should get an update when Bob sent a non-mention message");

        assert_eq!(alice_room.num_unread_messages(), 1);
        assert_eq!(alice_room.num_unread_notifications(), 1);
        assert_eq!(alice_room.num_unread_mentions(), 0);

        update_observer.assert_is_pending();
    }

    // Bob sends a mention message.
    bob_room
        .send(
            RoomMessageEventContent::text_plain("Hello my dear friend Alice!")
                .add_mentions(Mentions::with_user_ids([alice.user_id().unwrap().to_owned()])),
        )
        .await?;

    {
        debug!("Bob sent a mention message");
        update_observer
            .next()
            .await
            .expect("we should get an update when Bob sent a mention message");

        // The highlight also counts as a notification.
        assert_eq!(alice_room.num_unread_messages(), 2);
        assert_eq!(alice_room.num_unread_notifications(), 2);
        assert_eq!(alice_room.num_unread_mentions(), 1);

        update_observer.assert_is_pending();
    }

    // Alice marks the room as read.
    let event_id = latest_event.lock().await.take().unwrap().event_id().to_owned();
    alice_room.send_single_receipt(ReceiptType::Read, ReceiptThread::Unthreaded, event_id).await?;

    {
        debug!("Remote echo of marking the room as read");
        update_observer.next().await;

        assert!(!alice_room.are_members_synced());

        assert_eq!(alice_room.num_unread_messages(), 0);
        assert_eq!(alice_room.num_unread_notifications(), 0);
        assert_eq!(alice_room.num_unread_mentions(), 0);

        // Sometimes the server is slow at realizing that the room has been marked as
        // read, and zeroing the server-side notification_count.
        if alice_room.unread_notification_counts().notification_count == 2 {
            update_observer
                .next()
                .await
                .expect("server should fix the server-side notification count");
            assert_eq!(alice_room.unread_notification_counts().notification_count, 0);
        }

        update_observer.assert_is_pending();
    }

    // Alice sends a message.
    alice_room.send(RoomMessageEventContent::text_plain("hello bob")).await?;

    {
        debug!("Room members got synced + remote echo for hello bob.");
        update_observer.next().await.expect("syncing room members should update room info");

        assert!(alice_room.are_members_synced());

        assert_eq!(alice_room.num_unread_messages(), 0);
        assert_eq!(alice_room.num_unread_notifications(), 0);
        assert_eq!(alice_room.num_unread_mentions(), 0);

        update_observer.assert_is_pending();
    }

    // Now Alice is only interesting in mentions of their name.
    let settings = alice.notification_settings().await;

    let mut settings_changes = settings.subscribe_to_changes();

    warn!("Updating room notification mode to mentions and keywords only...");
    settings
        .set_room_notification_mode(
            alice_room.room_id(),
            matrix_sdk::notification_settings::RoomNotificationMode::MentionsAndKeywordsOnly,
        )
        .await?;
    warn!("Done!");

    // Wait for remote echo.
    timeout(Duration::from_secs(3), settings_changes.recv())
        .await
        .expect("timeout when waiting for settings update")
        .expect("should've received echo after updating settings");

    bob_room.send(RoomMessageEventContent::text_plain("I said hello!")).await?;

    debug!("Bob sent 'I said hello!'");
    assert!(update_observer.next().await.is_some());

    // The message doesn't contain a mention, so it doesn't notify Alice. But it
    // exists.
    assert_eq!(alice_room.num_unread_messages(), 1);
    assert_eq!(alice_room.num_unread_notifications(), 0);
    assert_eq!(alice_room.num_unread_mentions(), 0);

    // Bob sends a mention message.
    bob_room
        .send(
            RoomMessageEventContent::text_plain("Why, hello there Alice!")
                .add_mentions(Mentions::with_user_ids([alice.user_id().unwrap().to_owned()])),
        )
        .await?;

    debug!("Bob sent 'Why, hello there Alice!'");
    assert!(update_observer.next().await.is_some());

    // The highlight also counts as a notification.
    assert_eq!(alice_room.num_unread_messages(), 2);
    assert_eq!(alice_room.num_unread_notifications(), 1);
    assert_eq!(alice_room.num_unread_mentions(), 1);

    update_observer.assert_is_pending();

    Ok(())
}

/// Response preprocessor that drops to_device events
fn drop_todevice_events(response: &mut Bytes) {
    // Looks for a json payload containing "extensions" with a "to_device" part.
    // This should only match the sliding sync response. In all other cases, it
    // makes no changes.
    let Ok(mut json) = serde_json::from_slice::<Value>(response) else {
        return;
    };
    let Some(extensions) = json.get_mut("extensions").and_then(|e| e.as_object_mut()) else {
        return;
    };
    // Remove to_device field if it exists
    let Some(to_device) = extensions.remove("to_device") else {
        return;
    };

    info!("Dropping to_device: {to_device}");
    *response = serde_json::to_vec(&json).unwrap().into();
}

/// Proxy between client and homeserver that can do arbitrary changes to the
/// payloads.
///
/// It uses wiremock, but sends the actual server when it runs.
struct CustomResponder {
    client: reqwest::Client,
    drop_todevice: Arc<StdMutex<bool>>,
}

impl CustomResponder {
    fn new() -> Self {
        Self { client: reqwest::Client::new(), drop_todevice: Arc::new(StdMutex::new(true)) }
    }
}

impl wiremock::Respond for &CustomResponder {
    fn respond(&self, request: &wiremock::Request) -> wiremock::ResponseTemplate {
        // Convert the mocked request to an actual server request.
        let req = self
            .client
            .request(request.method.clone(), request.url.clone())
            .headers(request.headers.clone())
            .body(request.body.clone());

        // Run await inside of non-async fn by spawning a new thread and creating a new
        // runtime. We need to do this because the current runtime can't run blocking
        // tasks (hence can't run `Handle::block_on`).
        let drop_todevice = self.drop_todevice.clone();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                // Send the request to the actual backend.
                let response = timeout(Duration::from_secs(2), req.send()).await;

                if let Ok(Ok(response)) = response {
                    // Convert reqwest response to wiremock response.
                    let mut r = wiremock::ResponseTemplate::new(u16::from(response.status()));
                    for header in response.headers() {
                        if header.0 == "Content-Length" {
                            continue;
                        }
                        r = r.append_header(header.0.clone(), header.1.clone());
                    }

                    let mut bytes = response.bytes().await.unwrap_or_default();

                    // Manipulate the response
                    if *drop_todevice.lock().unwrap() {
                        drop_todevice_events(&mut bytes);
                    }

                    r.set_body_bytes(bytes)
                } else {
                    // Gateway timeout.
                    wiremock::ResponseTemplate::new(504)
                }
            })
        })
        .join()
        .expect("Thread panicked")
    }
}

async fn get_or_wait_for_room(client: &Client, room_id: &RoomId) -> Room {
    let (mut rooms, mut room_stream) = client.rooms_stream();

    loop {
        if let Some(room) = rooms.iter().find(|room| room.room_id() == room_id) {
            return room.clone();
        }

        if let Ok(Some(diffs)) = timeout(Duration::from_secs(3), room_stream.next()).await {
            for diff in diffs {
                diff.apply(&mut rooms);
            }
        } else {
            panic!("bob never founds out about the room");
        }
    }
}

#[tokio::test]
async fn test_delayed_invite_response_and_sent_message_decryption() {
    let alice = TestClientBuilder::new("alice").use_sqlite().build().await.unwrap();
    let bob = TestClientBuilder::new("bob").use_sqlite().build().await.unwrap();

    let alice_sync_service = SyncService::builder(alice.clone()).build().await.unwrap();
    alice_sync_service.start().await;

    let bob_sync_service = SyncService::builder(bob.clone()).build().await.unwrap();
    bob_sync_service.start().await;

    // Alice creates a room and will invite Bob.
    let alice_room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            invite: vec![],
            is_direct: true,
            preset: Some(RoomPreset::PrivateChat),
        }))
        .await
        .unwrap();
    alice_room.enable_encryption().await.unwrap();

    // Initial message to make sure any lazy /members call is performed before the
    // test actually starts
    alice_room
        .send(RoomMessageEventContent::text_plain("dummy message to make members call"))
        .await
        .unwrap();

    // Send the invite to Bob and a message to reproduce the edge case
    alice_room.invite_user_by_id(bob.user_id().unwrap()).await.unwrap();
    alice_room.send(RoomMessageEventContent::text_plain("hello world")).await.unwrap();

    let room_id = alice_room.room_id();

    // Wait until Bob receives the invite.
    let bob_room = get_or_wait_for_room(&bob, room_id).await;

    // Join the room from Bob's client.
    let bob_timeline = bob_room.timeline().await.unwrap();
    let (_, timeline_stream) = bob_timeline.subscribe().await;
    pin_mut!(timeline_stream);

    info!("Bob joins the room.");
    bob_room.join().await.unwrap();

    assert_eq!(alice_room.state(), RoomState::Joined);
    assert!(alice_room.latest_encryption_state().await.unwrap().is_encrypted());
    assert_eq!(bob_room.state(), RoomState::Joined);
    assert!(bob_room.latest_encryption_state().await.unwrap().is_encrypted());

    // Get previous events, including the sent messages.
    bob_timeline.paginate_backwards(3).await.unwrap();

    // Look for the sent message, which should not be an UTD event.
    while let Ok(Some(diffs)) = timeout(Duration::from_secs(3), timeline_stream.next()).await {
        trace!(?diffs, "Received diffs from Bob's room");

        for diff in diffs {
            match diff {
                VectorDiff::PushFront { value: event }
                | VectorDiff::PushBack { value: event }
                | VectorDiff::Insert { value: event, .. }
                | VectorDiff::Set { value: event, .. } => {
                    let Some(event) = event.as_event() else {
                        continue;
                    };

                    if event.content().is_unable_to_decrypt() {
                        info!("Observed UTD for {}", event.event_id().unwrap());
                    }

                    if let Some(message) = event.content().as_message() {
                        assert_eq!(message.body(), "hello world");
                        return;
                    }
                }

                _ => {}
            }
        }
    }

    panic!("We never received the decrypted event!");
}

#[tokio::test]
#[ignore]
async fn test_room_info_notable_update_deduplication() -> Result<()> {
    let alice = TestClientBuilder::new("alice").use_sqlite().build().await?;
    let bob = TestClientBuilder::new("bob").use_sqlite().build().await?;

    // Set up sliding sync and encryption for Alice.
    let alice_sync_service = SyncService::builder(alice.clone()).build().await.unwrap();
    alice_sync_service.start().await;

    // Set up sliding sync for bob.
    let bob_sync_service = SyncService::builder(bob.clone()).build().await.unwrap();
    bob_sync_service.start().await;

    // alice creates a room and invites bob.
    let alice_room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            invite: vec![bob.user_id().unwrap().to_owned()],
            is_direct: true,
            preset: Some(RoomPreset::TrustedPrivateChat),
        }))
        .await?;

    alice_room.enable_encryption().await.unwrap();

    let alice_room_list = alice_sync_service.room_list_service().all_rooms().await.unwrap();
    let (alice_rooms, alice_room_controller) = alice_room_list.entries_with_dynamic_adapters(10);

    alice_room_controller.set_filter(Box::new(new_filter_all(vec![])));

    pin_mut!(alice_rooms);

    // First, we observe the initial reset.
    assert_let!(Ok(Some(diffs)) = timeout(Duration::from_secs(3), alice_rooms.next()).await);
    assert_eq!(diffs.len(), 1);
    assert_matches!(
        &diffs[0],
        VectorDiff::Reset { values: rooms } => {
            assert_eq!(rooms.len(), 1);
            assert_eq!(rooms[0].room_id(), alice_room.room_id());
        }
    );

    assert_pending!(alice_rooms);

    // Alice sees the room.
    let alice_room = wait_for_room(&alice, alice_room.room_id()).await;
    assert_eq!(alice_room.state(), RoomState::Joined);

    assert!(alice_room.latest_encryption_state().await.unwrap().is_encrypted());

    // Bob sees and joins the room.
    let bob_room = wait_for_room(&bob, alice_room.room_id()).await;

    bob_room.join().await.unwrap();

    // Wait Bob to be in the room.
    sleep(Duration::from_secs(2)).await;
    assert_eq!(bob_room.state(), RoomState::Joined);

    assert_pending!(alice_rooms);

    // Send a message, it should arrive.
    let event = bob_room.send(RoomMessageEventContent::text_plain("hello world")).await?;

    // Wait the message from Bob to be sent.
    sleep(Duration::from_secs(2)).await;

    // Room has been updated because of the latest event.
    assert_let!(Ok(Some(diffs)) = timeout(Duration::from_secs(3), alice_rooms.next()).await);
    assert_eq!(diffs.len(), 1);
    assert_matches!(
        &diffs[0],
        VectorDiff::Set { index: 0, value: room } => {
            assert_eq!(room.room_id(), alice_room.room_id());
        }
    );

    // And there are no subsequent room updates.
    sleep(Duration::from_secs(2)).await;
    assert_pending!(alice_rooms);

    Ok(())
}

#[tokio::test]
async fn test_room_preview() -> Result<()> {
    let alice = TestClientBuilder::new("alice").use_sqlite().build().await?;
    let bob = TestClientBuilder::new("bob").use_sqlite().build().await?;

    let alice_sync_service = SyncService::builder(alice.clone()).build().await.unwrap();
    alice_sync_service.start().await;

    // Set up sliding sync for alice.
    let sliding_alice = alice
        .sliding_sync("main")?
        .with_all_extensions()
        .poll_timeout(Duration::from_secs(30))
        .network_timeout(Duration::from_secs(30))
        .add_list(
            SlidingSyncList::builder("all")
                .sync_mode(SlidingSyncMode::new_selective().add_range(0..=20))
                .required_state(vec![
                    // Explicitly request all the state events we need to get a preview for a known
                    // room.
                    (StateEventType::RoomName, "".to_owned()),
                    (StateEventType::RoomCanonicalAlias, "".to_owned()),
                    (StateEventType::RoomTopic, "".to_owned()),
                    (StateEventType::RoomCreate, "".to_owned()),
                    (StateEventType::RoomJoinRules, "".to_owned()),
                    (StateEventType::RoomHistoryVisibility, "".to_owned()),
                ]),
        )
        .build()
        .await?;

    // Alice creates a room in which they're alone, to start with.
    let suffix: u128 = rand::thread_rng().r#gen();
    let room_alias = format!("aliasy_mac_alias{suffix}");

    let room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            invite: vec![],
            is_direct: false,
            name: Some("Alice's Room".to_owned()),
            topic: Some("Discussing Alice's Topic".to_owned()),
            room_alias_name: Some(room_alias.clone()),
            initial_state: vec![
                InitialStateEvent::with_empty_state_key(RoomHistoryVisibilityEventContent::new(HistoryVisibility::WorldReadable)).to_raw_any(),
                InitialStateEvent::with_empty_state_key(RoomJoinRulesEventContent::new(JoinRule::Invite)).to_raw_any(),
            ],
        }))
        .await?;

    room.set_avatar_url(mxc_uri!("mxc://localhost/alice"), None).await?;

    // Alice creates another room, and still doesn't invite Bob.
    let private_room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            name: Some("Alice's Room 2".to_owned()),
            initial_state: vec![
                InitialStateEvent::with_empty_state_key(RoomHistoryVisibilityEventContent::new(HistoryVisibility::Shared)).to_raw_any(),
                InitialStateEvent::with_empty_state_key(RoomJoinRulesEventContent::new(JoinRule::Public)).to_raw_any(),
            ],
        }))
        .await?;

    let room_id = room.room_id();
    let private_room_id = private_room.room_id();

    // Wait for Alice's stream to stabilize (stop updating when we haven't received
    // successful updates for more than 2 seconds).
    let stream = sliding_alice.sync();
    pin_mut!(stream);

    // Wait for updates coming in under than 15 seconds. After that, we consider the
    // sync as stable.
    loop {
        match timeout(Duration::from_secs(15), stream.next()).await {
            Ok(None) | Err(_) => break,
            Ok(Some(up)) => {
                warn!("alice got an update: {up:?}");
            }
        }
    }

    get_room_preview_with_room_state(&alice, &bob, &room_alias, room_id, private_room_id).await;
    get_room_preview_with_room_summary(&alice, &bob, &room_alias, room_id, private_room_id).await;

    {
        // Dummy test for `Client::get_room_preview` which may call one or the other
        // methods.
        info!("Alice gets a preview of the public room using any method");
        let preview = alice.get_room_preview(room_id.into(), Vec::new()).await?;
        assert_room_preview(&preview, &room_alias);
        assert_eq!(preview.state, Some(RoomState::Joined));
        assert!(preview.heroes.is_some());
    }

    Ok(())
}

#[async_test]
async fn test_room_preview_with_room_directory_search_and_room_alias_only() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_alias = room_alias_id!("#a-room:matrix.org");
    let expected_room_id = room_id!("!a-room:matrix.org");

    // Allow resolving the room via the room directory
    server
        .mock_room_directory_resolve_alias()
        .ok(expected_room_id.as_ref(), Vec::new())
        .mock_once()
        .mount()
        .await;

    // Given a successful public room search
    let chunks = vec![
        PublicRoomsChunkInit {
            num_joined_members: uint!(0),
            room_id: expected_room_id.to_owned(),
            world_readable: true,
            guest_can_join: true,
        }
        .into(),
    ];
    server.mock_public_rooms().ok(chunks, None, None, Some(1)).mock_once().mount().await;

    // The room preview is found
    let preview = client
        .get_room_preview(room_alias.into(), Vec::new())
        .await
        .expect("room preview couldn't be retrieved");
    assert_eq!(preview.room_id, expected_room_id);
    assert!(preview.heroes.is_none());
}

#[async_test]
async fn test_room_preview_with_room_directory_search_and_room_alias_only_in_several_homeservers() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_alias = room_alias_id!("#a-room:matrix.org");
    let expected_room_id = room_id!("!a-room:matrix.org");

    // Allow resolving the room via the room directory
    server
        .mock_room_directory_resolve_alias()
        .ok(expected_room_id.as_ref(), Vec::new())
        .mock_once()
        .mount()
        .await;

    let via_1 = owned_server_name!("server1.com");
    let via_2 = owned_server_name!("server2.com");

    // Given a couple of successful public room search responses
    let via_map = BTreeMap::from_iter(vec![
        (
            via_1.to_owned(),
            // The actual room we want
            vec![
                PublicRoomsChunkInit {
                    num_joined_members: uint!(0),
                    room_id: expected_room_id.to_owned(),
                    world_readable: true,
                    guest_can_join: true,
                }
                .into(),
            ],
        ),
        (
            via_2.to_owned(),
            // Some other room
            vec![
                PublicRoomsChunkInit {
                    num_joined_members: uint!(1),
                    room_id: owned_room_id!("!some-other-room:matrix.org"),
                    world_readable: true,
                    guest_can_join: true,
                }
                .into(),
            ],
        ),
    ]);
    server
        .mock_public_rooms()
        .ok_with_via_params(via_map)
        // Expect this to be called once for every server in the `via_map`
        .expect(2)
        .mount()
        .await;

    // The room preview is found in the first response
    let preview = client
        .get_room_preview(room_alias.into(), vec![via_1, via_2])
        .await
        .expect("room preview couldn't be retrieved");
    assert_eq!(preview.room_id, expected_room_id);
    assert!(preview.heroes.is_none());
}

fn assert_room_preview(preview: &RoomPreview, room_alias: &str) {
    assert_eq!(preview.canonical_alias.as_ref().unwrap().alias(), room_alias);
    assert_eq!(preview.name.as_ref().unwrap(), "Alice's Room");
    assert_eq!(preview.topic.as_ref().unwrap(), "Discussing Alice's Topic");
    assert_eq!(preview.avatar_url.as_ref().unwrap(), mxc_uri!("mxc://localhost/alice"));
    assert_eq!(preview.num_joined_members, 1);
    assert!(preview.room_type.is_none());
    assert_eq!(preview.join_rule, Some(JoinRuleSummary::Invite));
    assert!(preview.is_world_readable.unwrap());
}

async fn get_room_preview_with_room_state(
    alice: &Client,
    bob: &Client,
    room_alias: &str,
    room_id: &RoomId,
    public_no_history_room_id: &RoomId,
) {
    // Alice has joined the room, so they get the full details.
    info!("Alice gets a preview of the public room from state events");
    let preview = RoomPreview::from_state_events(alice, room_id).await.unwrap();
    assert_room_preview(&preview, room_alias);
    assert_eq!(preview.state, Some(RoomState::Joined));
    assert!(preview.heroes.is_some());

    // Bob definitely doesn't know about the room, but they can get a preview of the
    // room too.
    info!("Bob gets a preview of the public room from state events");
    let preview = RoomPreview::from_state_events(bob, room_id).await.unwrap();
    assert_room_preview(&preview, room_alias);
    assert!(preview.state.is_none());
    assert!(preview.heroes.is_some());

    // Bob can't preview the second room, because its history visibility is neither
    // world-readable, nor have they joined the room before.
    info!("Bob gets a preview of the private room from state events");
    let preview_result = RoomPreview::from_state_events(bob, public_no_history_room_id).await;
    assert_eq!(preview_result.unwrap_err().as_client_api_error().unwrap().status_code, 403);
}

async fn get_room_preview_with_room_summary(
    alice: &Client,
    bob: &Client,
    room_alias: &str,
    room_id: &RoomId,
    public_no_history_room_id: &RoomId,
) {
    // Alice has joined the room, so they get the full details.
    info!("Alice gets a preview of the public room from msc3266 using the room id");
    let preview =
        RoomPreview::from_room_summary(alice, room_id.to_owned(), room_id.into(), Vec::new())
            .await
            .unwrap();

    assert_room_preview(&preview, room_alias);
    assert_eq!(preview.state, Some(RoomState::Joined));
    assert!(preview.heroes.is_some());

    // The preview also works when using the room alias parameter.
    info!("Alice gets a preview of the public room from msc3266 using the room alias");
    let full_alias = format!("#{room_alias}:{}", alice.user_id().unwrap().server_name());
    let preview = RoomPreview::from_room_summary(
        alice,
        room_id.to_owned(),
        <_>::try_from(full_alias.as_str()).unwrap(),
        Vec::new(),
    )
    .await
    .unwrap();

    assert_room_preview(&preview, room_alias);
    assert_eq!(preview.state, Some(RoomState::Joined));
    assert!(preview.heroes.is_some());

    // Bob definitely doesn't know about the room, but they can get a preview of the
    // room too.
    info!("Bob gets a preview of the public room from msc3266 using the room id");
    let preview =
        RoomPreview::from_room_summary(bob, room_id.to_owned(), room_id.into(), Vec::new())
            .await
            .unwrap();
    assert_room_preview(&preview, room_alias);
    assert!(preview.state.is_none());
    assert!(preview.heroes.is_none());

    // Bob can preview the second room with the room summary (because its join rule
    // is set to public, or because Alice is a member of that room).
    info!("Bob gets a preview of the private room from msc3266 using the room id");
    let preview = RoomPreview::from_room_summary(
        bob,
        public_no_history_room_id.to_owned(),
        public_no_history_room_id.into(),
        Vec::new(),
    )
    .await
    .unwrap();
    assert_eq!(preview.name.unwrap(), "Alice's Room 2");
    assert!(preview.state.is_none());
    assert!(preview.heroes.is_none());
}
