use std::{assert_matches, collections::BTreeMap, ops::Not as _, pin::pin, time::Duration};

use eyeball_im::VectorDiff;
use futures_util::{FutureExt, StreamExt};
use matrix_sdk::{
    Error, MemoryStore, SlidingSyncList, StateChanges, StateStore, ThreadingSupport,
    assert_let_timeout,
    authentication::oauth::{OAuthError, error::OAuthTokenRevocationError},
    config::{RequestConfig, StoreConfig},
    event_cache::RoomEventCacheUpdate,
    live_locations_observer::BeaconInfoUpdate,
    sleep::sleep,
    store::{RoomLoadSettings, ThreadSubscriptionStatus},
    sync::{RoomUpdate, State},
    test_utils::{client::mock_matrix_session, mocks::MatrixMockServer, test_client_builder},
};
use matrix_sdk_base::{RoomState, read_receipts::ReadReceipts, sync::RoomUpdates};
use matrix_sdk_common::{cross_process_lock::CrossProcessLockConfig, executor::spawn};
use matrix_sdk_test::{
    InvitedRoomBuilder, JoinedRoomBuilder, KnockedRoomBuilder, LeftRoomBuilder, TestDevice,
    TestRoom, User, async_test, event_factory::EventFactory,
};
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, RoomVersionId,
    api::client::{
        directory::{
            get_public_rooms,
            get_public_rooms_filtered::{self, v3::Request as PublicRoomsFilterRequest},
        },
        sync::sync_events::msc4186,
        threads::get_thread_subscriptions_changes::msc4308::{
            ThreadSubscription, ThreadUnsubscription,
        },
        uiaa,
    },
    assign,
    directory::Filter,
    event_id,
    events::{
        AnyRoomAccountDataEvent, GlobalAccountDataEventType,
        direct::DirectEventContent,
        room::{history_visibility::HistoryVisibility, member::MembershipState},
        tag::TagName,
    },
    owned_event_id, owned_mxc_uri,
    room::JoinRule,
    room_id,
    serde::Raw,
    uint,
};
use serde_json::json;
use strass::{assert_let, assert_variant_eq};
use stream_assert::{assert_next_matches, assert_pending};
use tempfile::tempdir;
#[cfg(feature = "sqlite")]
use tokio::time::timeout;
use tokio_stream::wrappers::BroadcastStream;

#[async_test]
async fn test_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server
        .mock_sync()
        .ok(|builder| {
            builder.add_joined_room(JoinedRoomBuilder::default());
        })
        .expect(1)
        .mount()
        .await;

    let response = client.sync_once(Default::default()).await.unwrap();

    assert_ne!(response.next_batch, "");
}

#[async_test]
async fn test_devices() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_devices().ok().expect(1).mount().await;

    client.devices().await.unwrap();
}

#[async_test]
async fn test_delete_devices() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    // The first request will require UIAA.
    server.mock_delete_devices().error_uiaa_password().mock_once().mount().await;

    // The second request containing the auth data will succeed.
    let auth_data = uiaa::AuthData::Password(assign!(
        uiaa::Password::new(
            uiaa::UserIdentifier::Matrix(uiaa::MatrixUserIdentifier::new(User::Alice.into())),
            "wordpass".to_owned(),
        ), {
            session: Some("oFIJVvtEOCKmRUTYKTYIIPHL".to_owned()),
        }
    ));
    server.mock_delete_devices().expect_uiaa_auth_data(&auth_data).ok().mock_once().mount().await;

    let devices = &[TestDevice::Default.into()];

    // The first request fails with a UIAA response.
    assert_let!(Err(e) = client.delete_devices(devices, None).await);
    assert_matches!(e.as_uiaa_response(), Some(_));

    // The second request succeeds.
    client.delete_devices(devices, Some(auth_data)).await.unwrap();
}

#[async_test]
async fn test_resolve_room_alias() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server
        .mock_room_directory_resolve_alias()
        .ok(TestRoom::Default.as_str(), vec![])
        .expect(1)
        .mount()
        .await;

    let alias = ruma::room_alias_id!("#alias:example.org");
    client.resolve_room_alias(alias).await.unwrap();
}

#[async_test]
async fn test_join_leave_room() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room = client.get_room(TestRoom::Default.into());
    assert!(room.is_none());

    // Sync the room as joined.
    let room = server.sync_default_joined_room(&client).await;
    assert_eq!(room.state(), RoomState::Joined);

    // Sync the room as left.
    server.sync_room(&client, LeftRoomBuilder::default()).await;

    assert_eq!(room.state(), RoomState::Left);
    let room = client.get_room(TestRoom::Default.into()).unwrap();
    assert_eq!(room.state(), RoomState::Left);
}

#[async_test]
async fn test_join_room_by_id() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = TestRoom::Default;

    server.mock_room_join(room_id.into()).ok().expect(1).mount().await;

    assert_eq!(
        // this is the `join_by_room_id::Response` but since no PartialEq we
        // check the RoomId field
        client.join_room_by_id(room_id.into()).await.unwrap().room_id(),
        room_id
    );
}

#[async_test]
async fn test_join_room_by_id_or_alias() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = TestRoom::Default;

    server.mock_room_join_by_id_or_alias(room_id).ok(room_id).expect(1).mount().await;

    assert_eq!(
        // this is the `join_by_room_id::Response` but since no PartialEq we
        // check the RoomId field
        client
            .join_room_by_id_or_alias(room_id.into(), &["server.com".try_into().unwrap()])
            .await
            .unwrap()
            .room_id(),
        room_id
    );
}

#[async_test]
async fn test_room_search_all() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_get_all_public_rooms().ok_default().expect(1).mount().await;

    let get_public_rooms::v3::Response { chunk, .. } =
        client.public_rooms(Some(10), None, None).await.unwrap();
    assert_eq!(chunk.len(), 1);
}

#[async_test]
async fn test_room_search_filtered() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_get_filtered_public_rooms().ok_default().expect(1).mount().await;

    let generic_search_term = Some("cheese".to_owned());
    let filter = assign!(Filter::new(), { generic_search_term });
    let request = assign!(PublicRoomsFilterRequest::new(), { filter });

    let get_public_rooms_filtered::v3::Response { chunk, .. } =
        client.public_rooms_filtered(request).await.unwrap();
    assert_eq!(chunk.len(), 1);
}

#[async_test]
async fn test_invited_rooms() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room = server.sync_room(&client, InvitedRoomBuilder::default()).await;
    assert_eq!(room.state(), RoomState::Invited);

    assert!(client.joined_rooms().is_empty());
    assert!(client.left_rooms().is_empty());
    assert!(!client.invited_rooms().is_empty());
    assert!(client.joined_space_rooms().is_empty());
}

#[async_test]
async fn test_left_rooms() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room = server.sync_room(&client, LeftRoomBuilder::default()).await;
    assert_eq!(room.state(), RoomState::Left);

    assert!(client.joined_rooms().is_empty());
    assert!(!client.left_rooms().is_empty());
    assert!(client.invited_rooms().is_empty());
    assert!(client.joined_space_rooms().is_empty());
}

#[async_test]
async fn test_joined_space_rooms() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let f = EventFactory::new();

    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_state_event(f.create(User::Alice, RoomVersionId::V10).with_space_type()),
        )
        .await;
    assert_eq!(room.state(), RoomState::Joined);
    assert!(room.is_space());

    assert!(!client.joined_rooms().is_empty());
    assert!(!client.joined_space_rooms().is_empty());

    assert!(client.left_rooms().is_empty());
    assert!(client.invited_rooms().is_empty());
}

#[async_test]
async fn test_whoami() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_who_am_i().ok().expect(1).mount().await;

    assert_eq!(client.whoami().await.unwrap().user_id, User::Alice);
}

