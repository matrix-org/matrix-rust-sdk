use std::time::Duration;

use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::StreamExt as _;
use matrix_sdk::{
    Client, Room,
    config::SyncSettings,
    test_utils::{
        logged_in_client_with_server,
        mocks::{MatrixMockServer, RoomMessagesResponseTemplate, RoomRelationsResponseTemplate},
    },
};
use matrix_sdk_base::deserialized_responses::TimelineEvent;
use matrix_sdk_common::executor::spawn;
use matrix_sdk_test::{
    BOB, JoinedRoomBuilder, StateTestEvent, SyncResponseBuilder, async_test,
    event_factory::EventFactory,
};
use matrix_sdk_ui::timeline::{RoomExt, TimelineBuilder, TimelineFocus};
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, RoomId, UserId, assign,
    event_id,
    events::{
        AnySyncTimelineEvent, AnyTimelineEvent,
        room::{
            encrypted::{
                EncryptedEventScheme, MegolmV1AesSha2ContentInit, RoomEncryptedEventContent,
            },
            message::RoomMessageEventContentWithoutRelation,
            pinned_events::RoomPinnedEventsEventContent,
        },
    },
    owned_device_id, owned_room_id, owned_user_id, room_id,
    serde::Raw,
    user_id,
};
use serde_json::json;
use stream_assert::assert_pending;
use tokio::time::sleep;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{header, method, path_regex},
};

use crate::mock_sync;

