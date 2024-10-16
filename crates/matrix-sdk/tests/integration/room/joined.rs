use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::future::join_all;
use matrix_sdk::{
    config::SyncSettings,
    room::{edit::EditedContent, Receipts, ReportedContentScore, RoomMemberRole},
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_base::RoomState;
use matrix_sdk_test::{
    async_test,
    event_factory::EventFactory,
    mocks::{mock_encryption_state, mock_redaction},
    test_json::{self, sync::CUSTOM_ROOM_POWER_LEVELS},
    EphemeralTestEvent, GlobalAccountDataTestEvent, JoinedRoomBuilder, StateTestEvent,
    SyncResponseBuilder, DEFAULT_TEST_ROOM_ID,
};
use ruma::{
    api::client::{membership::Invite3pidInit, receipt::create_receipt::v3::ReceiptType},
    assign, event_id,
    events::{
        direct::DirectUserIdentifier,
        receipt::ReceiptThread,
        room::message::{RoomMessageEventContent, RoomMessageEventContentWithoutRelation},
        TimelineEventType,
    },
    int, mxc_uri, owned_event_id, room_id, thirdparty, user_id, OwnedUserId, TransactionId,
};
use serde_json::{from_value, json, Value};
use wiremock::{
    matchers::{body_json, body_partial_json, header, method, path_regex},
    Mock, ResponseTemplate,
};

use crate::{logged_in_client_with_server, mock_sync, synced_client};
#[async_test]
async fn test_invite_user_by_id() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/invite$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let user = user_id!("@example:localhost");
    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    room.invite_user_by_id(user).await.unwrap();
}

#[async_test]
async fn test_invite_user_by_3pid() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/invite$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    room.invite_user_by_3pid(
        Invite3pidInit {
            id_server: "example.org".to_owned(),
            id_access_token: "IdToken".to_owned(),
            medium: thirdparty::Medium::Email,
            address: "address".to_owned(),
        }
        .into(),
    )
    .await
    .unwrap();
}

#[async_test]
async fn test_leave_room() -> Result<(), anyhow::Error> {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/leave$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await?;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    room.leave().await?;

    assert_eq!(room.state(), RoomState::Left);

    Ok(())
}

#[async_test]
async fn test_ban_user() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/ban$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let user = user_id!("@example:localhost");
    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    room.ban_user(user, None).await.unwrap();
}

#[async_test]
async fn test_unban_user() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/unban$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let user = user_id!("@example:localhost");
    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    room.unban_user(user, None).await.unwrap();
}

#[async_test]
async fn test_mark_as_unread() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("PUT"))
        .and(path_regex(
            r"^/_matrix/client/r0/user/.*/rooms/.*/account_data/com.famedly.marked_unread",
        ))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    room.set_unread_flag(true).await.unwrap();

    room.set_unread_flag(false).await.unwrap();
}

#[async_test]
async fn test_kick_user() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/kick$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let user = user_id!("@example:localhost");
    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    room.kick_user(user, None).await.unwrap();
}

#[async_test]
async fn test_send_single_receipt() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/receipt"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let event_id = event_id!("$xxxxxx:example.org").to_owned();
    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    room.send_single_receipt(ReceiptType::Read, ReceiptThread::Unthreaded, event_id).await.unwrap();
}

#[async_test]
async fn test_send_multiple_receipts() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/read_markers$"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let event_id = event_id!("$xxxxxx:example.org").to_owned();
    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let receipts = Receipts::new().fully_read_marker(event_id);
    room.send_multiple_receipts(receipts).await.unwrap();
}

#[async_test]
async fn test_typing_notice() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/typing"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    room.typing_notice(true).await.unwrap();
}

#[async_test]
async fn test_room_state_event_send() {
    use ruma::events::room::member::{MembershipState, RoomMemberEventContent};

    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let avatar_url = mxc_uri!("mxc://example.org/avA7ar");
    let member_event = assign!(RoomMemberEventContent::new(MembershipState::Join), {
        avatar_url: Some(avatar_url.to_owned())
    });
    let response =
        room.send_state_event_for_key(user_id!("@foo:bar.com"), member_event).await.unwrap();
    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id);
}

#[async_test]
async fn test_room_message_send() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;
    mock_encryption_state(&server, false).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let content = RoomMessageEventContent::text_plain("Hello world");
    let txn_id = TransactionId::new();
    let response = room.send(content).with_transaction_id(txn_id).await.unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[async_test]
