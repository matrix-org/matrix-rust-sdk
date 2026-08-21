use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error as _,
    io,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use assert_matches2::assert_matches;
use matrix_sdk::{
    BoxFuture, Error, MemoryStore, StateStore,
    config::{RequestConfig, StoreConfig, SyncSettings, SyncToken},
    deserialized_responses::RawSyncOrStrippedState,
    store::{StateStoreDataKey, StateStoreDataValue},
    sync::{SyncResponseHook, SyncResponseHookError},
    test_utils::mocks::{AnyRoomBuilder, MatrixMockServer},
};
use matrix_sdk_base::{RoomMemberships, sync::RoomUpdates};
use matrix_sdk_common::cross_process_lock::CrossProcessLockConfig;
use matrix_sdk_test::{
    InvitedRoomBuilder, JoinedRoomBuilder, KnockedRoomBuilder, SyncResponseBuilder, async_test,
    event_factory::EventFactory, stripped_state_event,
};
use ruma::{
    EventEncryptionAlgorithm, Int, MilliSecondsSinceUnixEpoch, RoomVersionId,
    api::client::sync::sync_events,
    event_id,
    events::{
        AnyStrippedStateEvent, AnySyncStateEvent, SyncStateEvent,
        room::{
            avatar::RoomAvatarEventContent,
            canonical_alias::RoomCanonicalAliasEventContent,
            create::RoomCreateEventContent,
            encryption::RoomEncryptionEventContent,
            guest_access::{GuestAccess, RoomGuestAccessEventContent},
            history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
            join_rules::RoomJoinRulesEventContent,
            member::RoomMemberEvent,
            name::{RoomNameEventContent, StrippedRoomNameEvent},
            pinned_events::RoomPinnedEventsEventContent,
            power_levels::RoomPowerLevelsEventContent,
            tombstone::RoomTombstoneEventContent,
            topic::RoomTopicEventContent,
        },
    },
    mxc_uri, owned_user_id,
    room::JoinRule,
    room_alias_id, room_id,
    serde::Raw,
    user_id,
};
use serde_json::json;
use stream_assert::{assert_pending, assert_ready};
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{method, path_regex},
};

#[derive(Debug)]
struct RecordingSyncResponseHook {
    store: Arc<MemoryStore>,
    calls: AtomicUsize,
    fail_next: AtomicBool,
    journal: Mutex<BTreeSet<String>>,
    observed_tokens: Mutex<Vec<Option<String>>>,
}

impl RecordingSyncResponseHook {
    fn new(store: Arc<MemoryStore>, fail_next: bool) -> Self {
        Self {
            store,
            calls: AtomicUsize::new(0),
            fail_next: AtomicBool::new(fail_next),
            journal: Mutex::new(BTreeSet::new()),
            observed_tokens: Mutex::new(Vec::new()),
        }
    }
}

impl SyncResponseHook for RecordingSyncResponseHook {
    fn on_sync_response<'a>(
        &'a self,
        response: &'a sync_events::v3::Response,
    ) -> BoxFuture<'a, Result<(), SyncResponseHookError>> {
        Box::pin(async move {
            let token = self
                .store
                .get_kv_data(StateStoreDataKey::SyncToken)
                .await
                .map_err(SyncResponseHookError::new)?
                .and_then(StateStoreDataValue::into_sync_token);
            self.observed_tokens.lock().unwrap().push(token);
            self.journal.lock().unwrap().insert(response.next_batch.clone());
            self.calls.fetch_add(1, Ordering::SeqCst);

            if self.fail_next.swap(false, Ordering::SeqCst) {
                Err(SyncResponseHookError::new(io::Error::other("journal unavailable")))
            } else {
                Ok(())
            }
        })
    }
}

fn invite_sync_response() -> (serde_json::Value, String) {
    let room_id = room_id!("!hook:localhost");
    let mut users = BTreeMap::new();
    users.insert(owned_user_id!("@example:localhost"), Int::new(100).unwrap());
    users.insert(owned_user_id!("@bob:localhost"), Int::new(0).unwrap());

    let f = EventFactory::new().room(room_id).sender(user_id!("@bob:localhost"));
    let power_levels_event: Raw<AnyStrippedStateEvent> = f.power_levels(&mut users).into();
    let invited_room = InvitedRoomBuilder::new(room_id).add_state_bulk([
        power_levels_event,
        stripped_state_event!({
            "content": {
                "membership": "join"
            },
            "sender": "@bob:localhost",
            "state_key": "@bob:localhost",
            "type": "m.room.member",
        }),
        stripped_state_event!({
            "content": {
                "displayname": "example",
                "membership": "invite"
            },
            "sender": "@bob:localhost",
            "state_key": "@example:localhost",
            "type": "m.room.member",
        }),
        stripped_state_event!({
            "content": {
                "name": "Hook test"
            },
            "sender": "@bob:localhost",
            "state_key": "",
            "type": "m.room.name",
        }),
    ]);

    let mut builder = SyncResponseBuilder::new();
    builder.add_invited_room(invited_room);
    let response = builder.build_json_sync_response();
    let next_batch = response["next_batch"].as_str().unwrap().to_owned();
    (response, next_batch)
}

async fn stored_sync_token(store: &MemoryStore) -> Option<String> {
    store
        .get_kv_data(StateStoreDataKey::SyncToken)
        .await
        .unwrap()
        .and_then(StateStoreDataValue::into_sync_token)
}

#[async_test]
async fn test_classic_sync_without_hook_preserves_normal_processing() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!no-hook:localhost");

    server
        .mock_sync()
        .ok(|builder| {
            builder.add_joined_room(JoinedRoomBuilder::new(room_id));
        })
        .mock_once()
        .mount()
        .await;

    let response = client.sync_once(SyncSettings::default()).await.unwrap();

    assert!(client.get_room(room_id).is_some());
    assert!(!response.next_batch.is_empty());
}

#[async_test]
async fn test_classic_sync_hook_failure_is_atomic_and_response_can_be_replayed() {
    let server = MatrixMockServer::new().await;
    let store = Arc::new(MemoryStore::default());
    let hook = Arc::new(RecordingSyncResponseHook::new(store.clone(), true));
    let client = server
        .client_builder()
        .on_builder(|builder| {
            builder
                .store_config(
                    StoreConfig::new(CrossProcessLockConfig::SingleProcess)
                        .state_store(store.clone()),
                )
                .sync_response_hook(hook.clone())
        })
        .build()
        .await;

    let room_id = room_id!("!hook:localhost");
    let event_handler_calls = Arc::new(AtomicUsize::new(0));
    client.add_event_handler({
        let calls = event_handler_calls.clone();
        move |_event: StrippedRoomNameEvent| {
            let calls = calls.clone();
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    let notification_handler_calls = Arc::new(AtomicUsize::new(0));
    client
        .register_notification_handler({
            let calls = notification_handler_calls.clone();
            move |_notification, _room, _client| {
                let calls = calls.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                }
            }
        })
        .await;

    let mut room_updates = client.subscribe_to_room_updates(room_id);
    let mut all_room_updates = client.subscribe_to_all_room_updates();
    let (response, next_batch) = invite_sync_response();
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/(r0|v3)/sync$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&response))
        .mount(server.server())
        .await;

    let error =
        client.sync_once(SyncSettings::default().token(SyncToken::NoToken)).await.unwrap_err();
    let Error::SyncResponseHook(hook_error) = &error else {
        panic!("unexpected error: {error}");
    };
    assert_eq!(error.to_string(), "classic sync response hook failed: journal unavailable");
    assert_eq!(hook_error.source().unwrap().to_string(), "journal unavailable");
    assert_eq!(stored_sync_token(&store).await, None);
    assert!(client.get_room(room_id).is_none());
    assert_eq!(event_handler_calls.load(Ordering::SeqCst), 0);
    assert_eq!(notification_handler_calls.load(Ordering::SeqCst), 0);
    assert!(room_updates.try_recv().is_err());
    assert!(all_room_updates.try_recv().is_err());

    client.sync_once(SyncSettings::default().token(SyncToken::NoToken)).await.unwrap();

    assert_eq!(stored_sync_token(&store).await.as_deref(), Some(next_batch.as_str()));
    assert!(client.get_room(room_id).is_some());
    assert_eq!(event_handler_calls.load(Ordering::SeqCst), 1);
    assert_eq!(notification_handler_calls.load(Ordering::SeqCst), 1);
    assert!(room_updates.try_recv().is_ok());
    let RoomUpdates { invited, .. } = all_room_updates.try_recv().unwrap();
    assert!(invited.contains_key(room_id));
    assert_eq!(hook.calls.load(Ordering::SeqCst), 2);
    assert_eq!(hook.journal.lock().unwrap().len(), 1);
    assert_eq!(&*hook.observed_tokens.lock().unwrap(), &[None, None]);
}

