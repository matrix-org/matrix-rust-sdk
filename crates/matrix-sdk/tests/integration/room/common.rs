use std::{assert_matches, ops::Not};

use js_int::uint;
use matrix_sdk::{
    RoomDisplayName, RoomMemberships,
    room::RoomMember,
    test_utils::mocks::{MatrixMockServer, RoomContextResponseTemplate},
};
use matrix_sdk_base::DmRoomDefinition;
use matrix_sdk_test::{
    InvitedRoomBuilder, JoinedRoomBuilder, LeftRoomBuilder, TestRoom, TestRoomAlias, User,
    async_test, bulk_room_members, event_factory::EventFactory,
};
use ruma::{
    event_id,
    events::{
        AnyGlobalAccountDataEvent, AnySyncStateEvent, AnySyncTimelineEvent,
        GlobalAccountDataEventType, StateEventType,
        room::{avatar, member::MembershipState},
    },
    mxc_uri, room_version_id,
    serde::Raw,
    user_id,
};
use serde_json::json;
use strass::{assert_let, assert_variant_eq};

#[async_test]
async fn test_user_presence() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let f = EventFactory::new().default_room();

    server
        .mock_get_members()
        .ok(vec![f.member(User::Alice).into(), f.member(User::Bob).into()])
        .expect(1)
        .mount()
        .await;

    let room = server.sync_default_joined_room(&client).await;
    let members: Vec<RoomMember> = room.members(RoomMemberships::ACTIVE).await.unwrap();

    assert_eq!(2, members.len());
}

#[async_test]
async fn test_calculate_room_names_from_summary() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().expect("client is logged in");
    let f = EventFactory::new();

    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_state_bulk([f.member(user_id).into(), f.member(User::Alice).into()])
                .set_room_summary(json!({
                    "m.joined_member_count": 2,
                    "m.invited_member_count": 0,
                    "m.heroes": [User::Alice],
                })),
        )
        .await;

    assert_variant_eq!(room.display_name().await.unwrap(), RoomDisplayName::Calculated("alice"));
}

#[async_test]
async fn test_room_names() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let own_user_id = client.user_id().unwrap();
    let f = EventFactory::new().sender(User::Alice);

    // Room with a canonical alias.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_state_event(f.canonical_alias(Some(TestRoomAlias::Canonical.into()), vec![])),
        )
        .await;

    assert_variant_eq!(room.display_name().await.unwrap(), RoomDisplayName::Aliased("canonical"));

    // Room with a name.
    let room = server
        .sync_room(
            &client,
            InvitedRoomBuilder::new(TestRoom::Invited).add_state_event(f.room_name("My Room Name")),
        )
        .await;

    assert_variant_eq!(room.display_name().await.unwrap(), RoomDisplayName::Named("My Room Name"));

    let own_left_member_event =
        f.member(own_user_id).sender(own_user_id).leave().into_raw_sync_state();

    // Left room with a lot of members.
    let room = server
        .sync_room(
            &client,
            LeftRoomBuilder::new(TestRoom::Left)
                .add_state_bulk(bulk_room_members(0, 0..15, "localhost", &MembershipState::Join))
                .add_timeline_event(own_left_member_event.clone().cast()),
        )
        .await;

    assert_variant_eq!(
        room.display_name().await.unwrap(),
        RoomDisplayName::Calculated("user_0, user_1, user_10, user_11, user_12, and 10 others")
    );

    // Room with joined and invited members.
    let room = server
        .sync_room(
            &client,
            LeftRoomBuilder::new(TestRoom::Other).add_state_bulk([
                f.member(User::Alice).into_raw_sync_state(),
                f.member(User::Alice).invited(User::Bob).display_name("Bob").into_raw_sync_state(),
                f.member(User::Carol).sender(User::Carol).leave().into_raw_sync_state(),
                own_left_member_event.clone(),
            ]),
        )
        .await;

    assert_variant_eq!(
        room.display_name().await.unwrap(),
        RoomDisplayName::Calculated("Bob, alice")
    );

    // Room with only left members.
    let room = server
        .sync_room(
            &client,
            LeftRoomBuilder::new(TestRoom::V12)
                .add_state_bulk(bulk_room_members(0, 0..3, "localhost", &MembershipState::Leave))
                .add_timeline_event(own_left_member_event.cast()),
        )
        .await;

    assert_variant_eq!(
        room.display_name().await.unwrap(),
        RoomDisplayName::EmptyWas("user_0, user_1, user_2")
    );
}