#[async_test]
async fn test_room_update_channel() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let f = EventFactory::new().sender(User::Alice);

    let prev_batch = "t392-516_47314_0_7_1_1_1_11444_1";

    let mut rx = client.subscribe_to_room_updates(TestRoom::Default.into());

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_account_data(f.marked_unread(false))
                .add_typing(f.typing([User::Bob]))
                .add_state_bulk([
                    f.create(User::Alice, RoomVersionId::V11).into(),
                    f.member(User::Alice).into(),
                    f.room_join_rules(JoinRule::Public).into(),
                    f.member(User::Bob).sender(User::Bob).into(),
                ])
                .add_timeline_event(f.text_msg("That was funny!"))
                .set_timeline_limited()
                .set_timeline_prev_batch(prev_batch)
                .set_unread_notifications_count(json!({ "notification_count": 11 })),
        )
        .await;

    let update = rx.recv().now_or_never().unwrap().unwrap();
    assert_let!(RoomUpdate::Joined { updates, .. } = update);

    assert_eq!(updates.account_data.len(), 1);
    assert_eq!(updates.ephemeral.len(), 1);
    assert_let!(State::Before(state_events) = updates.state);
    assert_eq!(state_events.len(), 4);

    assert!(updates.timeline.limited);
    assert_eq!(updates.timeline.events.len(), 1);
    assert_variant_eq!(updates.timeline.prev_batch, Some(prev_batch));

    assert_eq!(updates.unread_notifications.highlight_count, 0);
    assert_eq!(updates.unread_notifications.notification_count, 11);
}

#[async_test]
async fn test_subscribe_all_room_updates() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let f = EventFactory::new().sender(User::Alice);

    let left_room_id = TestRoom::Left;
    let joined_room_id = TestRoom::Default;
    let joined_room_prev_batch = "t392-516_47314_0_7_1_1_1_11444_1";
    let invited_room_id = TestRoom::Invited;
    let knocked_room_id = TestRoom::V12;

    let mut rx = client.subscribe_to_all_room_updates();

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder
                .add_left_room(
                    LeftRoomBuilder::new(left_room_id).add_timeline_event(f.text_msg("Goodbye!")),
                )
                .add_joined_room(
                    JoinedRoomBuilder::default()
                        .add_account_data(f.marked_unread(false))
                        .add_typing(f.typing([User::Bob]))
                        .add_state_event(f.member(User::Bob).sender(User::Bob))
                        .add_timeline_event(f.text_msg("Hello Bob!"))
                        .set_timeline_limited()
                        .set_timeline_prev_batch(joined_room_prev_batch)
                        .set_unread_notifications_count(json!({ "notification_count": 11 })),
                )
                .add_invited_room(InvitedRoomBuilder::new(invited_room_id).add_state_bulk([
                    f.create(User::Carol, RoomVersionId::V11).sender(User::Carol).into(),
                    f.member(User::Carol).sender(User::Carol).into(),
                ]))
                .add_knocked_room(KnockedRoomBuilder::new(knocked_room_id).add_state_bulk([
                    f.create(User::Bob, RoomVersionId::V12).sender(User::Bob).into(),
                    f.member(User::Bob).sender(User::Carol).into(),
                ]));
        })
        .await;

    let room_updates = rx.recv().now_or_never().unwrap().unwrap();
    assert_let!(RoomUpdates { left, joined, invited, knocked } = room_updates);

    // Check the left room updates.
    {
        assert_eq!(left.len(), 1);

        let (room_id, update) = left.iter().next().unwrap();

        assert_eq!(room_id, left_room_id);
        assert_let!(State::Before(state_events) = &update.state);
        assert!(state_events.is_empty());
        assert_eq!(update.timeline.events.len(), 1);
        assert!(update.account_data.is_empty());
    }

    // Check the joined room updates.
    {
        assert_eq!(joined.len(), 1);

        let (room_id, update) = joined.iter().next().unwrap();

        assert_eq!(room_id, joined_room_id);

        assert_eq!(update.account_data.len(), 1);
        assert_eq!(update.ephemeral.len(), 1);
        assert_let!(State::Before(state_events) = &update.state);
        assert_eq!(state_events.len(), 1);

        assert!(update.timeline.limited);
        assert_eq!(update.timeline.events.len(), 1);
        assert_variant_eq!(update.timeline.prev_batch, Some(joined_room_prev_batch));

        assert_eq!(update.unread_notifications.highlight_count, 0);
        assert_eq!(update.unread_notifications.notification_count, 11);
    }

    // Check the invited room updates.
    {
        assert_eq!(invited.len(), 1);

        let (room_id, update) = invited.iter().next().unwrap();

        assert_eq!(room_id, invited_room_id);
        assert_eq!(update.invite_state.events.len(), 2);
    }

    // Check the knocked room updates.
    {
        assert_eq!(knocked.len(), 1);

        let (room_id, update) = knocked.iter().next().unwrap();

        assert_eq!(room_id, knocked_room_id);
        assert_eq!(update.knock_state.events.len(), 2);
    }
}

// Check that the `Room::latest_encryption_state().await?.is_encrypted()` is
// properly deduplicated, meaning we only make a single request to the server,
// and that multiple calls do return the same result.
#[cfg(all(feature = "e2e-encryption", not(target_family = "wasm")))]
#[async_test]
async fn test_request_encryption_event_before_sending() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room = server.sync_default_joined_room(&client).await;

    server
        .mock_room_state_encryption()
        // Introduce a delay so the first `latest_encryption_state()`
        // doesn't finish before we make the second call.
        .with_delay(Duration::from_millis(50))
        .encrypted()
        .expect(1)
        .mount()
        .await;

    let first_handle = spawn({
        let room = room.clone();
        async move { room.latest_encryption_state().await.map(|state| state.is_encrypted()) }
    });

    let second_handle =
        spawn(
            async move { room.latest_encryption_state().await.map(|state| state.is_encrypted()) },
        );

    let first_encrypted =
        first_handle.await.unwrap().expect("We should be able to test if the room is encrypted.");
    let second_encrypted =
        second_handle.await.unwrap().expect("We should be able to test if the room is encrypted.");

    assert!(first_encrypted, "We should have found out that the room is encrypted.");
    assert_eq!(
        first_encrypted, second_encrypted,
        "Both attempts to find out if the room is encrypted should return the same result."
    );
}

// Check that we're fetching account data from the server when marking a room as
// a DM.
#[async_test]
async fn test_marking_room_as_dm() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().expect("user is logged in");
    let f = EventFactory::new();

    let account_data = client
        .account()
        .account_data::<DirectEventContent>()
        .await
        .expect("We should be able to fetch the account data event from the store");

    assert!(account_data.is_none(), "We should not have any account data initially");

    server
        .mock_get_global_account_data(user_id, &GlobalAccountDataEventType::Direct)
        .ok(f
            .direct()
            .add_user(User::Bob, TestRoom::Left)
            .add_user(User::Bob, TestRoom::V12)
            .add_user(User::Carol, TestRoom::V12)
            .into_content())
        .expect(1..)
        .named("m.direct account data GET")
        .mount()
        .await;

    server
        .mock_set_global_account_data(user_id, &GlobalAccountDataEventType::Direct)
        .expect_account_data(
            f.direct()
                .add_user(User::Bob, TestRoom::Left)
                .add_user(User::Bob, TestRoom::V12)
                .add_user(User::Bob, TestRoom::Default)
                .add_user(User::Carol, TestRoom::V12)
                .into_content(),
        )
        .ok()
        .expect(1..)
        .named("m.direct account data PUT")
        .mount()
        .await;

    client
        .account()
        .mark_as_dm(TestRoom::Default.into(), &[User::Bob.into()])
        .await
        .expect("We should be able to mark the room as a DM");
}

// Check that we're fetching account data from the server when marking a room as
// a DM, and that we don't clobber the previous entry if it was impossible to
// deserialize.
#[async_test]
async fn test_marking_room_as_dm_fails_if_undeserializable() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().expect("client is logged in");

    let account_data = client
        .account()
        .account_data::<DirectEventContent>()
        .await
        .expect("We should be able to fetch the account data event from the store");

    assert!(account_data.is_none(), "We should not have any account data initially");

    // The response must be valid JSON, but not a valid `DirectEventContent`
    // representation.
    server
        .mock_get_global_account_data(user_id, &GlobalAccountDataEventType::Direct)
        .ok_json(json!(["hey", null, true, 42]))
        .expect(1)
        .named("m.direct account data GET")
        .mount()
        .await;

    let result = client.account().mark_as_dm(TestRoom::Default.into(), &[User::Bob.into()]).await;

    assert_matches!(result, Err(Error::SerdeJson(_)));
}

