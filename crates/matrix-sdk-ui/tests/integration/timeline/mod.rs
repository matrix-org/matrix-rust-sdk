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

use std::ops::Not;

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk::{
    linked_chunk::{ChunkIdentifier, LinkedChunkId, Position, Update},
    test_utils::mocks::{
        MatrixMockServer, RoomContextResponseTemplate, RoomRelationsResponseTemplate,
    },
};
use matrix_sdk_test::{
    ALICE, BOB, JoinedRoomBuilder, RoomAccountDataTestEvent, StateTestEvent, async_test,
    event_factory::EventFactory,
};
use matrix_sdk_ui::{
    Timeline,
    timeline::{
        AnyOtherFullStateEventContent, Error, EventSendState, MsgLikeKind, OtherMessageLike,
        RedactError, RoomExt, TimelineBuilder, TimelineEventFocusThreadMode, TimelineEventItemId,
        TimelineEventShieldState, TimelineFocus, TimelineItemContent, VirtualTimelineItem,
        default_event_filter,
    },
};
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, event_id,
    events::{
        AnyTimelineEvent, MessageLikeEventType, TimelineEventType,
        room::{
            encryption::RoomEncryptionEventContent,
            message::{RedactedRoomMessageEventContent, RoomMessageEventContent},
        },
    },
    owned_event_id, room_id,
    serde::Raw,
    user_id,
};
use serde_json::json;
use sliding_sync::assert_timeline_stream;
use stream_assert::assert_pending;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{header, method, path_regex},
};

mod decryption;
mod echo;
mod edit;
mod focus_event;
mod media;
mod pagination;
mod pinned_event;
mod profiles;
mod queue;
mod reactions;
mod read_receipts;
mod redecryption;
mod replies;
mod subscribe;
mod thread;

pub(crate) mod sliding_sync;

