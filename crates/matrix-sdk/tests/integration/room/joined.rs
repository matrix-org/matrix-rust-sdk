use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::future::join_all;
use matrix_sdk::{
    attachment::{
        AttachmentConfig, AttachmentInfo, BaseImageInfo, BaseThumbnailInfo, BaseVideoInfo,
        Thumbnail,
    },
    config::SyncSettings,
    room::{Receipts, ReportedContentScore, RoomMemberRole},
};
use matrix_sdk_base::RoomState;
use matrix_sdk_test::{
    async_test, test_json, test_json::sync::CUSTOM_ROOM_POWER_LEVELS, EphemeralTestEvent,
    JoinedRoomBuilder, SyncResponseBuilder, DEFAULT_TEST_ROOM_ID,
};
use ruma::{
    api::client::{membership::Invite3pidInit, receipt::create_receipt::v3::ReceiptType},
    assign, event_id,
    events::{receipt::ReceiptThread, room::message::RoomMessageEventContent, TimelineEventType},
    int, mxc_uri, owned_event_id, room_id, thirdparty, uint, user_id, OwnedUserId, TransactionId,
};
use serde_json::json;
use wiremock::{
    matchers::{body_json, body_partial_json, header, method, path, path_regex},
    Mock, ResponseTemplate,
};

use crate::{logged_in_client_with_server, mock_encryption_state, mock_sync, synced_client};

