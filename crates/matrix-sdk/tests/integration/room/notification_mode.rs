use assert_matches::assert_matches;
use matrix_sdk::{
    SlidingSyncList, notification_settings::RoomNotificationMode,
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_base::RoomState;
use matrix_sdk_test::{
    InvitedRoomBuilder, JoinedRoomBuilder, TestRoom, async_test, event_factory::EventFactory,
};
use ruma::{
    api::client::sync::sync_events::msc4186,
    events::AnyGlobalAccountDataEvent,
    push::{Action, ConditionalPushRule, NewSimplePushRule, Ruleset, SoundTweakValue, Tweak},
    serde::Raw,
};

#[async_test]
async fn test_get_notification_mode() {
    let room_default_id = TestRoom::Default;
    let room_no_rules_id = TestRoom::Other;
    let room_not_joined_id = TestRoom::V12;
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();
    let f = EventFactory::new();

    // Add the rooms for the tests
    let mut ruleset = Ruleset::default();
    ruleset.override_ =
        [ConditionalPushRule::master(), ConditionalPushRule::suppress_notices()].into();
    ruleset.room.insert(
        NewSimplePushRule::new(
            room_default_id.into(),
            vec![Action::Notify, Action::SetTweak(Tweak::Sound(SoundTweakValue::Default))],
        )
        .into(),
    );
    ruleset.underride = [
        ConditionalPushRule::call(),
        ConditionalPushRule::room_one_to_one(),
        ConditionalPushRule::invite_for_me(user_id),
        ConditionalPushRule::member_event(),
        ConditionalPushRule::message(),
    ]
    .into();

    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder
                .add_joined_room(JoinedRoomBuilder::default())
                .add_joined_room(JoinedRoomBuilder::new(room_no_rules_id))
                .add_invited_room(InvitedRoomBuilder::new(room_not_joined_id))
                .add_global_account_data(f.push_rules(ruleset));
        })
        .await;

    // Joined room with a user-defined rule
    let room = client.get_room(room_default_id.into()).unwrap();
    assert_eq!(room.state(), RoomState::Joined);
    let mode = room.notification_mode().await;
    assert_matches!(mode, Some(RoomNotificationMode::AllMessages));

    // Joined room without user-defined rules As this room has no user-defined
    // rules, the encryption status will be fetched
    server.mock_room_state_encryption().encrypted().expect(1).mount().await;

    // With a room without specific notification rules
    let room = client.get_room(room_no_rules_id.into()).unwrap();
    assert_eq!(room.state(), RoomState::Joined);
    // getting the mode should return the default one
    let mode = room.notification_mode().await;
    assert_matches!(mode, Some(RoomNotificationMode::MentionsAndKeywordsOnly));

    // getting the user-defined mode must return None
    let mode = room.user_defined_notification_mode().await;
    assert_matches!(mode, None);

    // Room not joined
    let room = client.get_room(room_not_joined_id.into()).unwrap();
    assert_eq!(room.state(), RoomState::Invited);
    let mode = room.notification_mode().await;
    assert_eq!(mode, None);
}

#[async_test]
async fn test_cached_notification_mode_is_updated_when_syncing() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().unwrap();
    let room_id = TestRoom::Default;

    // If we receive a sliding sync response with custom rules for a room
    let mut ruleset = Ruleset::default();
    ruleset.override_ =
        [ConditionalPushRule::master(), ConditionalPushRule::suppress_notices()].into();
    ruleset.room.insert(
        NewSimplePushRule::new(
            room_id.into(),
            vec![Action::Notify, Action::SetTweak(Tweak::Sound(SoundTweakValue::Default))],
        )
        .into(),
    );
    ruleset.underride = [
        ConditionalPushRule::call(),
        ConditionalPushRule::room_one_to_one(),
        ConditionalPushRule::invite_for_me(user_id),
        ConditionalPushRule::member_event(),
        ConditionalPushRule::message(),
    ]
    .into();
    let f = EventFactory::new();
    let push_rules: Raw<AnyGlobalAccountDataEvent> = f.push_rules(ruleset).into_raw();
    let mut response = msc4186::Response::new("pos".to_owned());
    response.extensions.account_data.global.push(push_rules);
    response.rooms.insert(room_id.into(), msc4186::response::Room::default());

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
    let room = client.get_room(room_id.into()).expect("Room not found");
    assert_eq!(
        room.cached_user_defined_notification_mode(),
        Some(RoomNotificationMode::AllMessages)
    );

    // Now if we receive a response with no custom push rules for the room, just
    // the base ones
    let mut ruleset = Ruleset::default();
    ruleset.underride = [
        ConditionalPushRule::call(),
        ConditionalPushRule::room_one_to_one(),
        ConditionalPushRule::invite_for_me(user_id),
        ConditionalPushRule::member_event(),
        ConditionalPushRule::message(),
    ]
    .into();

    let f = EventFactory::new();
    let push_rules: Raw<AnyGlobalAccountDataEvent> = f.push_rules(ruleset).into_raw();
    let mut response = msc4186::Response::new("pos".to_owned());
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
    let room = client.get_room(room_id.into()).expect("Room not found");
    assert!(room.cached_user_defined_notification_mode().is_none());
}