#[async_test]
async fn test_timeline_is_threaded() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    {
        // A live timeline isn't threaded.
        let timeline = room.timeline().await.unwrap();
        assert!(timeline.is_threaded().not());
    }

    {
        // A thread timeline is threaded.
        let timeline = TimelineBuilder::new(&room)
            .with_focus(TimelineFocus::Thread { root_event_id: owned_event_id!("$thread_root") })
            .build()
            .await
            .unwrap();
        assert!(timeline.is_threaded());
    }

    {
        // An event-focused timeline, focused on a non-thread event, isn't threaded when
        // no context is requested.
        let f = EventFactory::new();
        let event_id = event_id!("$target");
        let event =
            f.text_msg("hello world").event_id(event_id).room(room_id).sender(&ALICE).into_event();
        server.mock_room_event().match_event_id().ok(event).mock_once().mount().await;
        server
            .mock_room_relations()
            .match_target_event(event_id.to_owned())
            .ok(RoomRelationsResponseTemplate::default()
                .events(Vec::<Raw<AnyTimelineEvent>>::new()))
            .mock_once()
            .mount()
            .await;

        let timeline = TimelineBuilder::new(&room)
            .with_focus(TimelineFocus::Event {
                target: owned_event_id!("$target"),
                num_context_events: 0,
                thread_mode: TimelineEventFocusThreadMode::Automatic { hide_threaded_events: true },
            })
            .build()
            .await
            .unwrap();
        assert!(timeline.is_threaded().not());
    }

    {
        // But an event-focused timeline, focused on an in-thread event, is threaded
        // when no context is requested \o/
        let f = EventFactory::new();
        let thread_root = event_id!("$thread_root");
        let event_id = event_id!("$thetarget");
        let event = f
            .text_msg("hey to you too")
            .event_id(event_id)
            .in_thread(thread_root, thread_root)
            .room(room_id)
            .sender(&ALICE)
            .into_event();

        server.mock_room_event().match_event_id().ok(event).mock_once().mount().await;
        server
            .mock_room_relations()
            .match_target_event(event_id.to_owned())
            .ok(RoomRelationsResponseTemplate::default()
                .events(Vec::<Raw<AnyTimelineEvent>>::new()))
            .mock_once()
            .mount()
            .await;

        let timeline = TimelineBuilder::new(&room)
            .with_focus(TimelineFocus::Event {
                target: owned_event_id!("$thetarget"),
                num_context_events: 0,
                thread_mode: TimelineEventFocusThreadMode::Automatic { hide_threaded_events: true },
            })
            .build()
            .await
            .unwrap();
        assert!(timeline.is_threaded());
    }

    {
        // An event-focused timeline, focused on a thread root, is also threaded
        // when no context is requested \o/
        let f = EventFactory::new();
        let event_id = event_id!("$atarget");
        let event = f
            .text_msg("hey to you too")
            .event_id(event_id)
            .room(room_id)
            .sender(&ALICE)
            .into_event();

        server.mock_room_event().match_event_id().ok(event).mock_once().mount().await;
        server
            .mock_room_relations()
            .match_target_event(event_id.to_owned())
            .ok(RoomRelationsResponseTemplate::default()
                .events(Vec::<Raw<AnyTimelineEvent>>::new()))
            .mock_once()
            .mount()
            .await;

        let timeline = TimelineBuilder::new(&room)
            .with_focus(TimelineFocus::Event {
                target: owned_event_id!("$atarget"),
                num_context_events: 0,
                thread_mode: TimelineEventFocusThreadMode::ForceThread,
            })
            .build()
            .await
            .unwrap();
        assert!(timeline.is_threaded());
    }

    {
        // An event-focused timeline, focused on a non-thread event, isn't threaded.
        let f = EventFactory::new();
        let event = f
            .text_msg("hello world")
            .event_id(event_id!("$target"))
            .room(room_id)
            .sender(&ALICE)
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
        assert!(timeline.is_threaded().not());
    }

    {
        // But an event-focused timeline, focused on an in-thread event, is threaded \o/
        let f = EventFactory::new();
        let thread_root = event_id!("$thread_root");
        let event = f
            .text_msg("hey to you too")
            .event_id(event_id!("$target"))
            .in_thread(thread_root, thread_root)
            .room(room_id)
            .sender(&ALICE)
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
        assert!(timeline.is_threaded());
    }

    {
        // An event-focused timeline, focused on a thread root, is also threaded \o/
        let f = EventFactory::new();
        let event = f
            .text_msg("hey to you too")
            .event_id(event_id!("$target"))
            .room(room_id)
            .sender(&ALICE)
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
                thread_mode: TimelineEventFocusThreadMode::ForceThread,
            })
            .build()
            .await
            .unwrap();
        assert!(timeline.is_threaded());
    }

    {
        // A pinned events timeline isn't threaded.
        let timeline = TimelineBuilder::new(&room)
            .with_focus(TimelineFocus::PinnedEvents {
                max_events_to_load: 0,
                max_concurrent_requests: 10,
            })
            .build()
            .await
            .unwrap();
        assert!(timeline.is_threaded().not());
    }
}

