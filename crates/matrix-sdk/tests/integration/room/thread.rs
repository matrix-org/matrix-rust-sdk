use assert_matches2::assert_matches;
use matrix_sdk::{
    notification_settings::RoomNotificationMode,
    room::ThreadSubscription,
    test_utils::mocks::{MatrixMockServer, PushRuleIdSpec},
};
use matrix_sdk_test::{ALICE, JoinedRoomBuilder, async_test, event_factory::EventFactory};
use ruma::{event_id, owned_event_id, push::RuleKind, room_id};

#[async_test]
async fn test_subscribe_thread() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!test:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    let root_id = owned_event_id!("$root");

    server
        .mock_room_put_thread_subscription()
        .match_room_id(room_id.to_owned())
        .match_thread_id(root_id.clone())
        .ok()
        .mock_once()
        .mount()
        .await;

    // I can subscribe to a thread.
    room.subscribe_thread(root_id.clone(), Some(root_id.clone())).await.unwrap();

    server
        .mock_room_get_thread_subscription()
        .match_room_id(room_id.to_owned())
        .match_thread_id(root_id.clone())
        .ok(true)
        .mock_once()
        .mount()
        .await;

    // I can get the subscription for that same thread.
    let subscription = room.fetch_thread_subscription(root_id.clone()).await.unwrap().unwrap();
    assert_eq!(subscription, ThreadSubscription { automatic: true });

    // If I try to get a subscription for a thread event that's unknown, I get no
    // `ThreadSubscription`, not an error.
    let subscription =
        room.fetch_thread_subscription(owned_event_id!("$another_root")).await.unwrap();
    assert!(subscription.is_none());

    // I can also unsubscribe from a thread.
    server
        .mock_room_delete_thread_subscription()
        .match_room_id(room_id.to_owned())
        .match_thread_id(root_id.clone())
        .ok()
        .mock_once()
        .mount()
        .await;

    room.unsubscribe_thread(root_id.clone()).await.unwrap();

    // Now, if I retry to get the subscription for this thread, it doesn't exist
    // anymore.
    let subscription = room.fetch_thread_subscription(root_id.clone()).await.unwrap();
    assert_matches!(subscription, None);

    // Subscribing automatically to the thread may also return a `M_SKIPPED` error
    // that should be non-fatal.
    server
        .mock_room_put_thread_subscription()
        .match_room_id(room_id.to_owned())
        .match_thread_id(root_id.clone())
        .conflicting_unsubscription()
        .mock_once()
        .mount()
        .await;

    room.subscribe_thread(root_id.clone(), Some(root_id.clone())).await.unwrap();

    // And in this case, the thread is still unsubscribed.
    let subscription = room.fetch_thread_subscription(root_id).await.unwrap();
    assert_matches!(subscription, None);
}

#[async_test]
async fn test_subscribe_thread_if_needed() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!test:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    // If there's no prior subscription, the function `subscribe_thread_if_needed`
    // will automatically subscribe to the thread, whether the new subscription
    // is automatic or not.
    for (root_id, automatic) in [
        (owned_event_id!("$root"), None),
        (owned_event_id!("$woot"), Some(owned_event_id!("$woot"))),
    ] {
        server
            .mock_room_put_thread_subscription()
            .match_room_id(room_id.to_owned())
            .match_thread_id(root_id.clone())
            .ok()
            .mock_once()
            .mount()
            .await;

        room.subscribe_thread_if_needed(&root_id, automatic).await.unwrap();
    }

    // If there's a prior automatic subscription, the function
    // `subscribe_thread_if_needed` will only subscribe to the thread if the new
    // subscription is manual.
    {
        let root_id = owned_event_id!("$toot");

        server
            .mock_room_get_thread_subscription()
            .match_room_id(room_id.to_owned())
            .match_thread_id(root_id.clone())
            .ok(true)
            .mock_once()
            .mount()
            .await;

        server
            .mock_room_put_thread_subscription()
            .match_room_id(room_id.to_owned())
            .match_thread_id(root_id.clone())
            .ok()
            .mock_once()
            .mount()
            .await;

        room.subscribe_thread_if_needed(&root_id, None).await.unwrap();
    }

    // Otherwise, it will be a no-op.
    {
        let root_id = owned_event_id!("$foot");

        server
            .mock_room_get_thread_subscription()
            .match_room_id(room_id.to_owned())
            .match_thread_id(root_id.clone())
            .ok(true)
            .mock_once()
            .mount()
            .await;

        room.subscribe_thread_if_needed(&root_id, Some(owned_event_id!("$foot"))).await.unwrap();
    }

    // The function `subscribe_thread_if_needed` is a no-op if there's a prior
    // manual subscription, whether the new subscription is automatic or not.
    for (root_id, automatic) in [
        (owned_event_id!("$root"), None),
        (owned_event_id!("$woot"), Some(owned_event_id!("$woot"))),
    ] {
        server
            .mock_room_get_thread_subscription()
            .match_room_id(room_id.to_owned())
            .match_thread_id(root_id.clone())
            .ok(false)
            .mock_once()
            .mount()
            .await;

        // No-op! (The PUT endpoint hasn't been mocked, so this would result in a 404 if
        // it were trying to hit it.)
        room.subscribe_thread_if_needed(&root_id, automatic).await.unwrap();
    }
}

