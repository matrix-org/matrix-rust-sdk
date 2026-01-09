use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::Duration,
};

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use futures_util::{future::join_all, pin_mut};
use matrix_sdk::{
    assert_next_with_timeout, assert_recv_with_timeout,
    config::SyncSettings,
    room::{Receipts, ReportedContentScore, RoomMemberRole, edit::EditedContent},
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_base::{EncryptionState, RoomMembersUpdate, RoomState};
use matrix_sdk_common::executor::spawn;
use matrix_sdk_test::{
    DEFAULT_TEST_ROOM_ID, InvitedRoomBuilder, JoinedRoomBuilder, RoomAccountDataTestEvent,
    SyncResponseBuilder, async_test,
    event_factory::EventFactory,
    mocks::mock_encryption_state,
    test_json::{self, sync::CUSTOM_ROOM_POWER_LEVELS},
};
use ruma::{
    OwnedUserId, RoomVersionId, TransactionId,
    api::client::{
        membership::Invite3pidInit, receipt::create_receipt::v3::ReceiptType,
        room::upgrade_room::v3::Request as UpgradeRoomRequest,
    },
    assign, event_id,
    events::{
        RoomAccountDataEventType, TimelineEventType,
        direct::DirectUserIdentifier,
        receipt::ReceiptThread,
        room::{
            member::MembershipState,
            message::{RoomMessageEventContent, RoomMessageEventContentWithoutRelation},
        },
    },
    int, mxc_uri, owned_event_id, room_id, thirdparty, user_id,
};
use serde_json::json;
use stream_assert::assert_pending;
use tokio::time::sleep;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{body_json, body_partial_json, header, method, path_regex},
};

use crate::{logged_in_client_with_server, mock_sync};
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
async fn test_leave_joined_room_does_not_forget_it() -> Result<(), anyhow::Error> {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = *DEFAULT_TEST_ROOM_ID;

    server.mock_room_leave().ok(room_id).mock_once().mount().await;
    // The forget endpoint should never be called
    server.mock_room_forget().ok().never().mount().await;

    let room = server.sync_joined_room(&client, room_id).await;

    room.leave().await?;

    assert_eq!(room.state(), RoomState::Left);

    // The left room is still around and in the right state
    let left_room = client.get_room(room_id);
    assert!(left_room.is_some());
    assert_eq!(left_room.unwrap().state(), RoomState::Left);

    Ok(())
}

#[async_test]
async fn test_leave_invited_room_also_forgets_it() -> Result<(), anyhow::Error> {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = *DEFAULT_TEST_ROOM_ID;

    server.mock_room_leave().ok(room_id).mock_once().mount().await;
    server.mock_room_forget().ok().mock_once().mount().await;

    let invited_room_builder = InvitedRoomBuilder::new(room_id);
    let room = server.sync_room(&client, invited_room_builder).await;

    room.leave().await?;

    assert_eq!(room.state(), RoomState::Left);

    let forgotten_room = client.get_room(room_id);
    assert!(forgotten_room.is_none());

    Ok(())
}

#[async_test]
async fn test_leave_room_also_leaves_predecessor() -> Result<(), anyhow::Error> {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let user = user_id!("@example:localhost");
    let room_a_id = room_id!("!room_a_id:localhost");
    let room_b_id = room_id!("!room_b_id:localhost");
    let create_room_a_event_id = event_id!("$create_room_a_event_id:localhost");
    let tombstone_room_a_event_id = event_id!("$tombstone_room_a_event_id:localhost");
    let create_room_b_event_id = event_id!("$create_room_b_event_id:localhost");

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::new(room_a_id).add_state_event(
                    EventFactory::new()
                        .create(user, RoomVersionId::V1)
                        .room(room_a_id)
                        .sender(user)
                        .event_id(create_room_a_event_id),
                ),
            );
        })
        .await;

    let room_a = client.get_room(room_a_id).expect("Room A not created");

    server.mock_upgrade_room().ok_with(room_b_id).mount().await;
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder
                .add_joined_room(
                    JoinedRoomBuilder::new(room_a_id).add_state_event(
                        EventFactory::new()
                            .room_tombstone("This room (A) repelaced by Room B!", room_b_id)
                            .room(room_a_id)
                            .sender(user)
                            .event_id(tombstone_room_a_event_id),
                    ),
                )
                .add_joined_room(
                    JoinedRoomBuilder::new(room_b_id).add_state_event(
                        EventFactory::new()
                            .create(user, RoomVersionId::V2)
                            .predecessor(room_a_id)
                            .room(room_b_id)
                            .sender(user)
                            .event_id(create_room_b_event_id),
                    ),
                );
        })
        .await;

    let upgrade_req = UpgradeRoomRequest::new(room_a_id.to_owned(), RoomVersionId::V11);
    let _response = client.send(upgrade_req).await?;

    assert!(room_a.is_tombstoned(), "Room A not tombstoned.");

    let room_b = client.get_room(room_b_id).expect("Room B not created");

    server.mock_room_leave().ok(room_b_id).mount().await;

    assert_eq!(room_a.state(), RoomState::Joined, "Room A not Joined");
    assert_eq!(room_b.state(), RoomState::Joined, "Room B not Joined");

    room_b.leave().await?;

    assert_eq!(room_b.state(), RoomState::Left, "Room B not Left");
    assert_eq!(room_a.state(), RoomState::Left, "Room A not Left");

    Ok(())
}