#[cfg(feature = "e2e-encryption")]
#[async_test]
async fn test_get_own_device() {
    use matrix_sdk::test_utils::client::MockClientBuilder;

    let client = MockClientBuilder::new(None).build().await;

    let device = client
        .encryption()
        .get_own_device()
        .await
        .unwrap()
        .expect("We should always have access to our own device, even before any sync");

    assert!(
        device.user_id() == client.user_id().expect("The client should know about its user ID"),
        "The user ID of the client and our own device should match"
    );
    assert!(
        device.device_id()
            == client.device_id().expect("The client should know about its device ID"),
        "The device ID of the client and our own device should match"
    );
}

#[cfg(feature = "e2e-encryption")]
#[async_test]
async fn test_cross_signing_status() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_upload_cross_signing_keys().ok().expect(1).mount().await;

    server.mock_upload_cross_signing_signatures().ok().expect(1).mount().await;

    server.mock_upload_keys().ok().expect(1).mount().await;

    let status = client
        .encryption()
        .cross_signing_status()
        .await
        .expect("We should be able to fetch our cross-signing status");

    assert!(
        !status.has_master && !status.has_self_signing && !status.has_user_signing,
        "Initially we shouldn't have any cross-signing keys"
    );

    client.encryption().bootstrap_cross_signing(None).await.unwrap();

    client
        .encryption()
        .cross_signing_status()
        .await
        .expect("We should be able to fetch our cross-signing status");

    let status = client
        .encryption()
        .cross_signing_status()
        .await
        .expect("We should have the private cross-signing keys after the bootstrap process");
    assert!(status.is_complete(), "We should have all the private cross-signing keys locally");
}

#[cfg(feature = "e2e-encryption")]
#[async_test]
async fn test_encrypt_room_event() {
    use ruma::events::{MessageLikeEventType, room::encrypted::OriginalSyncRoomEncryptedEvent};

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();

    server.mock_query_keys().ok().expect(1..).mount().await;

    let room = server.sync_default_joined_room(&client).await;

    server.mock_room_state_encryption().encrypted().expect(1).mount().await;

    assert!(
        room.latest_encryption_state()
            .await
            .expect("We should be able to check if the room is encrypted")
            .is_encrypted(),
        "The room should be encrypted"
    );

    server.mock_get_members().ok(vec![]).expect(1).mount().await;

    let (event_receiver, mock) = server
        .mock_room_send()
        .for_type(MessageLikeEventType::RoomEncrypted)
        .ok_with_capture(owned_event_id!("$foobar"), user_id);
    mock.expect(1).mount().await;

    room.send_raw("m.room.message", json!({"body": "Hello", "msgtype": "m.text"}))
        .with_transaction_id("foobar".into())
        .await
        .expect("We should be able to send a message to the encrypted room");

    let encrypted_event =
        event_receiver.await.unwrap().cast_unchecked::<OriginalSyncRoomEncryptedEvent>();

    let push_ctx =
        room.push_context().await.expect("We should be able to get the push action context");
    let timeline_event = room
        .decrypt_event(&encrypted_event, push_ctx.as_ref())
        .await
        .expect("We should be able to decrypt an event that we ourselves have encrypted");

    let event = timeline_event
        .raw()
        .deserialize()
        .expect("We should be able to deserialize the decrypted event");

    assert_let!(
        ruma::events::AnySyncTimelineEvent::MessageLike(
            ruma::events::AnySyncMessageLikeEvent::RoomMessage(message_event)
        ) = event
    );

    let message_event =
        message_event.as_original().expect("The decrypted event should not be a redacted event");

    assert_eq!(
        message_event.content.body(),
        "Hello",
        "The now decrypted message should match to our plaintext payload"
    );
}

#[cfg(not(feature = "e2e-encryption"))]
#[async_test]
async fn test_create_dm_non_encrypted() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = User::Carol;

    server
        .mock_create_room()
        .body_json(json!({
            "is_direct": true,
            "preset": "trusted_private_chat",
            "invite": [user_id],
        }))
        .ok()
        .expect(1)
        .mount()
        .await;

    client.create_dm(user_id.into()).await.unwrap();
}

#[cfg(feature = "e2e-encryption")]
#[async_test]
async fn test_create_dm_encrypted() {
    use ruma::events::room::encryption::RoomEncryptionEventContent;

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = User::Carol;

    server
        .mock_create_room()
        .body_json(json!({
            "is_direct": true,
            "preset": "trusted_private_chat",
            "invite": [user_id],
            "initial_state": [{
                "type": "m.room.encryption",
                "state_key": "",
                "content": RoomEncryptionEventContent::with_recommended_defaults(),
            }]
        }))
        .ok()
        .expect(1)
        .mount()
        .await;

    client.create_dm(user_id.into()).await.unwrap();
}

#[async_test]
async fn test_create_dm_error() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = User::Carol;

    // The endpoint is not mocked so we encounter a 404.
    let error = client.create_dm(user_id.into()).await.unwrap_err();
    let client_api_error = error.as_client_api_error().unwrap();

    assert_eq!(client_api_error.status_code, 404);
}

