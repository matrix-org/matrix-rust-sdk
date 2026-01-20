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

use std::{ops::Not as _, time::Duration};

use assert_matches2::{assert_let, assert_matches};
use eyeball_im::VectorDiff;
use futures_util::StreamExt as _;
use matrix_sdk::{
    Client, ThreadingSupport, assert_let_timeout,
    sleep::sleep,
    test_utils::mocks::{
        MatrixMockServer, RoomContextResponseTemplate, RoomRelationsResponseTemplate,
    },
};
use matrix_sdk_test::{
    ALICE, BOB, JoinedRoomBuilder, RoomAccountDataTestEvent, async_test,
    event_factory::EventFactory,
};
use matrix_sdk_ui::timeline::{
    RoomExt as _, TimelineBuilder, TimelineDetails, TimelineEventFocusThreadMode,
    TimelineEventItemId, TimelineFocus, VirtualTimelineItem,
};
use ruma::{
    MilliSecondsSinceUnixEpoch,
    api::client::receipt::create_receipt::v3::ReceiptType as SendReceiptType,
    event_id,
    events::{
        AnySyncTimelineEvent,
        poll::unstable_start::{
            NewUnstablePollStartEventContent, UnstablePollAnswer, UnstablePollStartContentBlock,
            UnstablePollStartEventContent,
        },
        receipt::{ReceiptThread, ReceiptType},
        room::{
            ImageInfo,
            message::{
                Relation, ReplacementMetadata, RoomMessageEventContent,
                RoomMessageEventContentWithoutRelation,
            },
        },
        sticker::{StickerEventContent, StickerMediaSource},
    },
    owned_event_id, owned_mxc_uri, room_id, user_id,
};
use stream_assert::assert_pending;
use tokio::task::yield_now;

async fn client_with_threading_support(server: &MatrixMockServer) -> Client {
    server
        .client_builder()
        .on_builder(|builder| {
            builder.with_threading_support(ThreadingSupport::Enabled { with_subscriptions: false })
        })
        .build()
        .await
}

#[async_test]
async fn test_new_empty_thread() {
    let server = MatrixMockServer::new().await;
    let client = client_with_threading_support(&server).await;

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
    let client = client_with_threading_support(&server).await;

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
    let client = client_with_threading_support(&server).await;

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
            f.text_msg("the last one!").event_id(latest_event_id).into(),
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

    // The read receipts haven't been filled, because we didn't have such
    // information for the current user.
    assert_eq!(summary.public_read_receipt_event_id, None);
    assert_eq!(summary.private_read_receipt_event_id, None);

    assert_let!(VectorDiff::PushFront { value } = &timeline_updates[1]);
    assert!(value.is_date_divider());
}

#[async_test]
async fn test_new_thread_reply_causes_thread_summary_update() {
    // A new thread reply received in sync will cause the thread root's thread
    // summary to be updated.

    let server = MatrixMockServer::new().await;
    let client = client_with_threading_support(&server).await;

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

    // The read receipts haven't been filled, because we didn't have such
    // information for the current user.
    assert_eq!(summary.public_read_receipt_event_id, None);
    assert_eq!(summary.private_read_receipt_event_id, None);

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

    // The read receipts haven't been filled, because we didn't have such
    // information for the current user.
    assert_eq!(summary.public_read_receipt_event_id, None);
    assert_eq!(summary.private_read_receipt_event_id, None);
}

