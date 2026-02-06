use assert_matches2::assert_matches;
use matrix_sdk::{
    deserialized_responses::RawSyncOrStrippedState, test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_test::{JoinedRoomBuilder, async_test};
use ruma::{
    EventEncryptionAlgorithm, MilliSecondsSinceUnixEpoch, RoomVersionId, event_id,
    events::{
        AnySyncStateEvent, SyncStateEvent,
        room::{
            avatar::RoomAvatarEventContent,
            canonical_alias::RoomCanonicalAliasEventContent,
            create::RoomCreateEventContent,
            encryption::RoomEncryptionEventContent,
            guest_access::{GuestAccess, RoomGuestAccessEventContent},
            history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
            join_rules::RoomJoinRulesEventContent,
            name::RoomNameEventContent,
            pinned_events::RoomPinnedEventsEventContent,
            power_levels::RoomPowerLevelsEventContent,
            tombstone::RoomTombstoneEventContent,
            topic::RoomTopicEventContent,
        },
    },
    mxc_uri,
    room::JoinRule,
    room_alias_id, room_id,
    serde::Raw,
};
use serde_json::json;
use stream_assert::{assert_pending, assert_ready};

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
        EventEncryptionAlgorithm::MegolmV1AesSha2
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
        EventEncryptionAlgorithm::MegolmV1AesSha2
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
        EventEncryptionAlgorithm::MegolmV1AesSha2
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
    assert_eq!(room.name(), None);
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
    assert_eq!(room.name(), None);
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
    assert_eq!(room.tombstone_content().unwrap().replacement_room, tombstone_replacement);
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