#[async_test]
async fn test_leave_predecessor_before_successor_no_error() -> Result<(), anyhow::Error> {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let user = user_id!("@example:localhost");
    let room_a_id = room_id!("!room_a_id:localhost");
    let room_b_id = room_id!("!room_b_id:localhost");
    let create_room_a_event_id = event_id!("$create_room_a_event_id:localhost");
    let tombstone_room_a_event_id = event_id!("$tombstone_room_a_event_id:localhost");
    let create_room_b_event_id = event_id!("$create_room_b_event_id:localhost");

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::new(room_a_id).add_state_event(
                    EventFactory::new()
                        .create(user, RoomVersionId::V1)
                        .room(room_a_id)
                        .sender(user)
                        .event_id(create_room_a_event_id),
                ),
            );
        })
        .await;

    let room_a = client.get_room(room_a_id).expect("Room A not created");

    server.mock_upgrade_room().ok_with(room_b_id).mount().await;
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder
                .add_joined_room(
                    JoinedRoomBuilder::new(room_a_id).add_state_event(
                        EventFactory::new()
                            .room_tombstone("This room (A) repelaced by Room B!", room_b_id)
                            .room(room_a_id)
                            .sender(user)
                            .event_id(tombstone_room_a_event_id),
                    ),
                )
                .add_joined_room(
                    JoinedRoomBuilder::new(room_b_id).add_state_event(
                        EventFactory::new()
                            .create(user, RoomVersionId::V2)
                            .predecessor(room_a_id)
                            .room(room_b_id)
                            .sender(user)
                            .event_id(create_room_b_event_id),
                    ),
                );
        })
        .await;

    let upgrade_req = UpgradeRoomRequest::new(room_a_id.to_owned(), RoomVersionId::V11);
    let _response = client.send(upgrade_req).await?;

    assert!(room_a.is_tombstoned(), "Room A not tombstoned.");

    let room_b = client.get_room(room_b_id).expect("Room B not created");

    server.mock_room_leave().ok(room_b_id).mount().await;

    assert_eq!(room_a.state(), RoomState::Joined, "Room A not Joined");
    assert_eq!(room_b.state(), RoomState::Joined, "Room B not Joined");

    let res = room_a.leave().await;

    assert_eq!(room_b.state(), RoomState::Joined, "Room B Left");
    assert_eq!(room_a.state(), RoomState::Left, "Room A not Left");
    assert!(res.is_ok(), "Error leaving Room A");

    let res = room_b.leave().await;

    assert_eq!(room_b.state(), RoomState::Left, "Room B not Left");
    assert_eq!(room_a.state(), RoomState::Left, "Room A not Left");
    assert!(res.is_ok(), "Error leaving Room B: {res:?}");

    Ok(())
}

