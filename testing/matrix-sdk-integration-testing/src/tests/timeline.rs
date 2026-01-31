// Copyright 2023 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{sync::Arc, time::Duration};

use anyhow::Result;
use assert_matches::assert_matches;
use assert_matches2::assert_let;
use assign::assign;
use eyeball_im::{Vector, VectorDiff};
use futures::pin_mut;
use futures_util::{FutureExt, StreamExt};
use matrix_sdk::{
    Client, Room, RoomState, assert_next_with_timeout,
    config::SyncSettings,
    deserialized_responses::{VerificationLevel, VerificationState},
    encryption::{
        BackupDownloadStrategy, EncryptionSettings, backups::BackupState, recovery::RecoveryState,
    },
    room::{
        edit::EditedContent,
        reply::{EnforceThread, Reply},
    },
    ruma::{
        MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, RoomId, UserId,
        api::client::room::create_room::v3::{Request as CreateRoomRequest, RoomPreset},
        events::{
            InitialStateEvent,
            room::{
                encryption::RoomEncryptionEventContent,
                message::{
                    ReplyWithinThread, RoomMessageEventContent,
                    RoomMessageEventContentWithoutRelation,
                },
            },
        },
    },
};
use matrix_sdk_test::{TestError, TestResult};
use matrix_sdk_ui::{
    Timeline,
    notification_client::NotificationClient,
    room_list_service::RoomListLoadingState,
    sync_service::SyncService,
    timeline::{
        EventSendState, EventTimelineItem, ReactionStatus, RoomExt, TimelineBuilder,
        TimelineEventFocusThreadMode, TimelineFocus, TimelineItem,
    },
};
use similar_asserts::assert_eq;
use stream_assert::assert_pending;
use tokio::{
    spawn,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tracing::{debug, warn};

use crate::helpers::{TestClientBuilder, wait_for_room};

/// Checks that there a timeline update, and returns the EventTimelineItem.
///
/// A macro to help lowering compile times and getting better error locations.
macro_rules! assert_event_is_updated {
    ($diff:expr, $event_id:expr, $index:expr) => {{
        assert_let!(VectorDiff::Set { index: i, value: event } = &$diff);
        assert_eq!(*i, $index, "unexpected position for event update, value = {event:?}");

        let event = event.as_event().unwrap();
        assert_eq!(event.event_id().unwrap(), $event_id);

        event.to_owned()
    }};
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_toggling_reaction() -> Result<()> {
    // Set up sync for user Alice, and create a room.
    let alice = TestClientBuilder::new("alice").use_sqlite().build().await?;

    let alice_clone = alice.clone();
    let alice_sync = spawn(async move {
        alice_clone.sync(Default::default()).await.expect("sync failed!");
    });

    debug!("Creating room…");
    let user_id = alice.user_id().unwrap().to_owned();
    let room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            is_direct: true,
        }))
        .await?;

    // Send a first message and then wait for the remote echo.
    //
    // First, spawn the task waiting for the remote echo, before sending the
    // message. Otherwise, we might get the remote echo before we started
    // waiting for it.

    let timeline = room.timeline().await.unwrap();
    let (mut items, mut stream) = timeline.subscribe().await;

    let event_id_task: JoinHandle<Result<_>> = spawn(async move {
        let find_event_id = |items: &Vector<Arc<TimelineItem>>| {
            items.iter().find_map(|item| {
                let event = item.as_event()?;
                if !event.is_local_echo() && event.content().as_message()?.body().trim() == "hi!" {
                    event.event_id().map(ToOwned::to_owned)
                } else {
                    None
                }
            })
        };

        if let Some(event_id) = find_event_id(&items) {
            return Ok(event_id);
        }

        warn!(?items, "Waiting for updates…");

        while let Some(diffs) = stream.next().await {
            warn!(?diffs, "received diffs");

            for diff in diffs {
                diff.apply(&mut items);
            }

            if let Some(event_id) = find_event_id(&items) {
                return Ok(event_id);
            }
        }

        unreachable!();
    });

    // Create a timeline for this room.
    debug!("Creating timeline…");
    let timeline = Arc::new(room.timeline().await.unwrap());

    // Send message.
    debug!("Sending initial message…");
    timeline.send(RoomMessageEventContent::text_plain("hi!").into()).await.unwrap();

    let event_id = timeout(Duration::from_secs(10), event_id_task)
        .await
        .expect("timeout")
        .expect("failed to join tokio task")
        .expect("waiting for the event id failed");

    alice_sync.abort();
    let _ = alice_sync.await;

    // Give a bit of time for the timeline to process all sync updates.
    sleep(Duration::from_secs(1)).await;

    let (mut items, mut stream) = timeline.subscribe().await;

    // Skip all stream updates that have happened so far.
    debug!("Skipping all other stream updates…");
    while let Some(Some(diffs)) = stream.next().now_or_never() {
        for diff in diffs {
            diff.apply(&mut items);
        }
    }

    let (message_position, item_id) = items
        .iter()
        .enumerate()
        .find_map(|(i, item)| {
            let item = item.as_event()?;
            (item.event_id()? == event_id).then_some((i, item.identifier()))
        })
        .expect("couldn't find the final position for the event id");

    let reaction_key = "👍".to_owned();

    // Toggle reaction multiple times.
    for _ in 0..3 {
        debug!("Starting the toggle reaction tests…");

        // Add the reaction.
        timeline.toggle_reaction(&item_id, &reaction_key).await.expect("toggling reaction");

        sleep(Duration::from_secs(1)).await;

        assert_let!(Some(timeline_updates) = stream.next().await);
        assert_eq!(timeline_updates.len(), 3);

        // Local echo is added.
        {
            let event = assert_event_is_updated!(timeline_updates[0], event_id, message_position);
            let reactions = event.content().reactions().cloned().unwrap_or_default();
            let reactions = reactions.get(&reaction_key).unwrap();
            let reaction = reactions.get(&user_id).unwrap();
            assert_matches!(reaction.status, ReactionStatus::LocalToRemote(..));
        }

        // Remote echo is added twice: one from the Send Queue because the event is sent
        // and inserted in the Event Cache and one from the Event Cache via the sync.
        // The difference is it gets a read receipt, that's why we get a second update.
        for timeline_update in timeline_updates.iter().skip(1) {
            let event = assert_event_is_updated!(timeline_update, event_id, message_position);

            let reactions = event.content().reactions().cloned().unwrap_or_default();
            let reactions = reactions.get(&reaction_key).unwrap();
            assert_eq!(reactions.keys().count(), 1);

            let reaction = reactions.get(&user_id).unwrap();
            assert_matches!(reaction.status, ReactionStatus::RemoteToRemote(..));

            // Remote event should have a timestamp <= than now.
            // Note: this can actually be equal because if the timestamp from
            // server is not available, it might be created with a local call to `now()`
            assert!(reaction.timestamp <= MilliSecondsSinceUnixEpoch::now());
        }

        assert_pending!(stream);

        // Redact the reaction.
        timeline
            .toggle_reaction(&item_id, &reaction_key)
            .await
            .expect("toggling reaction the second time");

        sleep(Duration::from_secs(1)).await;

        assert_let!(Some(timeline_updates) = stream.next().await);
        assert_eq!(timeline_updates.len(), 1);

        // The reaction is removed.
        let event = assert_event_is_updated!(timeline_updates[0], event_id, message_position);
        assert!(event.content().reactions().cloned().unwrap_or_default().is_empty());

        assert_pending!(stream);
    }

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_stale_local_echo_time_abort_edit() {
    // Set up sync for user Alice, and create a room.
    let alice = TestClientBuilder::new("alice").use_sqlite().build().await.unwrap();

    let alice_clone = alice.clone();
    let alice_sync = spawn(async move {
        alice_clone.sync(Default::default()).await.expect("sync failed!");
    });

    debug!("Creating room…");
    let room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            is_direct: true,
        }))
        .await
        .unwrap();

    // Create a timeline for this room, filtering out all non-message items.
    let timeline = room.timeline().await.unwrap();
    let (items, mut stream) = timeline
        .subscribe_filter_map(|item| {
            item.as_event()
                .and_then(|item| item.content().as_message().is_some().then(|| item.clone()))
        })
        .await;

    assert!(items.is_empty());

    // Send message.
    debug!("Sending initial message…");
    timeline.send(RoomMessageEventContent::text_plain("hi!").into()).await.unwrap();

    // Receiving the local echo for the message.
    let mut vector_diff = timeout(Duration::from_secs(5), stream.next()).await.unwrap().unwrap();

    // The event cache may decide to clear the timeline because it was a limited
    // sync, so account for that.
    while let VectorDiff::Clear = &vector_diff {
        vector_diff = timeout(Duration::from_secs(5), stream.next()).await.unwrap().unwrap();
    }

    let local_echo = assert_matches!(vector_diff, VectorDiff::PushBack { value } => value);

    if !local_echo.is_local_echo() {
        // If the server raced and we've already received the remote echo, then this
        // test is meaningless, so short-circuit and leave it.
        return;
    }

    assert!(local_echo.is_editable());
    assert_matches!(local_echo.send_state(), Some(EventSendState::NotSentYet { progress: None }));
    assert_eq!(local_echo.content().as_message().unwrap().body(), "hi!");

    // It is then sent. The timeline stream can be racy here:
    //
    // - either the local echo is marked as sent *before*, and we receive an update
    //   for this before the remote echo.
    // - or the remote echo comes up faster.
    //
    // We collect all diffs but we no longer check them. This is tested by unit
    // tests already, and testing that here is a bit more complex before of the racy
    // situation.
    {
        let mut diffs = Vec::with_capacity(3);

        while let Ok(Some(vector_diff)) = timeout(Duration::from_secs(15), stream.next()).await {
            diffs.push(vector_diff);
        }

        assert!(diffs.len() >= 2);
    }

    // Now do a crime: try to edit the local echo.
    // The edit works on the local echo and applies to the remote echo \o/.
    timeline
        .edit(
            &local_echo.identifier(),
            EditedContent::RoomMessage(RoomMessageEventContent::text_plain("bonjour").into()),
        )
        .await
        .unwrap();

    let vector_diff = timeout(Duration::from_secs(5), stream.next()).await.unwrap().unwrap();
    let remote_echo = assert_matches!(vector_diff, VectorDiff::Set { index: 0, value } => value);
    assert!(!remote_echo.is_local_echo());
    assert!(remote_echo.is_editable());

    assert_eq!(remote_echo.content().as_message().unwrap().body(), "bonjour");

    alice_sync.abort();

    assert_pending!(stream);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_enabling_backups_retries_decryption() {
    let encryption_settings = EncryptionSettings {
        auto_enable_backups: true,
        backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
        ..Default::default()
    };
    let alice = TestClientBuilder::new("alice")
        .use_sqlite()
        .encryption_settings(encryption_settings)
        .build()
        .await
        .unwrap();

    alice.encryption().wait_for_e2ee_initialization_tasks().await;
    alice
        .encryption()
        .recovery()
        .enable()
        .with_passphrase("Bomber's code")
        .await
        .expect("We should be able to enable recovery");

    let user_id = alice.user_id().expect("We should have access to the user ID now");
    assert_eq!(
        alice.encryption().backups().state(),
        BackupState::Enabled,
        "The backup state should now be BackupState::Enabled"
    );

    // Set up sync for user Alice, and create a room.
    let alice_sync = spawn({
        let alice = alice.clone();
        async move {
            alice.sync(Default::default()).await.expect("sync failed!");
        }
    });

    debug!("Creating room…");

    let initial_state = vec![
        InitialStateEvent::with_empty_state_key(
            RoomEncryptionEventContent::with_recommended_defaults(),
        )
        .to_raw_any(),
    ];

    let room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            is_direct: true,
            initial_state,
            preset: Some(RoomPreset::PrivateChat)
        }))
        .await
        .unwrap();

    assert!(
        room.latest_encryption_state()
            .await
            .expect("We should be able to check that the room is encrypted")
            .is_encrypted()
    );

    let event_id = room
        .send(RoomMessageEventContent::text_plain("It's a secret to everybody!"))
        .await
        .expect("We should be able to send a message to our new room")
        .response
        .event_id;

    alice
        .encryption()
        .backups()
        .wait_for_steady_state()
        .await
        .expect("We should be able to wait for our room keys to be uploaded");

    // We don't need Alice anymore.
    alice_sync.abort();

    // I know that this is Alice again, but it's the Alice's second device which she
    // named Bob.
    let bob = TestClientBuilder::with_exact_username(user_id.localpart().to_owned())
        .use_sqlite()
        .encryption_settings(encryption_settings)
        .build()
        .await
        .unwrap();

    bob.encryption().wait_for_e2ee_initialization_tasks().await;
    assert!(!bob.encryption().backups().are_enabled().await, "Backups shouldn't be enabled yet");

    // Sync once to get access to the room.
    bob.sync_once(Default::default()).await.expect("Bob should be able to perform an initial sync");
    // Let Bob sync continuously now.
    let bob_sync = spawn({
        let bob = bob.clone();
        async move {
            bob.sync(Default::default()).await.expect("sync failed!");
        }
    });

    // Load the event into the timeline.
    let room = bob.get_room(room.room_id()).expect("We should have access to our room now");
    let timeline = room.timeline().await.expect("We should be able to get a timeline for our room");
    timeline
        .paginate_backwards(50)
        .await
        .expect("We should be able to paginate the timeline to fetch the history");

    // Wait for the event cache and the timeline to do their job.
    // Timeline triggers a pagination, that inserts events in the event cache, that
    // then broadcasts new events into the timeline. All this is async.
    sleep(Duration::from_millis(300)).await;

    let item =
        timeline.item_by_event_id(&event_id).await.expect("The event should be in the timeline");

    // The event is not decrypted yet.
    assert!(item.content().is_unable_to_decrypt());

    // We now connect to the backup which will not give us the room key right away,
    // we first need to encounter a UTD to attempt the download.
    bob.encryption()
        .recovery()
        .recover("Bomber's code")
        .await
        .expect("We should be able to recover from Bob's device");

    // Let's subscribe to our timeline so we don't miss the transition from UTD to
    // decrypted event.
    let (_, mut stream) = timeline
        .subscribe_filter_map(|item| {
            item.as_event().cloned().filter(|item| item.event_id() == Some(&event_id))
        })
        .await;

    let room_key_stream = bob.encryption().backups().room_keys_for_room_stream(room.room_id());
    pin_mut!(room_key_stream);

    // Wait for the room key to arrive before continuing.
    let wait_for_room_key = async {
        if let Some(update) = room_key_stream.next().await {
            let _update = update.expect(
            "We should not miss the update since we're only broadcasting a small amount of updates",
        );
        }
    };

    timeout(Duration::from_secs(5), wait_for_room_key)
        .await
        .expect("We should have downloaded the room key from the backup");

    // Alright, we should now receive an update that the event had been decrypted.
    let _vector_diff = timeout(Duration::from_secs(5), stream.next()).await.unwrap().unwrap();

    // Let's fetch the event again.
    let item =
        timeline.item_by_event_id(&event_id).await.expect("The event should be in the timeline");

    // Yup it's decrypted great.
    assert_let!(
        Some(message) = item.content().as_message(),
        "The event should have been decrypted now"
    );

    assert_eq!(message.body(), "It's a secret to everybody!");

    bob_sync.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_room_keys_received_on_notification_client_trigger_redecryption() {
    let alice = TestClientBuilder::new("alice").use_sqlite().build().await.unwrap();
    alice.encryption().wait_for_e2ee_initialization_tasks().await;

    // Set up sync for user Alice, and create a room.
    let alice_sync = spawn({
        let alice = alice.clone();
        async move {
            alice.sync(Default::default()).await.expect("sync failed!");
        }
    });

    debug!("Creating the room…");

    // The room needs to be encrypted.
    let initial_state = vec![
        InitialStateEvent::with_empty_state_key(
            RoomEncryptionEventContent::with_recommended_defaults(),
        )
        .to_raw_any(),
    ];

    let alice_room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            is_direct: true,
            initial_state,
            preset: Some(RoomPreset::PrivateChat)
        }))
        .await
        .unwrap();

    assert!(
        alice_room
            .latest_encryption_state()
            .await
            .expect("We should be able to check that the room is encrypted")
            .is_encrypted()
    );

    // Create stream listening for devices.
    let devices_stream = alice
        .encryption()
        .devices_stream()
        .await
        .expect("We should be able to listen to the devices stream");

    // Now here comes bob.
    let bob = TestClientBuilder::new("bob").use_sqlite().build().await.unwrap();
    bob.encryption().wait_for_e2ee_initialization_tasks().await;

    debug!("Inviting Bob.");

    alice_room
        .invite_user_by_id(bob.user_id().expect("We should have access to bob's user ID"))
        .await
        .expect("We should be able to invite Bob to the room");

    // Sync once to get access to the room.
    let sync_service = SyncService::builder(bob.clone()).build().await.expect("Wat");
    sync_service.start().await;

    let bob_rooms = sync_service
        .room_list_service()
        .all_rooms()
        .await
        .expect("We should be able to get the room");

    debug!("Waiting for the room list to load");
    let wait_for_room_list_load = async {
        while let Some(state) = bob_rooms.loading_state().next().await {
            if let RoomListLoadingState::Loaded { .. } = state {
                break;
            }
        }
    };

    timeout(Duration::from_secs(5), wait_for_room_list_load)
        .await
        .expect("We should be able to load the room list");

    // Bob joins the room.
    let bob_room =
        bob.get_room(alice_room.room_id()).expect("We should have access to the room now");
    bob_room.join().await.expect("Bob should be able to join the room");

    debug!("Bob joined the room");
    assert_eq!(bob_room.state(), RoomState::Joined);
    assert!(bob_room.latest_encryption_state().await.unwrap().is_encrypted());

    // Now we need to wait for Bob's device to turn up.
    let wait_for_bob_device = async {
        pin_mut!(devices_stream);

        while let Some(devices) = devices_stream.next().await {
            if devices.new.contains_key(bob.user_id().unwrap()) {
                break;
            }
        }
    };

    timeout(Duration::from_secs(5), wait_for_bob_device)
        .await
        .expect("We should be able to load the room list");

    // Let's stop the sync so we don't receive the room key using the usual channel.
    sync_service.stop().await;

    debug!("Alice sends the message");
    let event_id = alice_room
        .send(RoomMessageEventContent::text_plain("It's a secret to everybody!"))
        .await
        .expect("We should be able to send a message to our new room")
        .response
        .event_id;

    // We don't need Alice anymore.
    alice_sync.abort();

    // Let's get the timeline and backpaginate to load the event.
    let timeline =
        bob_room.timeline().await.expect("We should be able to get a timeline for our room");

    let mut item = None;

    for _ in 0..10 {
        {
            // Clear any previously received previous-batch token.
            let (room_event_cache, _drop_handles) = bob_room.event_cache().await.unwrap();
            room_event_cache.clear().await.unwrap();
        }

        timeline
            .paginate_backwards(50)
            .await
            .expect("We should be able to paginate the timeline to fetch the history");

        // Wait for the event cache and the timeline to do their job.
        // Timeline triggers a pagination, that inserts events in the event cache, that
        // then broadcasts new events into the timeline. All this is async.
        sleep(Duration::from_millis(300)).await;

        if let Some(timeline_item) = timeline.item_by_event_id(&event_id).await {
            item = Some(timeline_item);
            break;
        }

        sleep(Duration::from_millis(100)).await
    }

    let item = item.expect("The event should be in the timeline by now");

    // The event is not decrypted yet.
    assert!(item.content().is_unable_to_decrypt());

    // Let's subscribe to our timeline so we don't miss the transition from UTD to
    // decrypted event.
    let (_, mut stream) = timeline
        .subscribe_filter_map(|item| {
            item.as_event().cloned().filter(|item| item.event_id() == Some(&event_id))
        })
        .await;

    // Now we create a notification client.
    let notification_client = bob
        .notification_client("BOB_NOTIFICATION_CLIENT".to_owned())
        .await
        .expect("We should be able to build a notification client");

    // This syncs and fetches the room key.
    debug!("The notification client syncs");
    let notification_client = NotificationClient::new(
        notification_client,
        matrix_sdk_ui::notification_client::NotificationProcessSetup::SingleProcess {
            sync_service: sync_service.into(),
        },
    )
    .await
    .expect("We should be able to build a notification client");

    let _ = notification_client
        .get_notification(bob_room.room_id(), &event_id)
        .await
        .expect("We should be able toe get a notification item for the given event");

    // Alright, we should now receive an update that the event had been decrypted.
    let _vector_diff = timeout(Duration::from_secs(10), stream.next()).await.unwrap().unwrap();

    // Let's fetch the event again.
    let item =
        timeline.item_by_event_id(&event_id).await.expect("The event should be in the timeline");

    // Yup it's decrypted great.
    assert_let!(
        Some(message) = item.content().as_message(),
        "The event should have been decrypted now"
    );

    assert_eq!(message.body(), "It's a secret to everybody!");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "Flaky: times out waiting for timeline update that message was from a secure device"]
