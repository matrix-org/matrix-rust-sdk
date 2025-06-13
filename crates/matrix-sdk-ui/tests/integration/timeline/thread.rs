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

use std::ops::Not as _;

use assert_matches2::{assert_let, assert_matches};
use eyeball_im::VectorDiff;
use futures_util::StreamExt as _;
use matrix_sdk::{
    assert_let_timeout,
    test_utils::mocks::{MatrixMockServer, RoomRelationsResponseTemplate},
};
use matrix_sdk_test::{async_test, event_factory::EventFactory, JoinedRoomBuilder, ALICE, BOB};
use matrix_sdk_ui::timeline::{RoomExt as _, TimelineBuilder, TimelineDetails, TimelineFocus};
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

    let timeline = TimelineBuilder::new(&room)
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
        factory
            .text_msg("Threaded event 4")
            .event_id(event_id!("$4"))
            .in_thread_reply(&thread_root_event_id, event_id!("$2"))
            .into_raw_sync()
            .cast(),
        factory
            .text_msg("Threaded event 3")
            .event_id(event_id!("$3"))
            .in_thread(&thread_root_event_id, event_id!("$2"))
            .into_raw_sync()
            .cast(),
    ];

    let batch2 = vec![
        factory
            .text_msg("Threaded event 2")
            .event_id(event_id!("$2"))
            .in_thread(&thread_root_event_id, event_id!("$1"))
            .into_raw_sync()
            .cast(),
        factory
            .text_msg("Threaded event 1")
            .event_id(event_id!("$1"))
            .in_thread(&thread_root_event_id, event_id!("$root"))
            .into_raw_sync()
            .cast(),
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

    let timeline = TimelineBuilder::new(&room)
        .with_focus(TimelineFocus::Thread { root_event_id: thread_root_event_id, num_events: 1 })
        .build()
        .await
        .unwrap();

    let (items, mut timeline_stream) = timeline.subscribe().await;
    assert_pending!(timeline_stream);

    assert_eq!(items.len(), 2 + 1); //  A date divider + the 2 events
    assert!(items[0].is_date_divider());

    let event_item = items[1].as_event().unwrap();
    assert_eq!(event_item.content().as_message().unwrap().body(), "Threaded event 3");
    // In a threaded timeline, threads aren't using the reply fallback, unless
    // they're an actual reply to another thread event.
    assert_matches!(event_item.content().in_reply_to(), None);

    let event_item = items[2].as_event().unwrap();
    assert_eq!(event_item.content().as_message().unwrap().body(), "Threaded event 4");
    // But this one is an actual reply to another thread event, so it has the
    // replied-to event correctly set.
    assert_eq!(event_item.content().in_reply_to().unwrap().event_id, event_id!("$2"));

    let hit_start = timeline.paginate_backwards(100).await.unwrap();
    assert!(hit_start);

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);

    // Remove date separator and insert a new one plus the remaining threaded
    // events and the thread root
    assert_eq!(timeline_updates.len(), 5);

    // Check the timeline diffs
    assert_let!(VectorDiff::PushFront { value } = &timeline_updates[0]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), event_id!("$2"));
    assert_matches!(event_item.content().in_reply_to(), None);

    assert_let!(VectorDiff::PushFront { value } = &timeline_updates[1]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), event_id!("$1"));
    assert_matches!(event_item.content().in_reply_to(), None);

    assert_let!(VectorDiff::PushFront { value } = &timeline_updates[2]);
    assert_eq!(value.as_event().unwrap().event_id().unwrap(), event_id!("$root"));

    assert_let!(VectorDiff::PushFront { value } = &timeline_updates[3]);
    assert!(value.is_date_divider());

    // Remove the other day divider.
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

