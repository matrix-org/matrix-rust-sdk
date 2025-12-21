use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Result;
use assign::assign;
use futures::Future;
use matrix_sdk::{
    Client, Room,
    encryption::EncryptionSettings,
    ruma::{
        EventEncryptionAlgorithm, OwnedEventId, OwnedRoomId, RoomId,
        api::client::room::create_room::v3::Request as CreateRoomRequest,
        events::{
            AnyMessageLikeEventContent, AnySyncTimelineEvent, OriginalSyncMessageLikeEvent,
            room::{
                encrypted::{OriginalSyncRoomEncryptedEvent, RoomEncryptedEventContent},
                encryption::RoomEncryptionEventContent,
                message::{MessageType, OriginalSyncRoomMessageEvent, RoomMessageEventContent},
            },
            room_key::ToDeviceRoomKeyEvent,
        },
        serde::Raw,
    },
};
use matrix_sdk_ui::{
    notification_client::{
        NotificationClient, NotificationEvent, NotificationProcessSetup, NotificationStatus,
    },
    sync_service::SyncService,
};
use serde_json::json;
use tempfile::tempdir;
use tracing::{Level, info, instrument, span};

use crate::helpers::{SyncTokenAwareClient, TestClientBuilder};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "Flakes by timing out sometimes"]
async fn test_multiple_clients_share_crypto_state() -> Result<()> {
    // This failed before https://github.com/matrix-org/matrix-rust-sdk/pull/3338
    // because, even though the cross-process lock was working, the SessionStore
    // inside the crypto store was caching the Olm sessions, meaning we didn't
    // actually get the new one even when we called regenerate_olm.

    // Given two alice clients with the same DB and user ID:
    // alice_main is a normal client, and alice_nse is a NotificationClient
    let alice_sqlite_dir = tempdir()?;
    let alice_main =
        ClientWrapper::new("alice", Some(alice_sqlite_dir.path()), Some("alice_main".to_owned()))
            .await;
    let alice_nse =
        NotificationClientWrapper::notification_duplicate_of(&alice_main, alice_sqlite_dir.path())
            .await;
    // Sanity: alice's clients have the same device_id
    assert_eq!(alice_main.client.device_id(), alice_nse.inner_client.device_id());

    // And given a normal client for bob
    let bob = ClientWrapper::new("bob", None, None).await;

    info!("alice's device: {}", alice_main.client.device_id().unwrap());
    info!("bob's device: {}", bob.client.device_id().unwrap());

    // And given they are both in an encrypted room together
    let room_id = create_encrypted_room(&alice_main, &bob).await;

    // And given both alices have an Olm session with bob (because they received a
    // message from him)
    {
        let _span = span!(Level::INFO, "msg1_from_bob").entered();

        let msg1 = bob.send(&room_id, "msg1_from_bob").await;
        alice_nse.nse_wait_until_received(&room_id, &msg1).await;
        alice_main.wait_until_received(&msg1).await;

        info!("alice_nse received msg1 from bob");
    }

    // When the alice_main process sends a message
    {
        let _span = span!(Level::INFO, "msg2_from_alice").entered();

        let msg2 = alice_main.send(&room_id, "msg2_from_alice").await;
        bob.wait_until_received(&msg2).await;

        info!("bob received msg2 from alice_main");
    }

    // Then the NSE process can still receive messages from bob.
    // (This means that the NSE process must have notice that its Olm machine was
    // out-of-date and refreshed it from the DB.)
    {
        let _span = span!(Level::INFO, "msg3_from_bob").entered();

        let msg3 = bob.send(&room_id, "msg3_from_bob").await;
        alice_nse.nse_wait_until_received(&room_id, &msg3).await;

        info!("alice_nse received msg3 from bob");
    }

    Ok(())
}

/// A wrapper around a SyncTokenAwareClient that sets up a SyncService and
/// decrypts events if their keys arrive late.
///
/// Received, decrypted events are stored in the `events` Vec.
struct ClientWrapper {
    /// The client we are wrapping.
    pub client: SyncTokenAwareClient,

    /// A sync service that is started whenever we wait_until_... something.
    sync_service: SyncService,