#[async_test]
async fn test_new_pinned_events_are_not_added_on_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!test:localhost");

    let f = EventFactory::new().room(room_id).sender(*BOB);
    let event_id = event_id!("$1");
    let event_1 = f
        .text_msg("in the end")
        .event_id(event_id)
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_raw_sync();

    // Mock /event endpoint for event $1.
    mock_events_endpoint(&server, room_id, vec![event_1]).await;

    // Mock /relations for event $1.
    server
        .mock_room_relations()
        .match_target_event(event_id.to_owned())
        .match_limit(256)
        .ok(RoomRelationsResponseTemplate::default().events(Vec::<Raw<AnyTimelineEvent>>::new()))
        .mock_once()
        .mount()
        .await;

    // Load initial timeline items: a `m.room.pinned_events` with events $1 and $2
    // pinned
    let room = PinnedEventsSync::new(room_id)
        .with_pinned_event_ids(vec!["$1", "$2"])
        .mock_and_sync(&client, &server)
        .await
        .expect("Room should be synced");
    let timeline =
        TimelineBuilder::new(&room).with_focus(pinned_events_focus(100)).build().await.unwrap();

    assert!(
        timeline.live_back_pagination_status().await.is_none(),
        "there should be no live back-pagination status for a pinned events timeline"
    );

    // Load timeline items
    let (items, mut timeline_stream) = timeline.subscribe().await;

    assert_eq!(items.len(), 1 + 1); // event item + a date divider
    assert!(items[0].is_date_divider());
    assert_eq!(items[1].as_event().unwrap().content().as_message().unwrap().body(), "in the end");
    assert_pending!(timeline_stream);

    // Load new pinned event contents from sync, $2 was pinned but wasn't available
    // before
    let event_2 = f
        .text_msg("pinned message!")
        .event_id(event_id!("$2"))
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_raw_sync();

    let _ = PinnedEventsSync::new(room_id)
        .with_timeline_events(vec![event_2])
        .mock_and_sync(&client, &server)
        .await
        .expect("Room should be synced");

    // Event $2 was received through sync, but it wasn't added to the pinned event
    // timeline.
    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_pinned_event_with_reaction() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!test:localhost");

    let f = EventFactory::new().room(room_id).sender(*BOB);
    let event_id = event_id!("$1");
    let event_1 = f
        .text_msg("in the end")
        .event_id(event_id)
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_raw_sync();

    let reaction = f.reaction(event_id, "👀").into_raw_timeline();

    // Mock /event endpoint for event $1.
    mock_events_endpoint(&server, room_id, vec![event_1]).await;

    // Mock /relations for event $1.
    server
        .mock_room_relations()
        .match_target_event(event_id.to_owned())
        .match_limit(256)
        .ok(RoomRelationsResponseTemplate::default().events(vec![reaction]))
        .mock_once()
        .mount()
        .await;

    // Load initial timeline items: a `m.room.pinned_events` with event $1 pinned
    let room = PinnedEventsSync::new(room_id)
        .with_pinned_event_ids(vec!["$1"])
        .mock_and_sync(&client, &server)
        .await
        .expect("Room should be synced");
    let timeline =
        TimelineBuilder::new(&room).with_focus(pinned_events_focus(100)).build().await.unwrap();

    assert!(
        timeline.live_back_pagination_status().await.is_none(),
        "there should be no live back-pagination status for a pinned events timeline"
    );

    // Load timeline items
    let (items, mut timeline_stream) = timeline.subscribe().await;

    // Verify that the timeline contains the pinned event and its reaction.
    assert_eq!(items.len(), 1 + 1); // event item + a date divider
    assert!(items[0].is_date_divider());
    assert_eq!(items[1].as_event().unwrap().content().as_message().unwrap().body(), "in the end");
    let reactions = items[1]
        .as_event()
        .unwrap()
        .content()
        .reactions()
        .expect("pinned event should have reactions");
    assert_eq!(reactions.len(), 1);
    assert!(reactions.get("👀").is_some());
    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_pinned_event_with_paginated_reactions() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!test:localhost");

    let f = EventFactory::new().room(room_id).sender(*BOB);
    let event_id = event_id!("$1");
    let event_1 = f
        .text_msg("in the end")
        .event_id(event_id)
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_raw_sync();

    let reaction_1 = f.reaction(event_id, "👀").into_raw_timeline();
    let reaction_2 = f.reaction(event_id, "🤔").into_raw_timeline();

    // Mock /event endpoint for event $1.
    mock_events_endpoint(&server, room_id, vec![event_1]).await;

    // Mock /relations for event $1, split across two pages.
    let token = "foo";
    server
        .mock_room_relations()
        .match_target_event(event_id.to_owned())
        .match_limit(256)
        .ok(RoomRelationsResponseTemplate::default().events(vec![reaction_1]).next_batch(token))
        .mock_once()
        .mount()
        .await;
    server
        .mock_room_relations()
        .match_target_event(event_id.to_owned())
        .match_limit(256)
        .match_from(token)
        .ok(RoomRelationsResponseTemplate::default().events(vec![reaction_2]))
        .mock_once()
        .mount()
        .await;

    // Load initial timeline items: a `m.room.pinned_events` with event $1 pinned
    let room = PinnedEventsSync::new(room_id)
        .with_pinned_event_ids(vec!["$1"])
        .mock_and_sync(&client, &server)
        .await
        .expect("Room should be synced");
    let timeline =
        TimelineBuilder::new(&room).with_focus(pinned_events_focus(100)).build().await.unwrap();

    assert!(
        timeline.live_back_pagination_status().await.is_none(),
        "there should be no live back-pagination status for a pinned events timeline"
    );

    // Load timeline items
    let (items, mut timeline_stream) = timeline.subscribe().await;

    // Verify that the timeline contains the pinned event and its reactions.
    assert_eq!(items.len(), 1 + 1); // event item + a date divider
    assert!(items[0].is_date_divider());
    assert_eq!(items[1].as_event().unwrap().content().as_message().unwrap().body(), "in the end");
    let reactions = items[1]
        .as_event()
        .unwrap()
        .content()
        .reactions()
        .expect("pinned event should have reactions");
    assert_eq!(reactions.len(), 2);
    assert!(reactions.get("👀").is_some());
    assert!(reactions.get("🤔").is_some());
    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_new_pinned_event_ids_reload_the_timeline() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!test:localhost");

    let f = EventFactory::new().room(room_id).sender(*BOB);
    let event_id_1 = event_id!("$1");
    let event_1 = f
        .text_msg("in the end")
        .event_id(event_id_1)
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_raw_sync();
    let event_id_2 = event_id!("$2");
    let event_2 = f
        .text_msg("it doesn't even matter")
        .event_id(event_id_2)
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_raw_sync();

    // Mock /event endpoint for events $1 and $2.
    mock_events_endpoint(&server, room_id, vec![event_1.clone(), event_2.clone()]).await;

    // Mock /relations endpoint for events $1 and $2.
    server
        .mock_room_relations()
        .match_target_event(event_id_1.to_owned())
        .match_limit(256)
        .ok(RoomRelationsResponseTemplate::default().events(Vec::<Raw<AnyTimelineEvent>>::new()))
        .mock_once()
        .mount()
        .await;
    server
        .mock_room_relations()
        .match_target_event(event_id_2.to_owned())
        .match_limit(256)
        .ok(RoomRelationsResponseTemplate::default().events(Vec::<Raw<AnyTimelineEvent>>::new()))
        .mock_once()
        .mount()
        .await;

    // Load initial timeline items: 2 text messages and a `m.room.pinned_events`
    // with event $1 and $2 pinned
    let room = PinnedEventsSync::new(room_id)
        .with_timeline_events(vec![event_2.clone()])
        .with_pinned_event_ids(vec!["$1"])
        .mock_and_sync(&client, &server)
        .await
        .expect("Room should be synced");

    let timeline =
        TimelineBuilder::new(&room).with_focus(pinned_events_focus(100)).build().await.unwrap();

    assert!(
        timeline.live_back_pagination_status().await.is_none(),
        "there should be no live back-pagination status for a focused timeline"
    );

    let (items, mut timeline_stream) = timeline.subscribe().await;

    assert_eq!(items.len(), 1 + 1); // event item + a date divider
    assert!(items[0].is_date_divider());
    assert_eq!(items[1].as_event().unwrap().content().as_message().unwrap().body(), "in the end");
    assert_pending!(timeline_stream);

    // Reload timeline with new pinned event ids
    mock_events_endpoint(&server, room_id, vec![event_1.clone(), event_2.clone()]).await;
    let _ = PinnedEventsSync::new(room_id)
        .with_pinned_event_ids(vec!["$1", "$2"])
        .mock_and_sync(&client, &server)
        .await
        .expect("Sync failed");

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 4);

    assert_let!(VectorDiff::Clear = &timeline_updates[0]);

    assert_let!(VectorDiff::PushBack { value } = &timeline_updates[1]);
    assert_eq!(value.as_event().unwrap().event_id().unwrap(), event_id!("$1"));

    assert_let!(VectorDiff::PushBack { value } = &timeline_updates[2]);
    assert_eq!(value.as_event().unwrap().event_id().unwrap(), event_id!("$2"));

    assert_let!(VectorDiff::PushFront { value } = &timeline_updates[3]);
    assert!(value.is_date_divider());

    assert_pending!(timeline_stream);

    // Reload timeline with no pinned event
    mock_events_endpoint(&server, room_id, vec![event_1.clone(), event_2.clone()]).await;
    let _ = PinnedEventsSync::new(room_id)
        .with_pinned_event_ids(Vec::new())
        .mock_and_sync(&client, &server)
        .await
        .expect("Sync failed");

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 1);
    assert_let!(VectorDiff::Clear = &timeline_updates[0]);

    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_max_events_to_load_is_honored() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!test:localhost");

    let f = EventFactory::new().room(room_id).sender(*BOB);
    let pinned_event = f
        .text_msg("in the end")
        .event_id(event_id!("$1"))
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_raw_sync();

    // Load initial timeline items: a text message and a `m.room.pinned_events`
    // with event $1 and $2 pinned
    mock_events_endpoint(&server, room_id, vec![pinned_event]).await;
    let room = PinnedEventsSync::new(room_id)
        .with_pinned_event_ids(vec!["$1", "$2"])
        .mock_and_sync(&client, &server)
        .await
        .expect("Sync failed");

    let ret = TimelineBuilder::new(&room).with_focus(pinned_events_focus(1)).build().await;

    // We're only taking the last event id, `$2`, and it's not available so the
    // timeline fails to initialise.
    assert!(ret.is_err());
}