#[async_test]
async fn test_state_event_getting() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let f = EventFactory::new().sender(User::Alice);

    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_state_bulk([
                f.room_encryption().into(),
                Raw::new(&json!({
                    "type": "m.custom.note",
                    "sender": "@example:localhost",
                    "content": {
                        "body": "Note 1",
                    },
                    "state_key": "note.1",
                    "origin_server_ts": 1611853078727u64,
                    "unsigned": {
                        "replaces_state": "$2s9GcbVxbbFS3EZY9vN1zhavaDJnF32cAIGAxi99NuQ",
                        "age": 15458166523u64
                    },
                    "event_id": "$NVCTvrlxodf3ZGjJ6foxepEq8ysSkTq8wG0wKeQBVZg"
                }))
                .unwrap()
                .cast_unchecked(),
                Raw::new(&json!({
                    "type": "m.custom.note",
                    "sender": "@example2:localhost",
                    "content": {
                        "body": "Note 2",
                    },
                    "state_key": "note.2",
                    "origin_server_ts": 1611853078727u64,
                    "unsigned": {
                        "replaces_state": "$2s9GcbVxbbFS3EZY9vN1zhavaDJnF32cAIGAxi99NuQ",
                        "age": 15458166523u64
                    },
                    "event_id": "$NVCTvrlxodf3ZGjJ6foxepEq8ysSkTq8wG0wKeQBVZg"
                }))
                .unwrap()
                .cast_unchecked(),
            ]),
        )
        .await;

    let state_events = room.get_state_events(StateEventType::RoomEncryption).await.unwrap();
    assert_eq!(state_events.len(), 1);

    let state_events = room.get_state_events("m.custom.note".into()).await.unwrap();
    assert_eq!(state_events.len(), 2);

    let encryption_event = room
        .get_state_event(StateEventType::RoomEncryption, "")
        .await
        .unwrap()
        .unwrap()
        .deserialize()
        .unwrap();

    assert_matches!(encryption_event.as_sync(), Some(AnySyncStateEvent::RoomEncryption(_)));
}

#[async_test]
async fn test_room_route() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let f = EventFactory::new();

    // Without eligible server
    let ip_address_user_id = user_id!("@creator:127.0.0.1");
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_timeline_bulk([
                f.create(ip_address_user_id, room_version_id!("6"))
                    .sender(ip_address_user_id)
                    .into_raw_sync(),
                f.member(ip_address_user_id).into_raw_sync(),
            ]),
        )
        .await;

    let route = room.route().await.unwrap();
    assert_eq!(route.len(), 0);

    // With a single eligible server
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_timeline_event(f.member(User::Default)),
        )
        .await;

    let route = room.route().await.unwrap();
    assert_eq!(route, ["localhost"]);

    // With two eligible servers
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_timeline_state_bulk(bulk_room_members(
                0,
                0..15,
                "notarealhs",
                &MembershipState::Join,
            )),
        )
        .await;

    let route = room.route().await.unwrap();
    assert_eq!(route, ["notarealhs", "localhost"]);

    // With three eligible servers
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_timeline_state_bulk([
                f.member(User::Bob).into(),
                f.member(User::Carol).into(),
            ]),
        )
        .await;

    let route = room.route().await.unwrap();
    assert_eq!(route, ["notarealhs", "other.server", "localhost"]);

    // With four eligible servers
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_timeline_state_bulk(bulk_room_members(
                1,
                0..10,
                "yourmatrix",
                &MembershipState::Join,
            )),
        )
        .await;

    let route = room.route().await.unwrap();
    assert_eq!(route, ["notarealhs", "yourmatrix", "other.server"]);

    // With power levels
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_timeline_event(
                f.power_levels().set_users([(User::Default, 50)]).sender(ip_address_user_id),
            ),
        )
        .await;

    let route = room.route().await.unwrap();
    assert_eq!(route, ["localhost", "notarealhs", "yourmatrix"]);

    // With higher power levels
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_timeline_event(
                f.power_levels()
                    .set_users([(User::Default, 50), (User::Bob, 70)])
                    .sender(ip_address_user_id),
            ),
        )
        .await;

    let route = room.route().await.unwrap();
    assert_eq!(route, ["other.server", "notarealhs", "yourmatrix"]);

    // With server ACLs
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_timeline_event(
                f.server_acl(true, vec!["*".to_owned()], vec!["notarealhs".to_owned()])
                    .state_key("")
                    .sender(ip_address_user_id),
            ),
        )
        .await;

    let route = room.route().await.unwrap();
    assert_eq!(route, ["other.server", "yourmatrix", "localhost"]);
}