async fn test_new_users_first_messages_dont_warn_about_insecure_device_if_it_is_secure() {
    async fn timeline_messages(timeline: &Timeline) -> Vec<EventTimelineItem> {
        timeline
            .items()
            .await
            .iter()
            .filter_map(|item| item.as_event())
            .filter(|e| e.content().as_message().is_some())
            .cloned()
            .collect()
    }

    /// Send the supplied message in the supplied room
    async fn send_message(room: &Room, message: &str) -> OwnedEventId {
        room.send(RoomMessageEventContent::text_plain(message))
            .await
            .expect("We should be able to send a message to our new room")
            .response
            .event_id
    }

    /// Wait for a room with the supplied ID to appear, and then join it and
    /// confirm it is encrypted.
    async fn join_room(joiner: &Client, room_id: &RoomId) -> Room {
        let sync_service = SyncService::builder(joiner.clone())
            .build()
            .await
            .expect("should be able to create a SyncService");

        sync_service.start().await;

        let room_list = sync_service
            .room_list_service()
            .all_rooms()
            .await
            .expect("should be able to get the RoomList instance");

        let until_room_list_is_loaded = async {
            while let Some(state) = room_list.loading_state().next().await {
                if let RoomListLoadingState::Loaded { .. } = state {
                    break;
                }
            }
        };

        timeout(Duration::from_secs(5), until_room_list_is_loaded)
            .await
            .expect("the RoomList should finish loading within 5 seconds");

        let room = joiner.get_room(room_id).expect("room should be in the room list");
        room.join().await.expect("should be able to join the room");

        assert_eq!(room.state(), RoomState::Joined);
        assert!(room.latest_encryption_state().await.unwrap().is_encrypted());

        sync_service.stop().await;

        room
    }

    /// Invite the supplied `invitee` to the supplied room. `inviter_room`
    /// should be a [`Room`] instance created by the inviting user.
    async fn invite_to_room(inviter_room: &Room, invitee: &Client) {
        inviter_room
            .invite_user_by_id(invitee.user_id().expect("client should have an ID"))
            .await
            .expect("should not fail to invite user to room");
    }

    /// Create a room and ensure it is encrypted
    async fn create_encrypted_room(client: &Client) -> Room {
        let room = client
        .create_room(assign!(CreateRoomRequest::new(), {
            is_direct: true,
            initial_state: vec![
                InitialStateEvent::with_empty_state_key(RoomEncryptionEventContent::with_recommended_defaults()).to_raw_any()
            ],
            preset: Some(RoomPreset::PrivateChat)
        }))
        .await
        .expect("should not fail to create room");

        assert!(
            room.latest_encryption_state()
                .await
                .expect("should be able to check that the room is encrypted")
                .is_encrypted()
        );

        room
    }

    /// Hit the `keys/query` endpoint to find the latest user information about
    /// the supplied user ID.
    async fn fetch_user_identity(client: &Client, user_id: Option<&UserId>) {
        client
            .encryption()
            .request_user_identity(user_id.expect("user_id should not be None"))
            .await
            .expect("requesting user identity should not fail")
            .expect("should be able to see other user's identity");
    }

    /// Create a new [`Client`] that has e2ee tasks done and is cross-signed.
    async fn new_cross_signed_client(username: &str) -> Client {
        let client = new_client(username).await;
        cross_sign(&client).await;
        client
    }

    /// Create a new [`Client`] that is not yet cross-signed, but has e2ee
    /// initialization done.
    async fn new_client(username: &str) -> Client {
        let client = TestClientBuilder::new(username).use_sqlite().build().await.unwrap();
        client.encryption().wait_for_e2ee_initialization_tasks().await;
        client
    }

    /// Cross-sign this client's identity, so others will see its devices as
    /// verified.
    async fn cross_sign(client: &Client) {
        client
            .encryption()
            .bootstrap_cross_signing(None)
            .await
            .expect("should not fail to bootstrap cross signing for alice");
    }

    // Given two clients who are in an encrypted room, but alice has not yet
    // completed cross-signing.
    let alice = new_client("alice").await;
    let bob = new_cross_signed_client("bob").await;
    let room_for_alice = create_encrypted_room(&alice).await;
    invite_to_room(&room_for_alice, &bob).await;
    let room_for_bob = join_room(&bob, room_for_alice.room_id()).await;

    // We focus on the timeline that gives bob's view of the room
    let timeline = room_for_bob.timeline().await.expect("should be able to get a timeline");
    let (_, mut timeline_stream) = timeline.subscribe().await;

    // When alice sends a message in the room and bob syncs it
    let event_id = send_message(&room_for_alice, "secret message").await;
    // Wait for the event to appear
    while timeline.item_by_event_id(&event_id).await.is_none() {
        bob.sync_once(SyncSettings::new()).await.expect("should not fail to sync");
    }
    assert_next_with_timeout!(timeline_stream);

    {
        // Then the message is decrypted but it's not from a verified device
        let messages = timeline_messages(&timeline).await;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content().as_message().unwrap().body(), "secret message");
        assert_eq!(
            messages[0].encryption_info().unwrap().verification_state,
            VerificationState::Unverified(VerificationLevel::UnsignedDevice)
        );
    }

    // But when alice becomes cross-signed and bob finds out about it
    cross_sign(&alice).await;
    bob.sync_once(SyncSettings::new()).await.expect("should not fail to sync");

    // Sync again to make sure the server has notified us about the update to
    // alice's device info.
    bob.sync_once(SyncSettings::new().timeout(Duration::from_millis(2000)))
        .await
        .expect("should not fail to sync");

    fetch_user_identity(&bob, alice.user_id()).await;
    let update2 = assert_next_with_timeout!(timeline_stream);

    {
        // Then we updated the timeline to reflect the fact that the message is from a
        // verified device.
        let messages = timeline_messages(&timeline).await;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].content().as_message().unwrap().body(), "secret message");
        assert_eq!(
            messages[0].encryption_info().unwrap().verification_state,
            VerificationState::Unverified(VerificationLevel::UnverifiedIdentity)
        );
        // (Note: the device is verified, but the _identity_ is not. We're not
        // worried about that - it's "pinned".)
    }

    {
        // And the final update just changed the one item
        assert_eq!(update2.len(), 1);
        assert_let!(VectorDiff::Set { index, .. } = &update2[0]);
        assert_eq!(*index, 10);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_thread_focused_timeline() -> TestResult {
    // Set up sync for user Alice, and create a room.
    let alice = TestClientBuilder::new("alice").use_sqlite().build().await?;

    let alice_clone = alice.clone();
    let alice_sync = spawn(async move {
        alice_clone.sync(Default::default()).await.expect("sync failed for alice!");
    });

    // Set up sync for another user Bob.
    let bob = TestClientBuilder::new("bob").use_sqlite().build().await?;

    // Enable Bob's event cache, to speed up creating the in-thread reply event.
    bob.event_cache().subscribe().unwrap();

    let bob_clone = bob.clone();
    let bob_sync = spawn(async move {
        bob_clone.sync(Default::default()).await.expect("sync failed for bob!");
    });

    debug!("Creating room…");
    let alice_room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            is_direct: true,
        }))
        .await?;

    alice_room.invite_user_by_id(bob.user_id().unwrap()).await?;

    // Bob joins the room.

    let bob_room = loop {
        if let Some(room) = bob.get_room(alice_room.room_id()) {
            room.join().await?;
            break room;
        }
        sleep(Duration::from_millis(10)).await;
    };

    // Bob sends messages in a thread.
    let resp = bob_room.send(RoomMessageEventContent::text_plain("Root message")).await?.response;
    let thread_root = resp.event_id;

    let thread_reply_event_content = bob_room
        .make_reply_event(
            RoomMessageEventContentWithoutRelation::text_plain("First reply"),
            Reply {
                event_id: thread_root.clone(),
                enforce_thread: EnforceThread::Threaded(ReplyWithinThread::No),
            },
        )
        .await?;

    let thread_reply_event_id = bob_room.send(thread_reply_event_content).await?.response.event_id;

    // Alice creates a timeline focused on the in-thread event, so this will use
    // /context, and the thread root will be part of the response.
    let timeline = TimelineBuilder::new(&alice_room)
        .with_focus(TimelineFocus::Event {
            target: thread_reply_event_id.clone(),
            num_context_events: 42,
            thread_mode: TimelineEventFocusThreadMode::Automatic { hide_threaded_events: true },
        })
        .build()
        .await?;

    // The timeline should be focused on the thread reply, so it should be
    // considered threaded.
    assert!(timeline.is_threaded());

    // Observe that the timeline contains both the root and the reply.
    let (items, mut stream) = timeline.subscribe().await;

    assert_eq!(items.len(), 3);
    assert!(items[0].is_date_divider());
    assert_eq!(items[1].as_event().unwrap().event_id().unwrap(), thread_root);
    assert_eq!(items[2].as_event().unwrap().event_id().unwrap(), thread_reply_event_id);

    // If Alice paginates backwards, nothing happens on the timeline, as we've hit
    // the start in the /context response.
    let hit_start = timeline.paginate_backwards(10).await?;
    assert!(hit_start);

    sleep(Duration::from_millis(100)).await;
    assert_pending!(stream);

    alice_sync.abort();
    bob_sync.abort();

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_local_echo_to_send_event_has_encryption_info() -> TestResult {
    // Set up sync for user Alice, and create a room.
    let alice = TestClientBuilder::new("alice").use_sqlite().build().await?;

    debug!("Creating room…");
    let initial_state = vec![
        InitialStateEvent::with_empty_state_key(
            RoomEncryptionEventContent::with_recommended_defaults(),
        )
        .to_raw_any(),
    ];

    let room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            is_direct: true,
            initial_state,
            preset: Some(RoomPreset::PrivateChat)
        }))
        .await?;

    // Let's sync now so we can be sure that we have the correct room state.
    alice.sync_once(Default::default()).await?;

    // The room should be encrypted.
    assert!(room.latest_encryption_state().await?.is_encrypted());

    // Create a timeline for this room, filtering out all non-message items.
    let timeline = room.timeline().await.unwrap();
    let (items, mut stream) = timeline
        .subscribe_filter_map(|item| {
            item.as_event()
                .and_then(|item| item.content().as_message().is_some().then(|| item.clone()))
        })
        .await;
    assert!(items.is_empty());

    // Send message.
    debug!("Sending initial message…");
    timeline.send(RoomMessageEventContent::text_plain("It's a secret to everybody").into()).await?;

    // Receiving the local echo for the message.
    let vector_diff = timeout(Duration::from_secs(5), stream.next()).await?;
    let local_echo = assert_matches!(vector_diff, Some(VectorDiff::PushBack { value }) => value);

    // We're not sent yet.
    assert!(local_echo.is_editable());
    assert_matches!(local_echo.send_state(), Some(EventSendState::NotSentYet { progress: None }));
    assert_eq!(local_echo.content().as_message().unwrap().body(), "It's a secret to everybody");

    // Now we receive an update from the send queue that the event is in the sent
    // state.
    let vector_diff = timeout(Duration::from_secs(5), stream.next()).await?;
    let sent_event =
        assert_matches!(vector_diff, Some(VectorDiff::Set { index: 0, value }) => value);
    assert_matches!(sent_event.send_state(), Some(EventSendState::Sent { .. }));

    // Now the next update, since we're not syncing, should be the send queue
    // inserting the event into the event cache.

    // We first remove the old item.
    let vector_diff = timeout(Duration::from_secs(5), stream.next()).await?;
    assert_matches!(vector_diff, Some(VectorDiff::Remove { index: 0 }));

    // Now the new event with the encryption info arrives.
    let vector_diff = timeout(Duration::from_secs(5), stream.next()).await?;
    let sent_event = assert_matches!(vector_diff, Some(VectorDiff::PushFront {  value }) => value);

    // The encryption info should be correctly populated.
    let encryption_info =
        sent_event.encryption_info().expect("The event should have encryption info available");

    // Since we're the sender, we should be in the verified state.
    assert_eq!(encryption_info.verification_state, VerificationState::Verified);
    assert_pending!(stream);

    Ok(())
}