#[async_test]
async fn test_leave_room_with_fake_predecessor_no_error() -> Result<(), anyhow::Error> {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let user = user_id!("@example:localhost");
    let room_id = room_id!("!room_id:localhost");
    let fake_room_id = room_id!("!fake_room_id:localhost");
    let create_room_event_id = event_id!("$create_room_event_id:localhost");

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::new(room_id).add_state_event(
                    EventFactory::new()
                        .create(user, RoomVersionId::V2)
                        .predecessor(fake_room_id)
                        .room(room_id)
                        .sender(user)
                        .event_id(create_room_event_id),
                ),
            );
        })
        .await;

    let room = client.get_room(room_id).expect("Room not created");

    server.mock_room_leave().ok(room_id).mount().await;

    assert_eq!(room.state(), RoomState::Joined, "Room not Joined");

    let res = room.leave().await;

    assert_eq!(room.state(), RoomState::Left, "Room not Left");
    assert!(res.is_ok(), "Error leaving Room: {res:?}");

    Ok(())
}

#[async_test]
async fn test_leave_room_fails_with_error() -> Result<(), anyhow::Error> {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let user = user_id!("@example:localhost");
    let room_id = room_id!("!room_id:localhost");
    let create_room_event_id = event_id!("$create_room_event_id:localhost");

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_joined_room(
                JoinedRoomBuilder::new(room_id).add_state_event(
                    EventFactory::new()
                        .create(user, RoomVersionId::V2)
                        .room(room_id)
                        .sender(user)
                        .event_id(create_room_event_id),
                ),
            );
        })
        .await;

    let room = client.get_room(room_id).expect("Room not created");

    server
        .mock_room_leave()
        .respond_with(
            ResponseTemplate::new(429).set_body_json(json!({"errcode": "M_LIMIT_EXCEEDED"})),
        )
        .mount()
        .await;

    assert_eq!(room.state(), RoomState::Joined, "Room not Joined");

    let res = room.leave().await;

    assert!(res.is_err(), "Should have returned Error");
    assert_eq!(room.state(), RoomState::Joined, "Room should still be joined");

    Ok(())
}

/// This test reflects a particular use case where a user is trying to leave a
/// room and the server replies the user is forbidden to do so.
#[async_test]
async fn test_leave_invited_room_with_no_permissions() -> Result<(), anyhow::Error> {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = *DEFAULT_TEST_ROOM_ID;

    server.mock_room_leave().forbidden().mock_once().mount().await;
    server.mock_room_forget().ok().mock_once().mount().await;

    let invited_room_builder = InvitedRoomBuilder::new(room_id);
    let room = server.sync_room(&client, invited_room_builder).await;

    room.leave().await?;

    assert_eq!(room.state(), RoomState::Left);

    let forgotten_room = client.get_room(room_id);
    assert!(forgotten_room.is_none());

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
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!test:example.org");

    // Initial sync with our test room.
    let room = server.sync_joined_room(&client, room_id).await;
    assert!(!room.is_marked_unread());

    // Setting the room as unread makes a request.
    {
        let _guard = server
            .mock_set_room_account_data(RoomAccountDataEventType::MarkedUnread)
            .ok()
            .expect(1)
            .mount_as_scoped()
            .await;
        room.set_unread_flag(true).await.unwrap();
    }

    // Unsetting the room as unread is a no-op.
    room.set_unread_flag(false).await.unwrap();

    // Now we mark the room as unread.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_account_data(RoomAccountDataTestEvent::MarkedUnread),
        )
        .await;
    assert!(room.is_marked_unread());

    // Unsetting the room as unread makes a request.
    {
        let _guard = server
            .mock_set_room_account_data(RoomAccountDataEventType::MarkedUnread)
            .ok()
            .expect(1)
            .mount_as_scoped()
            .await;
        room.set_unread_flag(false).await.unwrap();
    }

    // Setting the room as unread is a no-op.
    room.set_unread_flag(true).await.unwrap();
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
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_send_receipt(ReceiptType::Read).ok().mock_once().mount().await;

    // Initial sync with our test room.
    let room = server.sync_joined_room(&client, room_id!("!test:example.org")).await;

    room.send_single_receipt(
        ReceiptType::Read,
        ReceiptThread::Unthreaded,
        owned_event_id!("$xxxxxx:example.org"),
    )
    .await
    .unwrap();
}