    /// The received events and their plain text content.
    events: Arc<Mutex<Vec<(OwnedEventId, String)>>>,
}

type EncryptedEventInfo = (Raw<OriginalSyncRoomEncryptedEvent>, OwnedRoomId);

impl ClientWrapper {
    /// Create a new ClientWrapper.
    ///
    /// If sqlite_dir contains a path, that path is used for the sqlite DB.
    /// Otherwise, a random path is used.
    ///
    /// The contained SyncService always has a cross-process lock. If
    /// `cross_process_store_locks_holder_name` is supplied, it is used to
    /// identify this client's process. If not, the default name is used.
    async fn new(
        username: &str,
        sqlite_dir: Option<&Path>,
        cross_process_store_locks_holder_name: Option<String>,
    ) -> Self {
        let mut builder = TestClientBuilder::new(username);

        if let Some(holder_name) = cross_process_store_locks_holder_name {
            builder = builder.cross_process_store_locks_holder_name(holder_name);
        }

        let builder = if let Some(sqlite_dir) = sqlite_dir {
            builder.use_sqlite_dir(sqlite_dir)
        } else {
            builder.use_sqlite()
        };

        let events = Arc::new(Mutex::new(Vec::new()));
        let encrypted_events: Arc<Mutex<Vec<EncryptedEventInfo>>> =
            Arc::new(Mutex::new(Vec::new()));

        let inner_client = builder
            .encryption_settings(encryption_settings())
            .build()
            .await
            .expect("Failed to create client");

        let client = SyncTokenAwareClient::new(inner_client.clone());

        let sync_service = SyncService::builder(inner_client)
            .with_cross_process_lock()
            .build()
            .await
            .expect("Failed to create sync service");

        // Collect decrypted messages as they arrive and put them in events
        let events_clone = events.clone();
        client.add_event_handler(|ev: OriginalSyncRoomMessageEvent, _: Client| async move {
            let content = match ev.content.msgtype {
                MessageType::Text(text) => text.body,
                _ => panic!("Unexpected message type"),
            };

            events_clone.lock().unwrap().push((ev.event_id.clone(), content))
        });

        // Collect encrypted events and put them in encrypted_events
        let encrypted_events_clone = encrypted_events.clone();
        client.add_event_handler(
            |ev: Raw<OriginalSyncRoomEncryptedEvent>, room: Room| async move {
                encrypted_events_clone.lock().unwrap().push((ev, room.room_id().to_owned()));
            },
        );

        // Notice incoming keys and attempt to decrypt stored encrypted events
        let encrypted_events_clone2 = encrypted_events.clone();
        let events_clone2 = events.clone();
        client.add_event_handler(|_ev: ToDeviceRoomKeyEvent, client: Client| async move {
            // Whenever we received any room key, attempt to decrypt all existing encrypted
            // events. This could be more efficient, but it does the job.
            let evts = encrypted_events_clone2.lock().unwrap().clone();
            for (event, room_id) in evts.iter() {
                if let Some((event_id, content)) = decrypt_event(&client, room_id, event).await {
                    // If we did decrypt an event, remember it in our list of events we've seen
                    events_clone2.lock().unwrap().push((event_id, content));
                }
            }
        });

        Self { client, sync_service, events }
    }

    /// Create an encrypted room and invite the supplied people.
    async fn create_room(&self, invite: &[&ClientWrapper]) -> OwnedRoomId {
        let invite = invite.iter().map(|cw| cw.client.user_id().unwrap().to_owned()).collect();

        let request = assign!(CreateRoomRequest::new(), {
            invite,
            is_direct: true,
        });

        let room = self.client.create_room(request).await.expect("Failed to create room");
        self.enable_encryption(&room, 1).await;
        room.room_id().to_owned()
    }