async fn test_room_redact() {
    let (client, server) = synced_client().await;

    let event_id = event_id!("$h29iv0s8:example.com");
    mock_redaction(event_id).mount(&server).await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let txn_id = TransactionId::new();
    let reason = Some("Indecent material");
    let response =
        room.redact(event_id!("$xxxxxxxx:example.com"), reason, Some(txn_id)).await.unwrap();

    assert_eq!(response.event_id, event_id);
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_fetch_members_deduplication() {
    let (client, server) = synced_client().await;

    // We don't need any members, we're just checking if we're correctly
    // deduplicating calls to the method.
    let response_body = json!({
        "chunk": [],
    });

    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/members"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(response_body))
        .expect(1)
        .mount(&server)
        .await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let mut tasks = Vec::new();

    // Create N tasks that try to fetch the members.
    for _ in 0..5 {
        #[allow(unknown_lints, clippy::redundant_async_block)] // false positive
        let task = tokio::spawn({
            let room = room.clone();
            async move { room.sync_members().await }
        });

        tasks.push(task);
    }

    // Wait on all of them at once.
    join_all(tasks).await;

    // Ensure we called the endpoint exactly once.
    server.verify().await;
}

#[async_test]
async fn test_set_name() {
    let (client, server) = synced_client().await;

    mock_sync(&server, &*test_json::SYNC, None).await;
    let sync_settings = SyncSettings::new();
    client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    let name = "The room name";

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/m.room.name/$"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_json(json!({
            "name": name,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .expect(1)
        .mount(&server)
        .await;

    room.set_name(name.to_owned()).await.unwrap();
}

#[async_test]
async fn test_report_content() {
    let (client, server) = logged_in_client_with_server().await;

    let reason = "I am offended";
    let score = int!(-80);

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/report/\$offensive_event"))
        .and(body_json(json!({
            "reason": reason,
            "score": score,
        })))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .expect(1)
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();
    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let event_id = owned_event_id!("$offensive_event");
    let reason = "I am offended".to_owned();
    let score = ReportedContentScore::new(-80).unwrap();

    room.report_content(event_id, Some(score), Some(reason.to_owned())).await.unwrap();
}

#[async_test]
async fn test_subscribe_to_typing_notifications() {
    let (client, server) = logged_in_client_with_server().await;
    let typing_sequences: Arc<Mutex<Vec<Vec<OwnedUserId>>>> = Arc::new(Mutex::new(Vec::new()));
    // The expected typing sequences that we will receive, note that the current
    // user_id is filtered out.
    let asserted_typing_sequences =
        vec![vec![user_id!("@alice:matrix.org"), user_id!("@bob:example.com")], vec![]];
    let room_id = room_id!("!test:example.org");
    let mut sync_builder = SyncResponseBuilder::new();

    // Initial sync with our test room.
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Send to typing notification
    let room = client.get_room(room_id).unwrap();
    let join_handle = tokio::spawn({
        let typing_sequences = Arc::clone(&typing_sequences);
        async move {
            let (_drop_guard, mut subscriber) = room.subscribe_to_typing_notifications();

            while let Ok(typing_user_ids) = subscriber.recv().await {
                let mut typing_sequences = typing_sequences.lock().unwrap();
                typing_sequences.push(typing_user_ids);

                // When we have received 2 typing notifications, we can stop listening.
                if typing_sequences.len() == 2 {
                    break;
                }
            }
        }
    });

    // Then send a typing notification with 3 users typing, including the current
    // user.
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_ephemeral_event(
        EphemeralTestEvent::Custom(json!({
            "content": {
                "user_ids": [
                    "@alice:matrix.org",
                    "@bob:example.com",
                    "@example:localhost"
                ]
            },
            "room_id": "!jEsUZKDJdhlrceRyVU:example.org",
            "type": "m.typing"
        })),
    ));
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Then send a typing notification with no user typing
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_ephemeral_event(
        EphemeralTestEvent::Custom(json!({
            "content": {
                "user_ids": []
            },
            "room_id": "!jEsUZKDJdhlrceRyVU:example.org",
            "type": "m.typing"
        })),
    ));
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    join_handle.await.unwrap();
    assert_eq!(typing_sequences.lock().unwrap().to_vec(), asserted_typing_sequences);
}