#[async_test]
async fn test_send_single_receipt_with_unread_flag() {
    let event_id = owned_event_id!("$xxxxxx:example.org");

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_send_receipt(ReceiptType::Read).ok().expect(2).mount().await;

    // Initial sync with our test room, marked unread.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_account_data(RoomAccountDataTestEvent::MarkedUnread),
        )
        .await;
    assert!(room.is_marked_unread());

    // An unthreaded receipt triggers a marked unread update.
    {
        let _guard = server
            .mock_set_room_account_data(RoomAccountDataEventType::MarkedUnread)
            .ok()
            .mock_once()
            .mount_as_scoped()
            .await;

        room.send_single_receipt(ReceiptType::Read, ReceiptThread::Unthreaded, event_id.clone())
            .await
            .unwrap();
    }

    // A threaded read receipt update doesn't trigger a marked unread update.
    room.send_single_receipt(ReceiptType::Read, ReceiptThread::Main, event_id).await.unwrap();
}

#[async_test]
async fn test_send_multiple_receipts() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_send_read_markers().ok().mock_once().mount().await;

    // Initial sync with our test room.
    let room = server.sync_joined_room(&client, room_id!("!test:example.org")).await;

    let receipts = Receipts::new().fully_read_marker(owned_event_id!("$xxxxxx:example.org"));
    room.send_multiple_receipts(receipts).await.unwrap();
}

#[async_test]
async fn test_send_multiple_receipts_with_unread_flag() {
    let event_id = owned_event_id!("$xxxxxx:example.org");

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_send_read_markers().ok().mock_once().mount().await;
    server
        .mock_set_room_account_data(RoomAccountDataEventType::MarkedUnread)
        .ok()
        .mock_once()
        .mount()
        .await;

    // Initial sync with our test room, marked unread.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_account_data(RoomAccountDataTestEvent::MarkedUnread),
        )
        .await;
    assert!(room.is_marked_unread());

    // Sending receipts triggers a marked unread update.
    room.send_multiple_receipts(Receipts::new().public_read_receipt(event_id.clone()))
        .await
        .unwrap();
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
    let response = room.send(content).with_transaction_id(txn_id).await.unwrap().response;

    assert_eq!(event_id!("$h29iv0s8:example.com"), response.event_id)
}

#[async_test]
async fn test_room_redact() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = *DEFAULT_TEST_ROOM_ID;
    let room = server.sync_joined_room(&client, room_id).await;

    let event_id = event_id!("$h29iv0s8:example.com");
    server.mock_room_redact().ok(event_id).mock_once().mount().await;

    let txn_id = TransactionId::new();
    let reason = Some("Indecent material");
    let response =
        room.redact(event_id!("$xxxxxxxx:example.com"), reason, Some(txn_id)).await.unwrap();

    assert_eq!(response.event_id, event_id);
}

#[cfg(not(target_family = "wasm"))]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_fetch_members_deduplication() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room = server.sync_joined_room(&client, &DEFAULT_TEST_ROOM_ID).await;

    // We don't need any members, we're just checking if we're correctly
    // deduplicating calls to the method.
    server.mock_get_members().ok(Vec::new()).mock_once().mount().await;

    let mut tasks = Vec::new();

    // Create N tasks that try to fetch the members.
    for _ in 0..5 {
        #[allow(unknown_lints, clippy::redundant_async_block)] // false positive
        let task = spawn({
            let room = room.clone();
            async move { room.sync_members().await }
        });

        tasks.push(task);
    }

    // Wait on all of them at once.
    join_all(tasks).await;
}