async fn prepare_room_with_pinned_events(
    alice: &Client,
    recovery_passphrase: &str,
    number_of_normal_events: usize,
) -> Result<(OwnedRoomId, OwnedEventId), TestError> {
    let sync_service = SyncService::builder(alice.clone()).build().await?;
    sync_service.start().await;

    alice.encryption().wait_for_e2ee_initialization_tasks().await;
    alice.encryption().recovery().enable().with_passphrase(recovery_passphrase).await?;

    debug!("Creating room…");
    let room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            is_direct: true,
            initial_state: vec![],
            preset: Some(RoomPreset::PrivateChat)
        }))
        .await?;

    room.enable_encryption().await?;

    // Send an event to the encrypted room and pin it.
    let room_id = room.room_id().to_owned();
    let result =
        room.send(RoomMessageEventContent::text_plain("It's a secret to everybody")).await?;
    let event_id = result.response.event_id;

    let timeline = room.timeline().await?;
    timeline.pin_event(&event_id).await?;

    // Now send a bunch of normal events, this ensures that our pinned event isn't
    // in the main timeline when we restore things.
    for i in 0..number_of_normal_events {
        room.send(RoomMessageEventContent::text_plain(format!("Normal event {i}"))).await?;
    }

    sync_service.stop().await;

    Ok((room_id, event_id))
}