#[async_test]
async fn test_extract_bundled_thread_summary() {
    // A sync event that includes a bundled thread summary receives a
    // `ThreadSummary` in the associated timeline content.
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a:b.c");
    let room = server.sync_joined_room(&client, room_id).await;

    let timeline = room.timeline().await.unwrap();

    let (initial_items, mut stream) = timeline.subscribe().await;
    assert!(initial_items.is_empty());

    let f = EventFactory::new().room(room_id).sender(&ALICE);
    let thread_event_id = event_id!("$thread_root");
    let latest_event_id = event_id!("$latest_event");

    let event = f
        .text_msg("thready thread mcthreadface")
        .with_bundled_thread_summary(
            f.text_msg("the last one!").event_id(latest_event_id).into_raw(),
            42,
            false,
        )
        .event_id(thread_event_id);

    server.sync_room(&client, JoinedRoomBuilder::new(room_id).add_timeline_event(event)).await;

    assert_let_timeout!(Some(timeline_updates) = stream.next());
    // Message + day divider.
    assert_eq!(timeline_updates.len(), 2);

    // Check the timeline diffs.
    assert_let!(VectorDiff::PushBack { value } = &timeline_updates[0]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), thread_event_id);
    assert_let!(Some(summary) = event_item.content().thread_summary());

    // We get the latest event from the bundled thread summary.
    assert!(summary.latest_event.is_ready());
    assert_let!(TimelineDetails::Ready(latest_event) = summary.latest_event);
    assert_eq!(latest_event.content.as_message().unwrap().body(), "the last one!");
    assert_eq!(latest_event.sender, *ALICE);
    assert!(latest_event.sender_profile.is_unavailable());

    // We get the count from the bundled thread summary.
    assert_eq!(summary.num_replies, 42);

    assert_let!(VectorDiff::PushFront { value } = &timeline_updates[1]);
    assert!(value.is_date_divider());
}

#[async_test]
async fn test_new_thread_reply_causes_thread_summary() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a:b.c");
    let room = server.sync_joined_room(&client, room_id).await;

    let timeline = room.timeline().await.unwrap();

    let (initial_items, mut stream) = timeline.subscribe().await;
    assert!(initial_items.is_empty());

    // Start with a simple message, with no bundled thread info.
    let f = EventFactory::new().room(room_id).sender(&ALICE);
    let thread_event_id = event_id!("$thread_root");

    let event = f.text_msg("thready thread mcthreadface").event_id(thread_event_id);

    server.sync_room(&client, JoinedRoomBuilder::new(room_id).add_timeline_event(event)).await;

    assert_let_timeout!(Some(timeline_updates) = stream.next());
    // Message + day divider.
    assert_eq!(timeline_updates.len(), 2);

    // Sanity check the timeline diffs.
    assert_let!(VectorDiff::PushBack { value } = &timeline_updates[0]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), thread_event_id);
    assert!(event_item.content().thread_summary().is_none());

    assert_let!(VectorDiff::PushFront { value } = &timeline_updates[1]);
    assert!(value.is_date_divider());

    // When I receive a threaded reply to this event,
    let reply_event_id = event_id!("$thread_reply");
    let event = f
        .text_msg("thread reply")
        .sender(&BOB)
        .in_thread(thread_event_id, thread_event_id)
        .event_id(reply_event_id);

    server.sync_room(&client, JoinedRoomBuilder::new(room_id).add_timeline_event(event)).await;

    // The timeline sees the reply.
    //
    // TODO: maybe we should include the thread summaries if and only if the live
    // timeline is configured to exclude thread replies, aka, it requires
    // thread-focused timelines to consult the thread replies.
    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 3);

    assert_let!(VectorDiff::PushBack { value } = &timeline_updates[0]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), reply_event_id);
    assert!(event_item.content().thread_summary().is_none());
    assert_eq!(event_item.content().thread_root().as_deref(), Some(thread_event_id));
    // First, the replied-to event (thread root) doesn't have any thread summary
    // info.
    let replied_to_details = value.as_event().unwrap().content().in_reply_to().unwrap().event;
    assert_let!(TimelineDetails::Ready(replied_to_event) = replied_to_details);
    assert!(replied_to_event.content.thread_summary().is_none());

    // Since the replied-to item (the thread root) has been updated, all replies get
    // updated too, including the item we just pushed.
    assert_let!(VectorDiff::Set { index: 2, value } = &timeline_updates[1]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), reply_event_id);
    let replied_to_details = value.as_event().unwrap().content().in_reply_to().unwrap().event;
    assert_let!(TimelineDetails::Ready(replied_to_event) = replied_to_details);
    // Spoiling a bit here…
    assert!(replied_to_event.content.thread_summary().is_some());

    // And finally, the thread root event receives a thread summary.
    assert_let!(VectorDiff::Set { index: 1, value } = &timeline_updates[2]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), thread_event_id);
    assert!(event_item.content().thread_root().is_none());

    // The thread summary contains the detailed information about the latest event.
    assert_let!(Some(summary) = event_item.content().thread_summary());
    assert!(summary.latest_event.is_ready());
    assert_let!(TimelineDetails::Ready(latest_event) = summary.latest_event);
    assert_eq!(latest_event.content.as_message().unwrap().body(), "thread reply");
    assert_eq!(latest_event.sender, *BOB);
    assert!(latest_event.sender_profile.is_unavailable());

    // The thread summary contains the number of replies.
    assert_eq!(summary.num_replies, 1);

    assert_pending!(stream);

    // A new thread reply updates the number of replies in the thread.
    let another_reply_event_id = event_id!("$another_thread_reply");
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("another thread reply")
                    .sender(&BOB)
                    .in_thread(thread_event_id, reply_event_id)
                    .event_id(another_reply_event_id),
            ),
        )
        .await;

    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 4);

    // (Read receipt from Bob moves.)
    assert_let!(VectorDiff::Set { index: 2, .. } = &timeline_updates[0]);

    // Then we're seeing the new thread reply.
    assert_let!(VectorDiff::PushBack { value } = &timeline_updates[1]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), another_reply_event_id);
    assert!(event_item.content().thread_summary().is_none());
    assert_eq!(event_item.content().thread_root().as_deref(), Some(thread_event_id));

    // Then the first thread reply is updated with the up-to-date thread summary in
    // the replied-to event.
    assert_let!(VectorDiff::Set { index: 2, value } = &timeline_updates[2]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), reply_event_id);
    let replied_to_details = value.as_event().unwrap().content().in_reply_to().unwrap().event;
    assert_let!(TimelineDetails::Ready(replied_to_event) = replied_to_details);
    // Spoiling a bit here…
    assert_eq!(replied_to_event.content.thread_summary().unwrap().num_replies, 2);

    // Then, we receive an update for the thread root itself, which now has an
    // up-to-date thread summary.
    assert_let!(VectorDiff::Set { index: 1, value } = &timeline_updates[3]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), thread_event_id);
    assert!(event_item.content().thread_root().is_none());

    assert_let!(Some(summary) = event_item.content().thread_summary());

    // The latest event has been updated.
    assert!(summary.latest_event.is_ready());
    assert_let!(TimelineDetails::Ready(latest_event) = summary.latest_event);
    assert_eq!(latest_event.content.as_message().unwrap().body(), "another thread reply");
    assert_eq!(latest_event.sender, *BOB);
    assert!(latest_event.sender_profile.is_unavailable());

    // The number of replies has been updated.
    assert_eq!(summary.num_replies, 2);
}