#[async_test]
async fn test_room_permalink() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().expect("client is logged in");
    let f = EventFactory::new().sender(user_id);

    // Without aliases
    let room_id_no_sigil = &TestRoom::Default.as_str()[1..];
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_timeline_event(f.member(user_id))
                .add_timeline_state_bulk(bulk_room_members(
                    1,
                    0..5,
                    "notarealhs",
                    &MembershipState::Join,
                )),
        )
        .await;

    assert_eq!(
        room.matrix_to_permalink().await.unwrap().to_string(),
        format!("https://matrix.to/#/!{room_id_no_sigil}?via=notarealhs&via=localhost")
    );
    assert_eq!(
        room.matrix_permalink(false).await.unwrap().to_string(),
        format!("matrix:roomid/{room_id_no_sigil}?via=notarealhs&via=localhost")
    );

    // With an alternative alias
    let alt_alias = TestRoomAlias::Alt;
    let alt_alias_no_sigil = &alt_alias.as_str()[1..];
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_timeline_event(f.canonical_alias(None, vec![alt_alias.into()])),
        )
        .await;

    assert_eq!(
        room.matrix_to_permalink().await.unwrap().to_string(),
        format!("https://matrix.to/#/%23{alt_alias_no_sigil}")
    );
    assert_eq!(
        room.matrix_permalink(false).await.unwrap().to_string(),
        format!("matrix:r/{alt_alias_no_sigil}")
    );

    // With a canonical alias
    let canonical_alias = TestRoomAlias::Canonical;
    let canonical_alias_no_sigil = &canonical_alias.as_str()[1..];
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_timeline_event(
                f.canonical_alias(Some(canonical_alias.into()), vec![alt_alias.into()]),
            ),
        )
        .await;

    assert_eq!(
        room.matrix_to_permalink().await.unwrap().to_string(),
        format!("https://matrix.to/#/%23{canonical_alias_no_sigil}")
    );
    assert_eq!(
        room.matrix_permalink(false).await.unwrap().to_string(),
        format!("matrix:r/{canonical_alias_no_sigil}")
    );
    assert_eq!(
        room.matrix_permalink(true).await.unwrap().to_string(),
        format!("matrix:r/{canonical_alias_no_sigil}?action=join")
    );
}

#[async_test]
async fn test_room_event_permalink() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().expect("client is logged in");
    let f = EventFactory::new().sender(user_id);
    let event_id = event_id!("$15139375512JaHAW");
    let event_id_no_sigil = &event_id.as_str()[1..];

    // Without aliases
    let room_id_no_sigil = &TestRoom::Default.as_str()[1..];
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_timeline_event(f.member(user_id))
                .add_timeline_state_bulk(bulk_room_members(
                    1,
                    0..5,
                    "notarealhs",
                    &MembershipState::Join,
                )),
        )
        .await;

    assert_eq!(
        room.matrix_to_event_permalink(event_id).await.unwrap().to_string(),
        format!(
            "https://matrix.to/#/!{room_id_no_sigil}/${event_id_no_sigil}?via=notarealhs&via=localhost"
        )
    );
    assert_eq!(
        room.matrix_event_permalink(event_id).await.unwrap().to_string(),
        format!(
            "matrix:roomid/{room_id_no_sigil}/e/{event_id_no_sigil}?via=notarealhs&via=localhost"
        )
    );

    // Adding an alias doesn't change anything
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_timeline_event(f.canonical_alias(
                Some(TestRoomAlias::Canonical.into()),
                vec![TestRoomAlias::Alt.into()],
            )),
        )
        .await;

    assert_eq!(
        room.matrix_to_event_permalink(event_id).await.unwrap().to_string(),
        format!(
            "https://matrix.to/#/!{room_id_no_sigil}/${event_id_no_sigil}?via=notarealhs&via=localhost"
        )
    );
    assert_eq!(
        room.matrix_event_permalink(event_id).await.unwrap().to_string(),
        format!(
            "matrix:roomid/{room_id_no_sigil}/e/{event_id_no_sigil}?via=notarealhs&via=localhost"
        )
    );
}