#[async_test]
async fn test_set_name() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room = server.sync_joined_room(&client, &DEFAULT_TEST_ROOM_ID).await;

    let name = "The room name";

    Mock::given(method("PUT"))
        .and(path_regex(r"^/_matrix/client/v3/rooms/.*/state/m.room.name/$"))
        .and(header("authorization", "Bearer 1234"))
        .and(body_json(json!({
            "name": name,
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .expect(1)
        .mount(server.server())
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
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let typing_sequences: Arc<Mutex<Vec<Vec<OwnedUserId>>>> = Arc::new(Mutex::new(Vec::new()));
    // The expected typing sequences that we will receive, note that the current
    // user_id is filtered out.
    let asserted_typing_sequences =
        vec![vec![user_id!("@alice:matrix.org"), user_id!("@bob:example.com")], vec![]];
    let room_id = room_id!("!test:example.org");

    // Initial sync with our test room.
    let room = server.sync_joined_room(&client, room_id).await;

    // Send to typing notification
    let join_handle = spawn({
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
    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_typing(f.typing(vec![
                user_id!("@alice:matrix.org"),
                user_id!("@bob:example.com"),
                user_id!("@example:localhost"),
            ])),
        )
        .await;

    // Then send a typing notification with no user typing
    server.sync_room(&client, JoinedRoomBuilder::new(room_id).add_typing(f.typing(vec![]))).await;

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
    assert_eq!(power_level_admin, int!(100));

    // This user either does not exist in the room or has no special power level
    let power_level_unknown =
        room.get_user_power_level(user_id!("@non-existing:localhost")).await.unwrap();
    assert_eq!(power_level_unknown, int!(0));
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

    let f = EventFactory::new();
    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::default());
    sync_builder.add_global_account_data(
        f.direct().add_user("invited@localhost.com".into(), *DEFAULT_TEST_ROOM_ID),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    mock_encryption_state(&server, false).await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    assert!(room.is_direct().await.unwrap());
    assert!(room.direct_targets().contains(<&DirectUserIdentifier>::from("invited@localhost.com")));
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
        .ok(f.text_msg("hi").event_id(event_id).sender(&user_id).room(room_id).into_event())
        .expect(1)
        .named("/event")
        .mount()
        .await;

    let new_content = RoomMessageEventContentWithoutRelation::text_plain("uh i mean bonjour");

    // make_edit_event works, even if the event cache hasn't been enabled.
    room.make_edit_event(event_id, EditedContent::RoomMessage(new_content)).await.unwrap();
}

#[async_test]
async fn test_enable_encryption_doesnt_stay_unknown() {
    let mock = MatrixMockServer::new().await;
    let client = mock.client_builder().build().await;

    let room_id = room_id!("!a:b.c");
    let room = mock.sync_joined_room(&client, room_id).await;

    assert_matches!(room.encryption_state(), EncryptionState::Unknown);

    mock.mock_room_state_encryption().plain().mount().await;
    mock.mock_set_room_state_encryption().ok(event_id!("$1")).mount().await;

    assert_matches!(room.latest_encryption_state().await.unwrap(), EncryptionState::NotEncrypted);

    room.enable_encryption().await.expect("enabling encryption should work");

    mock.verify_and_reset().await;
    mock.mock_room_state_encryption().encrypted().mount().await;

    assert_matches!(room.encryption_state(), EncryptionState::Unknown);

    assert!(room.latest_encryption_state().await.unwrap().is_encrypted());
    assert_matches!(room.encryption_state(), EncryptionState::Encrypted);
}

#[cfg(feature = "experimental-encrypted-state-events")]
#[async_test]
async fn test_enable_state_encryption_doesnt_stay_unknown() {
    let mock = MatrixMockServer::new().await;
    let client = mock.client_builder().build().await;

    let room_id = room_id!("!a:b.c");
    let room = mock.sync_joined_room(&client, room_id).await;

    assert_matches!(room.encryption_state(), EncryptionState::Unknown);

    mock.mock_room_state_encryption().plain().mount().await;
    mock.mock_set_room_state_encryption().ok(event_id!("$1")).mount().await;

    assert_matches!(room.latest_encryption_state().await.unwrap(), EncryptionState::NotEncrypted);

    room.enable_encryption_with_state_event_encryption()
        .await
        .expect("enabling encryption should work");

    mock.verify_and_reset().await;
    mock.mock_room_state_encryption().state_encrypted().mount().await;

    assert_matches!(room.encryption_state(), EncryptionState::Unknown);

    assert!(room.latest_encryption_state().await.unwrap().is_state_encrypted());
    assert_matches!(room.encryption_state(), EncryptionState::StateEncrypted);
}

#[async_test]
async fn test_subscribe_to_knock_requests() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_room_state_encryption().plain().mount().await;

    let room_id = room_id!("!a:b.c");
    let f = EventFactory::new().room(room_id);

    let user_id = user_id!("@alice:b.c");
    let knock_event_id = event_id!("$alice-knock:b.c");
    let knock_event =
        f.member(user_id).membership(MembershipState::Knock).event_id(knock_event_id).into_raw();

    server.mock_get_members().ok(vec![knock_event]).mock_once().mount().await;

    let room = server.sync_joined_room(&client, room_id).await;
    let (stream, handle) = room.subscribe_to_knock_requests().await.unwrap();

    pin_mut!(stream);

    // We receive an initial knock request from Alice
    let initial = assert_next_with_timeout!(stream, 1000);
    assert_eq!(initial.len(), 1);

    let knock_request = &initial[0];
    assert_eq!(knock_request.event_id, knock_event_id);
    assert!(!knock_request.is_seen);

    // We then mark the knock request as seen
    room.mark_knock_requests_as_seen(&[user_id.to_owned()]).await.unwrap();

    // Now it's received again as seen
    let seen = assert_next_with_timeout!(stream, 1000);
    assert_eq!(initial.len(), 1);
    let seen_knock = &seen[0];
    assert_eq!(seen_knock.event_id, knock_event_id);
    assert!(seen_knock.is_seen);

    // If we then receive a new member event for Alice that's not 'knock'
    let joined_room_builder = JoinedRoomBuilder::new(room_id)
        .add_state_bulk(vec![f.member(user_id).membership(MembershipState::Invite).into()]);
    server.sync_room(&client, joined_room_builder).await;

    // The knock requests are now empty because we have new member events
    let updated_requests = assert_next_with_timeout!(stream, 1000);
    assert!(updated_requests.is_empty());

    // And it's emitted again because the seen id value has changed
    let updated_requests = assert_next_with_timeout!(stream);
    assert!(updated_requests.is_empty());

    // There should be no other knock requests
    assert_pending!(stream);

    // The seen knock request id is no longer there because the associated knock
    // request doesn't exist anymore
    let seen_knock_request_ids = room
        .get_seen_knock_request_ids()
        .await
        .expect("could not get current seen knock request ids");
    assert!(seen_knock_request_ids.is_empty());

    handle.abort();
}

#[async_test]
async fn test_subscribe_to_knock_requests_reloads_members_on_limited_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_room_state_encryption().plain().mount().await;

    let room_id = room_id!("!a:b.c");
    let f = EventFactory::new().room(room_id);

    let user_id = user_id!("@alice:b.c");
    let knock_event = f.member(user_id).membership(MembershipState::Knock).into_raw();

    server
        .mock_get_members()
        .ok(vec![knock_event])
        // The endpoint will be called twice:
        // 1. For the initial loading of room members.
        // 2. When a gappy (limited) sync is received.
        .expect(2)
        .mount()
        .await;

    let room = server.sync_joined_room(&client, room_id).await;
    let (stream, handle) = room.subscribe_to_knock_requests().await.unwrap();

    pin_mut!(stream);

    // We receive an initial knock request from Alice
    let initial = assert_next_with_timeout!(stream, 500);
    assert!(!initial.is_empty());

    // This limited sync should trigger a new emission of knock requests, with a
    // reloading of the room members
    server.sync_room(&client, JoinedRoomBuilder::new(room_id).set_timeline_limited()).await;

    // We should receive a new list of knock requests
    assert_next_with_timeout!(stream, 500);

    // There should be no other knock requests
    assert_pending!(stream);

    handle.abort();
}

#[async_test]
async fn test_remove_outdated_seen_knock_requests_ids_when_membership_changed() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_room_state_encryption().plain().mount().await;

    let room_id = room_id!("!a:b.c");
    let f = EventFactory::new().room(room_id);

    let user_id = user_id!("@alice:b.c");
    let knock_event_id = event_id!("$alice-knock:b.c");
    let knock_event =
        f.member(user_id).membership(MembershipState::Knock).event_id(knock_event_id).into();

    // When syncing the room, we'll have a knock request coming from alice
    let room = server
        .sync_room(&client, JoinedRoomBuilder::new(room_id).add_state_bulk(vec![knock_event]))
        .await;

    // We then mark the knock request as seen
    room.mark_knock_requests_as_seen(&[user_id.to_owned()]).await.unwrap();

    // Now it's received again as seen
    let seen = room.get_seen_knock_request_ids().await.unwrap();
    assert_eq!(seen.len(), 1);

    // If we then load the members again and the previously knocking member is in
    // another state now
    let joined_event = f.member(user_id).membership(MembershipState::Join).into_raw();

    server.mock_get_members().ok(vec![joined_event]).mock_once().mount().await;

    room.mark_members_missing();
    room.sync_members().await.expect("could not reload room members");

    // Calling remove outdated seen knock request ids will remove the seen id
    room.remove_outdated_seen_knock_requests_ids()
        .await
        .expect("could not remove outdated seen knock request ids");

    let seen = room.get_seen_knock_request_ids().await.unwrap();
    assert!(seen.is_empty());
}

#[async_test]
async fn test_remove_outdated_seen_knock_requests_ids_when_we_have_an_outdated_knock() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_room_state_encryption().plain().mount().await;

    let room_id = room_id!("!a:b.c");
    let f = EventFactory::new().room(room_id);

    let user_id = user_id!("@alice:b.c");
    let knock_event_id = event_id!("$alice-knock:b.c");
    let knock_event =
        f.member(user_id).membership(MembershipState::Knock).event_id(knock_event_id).into();

    // When syncing the room, we'll have a knock request coming from alice
    let room = server
        .sync_room(&client, JoinedRoomBuilder::new(room_id).add_state_bulk(vec![knock_event]))
        .await;

    // We then mark the knock request as seen
    room.mark_knock_requests_as_seen(&[user_id.to_owned()]).await.unwrap();

    // Now it's received again as seen
    let seen = room.get_seen_knock_request_ids().await.unwrap();
    assert_eq!(seen.len(), 1);

    // If we then load the members again and the previously knocking member has a
    // different event id
    let knock_event = f
        .member(user_id)
        .membership(MembershipState::Knock)
        .event_id(event_id!("$knock-2:b.c"))
        .into_raw();

    server.mock_get_members().ok(vec![knock_event]).mock_once().mount().await;

    room.mark_members_missing();
    room.sync_members().await.expect("could not reload room members");

    // Calling remove outdated seen knock request ids will remove the seen id
    room.remove_outdated_seen_knock_requests_ids()
        .await
        .expect("could not remove outdated seen knock request ids");

    let seen = room.get_seen_knock_request_ids().await.unwrap();
    assert!(seen.is_empty());
}