#[async_test]
async fn test_ambiguity_changes() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let example_id = User::Alice;
    let example_2_id = User::Bob;
    let example_3_id = User::Carol;
    let room_id = TestRoom::Default;
    let f = EventFactory::new();

    let mut updates = BroadcastStream::new(client.subscribe_to_room_updates(room_id.into()));
    assert_pending!(updates);

    // Initial sync, adds 2 members.
    let example_1_join_event_id = event_id!("$example_1_join");
    let example_2_join_event_id = event_id!("$example_2_join");
    let response = server
        .mock_sync()
        .ok_and_run_with_response(&client, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::default().add_state_bulk([
                    f.member(example_id)
                        .display_name("example")
                        .event_id(example_1_join_event_id)
                        .into(),
                    f.member(example_2_id)
                        .display_name("example 2")
                        .event_id(example_2_join_event_id)
                        .into(),
                ]),
            );
        })
        .await;

    let room = client.get_room(room_id.into()).unwrap();

    let changes = &response.rooms.joined.get(room_id.as_str()).unwrap().ambiguity_changes;

    // A new member always triggers an ambiguity change.
    let example_change = changes.get(example_1_join_event_id).unwrap();
    assert_eq!(example_change.member_id, example_id);
    assert!(!example_change.member_ambiguous);
    assert_eq!(example_change.ambiguated_member, None);
    assert_eq!(example_change.disambiguated_member, None);

    let example_2_change = changes.get(example_2_join_event_id).unwrap();
    assert_eq!(example_2_change.member_id, example_2_id);
    assert!(!example_2_change.member_ambiguous);
    assert_eq!(example_2_change.ambiguated_member, None);
    assert_eq!(example_2_change.disambiguated_member, None);

    let example = room.get_member_no_sync(example_id.into()).await.unwrap().unwrap();
    assert!(!example.name_ambiguous());
    let example_2 = room.get_member_no_sync(example_2_id.into()).await.unwrap().unwrap();
    assert!(!example_2.name_ambiguous());

    let changes = assert_next_matches!(updates, Ok(RoomUpdate::Joined { updates, .. }) => updates.ambiguity_changes);

    let example_change = changes.get(example_1_join_event_id).unwrap();
    assert_eq!(example_change.member_id, example_id);
    assert!(!example_change.member_ambiguous);
    assert_eq!(example_change.ambiguated_member, None);
    assert_eq!(example_change.disambiguated_member, None);

    let example_2_change = changes.get(example_2_join_event_id).unwrap();
    assert_eq!(example_2_change.member_id, example_2_id);
    assert!(!example_2_change.member_ambiguous);
    assert_eq!(example_2_change.ambiguated_member, None);
    assert_eq!(example_2_change.disambiguated_member, None);

    // Add 1 member and set all 3 to the same display name.
    let example_2_rename_1_event_id = event_id!("$example_2_rename_1");
    let example_3_join_event_id = event_id!("$example_3_join");

    let response = server
        .mock_sync()
        .ok_and_run_with_response(&client, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::default().add_state_bulk([
                    f.member(example_2_id)
                        .display_name("example")
                        .event_id(example_2_rename_1_event_id)
                        .into(),
                    f.member(example_3_id)
                        .display_name("example")
                        .event_id(example_3_join_event_id)
                        .into(),
                ]),
            );
        })
        .await;

    let changes = &response.rooms.joined.get(room_id.as_str()).unwrap().ambiguity_changes;

    // First joined member made both members ambiguous.
    let example_2_change = changes.get(example_2_rename_1_event_id).unwrap();
    assert_eq!(example_2_change.member_id, example_2_id);
    assert!(example_2_change.member_ambiguous);
    assert_variant_eq!(example_2_change.ambiguated_member, Some(example_id));
    assert_eq!(example_2_change.disambiguated_member, None);

    // Second joined member only adds itself as ambiguous.
    let example_3_change = changes.get(example_3_join_event_id).unwrap();
    assert_eq!(example_3_change.member_id, example_3_id);
    assert!(example_3_change.member_ambiguous);
    assert_eq!(example_3_change.ambiguated_member, None);
    assert_eq!(example_3_change.disambiguated_member, None);

    let example = room.get_member_no_sync(example_id.into()).await.unwrap().unwrap();
    assert!(example.name_ambiguous());
    let example_2 = room.get_member_no_sync(example_2_id.into()).await.unwrap().unwrap();
    assert!(example_2.name_ambiguous());
    let example_3 = room.get_member_no_sync(example_3_id.into()).await.unwrap().unwrap();
    assert!(example_3.name_ambiguous());

    let changes = assert_next_matches!(updates, Ok(RoomUpdate::Joined { updates, .. }) => updates.ambiguity_changes);

    let example_2_change = changes.get(example_2_rename_1_event_id).unwrap();
    assert_eq!(example_2_change.member_id, example_2_id);
    assert!(example_2_change.member_ambiguous);
    assert_variant_eq!(example_2_change.ambiguated_member, Some(example_id));
    assert_eq!(example_2_change.disambiguated_member, None);

    let example_3_change = changes.get(example_3_join_event_id).unwrap();
    assert_eq!(example_3_change.member_id, example_3_id);
    assert!(example_3_change.member_ambiguous);
    assert_eq!(example_3_change.ambiguated_member, None);
    assert_eq!(example_3_change.disambiguated_member, None);

    // Rename example 2 to a unique name.
    let example_2_rename_2_event_id = event_id!("$example_2_rename_2");

    let response = server
        .mock_sync()
        .ok_and_run_with_response(&client, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::default().add_state_event(
                    f.member(example_2_id)
                        .display_name("another example")
                        .event_id(example_2_rename_2_event_id),
                ),
            );
        })
        .await;

    let changes = &response.rooms.joined.get(room_id.as_str()).unwrap().ambiguity_changes;

    // example 2 is not ambiguous anymore.
    let example_2_change = changes.get(example_2_rename_2_event_id).unwrap();
    assert_eq!(example_2_change.member_id, example_2_id);
    assert!(!example_2_change.member_ambiguous);
    assert_eq!(example_2_change.ambiguated_member, None);
    assert_eq!(example_2_change.disambiguated_member, None);

    let example = room.get_member_no_sync(example_id.into()).await.unwrap().unwrap();
    assert!(example.name_ambiguous());
    let example_2 = room.get_member_no_sync(example_2_id.into()).await.unwrap().unwrap();
    assert!(!example_2.name_ambiguous());
    let example_3 = room.get_member_no_sync(example_3_id.into()).await.unwrap().unwrap();
    assert!(example_3.name_ambiguous());

    let changes = assert_next_matches!(updates, Ok(RoomUpdate::Joined { updates, .. }) => updates.ambiguity_changes);

    let example_2_change = changes.get(example_2_rename_2_event_id).unwrap();
    assert_eq!(example_2_change.member_id, example_2_id);
    assert!(!example_2_change.member_ambiguous);
    assert_eq!(example_2_change.ambiguated_member, None);
    assert_eq!(example_2_change.disambiguated_member, None);

    // Rename example 3, using the same name as example 2.
    let example_3_rename_event_id = event_id!("$example_3_rename");

    let response = server
        .mock_sync()
        .ok_and_run_with_response(&client, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::default().add_state_event(
                    f.member(example_3_id)
                        .display_name("another example")
                        .event_id(example_3_rename_event_id),
                ),
            );
        })
        .await;

    let changes = &response.rooms.joined.get(room_id.as_str()).unwrap().ambiguity_changes;

    // example 3 is now ambiguous with example 2, not example.
    let example_3_change = changes.get(example_3_rename_event_id).unwrap();
    assert_eq!(example_3_change.member_id, example_3_id);
    assert!(example_3_change.member_ambiguous);
    assert_variant_eq!(example_3_change.ambiguated_member, Some(example_2_id));
    assert_variant_eq!(example_3_change.disambiguated_member, Some(example_id));

    let example = room.get_member_no_sync(example_id.into()).await.unwrap().unwrap();
    assert!(!example.name_ambiguous());
    let example_2 = room.get_member_no_sync(example_2_id.into()).await.unwrap().unwrap();
    assert!(example_2.name_ambiguous());
    let example_3 = room.get_member_no_sync(example_3_id.into()).await.unwrap().unwrap();
    assert!(example_3.name_ambiguous());

    let changes = assert_next_matches!(updates, Ok(RoomUpdate::Joined { updates, .. }) => updates.ambiguity_changes);

    let example_3_change = changes.get(example_3_rename_event_id).unwrap();
    assert_eq!(example_3_change.member_id, example_3_id);
    assert!(example_3_change.member_ambiguous);
    assert_variant_eq!(example_3_change.ambiguated_member, Some(example_2_id));
    assert_variant_eq!(example_3_change.disambiguated_member, Some(example_id));

    // Rename example, still using a unique name.
    let example_rename_event_id = event_id!("$example_rename");

    let response = server
        .mock_sync()
        .ok_and_run_with_response(&client, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::default().add_state_event(
                    f.member(example_id)
                        .display_name("the first example")
                        .event_id(example_rename_event_id),
                ),
            );
        })
        .await;

    let changes = &response.rooms.joined.get(room_id.as_str()).unwrap().ambiguity_changes;

    // name change, even if still not ambiguous, triggers ambiguity change.
    let example_change = changes.get(example_rename_event_id).unwrap();
    assert_eq!(example_change.member_id, example_id);
    assert!(!example_change.member_ambiguous);
    assert_eq!(example_change.ambiguated_member, None);
    assert_eq!(example_change.disambiguated_member, None);

    let example = room.get_member_no_sync(example_id.into()).await.unwrap().unwrap();
    assert!(!example.name_ambiguous());
    let example_2 = room.get_member_no_sync(example_2_id.into()).await.unwrap().unwrap();
    assert!(example_2.name_ambiguous());
    let example_3 = room.get_member_no_sync(example_3_id.into()).await.unwrap().unwrap();
    assert!(example_3.name_ambiguous());

    let changes = assert_next_matches!(updates, Ok(RoomUpdate::Joined { updates, .. }) => updates.ambiguity_changes);

    let example_change = changes.get(example_rename_event_id).unwrap();
    assert_eq!(example_change.member_id, example_id);
    assert!(!example_change.member_ambiguous);
    assert_eq!(example_change.ambiguated_member, None);
    assert_eq!(example_change.disambiguated_member, None);

    // Change avatar.
    let example_avatar_event_id = event_id!("$example_avatar");

    let response = server
        .mock_sync()
        .ok_and_run_with_response(&client, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::default().add_state_event(
                    f.member(example_id)
                        .avatar_url("mxc://localhost/avatar")
                        .display_name("the first example")
                        .event_id(example_avatar_event_id),
                ),
            );
        })
        .await;

    // Avatar change does not trigger ambiguity change.
    assert!(response.rooms.joined.get(room_id.as_str()).unwrap().ambiguity_changes.is_empty());

    let changes = assert_next_matches!(updates, Ok(RoomUpdate::Joined { updates, .. }) => updates.ambiguity_changes);
    assert!(changes.is_empty());

    assert_pending!(updates);
}