#[async_test]
async fn test_cached_events_are_kept_for_different_room_instances() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!test:localhost");

    // Subscribe to the event cache.
    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let f = EventFactory::new().room(room_id).sender(*BOB);
    let event_id = event_id!("$1");
    let pinned_event = f
        .text_msg("in the end")
        .event_id(event_id)
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_raw_sync();

    // Mock /event for the pinned event.
    mock_events_endpoint(&server, room_id, vec![pinned_event]).await;

    // Mock /relations for the pinned event.
    server
        .mock_room_relations()
        .match_target_event(event_id.to_owned())
        .match_limit(256)
        .ok(RoomRelationsResponseTemplate::default().events(Vec::<Raw<AnyTimelineEvent>>::new()))
        .mock_once()
        .mount()
        .await;

    // Load initial timeline items: a `m.room.pinned_events` with event $1 and $2
    // pinned
    let room = PinnedEventsSync::new(room_id)
        .with_pinned_event_ids(vec!["$1", "$2"])
        .mock_and_sync(&client, &server)
        .await
        .expect("Sync failed");

    let (room_cache, _drop_handles) = room.event_cache().await.unwrap();
    let timeline =
        TimelineBuilder::new(&room).with_focus(pinned_events_focus(2)).build().await.unwrap();

    assert!(
        timeline.live_back_pagination_status().await.is_none(),
        "there should be no live back-pagination status for a focused timeline"
    );

    let (items, mut timeline_stream) = timeline.subscribe().await;

    assert!(!items.is_empty()); // We just loaded some events
    assert_pending!(timeline_stream);

    assert!(room_cache.find_event(event_id!("$1")).await.unwrap().is_some());

    // Drop the existing room and timeline instances
    drop(timeline_stream);
    drop(timeline);
    drop(room);

    // Set up a sync response with only the pinned event ids and no events, so if
    // they exist later we know they come from the cache
    let room = PinnedEventsSync::new(room_id)
        .with_pinned_event_ids(vec!["$1", "$2"])
        .mock_and_sync(&client, &server)
        .await
        .expect("Sync failed");

    // And a new timeline one
    let timeline =
        TimelineBuilder::new(&room).with_focus(pinned_events_focus(2)).build().await.unwrap();

    let (items, _) = timeline.subscribe().await;
    assert!(!items.is_empty()); // These events came from the cache
    assert!(room_cache.find_event(event_id!("$1")).await.unwrap().is_some());

    // Drop the existing room and timeline instances
    server.server().reset().await;
    drop(timeline);
    drop(room);
}