#[async_test]
async fn test_subscribe_to_knock_requests_clears_seen_ids_on_member_reload() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_room_state_encryption().plain().mount().await;

    let room_id = room_id!("!a:b.c");
    let f = EventFactory::new().room(room_id);

    let user_id = user_id!("@alice:b.c");
    let knock_event_id = event_id!("$alice-knock:b.c");
    let knock_event =
        f.member(user_id).membership(MembershipState::Knock).event_id(knock_event_id).into_raw();

    server.mock_get_members().ok(vec![knock_event]).mock_once().mount().await;

    let room = server.sync_joined_room(&client, room_id).await;
    let (stream, handle) = room.subscribe_to_knock_requests().await.unwrap();

    pin_mut!(stream);

    // We receive an initial knock request from Alice
    let initial = assert_next_with_timeout!(stream, 100);
    assert_eq!(initial.len(), 1);

    let knock_request = &initial[0];
    assert_eq!(knock_request.event_id, knock_event_id);
    assert!(!knock_request.is_seen);

    // We then mark the knock request as seen
    room.mark_knock_requests_as_seen(&[user_id.to_owned()]).await.unwrap();

    // Now it's received again as seen
    let seen = assert_next_with_timeout!(stream, 100);
    assert_eq!(seen.len(), 1);
    let seen_knock = &seen[0];
    assert_eq!(seen_knock.event_id, knock_event_id);
    assert!(seen_knock.is_seen);

    // If we then load the members again and the previously knocking member is in
    // another state now
    let joined_event = f.member(user_id).membership(MembershipState::Join).into_raw();

    server.mock_get_members().ok(vec![joined_event]).mock_once().mount().await;

    room.mark_members_missing();
    room.sync_members().await.expect("could not reload room members");

    // The knock requests are now empty because we have new member events
    let updated_requests = assert_next_with_timeout!(stream, 100);
    assert!(updated_requests.is_empty());

    // There should be no other knock requests
    assert_pending!(stream);

    // Give some time for the seen ids purging to be done
    sleep(Duration::from_millis(100)).await;

    // The seen knock request id is no longer there because the associated knock
    // request doesn't exist anymore
    let seen_knock_request_ids = room
        .get_seen_knock_request_ids()
        .await
        .expect("could not get current seen knock request ids");
    assert!(seen_knock_request_ids.is_empty());

    handle.abort();
}