#[async_test]
async fn test_get_suggested_user_role() {
    let (client, server) = logged_in_client_with_server().await;

    mock_sync(&server, &*test_json::DEFAULT_SYNC_SUMMARY, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings).await.unwrap();
    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let role_admin = room.get_suggested_user_role(user_id!("@example:localhost")).await.unwrap();
    assert_eq!(role_admin, RoomMemberRole::Administrator);

    // This user either does not exist in the room or has no special role
    let role_unknown =
        room.get_suggested_user_role(user_id!("@non-existing:localhost")).await.unwrap();
    assert_eq!(role_unknown, RoomMemberRole::User);
}

#[async_test]
async fn test_get_power_level_for_user() {
    let (client, server) = logged_in_client_with_server().await;

    mock_sync(&server, &*test_json::DEFAULT_SYNC_SUMMARY, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings).await.unwrap();
    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let power_level_admin =
        room.get_user_power_level(user_id!("@example:localhost")).await.unwrap();
    assert_eq!(power_level_admin, 100);

    // This user either does not exist in the room or has no special power level
    let power_level_unknown =
        room.get_user_power_level(user_id!("@non-existing:localhost")).await.unwrap();
    assert_eq!(power_level_unknown, 0);
}

#[async_test]
async fn test_get_users_with_power_levels() {
    let (client, server) = logged_in_client_with_server().await;

    mock_sync(&server, &*test_json::sync::SYNC_ADMIN_AND_MOD, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings).await.unwrap();
    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let users_with_power_levels = room.users_with_power_levels().await;
    assert_eq!(users_with_power_levels.len(), 2);
    assert_eq!(users_with_power_levels[user_id!("@admin:localhost")], 100);
    assert_eq!(users_with_power_levels[user_id!("@mod:localhost")], 50);
}

#[async_test]
async fn test_get_users_with_power_levels_is_empty_if_power_level_info_is_not_available() {
    let (client, server) = logged_in_client_with_server().await;

    mock_sync(&server, &*test_json::INVITE_SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings).await.unwrap();
    // The room doesn't have any power level info
    let room = client.get_room(room_id!("!696r7674:example.com")).unwrap();

    assert!(room.users_with_power_levels().await.is_empty());
}

#[async_test]
async fn test_reset_power_levels() {
    let (client, server) = logged_in_client_with_server().await;

    mock_sync(&server, &*CUSTOM_ROOM_POWER_LEVELS, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings).await.unwrap();
    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/m.room.power_levels/$"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_partial_json(json!({
            "events": {
                // 'm.room.avatar' is 100 here, if we receive a value '50', the reset worked
                "m.room.avatar": 50,
                "m.room.canonical_alias": 50,
                "m.room.history_visibility": 100,
                "m.room.name": 50,
                "m.room.power_levels": 100,
                "m.room.topic": 50
            },
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .expect(1)
        .mount(&server)
        .await;

    let initial_power_levels = room.power_levels().await.unwrap();
    assert_eq!(initial_power_levels.events[&TimelineEventType::RoomAvatar], int!(100));

    room.reset_power_levels().await.unwrap();
}

#[async_test]
async fn test_is_direct_invite_by_3pid() {
    let (client, server) = logged_in_client_with_server().await;

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::default());
    let data = json!({
        "content": {
            "invited@localhost.com": [*DEFAULT_TEST_ROOM_ID],
        },
        "event_id": "$757957878228ekrDs:localhost",
        "origin_server_ts": 17195787,
        "sender": "@example:localhost",
        "state_key": "",
        "type": "m.direct",
        "unsigned": {
          "age": 139298
        }
    });
    sync_builder.add_global_account_data_bulk(vec![from_value(data).unwrap()]);

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    mock_encryption_state(&server, false).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    assert!(room.is_direct().await.unwrap());
    assert!(room.direct_targets().contains(<&DirectUserIdentifier>::from("invited@localhost.com")));
}