#[async_test]
async fn test_event() {
    let event_id = event_id!("$foun39djjod0f");

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().expect("client is logged in");

    let f = EventFactory::new().sender(user_id);
    let room = server
        .sync_room(
            &client,
            // We need the member event and power levels locally so the push
            // rules processor works.
            JoinedRoomBuilder::default()
                .add_state_event(f.member(user_id).display_name("example"))
                .add_state_event(f.power_levels()),
        )
        .await;

    // First, loading the event results in a 404.
    room.load_or_fetch_event(event_id, None).await.unwrap_err();

    let f = EventFactory::new();
    server
        .mock_room_event()
        .ok(f
            .room_tombstone("This room has been replaced", TestRoom::V12)
            .sender(User::Bob)
            .event_id(event_id)
            .room(TestRoom::Default)
            .into())
        .named("/event")
        .mock_once()
        .mount()
        .await;

    // Then, we can find it using a network query.
    let timeline_event = room.load_or_fetch_event(event_id, None).await.unwrap();
    assert_let!(
        AnySyncTimelineEvent::State(AnySyncStateEvent::RoomTombstone(event)) =
            timeline_event.raw().deserialize().unwrap()
    );
    assert_eq!(event.event_id(), event_id);

    let push_actions = timeline_event.push_actions().unwrap();
    assert!(push_actions.iter().any(|a| a.is_highlight()));
    assert!(push_actions.iter().any(|a| a.should_notify()));
}

#[async_test]
async fn test_event_with_context() {
    let event_id = event_id!("$cur1234");
    let prev_event_id = event_id!("$prev1234");
    let next_event_id = event_id!("$next_1234");

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let user_id = client.user_id().expect("client is logged in");
    let f = EventFactory::new().default_room().sender(user_id);

    // We need the member event and power levels locally so the push rules
    // processor works.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_state_event(f.member(user_id).display_name("example"))
                .add_state_event(f.power_levels()),
        )
        .await;

    let event = f.text_msg("The requested message").event_id(event_id);
    let event_before = f.text_msg("A previous message").event_id(prev_event_id);
    let event_next = f.text_msg("A newer message").event_id(next_event_id);

    server
        .mock_room_event_context()
        .room(TestRoom::Default)
        .match_event_id()
        .ok(RoomContextResponseTemplate::new(event.into())
            .events_before(vec![event_before.into()])
            .events_after(vec![event_next.into()]))
        .expect(1)
        .mount()
        .await;

    let context_ret = room.event_with_context(event_id, false, uint!(1), None).await.unwrap();

    assert_let!(Some(timeline_event) = context_ret.event);
    assert_eq!(timeline_event.event_id(), Some(event_id));

    assert_eq!(1, context_ret.events_before.len());
    assert_eq!(context_ret.events_before[0].event_id(), Some(prev_event_id));

    assert_eq!(1, context_ret.events_after.len());
    assert_eq!(context_ret.events_after[0].event_id(), Some(next_event_id));
}

#[async_test]
async fn test_is_direct() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let own_user_id = client.user_id().unwrap();
    let f = EventFactory::new().default_room().sender(own_user_id);

    // Initialize the room with 2 members, including ourself.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_state_event(f.member(own_user_id).display_name("example"))
                .add_state_event(f.member(User::Bob).sender(User::Bob)),
        )
        .await;

    // The room is not direct.
    assert!(room.direct_targets().is_empty());
    assert!(!room.is_direct().await.unwrap());

    // Setting the room as direct will request the members of the room.
    server
        .mock_get_members()
        .ok(vec![
            f.member(own_user_id).display_name("example").into(),
            f.member(User::Bob).sender(User::Bob).into(),
        ])
        .expect(1)
        .named("members")
        .mount()
        .await;

    server
        .mock_set_global_account_data(own_user_id, &GlobalAccountDataEventType::Direct)
        .expect_account_data(f.direct().add_user(User::Bob, TestRoom::Default).into_content())
        .ok()
        .mock_once()
        .named("set_direct")
        .mount()
        .await;

    // Set the room as direct.
    room.set_is_direct(true).await.unwrap();

    // Mock the sync response we should get from the homeserver.
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_global_account_data(f.direct().add_user(User::Bob, TestRoom::Default));
        })
        .await;

    // The room is direct now.
    let direct_targets = room.direct_targets();
    assert_eq!(direct_targets.len(), 1);
    assert!(direct_targets.contains(User::Bob.as_str()));
    assert!(room.is_direct().await.unwrap());

    // Unset the room as direct.
    server
        .mock_set_global_account_data(own_user_id, &GlobalAccountDataEventType::Direct)
        .expect_account_data(f.direct().into_content())
        .ok()
        .expect(1)
        .named("unset_direct")
        .mount()
        .await;

    room.set_is_direct(false).await.unwrap();

    // Mock the sync response we should get from the homeserver.
    server
        .mock_sync()
        .ok_and_run(&client, |builder| {
            builder.add_global_account_data(f.direct());
        })
        .await;

    // The room is not direct anymore.
    assert!(room.direct_targets().is_empty());
    assert!(!room.is_direct().await.unwrap());
}