#[async_test]
async fn test_thread_filtering() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a:b.c");
    let sender_id = user_id!("@alice:b.c");
    let thread_root_event_id = owned_event_id!("$root");

    let room = server.sync_joined_room(&client, room_id).await;

    server
        .mock_room_relations()
        .match_target_event(thread_root_event_id.clone())
        .ok(RoomRelationsResponseTemplate::default().next_batch("next_batch"))
        .mock_once()
        .mount()
        .await;

    let filtered_timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Live { hide_threaded_events: true })
        .build()
        .await
        .unwrap();

    let (_, mut filtered_timeline_stream) = filtered_timeline.subscribe().await;

    let timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Live { hide_threaded_events: false })
        .build()
        .await
        .unwrap();

    let (_, mut timeline_stream) = timeline.subscribe().await;

    let thread_timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Thread {
            root_event_id: thread_root_event_id.clone(),
            num_events: 1,
        })
        .build()
        .await
        .unwrap();

    let (_, mut thread_timeline_stream) = thread_timeline.subscribe().await;

    let factory = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    factory
                        .text_msg("Thread root")
                        .sender(sender_id)
                        .event_id(&thread_root_event_id),
                )
                .add_timeline_event(
                    factory
                        .text_msg("Within thread")
                        .sender(sender_id)
                        .event_id(event_id!("$threaded_event"))
                        .in_thread(&thread_root_event_id, &thread_root_event_id),
                ),
        )
        .await;

    // A live timeline hiding in-thread events should only contain the date
    // separator and the thread root.
    {
        assert_let_timeout!(Some(timeline_updates) = filtered_timeline_stream.next());
        assert_eq!(timeline_updates.len(), 3);

        assert_let!(VectorDiff::PushBack { value } = &timeline_updates[0]);
        let event_item = value.as_event().unwrap();
        assert_eq!(event_item.content().as_message().unwrap().body(), "Thread root");
        assert_matches!(event_item.content().thread_summary(), None);

        // The item gets a thread summary.
        assert_let!(VectorDiff::Set { index: 0, value } = &timeline_updates[1]);
        assert_matches!(value.as_event().unwrap().content().thread_summary(), Some(_));

        assert_let!(VectorDiff::PushFront { value } = &timeline_updates[2]);
        assert!(value.is_date_divider());

        assert_pending!(filtered_timeline_stream);
    }

    // A non-filtered live timeline should contain all the items.
    {
        assert_let_timeout!(Some(timeline_updates) = timeline_stream.next());
        assert_eq!(timeline_updates.len(), 6);

        assert_let!(VectorDiff::PushBack { value } = &timeline_updates[0]);
        let event_item = value.as_event().unwrap();
        assert_eq!(event_item.content().as_message().unwrap().body(), "Thread root");
        assert_matches!(event_item.content().thread_summary(), None);
        assert!(event_item.read_receipts().is_empty().not());

        // The read receipt from the author moves to the second item.
        assert_let!(VectorDiff::Set { index: 0, value } = &timeline_updates[1]);
        let event_item = value.as_event().unwrap();
        assert_matches!(event_item.content().thread_summary(), None);
        assert!(event_item.read_receipts().is_empty());

        // The threaded event is pushed to the timeline.
        assert_let!(VectorDiff::PushBack { value } = &timeline_updates[2]);
        assert_eq!(
            value.as_event().unwrap().content().as_message().unwrap().body(),
            "Within thread"
        );

        // The thread summary gets updated:

        // The thread event is a reply (because of the reply fallback), and since its
        // replied-to timeline item has been updated, it also gets updated.
        assert_let!(VectorDiff::Set { index: 1, value } = &timeline_updates[3]);
        assert_eq!(
            value.as_event().unwrap().content().as_message().unwrap().body(),
            "Within thread"
        );

        // Then the thread summary is updated on the thread root.
        assert_let!(VectorDiff::Set { index: 0, value } = &timeline_updates[4]);
        assert_matches!(value.as_event().unwrap().content().thread_summary(), Some(_));

        assert_let!(VectorDiff::PushFront { value } = &timeline_updates[5]);
        assert!(value.is_date_divider());

        // That's all for now, folks!
        assert_pending!(timeline_stream);
    }

    // The threaded timeline should only contain the thread root and the threaded
    // event.
    {
        assert_let_timeout!(Some(timeline_updates) = thread_timeline_stream.next());
        assert_eq!(timeline_updates.len(), 5);

        assert_let!(VectorDiff::PushBack { value } = &timeline_updates[0]);
        let event_item = value.as_event().unwrap();
        assert_eq!(event_item.content().as_message().unwrap().body(), "Thread root");

        // The read receipt from the author moves to the second item.
        assert_let!(VectorDiff::Set { index: 0, value } = &timeline_updates[1]);
        let event_item = value.as_event().unwrap();
        assert_matches!(event_item.content().thread_summary(), None);
        assert!(event_item.read_receipts().is_empty());

        // The threaded event is pushed to the timeline.
        assert_let!(VectorDiff::PushBack { value } = &timeline_updates[2]);
        assert_eq!(
            value.as_event().unwrap().content().as_message().unwrap().body(),
            "Within thread"
        );

        // Then the thread summary is updated on the thread root.
        assert_let!(VectorDiff::Set { index: 0, value } = &timeline_updates[3]);
        assert_matches!(value.as_event().unwrap().content().thread_summary(), Some(_));

        assert_let!(VectorDiff::PushFront { value } = &timeline_updates[4]);
        assert!(value.is_date_divider());

        // That's all for now, folks!
        assert_pending!(timeline_stream);
    }
}
