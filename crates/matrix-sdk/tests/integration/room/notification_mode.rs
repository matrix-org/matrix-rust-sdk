use std::time::Duration;

use assert_matches::assert_matches;
use matrix_sdk::{
    SlidingSyncList, config::SyncSettings, notification_settings::RoomNotificationMode,
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_base::RoomState;
use matrix_sdk_test::{
    DEFAULT_TEST_ROOM_ID, InvitedRoomBuilder, JoinedRoomBuilder, SyncResponseBuilder, async_test,
    event_factory::EventFactory,
};
use ruma::{
    api::client::sync::sync_events::v5,
    events::AnyGlobalAccountDataEvent,
    push::{Action, ConditionalPushRule, NewSimplePushRule, Ruleset, Tweak},
    room_id,
    serde::Raw,
};
use serde_json::json;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{header, method, path_regex},
};

use crate::{logged_in_client_with_server, mock_sync};

#[async_test]
async fn test_get_notification_mode() {
    let room_no_rules_id = room_id!("!jEsUZKDJdhlrceRyVU:localhost");
    let room_not_joined_id = room_id!("!aBfUOMDJhmtucfVzGa:localhost");
    let (client, server) = logged_in_client_with_server().await;

    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    // Add the rooms for the tests
    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(&DEFAULT_TEST_ROOM_ID));
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_no_rules_id));
    sync_builder.add_invited_room(InvitedRoomBuilder::new(room_not_joined_id));

    let f = EventFactory::new();

    let mut ruleset = Ruleset::default();
    ruleset.override_ =
        [ConditionalPushRule::master(), ConditionalPushRule::suppress_notices()].into();
    ruleset.room.insert(
        NewSimplePushRule::new(
            (*DEFAULT_TEST_ROOM_ID).into(),
            vec![Action::Notify, Action::SetTweak(Tweak::Sound("default".into()))],
        )
        .into(),
    );
    ruleset.underride = [
        ConditionalPushRule::call(),
        ConditionalPushRule::room_one_to_one(),
        ConditionalPushRule::invite_for_me(client.user_id().unwrap()),
        ConditionalPushRule::member_event(),
        ConditionalPushRule::message(),
    ]
    .into();

    sync_builder.add_global_account_data(f.push_rules(ruleset));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
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

#[async_test]
async fn test_cached_notification_mode_is_updated_when_syncing() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    // If we receive a sliding sync response with custom rules for a room
    let mut ruleset = Ruleset::default();
    ruleset.override_ =
        [ConditionalPushRule::master(), ConditionalPushRule::suppress_notices()].into();
    ruleset.room.insert(
        NewSimplePushRule::new(
            (*DEFAULT_TEST_ROOM_ID).into(),
            vec![Action::Notify, Action::SetTweak(Tweak::Sound("default".into()))],
        )
        .into(),
    );
    ruleset.underride = [
        ConditionalPushRule::call(),
        ConditionalPushRule::room_one_to_one(),
        ConditionalPushRule::invite_for_me(client.user_id().unwrap()),
        ConditionalPushRule::member_event(),
        ConditionalPushRule::message(),
    ]
    .into();
    let f = EventFactory::new();
    let push_rules: Raw<AnyGlobalAccountDataEvent> = f.push_rules(ruleset).into_raw();
    let mut response = v5::Response::new("pos".to_owned());
    response.extensions.account_data.global.push(push_rules);
    response.rooms.insert(DEFAULT_TEST_ROOM_ID.to_owned(), v5::response::Room::default());

    server
        .mock_sliding_sync()
        .ok_and_run(
            &client,
            |builder| builder.add_list(SlidingSyncList::builder("rooms")),
            response,
        )
        .await;

    server.verify_and_reset().await;

    // We can later check the custom mode is applied
    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).expect("Room not found");
    assert_eq!(
        room.cached_user_defined_notification_mode(),
        Some(RoomNotificationMode::AllMessages)
    );

    // Now if we receive a response with no custom push rules for the room, just the
    // base ones
    let mut ruleset = Ruleset::default();
    ruleset.underride = [
        ConditionalPushRule::call(),
        ConditionalPushRule::room_one_to_one(),
        ConditionalPushRule::invite_for_me(client.user_id().unwrap()),
        ConditionalPushRule::member_event(),
        ConditionalPushRule::message(),
    ]
    .into();

    let f = EventFactory::new();
    let push_rules: Raw<AnyGlobalAccountDataEvent> = f.push_rules(ruleset).into_raw();
    let mut response = v5::Response::new("pos".to_owned());
    response.extensions.account_data.global.push(push_rules);

    server
        .mock_sliding_sync()
        .ok_and_run(
            &client,
            |builder| builder.add_list(SlidingSyncList::builder("rooms")),
            response,
        )
        .await;

    server.verify_and_reset().await;

    // And the custom notification mode for the room has been removed
    let room = client.get_room(&DEFAULT_TEST_ROOM_ID).expect("Room not found");
    assert!(room.cached_user_defined_notification_mode().is_none());
}