#[async_test]
async fn test_thread_msg_edit_reflects_in_summary() {
    // A new message edit of a threaded reply received in sync (but after we already
    // had a thread) will cause the thread root's thread summary to be updated
    // with the latest content.

    let server = MatrixMockServer::new().await;
    let client = client_with_threading_support(&server).await;

    let room_id = room_id!("!a:b.c");
    let room = server.sync_joined_room(&client, room_id).await;

    let timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Live { hide_threaded_events: true })
        .build()
        .await
        .unwrap();

    let (initial_items, mut stream) = timeline.subscribe().await;
    assert!(initial_items.is_empty());

    // Start with a message (with no bundled thread info), and a threaded reply to
    // it.
    let f = EventFactory::new().room(room_id).sender(&ALICE);
    let thread_event_id = event_id!("$thread_root");
    let reply_event_id = event_id!("$thread_reply");

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.text_msg("thready thread mcthreadface").event_id(thread_event_id),
                )
                .add_timeline_event(
                    f.text_msg("threaded reply")
                        .sender(&BOB)
                        .in_thread(thread_event_id, thread_event_id)
                        .event_id(reply_event_id),
                ),
        )
        .await;

    assert_let_timeout!(Some(timeline_updates) = stream.next());
    // Thread root + new implicit read receipt + new thread summary + day divider.
    // TODO: could we optimize this, to have only a single timeline update?
    assert_eq!(timeline_updates.len(), 4);

    // Sanity check the timeline diffs.
    {
        assert_let!(VectorDiff::PushBack { value } = &timeline_updates[0]);
        let event_item = value.as_event().unwrap();
        assert_eq!(event_item.event_id().unwrap(), thread_event_id);
        // At first, the summary isn't here.
        assert!(event_item.content().thread_summary().is_none());
        // And it only has the read receipt of its author.
        assert_eq!(event_item.read_receipts().len(), 1);

        // Then, a read-receipt "set" because Bob having sent a reply means he's seen
        // the thread root.
        assert_let!(VectorDiff::Set { index: 0, value } = &timeline_updates[1]);
        let event_item = value.as_event().unwrap();
        assert_eq!(event_item.event_id().unwrap(), thread_event_id);
        assert!(event_item.content().thread_summary().is_none());
        // Now, with Bob's read receipt.
        assert_eq!(event_item.read_receipts().len(), 2);

        // Eventually the summary comes in.
        assert_let!(VectorDiff::Set { index: 0, value } = &timeline_updates[2]);
        let event_item = value.as_event().unwrap();
        assert_eq!(event_item.event_id().unwrap(), thread_event_id);
        // Now there is a summary!
        assert_let!(Some(summary) = event_item.content().thread_summary());
        assert_let!(TimelineDetails::Ready(latest_event) = summary.latest_event);
        assert_eq!(
            latest_event.identifier,
            TimelineEventItemId::EventId(reply_event_id.to_owned())
        );
        assert_eq!(latest_event.content.as_message().unwrap().body(), "threaded reply");
        assert_eq!(summary.num_replies, 1);

        // And finally, the day divider.
        assert_let!(VectorDiff::PushFront { value } = &timeline_updates[3]);
        assert!(value.is_date_divider());
    }

    // When I receive an edit to that event, via sync,
    let edit_reply_event_id = event_id!("$thread_reply_edit");

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("*edited* threaded reply")
                    .edit(
                        reply_event_id,
                        RoomMessageEventContentWithoutRelation::text_plain("edited threaded reply"),
                    )
                    .sender(&BOB)
                    .event_id(edit_reply_event_id),
            ),
        )
        .await;

    // The root message is updated.
    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 1);

    assert_let!(VectorDiff::Set { index: 1, value } = &timeline_updates[0]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), thread_event_id);
    assert_let!(Some(summary) = event_item.content().thread_summary());

    // The latest event has been updated.
    assert_let!(TimelineDetails::Ready(latest_event) = summary.latest_event);
    assert_eq!(
        latest_event.identifier,
        TimelineEventItemId::EventId(edit_reply_event_id.to_owned())
    );
    assert_eq!(latest_event.content.as_message().unwrap().body(), "edited threaded reply");

    // Still only one reply; it's been edited.
    assert_eq!(summary.num_replies, 1);

    // That's all, folks!
    assert_pending!(stream);
}

