use std::{ops::ControlFlow, time::Duration};

use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use matrix_sdk::{
    assert_next_matches_with_timeout,
    config::SyncSettings,
    event_cache::{BackPaginationOutcome, TimelineHasBeenResetWhilePaginating},
    test_utils::{
        logged_in_client_with_server,
        mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
    },
    Client,
};
use matrix_sdk_base::deserialized_responses::TimelineEvent;
use matrix_sdk_test::{
    async_test, event_factory::EventFactory, JoinedRoomBuilder, StateTestEvent,
    SyncResponseBuilder, BOB,
};
use matrix_sdk_ui::{
    timeline::{RoomExt, TimelineFocus, TimelineItemContent},
    Timeline,
};
use ruma::{
    assign, event_id,
    events::{
        room::{
            encrypted::{
                EncryptedEventScheme, MegolmV1AesSha2ContentInit, RoomEncryptedEventContent,
            },
            message::RoomMessageEventContentWithoutRelation,
        },
        AnyTimelineEvent,
    },
    owned_device_id, owned_room_id, owned_user_id,
    serde::Raw,
    EventId, MilliSecondsSinceUnixEpoch, OwnedRoomId, RoomId, UserId,
};
use serde_json::json;
use stream_assert::assert_pending;
use tokio::time::sleep;
use wiremock::{
    matchers::{header, method, path_regex},
    Mock, ResponseTemplate,
};

use crate::mock_sync;

#[async_test]
async fn test_new_pinned_events_are_added_on_sync() {
    let mut test_helper = TestHelper::new().await;
    let room_id = test_helper.room_id.clone();

    // Join the room
    let _ = test_helper.do_initial_sync().await;

    let f = EventFactory::new().room(&room_id).sender(*BOB);
    let event_1 = f
        .text_msg("in the end")
        .event_id(event_id!("$1"))
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_timeline();

    // Mock /event endpoint for a timeline event
    test_helper.mock_events(vec![event_1.clone()]).await;

    // Load initial timeline items: a `m.room.pinned_events` with events $1 and $2
    // pinned
    test_helper.sync_once(Vec::new(), Some(vec!["$1", "$2"])).await.expect("Sync failed");

    let room = test_helper.client.get_room(&room_id).unwrap();
    let timeline =
        Timeline::builder(&room).with_focus(pinned_events_focus(100)).build().await.unwrap();

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

    // Load new pinned event contents from sync, $2 was pinned but wasn't available
    // before
    let event_2 = f
        .text_msg("pinned message!")
        .event_id(event_id!("$2"))
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_timeline();
    let event_3 = f
        .text_msg("normal message")
        .event_id(event_id!("$3"))
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_timeline();
    test_helper.mock_events(vec![event_2.clone(), event_3.clone()]).await;
    test_helper.sync_once(vec![event_2, event_3], None).await.expect("Sync failed");

    // The item is added automatically
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.as_event().unwrap().event_id().unwrap(), event_id!("$2"));
    });
    // The list is reloaded, so it's reset
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::Clear);
    // Then the loaded list items are added
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.as_event().unwrap().event_id().unwrap(), event_id!("$1"));
    });
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.as_event().unwrap().event_id().unwrap(), event_id!("$2"));
    });
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::PushFront { value } => {
        assert!(value.is_date_divider());
    });
}

