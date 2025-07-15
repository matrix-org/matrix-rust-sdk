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
    room::reply::{EnforceThread, Reply},
    test_utils::mocks::{MatrixMockServer, RoomRelationsResponseTemplate},
};
use matrix_sdk_test::{
    ALICE, BOB, JoinedRoomBuilder, RoomAccountDataTestEvent, async_test,
    event_factory::EventFactory,
};
use matrix_sdk_ui::timeline::{RoomExt as _, TimelineBuilder, TimelineDetails, TimelineFocus};
use ruma::{
    MilliSecondsSinceUnixEpoch,
    api::client::receipt::create_receipt::v3::ReceiptType as SendReceiptType,
    event_id,
    events::{
        receipt::{ReceiptThread, ReceiptType},
        room::message::{ReplyWithinThread, RoomMessageEventContentWithoutRelation},
    },
    owned_event_id, room_id, user_id,
};
use stream_assert::assert_pending;
use tokio::task::yield_now;

#[async_test]
async fn test_new_empty_thread() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a:b.c");

    let thread_root_event_id = owned_event_id!("$root");

    let room = server.sync_joined_room(&client, room_id).await;

    let timeline = TimelineBuilder::new(&room)
        .with_focus(TimelineFocus::Thread { root_event_id: thread_root_event_id })
        .build()
        .await
        .unwrap();

    let (items, mut timeline_stream) = timeline.subscribe().await;

    // At first, there are no items in the thread timeline.
    assert!(items.is_empty());
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
            .into_raw(),
        factory
            .text_msg("Threaded event 3")
            .event_id(event_id!("$3"))
            .in_thread(&thread_root_event_id, event_id!("$2"))
            .into_raw(),
    ];

    let batch2 = vec![
        factory
            .text_msg("Threaded event 2")
            .event_id(event_id!("$2"))
            .in_thread(&thread_root_event_id, event_id!("$1"))
            .into_raw(),
        factory
            .text_msg("Threaded event 1")
            .event_id(event_id!("$1"))
            .in_thread(&thread_root_event_id, event_id!("$root"))
            .into_raw(),
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
        .with_focus(TimelineFocus::Thread { root_event_id: thread_root_event_id.clone() })
        .build()
        .await
        .unwrap();

    let (items, mut timeline_stream) = timeline.subscribe().await;

    // At first, there are no items at all.
    assert!(items.is_empty());
    assert_pending!(timeline_stream);

    // We start a first pagination.
    timeline.paginate_backwards(20).await.unwrap();

    // We receive the two events from the first batch, plus a date divider.
    assert_let_timeout!(Some(timeline_updates) = timeline_stream.next());
    assert_eq!(timeline_updates.len(), 3);

    {
        assert_let!(VectorDiff::PushBack { value } = &timeline_updates[0]);
        let event_item = value.as_event().unwrap();
        assert_eq!(event_item.content().as_message().unwrap().body(), "Threaded event 3");
        // In a threaded timeline, threads aren't using the reply fallback, unless
        // they're an actual reply to another thread event.
        assert_matches!(event_item.content().in_reply_to(), None);
    }

    {
        assert_let!(VectorDiff::PushBack { value } = &timeline_updates[1]);
        let event_item = value.as_event().unwrap();
        assert_eq!(event_item.content().as_message().unwrap().body(), "Threaded event 4");
        // But this one is an actual reply to another thread event, so it has the
        // replied-to event correctly set.
        assert_eq!(event_item.content().in_reply_to().unwrap().event_id, event_id!("$2"));
    }

    let hit_start = timeline.paginate_backwards(100).await.unwrap();
    assert!(hit_start);

    assert_let_timeout!(Some(timeline_updates) = timeline_stream.next());

    // Remove date separator and insert a new one plus the remaining threaded
    // events and the thread root.
    assert_eq!(timeline_updates.len(), 3);

    // Check the timeline diffs
    assert_let!(VectorDiff::Insert { index: 1, value } = &timeline_updates[0]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), thread_root_event_id);
    assert_matches!(event_item.content().in_reply_to(), None);

    assert_let!(VectorDiff::Insert { index: 2, value } = &timeline_updates[1]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), event_id!("$1"));
    assert_matches!(event_item.content().in_reply_to(), None);

    assert_let!(VectorDiff::Insert { index: 3, value } = &timeline_updates[2]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), event_id!("$2"));
    assert_matches!(event_item.content().in_reply_to(), None);

    // Check the final items
    let items = timeline.items().await;

    assert_eq!(items.len(), 6);

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
async fn test_new_thread_reply_causes_thread_summary_update() {
    // A new thread reply received in sync will cause the thread root's thread
    // summary to be updated.

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
async fn test_thread_filtering_for_sync() {
    // Make sure that:
    // - a live timeline that shows threaded events will show them
    // - a live timeline that hides threaded events *will* hide them (and only keep
    //   the summary)
    // - a thread timeline will show the threaded events

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a:b.c");
    let sender_id = user_id!("@alice:b.c");
    let thread_root_event_id = owned_event_id!("$root");

    let room = server.sync_joined_room(&client, room_id).await;

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
        .with_focus(TimelineFocus::Thread { root_event_id: thread_root_event_id.clone() })
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

        assert_pending!(timeline_stream);
    }

    // The threaded timeline should only contain the thread root and the threaded
    // event.
    {
        assert_let_timeout!(Some(timeline_updates) = thread_timeline_stream.next());
        assert_eq!(timeline_updates.len(), 4);

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

        assert_let!(VectorDiff::PushFront { value } = &timeline_updates[3]);
        assert!(value.is_date_divider());

        assert_pending!(timeline_stream);
    }
}