#[async_test]
async fn test_pinned_timeline_with_pinned_event_ids_and_empty_result_fails() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!test:localhost");

    // Load initial timeline items: a `m.room.pinned_events` with event $1 and $2
    // pinned, but they're not available neither in the cache nor in the HS
    let room = PinnedEventsSync::new(room_id)
        .with_pinned_event_ids(vec!["$1", "$2"])
        .mock_and_sync(&client, &server)
        .await
        .expect("Sync failed");
    let ret = TimelineBuilder::new(&room).with_focus(pinned_events_focus(1)).build().await;

    // The timeline couldn't load any events so it fails to initialise
    assert!(ret.is_err());
}

#[async_test]
async fn test_pinned_timeline_with_no_pinned_event_ids_is_just_empty() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!test:localhost");

    // Load initial timeline items: an empty `m.room.pinned_events` event
    let room = PinnedEventsSync::new(room_id)
        .with_pinned_event_ids(Vec::new())
        .mock_and_sync(&client, &server)
        .await
        .expect("Sync failed");
    let timeline =
        TimelineBuilder::new(&room).with_focus(pinned_events_focus(1)).build().await.unwrap();

    // The timeline couldn't load any events, but it expected none, so it just
    // returns an empty list
    let (items, _) = timeline.subscribe().await;
    assert!(items.is_empty());
}