#[async_test]
async fn test_thread_poll_edit_reflects_in_summary() {
    // A new poll edit of a threaded reply received in sync (at the same time as the
    // original poll) will cause the thread root's thread summary to be updated
    // with the latest content.

    let server = MatrixMockServer::new().await;
    let client = client_with_threading_support(&server).await;

    let room_id = room_id!("!a:b.c");
    let room = server.sync_joined_room(&client, room_id).await;

    let timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Live { hide_threaded_events: true })
        .build()
        .await
        .unwrap();

    let (initial_items, mut stream) = timeline.subscribe().await;
    assert!(initial_items.is_empty());

    // Have all the events, including the edit, arrive in the same sync.
    let f = EventFactory::new().room(room_id).sender(&ALICE);
    let thread_event_id = event_id!("$thread_root");
    let reply_event_id = event_id!("$thread_reply");
    let edit_reply_event_id = event_id!("$thread_reply_edit");

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.text_msg("thready thread mcthreadface").event_id(thread_event_id),
                )
                .add_timeline_event(
                    f.poll_start(
                        "What's your favorite colour? Red, green, or blue?",
                        "What's your favorite colour?",
                        vec!["Red", "Green", "Blue"],
                    )
                    .sender(&BOB)
                    .in_thread(thread_event_id, thread_event_id)
                    .event_id(reply_event_id),
                )
                .add_timeline_event(
                    f.poll_edit(
                        reply_event_id,
                        // TODO: allow changing the fallback text too, in the event factory?
                        //"What's your favourite colour? Red, green, blue, or yellow?",
                        "What's your favourite colour?",
                        vec!["Red", "Green", "Blue", "Yellow"],
                    )
                    .sender(&BOB)
                    .event_id(edit_reply_event_id),
                ),
        )
        .await;

    assert_let_timeout!(Some(timeline_updates) = stream.next());
    // Thread root + new implicit read receipt + new thread summary + day divider.
    // TODO: could we optimize this, to have only a single timeline update?
    assert_eq!(timeline_updates.len(), 4);

    assert_let!(VectorDiff::PushBack { value } = &timeline_updates[0]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), thread_event_id);
    // At first, the summary isn't here.
    assert!(event_item.content().thread_summary().is_none());
    // And it only has the read receipt of its author.
    assert_eq!(event_item.read_receipts().len(), 1);

    // Then, a read-receipt "set" because Bob having sent a reply means he's seen
    // the thread root.
    assert_let!(VectorDiff::Set { index: 0, value } = &timeline_updates[1]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), thread_event_id);
    assert!(event_item.content().thread_summary().is_none());
    // Now, with Bob's read receipt.
    assert_eq!(event_item.read_receipts().len(), 2);

    // Eventually the summary comes in.
    assert_let!(VectorDiff::Set { index: 0, value } = &timeline_updates[2]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), thread_event_id);
    // Now there is a summary!
    assert_let!(Some(summary) = event_item.content().thread_summary());
    assert_let!(TimelineDetails::Ready(latest_event) = summary.latest_event);
    assert_eq!(
        latest_event.identifier,
        TimelineEventItemId::EventId(edit_reply_event_id.to_owned())
    );

    let poll_results = latest_event.content.as_poll().unwrap().results();
    assert_eq!(poll_results.question, "What's your favourite colour?");
    assert_eq!(poll_results.answers.len(), 4);

    assert_eq!(summary.num_replies, 1);

    // And finally, the day divider.
    assert_let!(VectorDiff::PushFront { value } = &timeline_updates[3]);
    assert!(value.is_date_divider());

    // That's all, folks!
    assert_pending!(stream);
}