#[async_test]
async fn test_reaction() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let msg_event_id = event_id!("$msg");
    let reaction_event_id = event_id!("$reaction");

    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("hello").sender(&ALICE).event_id(msg_event_id))
                .add_timeline_event(
                    f.reaction(msg_event_id, "👍").sender(&BOB).event_id(reaction_event_id),
                ),
        )
        .await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 4);

    // The new message starts with their author's read receipt.
    assert_let!(VectorDiff::PushBack { value: message } = &timeline_updates[0]);
    let event_item = message.as_event().unwrap();
    assert!(event_item.content().is_message());
    assert_eq!(event_item.read_receipts().len(), 1);

    // The new message is getting the reaction, which implies an implicit read
    // receipt that's obtained first.
    assert_let!(VectorDiff::Set { index: 0, value: updated_message } = &timeline_updates[1]);
    let event_item = updated_message.as_event().unwrap();
    assert_let!(Some(msg) = event_item.content().as_message());
    assert!(!msg.is_edited());
    assert_eq!(event_item.read_receipts().len(), 2);
    assert_eq!(event_item.content().reactions().cloned().unwrap_or_default().len(), 0);

    // Then the reaction is taken into account.
    assert_let!(VectorDiff::Set { index: 0, value: updated_message } = &timeline_updates[2]);
    let event_item = updated_message.as_event().unwrap();
    assert_let!(Some(msg) = event_item.content().as_message());
    assert!(!msg.is_edited());
    assert_eq!(event_item.read_receipts().len(), 2);
    let reactions = event_item.content().reactions().cloned().unwrap_or_default();
    assert_eq!(reactions.len(), 1);
    let group = &reactions["👍"];
    assert_eq!(group.len(), 1);
    let senders: Vec<_> = group.keys().collect();
    assert_eq!(senders.as_slice(), [*BOB]);

    // The date divider.
    assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[3]);
    assert!(date_divider.is_date_divider());

    assert_pending!(timeline_stream);

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.redaction(reaction_event_id).sender(&BOB)),
        )
        .await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 1);

    assert_let!(VectorDiff::Set { index: 1, value: updated_message } = &timeline_updates[0]);
    let event_item = updated_message.as_event().unwrap();
    assert_let!(Some(msg) = event_item.content().as_message());
    assert!(!msg.is_edited());
    assert_eq!(event_item.content().reactions().cloned().unwrap_or_default().len(), 0);

    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_redacted_message() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let event_id = event_id!("$eeG0HA0FAZ37wP8kXlNkxx3I");
    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(
                    f.redacted(*ALICE, RedactedRoomMessageEventContent::new()).event_id(event_id),
                )
                .add_timeline_event(f.redaction(event_id).sender(&ALICE)),
        )
        .await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 2);

    assert_let!(VectorDiff::PushBack { value: first } = &timeline_updates[0]);
    assert!(first.as_event().unwrap().content().is_redacted());

    assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[1]);
    assert!(date_divider.is_date_divider());

    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_redact_message() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let factory = EventFactory::new();
    factory.set_next_ts(MilliSecondsSinceUnixEpoch::now().get().into());

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                factory.sender(user_id!("@a:b.com")).text_msg("buy my bitcoins bro"),
            ),
        )
        .await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 2);

    assert_let!(VectorDiff::PushBack { value: first } = &timeline_updates[0]);
    assert_eq!(
        first.as_event().unwrap().content().as_message().unwrap().body(),
        "buy my bitcoins bro"
    );

    assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[1]);
    assert!(date_divider.is_date_divider());

    // Redacting a remote event works.
    server.mock_room_redact().ok(event_id!("$42")).mock_once().mount().await;

    timeline.redact(&first.as_event().unwrap().identifier(), Some("inapprops")).await.unwrap();

    // Redacting a local event works.
    timeline
        .send(RoomMessageEventContent::text_plain("i will disappear soon").into())
        .await
        .unwrap();

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 1);

    assert_let!(VectorDiff::PushBack { value: second } = &timeline_updates[0]);

    let second = second.as_event().unwrap();
    assert_matches!(second.send_state(), Some(EventSendState::NotSentYet { progress: None }));

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 1);

    // We haven't set a route for sending events, so this will fail.
    assert_let!(VectorDiff::Set { index, value: second } = &timeline_updates[0]);
    assert_eq!(*index, 2);

    let second = second.as_event().unwrap();
    assert!(second.is_local_echo());
    assert_matches!(second.send_state(), Some(EventSendState::SendingFailed { .. }));

    // Let's redact the local echo.
    timeline.redact(&second.identifier(), None).await.unwrap();

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 1);

    // Observe local echo being removed.
    assert_let!(VectorDiff::Remove { index: 2 } = &timeline_updates[0]);

    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_redact_local_sent_message() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    // Mock event sending.
    let event_id = event_id!("$ev0");
    server.mock_room_send().ok(event_id).mock_once().mount().await;

    // Send the event so it's added to the send queue as a local event.
    timeline
        .send(RoomMessageEventContent::text_plain("i will disappear soon").into())
        .await
        .unwrap();

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 2);

    // Assert the local event is in the timeline now and is not sent yet.
    assert_let!(VectorDiff::PushBack { value: item } = &timeline_updates[0]);
    let event = item.as_event().unwrap();
    assert!(event.is_local_echo());
    assert_matches!(event.send_state(), Some(EventSendState::NotSentYet { progress: None }));

    // As well as a date divider.
    assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[1]);
    assert!(date_divider.is_date_divider());

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 5);

    // We receive an update in the timeline from the send queue.
    assert_let!(VectorDiff::Set { index: 1, value: item } = &timeline_updates[0]);
    let event = item.as_event().unwrap();
    assert!(event.is_local_echo());
    assert_matches!(event.send_state(), Some(EventSendState::Sent { .. }));

    // And then it's inserted in the Event Cache, and considered remote.
    assert_matches!(&timeline_updates[1], VectorDiff::Remove { index: 1 });
    assert_let!(VectorDiff::PushFront { value: remote_event } = &timeline_updates[2]);
    assert_eq!(remote_event.as_event().unwrap().event_id(), Some(event_id));

    // The date divider is adjusted.
    assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[3]);
    assert!(date_divider.is_date_divider());

    assert_matches!(&timeline_updates[4], VectorDiff::Remove { index: 2 });

    assert_pending!(timeline_stream);

    // Mock the redaction response for the event we just sent. Ensure it's called
    // once.
    server.mock_room_redact().ok(event_id!("$redaction_event_id")).mock_once().mount().await;

    // Let's redact the local echo with the remote handle.
    timeline.redact(&event.identifier(), None).await.unwrap();
}