#[async_test]
async fn test_pinned_timeline_with_no_pinned_events_and_an_utd_on_sync_is_just_empty() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!test:localhost");
    let event_id = event_id!("$1:morpheus.localhost");
    let sender_id = owned_user_id!("@example:localhost");

    // Load initial timeline items: an empty `m.room.pinned_events` event
    let room = PinnedEventsSync::new(room_id)
        .with_pinned_event_ids(Vec::new())
        .mock_and_sync(&client, &server)
        .await
        .expect("Sync failed");

    // Mock encrypted event for which we don't have keys (an UTD)
    let utd_event = create_utd(room_id, &sender_id, event_id);
    mock_events_endpoint(&server, room_id, vec![utd_event]).await;

    let timeline =
        TimelineBuilder::new(&room).with_focus(pinned_events_focus(1)).build().await.unwrap();

    // The timeline couldn't load any events, but it expected none, so it just
    // returns an empty list
    let (items, _) = timeline.subscribe().await;
    assert!(items.is_empty());
}

#[async_test]
async fn test_pinned_timeline_with_no_pinned_events_on_pagination_is_just_empty() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!test:localhost");
    let event_id = event_id!("$1.localhost");
    let sender_id = owned_user_id!("@example:localhost");

    // Load initial timeline items: an empty `m.room.pinned_events` event
    let room = PinnedEventsSync::new(room_id)
        .with_pinned_event_ids(Vec::new())
        .mock_and_sync(&client, &server)
        .await
        .expect("Sync failed");
    let pinned_timeline =
        TimelineBuilder::new(&room).with_focus(pinned_events_focus(1)).build().await.unwrap();

    // The timeline couldn't load any events, but it expected none, so it just
    // returns an empty list
    let (pinned_items, mut pinned_events_stream) = pinned_timeline.subscribe().await;
    assert!(pinned_items.is_empty());

    // Create a non-pinned event to return in the pagination
    let not_pinned_event = EventFactory::new()
        .room(room_id)
        .sender(&sender_id)
        .text_msg("Hey")
        .event_id(event_id)
        .into_raw_timeline();

    server
        .mock_room_messages()
        .ok(assign!(RoomMessagesResponseTemplate::default(), {
            chunk: vec![not_pinned_event]
        }))
        .mock_once()
        .mount()
        .await;

    let (event_cache, _) = room.event_cache().await.expect("Event cache should be accessible");

    // Paginate backwards once using the event cache to load the event
    event_cache
        .pagination()
        .run_backwards_once(10)
        .await
        .expect("Pagination of events should successful");

    // Assert the event is loaded and added to the cache
    assert!(event_cache.find_event(event_id).await.unwrap().is_some());

    // And it won't cause an update in the pinned events timeline since it's not
    // pinned
    assert_pending!(pinned_events_stream);
}

#[async_test]
async fn test_pinned_timeline_with_pinned_utd_on_sync_contains_it() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!test:localhost");
    let event_id = event_id!("$1:morpheus.localhost");
    let sender_id = owned_user_id!("@example:localhost");

    // Sync the joined room with a pinned event id
    let room = PinnedEventsSync::new(room_id)
        .with_pinned_event_ids(vec![event_id.as_str()])
        .mock_and_sync(&client, &server)
        .await
        .expect("Sync failed");

    // Mock encrypted pinned event for which we don't have keys (an UTD)
    let utd_event = create_utd(room_id, &sender_id, event_id);
    mock_events_endpoint(&server, room_id, vec![utd_event]).await;
    server
        .mock_room_relations()
        .match_target_event(event_id.to_owned())
        .match_limit(256)
        .ok(RoomRelationsResponseTemplate::default().events(Vec::<Raw<AnyTimelineEvent>>::new()))
        .mock_once()
        .mount()
        .await;

    let timeline =
        TimelineBuilder::new(&room).with_focus(pinned_events_focus(1)).build().await.unwrap();

    // The timeline loaded with just a day divider and the pinned UTD
    let (items, _) = timeline.subscribe().await;
    assert_eq!(items.len(), 2);
    let pinned_utd_event = items.last().unwrap().as_event().unwrap();
    assert_eq!(pinned_utd_event.event_id().unwrap(), event_id);
}