#[async_test]
async fn test_new_pinned_event_ids_reload_the_timeline() {
    let mut test_helper = TestHelper::new().await;
    let room_id = test_helper.room_id.clone();

    // Join the room
    test_helper.do_initial_sync().await;

    let f = EventFactory::new().room(&room_id).sender(*BOB);
    let event_1 = f
        .text_msg("in the end")
        .event_id(event_id!("$1"))
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_timeline();
    let event_2 = f
        .text_msg("it doesn't even matter")
        .event_id(event_id!("$2"))
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_timeline();

    // Load initial timeline items: 2 text messages and a `m.room.pinned_events`
    // with event $1 and $2 pinned
    test_helper.mock_events(vec![event_1.clone(), event_2.clone()]).await;
    let _ = test_helper.sync_once(vec![event_2.clone()], Some(vec!["$1"])).await;

    let room = test_helper.client.get_room(&room_id).unwrap();
    let timeline =
        Timeline::builder(&room).with_focus(pinned_events_focus(100)).build().await.unwrap();

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
    test_helper.mock_events(vec![event_1.clone(), event_2.clone()]).await;
    test_helper.sync_once(Vec::new(), Some(vec!["$1", "$2"])).await.expect("Sync failed");

    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::Clear);
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.as_event().unwrap().event_id().unwrap(), event_id!("$1"));
    });
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::PushBack { value } => {
        assert_eq!(value.as_event().unwrap().event_id().unwrap(), event_id!("$2"));
    });
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::PushFront { value } => {
        assert!(value.is_date_divider());
    });
    assert_pending!(timeline_stream);

    // Reload timeline with no pinned event
    test_helper.mock_events(vec![event_1.clone(), event_2.clone()]).await;
    let _ = test_helper.sync_once(Vec::new(), Some(Vec::new())).await;

    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::Clear);
    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_max_events_to_load_is_honored() {
    let mut test_helper = TestHelper::new().await;
    let room_id = test_helper.room_id.clone();

    // Join the room
    let _ = test_helper.do_initial_sync().await;

    let f = EventFactory::new().room(&room_id).sender(*BOB);
    let pinned_event = f
        .text_msg("in the end")
        .event_id(event_id!("$1"))
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_timeline();

    // Load initial timeline items: a text message and a `m.room.pinned_events`
    // with event $1 and $2 pinned
    test_helper.mock_events(vec![pinned_event]).await;
    test_helper.sync_once(Vec::new(), Some(vec!["$1", "$2"])).await.expect("Sync failed");

    let room = test_helper.client.get_room(&room_id).unwrap();
    let ret = Timeline::builder(&room).with_focus(pinned_events_focus(1)).build().await;

    // We're only taking the last event id, `$2`, and it's not available so the
    // timeline fails to initialise.
    assert!(ret.is_err());
}

#[async_test]
async fn test_cached_events_are_kept_for_different_room_instances() {
    let mut test_helper = TestHelper::new().await;

    // Subscribe to the event cache.
    test_helper.client.event_cache().subscribe().unwrap();

    let room_id = test_helper.room_id.clone();

    // Join the room
    test_helper.do_initial_sync().await;

    let f = EventFactory::new().room(&room_id).sender(*BOB);
    let pinned_event = f
        .text_msg("in the end")
        .event_id(event_id!("$1"))
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_timeline();

    // Mock /event for some timeline events
    test_helper.mock_events(vec![pinned_event]).await;

    // Load initial timeline items: a `m.room.pinned_events` with event $1 and $2
    // pinned
    test_helper.sync_once(Vec::new(), Some(vec!["$1", "$2"])).await.expect("Sync failed");

    let room = test_helper.client.get_room(&room_id).unwrap();
    let (room_cache, _drop_handles) = room.event_cache().await.unwrap();
    let timeline =
        Timeline::builder(&room).with_focus(pinned_events_focus(2)).build().await.unwrap();

    assert!(
        timeline.live_back_pagination_status().await.is_none(),
        "there should be no live back-pagination status for a focused timeline"
    );

    let (items, mut timeline_stream) = timeline.subscribe().await;

    assert!(!items.is_empty()); // We just loaded some events
    assert_pending!(timeline_stream);

    assert!(room_cache.event(event_id!("$1")).await.is_some());

    // Drop the existing room and timeline instances
    drop(timeline_stream);
    drop(timeline);
    drop(room);

    // Set up a sync response with only the pinned event ids and no events, so if
    // they exist later we know they come from the cache
    test_helper.sync_once(Vec::new(), Some(vec!["$1", "$2"])).await.expect("Sync failed");

    // Get a new room instance
    let room = test_helper.client.get_room(&room_id).unwrap();

    // And a new timeline one
    let timeline =
        Timeline::builder(&room).with_focus(pinned_events_focus(2)).build().await.unwrap();

    let (items, _) = timeline.subscribe().await;
    assert!(!items.is_empty()); // These events came from the cache
    assert!(room_cache.event(event_id!("$1")).await.is_some());

    // Drop the existing room and timeline instances
    test_helper.server.server().reset().await;
    drop(timeline);
    drop(room);

    // Now remove the pinned events from the cache and try again
    test_helper.client.event_cache().empty_immutable_cache().await;

    test_helper.sync_once(Vec::new(), Some(vec!["$1", "$2"])).await.expect("Sync failed");

    // Get a new room instance
    let room = test_helper.client.get_room(&room_id).unwrap();

    // And a new timeline one
    let ret = Timeline::builder(&room).with_focus(pinned_events_focus(2)).build().await;

    // Since the events are no longer in the cache the timeline couldn't load them
    // and can't be initialised.
    assert!(ret.is_err());
}

