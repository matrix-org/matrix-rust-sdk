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
    encryption::{backups::BackupState, EncryptionSettings},
    room::edit::EditedContent,
    ruma::{
        api::client::room::create_room::v3::Request as CreateRoomRequest,
        events::{
            room::{encryption::RoomEncryptionEventContent, message::RoomMessageEventContent},
            InitialStateEvent,
        },
        MilliSecondsSinceUnixEpoch,
    },
};
use matrix_sdk_ui::timeline::{
    EventSendState, ReactionStatus, RoomExt, TimelineItem, TimelineItemContent,
};
use similar_asserts::assert_eq;
use tokio::{
    spawn,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tracing::{debug, warn};

use crate::helpers::TestClientBuilder;

/// Checks that there a timeline update, and returns the EventTimelineItem.
///
/// A macro to help lowering compile times and getting better error locations.
macro_rules! assert_event_is_updated {
    ($stream:expr, $event_id:expr, $index:expr) => {{
        assert_let!(
            Ok(Some(VectorDiff::Set { index: i, value: event })) =
                timeout(Duration::from_secs(1), $stream.next()).await
        );
        assert_eq!(i, $index, "unexpected position for event update, value = {event:?}");

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
                    event
                        .event_id()
                        .map(|event_id| (item.unique_id().to_owned(), event_id.to_owned()))
                } else {
                    None
                }
            })
        };

        if let Some(pair) = find_event_id(&items) {
            return Ok(pair);
        }

        warn!(?items, "Waiting for updates…");

        while let Some(diff) = stream.next().await {
            warn!(?diff, "received a diff");
            diff.apply(&mut items);
            if let Some(pair) = find_event_id(&items) {
                return Ok(pair);
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

    let (msg_uid, event_id) = timeout(Duration::from_secs(10), event_id_task)
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
    while let Some(Some(diff)) = stream.next().now_or_never() {
        diff.apply(&mut items);
    }

    let message_position = items
        .iter()
        .enumerate()
        .find_map(|(i, item)| (item.as_event()?.event_id()? == event_id).then_some(i))
        .expect("couldn't find the final position for the event id");

    let reaction_key = "👍".to_owned();

    // Toggle reaction multiple times.
    for _ in 0..3 {
        debug!("Starting the toggle reaction tests…");

        // Add the reaction.
        timeline.toggle_reaction(&msg_uid, &reaction_key).await.expect("toggling reaction");

        // Local echo is added.
        {
            let event = assert_event_is_updated!(stream, event_id, message_position);
            let reactions = event.reactions().get(&reaction_key).unwrap();
            let reaction = reactions.get(&user_id).unwrap();
            assert_matches!(reaction.status, ReactionStatus::LocalToRemote(..));
        }

        // Remote echo is added.
        {
            let event = assert_event_is_updated!(stream, event_id, message_position);

            let reactions = event.reactions().get(&reaction_key).unwrap();
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
            .toggle_reaction(&msg_uid, &reaction_key)
            .await
            .expect("toggling reaction the second time");

        // The reaction is removed.
        let event = assert_event_is_updated!(stream, event_id, message_position);
        assert!(event.reactions().is_empty());

        assert!(stream.next().now_or_never().is_none());
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
    while let Ok(Some(vector_diff)) = timeout(Duration::from_secs(3), stream.next()).await {
        let VectorDiff::Set { index: 0, value: echo } = vector_diff else {
            panic!("unexpected diff: {vector_diff:#?}");
        };

        if echo.is_local_echo() {
            // If the sender profile wasn't available, we may receive an update about it;
            // ignore it.
            if !has_sender_profile && echo.sender_profile().is_ready() {
                has_sender_profile = true;
                continue;
            }
            assert_matches!(echo.send_state(), Some(EventSendState::Sent { .. }));
        }
        assert!(echo.is_editable());
        assert_eq!(echo.content().as_message().unwrap().body(), "hi!");
    }

    // Now do a crime: try to edit the local echo.
    let did_edit = timeline
        .edit(
            &local_echo,
            EditedContent::RoomMessage(RoomMessageEventContent::text_plain("bonjour").into()),
        )
        .await
        .unwrap();

    // The edit works on the local echo and applies to the remote echo \o/.
    assert!(did_edit);

    let vector_diff = timeout(Duration::from_secs(5), stream.next()).await.unwrap().unwrap();
    let remote_echo = assert_matches!(vector_diff, VectorDiff::Set { index: 0, value } => value);
    assert!(!remote_echo.is_local_echo());
    assert!(remote_echo.is_editable());

    assert_eq!(remote_echo.content().as_message().unwrap().body(), "bonjour");

    alice_sync.abort();
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
            preset: Some(matrix_sdk::ruma::api::client::room::create_room::v3::RoomPreset::PrivateChat)
        }))
        .await
        .unwrap();

    assert!(room
        .is_encrypted()
        .await
        .expect("We should be able to check that the room is encrypted"));

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

    let item =
        timeline.item_by_event_id(&event_id).await.expect("The event should be in the timeline");

    // The event is not decrypted yet.
    assert_matches!(item.content(), TimelineItemContent::UnableToDecrypt(_));

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
        TimelineItemContent::Message(message) = item.content(),
        "The event should have been decrypted now"
    );

    assert_eq!(message.body(), "It's a secret to everybody!");

    bob_sync.abort();
}