#[async_test]
async fn test_redact_nonexisting_item() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();

    let error = timeline
        .redact(&TimelineEventItemId::EventId(owned_event_id!("$123:example.com")), None)
        .await
        .err();
    assert_matches!(error, Some(Error::RedactError(RedactError::ItemNotFound(_))));

    let error =
        timeline.redact(&TimelineEventItemId::TransactionId("something".into()), None).await.err();
    assert_matches!(error, Some(Error::RedactError(RedactError::ItemNotFound(_))));
}

#[async_test]
async fn test_read_marker() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("hello").sender(&ALICE).event_id(event_id!("$someplace:example.org")),
            ),
        )
        .await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 2);

    assert_let!(VectorDiff::PushBack { value: message } = &timeline_updates[0]);
    assert!(message.as_event().unwrap().content().is_message());

    assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[1]);
    assert!(date_divider.is_date_divider());

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_account_data(RoomAccountDataTestEvent::FullyRead),
        )
        .await;

    // Nothing should happen, the marker cannot be added at the end.
    assert_pending!(timeline_stream);

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("hello to you!")
                    .sender(&BOB)
                    .event_id(event_id!("$someotherplace:example.org")),
            ),
        )
        .await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 2);

    assert_let!(VectorDiff::PushBack { value: message } = &timeline_updates[0]);
    assert!(message.as_event().unwrap().content().is_message());

    assert_let!(VectorDiff::Insert { index: 2, value: marker } = &timeline_updates[1]);
    assert_matches!(marker.as_virtual().unwrap(), VirtualTimelineItem::ReadMarker);

    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_sync_highlighted() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    server.mock_room_state_encryption().plain().mount().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server
        .sync_room(
            &client,
            // We need the member event and power levels locally so the push rules processor works.
            JoinedRoomBuilder::new(room_id)
                .add_state_event(StateTestEvent::Member)
                .add_state_event(StateTestEvent::PowerLevels),
        )
        .await;

    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.text_msg("hello").sender(&ALICE).event_id(event_id!("$msda7m0df9E9op3")),
            ),
        )
        .await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 2);

    assert_let!(VectorDiff::PushBack { value: first } = &timeline_updates[0]);
    let remote_event = first.as_event().unwrap();
    // Own events don't trigger push rules.
    assert!(!remote_event.is_highlighted());

    assert_let!(VectorDiff::PushFront { value: date_divider } = &timeline_updates[1]);
    assert!(date_divider.is_date_divider());

    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.room_tombstone("This room has been replaced", room_id!("!newroom:localhost"))
                    .sender(&BOB),
            ),
        )
        .await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_eq!(timeline_updates.len(), 1);

    assert_let!(VectorDiff::PushBack { value: second } = &timeline_updates[0]);
    let remote_event = second.as_event().unwrap();
    // `m.room.tombstone` should be highlighted by default.
    assert!(remote_event.is_highlighted());

    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_duplicate_maintains_correct_order() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room.timeline().await.unwrap();

    // At the beginning, the timeline is empty.
    assert!(timeline.items().await.is_empty());

    let f = EventFactory::new().sender(user_id!("@a:b.c"));

    // We receive an event F, from a sliding sync with timeline limit=1.
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("C").event_id(event_id!("$c")).into_raw_sync()),
        )
        .await;

    // The timeline item represents the message we just received.
    let items = timeline.items().await;
    assert_eq!(items.len(), 2);

    assert!(items[0].is_date_divider());
    let content = items[1].as_event().unwrap().content().as_message().unwrap().body();
    assert_eq!(content, "C");

    // We receive multiple events, and C is now the last one (because we supposedly
    // increased the timeline limit).
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("A").event_id(event_id!("$a")).into_raw_sync())
                .add_timeline_event(f.text_msg("B").event_id(event_id!("$b")).into_raw_sync())
                .add_timeline_event(f.text_msg("C").event_id(event_id!("$c")).into_raw_sync()),
        )
        .await;

    let items = timeline.items().await;
    assert_eq!(items.len(), 4, "{items:?}");

    assert!(items[0].is_date_divider());
    let content = items[1].as_event().unwrap().content().as_message().unwrap().body();
    assert_eq!(content, "A");
    let content = items[2].as_event().unwrap().content().as_message().unwrap().body();
    assert_eq!(content, "B");
    let content = items[3].as_event().unwrap().content().as_message().unwrap().body();
    assert_eq!(content, "C");
}