#[async_test]
async fn test_thread_timeline_gets_related_events_from_sync() {
    // If a thread timeline receives a sync event related to an in-thread event, it
    // gets updated.

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a:b.c");
    let sender_id = user_id!("@alice:b.c");
    let thread_root_event_id = owned_event_id!("$root");
    let threaded_event_id = event_id!("$threaded_event");

    let room = server.sync_joined_room(&client, room_id).await;

    let timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Thread { root_event_id: thread_root_event_id.clone() })
        .build()
        .await
        .unwrap();

    let (initial_items, mut stream) = timeline.subscribe().await;

    // At first, the timeline is empty.
    assert!(initial_items.is_empty());
    assert_pending!(stream);

    // After a sync with an in-thread event, the timeline receives an update.
    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("Within thread")
                    .sender(sender_id)
                    .event_id(threaded_event_id)
                    .in_thread(&thread_root_event_id, threaded_event_id),
            ),
        )
        .await;

    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 2);

    assert_let!(VectorDiff::PushBack { value } = &timeline_updates[0]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id(), Some(threaded_event_id));
    assert!(event_item.content().reactions().unwrap().is_empty());

    assert_let!(VectorDiff::PushFront { value } = &timeline_updates[1]);
    assert!(value.is_date_divider());

    assert_pending!(stream);

    // When we get a reaction for the in-thread event, from sync, the timeline gets
    // updated, even though the reaction doesn't mention the thread directly.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.reaction(threaded_event_id, "👍").sender(sender_id)),
        )
        .await;

    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value } = &timeline_updates[0]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), threaded_event_id);
    assert!(event_item.content().reactions().unwrap().is_empty().not());

    // If I open another timeline on the same thread, I still see the related event.
    let other_timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Thread { root_event_id: thread_root_event_id })
        .build()
        .await
        .unwrap();

    let (initial_items, _thread_stream) = other_timeline.subscribe().await;

    assert_eq!(initial_items.len(), 2);

    // The date divider.
    assert!(initial_items[0].is_date_divider());

    // The threaded event with the reaction.
    let event_item = initial_items[1].as_event().unwrap();
    assert_eq!(event_item.event_id(), Some(threaded_event_id));
    assert!(event_item.content().reactions().unwrap().is_empty().not());
}