#[async_test]
async fn test_thread_push_rule_is_triggered_for_subscribed_threads() {
    // This test checks that the evaluation of push rules for threads will correctly
    // call `Room::fetch_thread_subscription` for threads.

    let server = MatrixMockServer::new().await;
    let client = server
        .client_builder()
        .on_builder(|builder| {
            builder.with_threading_support(matrix_sdk::ThreadingSupport::Enabled {
                with_subscriptions: true,
            })
        })
        .build()
        .await;

    let room_id = room_id!("!test:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    let thread_root_id = owned_event_id!("$root");
    let f = EventFactory::new().room(room_id).sender(*ALICE);

    // Make it so that the client has a member event for the current user.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_event(f.member(client.user_id().unwrap())),
        )
        .await;

    // Sanity check: we can get a push context.
    let push_context = room
        .push_context()
        .await
        .expect("getting a push context works")
        .expect("the push context should exist");

    // Mock the thread subscriptions endpoint so the user is subscribed to the
    // thread.
    server
        .mock_room_get_thread_subscription()
        .match_room_id(room_id.to_owned())
        .match_thread_id(thread_root_id.clone())
        .ok(true)
        .mock_once()
        .mount()
        .await;

    // Given an event in the thread I'm subscribed to, the push rule evaluation will
    // trigger the thread subscription endpoint,
    let event =
        f.text_msg("hello to you too!").in_thread(&thread_root_id, &thread_root_id).into_raw_sync();

    // And the event will trigger a notification.
    let actions = push_context.for_event(&event).await;
    assert!(actions.iter().any(|action| action.should_notify()));

    // But for a thread that I haven't subscribed to (i.e. the endpoint returns 404,
    // because it's not set up), no actions are returned.
    let another_thread_root_id = event_id!("$another_root");
    let event = f
        .text_msg("bonjour à vous également !")
        .in_thread(another_thread_root_id, another_thread_root_id)
        .into_raw_sync();

    let actions = push_context.for_event(&event).await;
    assert!(actions.is_empty());
}