#[async_test]
async fn test_pin_event_is_sent_successfully() {
    let mut setup = PinningTestSetup::new().await;
    let timeline = setup.timeline().await;

    setup.mock_sync(false).await;
    assert!(!timeline.items().await.is_empty());

    // Pinning a remote event succeeds.
    setup.server.mock_set_room_pinned_events().ok(owned_event_id!("$42")).mock_once().mount().await;

    let event_id = setup.event_id();
    assert!(timeline.pin_event(event_id).await.unwrap());
}

#[async_test]
async fn test_pin_event_is_returning_false_because_is_already_pinned() {
    let mut setup = PinningTestSetup::new().await;
    let timeline = setup.timeline().await;

    setup.mock_sync(true).await;
    assert!(!timeline.items().await.is_empty());

    let event_id = setup.event_id();
    assert!(!timeline.pin_event(event_id).await.unwrap());
}

#[async_test]
async fn test_pin_event_is_returning_an_error() {
    let mut setup = PinningTestSetup::new().await;
    let timeline = setup.timeline().await;

    setup.mock_sync(false).await;
    assert!(!timeline.items().await.is_empty());

    // Pinning a remote event fails.
    setup.server.mock_set_room_pinned_events().unauthorized().mock_once().mount().await;

    let event_id = setup.event_id();
    assert!(timeline.pin_event(event_id).await.is_err());
}

#[async_test]
async fn test_unpin_event_is_sent_successfully() {
    let mut setup = PinningTestSetup::new().await;
    let timeline = setup.timeline().await;

    setup.mock_sync(true).await;
    assert!(!timeline.items().await.is_empty());

    // Unpinning a remote event succeeds.
    setup.server.mock_set_room_pinned_events().ok(owned_event_id!("$42")).mock_once().mount().await;

    let event_id = setup.event_id();
    assert!(timeline.unpin_event(event_id).await.unwrap());
}

#[async_test]
async fn test_unpin_event_is_returning_false_because_is_not_pinned() {
    let mut setup = PinningTestSetup::new().await;
    let timeline = setup.timeline().await;

    setup.mock_sync(false).await;
    assert!(!timeline.items().await.is_empty());

    let event_id = setup.event_id();
    assert!(!timeline.unpin_event(event_id).await.unwrap());
}

#[async_test]
async fn test_unpin_event_is_returning_an_error() {
    let mut setup = PinningTestSetup::new().await;
    let timeline = setup.timeline().await;

    setup.mock_sync(true).await;
    assert!(!timeline.items().await.is_empty());

    // Unpinning a remote event fails.
    setup.server.mock_set_room_pinned_events().unauthorized().mock_once().mount().await;

    let event_id = setup.event_id();
    assert!(timeline.unpin_event(event_id).await.is_err());
}