#[async_test]
async fn test_call_notifications_ring_for_dms() {
    let (client, server) = logged_in_client_with_server().await;

    let mut sync_builder = SyncResponseBuilder::new();
    let room_builder = JoinedRoomBuilder::default().add_state_event(StateTestEvent::PowerLevels);
    sync_builder.add_joined_room(room_builder);
    sync_builder.add_global_account_data_event(GlobalAccountDataTestEvent::Direct);

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    mock_encryption_state(&server, false).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    assert!(room.is_direct().await.unwrap());
    assert!(!room.has_active_room_call());

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and({
            move |request: &wiremock::Request| {
                let content: Value = request.body_json().expect("The body should be a JSON body");
                assert_eq!(
                    content,
                    json!({
                        "application": "m.call",
                        "call_id": DEFAULT_TEST_ROOM_ID.to_string(),
                        "m.mentions": {"room" :true},
                        "notify_type": "ring"
                    }),
                );
                true
            }
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"event_id": "$event_id"})))
        .expect(1)
        .mount(&server)
        .await;

    room.send_call_notification_if_needed().await.unwrap();
}

#[async_test]
async fn test_call_notifications_notify_for_rooms() {
    let (client, server) = logged_in_client_with_server().await;

    let mut sync_builder = SyncResponseBuilder::new();
    let room_builder = JoinedRoomBuilder::default().add_state_event(StateTestEvent::PowerLevels);
    sync_builder.add_joined_room(room_builder);
    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;

    mock_encryption_state(&server, false).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    assert!(!room.is_direct().await.unwrap());
    assert!(!room.has_active_room_call());

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and({
            move |request: &wiremock::Request| {
                let content: Value = request.body_json().expect("The body should be a JSON body");
                assert_eq!(
                    content,
                    json!({
                        "application": "m.call",
                        "call_id": DEFAULT_TEST_ROOM_ID.to_string(),
                        "m.mentions": {"room" :true},
                        "notify_type": "notify"
                    }),
                );
                true
            }
        })
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"event_id": "$event_id"})))
        .expect(1)
        .mount(&server)
        .await;

    room.send_call_notification_if_needed().await.unwrap();
}

#[async_test]
async fn test_call_notifications_dont_notify_room_without_mention_powerlevel() {
    let (client, server) = logged_in_client_with_server().await;

    let mut sync_builder = SyncResponseBuilder::new();
    let mut power_level_event = StateTestEvent::PowerLevels.into_json_value();
    // Allow noone to send room notify events.
    *power_level_event.get_mut("content").unwrap().get_mut("notifications").unwrap() =
        json!({"room": 101});

    sync_builder.add_joined_room(
        JoinedRoomBuilder::default().add_state_event(StateTestEvent::Custom(power_level_event)),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    mock_encryption_state(&server, false).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    assert!(!room.is_direct().await.unwrap());
    assert!(!room.has_active_room_call());

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"event_id": "$event_id"})))
        // Expect no calls of the send because we dont have permission to notify.
        .expect(0)
        .mount(&server)
        .await;

    room.send_call_notification_if_needed().await.unwrap();
}

#[async_test]
async fn test_make_reply_event_doesnt_require_event_cache() {
    // Even if we don't have enabled the event cache, we'll resort to using the
    // /event query to get details on an event.

    let mock = MatrixMockServer::new().await;
    let client = mock.client_builder().build().await;
    let user_id = client.user_id().unwrap().to_owned();

    let room_id = room_id!("!galette:saucisse.bzh");
    let room = mock.sync_joined_room(&client, room_id).await;

    let event_id = event_id!("$1");
    let f = EventFactory::new();
    mock.mock_room_event()
        .ok(f.text_msg("hi").event_id(event_id).sender(&user_id).room(room_id).into_timeline())
        .expect(1)
        .named("/event")
        .mount()
        .await;

    let new_content = RoomMessageEventContentWithoutRelation::text_plain("uh i mean bonjour");

    // make_edit_event works, even if the event cache hasn't been enabled.
    room.make_edit_event(event_id, EditedContent::RoomMessage(new_content)).await.unwrap();
}

#[async_test]
async fn test_enable_encryption_doesnt_stay_unencrypted() {
    let mock = MatrixMockServer::new().await;
    let client = mock.client_builder().build().await;

    mock.mock_room_state_encryption().plain().mount().await;
    mock.mock_set_room_state_encryption().ok(event_id!("$1")).mount().await;

    let room_id = room_id!("!a:b.c");
    let room = mock.sync_joined_room(&client, room_id).await;

    assert!(!room.is_encrypted().await.unwrap());

    room.enable_encryption().await.expect("enabling encryption should work");

    mock.verify_and_reset().await;
    mock.mock_room_state_encryption().encrypted().mount().await;

    assert!(room.is_encrypted().await.unwrap());
}