async fn test_pinned_events_are_decrypted_after_recovering_with_event_count(
    event_count: usize,
) -> TestResult {
    const RECOVERY_PASSPHRASE: &str = "I am error";

    let encryption_settings = EncryptionSettings {
        auto_enable_cross_signing: true,
        auto_enable_backups: true,
        backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
    };

    // Set up sync for user Alice, and create a room.
    let alice = TestClientBuilder::new("alice")
        .encryption_settings(encryption_settings)
        .use_sqlite()
        .build()
        .await?;
    let user_id = alice.user_id().expect("We should have a user ID by now");

    let (room_id, event_id) =
        prepare_room_with_pinned_events(&alice, RECOVERY_PASSPHRASE, event_count).await?;

    // Now `another_alice` comes into play.
    let another_alice = TestClientBuilder::with_exact_username(user_id.localpart().to_owned())
        .encryption_settings(encryption_settings)
        .use_sqlite()
        .build()
        .await?;

    // Alright, we're done with the original Alice.
    drop(alice);

    // No rooms as of yet, we have not synced with the server as of yet.
    assert!(another_alice.rooms().is_empty());
    another_alice.event_cache().subscribe()?;

    let sync_service = SyncService::builder(another_alice.clone()).build().await?;
    // We need to subscribe to the room, otherwise we won't request the
    // `m.room.pinned_events` stat event.
    //
    // Additionally if we subscribe to the room after we already synced, we'll won't
    // receive the event, likely due to a Synapse bug.
    sync_service.room_list_service().subscribe_to_rooms(&[&room_id]).await;
    sync_service.start().await;
    another_alice.encryption().wait_for_e2ee_initialization_tasks().await;

    // Let's get the room.
    let room = wait_for_room(&another_alice, &room_id).await;

    assert!(room.latest_encryption_state().await?.is_encrypted(), "The room should be encrypted");

    // Let's see if the pinned event is there.
    let pinned_events = room.load_pinned_events().await?.unwrap_or_default();
    assert!(
        pinned_events.contains(&event_id),
        "The pinned event should be found in the pinned events state event"
    );

    // Let's see if the event is there, it should be a UTD.
    let event = room.event(&event_id, Default::default()).await?;
    assert!(event.kind.is_utd());

    // Alright, let's now get to the timeline with a PinnedEvents focus.
    let pinned_timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::PinnedEvents {
            max_events_to_load: 100,
            max_concurrent_requests: 10,
        })
        .build()
        .await?;

    let (items, mut stream) = pinned_timeline.subscribe_filter_map(|i| i.as_event().cloned()).await;

    // If we don't have any items as of yet, wait on the stream.
    if items.is_empty() {
        let _ = assert_next_with_timeout!(stream, 5000);
    }

    // Alright, let's get the event from the timeline.
    let item = pinned_timeline
        .item_by_event_id(&event_id)
        .await
        .expect("We should have access to the pinned event");

    // Still a UTD.
    assert!(
        item.content().is_unable_to_decrypt(),
        "The pinned event should be a UTD as we didn't recover yet"
    );
    assert_eq!(pinned_timeline.items().await.len(), 2);

    // Let's now recover.
    another_alice.encryption().recovery().recover(RECOVERY_PASSPHRASE).await?;
    assert_eq!(another_alice.encryption().recovery().state(), RecoveryState::Enabled);

    // The next update for the timeline should replace the UTD item with a decrypted
    // value.
    let next_item = assert_next_with_timeout!(stream, 5000);

    assert_let!(VectorDiff::Set { index: 0, value } = next_item);
    let content = value.content();

    // And we're not a UTD anymore.
    assert!(!content.is_unable_to_decrypt());
    let message = content.as_message().expect("The pinned event should be a message");
    assert_eq!(message.body(), "It's a secret to everybody");

    // And we check that we don't have any more items in the timeline, the UTD item
    // was indeed replaced.
    let items = pinned_timeline.items().await;
    assert_eq!(items.len(), 2);

    Ok(())
}