#[async_test]
async fn test_room_member_updates_sender_on_full_member_reload() {
    use assert_matches::assert_matches;
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a:b.c");
    let room = server.sync_joined_room(&client, room_id).await;

    let mut receiver = room.room_member_updates_sender.subscribe();
    assert!(receiver.is_empty());

    // When loading the full room member list
    let user_id = user_id!("@alice:b.c");
    let joined_event = EventFactory::new()
        .room(room_id)
        .member(user_id)
        .membership(MembershipState::Join)
        .into_raw();
    server.mock_get_members().ok(vec![joined_event]).mock_once().mount().await;
    room.sync_members().await.expect("could not reload room members");

    // The member updates sender emits a full reload
    let next = assert_recv_with_timeout!(receiver, 100);
    assert_matches!(next, RoomMembersUpdate::FullReload);
}

#[async_test]
async fn test_room_member_updates_sender_on_partial_members_update() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a:b.c");
    let room = server.sync_joined_room(&client, room_id).await;

    let mut receiver = room.room_member_updates_sender.subscribe();
    assert!(receiver.is_empty());

    // When loading a few room member updates
    let user_id = user_id!("@alice:b.c");
    let joined_event =
        EventFactory::new().room(room_id).member(user_id).membership(MembershipState::Join).into();
    server
        .sync_room(&client, JoinedRoomBuilder::new(room_id).add_state_bulk(vec![joined_event]))
        .await;

    // The member updates sender emits a partial update with the user ids of the
    // members
    let next = assert_recv_with_timeout!(receiver, 100);
    assert_let!(RoomMembersUpdate::Partial(user_ids) = next);
    assert_eq!(user_ids, BTreeSet::from_iter(vec![user_id!("@alice:b.c").to_owned()]));
}