#[async_test]
async fn test_pinned_timeline_with_pinned_event_ids_and_empty_result_fails() {
    let mut test_helper = TestHelper::new().await;
    let room_id = test_helper.room_id.clone();

    // Join the room
    test_helper.do_initial_sync().await;

    // Load initial timeline items: a `m.room.pinned_events` with event $1 and $2
    // pinned, but they're not available neither in the cache nor in the HS
    test_helper.sync_once(Vec::new(), Some(vec!["$1", "$2"])).await.expect("Sync failed");

    let room = test_helper.client.get_room(&room_id).unwrap();
    let ret = Timeline::builder(&room).with_focus(pinned_events_focus(1)).build().await;

    // The timeline couldn't load any events so it fails to initialise
    assert!(ret.is_err());
}

#[async_test]
async fn test_pinned_timeline_with_no_pinned_event_ids_is_just_empty() {
    let mut test_helper = TestHelper::new().await;
    let room_id = test_helper.room_id.clone();

    // Join the room
    test_helper.do_initial_sync().await;

    // Load initial timeline items: an empty `m.room.pinned_events` event
    test_helper.sync_once(Vec::new(), Some(Vec::new())).await.expect("Sync failed");

    let room = test_helper.client.get_room(&room_id).unwrap();
    let timeline =
        Timeline::builder(&room).with_focus(pinned_events_focus(1)).build().await.unwrap();

    // The timeline couldn't load any events, but it expected none, so it just
    // returns an empty list
    let (items, _) = timeline.subscribe().await;
    assert!(items.is_empty());
}

#[async_test]
async fn test_pinned_timeline_with_no_pinned_events_and_an_utd_on_sync_is_just_empty() {
    let mut test_helper = TestHelper::new().await;
    let room_id = test_helper.room_id.clone();
    let event_id = event_id!("$1:morpheus.localhost");
    let sender_id = owned_user_id!("@example:localhost");

    // Join the room
    let joined_room_builder = JoinedRoomBuilder::new(&room_id)
        // Set up encryption
        .add_state_event(StateTestEvent::Encryption);

    // Sync the joined room
    test_helper
        .server
        .mock_sync()
        .ok_and_run(&test_helper.client, move |sync_builder| {
            sync_builder.add_joined_room(joined_room_builder);
        })
        .await;

    // Load initial timeline items: an empty `m.room.pinned_events` event
    test_helper.sync_once(Vec::new(), Some(Vec::new())).await.expect("Sync failed");

    // Mock encrypted event for which we don't have keys (an UTD)
    let utd_event = create_utd(&room_id, &sender_id, event_id);
    test_helper
        .server
        .mock_room_event()
        .match_event_id()
        .ok(TimelineEvent::new(utd_event))
        .mount()
        .await;

    let room = test_helper.client.get_room(&room_id).unwrap();
    let timeline =
        Timeline::builder(&room).with_focus(pinned_events_focus(1)).build().await.unwrap();

    // The timeline couldn't load any events, but it expected none, so it just
    // returns an empty list
    let (items, _) = timeline.subscribe().await;
    assert!(items.is_empty());
}