#[async_test]
async fn test_edited_events_are_reflected_in_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!test:localhost");

    let f = EventFactory::new().room(room_id).sender(*BOB);
    let event_id = event_id!("$1");
    let pinned_event = f
        .text_msg("in the end")
        .event_id(event_id)
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_raw_sync();

    // Mock /event for the pinned event.
    mock_events_endpoint(&server, room_id, vec![pinned_event]).await;

    // Mock /relations for the pinned event.
    server
        .mock_room_relations()
        .match_target_event(event_id.to_owned())
        .match_limit(256)
        .ok(RoomRelationsResponseTemplate::default().events(Vec::<Raw<AnyTimelineEvent>>::new()))
        .mock_once()
        .mount()
        .await;

    // Load initial timeline items: a text message and a `m.room.pinned_events` with
    // event $1.
    let room = PinnedEventsSync::new(room_id)
        .with_pinned_event_ids(vec!["$1"])
        .mock_and_sync(&client, &server)
        .await
        .expect("Sync failed");
    let timeline =
        TimelineBuilder::new(&room).with_focus(pinned_events_focus(100)).build().await.unwrap();

    assert!(
        timeline.live_back_pagination_status().await.is_none(),
        "there should be no live back-pagination status for a focused timeline"
    );

    // Load timeline items.
    let (items, mut timeline_stream) = timeline.subscribe().await;

    assert_eq!(items.len(), 1 + 1); // event item + a date divider
    assert!(items[0].is_date_divider());
    assert_eq!(items[1].as_event().unwrap().content().as_message().unwrap().body(), "in the end");
    assert_pending!(timeline_stream);

    let edited_event = f
        .text_msg("* edited message!")
        .edit(
            event_id!("$1"),
            RoomMessageEventContentWithoutRelation::text_plain("edited message!"),
        )
        .event_id(event_id!("$2"))
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_raw_sync();

    // Mock /event for some timeline events.
    mock_events_endpoint(&server, room_id, vec![edited_event.clone()]).await;

    // Load new pinned event contents from sync, where $2 is and edit on $1.
    let _ = PinnedEventsSync::new(room_id)
        .with_timeline_events(vec![edited_event])
        .mock_and_sync(&client, &server)
        .await
        .expect("Sync failed");

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 1);

    // The edit does replace the original event.
    assert_let!(VectorDiff::Set { index: 1, value } = &timeline_updates[0]);
    let event = value.as_event().unwrap();
    assert_eq!(event.event_id().unwrap(), event_id!("$1"));
    assert_eq!(event.content().as_message().unwrap().body(), "edited message!");

    // That's all, folks!
    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_redacted_events_are_reflected_in_sync() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    let room_id = room_id!("!test:localhost");

    let f = EventFactory::new().room(room_id).sender(*BOB);
    let event_id = event_id!("$1");
    let pinned_event = f
        .text_msg("in the end")
        .event_id(event_id!("$1"))
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_raw_sync();

    // Mock /event for the pinned timeline event.
    mock_events_endpoint(&server, room_id, vec![pinned_event]).await;

    // Mock /relations for pinned timeline event.
    server
        .mock_room_relations()
        .match_target_event(event_id.to_owned())
        .match_limit(256)
        .ok(RoomRelationsResponseTemplate::default().events(Vec::<Raw<AnyTimelineEvent>>::new()))
        .mock_once()
        .mount()
        .await;

    // Load initial timeline items: a text message and a `m.room.pinned_events` with
    // event $1
    let room = PinnedEventsSync::new(room_id)
        .with_pinned_event_ids(vec!["$1"])
        .mock_and_sync(&client, &server)
        .await
        .expect("Sync failed");
    let timeline =
        TimelineBuilder::new(&room).with_focus(pinned_events_focus(100)).build().await.unwrap();

    assert!(
        timeline.live_back_pagination_status().await.is_none(),
        "there should be no live back-pagination status for a focused timeline"
    );

    // Load timeline items
    let (items, mut timeline_stream) = timeline.subscribe().await;

    assert_eq!(items.len(), 1 + 1); // event item + a date divider
    assert!(items[0].is_date_divider());
    assert_eq!(items[1].as_event().unwrap().content().as_message().unwrap().body(), "in the end");
    assert_pending!(timeline_stream);

    let redaction_event = f
        .redaction(event_id!("$1"))
        .event_id(event_id!("$2"))
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_raw_sync();

    // Mock /event for some timeline events
    mock_events_endpoint(&server, room_id, vec![redaction_event.clone()]).await;

    // Load new pinned event contents from sync, where $1 is now redacted
    let _ = PinnedEventsSync::new(room_id)
        .with_timeline_events(vec![redaction_event])
        .mock_and_sync(&client, &server)
        .await
        .expect("Sync failed");

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 1);

    // The redaction takes place.
    assert_let!(VectorDiff::Set { index: 1, value } = &timeline_updates[0]);
    let event = value.as_event().unwrap();
    assert!(event.content().is_redacted());

    // That's all, folks!
    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_ensure_max_concurrency_is_observed() {
    let (client, server) = logged_in_client_with_server().await;
    let room_id = owned_room_id!("!a_room:example.org");

    let pinned_event_ids: Vec<String> = (0..100).map(|idx| format!("${idx}")).collect();

    let max_concurrent_requests: u16 = 10;

    let joined_room_builder = JoinedRoomBuilder::new(&room_id)
        // Set up encryption
        .add_state_event(StateTestEvent::Encryption)
        // Add 100 pinned events
        .add_state_event(StateTestEvent::Custom(json!(
            {
                "content": {
                    "pinned": pinned_event_ids
                },
                "event_id": "$15139375513VdeRF:localhost",
                "origin_server_ts": 151393755,
                "sender": "@example:localhost",
                "state_key": "",
                "type": "m.room.pinned_events",
                "unsigned": {
                    "age": 703422
                }
            }
        )));

    let pinned_event =
        EventFactory::new().room(&room_id).sender(*BOB).text_msg("A message").into_raw_timeline();
    Mock::given(method("GET"))
        .and(path_regex(r"/_matrix/client/r0/rooms/.*/event/.*"))
        .and(header("authorization", "Bearer 1234"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(60))
                .set_body_json(pinned_event.json()),
        )
        // Verify this endpoint is only called the max concurrent amount of times.
        .expect(max_concurrent_requests as u64)
        .mount(&server)
        .await;

    let mut sync_response_builder = SyncResponseBuilder::new();
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));
    let json_response =
        sync_response_builder.add_joined_room(joined_room_builder).build_json_sync_response();
    mock_sync(&server, json_response, None).await;
    let _ = client.sync_once(sync_settings.clone()).await;

    let room = client.get_room(&room_id).unwrap();

    // Start loading the pinned event timeline asynchronously.
    let handle = spawn({
        let timeline_builder = room.timeline_builder().with_focus(pinned_events_focus(100));
        async {
            let _ = timeline_builder.build().await;
        }
    });

    // Give the timeline enough time to spawn the maximum number of concurrent
    // requests.
    sleep(Duration::from_secs(2)).await;

    // Abort handle to stop requests from being processed.
    handle.abort();

    // The real check happens here, based on the `max_concurrent_requests` expected
    // value set above for the mock endpoint.
    server.verify().await;
}