#[async_test]
async fn test_report_room() {
    let (client, server) = logged_in_client_with_server().await;
    let reason = "this makes me sad";

    Mock::given(method("POST"))
        .and(path_regex(r"^/_matrix/client/.*/rooms/.*/report$"))
        .and(body_json(json!({
            "reason": reason,
        })))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EMPTY))
        .mount(&server)
        .await;

    mock_sync(&server, &*test_json::SYNC, None).await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let _response = client.sync_once(sync_settings).await.unwrap();
    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();

    room.report_room(reason.to_owned()).await.unwrap();
}

#[async_test]
async fn test_set_own_member_display_name() {
    // Given a room with an existing member event for the current user.
    let (client, server) = logged_in_client_with_server().await;
    let mut sync_builder = SyncResponseBuilder::new();
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let room_id = &DEFAULT_TEST_ROOM_ID;
    let user_id = client.user_id().unwrap();

    let existing_name = "Old Name";
    let existing_avatar_url = mxc_uri!("mxc://example.org/avA7ar");

    let member_event = EventFactory::new()
        .member(user_id)
        .display_name(existing_name.to_owned())
        .avatar_url(existing_avatar_url)
        .membership(MembershipState::Join);

    let response = sync_builder
        .add_joined_room(JoinedRoomBuilder::new(room_id).add_state_event(member_event))
        .build_json_sync_response();

    mock_sync(&server, response, None).await;
    client.sync_once(sync_settings).await.unwrap();

    let room = client.get_room(room_id).unwrap();

    // When setting a new display name.
    let new_name = "New Name";

    // Then the user's display name is updated without changing the avatar URL.
    Mock::given(method("PUT"))
        .and(path_regex(format!(r"^/_matrix/client/r0/rooms/.*/state/m.room.member/{user_id}")))
        .and(header("authorization", "Bearer 1234"))
        .and(body_partial_json(json!({
            "displayname": new_name,
            "avatar_url": existing_avatar_url.to_string(),
            "membership": "join"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::EVENT_ID))
        .expect(1)
        .mount(&server)
        .await;

    room.set_own_member_display_name(Some(new_name.to_owned())).await.unwrap();
}