#[async_test]
async fn test_room_avatar() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let own_user_id = client.user_id().unwrap();
    let f = EventFactory::new().default_room().sender(own_user_id);

    // Room without avatar.
    let room = server.sync_default_joined_room(&client).await;

    assert_eq!(room.avatar_url(), None);
    assert_matches!(room.avatar_info(), None);

    // Set the avatar, but not the info.
    let avatar_url_1 = mxc_uri!("mxc://server.local/abcdef");

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default().add_timeline_event(f.room_avatar().url(avatar_url_1)),
        )
        .await;

    assert_eq!(room.avatar_url().as_deref(), Some(avatar_url_1));
    assert_matches!(room.avatar_info(), None);

    // Set the avatar and the info.
    let avatar_url_2 = mxc_uri!("mxc://server.local/ghijkl");
    let mut avatar_info_2 = avatar::ImageInfo::new();
    avatar_info_2.height = Some(uint!(200));
    avatar_info_2.width = Some(uint!(200));
    avatar_info_2.mimetype = Some("image/png".to_owned());
    avatar_info_2.size = Some(uint!(5243));

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::default()
                .add_timeline_event(f.room_avatar().url(avatar_url_2).info(avatar_info_2)),
        )
        .await;

    assert_eq!(room.avatar_url().as_deref(), Some(avatar_url_2));
    let avatar_info = room.avatar_info().unwrap();
    assert_eq!(avatar_info.height, Some(uint!(200)));
    assert_eq!(avatar_info.width, Some(uint!(200)));
    assert_eq!(avatar_info.mimetype.as_deref(), Some("image/png"));
    assert_eq!(avatar_info.size, Some(uint!(5243)));
}

#[async_test]
async fn test_is_dm_using_matrix_spec() {
    let server = MatrixMockServer::new().await;
    let client = server
        .client_builder()
        .on_builder(|b| b.dm_room_definition(DmRoomDefinition::MatrixSpec))
        .build()
        .await;
    let alice_user_id = User::Alice;
    let room_id = TestRoom::Default;

    // We mock the m.direct account data with a single target for the room id
    let direct_data = EventFactory::new()
        .direct()
        .add_user(alice_user_id, room_id)
        .into_raw::<AnyGlobalAccountDataEvent>();
    server
        .mock_sync()
        .ok_and_run(&client, |response| {
            response.add_global_account_data(direct_data);
        })
        .await;

    let room = server.sync_default_joined_room(&client).await;
    // Room has direct targets, so it's considered direct and a DM using the
    // spec definition.
    assert!(room.compute_is_dm().await.unwrap());

    // We mock the m.direct account data with no targets
    let direct_data = EventFactory::new().direct().into_raw::<AnyGlobalAccountDataEvent>();
    server
        .mock_sync()
        .ok_and_run(&client, |response| {
            response.add_global_account_data(direct_data);
        })
        .await;

    // Room doesn't have direct targets anymore, so it's not a DM using the spec
    // definition.
    assert!(room.compute_is_dm().await.unwrap().not());
}

#[async_test]
async fn test_is_dm_using_at_most_two_members_definition() {
    let server = MatrixMockServer::new().await;
    let client = server
        .client_builder()
        .on_builder(|b| b.dm_room_definition(DmRoomDefinition::TwoMembers))
        .build()
        .await;
    let alice_user_id = User::Alice;
    let room_id = TestRoom::Default;

    // We mock the m.direct account data with a single target for the room id
    let direct_data = EventFactory::new()
        .direct()
        .add_user(alice_user_id, room_id)
        .into_raw::<AnyGlobalAccountDataEvent>();
    server
        .mock_sync()
        .ok_and_run(&client, |response| {
            response.add_global_account_data(direct_data);
        })
        .await;

    // The room has direct targets, and 2 active users, so it's a DM.
    let room =
        server.sync_room(&client, JoinedRoomBuilder::default().set_joined_members_count(2)).await;
    assert!(room.compute_is_dm().await.unwrap());

    // The room has direct targets, and a single active user, so it's a DM.
    let room =
        server.sync_room(&client, JoinedRoomBuilder::default().set_joined_members_count(1)).await;
    assert!(room.compute_is_dm().await.unwrap());

    // The room has direct targets, and > 2 active users, so it's NOT a DM.
    let room =
        server.sync_room(&client, JoinedRoomBuilder::default().set_joined_members_count(3)).await;
    assert!(!room.compute_is_dm().await.unwrap());
}