#[async_test]
async fn test_thread_timeline_gets_local_echoes() {
    // If a thread timeline receives a local echo of an in-thread event, it
    // gets updated. If the event is a reaction, it gets updated too.

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a:b.c");
    let thread_root_event_id = owned_event_id!("$root");
    let threaded_event_id = event_id!("$threaded_event");

    let room = server.sync_joined_room(&client, room_id).await;

    let timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Thread { root_event_id: thread_root_event_id.clone() })
        .build()
        .await
        .unwrap();

    let (initial_items, mut stream) = timeline.subscribe().await;

    // At first, the timeline is empty.
    assert!(initial_items.is_empty());
    assert_pending!(stream);

    // Start the timeline with an in-thread event.
    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("hello world")
                    .sender(*ALICE)
                    .event_id(threaded_event_id)
                    .in_thread(&thread_root_event_id, threaded_event_id)
                    .server_ts(MilliSecondsSinceUnixEpoch::now()),
            ),
        )
        .await;

    // Sanity check: I receive the event and the date divider.
    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 2);

    // If I send a local echo for an in-thread event, the timeline receives an
    // update.
    let sent_event_id = event_id!("$sent_msg");
    server.mock_room_state_encryption().plain().mount().await;
    server.mock_room_send().ok(sent_event_id).mock_once().mount().await;
    timeline
        .send_reply(
            RoomMessageEventContentWithoutRelation::text_plain("hello to you too!"),
            Reply {
                event_id: threaded_event_id.to_owned(),
                enforce_thread: EnforceThread::Threaded(ReplyWithinThread::No),
            },
        )
        .await
        .unwrap();

    // I get the local echo for the in-thread event.
    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 1);

    assert_let!(VectorDiff::PushBack { value } = &timeline_updates[0]);
    let event_item = value.as_event().unwrap();
    assert!(event_item.is_local_echo());
    assert!(event_item.event_id().is_none());

    // Then the local echo morphs into a sent local echo.
    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 1);

    assert_let!(VectorDiff::Set { index: 2, value } = &timeline_updates[0]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id(), Some(sent_event_id));
    assert_eq!(event_item.event_id(), Some(sent_event_id));
    assert!(event_item.content().reactions().unwrap().is_empty());

    // Then nothing else.
    assert_pending!(stream);

    // If I send a reaction for the in-thread event, the timeline gets updated, even
    // though the reaction doesn't mention the thread directly.
    server.mock_room_send().ok(event_id!("$reaction_id")).mock_once().mount().await;
    timeline.toggle_reaction(&event_item.identifier(), "👍").await.unwrap();

    // Then I get the reaction as a local echo first.
    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 2, value } = &timeline_updates[0]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), sent_event_id);
    assert!(event_item.content().reactions().unwrap().is_empty().not());

    // Then as a remote echo.
    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 2, value } = &timeline_updates[0]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), sent_event_id);
    assert!(event_item.content().reactions().unwrap().is_empty().not());

    // Then we're done.
    assert_pending!(stream);
}

#[async_test]
async fn test_sending_read_receipt_with_no_events_doesnt_unset_read_flag() {
    // If a thread timeline has no events, then marking it as read doesn't unset the
    // unread flag on the room.

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a:b.c");
    let thread_root_event_id = owned_event_id!("$root");

    // Start with a room manually marked as unread.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_account_data(RoomAccountDataTestEvent::MarkedUnread),
        )
        .await;

    // Create a threaded timeline, with no events in it.
    let timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Thread { root_event_id: thread_root_event_id.clone() })
        .build()
        .await
        .unwrap();

    let (initial_items, mut stream) = timeline.subscribe().await;

    // Sanity check: the timeline is empty.
    assert!(initial_items.is_empty());
    assert_pending!(stream);

    // Try to mark the timeline as read.
    // This should not unset the unread flag on the room (if it tried to do so, the
    // test would fail with a 404, because the endpoint hasn't been set).
    let marked_as_read = timeline.mark_as_read(SendReceiptType::Read).await.unwrap();
    assert!(marked_as_read.not());
}