/// Test that pinned UTD events, once decrypted by R2D2 (the redecryptor), get
/// replaced in the timeline with the decrypted variant.
///
/// We do this by first creating the pinned events on one Client, called
/// `alice`. Then another client object is created, called `another_alice`.
/// `another_alice` initially doesn't have access to the room history.
///
/// Only once `another_alice` recovers things and gets access to the backup can
/// she download the room key to decrypt the pinned event.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_pinned_events_are_decrypted_after_recovering_with_event_in_timeline() -> TestResult {
    test_pinned_events_are_decrypted_after_recovering_with_event_count(0).await
}

/// Test that pinned UTD events, once decrypted by R2D2 (the redecryptor), get
/// replaced in the timeline with the decrypted variant even if the pinened UTD
/// event isn't part of the main timeline and thus wasn't put into the event
/// cache by the main timeline backpaginating.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// FIXME: This test is ignored because R2D2 can't decrypt this pinned event as it's never put into
// the event cache.
#[ignore]
async fn test_pinned_events_are_decrypted_after_recovering_with_event_not_in_timeline() -> TestResult
{
    test_pinned_events_are_decrypted_after_recovering_with_event_count(30).await
}

/// Test that UTDs in a timeline focused on a single event, once decrypted by
/// R2D2 (the redecryptor), get replaced in the timeline with the decrypted
/// variant even if the focused UTD event isn't part of the main timeline and
/// thus wasn't put into the event cache by the main timeline backpaginating.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
// FIXME: This test is ignored because R2D2 can't decrypt this event as
// it's never put into the event cache.
#[ignore]
async fn test_permalink_timelines_redecrypt() -> TestResult {
    const RECOVERY_PASSPHRASE: &str = "I am error";

    let encryption_settings = EncryptionSettings {
        auto_enable_cross_signing: true,
        auto_enable_backups: true,
        backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
    };

    // Set up sync for user Alice, and create a room.
    let alice = TestClientBuilder::new("alice")
        .encryption_settings(encryption_settings)
        .use_sqlite()
        .build()
        .await?;
    let user_id = alice.user_id().expect("We should have a user ID by now");

    // We can reuse the function for pinned events and the pinned event as our
    // targeted event.
    let (room_id, event_id) =
        prepare_room_with_pinned_events(&alice, RECOVERY_PASSPHRASE, 30).await?;

    // Now `another_alice` comes into play.
    let another_alice = TestClientBuilder::with_exact_username(user_id.localpart().to_owned())
        .encryption_settings(encryption_settings)
        .use_sqlite()
        .build()
        .await?;

    // Alright, we're done with the original Alice.
    drop(alice);

    // No rooms as of yet, we have not synced with the server as of yet.
    assert!(another_alice.rooms().is_empty());
    another_alice.event_cache().subscribe()?;

    let sync_service = SyncService::builder(another_alice.clone()).build().await?;
    // We need to subscribe to the room, otherwise we won't request the
    // `m.room.pinned_events` state event.
    //
    // Additionally if we subscribe to the room after we already synced, we won't
    // receive the event, likely due to a Synapse bug.
    sync_service.room_list_service().subscribe_to_rooms(&[&room_id]).await;
    sync_service.start().await;
    another_alice.encryption().wait_for_e2ee_initialization_tasks().await;

    // Let's get the room.
    let room = wait_for_room(&another_alice, &room_id).await;

    assert!(room.latest_encryption_state().await?.is_encrypted(), "The room should be encrypted");

    // Let's see if the event is there, it should be a UTD.
    let event = room.event(&event_id, Default::default()).await?;
    assert!(event.kind.is_utd());

    // Alright, let's now go to the timeline with an Event focus.
    let permalink_timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Event {
            target: event_id.to_owned(),
            num_context_events: 1,
            thread_mode: TimelineEventFocusThreadMode::Automatic { hide_threaded_events: true },
        })
        .build()
        .await?;

    let (items, mut stream) =
        permalink_timeline.subscribe_filter_map(|i| i.as_event().cloned()).await;

    // If we don't have any items as of yet, wait on the stream.
    if items.is_empty() {
        let _ = assert_next_with_timeout!(stream, 5000);
    }

    // Alright, let's get the event from the timeline.
    let item = permalink_timeline
        .item_by_event_id(&event_id)
        .await
        .expect("We should have access to the focused event");

    // Still a UTD.
    assert!(
        item.content().is_unable_to_decrypt(),
        "The focused event should be a UTD as we didn't recover yet"
    );
    assert_eq!(permalink_timeline.items().await.len(), 3);

    // Let's now recover.
    another_alice.encryption().recovery().recover(RECOVERY_PASSPHRASE).await?;
    assert_eq!(another_alice.encryption().recovery().state(), RecoveryState::Enabled);

    // The next update for the timeline should replace the UTD item with a decrypted
    // value.
    let next_item = assert_next_with_timeout!(stream, 5000);

    assert_let!(VectorDiff::Set { index: 0, value } = next_item);
    let content = value.content();

    // And we're not a UTD anymore.
    assert!(!content.is_unable_to_decrypt());
    let message = content.as_message().expect("The focused event should be a message");
    assert_eq!(message.body(), "It's a secret to everybody");

    // And we check that we don't have any more items in the timeline, the UTD item
    // was indeed replaced.
    let items = permalink_timeline.items().await;
    assert_eq!(items.len(), 3);

    Ok(())
}