#[async_test]
async fn test_avatar_url_changes() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let example_id = User::Alice;
    let example_2_id = User::Bob;
    let room_id = TestRoom::Default;
    let f = EventFactory::new();

    let mut updates = BroadcastStream::new(client.subscribe_to_room_updates(room_id.into()));
    assert_pending!(updates);

    // Initial sync, adds 2 members.
    let example_1_join_event_id = event_id!("$example_1_join");
    let example_2_join_event_id = event_id!("$example_2_join");
    let response = server
        .mock_sync()
        .ok_and_run_with_response(&client, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::default().add_state_bulk([
                    f.member(example_id)
                        .display_name("example")
                        .event_id(example_1_join_event_id)
                        .into(),
                    f.member(example_2_id)
                        .display_name("example 2")
                        .event_id(example_2_join_event_id)
                        .into(),
                ]),
            );
        })
        .await;

    // No changes since the users didn't have any avatar URLs.
    assert!(response.rooms.joined.get(room_id.as_str()).unwrap().avatar_changes.is_none());

    let changes = assert_next_matches!(updates, Ok(RoomUpdate::Joined { updates, .. }) => updates.avatar_changes);
    assert!(changes.is_none());

    let (room_cache, _handle) =
        client.event_cache().unwrap().room(room_id.into()).await.expect("room cache");
    let (_, mut subscriber) = room_cache.subscribe().await.expect("subscription");

    // Now we sync a room member with an avatar URL.
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::default().add_state_bulk([
                    f.member(example_id)
                        .avatar_url("mxc://localhost/avatar")
                        .display_name("the first example")
                        .into_raw_sync_state(),
                    f.member(example_2_id)
                        .avatar_url("mxc://localhost/avatar2")
                        .display_name("the second example")
                        .into_raw_sync_state(),
                ]),
            );
        })
        .await;

    sleep(Duration::from_secs(1)).await;

    let changes = assert_next_matches!(updates, Ok(RoomUpdate::Joined { updates, .. }) => updates.avatar_changes.expect("avatar changes") );
    assert_eq!(changes.len(), 2);
    assert_let!(Some(Some(avatar_url)) = changes.get(example_id.as_str()));
    assert_eq!(avatar_url, "mxc://localhost/avatar");
    assert_let!(Some(Some(avatar_url)) = changes.get(example_2_id.as_str()));
    assert_eq!(avatar_url, "mxc://localhost/avatar2");

    // The room event cache emits a RoomEventCacheUpdate when the avatar URL
    // changes. This will trigger a timeline item refresh.
    let mut found = false;

    while !subscriber.is_empty() {
        let changes = subscriber.recv().await.expect("subscription event");

        if let RoomEventCacheUpdate::UpdateMembers { avatar_changes, .. } = changes
            && let Some(avatar_changes) = avatar_changes
        {
            assert_eq!(
                avatar_changes,
                BTreeMap::from([
                    (example_id.into(), Some(owned_mxc_uri!("mxc://localhost/avatar"))),
                    (example_2_id.into(), Some(owned_mxc_uri!("mxc://localhost/avatar2")))
                ])
            );

            found = true;
        }
    }

    assert!(found, "Failed to receive an `UpdateMembers`");

    // And after that, receive the first room member without an avatar URL.
    let avatar_removal =
        f.member(example_id).display_name("the first example").into_raw_sync_state();
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::default().add_state_event(avatar_removal.clone()),
            );
        })
        .await;

    // There is a single change: the avatar is now None
    let changes = assert_next_matches!(updates, Ok(RoomUpdate::Joined { updates, .. }) => updates.avatar_changes.expect("avatar changes") );
    assert_eq!(changes.len(), 1);
    assert_let!(Some(None) = changes.get(example_id.as_str()));

    // If we receive the same event again, nothing should happen
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_joined_room(JoinedRoomBuilder::default().add_state_event(avatar_removal));
        })
        .await;

    // There aren't any changes
    let changes = assert_next_matches!(updates, Ok(RoomUpdate::Joined { updates, .. }) => updates.avatar_changes );
    assert!(changes.is_none());
}

#[async_test]
async fn test_rooms_stream() {
    use futures_util::StreamExt as _;

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let (rooms, mut rooms_stream) = client.rooms_stream();

    assert!(rooms.is_empty());
    assert_pending!(rooms_stream);

    // This test requires the room IDs to be sorted lexicographically for the
    // order of the diffs below.
    let room_id_1 = TestRoom::V12;
    let room_id_2 = TestRoom::Default;
    let room_id_3 = TestRoom::Other;

    assert!(client.get_room(room_id_1.into()).is_none());
    assert!(client.get_room(room_id_2.into()).is_none());
    assert!(client.get_room(room_id_3.into()).is_none());

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder
                .add_joined_room(JoinedRoomBuilder::new(room_id_1))
                .add_joined_room(JoinedRoomBuilder::new(room_id_2))
                .add_joined_room(JoinedRoomBuilder::new(room_id_3));
        })
        .await;

    // Rooms are created.
    assert!(client.get_room(room_id_1.into()).is_some());
    assert!(client.get_room(room_id_2.into()).is_some());
    assert!(client.get_room(room_id_3.into()).is_some());

    // We receive 3 diffs…
    assert_let_timeout!(Some(diffs) = rooms_stream.next());
    assert_eq!(diffs.len(), 3);

    // … which map to the new rooms!that
    assert_let!(VectorDiff::PushBack { value: room_1 } = &diffs[0]);
    assert_eq!(room_1.room_id(), room_id_1);
    assert_let!(VectorDiff::PushBack { value: room_2 } = &diffs[1]);
    assert_eq!(room_2.room_id(), room_id_2);
    assert_let!(VectorDiff::PushBack { value: room_3 } = &diffs[2]);
    assert_eq!(room_3.room_id(), room_id_3);

    assert_pending!(rooms_stream);
}

#[async_test]
async fn test_observe_own_beacon_info_updates_emits_room_id_event_id_and_content() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().expect("client is logged in");

    let stream = client.observe_own_beacon_info_updates().unwrap();
    let mut stream = pin!(stream);

    let now = MilliSecondsSinceUnixEpoch::now();
    let f = EventFactory::new();

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_state_event(
                f.beacon_info(
                    Some("Live Share".to_owned()),
                    Duration::from_millis(300_000),
                    true,
                    Some(now),
                )
                .event_id(event_id!("$own_beacon_info"))
                .sender(user_id)
                .state_key(user_id)
                .into_raw_sync_state(),
            ),
        )
        .await;

    let update: BeaconInfoUpdate = stream.next().await.expect("expected a beacon_info update");
    assert_eq!(update.room_id, TestRoom::Default);
    assert_eq!(update.event_id, "$own_beacon_info");
    assert_eq!(update.content.description, Some("Live Share".to_owned()));
    assert!(update.content.live);
}

#[async_test]
async fn test_observe_own_beacon_info_updates_filters_other_users() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let stream = client.observe_own_beacon_info_updates().unwrap();
    let mut stream = pin!(stream);

    let now = MilliSecondsSinceUnixEpoch::now();
    let f = EventFactory::new();

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_state_event(
                f.beacon_info(None, Duration::from_millis(300_000), true, Some(now))
                    .event_id(event_id!("$other_beacon_info"))
                    .sender(User::Alice)
                    .state_key(User::Alice)
                    .into_raw_sync_state(),
            ),
        )
        .await;

    assert!(stream.next().now_or_never().is_none());
}

#[async_test]
async fn test_observe_own_beacon_info_updates_delivers_updates_from_multiple_rooms() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().expect("client is logged in");

    let stream = client.observe_own_beacon_info_updates().unwrap();
    let mut stream = pin!(stream);

    let now = MilliSecondsSinceUnixEpoch::now();
    let f = EventFactory::new();
    let second_room = TestRoom::V12;

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_state_event(
                f.beacon_info(None, Duration::from_millis(300_000), true, Some(now))
                    .event_id(event_id!("$room1_beacon"))
                    .sender(user_id)
                    .state_key(user_id)
                    .into_raw_sync_state(),
            ),
        )
        .await;

    let first = stream.next().await.expect("expected first update");

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(second_room).add_state_event(
                f.beacon_info(None, Duration::from_millis(300_000), false, Some(now))
                    .event_id(event_id!("$room2_beacon"))
                    .sender(user_id)
                    .state_key(user_id)
                    .into_raw_sync_state(),
            ),
        )
        .await;

    let second = stream.next().await.expect("expected second update");

    assert_eq!(first.room_id, TestRoom::Default);
    assert_eq!(second.room_id, second_room);
}