#[async_test]
async fn test_pinned_timeline_with_no_pinned_events_on_pagination_is_just_empty() {
    let mut test_helper = TestHelper::new().await;
    let room_id = test_helper.room_id.clone();
    let event_id = event_id!("$1.localhost");
    let sender_id = owned_user_id!("@example:localhost");

    // Join the room
    let joined_room_builder = JoinedRoomBuilder::new(&room_id)
        // Set up encryption
        .add_state_event(StateTestEvent::Encryption);

    // Sync the joined room
    test_helper
        .server
        .mock_sync()
        .ok_and_run(&test_helper.client, move |sync_builder| {
            sync_builder.add_joined_room(joined_room_builder);
        })
        .await;

    // Load initial timeline items: an empty `m.room.pinned_events` event
    test_helper.sync_once(Vec::new(), Some(Vec::new())).await.expect("Sync failed");

    let room = test_helper.client.get_room(&room_id).unwrap();
    let pinned_timeline =
        Timeline::builder(&room).with_focus(pinned_events_focus(1)).build().await.unwrap();

    // The timeline couldn't load any events, but it expected none, so it just
    // returns an empty list
    let (pinned_items, mut pinned_events_stream) = pinned_timeline.subscribe().await;
    assert!(pinned_items.is_empty());

    // Create a non-pinned event to return in the pagination
    let not_pinned_event = EventFactory::new()
        .room(&room_id)
        .sender(&sender_id)
        .text_msg("Hey")
        .event_id(event_id)
        .into_raw_timeline();

    test_helper
        .server
        .mock_room_messages()
        .ok(assign!(RoomMessagesResponseTemplate::default(), {
            chunk: vec![not_pinned_event]
        }))
        .mock_once()
        .mount()
        .await;

    let (event_cache, _) = room.event_cache().await.expect("Event cache should be accessible");

    async fn once(
        outcome: BackPaginationOutcome,
        _timeline_has_been_reset: TimelineHasBeenResetWhilePaginating,
    ) -> ControlFlow<BackPaginationOutcome, ()> {
        ControlFlow::Break(outcome)
    }

    // Paginate backwards once using the event cache to load the event
    event_cache
        .pagination()
        .run_backwards(10, once)
        .await
        .expect("Pagination of events should successful");

    // Assert the event is loaded and added to the cache
    assert!(event_cache.event(event_id).await.is_some());

    // And it won't cause an update in the pinned events timeline since it's not
    // pinned
    assert_pending!(pinned_events_stream);
}

#[async_test]
async fn test_pinned_timeline_with_pinned_utd_on_sync_contains_it() {
    let test_helper = TestHelper::new().await;
    let room_id = test_helper.room_id.clone();
    let event_id = event_id!("$1:morpheus.localhost");
    let sender_id = owned_user_id!("@example:localhost");

    // Join the room
    let joined_room_builder = JoinedRoomBuilder::new(&room_id)
        // Set up encryption
        .add_state_event(StateTestEvent::Encryption)
        // And pinned event ids
        .add_state_event(StateTestEvent::Custom(json!(
            {
                "content": {
                    "pinned": [event_id]
                },
                "event_id": "$15139375513VdeRF:localhost",
                "origin_server_ts": 151393755,
                "sender": sender_id,
                "state_key": "",
                "type": "m.room.pinned_events",
                "unsigned": {
                    "age": 703422
                }
            }
        )));

    // Sync the joined room
    test_helper
        .server
        .mock_sync()
        .ok_and_run(&test_helper.client, move |sync_builder| {
            sync_builder.add_joined_room(joined_room_builder);
        })
        .await;

    // Mock encrypted pinned event for which we don't have keys (an UTD)
    let utd_event = create_utd(&room_id, &sender_id, event_id);
    test_helper
        .server
        .mock_room_event()
        .match_event_id()
        .ok(TimelineEvent::new(utd_event))
        .mock_once()
        .mount()
        .await;

    let room = test_helper.client.get_room(&room_id).unwrap();
    let timeline =
        Timeline::builder(&room).with_focus(pinned_events_focus(1)).build().await.unwrap();

    // The timeline loaded with just a day divider and the pinned UTD
    let (items, _) = timeline.subscribe().await;
    assert_eq!(items.len(), 2);
    let pinned_utd_event = items.last().unwrap().as_event().unwrap();
    assert_eq!(pinned_utd_event.event_id().unwrap(), event_id);
}