#[async_test]
async fn test_read_receipts() {
    // Threaded read receipts are correctly handled in a thread timeline.

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a:b.c");
    let thread_root = owned_event_id!("$root");
    let receipt_thread = ReceiptThread::Thread(thread_root.clone());

    // Start with an empty room.
    let room = server.sync_joined_room(&client, room_id).await;

    // Create a threaded timeline, with no events in it.
    let timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Thread { root_event_id: thread_root.clone() })
        .build()
        .await
        .unwrap();

    let (initial_items, mut stream) = timeline.subscribe().await;

    // Sanity check: the timeline is empty.
    assert!(initial_items.is_empty());
    assert_pending!(stream);

    // Receive two events from the server, one of which is a read receipt.
    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.text_msg("hey to you too!")
                        .sender(*ALICE)
                        .in_thread(&thread_root, &thread_root)
                        .event_id(event_id!("$1")),
                )
                .add_timeline_event(
                    f.text_msg("how's it going?")
                        .sender(*ALICE)
                        .in_thread(&thread_root, event_id!("$1"))
                        .event_id(event_id!("$2")),
                )
                .add_timeline_event(
                    f.text_msg("good, u?")
                        .sender(*BOB)
                        .in_thread(&thread_root, event_id!("$2"))
                        .event_id(event_id!("$3")),
                ),
        )
        .await;

    // Sanity check: we receive the three events, plus a date divider.
    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 5);

    // Second event has Alice's implicit read receipt, third event has Bob's
    // implicit read receipt.
    {
        // Alice's event gets pushed back, first, with its own implicit read receipt.
        assert_let!(VectorDiff::PushBack { value } = &timeline_updates[0]);
        let ev0 = value.as_event().unwrap();
        assert_eq!(ev0.event_id(), Some(event_id!("$1")));
        let rr = ev0.read_receipts();
        assert_eq!(rr.len(), 1);
        assert_eq!(rr[*ALICE].thread, receipt_thread);

        // But then, as we're about to push another event from Alice, its read receipt
        // disappears from the first event.
        // XXX wouldn't it be nice that we didn't have this update?
        assert_let!(VectorDiff::Set { index: 0, value } = &timeline_updates[1]);
        let ev0 = value.as_event().unwrap();
        assert_eq!(ev0.event_id(), Some(event_id!("$1")));
        assert!(ev0.read_receipts().is_empty());

        // Second event has Alice's implicit read receipt.
        assert_let!(VectorDiff::PushBack { value } = &timeline_updates[2]);
        let ev1 = value.as_event().unwrap();
        assert_eq!(ev1.event_id(), Some(event_id!("$2")));
        let rr = ev1.read_receipts();
        assert_eq!(rr.len(), 1);
        assert_eq!(rr[*ALICE].thread, receipt_thread);

        // Third event has Bob's implicit read receipt.
        assert_let!(VectorDiff::PushBack { value } = &timeline_updates[3]);
        let ev2 = value.as_event().unwrap();
        assert_eq!(ev2.event_id(), Some(event_id!("$3")));
        let rr = ev2.read_receipts();
        assert_eq!(rr.len(), 1);
        assert_eq!(rr[*BOB].thread, receipt_thread);

        assert_let!(VectorDiff::PushFront { value } = &timeline_updates[4]);
        assert!(value.is_date_divider());
    }

    // Receive a read receipt update:
    // - an explicit read receipt for Alice on $3, which will move their read
    //   receipt to the latest event.
    // - an explicit read receipt for Bob on $3, which will not do anything (because
    //   of the implicit read receipt)
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_receipt(
                f.read_receipts()
                    .add(event_id!("$3"), *ALICE, ReceiptType::Read, receipt_thread.clone())
                    .add(event_id!("$3"), *BOB, ReceiptType::Read, receipt_thread.clone())
                    .into_event(),
            ),
        )
        .await;

    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 2);

    // Alice's previous event is updated, and loses its read receipt.
    {
        assert_let!(VectorDiff::Set { index: 2, value } = &timeline_updates[0]);
        let event_item = value.as_event().unwrap();
        assert_eq!(event_item.event_id(), Some(event_id!("$2")));
        assert!(event_item.read_receipts().is_empty());
    }

    // Bob's event is updated, and it gets a read receipt for Alice.
    {
        assert_let!(VectorDiff::Set { index: 3, value } = &timeline_updates[1]);
        let event_item = value.as_event().unwrap();
        assert_eq!(event_item.event_id(), Some(event_id!("$3")));
        let rr = event_item.read_receipts();
        assert_eq!(rr.len(), 2);
        assert_eq!(rr[*ALICE].thread, receipt_thread);
        assert_eq!(rr[*BOB].thread, receipt_thread);
    }
}

