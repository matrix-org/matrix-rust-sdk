use std::sync::Arc;

use assert_matches::assert_matches;
use matrix_sdk::{
    Client, SlidingSyncList, notification_settings::RoomNotificationMode,
    test_utils::mocks::MatrixMockServer,
};
use matrix_sdk_base::{
    RoomState,
    cross_process_lock::CrossProcessLockConfig,
    store::{MemoryStore, StateChanges, StoreConfig},
};
use matrix_sdk_test::{
    InvitedRoomBuilder, JoinedRoomBuilder, TestRoom, async_test, event_factory::EventFactory,
};
use ruma::{
    RoomId,
    api::client::sync::sync_events::msc4186,
    events::{AnyGlobalAccountDataEvent, GlobalAccountDataEventType},
    push::{Action, ConditionalPushRule, NewSimplePushRule, Ruleset, SoundTweakValue, Tweak},
    serde::Raw,
};
use serde_json::json;

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

/// Build a client on top of `state_store`, which outlives the clients like
/// the on-disk store of an app that gets restarted.
async fn build_client_with_state_store(
    server: &MatrixMockServer,
    state_store: Arc<MemoryStore>,
) -> Client {
    server
        .client_builder()
        .on_builder(move |builder| {
            builder.store_config(
                StoreConfig::new(CrossProcessLockConfig::MultiProcess {
                    holder_name: "test".to_owned(),
                })
                .state_store(state_store),
            )
        })
        .build()
        .await
}

/// An `m.push_rules` event muting `muted_room`, or without any room-specific
/// rule if `muted_room` is `None`.
fn push_rules_event(muted_room: Option<&RoomId>) -> Raw<AnyGlobalAccountDataEvent> {
    let override_rules = match muted_room {
        Some(room_id) => json!([{
            "actions": [],
            "rule_id": room_id,
            "default": false,
            "enabled": true,
            "conditions": [{ "kind": "event_match", "key": "room_id", "pattern": room_id }],
        }]),
        None => json!([]),
    };

    Raw::from_json_string(
        json!({
            "type": "m.push_rules",
            "content": { "global": { "override": override_rules } },
        })
        .to_string(),
    )
    .unwrap()
}

/// Save `event` in the state store, without going through a sync.
async fn save_push_rules(client: &Client, event: Raw<AnyGlobalAccountDataEvent>) {
    let mut changes = StateChanges::default();
    changes.account_data.insert(GlobalAccountDataEventType::PushRules, event);
    client.state_store().save_changes(&changes).await.unwrap();
}

#[async_test]
async fn test_user_defined_notification_mode_updates_and_persists_the_cache() {
    let server = MatrixMockServer::new().await;
    let state_store = Arc::new(MemoryStore::new());
    let room_id: &RoomId = TestRoom::Default.into();

    // First run: the room is synced without any push rule, then the user mutes
    // it. The new rules are only in the store: no sync updated the cache.
    {
        let client = build_client_with_state_store(&server, state_store.clone()).await;
        server
            .mock_sync()
            .ok_and_run(&client, |builder| {
                builder.add_joined_room(JoinedRoomBuilder::default());
            })
            .await;

        save_push_rules(&client, push_rules_event(Some(room_id))).await;

        let room = client.get_room(room_id).unwrap();
        assert_eq!(room.user_defined_notification_mode().await, Some(RoomNotificationMode::Mute));
        assert_eq!(room.cached_user_defined_notification_mode(), Some(RoomNotificationMode::Mute));
    }

    // Restart: the cache filled by `user_defined_notification_mode()` must have
    // been persisted.
    {
        let client = build_client_with_state_store(&server, state_store.clone()).await;
        let room = client.get_room(room_id).unwrap();
        assert_eq!(
            room.cached_user_defined_notification_mode(),
            Some(RoomNotificationMode::Mute),
            "the cached mode must survive a restart",
        );

        // The user unmutes the room: the room-specific rule is gone, so the
        // stale cached value must be cleared.
        save_push_rules(&client, push_rules_event(None)).await;

        assert_eq!(room.user_defined_notification_mode().await, None);
        assert_eq!(
            room.cached_user_defined_notification_mode(),
            None,
            "the cached mode must be cleared when the rule is removed",
        );
    }

    // Restart again: the cleared cache must have been persisted too.
    let client = build_client_with_state_store(&server, state_store).await;
    let room = client.get_room(room_id).unwrap();
    assert_eq!(room.cached_user_defined_notification_mode(), None);
}

#[async_test]
async fn test_cached_notification_mode_is_updated_and_persisted_by_sync_v2() {
    let server = MatrixMockServer::new().await;
    let state_store = Arc::new(MemoryStore::new());
    let room_id: &RoomId = TestRoom::Default.into();

    // First run: a `/sync` (v2) mutes the room.
    {
        let client = build_client_with_state_store(&server, state_store.clone()).await;
        server
            .mock_sync()
            .ok_and_run(&client, |builder| {
                builder
                    .add_joined_room(JoinedRoomBuilder::default())
                    .add_global_account_data(push_rules_event(Some(room_id)));
            })
            .await;

        let room = client.get_room(room_id).unwrap();
        assert_eq!(
            room.cached_user_defined_notification_mode(),
            Some(RoomNotificationMode::Mute),
            "the sync must fill the cache",
        );
    }

    // Restart: before the first sync, the cached mode is read from the store.
    {
        let client = build_client_with_state_store(&server, state_store.clone()).await;
        let room = client.get_room(room_id).unwrap();
        assert_eq!(
            room.cached_user_defined_notification_mode(),
            Some(RoomNotificationMode::Mute),
            "the cached mode must survive a restart",
        );

        // A new sync unmutes the room, which doesn't appear in the response.
        server
            .mock_sync()
            .ok_and_run(&client, |builder| {
                builder.add_global_account_data(push_rules_event(None));
            })
            .await;

        assert_eq!(room.cached_user_defined_notification_mode(), None);
    }

    // Restart again: the cleared cache must have been persisted too.
    let client = build_client_with_state_store(&server, state_store).await;
    let room = client.get_room(room_id).unwrap();
    assert_eq!(room.cached_user_defined_notification_mode(), None);
}