#[async_test]
async fn test_thread_filtering_for_sync() {
    // Make sure that:
    // - a live timeline that shows threaded events will show them
    // - a live timeline that hides threaded events *will* hide them (and only keep
    //   the summary)
    // - a thread timeline will show the threaded events

    let server = MatrixMockServer::new().await;
    let client = client_with_threading_support(&server).await;

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
    let client = client_with_threading_support(&server).await;

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
    let client = client_with_threading_support(&server).await;

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

    let (event_receiver, mock_builder) =
        server.mock_room_send().ok_with_capture(sent_event_id, *ALICE);
    mock_builder.mock_once().mount().await;

    timeline.send(RoomMessageEventContent::text_plain("hello to you too!").into()).await.unwrap();

    // I get the local echo for the in-thread event.
    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 1);

    assert_let!(VectorDiff::PushBack { value } = &timeline_updates[0]);
    let event_item = value.as_event().unwrap();
    assert!(event_item.is_local_echo());
    assert!(event_item.event_id().is_none());

    // The thread information is properly filled.
    let msglike = event_item.content().as_msglike().unwrap();
    assert_eq!(msglike.thread_root.as_ref(), Some(&thread_root_event_id));
    assert!(msglike.in_reply_to.is_none());

    // Then the local echo morphs into a sent local echo.
    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 3);

    // The local event is updated.
    assert_let!(VectorDiff::Set { index: 2, value } = &timeline_updates[0]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id(), Some(sent_event_id));
    assert!(event_item.content().reactions().unwrap().is_empty());

    // The local event is inserted in the Event Cache as a remote event.
    assert_matches!(&timeline_updates[1], VectorDiff::Remove { index: 2 });
    assert_let!(VectorDiff::PushBack { value: remote_event } = &timeline_updates[2]);
    assert_eq!(remote_event.as_event().unwrap().event_id(), Some(sent_event_id));

    // Then nothing else.
    assert_pending!(stream);

    // The raw event includes the correctly reply-to fallback.
    {
        let raw_event = event_receiver.await.unwrap();
        let event = raw_event.deserialize().unwrap();
        assert_let!(
            AnySyncTimelineEvent::MessageLike(
                ruma::events::AnySyncMessageLikeEvent::RoomMessage(event)
            ) = event
        );
        let event = event.as_original().unwrap();
        assert_let!(Some(Relation::Thread(thread)) = event.content.relates_to.clone());
        assert_eq!(thread.event_id, thread_root_event_id);
        assert!(thread.is_falling_back);
        // The reply-to fallback is set to the latest in-thread event, not the thread
        // root.
        assert_eq!(thread.in_reply_to.unwrap().event_id, threaded_event_id);
    }

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
    assert!(!timeline_updates.is_empty());

    // Sometimes, a double `VectorDiff::Set` is received because of another update.
    // Let's make the test reliable.
    for timeline_update in timeline_updates {
        assert_let!(VectorDiff::Set { index: 2, value } = timeline_update);
        let event_item = value.as_event().unwrap();
        assert_eq!(event_item.event_id().unwrap(), sent_event_id);
        assert!(event_item.content().reactions().unwrap().is_empty().not());
    }

    // Then we're done.
    assert_pending!(stream);
}

#[async_test]
async fn test_thread_timeline_can_send_edit() {
    // If I send an edit to a threaded timeline, it just works (aka the system to
    // set the threaded relationship doesn't kick in, since there's already a
    // relationship).

    let server = MatrixMockServer::new().await;
    let client = client_with_threading_support(&server).await;

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

    // If I send an edit to an in-thread event, the timeline receives an update.
    // Note: it's a bit far fetched to send an edit without using
    // `Timeline::edit()`, but since it's possible…
    let sent_event_id = event_id!("$sent_msg");
    server.mock_room_state_encryption().plain().mount().await;

    // No mock_once here: this endpoint may or may not be called, depending on
    // timing of the end of the test.
    server.mock_room_send().ok(sent_event_id).mount().await;

    timeline
        .send(
            RoomMessageEventContent::text_plain("bonjour monde")
                .make_replacement(ReplacementMetadata::new(threaded_event_id.to_owned(), None))
                .into(),
        )
        .await
        .unwrap();

    // I get the local echo for the in-thread event.
    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 1);

    assert_let!(VectorDiff::Set { index: 1, value } = &timeline_updates[0]);
    let event_item = value.as_event().unwrap();

    // The thread information is (still) present.
    let msglike = event_item.content().as_msglike().unwrap();
    assert_eq!(msglike.thread_root.as_ref(), Some(&thread_root_event_id));
    assert!(msglike.in_reply_to.is_none());

    // Text is eagerly updated.
    assert_eq!(msglike.as_message().unwrap().body(), "bonjour monde");

    // Then we're done.
    assert_pending!(stream);
}