#[async_test]
async fn test_thread_push_rules_and_notification_modes() {
    // This test checks that, given a combination of a global notification mode, and
    // a room notification mode, we do get notifications for thread events according
    // to the subscriptions.

    let server = MatrixMockServer::new().await;
    let client = server
        .client_builder()
        .on_builder(|builder| {
            builder.with_threading_support(matrix_sdk::ThreadingSupport::Enabled {
                with_subscriptions: true,
            })
        })
        .build()
        .await;

    let room_id = room_id!("!test:example.org");
    let f = EventFactory::new().room(room_id).sender(*ALICE);
    // Make it so that the client has a member event for the current user.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_state_event(f.member(client.user_id().unwrap())),
        )
        .await;

    // Sanity check: we can get a push context.
    room.push_context()
        .await
        .expect("getting a push context works")
        .expect("the push context should exist");

    // Mock push rules endpoints, allowing any modification to any rule.
    server.mock_set_push_rules_actions(RuleKind::Underride, PushRuleIdSpec::Any).ok().mount().await;
    server.mock_set_push_rules_actions(RuleKind::Room, PushRuleIdSpec::Any).ok().mount().await;
    server.mock_set_push_rules(RuleKind::Underride, PushRuleIdSpec::Any).ok().mount().await;
    server.mock_set_push_rules(RuleKind::Room, PushRuleIdSpec::Any).ok().mount().await;
    server.mock_set_push_rules(RuleKind::Override, PushRuleIdSpec::Any).ok().mount().await;
    server.mock_delete_push_rules(RuleKind::Room, PushRuleIdSpec::Any).ok().mount().await;
    server.mock_delete_push_rules(RuleKind::Override, PushRuleIdSpec::Any).ok().mount().await;

    // Given a thread I'm subscribed to,
    let thread_root_id = owned_event_id!("$root");

    server
        .mock_room_get_thread_subscription()
        .match_room_id(room_id.to_owned())
        .match_thread_id(thread_root_id.clone())
        .ok(true)
        .mount()
        .await;

    // Given an event in the thread I'm subscribed to,
    let event =
        f.text_msg("hello to you too!").in_thread(&thread_root_id, &thread_root_id).into_raw_sync();

    // If global mode = AllMessages,
    let settings = client.notification_settings().await;

    let is_encrypted = false;
    let is_one_to_one = false;
    settings
        .set_default_room_notification_mode(
            is_encrypted.into(),
            is_one_to_one.into(),
            RoomNotificationMode::AllMessages,
        )
        .await
        .unwrap();

    // If room mode = AllMessages,
    settings.set_room_notification_mode(room_id, RoomNotificationMode::AllMessages).await.unwrap();
    // Ack the push rules change via sync, for it to be applied in the push context.
    let ruleset = settings.ruleset().await;
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_global_account_data(f.push_rules(ruleset));
        })
        .await;

    // The thread event will trigger a notification.
    let actions = room.push_context().await.unwrap().unwrap().traced_for_event(&event).await;
    assert!(actions.iter().any(|action| action.should_notify()));

    // If room mode = mentions and keywords only,
    settings
        .set_room_notification_mode(room_id, RoomNotificationMode::MentionsAndKeywordsOnly)
        .await
        .unwrap();
    let ruleset = settings.ruleset().await;
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_global_account_data(f.push_rules(ruleset));
        })
        .await;

    // The thread event will trigger a notification.
    let actions = room.push_context().await.unwrap().unwrap().traced_for_event(&event).await;
    assert!(actions.iter().any(|action| action.should_notify()));

    // If room mode = mute,
    settings.set_room_notification_mode(room_id, RoomNotificationMode::Mute).await.unwrap();
    let ruleset = settings.ruleset().await;
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_global_account_data(f.push_rules(ruleset));
        })
        .await;

    // The thread event will not trigger a notification, as the room has been muted.
    let actions = room.push_context().await.unwrap().unwrap().traced_for_event(&event).await;
    assert!(!actions.iter().any(|action| action.should_notify()));

    // Now, if global mode = mentions only,
    settings
        .set_default_room_notification_mode(
            is_encrypted.into(),
            is_one_to_one.into(),
            RoomNotificationMode::MentionsAndKeywordsOnly,
        )
        .await
        .unwrap();

    // If room mode = AllMessages,
    settings.set_room_notification_mode(room_id, RoomNotificationMode::AllMessages).await.unwrap();
    // Ack the push rules change via sync, for it to be applied in the push context.
    let ruleset = settings.ruleset().await;
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_global_account_data(f.push_rules(ruleset));
        })
        .await;

    // The thread event will trigger a notification.
    let actions = room.push_context().await.unwrap().unwrap().traced_for_event(&event).await;
    assert!(actions.iter().any(|action| action.should_notify()));

    // If room mode = Mentions only,
    settings
        .set_room_notification_mode(room_id, RoomNotificationMode::MentionsAndKeywordsOnly)
        .await
        .unwrap();
    // Ack the push rules change via sync, for it to be applied in the push context.
    let ruleset = settings.ruleset().await;
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_global_account_data(f.push_rules(ruleset));
        })
        .await;

    // The thread event will trigger a notification.
    let actions = room.push_context().await.unwrap().unwrap().traced_for_event(&event).await;
    assert!(actions.iter().any(|action| action.should_notify()));

    // If room mode = mute,
    settings.set_room_notification_mode(room_id, RoomNotificationMode::Mute).await.unwrap();
    // Ack the push rules change via sync, for it to be applied in the push context.
    let ruleset = settings.ruleset().await;
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_global_account_data(f.push_rules(ruleset));
        })
        .await;

    // The thread event will not trigger a notification, as the room has been muted.
    let actions = room.push_context().await.unwrap().unwrap().traced_for_event(&event).await;
    assert!(!actions.iter().any(|action| action.should_notify()));
}