#[async_test]
async fn test_timeline_without_encryption_info() {
    // The room encryption state is NOT mocked on purpose.
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!a98sd12bjh:example.org");

    let f = EventFactory::new();
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.text_msg("A message").sender(*BOB)),
        )
        .await;

    // Previously this would have panicked.
    let timeline = room.timeline().await.unwrap();

    let (items, _) = timeline.subscribe().await;
    assert_eq!(items.len(), 2);
    assert!(items[0].as_virtual().is_some());
    // No encryption, no shields.
    assert_eq!(items[1].as_event().unwrap().get_shield(false), TimelineEventShieldState::None);
}

#[async_test]
async fn test_timeline_without_encryption_can_update() {
    // The room encryption state is NOT mocked on purpose.
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    client.event_cache().subscribe().unwrap();

    let room_id = room_id!("!jEsUZKDJdhlrceRyVU:example.org");

    let f = EventFactory::new().room(room_id).sender(*BOB);
    let room = server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(f.text_msg("A message")),
        )
        .await;

    // Previously this would have panicked.
    // We're creating a timeline without read receipts tracking to check only the
    // encryption changes.
    let timeline = TimelineBuilder::new(&room).build().await.unwrap();

    let (items, mut stream) = timeline.subscribe().await;
    assert_eq!(items.len(), 2);
    assert!(items[0].as_virtual().is_some());
    // No encryption, no shields
    assert_eq!(items[1].as_event().unwrap().get_shield(false), TimelineEventShieldState::None);

    let encryption_event_content = RoomEncryptionEventContent::with_recommended_defaults();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id)
                .add_timeline_event(f.event(encryption_event_content).state_key("").into_raw_sync())
                .add_timeline_event(f.text_msg("An encrypted message").into_raw_sync()),
        )
        .await;

    assert_let!(Some(timeline_updates) = stream.next().await);
    assert_eq!(timeline_updates.len(), 3);

    // Previous timeline event now has a shield.
    assert_let!(VectorDiff::Set { index, value } = &timeline_updates[0]);
    assert_eq!(*index, 1);
    assert_ne!(value.as_event().unwrap().get_shield(false), TimelineEventShieldState::None);

    // Room encryption event is received.
    assert_let!(VectorDiff::PushBack { value } = &timeline_updates[1]);
    assert_let!(TimelineItemContent::OtherState(other_state) = value.as_event().unwrap().content());
    assert_let!(AnyOtherFullStateEventContent::RoomEncryption(_) = other_state.content());
    assert_ne!(value.as_event().unwrap().get_shield(false), TimelineEventShieldState::None);

    // New message event is received and has a shield.
    assert_let!(VectorDiff::PushBack { value } = &timeline_updates[2]);
    assert_ne!(value.as_event().unwrap().get_shield(false), TimelineEventShieldState::None);

    assert_pending!(stream);
}

#[async_test]
async fn test_timeline_receives_a_limited_number_of_events_when_subscribing() {
    let room_id = room_id!("!foo:bar.baz");
    let event_factory = EventFactory::new().room(room_id).sender(&ALICE);

    let mock_server = MatrixMockServer::new().await;
    let client = mock_server.client_builder().build().await;

    mock_server.sync_joined_room(&client, room_id).await;

    // Let's store events in the event cache _before_ the timeline is created.
    {
        let event_cache_store = client.event_cache_store().lock().await.unwrap();

        // The event cache contains 30 events.
        event_cache_store
            .as_clean()
            .unwrap()
            .handle_linked_chunk_updates(
                LinkedChunkId::Room(room_id),
                vec![
                    Update::NewItemsChunk {
                        previous: None,
                        new: ChunkIdentifier::new(42),
                        next: None,
                    },
                    Update::PushItems {
                        at: Position::new(ChunkIdentifier::new(42), 0),
                        items: (0..30)
                            .map(|nth| {
                                event_factory
                                    .text_msg("foo")
                                    .event_id(&EventId::parse(format!("$ev{nth}")).unwrap())
                                    .into_event()
                            })
                            .collect::<Vec<_>>(),
                    },
                ],
            )
            .await
            .unwrap();
    }

    // Set up the event cache.
    let event_cache = client.event_cache();
    event_cache.subscribe().unwrap();

    let room = client.get_room(room_id).unwrap();

    // Now the timeline is created.
    let timeline = room.timeline().await.unwrap();

    let (timeline_initial_items, mut timeline_stream) = timeline.subscribe().await;

    // The timeline receives 20 initial values, not 30!
    assert_eq!(timeline_initial_items.len(), 20);
    assert_pending!(timeline_stream);

    // To get the other, the timeline needs to paginate.
    //
    // Now let's do a backwards pagination of 5 items.
    let hit_end_of_timeline = timeline.paginate_backwards(5).await.unwrap();

    assert!(hit_end_of_timeline.not());

    // Oh, 5 new items, without even hitting the network because the timeline
    // already has these!
    assert_timeline_stream! {
        [timeline_stream]
        prepend "$ev9";
        prepend "$ev8";
        prepend "$ev7";
        prepend "$ev6";
        prepend "$ev5";
    };
    assert_pending!(timeline_stream);
}