#[async_test]
async fn test_send_sticker_thread() {
    // If I send a sticker to a threaded timeline, it just works (aka the system to
    // set the threaded relationship does kick in).

    let server = MatrixMockServer::new().await;
    let client = client_with_threading_support(&server).await;

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

    server.mock_room_state_encryption().plain().mount().await;

    let sent_event_id = event_id!("$sent_msg");
    server.mock_room_send().ok(sent_event_id).mount().await;

    let media_src = owned_mxc_uri!("mxc://example.com/1");
    timeline
        .send(
            StickerEventContent::new("sticker!".to_owned(), ImageInfo::new(), media_src.clone())
                .into(),
        )
        .await
        .unwrap();

    // I get the local echo for the in-thread event.
    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 1);

    assert_let!(VectorDiff::PushBack { value } = &timeline_updates[0]);
    let event_item = value.as_event().unwrap();

    // The content matches what we expect.
    let sticker_item = event_item.content().as_sticker().unwrap();
    let content = sticker_item.content();
    assert_eq!(content.body, "sticker!");
    assert_let!(StickerMediaSource::Plain(plain) = content.source.clone());
    assert_eq!(plain, media_src);

    // Then we're done.
    assert_pending!(stream);
}

#[async_test]
async fn test_send_poll_thread() {
    // If I send a poll to a threaded timeline, it just works (aka the system to
    // set the threaded relationship does kick in).

    let server = MatrixMockServer::new().await;
    let client = client_with_threading_support(&server).await;

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

    server.mock_room_state_encryption().plain().mount().await;

    let sent_event_id = event_id!("$sent_msg");
    server.mock_room_send().ok(sent_event_id).mount().await;

    timeline
        .send(
            UnstablePollStartEventContent::New(NewUnstablePollStartEventContent::plain_text(
                "let's vote",
                UnstablePollStartContentBlock::new(
                    "what day is it today?",
                    vec![
                        UnstablePollAnswer::new("0", "monday"),
                        UnstablePollAnswer::new("1", "friday"),
                    ]
                    .try_into()
                    .unwrap(),
                ),
            ))
            .into(),
        )
        .await
        .unwrap();

    // I get the local echo for the in-thread event.
    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 1);

    assert_let!(VectorDiff::PushBack { value } = &timeline_updates[0]);
    let event_item = value.as_event().unwrap();

    // The content is a poll.
    assert!(event_item.content().is_poll());

    // Then we're done.
    assert_pending!(stream);
}

#[async_test]
async fn test_sending_read_receipt_with_no_events_doesnt_unset_read_flag() {
    // If a thread timeline has no events, then marking it as read doesn't unset the
    // unread flag on the room.

    let server = MatrixMockServer::new().await;
    let client = client_with_threading_support(&server).await;

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
    let client = client_with_threading_support(&server).await;

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
    let client = client_with_threading_support(&server).await;
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
async fn test_initial_read_receipts_compatibility_mode() {
    // If there are initial read receipts in the store, that are using the
    // "unthreaded" kind of receipt, then they're used as "main" receipts as
    // well.

    let server = MatrixMockServer::new().await;
    let client = client_with_threading_support(&server).await;
    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!a:b.c");

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
                    f.text_msg("hello in main timeline").sender(*BOB).event_id(event_id!("$0")),
                )
                .add_timeline_event(
                    f.text_msg("hello back in main timeline")
                        .sender(*ALICE)
                        .event_id(event_id!("$1")),
                )
                .add_receipt(
                    f.read_receipts()
                        .add(event_id!("$1"), *BOB, ReceiptType::Read, ReceiptThread::Unthreaded)
                        .into_event(),
                ),
        )
        .await;

    // Create a main timeline.
    let timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Live { hide_threaded_events: true })
        .build()
        .await
        .unwrap();

    let (mut initial_items, mut stream) = timeline.subscribe().await;

    // Wait for the initial items.
    if initial_items.is_empty() {
        assert_let_timeout!(Some(timeline_updates) = stream.next());
        for up in timeline_updates {
            up.apply(&mut initial_items);
        }
    }

    // After stabilizing the timeline, we should see the initial read receipts
    // set as intended.
    assert_eq!(initial_items.len(), 3);

    assert!(initial_items[0].is_date_divider());

    {
        let ev = initial_items[1].as_event().unwrap();
        assert_eq!(ev.event_id(), Some(event_id!("$0")));
        let rr = ev.read_receipts();
        assert!(rr.is_empty());
    }

    {
        let ev = initial_items[2].as_event().unwrap();
        assert_eq!(ev.event_id(), Some(event_id!("$1")));
        let rr = ev.read_receipts();
        assert_eq!(rr.len(), 2);
        assert!(rr.get(*ALICE).is_some());
        assert!(rr.get(*BOB).is_some());
    }
}