#[async_test]
async fn test_initial_read_receipts_are_correctly_populated() {
    // If there are initial read receipts in the store, they should be correctly
    // populated in the timeline.

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!a:b.c");
    let thread_root = owned_event_id!("$root");
    let receipt_thread = ReceiptThread::Thread(thread_root.clone());

    // Start with a room that has an event with some initial read receipts.
    //
    // It is sync'd *before* the timeline is created, so the timeline will have to
    // load the receipts from the store.
    let f = EventFactory::new();
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.text_msg("hey to you too!")
                        .sender(*ALICE)
                        .in_thread(&thread_root, &thread_root)
                        .event_id(event_id!("$1")),
                )
                .add_receipt(
                    f.read_receipts()
                        .add(event_id!("$1"), *BOB, ReceiptType::Read, receipt_thread.clone())
                        .into_event(),
                ),
        )
        .await;

    // Create a threaded timeline.
    let timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Thread { root_event_id: thread_root.clone() })
        .build()
        .await
        .unwrap();

    let (mut initial_items, mut stream) = timeline.subscribe().await;

    if initial_items.is_empty() {
        assert_let_timeout!(Some(timeline_updates) = stream.next());
        for up in timeline_updates {
            up.apply(&mut initial_items);
        }
    }

    // After stabilizing the timeline, we should see the initial read receipts
    // set as intended.
    assert_eq!(initial_items.len(), 2);
    let ev = initial_items[1].as_event().unwrap();
    assert_eq!(ev.event_id(), Some(event_id!("$1")));
    let rr = ev.read_receipts();
    assert_eq!(rr.len(), 2);
    assert_eq!(rr[*ALICE].thread, receipt_thread);
    assert_eq!(rr[*BOB].thread, receipt_thread);
}