#[async_test]
async fn test_observe_own_beacon_info_updates_stays_idle_without_matching_updates() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let stream = client.observe_own_beacon_info_updates().unwrap();
    let mut stream = pin!(stream);

    server.sync_default_joined_room(&client).await;

    assert!(stream.next().now_or_never().is_none());
}

#[async_test]
async fn test_total_unread_notifications() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let f = EventFactory::new();

    let set_num_notifications = async |room: &matrix_sdk::Room, num: u64| {
        room.update_room_info(|mut room_info| {
            room_info
                .set_read_receipts(ReadReceipts { num_notifications: num, ..Default::default() });

            (room_info, Default::default())
        })
        .await;
    };

    // A room with unread notifications contributes all of them.
    let notifications_room = server.sync_joined_room(&client, room_id!("!a:b.c")).await;
    set_num_notifications(&notifications_room, 3).await;

    // A room the user marked as unread by hand has no count of its own, so it
    // contributes one.
    let marked_unread_room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id!("!d:b.c")).add_account_data(f.marked_unread(true)),
        )
        .await;
    assert!(marked_unread_room.is_marked_unread());

    // A room that is both only needs the user's attention once, so it
    // contributes its notifications and not one more.
    let both_notifications_and_unread = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id!("!e:b.c")).add_account_data(f.marked_unread(true)),
        )
        .await;
    assert!(both_notifications_and_unread.is_marked_unread());
    set_num_notifications(&both_notifications_and_unread, 2).await;

    // A room we haven't joined doesn't contribute at all.
    let invited_room = server.sync_room(&client, InvitedRoomBuilder::new(room_id!("!f:b.c"))).await;
    set_num_notifications(&invited_room, 100).await;

    // A joined space is hidden from room lists, so it doesn't contribute.
    let space = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id!("!g:b.c"))
                .add_state_event(f.create(User::Alice, RoomVersionId::V11).with_space_type()),
        )
        .await;
    assert!(space.is_space());
    set_num_notifications(&space, 100).await;

    // An old room version whose successor is joined is hidden from room lists,
    // so it doesn't contribute. Its successor does.
    let successor_room_id = room_id!("!i:b.c");
    let old_version = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id!("!h:b.c")).add_state_event(
                f.room_tombstone("upgraded", successor_room_id).sender(User::Alice),
            ),
        )
        .await;
    set_num_notifications(&old_version, 100).await;
    let successor_room = server.sync_joined_room(&client, successor_room_id).await;
    set_num_notifications(&successor_room, 4).await;

    // An old room version whose successor is only invited is still the active
    // version, so it contributes.
    let invited_successor_room_id = room_id!("!k:b.c");
    let active_old_version = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id!("!j:b.c")).add_state_event(
                f.room_tombstone("upgraded", invited_successor_room_id).sender(User::Alice),
            ),
        )
        .await;
    set_num_notifications(&active_old_version, 5).await;
    server.sync_room(&client, InvitedRoomBuilder::new(invited_successor_room_id)).await;

    assert_eq!(client.total_unread_notifications(), 3 + 1 + 2 + 4 + 5);
}

#[async_test]
async fn test_observe_own_beacon_info_updates_requires_known_own_user_id() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().unlogged().build().await;

    let res = client.observe_own_beacon_info_updates();

    assert!(matches!(res, Err(Error::AuthenticationRequired)));
}

#[async_test]
async fn test_dms_are_processed_in_any_sync_response() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_a_id = User::Alice;
    let user_b_id = User::Bob;
    let room_id_1 = TestRoom::Default;
    let room_id_2 = TestRoom::V12;
    let f = EventFactory::new();

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder
                .add_global_account_data(
                    f.direct().add_user(user_a_id, room_id_1).add_user(user_b_id, room_id_2),
                )
                .add_joined_room(JoinedRoomBuilder::new(room_id_1));
        })
        .await;

    let room_1 = client.get_room(room_id_1.into()).unwrap();
    assert!(room_1.is_direct().await.unwrap());

    // Now perform a sync without new account data
    let room_2 = server.sync_joined_room(&client, room_id_2).await;
    assert!(room_2.is_direct().await.unwrap());
}

#[async_test]
async fn test_restore_room() {
    let room_id = TestRoom::Default;

    // Create memory store with some room data.
    let store = MemoryStore::new();

    let mut changes = StateChanges::default();

    let f = EventFactory::new().room(room_id).sender(User::Alice);
    let raw_tag_event: Raw<AnyRoomAccountDataEvent> =
        f.tag_from_names(vec![TagName::Favorite]).into();
    let tag_event = raw_tag_event.deserialize().unwrap();
    changes.add_room_account_data(room_id.into(), tag_event, raw_tag_event);

    let raw_pinned_events_event: Raw<_> = f
        .room_pinned_events(vec![owned_event_id!("$a"), owned_event_id!("$b")])
        .into_raw_sync_state();
    let pinned_events_event = raw_pinned_events_event.deserialize().unwrap();
    changes.add_state_event(room_id.into(), pinned_events_event, raw_pinned_events_event);

    let room_info = serde_json::from_value(json!({
        "room_id": room_id,
        "room_state": "Joined",
        "notification_counts": {
            "highlight_count": 0,
            "notification_count": 0,
        },
        "summary": {
            "room_heroes": [],
            "joined_member_count": 1,
            "invited_member_count": 0,
        },
        "members_synced": true,
        "last_prev_batch": "pb",
        "sync_info": "FullySynced",
        "encryption_state_synced": true,
        "base_info": {
            "avatar": null,
            "canonical_alias": null,
            "create": null,
            "dm_targets": [],
            "encryption": null,
            "guest_access": null,
            "history_visibility": null,
            "join_rules": null,
            "max_power_level": 100,
            "name": null,
            "tombstone": null,
            "topic": null,
        },
    }))
    .unwrap();
    changes.add_room(room_info);

    store.save_changes(&changes).await.unwrap();

    // Build a client with that store.
    let store_config = StoreConfig::new(CrossProcessLockConfig::SingleProcess).state_store(store);
    let client = test_client_builder(None)
        .request_config(RequestConfig::new().disable_retry())
        .store_config(store_config)
        .build()
        .await
        .unwrap();
    client
        .matrix_auth()
        .restore_session(mock_matrix_session(), RoomLoadSettings::default())
        .await
        .unwrap();

    let room = client.get_room(room_id.into()).unwrap();
    assert!(room.is_favourite());
    assert!(!room.pinned_event_ids().unwrap().is_empty());
}

#[async_test]
#[cfg(feature = "sqlite")]
async fn test_client_pause_resume_with_sqlite_store() {
    let tempdir = tempdir().unwrap();

    let server = MatrixMockServer::new().await;
    let client = server
        .client_builder()
        .on_builder(|builder| builder.sqlite_store(tempdir.path(), None))
        .build()
        .await;

    client
        .state_store()
        .set_custom_value(b"pause_resume_key", b"before_pause".to_vec())
        .await
        .unwrap();

    assert_eq!(
        client.state_store().get_custom_value(b"pause_resume_key").await.unwrap().as_deref(),
        Some(b"before_pause".as_slice())
    );

    client.pause().await.unwrap();

    let read_err = client.state_store().get_custom_value(b"pause_resume_key").await.unwrap_err();
    assert!(
        read_err.to_string().contains("closed"),
        "read while client is paused should mention 'closed', got: {read_err}"
    );

    let write_err = client
        .state_store()
        .set_custom_value(b"pause_resume_key", b"while_paused".to_vec())
        .await
        .unwrap_err();
    assert!(
        write_err.to_string().contains("closed"),
        "write while client is paused should mention 'closed', got: {write_err}"
    );

    client.resume().await.unwrap();

    assert_eq!(
        client.state_store().get_custom_value(b"pause_resume_key").await.unwrap().as_deref(),
        Some(b"before_pause".as_slice())
    );

    client
        .state_store()
        .set_custom_value(b"pause_resume_key", b"after_resume".to_vec())
        .await
        .unwrap();

    assert_eq!(
        client.state_store().get_custom_value(b"pause_resume_key").await.unwrap().as_deref(),
        Some(b"after_resume".as_slice())
    );
}