#[async_test]
async fn test_send_read_receipts() {
    // Threaded read receipts can be sent from a thread timeline. Trying to send a
    // read receipt on an event that had one is a no-op.

    let server = MatrixMockServer::new().await;
    let client = client_with_threading_support(&server).await;
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

    // Since the room has no `m.fully_read` event, the read marker falls back to
    // the normal read receipt.
    let ev = initial_items[3].as_virtual().unwrap();
    assert!(matches!(ev, VirtualTimelineItem::ReadMarker));

    let ev = initial_items[4].as_event().unwrap();
    assert_eq!(ev.event_id(), Some(event_id!("$3")));
    assert!(ev.read_receipts().is_empty());

    let ev = initial_items[5].as_event().unwrap();
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

#[async_test]
async fn test_permalink_doesnt_listen_to_thread_sync() {
    let server = MatrixMockServer::new().await;

    let client = client_with_threading_support(&server).await;
    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!a:b.c");
    let room = server.sync_joined_room(&client, room_id).await;

    let f = EventFactory::new().sender(&ALICE).room(room_id);
    let thread_root = event_id!("$thread_root");
    let event = f
        .text_msg("hey to you too")
        .event_id(event_id!("$target"))
        .in_thread(thread_root, thread_root)
        .into_event();

    server
        .mock_room_event_context()
        .ok(RoomContextResponseTemplate::new(event))
        .mock_once()
        .mount()
        .await;

    let timeline = TimelineBuilder::new(&room)
        .with_focus(TimelineFocus::Event {
            target: owned_event_id!("$target"),
            num_context_events: 2,
            thread_mode: TimelineEventFocusThreadMode::Automatic { hide_threaded_events: true },
        })
        .build()
        .await
        .unwrap();

    // Sanity check.
    assert!(timeline.is_threaded());

    // Subscribe to the timeline changes.
    let (initial_items, mut stream) = timeline.subscribe().await;

    // Initially, it contains a date divider and the target event only.
    assert_eq!(initial_items.len(), 2);
    assert!(initial_items[0].is_date_divider());
    assert_eq!(initial_items[1].as_event().unwrap().event_id(), Some(event_id!("$target")));

    assert_pending!(stream);

    // If a back-pagination happens in the thread event cache, it should NOT be
    // reflected in the focused timeline.
    server
        .mock_room_relations()
        .match_target_event(thread_root.to_owned())
        .ok(RoomRelationsResponseTemplate::default()
            .events(vec![
                f.text_msg("a new threaded event")
                    .event_id(event_id!("$new_threaded"))
                    .in_thread(thread_root, event_id!("$new_threaded")),
            ])
            // Include a next-batch token to not have to mock /event for the root.
            .next_batch("next-batch"))
        .mock_once()
        .mount()
        .await;

    let (room_event_cache, _drop_guards) = room.event_cache().await.unwrap();
    let hit_start =
        room_event_cache.paginate_thread_backwards(thread_root.to_owned(), 42).await.unwrap();
    assert!(hit_start.not());

    sleep(Duration::from_millis(100)).await;

    // The stream should still be pending!
    assert_pending!(stream);
}

#[async_test]
async fn test_redacted_replied_to_is_updated() {
    // When we're in a thread, and the thread has one thread reply, and another
    // in-thread reply to the first threaded reply; if the first threaded reply
    // is redacted, the second in-thread reply should have its replied-to
    // information updated correctly.
    let server = MatrixMockServer::new().await;

    let client = client_with_threading_support(&server).await;
    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!a:b.c");
    let f = EventFactory::new().sender(&ALICE).room(room_id);
    let thread_root = event_id!("$thread_root");
    let first_reply = event_id!("$first_reply");
    let second_reply = event_id!("$second_reply");

    let room = server.sync_joined_room(&client, room_id).await;

    let timeline = TimelineBuilder::new(&room)
        .with_focus(TimelineFocus::Thread { root_event_id: thread_root.to_owned() })
        .build()
        .await
        .unwrap();

    // Subscribe to the timeline changes.
    let (initial_items, mut stream) = timeline.subscribe().await;
    assert!(initial_items.is_empty());
    assert_pending!(stream);

    // After syncing the initial thread content,
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.text_msg("hey to you too!")
                        .event_id(first_reply)
                        .in_thread(thread_root, thread_root),
                )
                .add_timeline_event(
                    f.text_msg("how are you doing?")
                        .event_id(second_reply)
                        .in_thread_reply(thread_root, first_reply),
                ),
        )
        .await;

    // The timeline sees the new events.
    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 3);
    assert_let!(VectorDiff::PushBack { value } = &timeline_updates[0]);
    let ev1 = value.as_event().unwrap();
    assert_eq!(ev1.event_id(), Some(first_reply));

    assert_let!(VectorDiff::PushBack { value } = &timeline_updates[1]);
    let ev2 = value.as_event().unwrap();
    assert_eq!(ev2.event_id(), Some(second_reply));

    let msglike = ev2.content().as_msglike().unwrap();
    let in_reply_to = msglike.in_reply_to.as_ref().unwrap();
    assert_eq!(in_reply_to.event_id, first_reply);
    assert_let!(TimelineDetails::Ready(replied_to) = &in_reply_to.event);
    assert!(replied_to.content.is_message());

    assert_let!(VectorDiff::PushFront { value } = &timeline_updates[2]);
    assert!(value.is_date_divider());

    // When the first reply is redacted,
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.redaction(first_reply).event_id(event_id!("$redaction"))),
        )
        .await;

    // The timeline sees the redaction as a removal,
    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 2);

    assert_let!(VectorDiff::Remove { index: 1 } = &timeline_updates[0]);

    // And then the replied-to update happens independently.
    assert_let!(VectorDiff::Set { index: 1, value } = &timeline_updates[1]);
    let ev2 = value.as_event().unwrap();
    assert_eq!(ev2.event_id(), Some(second_reply));
    let msglike = ev2.content().as_msglike().unwrap();
    let in_reply_to = msglike.in_reply_to.as_ref().unwrap();
    assert_eq!(in_reply_to.event_id, first_reply);
    assert_let!(TimelineDetails::Ready(replied_to_event) = &in_reply_to.event);
    assert!(replied_to_event.content.is_redacted());
}