#[async_test]
async fn test_custom_msglike_event_in_timeline() {
    let server = MatrixMockServer::new().await;
    let client = server.client_builder().build().await;

    let room_id = room_id!("!a98sd12bjh:example.org");
    let room = server.sync_joined_room(&client, room_id).await;

    server.mock_room_state_encryption().plain().mount().await;

    let timeline = room
        .timeline_builder()
        .event_filter(|event, room_version| {
            event.event_type() == TimelineEventType::from("rs.matrix-sdk.custom.test")
                || default_event_filter(event, room_version)
        })
        .build()
        .await
        .unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let event_id = event_id!("$eeG0HA0FAZ37wP8kXlNk123I");
    let f = EventFactory::new();
    server
        .sync_room(
            &client,
            JoinedRoomBuilder::new(room_id).add_timeline_event(
                f.custom_message_like_event().event_id(event_id).sender(user_id!("@a:b.c")),
            ),
        )
        .await;

    assert_let!(Some(timeline_updates) = timeline_stream.next().await);
    assert_let!(VectorDiff::PushBack { value: first } = &timeline_updates[0]);
    let event_type = MessageLikeEventType::from("rs.matrix-sdk.custom.test");
    let other_msglike = OtherMessageLike::from_event_type(event_type);
    assert_matches!(
        first.as_event().unwrap().content().as_msglike().unwrap().kind.clone(),
        MsgLikeKind::Other(observed_other) => {
           assert_eq!(observed_other, other_msglike);
       }
    );
}

struct PinningTestSetup<'a> {
    event_id: &'a EventId,
    room_id: &'a ruma::RoomId,
    client: matrix_sdk::Client,
    server: MatrixMockServer,
}

impl PinningTestSetup<'_> {
    async fn new() -> Self {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let room_id = room_id!("!a98sd12bjh:example.org");
        server.sync_joined_room(&client, room_id).await;

        server.mock_room_state_encryption().plain().mount().await;

        // This is necessary to get an empty list of pinned events when there are no
        // pinned events state event in the required state
        Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/m.room.pinned_events/.*"))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({})))
            .mount(server.server())
            .await;

        let event_id = event_id!("$a");
        Self { event_id, room_id, client, server }
    }

    async fn timeline(&self) -> Timeline {
        let room = self.client.get_room(self.room_id).unwrap();
        room.timeline().await.unwrap()
    }

    async fn mock_sync(&mut self, is_using_pinned_state_event: bool) {
        let f = EventFactory::new().sender(user_id!("@a:b.c"));

        let mut joined_room_builder = JoinedRoomBuilder::new(self.room_id)
            .add_timeline_event(f.text_msg("A").event_id(self.event_id).into_raw_sync());

        if is_using_pinned_state_event {
            joined_room_builder =
                joined_room_builder.add_state_event(StateTestEvent::RoomPinnedEvents);
        }

        self.server.sync_room(&self.client, joined_room_builder).await;
    }

    fn event_id(&self) -> &EventId {
        self.event_id
    }
}