#[async_test]
async fn test_classic_sync_http_retry_invokes_hook_only_for_decoded_response() {
    let server = MatrixMockServer::new().await;
    let store = Arc::new(MemoryStore::default());
    let hook = Arc::new(RecordingSyncResponseHook::new(store, false));
    let client = server
        .client_builder()
        .on_builder(|builder| {
            builder
                .request_config(RequestConfig::new().retry_limit(2))
                .sync_response_hook(hook.clone())
        })
        .build()
        .await;

    let attempts = Arc::new(AtomicUsize::new(0));
    let response = SyncResponseBuilder::new().build_json_sync_response();
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/(r0|v3)/sync$"))
        .respond_with({
            let attempts = attempts.clone();
            move |_request: &wiremock::Request| {
                if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(500)
                        .set_body_json(json!({"errcode": "M_UNKNOWN", "error": "temporary"}))
                } else {
                    ResponseTemplate::new(200).set_body_json(&response)
                }
            }
        })
        .expect(2)
        .mount(server.server())
        .await;

    client.sync_once(SyncSettings::default().token(SyncToken::NoToken)).await.unwrap();

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(hook.calls.load(Ordering::SeqCst), 1);
}

#[async_test]
async fn test_receive_room_encryption_event_via_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let user_id = client.user_id().unwrap();
    let room_id = room_id!("!abc");
    let room = server.sync_joined_room(&client, room_id).await;

    // The room info is empty and there is no state event.
    assert_matches!(room.encryption_settings(), None);
    assert_matches!(room.get_state_event_static::<RoomEncryptionEventContent>().await, Ok(None));

    // Listen to raw and deserialized events.
    let raw_event_observer =
        client.observe_room_events::<Raw<SyncStateEvent<RoomEncryptionEventContent>>, ()>(room_id);
    let mut raw_event_subscriber = raw_event_observer.subscribe();
    let event_observer =
        client.observe_room_events::<SyncStateEvent<RoomEncryptionEventContent>, ()>(room_id);
    let mut event_subscriber = event_observer.subscribe();

    // First we receive a valid event.
    let valid_raw_event = Raw::new(&json!({
        "content": {
            "algorithm": "m.megolm.v1.aes-sha2",
        },
        "type": "m.room.encryption",
        "state_key": "",
        "sender": user_id,
        "event_id": "$validevent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_bulk(vec![valid_raw_event.clone()]),
        )
        .await;

    // The room info is set and the valid state event is in the store.
    assert_eq!(
        room.encryption_settings().unwrap().algorithm,
        Some(EventEncryptionAlgorithm::MegolmV1AesSha2)
    );
    assert_matches!(
        room.get_state_event_static::<RoomEncryptionEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    assert_matches!(raw_event.deserialize(), Ok(_));

    // We receive both the raw and deserialized events.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    let (event, _) = assert_ready!(event_subscriber);
    assert_eq!(
        event.as_original().unwrap().content.algorithm,
        EventEncryptionAlgorithm::MegolmV1AesSha2
    );

    // Now we receive an event with an invalid content but a valid type
    // and state key.
    let raw_event_with_invalid_content = Raw::new(&json!({
        "content": {
            // It's a boolean!
            "algorithm": true,
        },
        "type": "m.room.encryption",
        "state_key": "",
        "sender": user_id,
        "event_id": "$eventwithinvalidcontent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_content.clone()]),
        )
        .await;

    // The room info didn't change but the invalid state event is in the store.
    assert_eq!(
        room.encryption_settings().unwrap().algorithm,
        Some(EventEncryptionAlgorithm::MegolmV1AesSha2)
    );
    assert_matches!(
        room.get_state_event_static::<RoomEncryptionEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event but not the deserialized one since it fails
    // to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_pending!(event_subscriber);

    // Finally we receive an event with an invalid state key.
    let raw_event_with_invalid_state_key = Raw::new(&json!({
        "content": {
            "algorithm": "m.megolm.v1.aes-sha2",
        },
        "type": "m.room.encryption",
        // It's a number!
        "state_key": 1,
        "sender": user_id,
        "event_id": "$eventwithinvalidstatekey",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_state_key.clone()]),
        )
        .await;

    // Nothing has changed.
    assert_eq!(
        room.encryption_settings().unwrap().algorithm,
        Some(EventEncryptionAlgorithm::MegolmV1AesSha2)
    );
    assert_matches!(
        room.get_state_event_static::<RoomEncryptionEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event because the event handlers only care about the type,
    // but not the deserialized one since it fails to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_state_key.json().get());
    assert_pending!(event_subscriber);
}

#[async_test]
async fn test_receive_room_avatar_event_via_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let user_id = client.user_id().unwrap();
    let room_id = room_id!("!abc");
    let room = server.sync_joined_room(&client, room_id).await;

    // The room info is empty and there is no state event.
    assert_eq!(room.avatar_url(), None);
    assert_matches!(room.get_state_event_static::<RoomAvatarEventContent>().await, Ok(None));

    // Listen to raw and deserialized events.
    let raw_event_observer =
        client.observe_room_events::<Raw<SyncStateEvent<RoomAvatarEventContent>>, ()>(room_id);
    let mut raw_event_subscriber = raw_event_observer.subscribe();
    let event_observer =
        client.observe_room_events::<SyncStateEvent<RoomAvatarEventContent>, ()>(room_id);
    let mut event_subscriber = event_observer.subscribe();

    // First we receive a valid event.
    let avatar_url = mxc_uri!("mxc://localhost/1234");
    let valid_raw_event = Raw::new(&json!({
        "content": {
            "url": avatar_url,
        },
        "type": "m.room.avatar",
        "state_key": "",
        "sender": user_id,
        "event_id": "$validevent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_bulk(vec![valid_raw_event.clone()]),
        )
        .await;

    // The room info is set and the valid state event is in the store.
    assert_eq!(room.avatar_url().as_deref(), Some(avatar_url));
    assert_matches!(
        room.get_state_event_static::<RoomAvatarEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    assert_matches!(raw_event.deserialize(), Ok(_));

    // We receive both the raw and deserialized events.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    let (event, _) = assert_ready!(event_subscriber);
    assert_eq!(event.as_original().unwrap().content.url.as_deref(), Some(avatar_url));

    // Now we receive an event with an invalid content but a valid type
    // and state key.
    let raw_event_with_invalid_content = Raw::new(&json!({
        "content": {
            // It's a boolean!
            "url": true,
        },
        "type": "m.room.avatar",
        "state_key": "",
        "sender": user_id,
        "event_id": "$eventwithinvalidcontent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_content.clone()]),
        )
        .await;

    // The room info is unset and the invalid state event is in the store.
    assert_eq!(room.avatar_url(), None);
    assert_matches!(
        room.get_state_event_static::<RoomAvatarEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event but not the deserialized one since it fails
    // to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_pending!(event_subscriber);

    // Finally we receive an event with an invalid state key.
    let raw_event_with_invalid_state_key = Raw::new(&json!({
        "content": {
            "url": "mxc://localhost/zyxw",
        },
        "type": "m.room.avatar",
        // It's a number!
        "state_key": 1,
        "sender": user_id,
        "event_id": "$eventwithinvalidstatekey",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_state_key.clone()]),
        )
        .await;

    // Nothing has changed.
    assert_eq!(room.avatar_url(), None);
    assert_matches!(
        room.get_state_event_static::<RoomAvatarEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event because the event handlers only care about the type,
    // but not the deserialized one since it fails to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_state_key.json().get());
    assert_pending!(event_subscriber);
}

#[async_test]
async fn test_receive_room_name_event_via_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let user_id = client.user_id().unwrap();
    let room_id = room_id!("!abc");
    let room = server.sync_joined_room(&client, room_id).await;

    // The room info is empty and there is no state event.
    assert_eq!(room.name(), None);
    assert_matches!(room.get_state_event_static::<RoomNameEventContent>().await, Ok(None));

    // Listen to raw and deserialized events.
    let raw_event_observer =
        client.observe_room_events::<Raw<SyncStateEvent<RoomNameEventContent>>, ()>(room_id);
    let mut raw_event_subscriber = raw_event_observer.subscribe();
    let event_observer =
        client.observe_room_events::<SyncStateEvent<RoomNameEventContent>, ()>(room_id);
    let mut event_subscriber = event_observer.subscribe();

    // First we receive a valid event.
    let room_name = "My room";
    let valid_raw_event = Raw::new(&json!({
        "content": {
            "name": room_name,
        },
        "type": "m.room.name",
        "state_key": "",
        "sender": user_id,
        "event_id": "$validevent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_bulk(vec![valid_raw_event.clone()]),
        )
        .await;

    // The room info is set and the valid state event is in the store.
    assert_eq!(room.name().as_deref(), Some(room_name));
    assert_matches!(
        room.get_state_event_static::<RoomNameEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    assert_matches!(raw_event.deserialize(), Ok(_));

    // We receive both the raw and deserialized events.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    let (event, _) = assert_ready!(event_subscriber);
    assert_eq!(event.as_original().unwrap().content.name, "My room");

    // Now we receive an event with an invalid content but a valid type
    // and state key.
    let raw_event_with_invalid_content = Raw::new(&json!({
        "content": {
            // It's a boolean!
            "name": true,
        },
        "type": "m.room.name",
        "state_key": "",
        "sender": user_id,
        "event_id": "$eventwithinvalidcontent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_content.clone()]),
        )
        .await;

    // The room info is unset and the invalid state event is in the store.
    assert_eq!(room.name(), None);
    assert_matches!(
        room.get_state_event_static::<RoomNameEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event but not the deserialized one since it fails
    // to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_pending!(event_subscriber);

    // Finally we receive an event with an invalid state key.
    let raw_event_with_invalid_state_key = Raw::new(&json!({
        "content": {
            "name": room_name,
        },
        "type": "m.room.name",
        // It's a number!
        "state_key": 1,
        "sender": user_id,
        "event_id": "$eventwithinvalidstatekey",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_state_key.clone()]),
        )
        .await;

    // Nothing has changed.
    assert_eq!(room.name(), None);
    assert_matches!(
        room.get_state_event_static::<RoomNameEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event because the event handlers only care about the type,
    // but not the deserialized one since it fails to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_state_key.json().get());
    assert_pending!(event_subscriber);
}

#[async_test]
async fn test_receive_room_create_event_via_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let user_id = client.user_id().unwrap();
    let room_id = room_id!("!abc");
    let room = server.sync_joined_room(&client, room_id).await;

    // The room info is empty and there is no state event.
    assert_matches!(room.create_content(), None);
    assert_matches!(room.get_state_event_static::<RoomCreateEventContent>().await, Ok(None));

    // Listen to raw and deserialized events.
    let raw_event_observer =
        client.observe_room_events::<Raw<SyncStateEvent<RoomCreateEventContent>>, ()>(room_id);
    let mut raw_event_subscriber = raw_event_observer.subscribe();
    let event_observer =
        client.observe_room_events::<SyncStateEvent<RoomCreateEventContent>, ()>(room_id);
    let mut event_subscriber = event_observer.subscribe();

    // First we receive a valid event.
    let valid_raw_event = Raw::new(&json!({
        "content": {
            "room_version": "12",
        },
        "type": "m.room.create",
        "state_key": "",
        "sender": user_id,
        "event_id": "$validevent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_bulk(vec![valid_raw_event.clone()]),
        )
        .await;

    // The room info is set and the valid state event is in the store.
    assert_eq!(room.create_content().unwrap().room_version, RoomVersionId::V12);
    assert_matches!(
        room.get_state_event_static::<RoomCreateEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    assert_matches!(raw_event.deserialize(), Ok(_));

    // We receive both the raw and deserialized events.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    let (event, _) = assert_ready!(event_subscriber);
    assert_eq!(event.as_original().unwrap().content.room_version, RoomVersionId::V12);

    // Now we receive an event with an invalid content but a valid type
    // and state key.
    let raw_event_with_invalid_content = Raw::new(&json!({
        "content": {
            // It's a boolean!
            "room_version": true,
        },
        "type": "m.room.create",
        "state_key": "",
        "sender": user_id,
        "event_id": "$eventwithinvalidcontent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_content.clone()]),
        )
        .await;

    // The room info didn't change because it never changes after being set, and the
    // invalid state event is in the store.
    assert_eq!(room.create_content().unwrap().room_version, RoomVersionId::V12);
    assert_matches!(
        room.get_state_event_static::<RoomCreateEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event but not the deserialized one since it fails
    // to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_pending!(event_subscriber);

    // We checked that the create content is immutable, now let us try again with a
    // new room to see if the event would even be accepted.
    let room_id = room_id!("!def");
    let room = server.sync_joined_room(&client, room_id).await;

    // The new room info is empty and there is no state event.
    assert_matches!(room.create_content(), None);
    assert_matches!(room.get_state_event_static::<RoomCreateEventContent>().await, Ok(None));

    // Listen to raw and deserialized events in the new room.
    let raw_event_observer =
        client.observe_room_events::<Raw<SyncStateEvent<RoomCreateEventContent>>, ()>(room_id);
    let mut raw_event_subscriber = raw_event_observer.subscribe();
    let event_observer =
        client.observe_room_events::<SyncStateEvent<RoomCreateEventContent>, ()>(room_id);
    let mut event_subscriber = event_observer.subscribe();

    // Receive the event with the invalid content in the new room.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_content.clone()]),
        )
        .await;

    // The room info didn't change because the event is invalid, but the
    // invalid state event is in the store.
    assert_matches!(room.create_content(), None);
    assert_matches!(
        room.get_state_event_static::<RoomCreateEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event but not the deserialized one since it fails
    // to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_pending!(event_subscriber);

    // Finally we receive an event with an invalid state key.
    let raw_event_with_invalid_state_key = Raw::new(&json!({
        "content": {
            "room_version": "12",
        },
        "type": "m.room.create",
        // It's a number!
        "state_key": 1,
        "sender": user_id,
        "event_id": "$eventwithinvalidstatekey",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_state_key.clone()]),
        )
        .await;

    // Nothing has changed.
    assert_matches!(room.create_content(), None);
    assert_matches!(
        room.get_state_event_static::<RoomCreateEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event because the event handlers only care about the type,
    // but not the deserialized one since it fails to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_state_key.json().get());
    assert_pending!(event_subscriber);
}

#[async_test]
async fn test_receive_room_history_visibility_event_via_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let user_id = client.user_id().unwrap();
    let room_id = room_id!("!abc");
    let room = server.sync_joined_room(&client, room_id).await;

    // The room info is empty and there is no state event.
    assert_eq!(room.history_visibility(), None);
    assert_matches!(
        room.get_state_event_static::<RoomHistoryVisibilityEventContent>().await,
        Ok(None)
    );

    // Listen to raw and deserialized events.
    let raw_event_observer = client
        .observe_room_events::<Raw<SyncStateEvent<RoomHistoryVisibilityEventContent>>, ()>(room_id);
    let mut raw_event_subscriber = raw_event_observer.subscribe();
    let event_observer = client
        .observe_room_events::<SyncStateEvent<RoomHistoryVisibilityEventContent>, ()>(room_id);
    let mut event_subscriber = event_observer.subscribe();

    // First we receive a valid event.
    let valid_raw_event = Raw::new(&json!({
        "content": {
            "history_visibility": "shared",
        },
        "type": "m.room.history_visibility",
        "state_key": "",
        "sender": user_id,
        "event_id": "$validevent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_bulk(vec![valid_raw_event.clone()]),
        )
        .await;

    // The room info is set and the valid state event is in the store.
    assert_eq!(room.history_visibility(), Some(HistoryVisibility::Shared));
    assert_matches!(
        room.get_state_event_static::<RoomHistoryVisibilityEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    assert_matches!(raw_event.deserialize(), Ok(_));

    // We receive both the raw and deserialized events.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    let (event, _) = assert_ready!(event_subscriber);
    assert_eq!(event.as_original().unwrap().content.history_visibility, HistoryVisibility::Shared);

    // Now we receive an event with an invalid content but a valid type
    // and state key.
    let raw_event_with_invalid_content = Raw::new(&json!({
        "content": {
            // It's a boolean!
            "history_visibility": true,
        },
        "type": "m.room.history_visibility",
        "state_key": "",
        "sender": user_id,
        "event_id": "$eventwithinvalidcontent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_content.clone()]),
        )
        .await;

    // The room info is unset and the invalid state event is in the store.
    assert_eq!(room.history_visibility(), None);
    assert_matches!(
        room.get_state_event_static::<RoomHistoryVisibilityEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event but not the deserialized one since it fails
    // to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_pending!(event_subscriber);

    // Finally we receive an event with an invalid state key.
    let raw_event_with_invalid_state_key = Raw::new(&json!({
        "content": {
            "history_visibility": "shared",
        },
        "type": "m.room.history_visibility",
        // It's a number!
        "state_key": 1,
        "sender": user_id,
        "event_id": "$eventwithinvalidstatekey",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_state_key.clone()]),
        )
        .await;

    // Nothing has changed.
    assert_eq!(room.name(), None);
    assert_matches!(
        room.get_state_event_static::<RoomHistoryVisibilityEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event because the event handlers only care about the type,
    // but not the deserialized one since it fails to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_state_key.json().get());
    assert_pending!(event_subscriber);
}

#[async_test]
async fn test_receive_room_guest_access_event_via_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let user_id = client.user_id().unwrap();
    let room_id = room_id!("!abc");
    let room = server.sync_joined_room(&client, room_id).await;

    // The room info uses the default and there is no state event.
    assert_eq!(room.guest_access(), GuestAccess::Forbidden);
    assert_matches!(room.get_state_event_static::<RoomGuestAccessEventContent>().await, Ok(None));

    // Listen to raw and deserialized events.
    let raw_event_observer =
        client.observe_room_events::<Raw<SyncStateEvent<RoomGuestAccessEventContent>>, ()>(room_id);
    let mut raw_event_subscriber = raw_event_observer.subscribe();
    let event_observer =
        client.observe_room_events::<SyncStateEvent<RoomGuestAccessEventContent>, ()>(room_id);
    let mut event_subscriber = event_observer.subscribe();

    // First we receive a valid event.
    let valid_raw_event = Raw::new(&json!({
        "content": {
            "guest_access": "can_join",
        },
        "type": "m.room.guest_access",
        "state_key": "",
        "sender": user_id,
        "event_id": "$validevent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_bulk(vec![valid_raw_event.clone()]),
        )
        .await;

    // The room info is set and the valid state event is in the store.
    assert_eq!(room.guest_access(), GuestAccess::CanJoin);
    assert_matches!(
        room.get_state_event_static::<RoomGuestAccessEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    assert_matches!(raw_event.deserialize(), Ok(_));

    // We receive both the raw and deserialized events.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    let (event, _) = assert_ready!(event_subscriber);
    assert_eq!(event.as_original().unwrap().content.guest_access, GuestAccess::CanJoin);

    // Now we receive an event with an invalid content but a valid type
    // and state key.
    let raw_event_with_invalid_content = Raw::new(&json!({
        "content": {
            // It's a boolean!
            "guest_access": true,
        },
        "type": "m.room.guest_access",
        "state_key": "",
        "sender": user_id,
        "event_id": "$eventwithinvalidcontent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_content.clone()]),
        )
        .await;

    // The room info reverted to the default and the invalid state event is in the
    // store.
    assert_eq!(room.guest_access(), GuestAccess::Forbidden);
    assert_matches!(
        room.get_state_event_static::<RoomGuestAccessEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event but not the deserialized one since it fails
    // to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_pending!(event_subscriber);

    // Finally we receive an event with an invalid state key.
    let raw_event_with_invalid_state_key = Raw::new(&json!({
        "content": {
            "guest_access": "can_join",
        },
        "type": "m.room.guest_access",
        // It's a number!
        "state_key": 1,
        "sender": user_id,
        "event_id": "$eventwithinvalidstatekey",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_state_key.clone()]),
        )
        .await;

    // Nothing has changed.
    assert_eq!(room.guest_access(), GuestAccess::Forbidden);
    assert_matches!(
        room.get_state_event_static::<RoomGuestAccessEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event because the event handlers only care about the type,
    // but not the deserialized one since it fails to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_state_key.json().get());
    assert_pending!(event_subscriber);
}

#[async_test]
async fn test_receive_room_join_rules_event_via_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let user_id = client.user_id().unwrap();
    let room_id = room_id!("!abc");
    let room = server.sync_joined_room(&client, room_id).await;

    // The room info is empty and there is no state event.
    assert_eq!(room.join_rule(), None);
    assert_matches!(room.get_state_event_static::<RoomJoinRulesEventContent>().await, Ok(None));

    // Listen to raw and deserialized events.
    let raw_event_observer =
        client.observe_room_events::<Raw<SyncStateEvent<RoomJoinRulesEventContent>>, ()>(room_id);
    let mut raw_event_subscriber = raw_event_observer.subscribe();
    let event_observer =
        client.observe_room_events::<SyncStateEvent<RoomJoinRulesEventContent>, ()>(room_id);
    let mut event_subscriber = event_observer.subscribe();

    // First we receive a valid event.
    let valid_raw_event = Raw::new(&json!({
        "content": {
            "join_rule": "public",
        },
        "type": "m.room.join_rules",
        "state_key": "",
        "sender": user_id,
        "event_id": "$validevent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_bulk(vec![valid_raw_event.clone()]),
        )
        .await;

    // The room info is set and the valid state event is in the store.
    assert_eq!(room.join_rule(), Some(JoinRule::Public));
    assert_matches!(
        room.get_state_event_static::<RoomJoinRulesEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    assert_matches!(raw_event.deserialize(), Ok(_));

    // We receive both the raw and deserialized events.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    let (event, _) = assert_ready!(event_subscriber);
    assert_eq!(event.as_original().unwrap().content.join_rule, JoinRule::Public);

    // Now we receive an event with an invalid content but a valid type
    // and state key.
    let raw_event_with_invalid_content = Raw::new(&json!({
        "content": {
            // It's a boolean!
            "join_rule": true,
        },
        "type": "m.room.join_rules",
        "state_key": "",
        "sender": user_id,
        "event_id": "$eventwithinvalidcontent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_content.clone()]),
        )
        .await;

    // The room info is unset and the invalid state event is in the
    // store.
    assert_eq!(room.join_rule(), None);
    assert_matches!(
        room.get_state_event_static::<RoomJoinRulesEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event but not the deserialized one since it fails
    // to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_pending!(event_subscriber);

    // Finally we receive an event with an invalid state key.
    let raw_event_with_invalid_state_key = Raw::new(&json!({
        "content": {
            "join_rule": "public",
        },
        "type": "m.room.join_rules",
        // It's a number!
        "state_key": 1,
        "sender": user_id,
        "event_id": "$eventwithinvalidstatekey",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_state_key.clone()]),
        )
        .await;

    // Nothing has changed.
    assert_eq!(room.join_rule(), None);
    assert_matches!(
        room.get_state_event_static::<RoomJoinRulesEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event because the event handlers only care about the type,
    // but not the deserialized one since it fails to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_state_key.json().get());
    assert_pending!(event_subscriber);
}

#[async_test]
async fn test_receive_room_canonical_alias_event_via_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let user_id = client.user_id().unwrap();
    let room_id = room_id!("!abc");
    let room = server.sync_joined_room(&client, room_id).await;

    // The room info is empty and there is no state event.
    assert_eq!(room.canonical_alias(), None);
    assert_matches!(
        room.get_state_event_static::<RoomCanonicalAliasEventContent>().await,
        Ok(None)
    );

    // Listen to raw and deserialized events.
    let raw_event_observer = client
        .observe_room_events::<Raw<SyncStateEvent<RoomCanonicalAliasEventContent>>, ()>(room_id);
    let mut raw_event_subscriber = raw_event_observer.subscribe();
    let event_observer =
        client.observe_room_events::<SyncStateEvent<RoomCanonicalAliasEventContent>, ()>(room_id);
    let mut event_subscriber = event_observer.subscribe();

    // First we receive a valid event.
    let room_alias = room_alias_id!("#myroom:localhost");
    let valid_raw_event = Raw::new(&json!({
        "content": {
            "alias": room_alias,
        },
        "type": "m.room.canonical_alias",
        "state_key": "",
        "sender": user_id,
        "event_id": "$validevent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_bulk(vec![valid_raw_event.clone()]),
        )
        .await;

    // The room info is set and the valid state event is in the store.
    assert_eq!(room.canonical_alias().as_deref(), Some(room_alias));
    assert_matches!(
        room.get_state_event_static::<RoomCanonicalAliasEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    assert_matches!(raw_event.deserialize(), Ok(_));

    // We receive both the raw and deserialized events.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    let (event, _) = assert_ready!(event_subscriber);
    assert_eq!(event.as_original().unwrap().content.alias.as_deref(), Some(room_alias));

    // Now we receive an event with an invalid content but a valid type
    // and state key.
    let raw_event_with_invalid_content = Raw::new(&json!({
        "content": {
            // It's a boolean!
            "alias": true,
        },
        "type": "m.room.canonical_alias",
        "state_key": "",
        "sender": user_id,
        "event_id": "$eventwithinvalidcontent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_content.clone()]),
        )
        .await;

    // The room info is unset and the invalid state event is in the
    // store.
    assert_eq!(room.canonical_alias(), None);
    assert_matches!(
        room.get_state_event_static::<RoomCanonicalAliasEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event but not the deserialized one since it fails
    // to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_pending!(event_subscriber);

    // Finally we receive an event with an invalid state key.
    let raw_event_with_invalid_state_key = Raw::new(&json!({
        "content": {
            "alias": room_alias,
        },
        "type": "m.room.canonical_alias",
        // It's a number!
        "state_key": 1,
        "sender": user_id,
        "event_id": "$eventwithinvalidstatekey",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_state_key.clone()]),
        )
        .await;

    // Nothing has changed.
    assert_eq!(room.canonical_alias(), None);
    assert_matches!(
        room.get_state_event_static::<RoomCanonicalAliasEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event because the event handlers only care about the type,
    // but not the deserialized one since it fails to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_state_key.json().get());
    assert_pending!(event_subscriber);
}

#[async_test]
async fn test_receive_room_topic_event_via_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let user_id = client.user_id().unwrap();
    let room_id = room_id!("!abc");
    let room = server.sync_joined_room(&client, room_id).await;

    // The room info is empty and there is no state event.
    assert_eq!(room.topic(), None);
    assert_matches!(room.get_state_event_static::<RoomTopicEventContent>().await, Ok(None));

    // Listen to raw and deserialized events.
    let raw_event_observer =
        client.observe_room_events::<Raw<SyncStateEvent<RoomTopicEventContent>>, ()>(room_id);
    let mut raw_event_subscriber = raw_event_observer.subscribe();
    let event_observer =
        client.observe_room_events::<SyncStateEvent<RoomTopicEventContent>, ()>(room_id);
    let mut event_subscriber = event_observer.subscribe();

    // First we receive a valid event.
    let room_topic = "A room about me, myself, and I!";
    let valid_raw_event = Raw::new(&json!({
        "content": {
            "topic": room_topic,
        },
        "type": "m.room.topic",
        "state_key": "",
        "sender": user_id,
        "event_id": "$validevent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_bulk(vec![valid_raw_event.clone()]),
        )
        .await;

    // The room info is set and the valid state event is in the store.
    assert_eq!(room.topic().as_deref(), Some(room_topic));
    assert_matches!(
        room.get_state_event_static::<RoomTopicEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    assert_matches!(raw_event.deserialize(), Ok(_));

    // We receive both the raw and deserialized events.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    let (event, _) = assert_ready!(event_subscriber);
    assert_eq!(event.as_original().unwrap().content.topic, room_topic);

    // Now we receive an event with an invalid content but a valid type
    // and state key.
    let raw_event_with_invalid_content = Raw::new(&json!({
        "content": {
            // It's a boolean!
            "topic": true,
        },
        "type": "m.room.topic",
        "state_key": "",
        "sender": user_id,
        "event_id": "$eventwithinvalidcontent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_content.clone()]),
        )
        .await;

    // The room info is unset and the invalid state event is in the
    // store.
    assert_eq!(room.topic(), None);
    assert_matches!(
        room.get_state_event_static::<RoomTopicEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event but not the deserialized one since it fails
    // to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_pending!(event_subscriber);

    // Finally we receive an event with an invalid state key.
    let raw_event_with_invalid_state_key = Raw::new(&json!({
        "content": {
            "topic": room_topic,
        },
        "type": "m.room.topic",
        // It's a number!
        "state_key": 1,
        "sender": user_id,
        "event_id": "$eventwithinvalidstatekey",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_state_key.clone()]),
        )
        .await;

    // Nothing has changed.
    assert_eq!(room.topic(), None);
    assert_matches!(
        room.get_state_event_static::<RoomTopicEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event because the event handlers only care about the type,
    // but not the deserialized one since it fails to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_state_key.json().get());
    assert_pending!(event_subscriber);
}

#[async_test]
async fn test_receive_room_tombstone_event_via_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let user_id = client.user_id().unwrap();
    let room_id = room_id!("!abc");
    let room = server.sync_joined_room(&client, room_id).await;

    // The room info is empty and there is no state event.
    assert_matches!(room.tombstone_content(), None);
    assert_matches!(room.get_state_event_static::<RoomTombstoneEventContent>().await, Ok(None));

    // Listen to raw and deserialized events.
    let raw_event_observer =
        client.observe_room_events::<Raw<SyncStateEvent<RoomTombstoneEventContent>>, ()>(room_id);
    let mut raw_event_subscriber = raw_event_observer.subscribe();
    let event_observer =
        client.observe_room_events::<SyncStateEvent<RoomTombstoneEventContent>, ()>(room_id);
    let mut event_subscriber = event_observer.subscribe();

    // First we receive a valid event.
    let tombstone_replacement = room_id!("!replacement");
    let valid_raw_event = Raw::new(&json!({
        "content": {
            "body": "!",
            "replacement_room": tombstone_replacement,
        },
        "type": "m.room.tombstone",
        "state_key": "",
        "sender": user_id,
        "event_id": "$validevent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_bulk(vec![valid_raw_event.clone()]),
        )
        .await;

    // The room info is set and the valid state event is in the store.
    assert_eq!(
        room.tombstone_content().unwrap().replacement_room.as_deref(),
        Some(tombstone_replacement)
    );
    assert_matches!(
        room.get_state_event_static::<RoomTombstoneEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    assert_matches!(raw_event.deserialize(), Ok(_));

    // We receive both the raw and deserialized events.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    let (event, _) = assert_ready!(event_subscriber);
    assert_eq!(event.as_original().unwrap().content.replacement_room, tombstone_replacement);

    // Now we receive an event with an invalid content but a valid type
    // and state key.
    let raw_event_with_invalid_content = Raw::new(&json!({
        "content": {
            "body": "!",
            // It's a boolean!
            "replacement_room": true,
        },
        "type": "m.room.tombstone",
        "state_key": "",
        "sender": user_id,
        "event_id": "$eventwithinvalidcontent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_content.clone()]),
        )
        .await;

    // The room info is unset and the invalid state event is in the
    // store.
    assert_matches!(room.tombstone_content(), None);
    assert_matches!(
        room.get_state_event_static::<RoomTombstoneEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event but not the deserialized one since it fails
    // to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_pending!(event_subscriber);

    // Finally we receive an event with an invalid state key.
    let raw_event_with_invalid_state_key = Raw::new(&json!({
        "content": {
            "body": "!",
            "replacement_room": tombstone_replacement,
        },
        "type": "m.room.tombstone",
        // It's a number!
        "state_key": 1,
        "sender": user_id,
        "event_id": "$eventwithinvalidstatekey",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_state_key.clone()]),
        )
        .await;

    // Nothing has changed.
    assert_matches!(room.tombstone_content(), None);
    assert_matches!(
        room.get_state_event_static::<RoomTombstoneEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event because the event handlers only care about the type,
    // but not the deserialized one since it fails to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_state_key.json().get());
    assert_pending!(event_subscriber);
}

#[async_test]
async fn test_receive_room_power_levels_event_via_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let user_id = client.user_id().unwrap();
    let room_id = room_id!("!abc");
    let room = server.sync_joined_room(&client, room_id).await;

    // The room info uses the default and there is no state event.
    assert_matches!(room.max_power_level(), 100);
    assert_matches!(room.get_state_event_static::<RoomPowerLevelsEventContent>().await, Ok(None));

    // Listen to raw and deserialized events.
    let raw_event_observer =
        client.observe_room_events::<Raw<SyncStateEvent<RoomPowerLevelsEventContent>>, ()>(room_id);
    let mut raw_event_subscriber = raw_event_observer.subscribe();
    let event_observer =
        client.observe_room_events::<SyncStateEvent<RoomPowerLevelsEventContent>, ()>(room_id);
    let mut event_subscriber = event_observer.subscribe();

    // First we receive a valid event.
    let valid_raw_event = Raw::new(&json!({
        "content": {
            "users_default": -10,
        },
        "type": "m.room.power_levels",
        "state_key": "",
        "sender": user_id,
        "event_id": "$validevent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_bulk(vec![valid_raw_event.clone()]),
        )
        .await;

    // The room info is set and the valid state event is in the store.
    assert_eq!(room.max_power_level(), -10);
    assert_matches!(
        room.get_state_event_static::<RoomPowerLevelsEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    assert_matches!(raw_event.deserialize(), Ok(_));

    // We receive both the raw and deserialized events.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    let (event, _) = assert_ready!(event_subscriber);
    assert_eq!(i64::from(event.as_original().unwrap().content.users_default), -10);

    // Now we receive an event with an invalid content but a valid type
    // and state key.
    let raw_event_with_invalid_content = Raw::new(&json!({
        "content": {
            // It's a boolean!
            "users_default": true,
        },
        "type": "m.room.power_levels",
        "state_key": "",
        "sender": user_id,
        "event_id": "$eventwithinvalidcontent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_content.clone()]),
        )
        .await;

    // The room info is reset and the invalid state event is in the
    // store.
    assert_eq!(room.max_power_level(), 100);
    assert_matches!(
        room.get_state_event_static::<RoomPowerLevelsEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event but not the deserialized one since it fails
    // to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_pending!(event_subscriber);

    // Finally we receive an event with an invalid state key.
    let raw_event_with_invalid_state_key = Raw::new(&json!({
        "content": {
            "users_default": -10,
        },
        "type": "m.room.power_levels",
        // It's a number!
        "state_key": 1,
        "sender": user_id,
        "event_id": "$eventwithinvalidstatekey",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_state_key.clone()]),
        )
        .await;

    // Nothing has changed.
    assert_eq!(room.max_power_level(), 100);
    assert_matches!(
        room.get_state_event_static::<RoomPowerLevelsEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event because the event handlers only care about the type,
    // but not the deserialized one since it fails to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_state_key.json().get());
    assert_pending!(event_subscriber);
}

#[async_test]
async fn test_receive_room_pinned_events_event_via_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let user_id = client.user_id().unwrap();
    let room_id = room_id!("!abc");
    let room = server.sync_joined_room(&client, room_id).await;

    // The room info is empty and there is no state event.
    assert_eq!(room.pinned_event_ids(), None);
    assert_matches!(room.get_state_event_static::<RoomPinnedEventsEventContent>().await, Ok(None));

    // Listen to raw and deserialized events.
    let raw_event_observer = client
        .observe_room_events::<Raw<SyncStateEvent<RoomPinnedEventsEventContent>>, ()>(room_id);
    let mut raw_event_subscriber = raw_event_observer.subscribe();
    let event_observer =
        client.observe_room_events::<SyncStateEvent<RoomPinnedEventsEventContent>, ()>(room_id);
    let mut event_subscriber = event_observer.subscribe();

    // First we receive a valid event.
    let pinned_event = event_id!("$pinned");
    let valid_raw_event = Raw::new(&json!({
        "content": {
            "pinned": [pinned_event],
        },
        "type": "m.room.pinned_events",
        "state_key": "",
        "sender": user_id,
        "event_id": "$validevent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_bulk(vec![valid_raw_event.clone()]),
        )
        .await;

    // The room info is set and the valid state event is in the store.
    assert_eq!(room.pinned_event_ids().unwrap(), &[pinned_event]);
    assert_matches!(
        room.get_state_event_static::<RoomPinnedEventsEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    assert_matches!(raw_event.deserialize(), Ok(_));

    // We receive both the raw and deserialized events.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    let (event, _) = assert_ready!(event_subscriber);
    assert_eq!(event.as_original().unwrap().content.pinned, &[pinned_event]);

    // Now we receive an event with an invalid content but a valid type
    // and state key.
    let raw_event_with_invalid_content = Raw::new(&json!({
        "content": {
            // It's a boolean!
            "pinned": true,
        },
        "type": "m.room.pinned_events",
        "state_key": "",
        "sender": user_id,
        "event_id": "$eventwithinvalidcontent",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_content.clone()]),
        )
        .await;

    // The room info is unset and the invalid state event is in the
    // store.
    assert_eq!(room.pinned_event_ids(), None);
    assert_matches!(
        room.get_state_event_static::<RoomPinnedEventsEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event but not the deserialized one since it fails
    // to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_pending!(event_subscriber);

    // Finally we receive an event with an invalid state key.
    let raw_event_with_invalid_state_key = Raw::new(&json!({
        "content": {
            "pinned": [pinned_event],
        },
        "type": "m.room.pinned_events",
        // It's a number!
        "state_key": 1,
        "sender": user_id,
        "event_id": "$eventwithinvalidstatekey",
        "origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
    }))
    .unwrap()
    .cast_unchecked::<AnySyncStateEvent>();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_state_bulk(vec![raw_event_with_invalid_state_key.clone()]),
        )
        .await;

    // Nothing has changed.
    assert_eq!(room.pinned_event_ids(), None);
    assert_matches!(
        room.get_state_event_static::<RoomPinnedEventsEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Sync(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // We receive the raw event because the event handlers only care about the type,
    // but not the deserialized one since it fails to deserialize.
    let (raw_event, _) = assert_ready!(raw_event_subscriber);
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_state_key.json().get());
    assert_pending!(event_subscriber);
}

#[async_test]
async fn test_receive_stripped_room_encryption_event_via_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();

    // First we receive a valid event.
    let valid_raw_event = Raw::new(&json!({
        "content": {
            "algorithm": "m.megolm.v1.aes-sha2",
        },
        "type": "m.room.encryption",
        "state_key": "",
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            InvitedRoomBuilder::new(room_id!("!roomwithvalidencryption"))
                .add_state_bulk(vec![valid_raw_event.clone()]),
        )
        .await;

    // The room info is set and the valid state event is in the store.
    assert_eq!(
        room.encryption_settings().unwrap().algorithm,
        Some(EventEncryptionAlgorithm::MegolmV1AesSha2)
    );
    assert_matches!(
        room.get_state_event_static::<RoomEncryptionEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Stripped(raw_event)))
    );
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    assert_matches!(raw_event.deserialize(), Ok(_));

    // Then we receive a redacted event.
    let redacted_raw_event = Raw::new(&json!({
        "content": {},
        "type": "m.room.encryption",
        "state_key": "",
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            InvitedRoomBuilder::new(room_id!("!roomwithredactedencryption"))
                .add_state_bulk(vec![redacted_raw_event.clone()]),
        )
        .await;

    // The room info is empty but the state event is in the store.
    assert_matches!(room.encryption_settings(), None);
    assert_matches!(
        room.get_state_event_static::<RoomEncryptionEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Stripped(raw_event)))
    );
    assert_eq!(raw_event.json().get(), redacted_raw_event.json().get());
    assert_matches!(raw_event.deserialize(), Ok(_));

    // Now we receive an event with an invalid content but a valid type
    // and state key.
    let raw_event_with_invalid_content = Raw::new(&json!({
        "content": {
            // It's a boolean!
            "algorithm": true,
        },
        "type": "m.room.encryption",
        "state_key": "",
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            InvitedRoomBuilder::new(room_id!("!roomwithinvalidencryptioncontent"))
                .add_state_bulk(vec![raw_event_with_invalid_content.clone()]),
        )
        .await;

    // The room info is empty but the state event is in the store.
    assert_matches!(room.encryption_settings(), None);
    assert_matches!(
        room.get_state_event_static::<RoomEncryptionEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Stripped(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // Finally we receive an event with an invalid state key.
    let raw_event_with_invalid_state_key = Raw::new(&json!({
        "content": {
            "algorithm": "m.megolm.v1.aes-sha2",
        },
        "type": "m.room.encryption",
        // It's a number!
        "state_key": 1,
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            InvitedRoomBuilder::new(room_id!("!roomwithinvalidencryptionstatekey"))
                .add_state_bulk(vec![raw_event_with_invalid_state_key.clone()]),
        )
        .await;

    // The room info is empty and the state event is not in the store.
    assert_matches!(room.encryption_settings(), None);
    assert_matches!(room.get_state_event_static::<RoomEncryptionEventContent>().await, Ok(None));
}

#[async_test]
async fn test_receive_stripped_room_avatar_event_via_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();

    // First we receive a valid event.
    let avatar_url = mxc_uri!("mxc://localhost/1234");
    let valid_raw_event = Raw::new(&json!({
        "content": {
            "url": avatar_url,
        },
        "type": "m.room.avatar",
        "state_key": "",
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            InvitedRoomBuilder::new(room_id!("!roomwithvalidname"))
                .add_state_bulk(vec![valid_raw_event.clone()]),
        )
        .await;

    // The room info is set and the valid state event is in the store.
    assert_eq!(room.avatar_url().as_deref(), Some(avatar_url));
    assert_matches!(
        room.get_state_event_static::<RoomAvatarEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Stripped(raw_event)))
    );
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    assert_matches!(raw_event.deserialize(), Ok(_));

    // Now we receive an event with an invalid content but a valid type
    // and state key.
    let raw_event_with_invalid_content = Raw::new(&json!({
        "content": {
            // It's a boolean!
            "url": true,
        },
        "type": "m.room.avatar",
        "state_key": "",
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            InvitedRoomBuilder::new(room_id!("!roomwithinvalidavatarcontent"))
                .add_state_bulk(vec![raw_event_with_invalid_content.clone()]),
        )
        .await;

    // The room info is not set but the invalid state event is in the store.
    assert_eq!(room.avatar_url(), None);
    assert_matches!(
        room.get_state_event_static::<RoomAvatarEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Stripped(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // Finally we receive an event with an invalid state key.
    let raw_event_with_invalid_state_key = Raw::new(&json!({
        "content": {
            "url": "mxc://localhost/zyxw",
        },
        "type": "m.room.avatar",
        // It's a number!
        "state_key": 1,
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            InvitedRoomBuilder::new(room_id!("!roomwithinvalidavatarstatekey"))
                .add_state_bulk(vec![raw_event_with_invalid_state_key.clone()]),
        )
        .await;

    // The room info is not set and the invalid state event is not in the store.
    assert_eq!(room.avatar_url(), None);
    assert_matches!(room.get_state_event_static::<RoomAvatarEventContent>().await, Ok(None));
}

#[async_test]
async fn test_receive_stripped_room_name_event_via_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();

    // First we receive a valid event.
    let room_name = "My room";
    let valid_raw_event = Raw::new(&json!({
        "content": {
            "name": room_name,
        },
        "type": "m.room.name",
        "state_key": "",
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            KnockedRoomBuilder::new(room_id!("!roomwithvalidname"))
                .add_state_bulk(vec![valid_raw_event.clone()]),
        )
        .await;

    // The room info is set and the valid state event is in the store.
    assert_eq!(room.name().as_deref(), Some(room_name));
    assert_matches!(
        room.get_state_event_static::<RoomNameEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Stripped(raw_event)))
    );
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    assert_matches!(raw_event.deserialize(), Ok(_));

    // Now we receive an event with an invalid content but a valid type
    // and state key.
    let raw_event_with_invalid_content = Raw::new(&json!({
        "content": {
            // It's a boolean!
            "name": true,
        },
        "type": "m.room.name",
        "state_key": "",
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            KnockedRoomBuilder::new(room_id!("!roomwithinvalidnamecontent"))
                .add_state_bulk(vec![raw_event_with_invalid_content.clone()]),
        )
        .await;

    // The room info is not set but the invalid state event is in the store.
    assert_eq!(room.name(), None);
    assert_matches!(
        room.get_state_event_static::<RoomNameEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Stripped(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // Finally we receive an event with an invalid state key.
    let raw_event_with_invalid_state_key = Raw::new(&json!({
        "content": {
            "name": room_name,
        },
        "type": "m.room.name",
        // It's a number!
        "state_key": 1,
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            KnockedRoomBuilder::new(room_id!("!roomwithinvalidnamestatekey"))
                .add_state_bulk(vec![raw_event_with_invalid_state_key.clone()]),
        )
        .await;

    // The room info is not set and the invalid state event is not in the store.
    assert_eq!(room.name(), None);
    assert_matches!(room.get_state_event_static::<RoomNameEventContent>().await, Ok(None));
}

#[async_test]
async fn test_receive_stripped_room_create_event_via_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();

    // First we receive a valid event.
    let valid_raw_event = Raw::new(&json!({
        "content": {
            "room_version": "12",
        },
        "type": "m.room.create",
        "state_key": "",
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            KnockedRoomBuilder::new(room_id!("!roomwithvalidcreate"))
                .add_state_bulk(vec![valid_raw_event.clone()]),
        )
        .await;

    // The room info is set and the valid state event is in the store.
    assert_eq!(room.create_content().unwrap().room_version, RoomVersionId::V12);
    assert_matches!(
        room.get_state_event_static::<RoomCreateEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Stripped(raw_event)))
    );
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    assert_matches!(raw_event.deserialize(), Ok(_));

    // Now we receive an event with an invalid content but a valid type
    // and state key.
    let raw_event_with_invalid_content = Raw::new(&json!({
        "content": {
            // It's a boolean!
            "room_version": true,
        },
        "type": "m.room.create",
        "state_key": "",
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            KnockedRoomBuilder::new(room_id!("!roomwithinvalidcreatecontent"))
                .add_state_bulk(vec![raw_event_with_invalid_content.clone()]),
        )
        .await;

    // The room info is not set but the invalid state event is in the store.
    assert_matches!(room.create_content(), None);
    assert_matches!(
        room.get_state_event_static::<RoomCreateEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Stripped(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // Finally we receive an event with an invalid state key.
    let raw_event_with_invalid_state_key = Raw::new(&json!({
        "content": {
            "room_version": "12",
        },
        "type": "m.room.create",
        // It's a number!
        "state_key": 1,
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            KnockedRoomBuilder::new(room_id!("!roomwithinvalidcreatestatekey"))
                .add_state_bulk(vec![raw_event_with_invalid_state_key.clone()]),
        )
        .await;

    // The room info is not set and the invalid state event is not in the store.
    assert_matches!(room.create_content(), None);
    assert_matches!(room.get_state_event_static::<RoomCreateEventContent>().await, Ok(None));
}

#[async_test]
async fn test_receive_stripped_room_join_rules_event_via_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();

    // First we receive a valid event.
    let valid_raw_event = Raw::new(&json!({
        "content": {
            "join_rule": "public",
        },
        "type": "m.room.join_rules",
        "state_key": "",
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            InvitedRoomBuilder::new(room_id!("!roomwithvalidjoinrules"))
                .add_state_bulk(vec![valid_raw_event.clone()]),
        )
        .await;

    // The room info is set and the valid state event is in the store.
    assert_eq!(room.join_rule(), Some(JoinRule::Public));
    assert_matches!(
        room.get_state_event_static::<RoomJoinRulesEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Stripped(raw_event)))
    );
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    assert_matches!(raw_event.deserialize(), Ok(_));

    // Now we receive an event with an invalid content but a valid type
    // and state key.
    let raw_event_with_invalid_content = Raw::new(&json!({
        "content": {
            // It's a boolean!
            "join_rule": true,
        },
        "type": "m.room.join_rules",
        "state_key": "",
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            InvitedRoomBuilder::new(room_id!("!roomwithinvalidjoinrulescontent"))
                .add_state_bulk(vec![raw_event_with_invalid_content.clone()]),
        )
        .await;

    // The room info is not set but the invalid state event is in the store.
    assert_eq!(room.join_rule(), None);
    assert_matches!(
        room.get_state_event_static::<RoomJoinRulesEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Stripped(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // Finally we receive an event with an invalid state key.
    let raw_event_with_invalid_state_key = Raw::new(&json!({
        "content": {
            "join_rule": "public",
        },
        "type": "m.room.join_rules",
        // It's a number!
        "state_key": 1,
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            InvitedRoomBuilder::new(room_id!("!roomwithinvalidjoinrulesstatekey"))
                .add_state_bulk(vec![raw_event_with_invalid_state_key.clone()]),
        )
        .await;

    // The room info is not set and the invalid state event is not in the store.
    assert_eq!(room.join_rule(), None);
    assert_matches!(room.get_state_event_static::<RoomJoinRulesEventContent>().await, Ok(None));
}

#[async_test]
async fn test_receive_stripped_room_canonical_alias_event_via_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();

    // First we receive a valid event.
    let room_alias = room_alias_id!("#myroom:localhost");
    let valid_raw_event = Raw::new(&json!({
        "content": {
            "alias": room_alias,
        },
        "type": "m.room.canonical_alias",
        "state_key": "",
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            InvitedRoomBuilder::new(room_id!("!roomwithvalidcanonicalalias"))
                .add_state_bulk(vec![valid_raw_event.clone()]),
        )
        .await;

    // The room info is set and the valid state event is in the store.
    assert_eq!(room.canonical_alias().as_deref(), Some(room_alias));
    assert_matches!(
        room.get_state_event_static::<RoomCanonicalAliasEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Stripped(raw_event)))
    );
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    assert_matches!(raw_event.deserialize(), Ok(_));

    // Now we receive an event with an invalid content but a valid type
    // and state key.
    let raw_event_with_invalid_content = Raw::new(&json!({
        "content": {
            // It's a boolean!
            "alias": true,
        },
        "type": "m.room.canonical_alias",
        "state_key": "",
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            InvitedRoomBuilder::new(room_id!("!roomwithinvalidcanonicalaliascontent"))
                .add_state_bulk(vec![raw_event_with_invalid_content.clone()]),
        )
        .await;

    // The room info is not set but the invalid state event is in the store.
    assert_eq!(room.canonical_alias(), None);
    assert_matches!(
        room.get_state_event_static::<RoomCanonicalAliasEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Stripped(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // Finally we receive an event with an invalid state key.
    let raw_event_with_invalid_state_key = Raw::new(&json!({
        "content": {
            "alias": room_alias,
        },
        "type": "m.room.canonical_alias",
        // It's a number!
        "state_key": 1,
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            InvitedRoomBuilder::new(room_id!("!roomwithinvalidcanonicalaliasstatekey"))
                .add_state_bulk(vec![raw_event_with_invalid_state_key.clone()]),
        )
        .await;

    // The room info is not set and the invalid state event is not in the store.
    assert_eq!(room.canonical_alias(), None);
    assert_matches!(
        room.get_state_event_static::<RoomCanonicalAliasEventContent>().await,
        Ok(None)
    );
}

#[async_test]
async fn test_receive_stripped_room_topic_event_via_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();

    // First we receive a valid event.
    let room_topic = "A room about me, myself, and I!";
    let valid_raw_event = Raw::new(&json!({
        "content": {
            "topic": room_topic,
        },
        "type": "m.room.topic",
        "state_key": "",
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            KnockedRoomBuilder::new(room_id!("!roomwithvalidtopic"))
                .add_state_bulk(vec![valid_raw_event.clone()]),
        )
        .await;

    // The room info is set and the valid state event is in the store.
    assert_eq!(room.topic().as_deref(), Some(room_topic));
    assert_matches!(
        room.get_state_event_static::<RoomTopicEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Stripped(raw_event)))
    );
    assert_eq!(raw_event.json().get(), valid_raw_event.json().get());
    assert_matches!(raw_event.deserialize(), Ok(_));

    // Now we receive an event with an invalid content but a valid type
    // and state key.
    let raw_event_with_invalid_content = Raw::new(&json!({
        "content": {
            // It's a boolean!
            "topic": true,
        },
        "type": "m.room.topic",
        "state_key": "",
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            KnockedRoomBuilder::new(room_id!("!roomwithinvalidtopiccontent"))
                .add_state_bulk(vec![raw_event_with_invalid_content.clone()]),
        )
        .await;

    // The room info is not set but the invalid state event is in the store.
    assert_eq!(room.topic(), None);
    assert_matches!(
        room.get_state_event_static::<RoomTopicEventContent>().await,
        Ok(Some(RawSyncOrStrippedState::Stripped(raw_event)))
    );
    assert_eq!(raw_event.json().get(), raw_event_with_invalid_content.json().get());
    assert_matches!(raw_event.deserialize(), Err(_));

    // Finally we receive an event with an invalid state key.
    let raw_event_with_invalid_state_key = Raw::new(&json!({
        "content": {
            "topic": room_topic,
        },
        "type": "m.room.topic",
        // It's a number!
        "state_key": 1,
        "sender": user_id,
    }))
    .unwrap()
    .cast_unchecked::<AnyStrippedStateEvent>();

    let room = server
        .sync_room(
            &client,
            KnockedRoomBuilder::new(room_id!("!roomwithinvalidtopicstatekey"))
                .add_state_bulk(vec![raw_event_with_invalid_state_key.clone()]),
        )
        .await;

    // The room info is not set and the invalid state event is not in the store.
    assert_eq!(room.topic(), None);
    assert_matches!(room.get_state_event_static::<RoomTopicEventContent>().await, Ok(None));
}

#[async_test]
async fn test_update_active_service_members() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();
    let service_member_id_1 = owned_user_id!("@service_1:localhost");
    let service_member_id_2 = owned_user_id!("@service_2:localhost");

    let room_id = room_id!("!room:localhost");
    let service_members_event = EventFactory::new()
        .room(room_id)
        .sender(user_id)
        .member_hints(BTreeSet::from_iter(vec![
            service_member_id_1.clone(),
            service_member_id_2.clone(),
        ]))
        .into_raw_sync_state();
    let own_member_event =
        EventFactory::new().room(room_id).member(user_id).display_name("Me").into_raw_sync_state();

    // We start with just the room and our own member event, no service events
    let room = server
        .sync_room(
            &client,
            AnyRoomBuilder::Joined(
                JoinedRoomBuilder::new(room_id)
                    .add_state_event(own_member_event)
                    .add_state_event(service_members_event),
            ),
        )
        .await;

    // We check the active human and service members: no service members and a
    // single human member
    let active_members = room.members_no_sync(RoomMemberships::ACTIVE).await.unwrap();
    assert_eq!(active_members.len(), 1);
    assert_eq!(room.service_members().unwrap().len(), 2);
    assert!(room.update_active_service_members().await.unwrap().unwrap().is_empty());
    assert!(room.active_service_members_count().is_none());

    // Now another user joined the room
    let human_user = EventFactory::new()
        .room(room_id)
        .member(user_id!("@human:localhost"))
        .display_name("Human")
        .into_raw_sync_state();
    let room = server
        .sync_room(
            &client,
            AnyRoomBuilder::Joined(JoinedRoomBuilder::new(room_id).add_state_event(human_user)),
        )
        .await;

    // We check the active human and service members: no service members and 2 human
    // members
    let active_members = room.members_no_sync(RoomMemberships::ACTIVE).await.unwrap();
    assert_eq!(active_members.len(), 2);
    assert_eq!(room.service_members().unwrap().len(), 2);
    assert!(room.update_active_service_members().await.unwrap().unwrap().is_empty());
    assert!(room.active_service_members_count().is_none());

    // Now one of the service members in the member hints joined the room
    let service_member_1 =
        EventFactory::new().room(room_id).member(&service_member_id_1).into_raw_sync_state();
    let room = server
        .sync_room(
            &client,
            AnyRoomBuilder::Joined(
                JoinedRoomBuilder::new(room_id).add_state_event(service_member_1),
            ),
        )
        .await;

    // We check the active human and service members: 1 service member and 2 human
    // members
    let active_members = room.members_no_sync(RoomMemberships::ACTIVE).await.unwrap();
    assert_eq!(active_members.len(), 3);
    assert_eq!(room.service_members().unwrap().len(), 2);
    assert_eq!(room.update_active_service_members().await.unwrap().unwrap().len(), 1);
    assert_eq!(room.active_service_members_count().unwrap_or_default(), 1);

    // And a second one joins too
    let service_member_2 =
        EventFactory::new().room(room_id).member(&service_member_id_2).into_raw_sync_state();
    let room = server
        .sync_room(
            &client,
            AnyRoomBuilder::Joined(
                JoinedRoomBuilder::new(room_id).add_state_event(service_member_2),
            ),
        )
        .await;

    // We check the active human and service members: 2 service member and 2 human
    // members The active service members match the member hints
    let active_members = room.members_no_sync(RoomMemberships::ACTIVE).await.unwrap();
    assert_eq!(active_members.len(), 4);
    assert_eq!(room.service_members().unwrap().len(), 2);
    assert_eq!(room.update_active_service_members().await.unwrap().unwrap().len(), 2);
    assert_eq!(room.active_service_members_count().unwrap_or_default(), 2);

    // And now the 2nd service member leaves the room
    let service_member_2_left = EventFactory::new()
        .room(room_id)
        .member(&service_member_id_2)
        .leave()
        .into_raw_sync_state();
    let room = server
        .sync_room(
            &client,
            AnyRoomBuilder::Joined(
                JoinedRoomBuilder::new(room_id).add_state_event(service_member_2_left),
            ),
        )
        .await;

    // We check the active human and service members: 1 service member and 2 human
    // members again
    let active_members = room.members_no_sync(RoomMemberships::ACTIVE).await.unwrap();
    assert_eq!(active_members.len(), 3);
    assert_eq!(room.service_members().unwrap().len(), 2);
    assert_eq!(room.update_active_service_members().await.unwrap().unwrap().len(), 1);
    assert_eq!(room.active_service_members_count().unwrap_or_default(), 1);
}

#[async_test]
async fn test_active_service_members_resets_when_member_counts_change() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();
    let service_member_id_1 = owned_user_id!("@service_1:localhost");
    let service_member_id_2 = owned_user_id!("@service_2:localhost");

    let room_id = room_id!("!room:localhost");
    let service_members_event = EventFactory::new()
        .room(room_id)
        .sender(user_id)
        .member_hints(BTreeSet::from_iter(vec![
            service_member_id_1.clone(),
            service_member_id_2.clone(),
        ]))
        .into_raw_sync_state();
    let own_member_event =
        EventFactory::new().room(room_id).member(user_id).display_name("Me").into_raw_sync_state();
    let service_member_1 =
        EventFactory::new().room(room_id).member(&service_member_id_1).into_raw_sync_state();
    let service_member_2 =
        EventFactory::new().room(room_id).member(&service_member_id_2).into_raw_sync_state();

    // We start with just the room and our own member event, no service events
    let room = server
        .sync_room(
            &client,
            AnyRoomBuilder::Joined(
                JoinedRoomBuilder::new(room_id)
                    .add_state_event(own_member_event)
                    .add_state_event(service_member_1)
                    .add_state_event(service_member_2)
                    .add_state_event(service_members_event),
            ),
        )
        .await;

    let active_members = room.members_no_sync(RoomMemberships::ACTIVE).await.unwrap();
    assert_eq!(active_members.len(), 3);
    assert_eq!(room.service_members().unwrap().len(), 2);

    // We got some computed values after update_active_service_members
    assert_eq!(room.update_active_service_members().await.unwrap().unwrap().len(), 2);
    assert_eq!(room.active_service_members_count().unwrap(), 2);

    // But then the member counts change
    let room = server
        .sync_room(
            &client,
            AnyRoomBuilder::Joined(JoinedRoomBuilder::new(room_id).set_joined_members_count(32)),
        )
        .await;

    // And the active service members should be reset to None
    assert!(room.active_service_members_count().is_none());
}

#[async_test]
async fn test_cached_active_service_members_resets_when_member_counts_change() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();
    let service_member_id_1 = owned_user_id!("@service_1:localhost");
    let service_member_id_2 = owned_user_id!("@service_2:localhost");

    let room_id = room_id!("!room:localhost");
    let service_members_event = EventFactory::new()
        .room(room_id)
        .sender(user_id)
        .member_hints(BTreeSet::from_iter(vec![
            service_member_id_1.clone(),
            service_member_id_2.clone(),
        ]))
        .into_raw_sync_state();
    let own_member_event =
        EventFactory::new().room(room_id).member(user_id).display_name("Me").into_raw_sync_state();
    let service_member_1 =
        EventFactory::new().room(room_id).member(&service_member_id_1).into_raw();
    let service_member_2 =
        EventFactory::new().room(room_id).member(&service_member_id_2).into_raw();

    // We start with just the room and our own member event, no service events
    let room = server
        .sync_room(
            &client,
            AnyRoomBuilder::Joined(
                JoinedRoomBuilder::new(room_id)
                    .add_state_event(own_member_event)
                    .add_state_event(service_member_1)
                    .add_state_event(service_member_2)
                    .add_state_event(service_members_event),
            ),
        )
        .await;

    let active_members = room.members_no_sync(RoomMemberships::ACTIVE).await.unwrap();
    assert_eq!(active_members.len(), 3);
    assert_eq!(room.service_members().unwrap().len(), 2);

    // We got some computed values after update_active_service_members
    assert_eq!(room.update_active_service_members().await.unwrap().unwrap().len(), 2);
    assert_eq!(room.active_service_members_count().unwrap(), 2);

    // But then the member counts change
    let room = server
        .sync_room(
            &client,
            AnyRoomBuilder::Joined(JoinedRoomBuilder::new(room_id).set_joined_members_count(32)),
        )
        .await;

    // And the active service members should be reset to None
    assert!(room.active_service_members_count().is_none());
}

#[async_test]
async fn test_cached_active_service_members_updates_on_sync_members() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();
    let service_member_id_1 = owned_user_id!("@service_1:localhost");
    let service_member_id_2 = owned_user_id!("@service_2:localhost");

    let room_id = room_id!("!room:localhost");
    let service_members_event = EventFactory::new()
        .room(room_id)
        .sender(user_id)
        .member_hints(BTreeSet::from_iter(vec![
            service_member_id_1.clone(),
            service_member_id_2.clone(),
        ]))
        .into_raw_sync_state();
    let own_member_event =
        EventFactory::new().room(room_id).member(user_id).display_name("Me").into_raw_sync_state();
    let service_member_1 = EventFactory::new()
        .room(room_id)
        .member(&service_member_id_1)
        .into_raw::<RoomMemberEvent>();
    let service_member_2 = EventFactory::new()
        .room(room_id)
        .member(&service_member_id_2)
        .into_raw::<RoomMemberEvent>();

    // We start with just the room and our own member event, no service events
    let room = server
        .sync_room(
            &client,
            AnyRoomBuilder::Joined(
                JoinedRoomBuilder::new(room_id)
                    .add_state_event(own_member_event)
                    .add_state_event(service_members_event),
            ),
        )
        .await;

    let active_members = room.members_no_sync(RoomMemberships::ACTIVE).await.unwrap();
    assert_eq!(active_members.len(), 1);
    assert_eq!(room.service_members().unwrap().len(), 2);

    // We don't have a computed value for active service members yet
    assert!(room.active_service_members_count().is_none());

    // But if we now sync the room members, the active service members should be
    // updated
    server
        .mock_get_members()
        .ok(vec![service_member_1, service_member_2])
        .mock_once()
        .mount()
        .await;

    room.sync_members().await.expect("sync_members");

    // We got some computed values after sync_members
    assert!(room.active_service_members_count().is_some());
    assert_eq!(room.active_service_members_count().unwrap(), 2);
}
