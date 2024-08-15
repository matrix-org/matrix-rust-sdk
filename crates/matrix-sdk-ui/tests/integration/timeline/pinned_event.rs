use std::time::Duration;

use assert_matches::assert_matches;
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk::{
    config::SyncSettings,
    sync::SyncResponse,
    test_utils::{events::EventFactory, logged_in_client_with_server},
    Client,
};
use matrix_sdk_test::{async_test, JoinedRoomBuilder, StateTestEvent, SyncResponseBuilder, BOB};
use matrix_sdk_ui::{timeline::TimelineFocus, Timeline};
use ruma::{event_id, owned_room_id, OwnedEventId, OwnedRoomId};
use serde_json::json;
use stream_assert::assert_pending;
use wiremock::MockServer;

use crate::{mock_event, mock_sync};

#[async_test]
async fn test_new_pinned_events_are_added_on_sync() {
    let mut test_helper = TestHelper::new().await;
    let room_id = test_helper.room_id.clone();

    // Join the room
    let _ = test_helper.setup_initial_sync_response().await;
    test_helper.server.reset().await;

    // Load initial timeline items: a text message and a `m.room.pinned_events` with
    // events $1 and $2 pinned
    let _ = test_helper
        .setup_sync_response(vec![("$1", "in the end", false)], Some(vec!["$1", "$2"]))
        .await;

    let room = test_helper.client.get_room(&room_id).unwrap();
    let timeline = Timeline::builder(&room)
        .with_focus(TimelineFocus::PinnedEvents { max_events_to_load: 100 })
        .build()
        .await
        .unwrap();
    test_helper.server.reset().await;

    assert!(
        timeline.live_back_pagination_status().await.is_none(),
        "there should be no live back-pagination status for a focused timeline"
    );

    // Load timeline items
    let (items, mut timeline_stream) = timeline.subscribe().await;

    assert_eq!(items.len(), 1 + 1); // event item + a day divider
    assert!(items[0].is_day_divider());
    assert_eq!(items[1].as_event().unwrap().content().as_message().unwrap().body(), "in the end");
    assert_pending!(timeline_stream);
    test_helper.server.reset().await;

    // Load new pinned event contents from sync, $2 was pinned but wasn't available
    // before
    let _ = test_helper
        .setup_sync_response(
            vec![("$2", "pinned message!", true), ("$3", "normal message", true)],
            None,
        )
        .await;

    // The list is reloaded, so it's reset
    assert_matches!(timeline_stream.next().await.unwrap(), VectorDiff::Clear);
    assert_matches!(timeline_stream.next().await.unwrap(), VectorDiff::PushBack { value } => {
        assert_eq!(value.as_event().unwrap().event_id().unwrap(), event_id!("$1"));
    });
    assert_matches!(timeline_stream.next().await.unwrap(), VectorDiff::PushBack { value } => {
        assert_eq!(value.as_event().unwrap().event_id().unwrap(), event_id!("$2"));
    });
    assert_matches!(timeline_stream.next().await.unwrap(), VectorDiff::PushFront { value } => {
        assert!(value.is_day_divider());
    });
    test_helper.server.reset().await;
}

#[async_test]
async fn test_new_pinned_event_ids_reload_the_timeline() {
    let mut test_helper = TestHelper::new().await;
    let room_id = test_helper.room_id.clone();

    // Join the room
    let _ = test_helper.setup_initial_sync_response().await;
    test_helper.server.reset().await;

    // Load initial timeline items: 2 text messages and a `m.room.pinned_events`
    // with event $1 and $2 pinned
    let _ = test_helper
        .setup_sync_response(
            vec![("$1", "in the end", false), ("$2", "it doesn't even matter", true)],
            Some(vec!["$1"]),
        )
        .await;

    let room = test_helper.client.get_room(&room_id).unwrap();
    let timeline = Timeline::builder(&room)
        .with_focus(TimelineFocus::PinnedEvents { max_events_to_load: 100 })
        .build()
        .await
        .unwrap();

    assert!(
        timeline.live_back_pagination_status().await.is_none(),
        "there should be no live back-pagination status for a focused timeline"
    );

    let (items, mut timeline_stream) = timeline.subscribe().await;

    assert_eq!(items.len(), 1 + 1); // event item + a day divider
    assert!(items[0].is_day_divider());
    assert_eq!(items[1].as_event().unwrap().content().as_message().unwrap().body(), "in the end");
    assert_pending!(timeline_stream);
    test_helper.server.reset().await;

    // Reload timeline with new pinned event ids
    let _ = test_helper
        .setup_sync_response(
            vec![("$1", "in the end", false), ("$2", "it doesn't even matter", false)],
            Some(vec!["$1", "$2"]),
        )
        .await;

    assert_matches!(timeline_stream.next().await.unwrap(), VectorDiff::Clear);
    assert_matches!(timeline_stream.next().await.unwrap(), VectorDiff::PushBack { value } => {
        assert_eq!(value.as_event().unwrap().event_id().unwrap(), event_id!("$1"));
    });
    assert_matches!(timeline_stream.next().await.unwrap(), VectorDiff::PushBack { value } => {
        assert_eq!(value.as_event().unwrap().event_id().unwrap(), event_id!("$2"));
    });
    assert_matches!(timeline_stream.next().await.unwrap(), VectorDiff::PushFront { value } => {
        assert!(value.is_day_divider());
    });
    assert_pending!(timeline_stream);
    test_helper.server.reset().await;

    // Reload timeline with no pinned event ids
    let _ = test_helper
        .setup_sync_response(
            vec![("$1", "in the end", false), ("$2", "it doesn't even matter", false)],
            Some(Vec::new()),
        )
        .await;

    assert_matches!(timeline_stream.next().await.unwrap(), VectorDiff::Clear);
    assert_pending!(timeline_stream);
    test_helper.server.reset().await;
}