#[async_test]
#[cfg(feature = "sqlite")]
async fn test_client_pause_waits_for_held_state_store_write() {
    let tempdir = tempdir().unwrap();

    let server = MatrixMockServer::new().await;
    let client = server
        .client_builder()
        .on_builder(|builder| builder.sqlite_store(tempdir.path(), None))
        .build()
        .await;

    client.state_store().set_custom_value(b"held_write_key", b"initial".to_vec()).await.unwrap();

    let write_fut = client.state_store().set_custom_value(b"held_write_key", b"updated".to_vec());
    tokio::pin!(write_fut);

    assert!(
        write_fut.as_mut().now_or_never().is_none(),
        "write future should not complete immediately before being awaited"
    );

    let client_clone = client.clone();
    let pause_handle = spawn(async move {
        client_clone.pause().await.unwrap();
    });

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(
        !pause_handle.is_finished(),
        "pause should wait for the in-flight state store write to finish"
    );

    write_fut.await.unwrap();

    timeout(Duration::from_secs(3), pause_handle)
        .await
        .expect("pause should complete after the held write finishes")
        .unwrap();

    let closed_err = client.state_store().get_custom_value(b"held_write_key").await.unwrap_err();
    assert!(
        closed_err.to_string().contains("closed"),
        "store should be closed after client pause completes, got: {closed_err}"
    );

    client.resume().await.unwrap();

    assert_eq!(
        client.state_store().get_custom_value(b"held_write_key").await.unwrap().as_deref(),
        Some(b"updated".as_slice())
    );
}

#[async_test]
async fn test_logout() {
    let server = MatrixMockServer::new().await;

    // Test unauthenticated client.
    let unlogged_client = server.client_builder().unlogged().build().await;
    let res = unlogged_client.logout().await;
    assert_matches!(res, Err(Error::AuthenticationRequired));

    // Test MatrixAuth.
    server.mock_logout().ok().mock_once().named("matrix_logout").mount().await;

    let matrix_auth_client = server.client_builder().build().await;
    matrix_auth_client.logout().await.unwrap();

    // Test OAuth.
    server
        .oauth()
        .mock_server_metadata()
        .ok()
        .mock_once()
        .named("oauth_server_metadata")
        .mount()
        .await;

    let oauth_client = server.client_builder().logged_in_with_oauth().build().await;
    let res = oauth_client.logout().await;

    // This returns an error because it requires a HTTPS server URI, or to be
    // able to call `OAuth::insecure_rewrite_https_to_http()`, but at least we
    // are testing the OAuth branch inside `Client::logout()`.
    assert_let!(Err(Error::OAuth(oauth_error)) = res);
    assert_matches!(*oauth_error, OAuthError::Logout(OAuthTokenRevocationError::Url(_)));
}

#[async_test]
async fn test_room_sync_state_after() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = TestRoom::Default;

    let mut rx = client.subscribe_to_room_updates(room_id.into());

    let f = EventFactory::new().sender(User::Alice).room(room_id);

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .use_state_after()
                .add_state_bulk([
                    f.create(User::Alice, RoomVersionId::V1).into(),
                    f.power_levels().set_users([(User::Alice, 100), (User::Bob, 0)]).into(),
                    f.room_history_visibility(HistoryVisibility::WorldReadable).into(),
                    f.room_join_rules(JoinRule::Public).into(),
                    f.member(User::Carol).sender(User::Carol).leave().into(),
                ])
                .add_timeline_bulk([
                    f.member(User::Carol).into_raw_timeline().cast(),
                    f.room_name("room name").into_raw_timeline().cast(),
                ]),
        )
        .await;

    let update = rx.recv().now_or_never().unwrap().unwrap();
    assert_let!(RoomUpdate::Joined { updates, .. } = update);

    // We received the `state_after`.
    assert_let!(State::After(state_events) = updates.state);
    assert_eq!(state_events.len(), 5);
    assert_eq!(updates.timeline.events.len(), 2);

    // Only the state events in `state_after` were used.
    let room = client.get_room(room_id.into()).unwrap();

    assert!(room.create_content().is_some());
    assert_eq!(room.history_visibility().unwrap(), HistoryVisibility::WorldReadable);
    assert_eq!(room.join_rule().unwrap(), JoinRule::Public);
    assert!(room.name().is_none());

    let member = room.get_member_no_sync(User::Carol.into()).await.unwrap().unwrap();
    assert_eq!(*member.membership(), MembershipState::Leave);
}

#[cfg(feature = "federation-api")]
#[async_test]
async fn test_server_vendor_info() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    // Mock the federation version endpoint
    server.mock_federation_version().ok("Synapse", "1.70.0").mount().await;

    let server_info = client.server_vendor_info(None).await.unwrap();

    assert_eq!(server_info.server_name, "Synapse");
    assert_eq!(server_info.version, "1.70.0");
}

#[async_test]
async fn test_server_version_without_auth() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    // If we provide an access token, we encounter a failure, likely because the
    // token has expired.
    server.mock_versions().expect_default_access_token().error_unknown_token(true).mount().await;

    // If we do not provide an access token, all is fine as the endpoint does
    // not require one.
    server.mock_versions().expect_missing_access_token().ok().mount().await;

    let request_config = RequestConfig::new().disable_retry();
    client
        .fetch_server_versions(Some(request_config))
        .await
        .expect_err("We should fail here since we provided an auth token.");

    let request_config = RequestConfig::new().disable_retry().skip_auth();
    client
        .fetch_server_versions(Some(request_config))
        .await
        .expect("We should not fail here since we did not provide an auth token.");
}

#[cfg(feature = "federation-api")]
#[async_test]
async fn test_server_vendor_info_with_missing_fields() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    // Mock the federation version endpoint with missing fields
    server.mock_federation_version().ok_empty().mount().await;

    let server_info = client.server_vendor_info(None).await.unwrap();

    // Should use defaults for missing fields
    assert_eq!(server_info.server_name, "unknown");
    assert_eq!(server_info.version, "unknown");
}

#[async_test]
async fn test_fetch_thread_subscriptions() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room1 = TestRoom::Default;
    let room2 = TestRoom::V12;
    let room3 = TestRoom::Other;

    let thread1 = owned_event_id!("$thread1:example.com");
    let thread2 = owned_event_id!("$thread2:example.com");
    let thread3 = owned_event_id!("$thread3:example.com");

    server
        .mock_get_thread_subscriptions()
        .match_from("from")
        .match_to("to")
        .add_subscription(room1.into(), thread1.clone(), ThreadSubscription::new(true, uint!(42)))
        .add_subscription(room2.into(), thread2.clone(), ThreadSubscription::new(false, uint!(7)))
        .add_unsubscription(room3.into(), thread3.clone(), ThreadUnsubscription::new(uint!(13)))
        .ok(Some("next_batch_token".to_owned()))
        .mount()
        .await;

    let response = client
        .fetch_thread_subscriptions(Some("from".to_owned()), Some("to".to_owned()), None)
        .await
        .unwrap();

    assert_eq!(response.end.as_deref(), Some("next_batch_token"));

    assert_eq!(response.subscribed.len(), 2);

    let s1 = &response.subscribed[room1.as_str()][&thread1];
    assert!(s1.automatic);
    assert_eq!(s1.bump_stamp, uint!(42));

    let s2 = &response.subscribed[room2.as_str()][&thread2];
    assert!(s2.automatic.not());
    assert_eq!(s2.bump_stamp, uint!(7));

    assert_eq!(response.unsubscribed.len(), 1);

    let u = &response.unsubscribed[room3.as_str()][&thread3];
    assert_eq!(u.bump_stamp, uint!(13));
}

/// Create a sliding sync thread_subscription response with no `prev_batch`
/// token.
fn thread_subscription_response(
    room1: TestRoom,
    thread1: &EventId,
    room2: TestRoom,
    thread2: &EventId,
) -> msc4186::response::ThreadSubscriptions {
    assign!(msc4186::response::ThreadSubscriptions::default(), {
        subscribed: {
            let mut map = BTreeMap::new();
            map.insert(room1.into(), {
                let mut threads = BTreeMap::new();
                threads.insert(thread1.to_owned(), ThreadSubscription::new(true, uint!(42)));
                threads
            });
            map
        },
        unsubscribed: {
            let mut map = BTreeMap::new();
            map.insert(room2.into(), {
                let mut threads = BTreeMap::new();
                threads.insert(thread2.to_owned(), ThreadUnsubscription::new(uint!(7)));
                threads
            });
            map
        },
        prev_batch: None,
    })
}