#[async_test]
async fn test_redaction_affects_thread_summary() {
    // When an in-thread event is being redacted, the thread summary of the root
    // will be correctly updated.

    let server = MatrixMockServer::new().await;
    let client = client_with_threading_support(&server).await;

    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!a:b.c");
    let f = EventFactory::new().room(room_id).sender(&ALICE);

    let thread_root = event_id!("$thread_root");
    let thread_reply = event_id!("$thread_reply");

    // Start with an initial sync with a thread root and a threaded reply.
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("thread root").event_id(thread_root))
                .add_timeline_event(
                    f.text_msg("thread reply")
                        .in_thread(thread_root, thread_root)
                        .event_id(thread_reply),
                ),
        )
        .await;

    // Create a main timeline.
    let timeline = room
        .timeline_builder()
        .with_focus(TimelineFocus::Live { hide_threaded_events: true })
        .build()
        .await
        .unwrap();

    let (mut initial_items, mut stream) = timeline.subscribe().await;

    // Wait for the timeline's state to stabilize.
    if initial_items.is_empty() {
        assert_let_timeout!(Some(timeline_updates) = stream.next());
        for up in timeline_updates {
            up.apply(&mut initial_items);
        }
    }

    assert_eq!(initial_items.len(), 2);
    assert!(initial_items[0].is_date_divider());

    // The root has a summary.
    let event_item = initial_items[1].as_event().unwrap();
    assert_eq!(event_item.event_id(), Some(thread_root));
    let summary = event_item.content().as_msglike().unwrap().thread_summary.as_ref().unwrap();
    assert_eq!(summary.num_replies, 1);
    assert_let!(TimelineDetails::Ready(embedded) = &summary.latest_event);
    assert_eq!(embedded.identifier, TimelineEventItemId::EventId(thread_reply.to_owned()));

    assert_pending!(stream);

    // We receive a redaction over sync for the one reply to the thread root.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.redaction(thread_reply).event_id(event_id!("$redaction"))),
        )
        .await;

    // The thread summary has disappeared!
    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value } = &timeline_updates[0]);
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id(), Some(thread_root));
    assert!(event_item.content().as_msglike().unwrap().thread_summary.is_none());
}