async fn mock_events_endpoint(
    server: &MatrixMockServer,
    room_id: &RoomId,
    events: Vec<Raw<AnySyncTimelineEvent>>,
) {
    for event in events {
        server
            .mock_room_event()
            .room(room_id.to_owned())
            .match_event_id()
            .ok(TimelineEvent::from_plaintext(event))
            .mount()
            .await;
    }
}

/// Allows to mock and sync a room adding optional timeline events and pinned
/// event ids
#[derive(Debug, Clone)]
struct PinnedEventsSync {
    room_id: OwnedRoomId,
    timeline_events: Vec<Raw<AnySyncTimelineEvent>>,
    pinned_event_ids: Option<Vec<OwnedEventId>>,
}

impl PinnedEventsSync {
    fn new(room_id: &RoomId) -> Self {
        Self { room_id: room_id.to_owned(), timeline_events: Vec::new(), pinned_event_ids: None }
    }

    fn with_timeline_events(mut self, items: Vec<Raw<AnySyncTimelineEvent>>) -> Self {
        self.timeline_events = items;
        self
    }

    fn with_pinned_event_ids(mut self, pinned_event_ids: Vec<&str>) -> Self {
        let pinned_event_ids: Vec<OwnedEventId> = pinned_event_ids
            .into_iter()
            .map(|id| match EventId::parse(id) {
                Ok(id) => id,
                Err(_) => panic!("Invalid event id: {id}"),
            })
            .collect();

        self.pinned_event_ids = Some(pinned_event_ids);
        self
    }