#[async_test]
async fn test_max_events_to_load_is_honored() {
    let mut test_helper = TestHelper::new().await;
    let room_id = test_helper.room_id.clone();

    // Join the room
    let _ = test_helper.setup_initial_sync_response().await;
    test_helper.server.reset().await;

    // Load initial timeline items: a text message and a `m.room.pinned_events`
    // with event $1 and $2 pinned
    let _ = test_helper
        .setup_sync_response(vec![("$1", "in the end", false)], Some(vec!["$1", "$2"]))
        .await;

    let room = test_helper.client.get_room(&room_id).unwrap();
    let ret = Timeline::builder(&room)
        .with_focus(TimelineFocus::PinnedEvents { max_events_to_load: 1 })
        .build()
        .await;

    // We're only taking the last event id, `$2`, and it's not available so the
    // timeline fails to initialise.
    assert!(ret.is_err());

    test_helper.server.reset().await;
}

#[async_test]
async fn test_cached_events_are_kept_for_different_room_instances() {
    let mut test_helper = TestHelper::new().await;

    // Subscribe to the event cache.
    test_helper.client.event_cache().subscribe().unwrap();

    let room_id = test_helper.room_id.clone();

    // Join the room
    let _ = test_helper.setup_initial_sync_response().await;
    test_helper.server.reset().await;

    // Load initial timeline items: a text message and a `m.room.pinned_events`
    // with event $1 and $2 pinned
    let _ = test_helper
        .setup_sync_response(vec![("$1", "in the end", false)], Some(vec!["$1", "$2"]))
        .await;

    let room = test_helper.client.get_room(&room_id).unwrap();
    let (room_cache, _drop_handles) = room.event_cache().await.unwrap();
    let timeline = Timeline::builder(&room)
        .with_focus(TimelineFocus::PinnedEvents { max_events_to_load: 2 })
        .build()
        .await
        .unwrap();

    assert!(
        timeline.live_back_pagination_status().await.is_none(),
        "there should be no live back-pagination status for a focused timeline"
    );

    let (items, mut timeline_stream) = timeline.subscribe().await;

    assert!(!items.is_empty()); // We just loaded some events
    assert_pending!(timeline_stream);

    assert!(room_cache.event(event_id!("$1")).await.is_some());

    // Drop the existing room and timeline instances
    test_helper.server.reset().await;
    drop(timeline_stream);
    drop(timeline);
    drop(room);

    // Set up a sync response with only the pinned event ids and no events, so if
    // they exist later we know they come from the cache
    let _ = test_helper.setup_sync_response(Vec::new(), Some(vec!["$1", "$2"])).await;

    // Get a new room instance
    let room = test_helper.client.get_room(&room_id).unwrap();

    // And a new timeline one
    let timeline = Timeline::builder(&room)
        .with_focus(TimelineFocus::PinnedEvents { max_events_to_load: 2 })
        .build()
        .await
        .unwrap();

    let (items, _) = timeline.subscribe().await;
    assert!(!items.is_empty()); // These events came from the cache
    assert!(room_cache.event(event_id!("$1")).await.is_some());

    // Drop the existing room and timeline instances
    test_helper.server.reset().await;
    drop(timeline);
    drop(room);

    // Now remove the pinned events from the cache and try again
    test_helper.client.event_cache().empty_immutable_cache().await;

    let _ = test_helper.setup_sync_response(Vec::new(), Some(vec!["$1", "$2"])).await;

    // Get a new room instance
    let room = test_helper.client.get_room(&room_id).unwrap();

    // And a new timeline one
    let ret = Timeline::builder(&room)
        .with_focus(TimelineFocus::PinnedEvents { max_events_to_load: 2 })
        .build()
        .await;

    // Since the events are no longer in the cache the timeline couldn't load them
    // and can't be initialised.
    assert!(ret.is_err());

    test_helper.server.reset().await;
}