#[async_test]
async fn test_main_timeline_has_receipts_in_thread_summaries() {
    // The initial read receipts for thread events are filled, as part of the
    // `ThreadSummary`, for main timeline:
    // - at start
    // - upon update of the read receipts events

    let server = MatrixMockServer::new().await;
    let client = client_with_threading_support(&server).await;
    client.event_cache().subscribe().unwrap();

    let own_user = client.user_id().unwrap();

    // Sync some initial read receipts, one for the main timeline (that won't be
    // used) and another one for a threaded timeline, that will be used later.
    let room_id = room_id!("!a:b.c");
    let f = EventFactory::new().room(room_id).sender(&ALICE);

    let thread_event_id = event_id!("$thread_root");
    let latest_event_id = event_id!("$latest_event");

    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_receipt(
                    // Add a receipt for the latest event of the thread.
                    f.read_receipts()
                        .add(
                            latest_event_id,
                            own_user,
                            ReceiptType::Read,
                            ReceiptThread::Thread(thread_event_id.to_owned()),
                        )
                        .into_event(),
                )
                .add_timeline_event(
                    // And the thread root itself.
                    f.text_msg("thready thread mcthreadface")
                        .with_bundled_thread_summary(
                            f.text_msg("the last one!").event_id(latest_event_id).into(),
                            42,
                            false,
                        )
                        .event_id(thread_event_id),
                ),
        )
        .await;

    let timeline = room.timeline().await.unwrap();
    let (mut initial_items, mut stream) = timeline.subscribe().await;

    // Wait for the initial items.
    if initial_items.is_empty() {
        assert_let_timeout!(Some(timeline_updates) = stream.next());
        for up in timeline_updates {
            up.apply(&mut initial_items);
        }
    }

    // Let's take a look at the items, now that they've been loaded: there will be
    // the day divider and the thread root itself.
    let items = timeline.items().await;
    assert_eq!(items.len(), 2);

    // First, the day divider.
    let value = &items[0];
    assert!(value.is_date_divider());

    // Second, the event with the thread summary that has some read receipts.
    let value = &items[1];
    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), thread_event_id);
    assert_let!(Some(summary) = event_item.content().thread_summary());

    // We get the latest event from the bundled thread summary, and it's loaded.
    assert!(summary.latest_event.is_ready());

    // The public read receipt event id is filled (but the private isn't).
    assert_eq!(summary.public_read_receipt_event_id.as_deref(), Some(latest_event_id));
    assert_eq!(summary.private_read_receipt_event_id, None);

    // Now, a new read receipt is received for the same thread (we haven't received
    // the thread event yet).
    let new_latest_event_id = event_id!("$new_latest_event_id");

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_receipt(
                // Add a receipt for the latest event of the thread.
                f.read_receipts()
                    .add(
                        new_latest_event_id,
                        own_user,
                        ReceiptType::ReadPrivate,
                        ReceiptThread::Thread(thread_event_id.to_owned()),
                    )
                    .into_event(),
            ),
        )
        .await;

    assert_let_timeout!(Some(timeline_updates) = stream.next());
    assert_eq!(timeline_updates.len(), 1);
    assert_let!(VectorDiff::Set { index: 1, value } = &timeline_updates[0]);

    let event_item = value.as_event().unwrap();
    assert_eq!(event_item.event_id().unwrap(), thread_event_id);
    assert_let!(Some(summary) = event_item.content().thread_summary());

    // Now, the private read receipt is *also* filled.
    assert_eq!(summary.public_read_receipt_event_id.as_deref(), Some(latest_event_id));
    assert_eq!(summary.private_read_receipt_event_id.as_deref(), Some(new_latest_event_id));
}