#[async_test]
async fn test_edited_events_are_reflected_in_sync() {
    let mut test_helper = TestHelper::new().await;
    let room_id = test_helper.room_id.clone();

    // Join the room
    test_helper.do_initial_sync().await;

    let f = EventFactory::new().room(&room_id).sender(*BOB);
    let pinned_event = f
        .text_msg("in the end")
        .event_id(event_id!("$1"))
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_timeline();

    // Mock /event for some timeline events
    test_helper.mock_events(vec![pinned_event]).await;

    // Load initial timeline items: a text message and a `m.room.pinned_events` with
    // event $1
    test_helper.sync_once(Vec::new(), Some(vec!["$1"])).await.expect("Sync failed");

    let room = test_helper.client.get_room(&room_id).unwrap();
    let timeline =
        Timeline::builder(&room).with_focus(pinned_events_focus(100)).build().await.unwrap();

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

    let edited_event = f
        .text_msg("edited message!")
        .edit(
            event_id!("$1"),
            RoomMessageEventContentWithoutRelation::text_plain("* edited message!"),
        )
        .event_id(event_id!("$2"))
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_timeline();

    // Mock /event for some timeline events
    test_helper.mock_events(vec![edited_event.clone()]).await;

    // Load new pinned event contents from sync, where $2 is and edit on $1
    test_helper.sync_once(vec![edited_event], None).await.expect("Sync failed");

    // The list is reloaded, so it's reset
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::Clear);
    // Then the loaded list items are added
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::PushBack { value } => {
        let event = value.as_event().unwrap();
        assert_eq!(event.event_id().unwrap(), event_id!("$1"));
    });
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::PushFront { value } => {
        assert!(value.is_date_divider());
    });
    // The edit replaces the original event
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::Set { index, value } => {
        assert_eq!(index, 1);
        match value.as_event().unwrap().content() {
            TimelineItemContent::Message(m) => {
                assert_eq!(m.body(), "* edited message!")
            }
            _ => panic!("Should be a message event"),
        }
    });
    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_redacted_events_are_reflected_in_sync() {
    let mut test_helper = TestHelper::new().await;
    let room_id = test_helper.room_id.clone();

    // Join the room
    test_helper.do_initial_sync().await;

    let f = EventFactory::new().room(&room_id).sender(*BOB);
    let pinned_event = f
        .text_msg("in the end")
        .event_id(event_id!("$1"))
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_timeline();

    // Mock /event for some timeline events
    test_helper.mock_events(vec![pinned_event]).await;

    // Load initial timeline items: a text message and a `m.room.pinned_events` with
    // event $1
    test_helper.sync_once(Vec::new(), Some(vec!["$1"])).await.expect("Sync failed");

    let room = test_helper.client.get_room(&room_id).unwrap();
    let timeline =
        Timeline::builder(&room).with_focus(pinned_events_focus(100)).build().await.unwrap();

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
        .into_timeline();

    // Mock /event for some timeline events
    test_helper.mock_events(vec![redaction_event.clone()]).await;

    // Load new pinned event contents from sync, where $1 is now redacted
    let _ = test_helper.sync_once(vec![redaction_event], None).await;

    // The list is reloaded, so it's reset
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::Clear);
    // Then the loaded list items are added
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::PushBack { value } => {
        let event = value.as_event().unwrap();
        assert_eq!(event.event_id().unwrap(), event_id!("$1"));
    });
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::PushFront { value } => {
        assert!(value.is_date_divider());
    });
    // The redaction replaces the original event
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::Set { index, value } => {
        assert_eq!(index, 1);
        assert_matches!(value.as_event().unwrap().content(), TimelineItemContent::RedactedMessage);
    });
    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_edited_events_survive_pinned_event_ids_change() {
    let mut test_helper = TestHelper::new().await;
    let room_id = test_helper.room_id.clone();

    // Join the room
    test_helper.do_initial_sync().await;

    let f = EventFactory::new().room(&room_id).sender(*BOB);
    let pinned_event = f
        .text_msg("in the end")
        .event_id(event_id!("$1"))
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_timeline();

    // Mock /event for some timeline events
    test_helper.mock_events(vec![pinned_event]).await;

    // Load initial timeline items: a `m.room.pinned_events` with event $1 pinned
    test_helper.sync_once(Vec::new(), Some(vec!["$1"])).await.expect("Sync failed");

    let room = test_helper.client.get_room(&room_id).unwrap();
    let timeline =
        Timeline::builder(&room).with_focus(pinned_events_focus(100)).build().await.unwrap();

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

    let edited_pinned_event = f
        .text_msg("* edited message!")
        .edit(
            event_id!("$1"),
            RoomMessageEventContentWithoutRelation::text_plain("edited message!"),
        )
        .event_id(event_id!("$2"))
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_timeline();

    // Mock /event for some timeline events
    test_helper.mock_events(vec![edited_pinned_event.clone()]).await;

    // Load new pinned event contents from sync, $2 was pinned but wasn't available
    // before
    test_helper.sync_once(vec![edited_pinned_event], None).await.expect("Sync failed");

    // The list is reloaded, so it's reset
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::Clear);
    // Then the loaded list items are added
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::PushBack { value } => {
        let event = value.as_event().unwrap();
        assert_eq!(event.event_id().unwrap(), event_id!("$1"));
    });
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::PushFront { value } => {
        assert!(value.is_date_divider());
    });
    // The edit replaces the original event
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::Set { index, value } => {
        assert_eq!(index, 1);
        match value.as_event().unwrap().content() {
            TimelineItemContent::Message(m) => {
                assert_eq!(m.body(), "edited message!")
            }
            _ => panic!("Should be a message event"),
        }
    });
    assert_pending!(timeline_stream);

    let new_pinned_event = f
        .text_msg("new message")
        .event_id(event_id!("$3"))
        .server_ts(MilliSecondsSinceUnixEpoch::now())
        .into_timeline();

    // Mock /event for some timeline events
    test_helper.mock_events(vec![new_pinned_event.clone()]).await;

    // Load new pinned event contents from sync: $3
    test_helper
        .sync_once(vec![new_pinned_event], Some(vec!["$1", "$3"]))
        .await
        .expect("Sync failed");

    // New item gets added
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::PushBack { value } => {
        let event = value.as_event().unwrap();
        assert_eq!(event.event_id().unwrap(), event_id!("$3"));
    });
    // The list is reloaded, so it's reset
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::Clear);
    // Then the loaded list items are added
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::PushBack { value } => {
        let event = value.as_event().unwrap();
        assert_eq!(event.event_id().unwrap(), event_id!("$1"));
    });
    // The edit replaces the original event
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::Set { index, value } => {
        assert_eq!(index, 0);
        match value.as_event().unwrap().content() {
            TimelineItemContent::Message(m) => {
                assert_eq!(m.body(), "edited message!")
            }
            _ => panic!("Should be a message event"),
        }
    });
    // The new pinned event is added
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::PushBack { value } => {
        let event = value.as_event().unwrap();
        assert_eq!(event.event_id().unwrap(), event_id!("$3"));
    });
    assert_next_matches_with_timeout!(timeline_stream, VectorDiff::PushFront { value } => {
        assert!(value.is_date_divider());
    });
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
    let handle = tokio::spawn({
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

struct TestHelper {
    pub client: Client,
    pub server: MatrixMockServer,
    pub room_id: OwnedRoomId,
}

impl TestHelper {
    async fn new() -> Self {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        Self { client, server, room_id: owned_room_id!("!a98sd12bjh:example.org") }
    }

    async fn do_initial_sync(&mut self) {
        let joined_room_builder = JoinedRoomBuilder::new(&self.room_id)
            // Set up encryption
            .add_state_event(StateTestEvent::Encryption);

        self.server
            .mock_sync()
            .ok_and_run(&self.client, move |sync_builder| {
                // Mark the room as joined.
                sync_builder.add_joined_room(joined_room_builder);
            })
            .await;
    }

    async fn mock_events(&mut self, events: Vec<TimelineEvent>) {
        for event in events {
            self.server
                .mock_room_event()
                .room(self.room_id.to_owned())
                .match_event_id()
                .ok(event)
                .mount()
                .await;
        }
    }

    async fn sync_once(
        &mut self,
        timeline_events: Vec<TimelineEvent>,
        pinned_event_ids: Option<Vec<&str>>,
    ) -> Result<(), matrix_sdk::Error> {
        let mut joined_room_builder = JoinedRoomBuilder::new(&self.room_id);
        for timeline_event in timeline_events {
            joined_room_builder = joined_room_builder.add_timeline_event(timeline_event.into_raw());
        }

        if let Some(pinned_event_ids) = pinned_event_ids {
            let pinned_event_ids: Vec<String> =
                pinned_event_ids.into_iter().map(|id| id.to_owned()).collect();
            joined_room_builder =
                joined_room_builder.add_state_event(StateTestEvent::Custom(json!(
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
                )))
        }

        self.server
            .mock_sync()
            .ok_and_run(&self.client, move |sync_builder| {
                sync_builder.add_joined_room(joined_room_builder);
            })
            .await;

        Ok(())
    }
}

fn create_utd(room_id: &RoomId, sender_id: &UserId, event_id: &EventId) -> Raw<AnyTimelineEvent> {
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
        .into_raw_timeline()
}

fn pinned_events_focus(max_events_to_load: u16) -> TimelineFocus {
    TimelineFocus::PinnedEvents { max_events_to_load, max_concurrent_requests: 10 }
}