#[async_test]
async fn test_pinned_timeline_with_pinned_event_ids_and_empty_result_fails() {
    let mut test_helper = TestHelper::new().await;
    let room_id = test_helper.room_id.clone();

    // Join the room
    let _ = test_helper.setup_initial_sync_response().await;
    test_helper.server.reset().await;

    // Load initial timeline items: a `m.room.pinned_events` with event $1 and $2
    // pinned, but they're not available neither in the cache nor in the HS
    let _ = test_helper.setup_sync_response(Vec::new(), Some(vec!["$1", "$2"])).await;

    let room = test_helper.client.get_room(&room_id).unwrap();
    let ret = Timeline::builder(&room)
        .with_focus(TimelineFocus::PinnedEvents { max_events_to_load: 1 })
        .build()
        .await;

    // The timeline couldn't load any events so it fails to initialise
    assert!(ret.is_err());

    test_helper.server.reset().await;
}

#[async_test]
async fn test_pinned_timeline_with_no_pinned_event_ids_is_just_empty() {
    let mut test_helper = TestHelper::new().await;
    let room_id = test_helper.room_id.clone();

    // Join the room
    let _ = test_helper.setup_initial_sync_response().await;
    test_helper.server.reset().await;

    // Load initial timeline items: an empty `m.room.pinned_events` event
    let _ = test_helper.setup_sync_response(Vec::new(), Some(Vec::new())).await;

    let room = test_helper.client.get_room(&room_id).unwrap();
    let timeline = Timeline::builder(&room)
        .with_focus(TimelineFocus::PinnedEvents { max_events_to_load: 1 })
        .build()
        .await
        .unwrap();

    // The timeline couldn't load any events, but it expected none, so it just
    // returns an empty list
    let (items, _) = timeline.subscribe().await;
    assert!(items.is_empty());

    test_helper.server.reset().await;
}

struct TestHelper {
    pub client: Client,
    pub server: MockServer,
    pub room_id: OwnedRoomId,
    pub sync_settings: SyncSettings,
    pub sync_response_builder: SyncResponseBuilder,
}

impl TestHelper {
    async fn new() -> Self {
        let (client, server) = logged_in_client_with_server().await;
        Self {
            client,
            server,
            room_id: owned_room_id!("!a98sd12bjh:example.org"),
            sync_settings: SyncSettings::new().timeout(Duration::from_millis(3000)),
            sync_response_builder: SyncResponseBuilder::new(),
        }
    }

    async fn setup_initial_sync_response(&mut self) -> Result<SyncResponse, matrix_sdk::Error> {
        let joined_room_builder = JoinedRoomBuilder::new(&self.room_id)
            // Set up encryption
            .add_state_event(StateTestEvent::Encryption);

        // Mark the room as joined.
        let json_response = self
            .sync_response_builder
            .add_joined_room(joined_room_builder)
            .build_json_sync_response();
        mock_sync(&self.server, json_response, None).await;
        self.client.sync_once(self.sync_settings.clone()).await
    }

    async fn setup_sync_response(
        &mut self,
        text_messages: Vec<(&str, &str, bool)>,
        pinned_event_ids: Option<Vec<&str>>,
    ) -> Result<SyncResponse, matrix_sdk::Error> {
        let mut joined_room_builder = JoinedRoomBuilder::new(&self.room_id);
        for (id, txt, add_to_timeline) in text_messages {
            let event_id: OwnedEventId = id.try_into().unwrap();
            let f = EventFactory::new().room(&self.room_id);
            let event_builder = f.text_msg(txt).event_id(&event_id).sender(*BOB);
            mock_event(&self.server, &self.room_id, &event_id, event_builder.into_timeline()).await;

            if add_to_timeline {
                let event_builder = f.text_msg(txt).event_id(&event_id).sender(*BOB);
                joined_room_builder = joined_room_builder
                    .add_timeline_event(event_builder.into_raw_timeline().cast());
            }
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

        // Mark the room as joined.
        let json_response = self
            .sync_response_builder
            .add_joined_room(joined_room_builder)
            .build_json_sync_response();
        mock_sync(&self.server, json_response, None).await;
        self.client.sync_once(self.sync_settings.clone()).await
    }
}
