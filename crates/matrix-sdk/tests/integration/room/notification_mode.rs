use std::time::Duration;

use assert_matches::assert_matches;
use matrix_sdk::{config::SyncSettings, notification_settings::RoomNotificationMode};
use matrix_sdk_base::RoomState;
use matrix_sdk_test::{
    async_test, GlobalAccountDataTestEvent, InvitedRoomBuilder, JoinedRoomBuilder,
    SyncResponseBuilder, DEFAULT_TEST_ROOM_ID,
};
use ruma::room_id;
use serde_json::json;
use wiremock::{
    matchers::{header, method, path_regex},
    Mock, ResponseTemplate,
};

use crate::{logged_in_client_with_server, mock_sync};

#[async_test]
async fn get_notification_mode() {
    let room_no_rules_id = room_id!("!jEsUZKDJdhlrceRyVU:localhost");
    let room_not_joined_id = room_id!("!aBfUOMDJhmtucfVzGa:localhost");
    let (client, server) = logged_in_client_with_server().await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    // Add the rooms for the tests
    let mut ev_builder = SyncResponseBuilder::new();
    ev_builder.add_joined_room(JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID));
    ev_builder.add_joined_room(JoinedRoomBuilder::new(room_no_rules_id));
    ev_builder.add_invited_room(InvitedRoomBuilder::new(room_not_joined_id));
    ev_builder.add_global_account_data_event(GlobalAccountDataTestEvent::PushRules);

    mock_sync(&server, ev_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Joined room with a user-defined rule
    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).unwrap();
    assert_eq!(room.state(), RoomState::Joined);
    let mode = room.notification_mode().await;
    assert_matches!(mode, Some(RoomNotificationMode::AllMessages));

    // Joined room without user-defined rules
    // As this room has no user-defined rules, the encryption status will be fetched
    Mock::given(method("GET"))
        .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/m.room.encryption/"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({
                    "algorithm": "m.megolm.v1.aes-sha2",
                    "rotation_period_ms": 604800000,
                    "rotation_period_msgs": 100
                }))
                // Introduce a delay so the first `is_encrypted()` doesn't finish before we make
                // the second call.
                .set_delay(Duration::from_millis(50)),
        )
        .mount(&server)
        .await;

    // With a room without specific notification rules
    let room = client.get_room(room_no_rules_id).unwrap();
    assert_eq!(room.state(), RoomState::Joined);
    // getting the mode should return the default one
    let mode = room.notification_mode().await;
    assert_matches!(mode, Some(RoomNotificationMode::MentionsAndKeywordsOnly));

    // getting the user-defined mode must return None
    let mode = room.user_defined_notification_mode().await;
    assert_matches!(mode, None);

    // Room not joined
    let room = client.get_room(room_not_joined_id).unwrap();
    assert_eq!(room.state(), RoomState::Invited);
    let mode = room.notification_mode().await;
    assert_eq!(mode, None);
}
