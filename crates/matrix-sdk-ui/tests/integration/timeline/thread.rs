// Copyright 2023 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::StreamExt as _;
use matrix_sdk::test_utils::mocks::{MatrixMockServer, RoomRelationsResponseTemplate};
use matrix_sdk_test::{async_test, event_factory::EventFactory};
use matrix_sdk_ui::{timeline::TimelineFocus, Timeline};
use ruma::{event_id, events::AnyTimelineEvent, owned_event_id, room_id, serde::Raw, user_id};
use stream_assert::assert_pending;

#[async_test]
async fn test_new_thread() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a:b.c");
    let sender_id = user_id!("@alice:b.c");

    let factory = EventFactory::new().room(room_id).sender(sender_id);

    let thread_root_event_id = owned_event_id!("$root");

    server
        .mock_room_event()
        .match_event_id()
        .ok(factory
            .text_msg("Thread root")
            .sender(sender_id)
            .event_id(&thread_root_event_id)
            .into())
        .mock_once()
        .mount()
        .await;

    server
        .mock_room_relations()
        .match_target_event(thread_root_event_id.clone())
        .ok(RoomRelationsResponseTemplate::default().events(Vec::<Raw<AnyTimelineEvent>>::new()))
        .mock_once()
        .mount()
        .await;

    let room = server.sync_joined_room(&client, room_id).await;

    let timeline = Timeline::builder(&room)
        .with_focus(TimelineFocus::Thread { root_event_id: thread_root_event_id, num_events: 1 })
        .build()
        .await
        .unwrap();

    let (items, mut timeline_stream) = timeline.subscribe().await;

    assert_eq!(items.len(), 1 + 1); // a date divider + the thread root
    assert!(items[0].is_date_divider());
    assert_eq!(items[1].as_event().unwrap().content().as_message().unwrap().body(), "Thread root");
    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_thread_backpagination() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a:b.c");
    let sender_id = user_id!("@alice:b.c");

    let factory = EventFactory::new().room(room_id).sender(sender_id);

    let thread_root_event_id = owned_event_id!("$root");

    server
        .mock_room_event()
        .match_event_id()
        .ok(factory
            .text_msg("Thread root")
            .sender(sender_id)
            .event_id(&thread_root_event_id)
            .into())
        .mock_once()
        .mount()
        .await;

    let batch1 = vec![
        factory.text_msg("Threaded event 4").event_id(event_id!("$3")).into_raw_sync().cast(),
        factory.text_msg("Threaded event 3").event_id(event_id!("$4")).into_raw_sync().cast(),
    ];
    let batch2 = vec![
        factory.text_msg("Threaded event 2").event_id(event_id!("$2")).into_raw_sync().cast(),
        factory.text_msg("Threaded event 1").event_id(event_id!("$1")).into_raw_sync().cast(),
    ];

    server
        .mock_room_relations()
        .match_target_event(thread_root_event_id.clone())
        .ok(RoomRelationsResponseTemplate::default().events(batch1).next_batch("next_batch"))
        .mock_once()
        .mount()
        .await;

    server
        .mock_room_relations()
        .match_target_event(thread_root_event_id.clone())
        .match_from("next_batch")
        .ok(RoomRelationsResponseTemplate::default().events(batch2))
        .mock_once()
        .mount()
        .await;

    let room = server.sync_joined_room(&client, room_id).await;

    let timeline = Timeline::builder(&room)
        .with_focus(TimelineFocus::Thread { root_event_id: thread_root_event_id, num_events: 1 })
        .build()
        .await
        .unwrap();

    let (items, mut timeline_stream) = timeline.subscribe().await;
    assert_pending!(timeline_stream);

    assert_eq!(items.len(), 2 + 1); //  A date divider + the 2 events
    assert!(items[0].is_date_divider());

    assert_eq!(
        items[1].as_event().unwrap().content().as_message().unwrap().body(),
        "Threaded event 3"
    );
    assert_eq!(
        items[2].as_event().unwrap().content().as_message().unwrap().body(),
        "Threaded event 4"
    );

    let hit_start = timeline.paginate_backwards(100).await.unwrap();
    assert!(hit_start);

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);

    // Remove date separator and insert a new one plus the remaining threaded
    // events and the thread root
    assert_eq!(timeline_updates.len(), 5);

    println!("Stefan: {:?}", timeline_updates);

    // Check the timeline diffs
    assert_let!(VectorDiff::PushFront { value } = &timeline_updates[0]);
    assert_eq!(value.as_event().unwrap().event_id().unwrap(), event_id!("$2"));

    assert_let!(VectorDiff::PushFront { value } = &timeline_updates[1]);
    assert_eq!(value.as_event().unwrap().event_id().unwrap(), event_id!("$1"));

    assert_let!(VectorDiff::PushFront { value } = &timeline_updates[2]);
    assert_eq!(value.as_event().unwrap().event_id().unwrap(), event_id!("$root"));

    assert_let!(VectorDiff::PushFront { value } = &timeline_updates[3]);
    assert!(value.is_date_divider());

    assert_let!(VectorDiff::Remove { index: 4 } = &timeline_updates[4]);

    // Check the final items
    let items = timeline.items().await;

    assert!(items[0].is_date_divider());

    assert_eq!(items[1].as_event().unwrap().content().as_message().unwrap().body(), "Thread root");

    assert_eq!(
        items[2].as_event().unwrap().content().as_message().unwrap().body(),
        "Threaded event 1"
    );

    assert_eq!(
        items[3].as_event().unwrap().content().as_message().unwrap().body(),
        "Threaded event 2"
    );
    assert_eq!(
        items[4].as_event().unwrap().content().as_message().unwrap().body(),
        "Threaded event 3"
    );
    assert_eq!(
        items[5].as_event().unwrap().content().as_message().unwrap().body(),
        "Threaded event 4"
    );
}