#[async_test]
async fn test_send_read_receipts() {
    // Threaded read receipts can be sent from a thread timeline. Trying to send a
    // read receipt on an event that had one is a no-op.

    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;
    client.event_cache().subscribe().unwrap();

    let user_id = client.user_id().unwrap();

    let room_id = room_id!("!a:b.c");
    let thread_root = owned_event_id!("$root");
    let receipt_thread = ReceiptThread::Thread(thread_root.clone());

    // Start with a room with some events.
    let f = EventFactory::new();
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.text_msg("hey to you too!")
                        .sender(*ALICE)
                        .in_thread(&thread_root, &thread_root)
                        .event_id(event_id!("$1")),
                )
                .add_timeline_event(
                    f.text_msg("how's it going?")
                        .sender(user_id)
                        .in_thread(&thread_root, event_id!("$1"))
                        .event_id(event_id!("$2")),
                )
                .add_timeline_event(
                    f.text_msg("good and you?")
                        .sender(*BOB)
                        .in_thread(&thread_root, event_id!("$2"))
                        .event_id(event_id!("$3")),
                )
                .add_timeline_event(
                    f.text_msg("u there?")
                        .sender(*BOB)
                        .in_thread(&thread_root, event_id!("$3"))
                        .event_id(event_id!("$4")),
                ),
        )
        .await;

    // Create a threaded timeline.
    let timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Thread { root_event_id: thread_root.clone() })
        .build()
        .await
        .unwrap();

    let (mut initial_items, mut stream) = timeline.subscribe().await;

    // Either the initial timeline is not empty, or it will soon receive an update
    // from the event cache.
    if initial_items.is_empty() {
        assert_let_timeout!(Some(timeline_updates) = stream.next());
        for up in timeline_updates {
            up.apply(&mut initial_items);
        }
    }

    // Now that the timeline is populated, we can check the initial read receipts.
    // Note: 0 is the index of the date divider.
    let ev = initial_items[1].as_event().unwrap();
    assert_eq!(ev.event_id(), Some(event_id!("$1")));
    let rr = ev.read_receipts();
    assert_eq!(rr.len(), 1);
    assert_eq!(rr[*ALICE].thread, receipt_thread);

    // The timeline doesn't include the read receipt for the current user, but this
    // is where it would be, if it did.
    let ev = initial_items[2].as_event().unwrap();
    assert_eq!(ev.event_id(), Some(event_id!("$2")));
    assert!(ev.read_receipts().is_empty());

    let ev = initial_items[3].as_event().unwrap();
    assert_eq!(ev.event_id(), Some(event_id!("$3")));
    assert!(ev.read_receipts().is_empty());

    let ev = initial_items[4].as_event().unwrap();
    assert_eq!(ev.event_id(), Some(event_id!("$4")));
    let rr = ev.read_receipts();
    assert_eq!(rr.len(), 1);
    assert_eq!(rr[*BOB].thread, receipt_thread);

    // If the user tries to send a read receipt for an event sent before one of
    // theirs, it is a no-op.
    let did_send =
        timeline.send_single_receipt(SendReceiptType::Read, owned_event_id!("$1")).await.unwrap();
    assert!(did_send.not());

    // If the user tries to send a read receipt for their own event, it is a no-op.
    let did_send =
        timeline.send_single_receipt(SendReceiptType::Read, owned_event_id!("$2")).await.unwrap();
    assert!(did_send.not());

    // But they can send it to a following event.
    server
        .mock_send_receipt(SendReceiptType::Read)
        .match_thread(ReceiptThread::Thread(thread_root.clone()))
        .match_event_id(event_id!("$3"))
        .ok()
        .mock_once()
        .mount()
        .await;

    let did_send =
        timeline.send_single_receipt(SendReceiptType::Read, owned_event_id!("$3")).await.unwrap();
    assert!(did_send);

    // At this point, the read receipts aren't optimistically updated.
    assert_pending!(stream);

    // Simulate a remote echo for the read receipt.
    {
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id).add_receipt(
                    f.read_receipts()
                        .add(event_id!("$3"), user_id, ReceiptType::Read, receipt_thread.clone())
                        .into_event(),
                ),
            )
            .await;

        // The timeline receives an update for the read receipt, but it won't move it,
        // since it doesn't signal our own read receipt.
        yield_now().await;
        assert_pending!(stream);
    }

    // And the user can mark the whole thread as read, which will send a read
    // receipt on the last event.
    server
        .mock_send_receipt(SendReceiptType::Read)
        .match_thread(ReceiptThread::Thread(thread_root.clone()))
        .match_event_id(event_id!("$4"))
        .ok()
        .mock_once()
        .mount()
        .await;

    let did_send = timeline.mark_as_read(SendReceiptType::Read).await.unwrap();
    assert!(did_send);

    // Simulate a remote echo for the read receipt.
    {
        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id).add_receipt(
                    f.read_receipts()
                        .add(event_id!("$4"), user_id, ReceiptType::Read, receipt_thread.clone())
                        .into_event(),
                ),
            )
            .await;

        // The timeline receives an update for the read receipt, but it won't move it,
        // since it doesn't signal our own read receipt.
        yield_now().await;
        assert_pending!(stream);
    }

    // Trying to mark the thread as read again is a no-op.
    let did_send = timeline.mark_as_read(SendReceiptType::Read).await.unwrap();
    assert!(did_send.not());
}