    /// Send an m.room.encryption event in the supplied room to make it
    /// encrypted.
    async fn enable_encryption(&self, room: &Room, rotation_period_msgs: usize) {
        // Adapted from crates/matrix-sdk/src/room/mod.rs enable_encryption
        if !room.latest_encryption_state().await.expect("Failed to check encrypted").is_encrypted()
        {
            let content: RoomEncryptionEventContent = serde_json::from_value(json!({
                "algorithm": EventEncryptionAlgorithm::MegolmV1AesSha2,
                "rotation_period_msgs": rotation_period_msgs,
            }))
            .expect("Failed parsing encryption JSON");

            room.send_state_event(content).await.expect("Failed to send state event");

            // Give the sync loop time to run, to be fairly sure the encryption event is
            // received
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Join the room with the supplied ID.
    async fn join(&self, room_id: &RoomId) {
        let room = self.wait_until_room_exists(room_id).await;
        room.join().await.expect("Unable to join room")
    }

    /// Send a text message in the supplied room and return the event ID and
    /// contents.
    async fn send(&self, room_id: &RoomId, message: &str) -> (OwnedEventId, String) {
        let room = self.wait_until_room_exists(room_id).await;

        (
            room.send(RoomMessageEventContent::text_plain(message.to_owned()))
                .await
                .expect("Sending message failed")
                .response
                .event_id,
            message.to_owned(),
        )
    }

    /// Return true if the room with the supplied ID is encrypted.
    async fn room_is_encrypted(&self, room_id: &RoomId) -> bool {
        self.wait_until_room_exists(room_id)
            .await
            .latest_encryption_state()
            .await
            .expect("Failed to check encrypted")
            .is_encrypted()
    }

    /// Wait (syncing if needed) until the room with supplied ID exists, or time
    /// out.
    async fn wait_until_room_exists(&self, room_id: &RoomId) -> Room {
        self.sync_until(|| async { self.client.get_room(room_id) })
            .await
            .unwrap_or_else(|| panic!("Timed out waiting for room {room_id} to exist"))
    }

    /// Wait (syncing if needed) until the user appears in the supplied room, or
    /// time out.
    async fn wait_until_user_in_room(&self, room_id: &RoomId, other: &ClientWrapper) {
        let room = self.wait_until_room_exists(room_id).await;
        let user_id = other.client.user_id().unwrap();

        self.sync_until(|| async {
            room.get_member_no_sync(user_id).await.expect("get_member failed")
        })
        .await
        .unwrap_or_else(|| panic!("Timed out waiting for user {user_id} to be in room {room_id}"));
    }

    /// Wait (syncing if needed) until the event with this ID appears, or time
    /// out.
    #[instrument(skip(self))]
    async fn wait_until_received(&self, event_info: &(OwnedEventId, String)) {
        self.sync_until(|| async {
            self.events.lock().unwrap().contains(event_info).then_some(())
        })
        .await
        .unwrap_or_else(|| {
            panic!(
                "Timed out waiting for event ({}, {}) to be received. Events: {:?}",
                event_info.0,
                event_info.1,
                self.events.lock().unwrap()
            )
        });
    }

    /// Start syncing and then wait until the supplied function returns Some, or
    /// time out.
    ///
    /// Returns the returned value, or None if we timed out.
    async fn sync_until<F, R, S>(&self, f: F) -> Option<S>
    where
        F: Fn() -> R,
        R: Future<Output = Option<S>>,
    {
        self.sync_service.start().await;

        // Repeatedly call f until it returns Some
        let end_time = Instant::now() + timeout();
        while Instant::now() < end_time {
            if let Some(ans) = f().await {
                // We found what we were looking for
                self.sync_service.stop().await;
                return Some(ans);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        // We timed out
        self.sync_service.stop().await;
        None
    }
}

/// A wrapper around a NotificationClient that can wait until a given event has
/// been received and decrypted.
///
/// Received, decrypted events are stored in the `events` Vec.
struct NotificationClientWrapper {
    /// The raw client we are wrapping.
    pub inner_client: Client,

    /// The NotificationClient we are wrapping.
    pub notif_client: NotificationClient,

    /// The received events and their plain text content.
    events: Arc<Mutex<Vec<(OwnedEventId, String)>>>,
}

impl NotificationClientWrapper {
    /// Create a NotificationClientWrapper whose Client is a duplicate of
    /// `other`'s. This means it has the same user ID and device ID because
    /// it is created using [`Client::restore_session`].
    async fn notification_duplicate_of(other: &ClientWrapper, sqlite_dir: &Path) -> Self {
        let events = Arc::new(Mutex::new(Vec::new()));

        let inner_client = TestClientBuilder::new(other.client.user_id().unwrap())
            .use_sqlite()
            .encryption_settings(encryption_settings())
            .use_sqlite_dir(sqlite_dir)
            .duplicate(&other.client)
            .await
            .expect("Failed to duplicate client");

        let notif_client = NotificationClient::new(
            inner_client.clone(),
            NotificationProcessSetup::MultipleProcesses,
        )
        .await
        .expect("Failed to build NotificationClient");

        Self { inner_client, notif_client, events }
    }

    /// Wait (using [`NotificationClient::get_notification`]) until the event
    /// with this ID appears, or time out.
    #[instrument(skip(self))]
    async fn nse_wait_until_received(&self, room_id: &RoomId, event_info: &(OwnedEventId, String)) {
        if self.events.lock().unwrap().contains(event_info) {
            // The event is already here - we are done.
            return;
        }

        // Wait until this event can be got via get_notification
        let end_time = Instant::now() + timeout();
        while Instant::now() < end_time {
            let item = self
                .notif_client
                .get_notification(room_id, &event_info.0)
                .await
                .expect("Failed to get_notification");

            if let NotificationStatus::Event(item) = item
                && let NotificationEvent::Timeline(sync_timeline_event) = item.event
                && let AnySyncTimelineEvent::MessageLike(event) = *sync_timeline_event
                && let AnyMessageLikeEventContent::RoomMessage(event_content) =
                    event.original_content().expect("Empty original content")
            {
                self.events
                    .lock()
                    .unwrap()
                    .push((event_info.0.clone(), event_content.body().to_owned()));
                return;
            }
        }

        panic!(
            "Timed out waiting for event ({}, {}) to be received via NotificationClient. Events: {:?}",
            event_info.0,
            event_info.1,
            self.events.lock().unwrap()
        );
    }
}

/// Attempt to decrypt an event and return its ID and text body if we succeed.
/// Otherwise, return None.
///
/// Only works for room messages that contain text - anything else is ignored
/// and we return None.
async fn decrypt_event(
    client: &Client,
    room_id: &RoomId,
    event: &Raw<OriginalSyncMessageLikeEvent<RoomEncryptedEventContent>>,
) -> Option<(OwnedEventId, String)> {
    let room = client.get_room(room_id).unwrap();
    let push_ctx = room.push_context().await.unwrap();
    let Ok(decrypted) = room.decrypt_event(event, push_ctx.as_ref()).await else {
        return None;
    };

    let Ok(deserialized) = decrypted.raw().deserialize() else { return None };

    let AnySyncTimelineEvent::MessageLike(message) = &deserialized else { return None };

    let Some(AnyMessageLikeEventContent::RoomMessage(content)) = message.original_content() else {
        return None;
    };

    let MessageType::Text(text) = content.msgtype else { return None };

    let event_id = deserialized.event_id().to_owned();
    info!("Successfully decrypted event {event_id}");

    Some((event_id, text.body))
}

/// The standard settings for an encrypted room.
fn encryption_settings() -> EncryptionSettings {
    EncryptionSettings { auto_enable_cross_signing: true, ..Default::default() }
}

/// How long to wait before giving up on an operation.
fn timeout() -> Duration {
    Duration::from_secs(10)
}

/// Create an encrypted room as `alice_main` and join `bob` to it.
async fn create_encrypted_room(alice_main: &ClientWrapper, bob: &ClientWrapper) -> OwnedRoomId {
    let room_id = alice_main.create_room(&[bob]).await;

    info!("alice_main has created and enabled encryption in the room");

    bob.join(&room_id).await;

    alice_main.wait_until_user_in_room(&room_id, bob).await;

    // Sanity: the room is encrypted
    assert!(bob.room_is_encrypted(&room_id).await);
    assert!(alice_main.room_is_encrypted(&room_id).await);

    info!("alice_main and bob are both aware of each other in the e2ee room");
    room_id
}