#[async_test]
async fn invite_user_by_id() {
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
async fn invite_user_by_3pid() {
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
async fn leave_room() -> Result<(), anyhow::Error> {
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
async fn ban_user() {
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
async fn unban_user() {
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
async fn kick_user() {
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
async fn send_single_receipt() {
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
async fn send_multiple_receipts() {
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
async fn typing_notice() {
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
async fn room_state_event_send() {
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
async fn room_message_send() {
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
    let response = room.send(content).with_transaction_id(&txn_id).await.unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[async_test]
async fn room_attachment_send() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_partial_json(json!({
            "info": {
                "mimetype": "image/jpeg",
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/media/r0/upload"))
        .and(header("authorization", "Bearer 1234"))
        .and(header("content-type", "image/jpeg"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
          "content_uri": "mxc://example.com/AQwafuaFswefuhsfAFAgsw"
        })))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;
    mock_encryption_state(&server, false).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let response = room
        .send_attachment(
            "image",
            &mime::IMAGE_JPEG,
            b"Hello world".to_vec(),
            AttachmentConfig::new(),
        )
        .await
        .unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[async_test]
async fn room_attachment_send_info() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_partial_json(json!({
            "info": {
                "mimetype": "image/jpeg",
                "h": 600,
                "w": 800,
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/media/r0/upload"))
        .and(header("authorization", "Bearer 1234"))
        .and(header("content-type", "image/jpeg"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
          "content_uri": "mxc://example.com/AQwafuaFswefuhsfAFAgsw"
        })))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;
    mock_encryption_state(&server, false).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let config = AttachmentConfig::new()
        .info(AttachmentInfo::Image(BaseImageInfo {
            height: Some(uint!(600)),
            width: Some(uint!(800)),
            size: None,
            blurhash: None,
        }))
        .caption(Some("image caption".to_owned()));

    let response = room
        .send_attachment("image.jpg", &mime::IMAGE_JPEG, b"Hello world".to_vec(), config)
        .await
        .unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[async_test]
async fn room_attachment_send_wrong_info() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_partial_json(json!({
            "info": {
                "mimetype": "image/jpeg",
                "h": 600,
                "w": 800,
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/media/r0/upload"))
        .and(header("authorization", "Bearer 1234"))
        .and(header("content-type", "image/jpeg"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
          "content_uri": "mxc://example.com/AQwafuaFswefuhsfAFAgsw"
        })))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;
    mock_encryption_state(&server, false).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let config = AttachmentConfig::new()
        .info(AttachmentInfo::Video(BaseVideoInfo {
            height: Some(uint!(600)),
            width: Some(uint!(800)),
            duration: Some(Duration::from_millis(3600)),
            size: None,
            blurhash: None,
        }))
        .caption(Some("image caption".to_owned()));

    let response =
        room.send_attachment("image.jpg", &mime::IMAGE_JPEG, b"Hello world".to_vec(), config).await;

    response.unwrap_err();
}

#[async_test]
async fn room_attachment_send_info_thumbnail() {
    let (client, server) = logged_in_client_with_server().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/send/.*"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_partial_json(json!({
            "info": {
                "mimetype": "image/jpeg",
                "h": 600,
                "w": 800,
                "thumbnail_info": {
                    "h": 360,
                    "w": 480,
                    "mimetype":"image/jpeg",
                    "size": 3600,
                },
                "thumbnail_url": "mxc://example.com/AQwafuaFswefuhsfAFAgsw",
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/_matrix/media/r0/upload"))
        .and(header("authorization", "Bearer 1234"))
        .and(header("content-type", "image/jpeg"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
          "content_uri": "mxc://example.com/AQwafuaFswefuhsfAFAgsw"
        })))
        .expect(2)
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;
    mock_encryption_state(&server, false).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let config = AttachmentConfig::with_thumbnail(Thumbnail {
        data: b"Thumbnail".to_vec(),
        content_type: mime::IMAGE_JPEG,
        info: Some(BaseThumbnailInfo {
            height: Some(uint!(360)),
            width: Some(uint!(480)),
            size: Some(uint!(3600)),
        }),
    })
    .info(AttachmentInfo::Image(BaseImageInfo {
        height: Some(uint!(600)),
        width: Some(uint!(800)),
        size: None,
        blurhash: None,
    }));

    let response = room
        .send_attachment("image", &mime::IMAGE_JPEG, b"Hello world".to_vec(), config)
        .await
        .unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[async_test]
async fn room_redact() {
    let (client, server) = synced_client().await;

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/redact/.*?/.*?"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .mount(&server)
        .await;

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    let event_id = event_id!("$xxxxxxxx:example.com");

    let txn_id = TransactionId::new();
    let reason = Some("Indecent material");
    let response = room.redact(event_id, reason, Some(txn_id)).await.unwrap();

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[cfg(not(target_arch = "wasm32"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_members_deduplication() {
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
        // Expect that we're only going to send the request out once.
        .expect(1..=1)
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
async fn set_name() {
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
async fn report_content() {
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
async fn subscribe_to_typing_notifications() {
    let (client, server) = logged_in_client_with_server().await;
    let typing_sequences: Arc<Mutex<Vec<Vec<OwnedUserId>>>> = Arc::new(Mutex::new(Vec::new()));
    // The expected typing sequences that we will receive, note that the current
    // user_id is filtered out.
    let asserted_typing_sequences =
        vec![vec![user_id!("@alice:matrix.org"), user_id!("@bob:example.com")], vec![]];
    let room_id = room_id!("!test:example.org");
    let mut ev_builder = SyncResponseBuilder::new();

    // Initial sync with our test room.
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id));
    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
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
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_ephemeral_event(
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
    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Then send a typing notification with no user typing
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_ephemeral_event(
        EphemeralTestEvent::Custom(json!({
            "content": {
                "user_ids": []
            },
            "room_id": "!jEsUZKDJdhlrceRyVU:example.org",
            "type": "m.typing"
        })),
    ));
    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    join_handle.await.unwrap();
    assert_eq!(typing_sequences.lock().unwrap().to_vec(), asserted_typing_sequences);
}

#[async_test]
async fn get_suggested_user_role() {
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
async fn get_power_level_for_user() {
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
async fn get_users_with_power_levels() {
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
async fn get_users_with_power_levels_is_empty_if_power_level_info_is_not_available() {
    let (client, server) = logged_in_client_with_server().await;

    mock_sync(&server, &*test_json::INVITE_SYNC, None).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings).await.unwrap();
    // The room doesn't have any power level info
    let room = client.get_room(room_id!("!696r7674:example.com")).unwrap();

    assert!(room.users_with_power_levels().await.is_empty());
}

#[async_test]
async fn reset_power_levels() {
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

    let initial_power_levels = room.room_power_levels().await.unwrap();
    assert_eq!(initial_power_levels.events[&TimelineEventType::RoomAvatar], int!(100));

    room.reset_power_levels().await.unwrap();
}