#[async_test]
async fn test_sync_thread_subscriptions() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room1 = TestRoom::Default;
    let room2 = TestRoom::V12;

    let thread1 = owned_event_id!("$thread1:example.com");
    let thread2 = owned_event_id!("$thread2:example.com");

    // At first, there are no thread subscriptions at all.
    let stored1 = client
        .state_store()
        .load_thread_subscription(room1.into(), &thread1)
        .await
        .expect("loading room1/thread1 works fine");
    assert_matches!(stored1, None);

    let stored2 = client
        .state_store()
        .load_thread_subscription(room2.into(), &thread2)
        .await
        .expect("loading room2/thread2 works fine");
    assert_matches!(stored2, None);

    // When I sliding-sync thread subscriptions,
    server
        .mock_sliding_sync()
        .ok_and_run(
            &client,
            |config_builder| {
                config_builder.with_thread_subscriptions_extension(
                    assign!(msc4186::request::ThreadSubscriptions::default(), {
                        enabled: Some(true),
                        limit: Some(uint!(10)),
                    }),
                )
            },
            assign!(msc4186::Response::new("pos".to_owned()), {
                extensions: assign!(msc4186::response::Extensions::default(), {
                    thread_subscriptions: thread_subscription_response(
                        room1, &thread1, room2, &thread2,
                    ),
                }),
            }),
        )
        .await;

    // Then they're stored in the local database.
    let stored1 = client
        .state_store()
        .load_thread_subscription(room1.into(), &thread1)
        .await
        .expect("loading room1/thread1 works fine")
        .expect("found room1/thread1 subscription");

    assert_eq!(stored1.status, ThreadSubscriptionStatus::Subscribed { automatic: true });
    assert_eq!(stored1.bump_stamp, Some(42));

    let stored2 = client
        .state_store()
        .load_thread_subscription(room2.into(), &thread2)
        .await
        .expect("loading room2/thread2 works fine")
        .expect("found room2/thread2 unsubscription");

    assert_eq!(stored2.status, ThreadSubscriptionStatus::Unsubscribed);
    assert_eq!(stored2.bump_stamp, Some(7));
}

#[async_test]
async fn test_sync_thread_subscriptions_with_catchup() {
    let server = MatrixMockServer::new().await;

    // Make sure to advertise support for thread subscriptions.
    server.mock_versions().with_thread_subscriptions().ok().mount().await;

    let client = server
        .client_builder()
        .no_server_versions()
        .on_builder(|builder| {
            builder.with_threading_support(ThreadingSupport::Enabled { with_subscriptions: true })
        })
        .build()
        .await;

    let room_id1 = TestRoom::Default;
    let room_id2 = TestRoom::V12;

    let thread1 = owned_event_id!("$thread1:example.com");
    let thread2 = owned_event_id!("$thread2:example.com");
    let thread3 = owned_event_id!("$thread3:example.com");

    // The provided catchup token will be used to fetch more thread
    // subscriptions via the msc4308 companion endpoint.
    server
        .mock_get_thread_subscriptions()
        .match_from("catchup_token")
        .add_subscription(
            room_id1.into(),
            thread3.clone(),
            ThreadSubscription::new(false, uint!(1337)),
        )
        .with_delay(Duration::from_millis(300)) // Simulate some network delay.
        // No more subscriptions after the first catchup request.
        .ok(None)
        .mock_once()
        .mount()
        .await;

    // When I sliding-sync thread subscriptions, and the response includes this
    // catch-up token,
    let mut thread_subscriptions =
        thread_subscription_response(room_id1, &thread1, room_id2, &thread2);
    thread_subscriptions.prev_batch = Some("catchup_token".to_owned());

    server
        .mock_sliding_sync()
        .ok_and_run(
            &client,
            |config_builder| {
                config_builder
                    .with_thread_subscriptions_extension(
                        assign!(msc4186::request::ThreadSubscriptions::default(), {
                            enabled: Some(true),
                            limit: Some(uint!(10)),
                        }),
                    )
                    .add_list(SlidingSyncList::builder("rooms"))
            },
            assign!(msc4186::Response::new("pos".to_owned()), {
                rooms: {
                    let mut rooms = BTreeMap::new();
                    rooms.insert(room_id1.into(), msc4186::response::Room::default());
                    rooms.insert(room_id2.into(), msc4186::response::Room::default());
                    rooms
                },
                extensions: assign!(msc4186::response::Extensions::default(), {
                    thread_subscriptions,
                }),
            }),
        )
        .await;

    // If I try to get the subscription status for thread 1, it's still hitting
    // network, because it doesn't know yet about the result of the catch-up
    // request. (Ideally, the choice of whether some information is outdated or
    // not would be per room/thread pair, but for simplicity it's global right
    // now.)
    server
        .mock_room_get_thread_subscription()
        .match_room_id(room_id1.into())
        .match_thread_id(thread1.clone())
        .ok(true)
        .mock_once()
        .mount()
        .await;

    let room1 = client.get_room(room_id1.into()).unwrap();
    let sub1 = room1.load_or_fetch_thread_subscription(&thread1).await.unwrap();
    assert_eq!(sub1, Some(matrix_sdk::room::ThreadSubscription { automatic: true }));

    // All the thread subscriptions are eventually known in the database.
    sleep(Duration::from_millis(400)).await;

    let stored3 = client
        .state_store()
        .load_thread_subscription(room_id1.into(), &thread3)
        .await
        .expect("loading room1/thread3 works fine")
        .expect("found room1/thread3 subscription");
    assert_eq!(stored3.status, ThreadSubscriptionStatus::Subscribed { automatic: false });
    assert_eq!(stored3.bump_stamp, Some(1337));

    // So the client will use the database only to load_or_fetch thread
    // subscriptions. (Which is confirmed by the absence of mocking the
    // room_get_thread_subscription endpoint for thread3.)
    let sub3 = room1.load_or_fetch_thread_subscription(&thread3).await.unwrap();
    assert_eq!(sub3, Some(matrix_sdk::room::ThreadSubscription { automatic: false }));
}

#[async_test]
#[cfg(feature = "sqlite")]
async fn test_sync_processing_of_custom_join_rule() {
    let tempdir = tempdir().unwrap();

    let server = MatrixMockServer::new().await;
    let client = server
        .client_builder()
        .on_builder(|builder| builder.sqlite_store(tempdir.path(), None))
        .build()
        .await;

    server
        .mock_sync()
        .ok(|builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::default().add_state_event(Raw::from_json(
                    serde_json::value::to_raw_value(&json!({
                            "content": {
                                "join_rule": "my_custom_rule"
                            },
                            "event_id": "$15139375513VdeRF:localhost",
                            "origin_server_ts": 151393755,
                            "sender": "@example:localhost",
                            "state_key": "",
                            "type": "m.room.join_rules",
                    }))
                    .unwrap(),
                )),
            );
        })
        .mock_once()
        .mount()
        .await;

    client
        .sync_once(Default::default())
        .await
        .expect("We should be able to process the sync despite there being a custom join rule");
}

#[async_test]
#[cfg(feature = "sqlite")]
async fn test_sync_processing_of_custom_stripped_join_rule() {
    let tempdir = tempdir().unwrap();

    let server = MatrixMockServer::new().await;
    let client = server
        .client_builder()
        .on_builder(|builder| builder.sqlite_store(tempdir.path(), None))
        .build()
        .await;

    server
        .mock_sync()
        .ok(|builder| {
            builder.add_invited_room(
                InvitedRoomBuilder::default().add_state_event(Raw::from_json(
                    serde_json::value::to_raw_value(&json!({
                        "content": {
                            "join_rule": "my_custom_rule"
                        },
                        "event_id": "$15139375513VdeRF:localhost",
                        "origin_server_ts": 151393755,
                        "sender": "@example:localhost",
                        "state_key": "",
                        "type": "m.room.join_rules",
                    }))
                    .unwrap(),
                )),
            );
        })
        .mock_once()
        .mount()
        .await;

    client
        .sync_once(Default::default())
        .await
        .expect("We should be able to process the sync despite there being a custom join rule");
}