    async fn mock_and_sync(
        self,
        client: &Client,
        server: &MatrixMockServer,
    ) -> Result<Room, matrix_sdk::Error> {
        let mut joined_room_builder = JoinedRoomBuilder::new(&self.room_id)
            // Set up encryption
            .add_state_event(StateTestEvent::Encryption);

        joined_room_builder = joined_room_builder.add_timeline_bulk(self.timeline_events);

        if let Some(pinned_event_ids) = self.pinned_event_ids {
            let pinned_events_event = EventFactory::new()
                .room(&self.room_id)
                .event(RoomPinnedEventsEventContent::new(pinned_event_ids))
                .sender(user_id!("@example:localhost"))
                .state_key("")
                .into();

            joined_room_builder = joined_room_builder.add_state_bulk(vec![pinned_events_event]);
        }

        Ok(server.sync_room(client, joined_room_builder).await)
    }
}

fn create_utd(
    room_id: &RoomId,
    sender_id: &UserId,
    event_id: &EventId,
) -> Raw<AnySyncTimelineEvent> {
    EventFactory::new()
        .room(room_id)
        .sender(sender_id)
        .event(RoomEncryptedEventContent::new(
            EncryptedEventScheme::MegolmV1AesSha2(
                MegolmV1AesSha2ContentInit {
                    ciphertext: String::from(
                        "AwgAEpABhetEzzZzyYrxtEVUtlJnZtJcURBlQUQJ9irVeklCTs06LwgTMQj61PMUS4Vy\
                           YOX+PD67+hhU40/8olOww+Ud0m2afjMjC3wFX+4fFfSkoWPVHEmRVucfcdSF1RSB4EmK\
                           PIP4eo1X6x8kCIMewBvxl2sI9j4VNvDvAN7M3zkLJfFLOFHbBviI4FN7hSFHFeM739Zg\
                           iwxEs3hIkUXEiAfrobzaMEM/zY7SDrTdyffZndgJo7CZOVhoV6vuaOhmAy4X2t4UnbuV\
                           JGJjKfV57NAhp8W+9oT7ugwO",
                    ),
                    device_id: owned_device_id!("KIUVQQSDTM"),
                    sender_key: String::from("LvryVyoCjdONdBCi2vvoSbI34yTOx7YrCFACUEKoXnc"),
                    session_id: String::from("64H7XKokIx0ASkYDHZKlT5zd/Zccz/cQspPNdvnNULA"),
                }
                .into(),
            ),
            None,
        ))
        .event_id(event_id)
        .into_raw_sync()
}

fn pinned_events_focus(max_events_to_load: u16) -> TimelineFocus {
    TimelineFocus::PinnedEvents { max_events_to_load, max_concurrent_requests: 10 }
}
