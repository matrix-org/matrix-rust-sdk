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
    assert_next_with_timeout,
    config::SyncSettings,
    deserialized_responses::{VerificationLevel, VerificationState},
    encryption::{backups::BackupState, EncryptionSettings},
    room::edit::EditedContent,
    ruma::{
        api::client::room::create_room::v3::{Request as CreateRoomRequest, RoomPreset},
        events::{
            room::{encryption::RoomEncryptionEventContent, message::RoomMessageEventContent},
            InitialStateEvent,
        },
        MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId, UserId,
    },
    Client, Room, RoomState,
};
use matrix_sdk_ui::{
    notification_client::NotificationClient,
    room_list_service::RoomListLoadingState,
    sync_service::SyncService,
    timeline::{EventSendState, EventTimelineItem, ReactionStatus, RoomExt, TimelineItem},
    Timeline,
};
use similar_asserts::assert_eq;
use stream_assert::assert_pending;
use tokio::{
    spawn,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tracing::{debug, trace, warn};

use crate::helpers::TestClientBuilder;

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
        assert_eq!(timeline_updates.len(), 2);

        // Local echo is added.
        {
            let event = assert_event_is_updated!(timeline_updates[0], event_id, message_position);
            let reactions = event.content().reactions();
            let reactions = reactions.get(&reaction_key).unwrap();
            let reaction = reactions.get(&user_id).unwrap();
            assert_matches!(reaction.status, ReactionStatus::LocalToRemote(..));
        }

        // Remote echo is added.
        {
            let event = assert_event_is_updated!(timeline_updates[1], event_id, message_position);

            let reactions = event.content().reactions();
            let reactions = reactions.get(&reaction_key).unwrap();
            assert_eq!(reactions.keys().count(), 1);

            let reaction = reactions.get(&user_id).unwrap();
            assert_matches!(reaction.status, ReactionStatus::RemoteToRemote(..));

            // Remote event should have a timestamp <= than now.
            // Note: this can actually be equal because if the timestamp from
            // server is not available, it might be created with a local call to `now()`
            assert!(reaction.timestamp <= MilliSecondsSinceUnixEpoch::now());
            assert!(stream.next().now_or_never().is_none());
        }

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
        assert!(event.content().reactions().is_empty());

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
    assert_matches!(local_echo.send_state(), Some(EventSendState::NotSentYet));
    assert_eq!(local_echo.content().as_message().unwrap().body(), "hi!");

    let mut has_sender_profile = local_echo.sender_profile().is_ready();

    // It is then sent. The timeline stream can be racy here:
    //
    // - either the local echo is marked as sent *before*, and we receive an update
    //   for this before the remote echo.
    // - or the remote echo comes up faster.
    //
    // Handle both orderings.
    {
        let mut diffs = Vec::with_capacity(3);

        while let Ok(Some(vector_diff)) = timeout(Duration::from_secs(15), stream.next()).await {
            diffs.push(vector_diff);
        }

        trace!(?diffs, "Received diffs");

        assert!(diffs.len() >= 2);

        for diff in diffs {
            match diff {
                VectorDiff::Set { index: 0, value: event }
                | VectorDiff::PushBack { value: event }
                | VectorDiff::Insert { index: 0, value: event } => {
                    if event.is_local_echo() {
                        // If the sender profile wasn't available, we may receive an update about
                        // it; ignore it.
                        if !has_sender_profile && event.sender_profile().is_ready() {
                            has_sender_profile = true;
                            continue;
                        }

                        assert_matches!(event.send_state(), Some(EventSendState::Sent { .. }));
                    }

                    assert!(event.is_editable());
                    assert_eq!(event.content().as_message().unwrap().body(), "hi!");
                }

                VectorDiff::Remove { index } => assert_eq!(index, 0),

                diff => {
                    panic!("unexpected diff: {diff:?}");
                }
            }
        }
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
        backup_download_strategy:
            matrix_sdk::encryption::BackupDownloadStrategy::AfterDecryptionFailure,
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

    let initial_state =
        vec![InitialStateEvent::new(RoomEncryptionEventContent::with_recommended_defaults())
            .to_raw_any()];

    let room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            is_direct: true,
            initial_state,
            preset: Some(RoomPreset::PrivateChat)
        }))
        .await
        .unwrap();

    assert!(room
        .latest_encryption_state()
        .await
        .expect("We should be able to check that the room is encrypted")
        .is_encrypted());

    let event_id = room
        .send(RoomMessageEventContent::text_plain("It's a secret to everybody!"))
        .await
        .expect("We should be able to send a message to our new room")
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
    if let Some(update) = room_key_stream.next().await {
        let _update = update.expect(
            "We should not miss the update since we're only broadcasting a small amount of updates",
        );
    }

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
    let initial_state =
        vec![InitialStateEvent::new(RoomEncryptionEventContent::with_recommended_defaults())
            .to_raw_any()];

    let alice_room = alice
        .create_room(assign!(CreateRoomRequest::new(), {
            is_direct: true,
            initial_state,
            preset: Some(RoomPreset::PrivateChat)
        }))
        .await
        .unwrap();

    assert!(alice_room
        .latest_encryption_state()
        .await
        .expect("We should be able to check that the room is encrypted")
        .is_encrypted());

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
                InitialStateEvent::new(RoomEncryptionEventContent::with_recommended_defaults()).to_raw_any()
            ],
            preset: Some(RoomPreset::PrivateChat)
        }))
        .await
        .expect("should not fail to create room");

        assert!(room
            .latest_encryption_state()
            .await
            .expect("should be able to check that the room is encrypted")
            .is_encrypted());

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
