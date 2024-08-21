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

use std::time::Duration;

use assert_matches::assert_matches;
use assert_matches2::assert_let;
use eyeball_im::VectorDiff;
use futures_util::StreamExt;
use matrix_sdk::{
    config::SyncSettings,
    test_utils::{events::EventFactory, logged_in_client_with_server},
};
use matrix_sdk_test::{
    async_test,
    mocks::{mock_encryption_state, mock_redaction},
    sync_timeline_event, JoinedRoomBuilder, RoomAccountDataTestEvent, StateTestEvent,
    SyncResponseBuilder,
};
use matrix_sdk_ui::timeline::{EventSendState, RoomExt, TimelineItemContent, VirtualTimelineItem};
use ruma::{
    event_id, events::room::message::RoomMessageEventContent, room_id, user_id,
    MilliSecondsSinceUnixEpoch,
};
use serde_json::json;
use wiremock::{
    matchers::{header, method, path_regex},
    Mock, ResponseTemplate,
};

use crate::mock_sync;

mod echo;
mod edit;
mod focus_event;
mod pagination;
mod pinned_event;
mod profiles;
mod queue;
mod reactions;
mod read_receipts;
mod replies;
mod subscribe;

pub(crate) mod sliding_sync;

#[async_test]
async fn test_reaction() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "body": "hello",
                    "msgtype": "m.text",
                },
                "event_id": "$TTvQUp1e17qkw41rBSjpZ",
                "origin_server_ts": 152037280,
                "sender": "@alice:example.org",
                "type": "m.room.message",
            }))
            .add_timeline_event(sync_timeline_event!({
                "content": {
                    "m.relates_to": {
                        "event_id": "$TTvQUp1e17qkw41rBSjpZ",
                        "key": "👍",
                        "rel_type": "m.annotation",
                    },
                },
                "event_id": "$031IXQRi27504",
                "origin_server_ts": 152038300,
                "sender": "@bob:example.org",
                "type": "m.reaction",
            })),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // The new message starts with their author's read receipt.
    assert_let!(Some(VectorDiff::PushBack { value: message }) = timeline_stream.next().await);
    let event_item = message.as_event().unwrap();
    assert_matches!(event_item.content(), TimelineItemContent::Message(_));
    assert_eq!(event_item.read_receipts().len(), 1);

    // The new message is getting the reaction, which implies an implicit read
    // receipt that's obtained first.
    assert_let!(
        Some(VectorDiff::Set { index: 0, value: updated_message }) = timeline_stream.next().await
    );
    let event_item = updated_message.as_event().unwrap();
    assert_let!(TimelineItemContent::Message(msg) = event_item.content());
    assert!(!msg.is_edited());
    assert_eq!(event_item.read_receipts().len(), 2);
    assert_eq!(event_item.reactions().len(), 0);

    // Then the reaction is taken into account.
    assert_let!(
        Some(VectorDiff::Set { index: 0, value: updated_message }) = timeline_stream.next().await
    );
    let event_item = updated_message.as_event().unwrap();
    assert_let!(TimelineItemContent::Message(msg) = event_item.content());
    assert!(!msg.is_edited());
    assert_eq!(event_item.read_receipts().len(), 2);
    assert_eq!(event_item.reactions().len(), 1);
    let group = &event_item.reactions()["👍"];
    assert_eq!(group.len(), 1);
    let senders: Vec<_> = group.keys().collect();
    assert_eq!(senders.as_slice(), [user_id!("@bob:example.org")]);

    // The day divider.
    assert_let!(Some(VectorDiff::PushFront { value: day_divider }) = timeline_stream.next().await);
    assert!(day_divider.is_day_divider());

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        sync_timeline_event!({
            "content": {},
            "redacts": "$031IXQRi27504",
            "event_id": "$N6eUCBc3vu58PL8TobGaVQzM",
            "sender": "@bob:example.org",
            "origin_server_ts": 152037280,
            "type": "m.room.redaction",
        }),
    ));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(
        Some(VectorDiff::Set { index: 1, value: updated_message }) = timeline_stream.next().await
    );
    let event_item = updated_message.as_event().unwrap();
    assert_let!(TimelineItemContent::Message(msg) = event_item.content());
    assert!(!msg.is_edited());
    assert_eq!(event_item.reactions().len(), 0);
}

#[async_test]
async fn test_redacted_message() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(sync_timeline_event!({
                "content": {},
                "event_id": "$eeG0HA0FAZ37wP8kXlNkxx3I",
                "origin_server_ts": 152035910,
                "sender": "@alice:example.org",
                "type": "m.room.message",
                "unsigned": {
                    "redacted_because": {
                        "content": {},
                        "redacts": "$eeG0HA0FAZ37wP8kXlNkxx3I",
                        "event_id": "$N6eUCBc3vu58PL8TobGaVQzM",
                        "sender": "@alice:example.org",
                        "origin_server_ts": 152037280,
                        "type": "m.room.redaction",
                    },
                },
            }))
            .add_timeline_event(sync_timeline_event!({
                "content": {},
                "redacts": "$eeG0HA0FAZ37wP8kXlNkxx3I",
                "event_id": "$N6eUCBc3vu58PL8TobGaVQzM",
                "sender": "@alice:example.org",
                "origin_server_ts": 152037280,
                "type": "m.room.redaction",
            })),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::PushBack { value: first }) = timeline_stream.next().await);
    assert_matches!(first.as_event().unwrap().content(), TimelineItemContent::RedactedMessage);

    assert_let!(Some(VectorDiff::PushFront { value: day_divider }) = timeline_stream.next().await);
    assert!(day_divider.is_day_divider());
}

#[async_test]
async fn test_redact_message() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    let factory = EventFactory::new();
    factory.set_next_ts(MilliSecondsSinceUnixEpoch::now().get().into());

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id).add_timeline_event(
            factory.sender(user_id!("@a:b.com")).text_msg("buy my bitcoins bro"),
        ),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::PushBack { value: first }) = timeline_stream.next().await);
    assert_eq!(
        first.as_event().unwrap().content().as_message().unwrap().body(),
        "buy my bitcoins bro"
    );

    assert_let!(Some(VectorDiff::PushFront { value: day_divider }) = timeline_stream.next().await);
    assert!(day_divider.is_day_divider());

    // Redacting a remote event works.
    mock_redaction(event_id!("$42")).mount(&server).await;

    let event_id = first.as_event().unwrap();

    let did_redact = timeline.redact(event_id, Some("inapprops")).await.unwrap();
    assert!(did_redact);

    // Redacting a local event works.
    timeline
        .send(RoomMessageEventContent::text_plain("i will disappear soon").into())
        .await
        .unwrap();

    assert_let!(Some(VectorDiff::PushBack { value: second }) = timeline_stream.next().await);

    let second = second.as_event().unwrap();
    assert_matches!(second.send_state(), Some(EventSendState::NotSentYet));

    // We haven't set a route for sending events, so this will fail.
    assert_let!(Some(VectorDiff::Set { index, value: second }) = timeline_stream.next().await);
    assert_eq!(index, 2);

    let second = second.as_event().unwrap();
    assert!(second.is_local_echo());
    assert_matches!(second.send_state(), Some(EventSendState::SendingFailed { .. }));

    // Let's redact the local echo.
    let did_redact = timeline.redact(second, None).await.unwrap();
    assert!(did_redact);

    // Observe local echo being removed.
    assert_matches!(timeline_stream.next().await, Some(VectorDiff::Remove { index: 2 }));
}

#[async_test]
async fn test_read_marker() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        sync_timeline_event!({
            "content": {
                "body": "hello",
                "msgtype": "m.text",
            },
            "event_id": "$someplace:example.org",
            "origin_server_ts": 152037280,
            "sender": "@alice:example.org",
            "type": "m.room.message",
        }),
    ));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::PushBack { value: message }) = timeline_stream.next().await);
    assert_matches!(message.as_event().unwrap().content(), TimelineItemContent::Message(_));

    assert_let!(Some(VectorDiff::PushFront { value: day_divider }) = timeline_stream.next().await);
    assert!(day_divider.is_day_divider());

    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id).add_account_data(RoomAccountDataTestEvent::FullyRead),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // Nothing should happen, the marker cannot be added at the end.

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        sync_timeline_event!({
            "content": {
                "body": "hello to you!",
                "msgtype": "m.text",
            },
            "event_id": "$someotherplace:example.org",
            "origin_server_ts": 152067280,
            "sender": "@bob:example.org",
            "type": "m.room.message",
        }),
    ));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::PushBack { value: message }) = timeline_stream.next().await);
    assert_matches!(message.as_event().unwrap().content(), TimelineItemContent::Message(_));

    assert_let!(
        Some(VectorDiff::Insert { index: 2, value: marker }) = timeline_stream.next().await
    );
    assert_matches!(marker.as_virtual().unwrap(), VirtualTimelineItem::ReadMarker);
}

#[async_test]
async fn test_sync_highlighted() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder
        // We need the member event and power levels locally so the push rules processor works.
        .add_joined_room(
            JoinedRoomBuilder::new(room_id)
                .add_state_event(StateTestEvent::Member)
                .add_state_event(StateTestEvent::PowerLevels),
        );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();
    let (_, mut timeline_stream) = timeline.subscribe().await;

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        sync_timeline_event!({
            "content": {
                "body": "hello",
                "msgtype": "m.text",
            },
            "event_id": "$msda7m0df9E9op3",
            "origin_server_ts": 152037280,
            "sender": "@example:localhost",
            "type": "m.room.message",
        }),
    ));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::PushBack { value: first }) = timeline_stream.next().await);
    let remote_event = first.as_event().unwrap();
    // Own events don't trigger push rules.
    assert!(!remote_event.is_highlighted());

    assert_let!(Some(VectorDiff::PushFront { value: day_divider }) = timeline_stream.next().await);
    assert!(day_divider.is_day_divider());

    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id).add_timeline_event(
        sync_timeline_event!({
            "content": {
                "body": "This room has been replaced",
                "replacement_room": "!newroom:localhost",
            },
            "event_id": "$foun39djjod0f",
            "origin_server_ts": 152039280,
            "sender": "@bob:localhost",
            "state_key": "",
            "type": "m.room.tombstone",
        }),
    ));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    assert_let!(Some(VectorDiff::PushBack { value: second }) = timeline_stream.next().await);
    let remote_event = second.as_event().unwrap();
    // `m.room.tombstone` should be highlighted by default.
    assert!(remote_event.is_highlighted());
}

#[async_test]
async fn test_duplicate_maintains_correct_order() {
    let room_id = room_id!("!a98sd12bjh:example.org");
    let (client, server) = logged_in_client_with_server().await;
    let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

    let mut sync_builder = SyncResponseBuilder::new();
    sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    mock_encryption_state(&server, false).await;

    let room = client.get_room(room_id).unwrap();
    let timeline = room.timeline().await.unwrap();

    // At the beginning, the timeline is empty.
    assert!(timeline.items().await.is_empty());

    let f = EventFactory::new().sender(user_id!("@a:b.c"));

    // We receive an event F, from a sliding sync with timeline limit=1.
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(f.text_msg("C").event_id(event_id!("$c")).into_raw_sync()),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    // The timeline item represents the message we just received.
    let items = timeline.items().await;
    assert_eq!(items.len(), 2);

    assert!(items[0].is_day_divider());
    let content = items[1].as_event().unwrap().content().as_message().unwrap().body();
    assert_eq!(content, "C");

    // We receive multiple events, and C is now the last one (because we supposedly
    // increased the timeline limit).
    sync_builder.add_joined_room(
        JoinedRoomBuilder::new(room_id)
            .add_timeline_event(f.text_msg("A").event_id(event_id!("$a")).into_raw_sync())
            .add_timeline_event(f.text_msg("B").event_id(event_id!("$b")).into_raw_sync())
            .add_timeline_event(f.text_msg("C").event_id(event_id!("$c")).into_raw_sync()),
    );

    mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
    let _response = client.sync_once(sync_settings.clone()).await.unwrap();
    server.reset().await;

    let items = timeline.items().await;
    assert_eq!(items.len(), 4, "{items:?}");

    assert!(items[0].is_day_divider());
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
    setup
        .mock_response(ResponseTemplate::new(200).set_body_json(json!({
            "event_id": "$42"
        })))
        .await;

    let event_id = setup.event_id();
    assert!(timeline.pin_event(event_id).await.unwrap());

    setup.reset_server().await;
}

#[async_test]
async fn test_pin_event_is_returning_false_because_is_already_pinned() {
    let mut setup = PinningTestSetup::new().await;
    let timeline = setup.timeline().await;

    setup.mock_sync(true).await;
    assert!(!timeline.items().await.is_empty());

    let event_id = setup.event_id();
    assert!(!timeline.pin_event(event_id).await.unwrap());

    setup.reset_server().await;
}

#[async_test]
async fn test_pin_event_is_returning_an_error() {
    let mut setup = PinningTestSetup::new().await;
    let timeline = setup.timeline().await;

    setup.mock_sync(false).await;
    assert!(!timeline.items().await.is_empty());

    // Pinning a remote event fails.
    setup.mock_response(ResponseTemplate::new(400)).await;

    let event_id = setup.event_id();
    assert!(timeline.pin_event(event_id).await.is_err());

    setup.reset_server().await;
}

#[async_test]
async fn test_unpin_event_is_sent_successfully() {
    let mut setup = PinningTestSetup::new().await;
    let timeline = setup.timeline().await;

    setup.mock_sync(true).await;
    assert!(!timeline.items().await.is_empty());

    // Unpinning a remote event succeeds.
    setup
        .mock_response(ResponseTemplate::new(200).set_body_json(json!({
            "event_id": "$42"
        })))
        .await;

    let event_id = setup.event_id();
    assert!(timeline.unpin_event(event_id).await.unwrap());

    setup.reset_server().await;
}

#[async_test]
async fn test_unpin_event_is_returning_false_because_is_not_pinned() {
    let mut setup = PinningTestSetup::new().await;
    let timeline = setup.timeline().await;

    setup.mock_sync(false).await;
    assert!(!timeline.items().await.is_empty());

    let event_id = setup.event_id();
    assert!(!timeline.unpin_event(event_id).await.unwrap());

    setup.reset_server().await;
}

#[async_test]
async fn test_unpin_event_is_returning_an_error() {
    let mut setup = PinningTestSetup::new().await;
    let timeline = setup.timeline().await;

    setup.mock_sync(true).await;
    assert!(!timeline.items().await.is_empty());

    // Unpinning a remote event fails.
    setup.mock_response(ResponseTemplate::new(400)).await;

    let event_id = setup.event_id();
    assert!(timeline.unpin_event(event_id).await.is_err());

    setup.reset_server().await;
}

struct PinningTestSetup<'a> {
    event_id: &'a ruma::EventId,
    room_id: &'a ruma::RoomId,
    client: matrix_sdk::Client,
    server: wiremock::MockServer,
    sync_settings: SyncSettings,
    sync_builder: SyncResponseBuilder,
}

impl PinningTestSetup<'_> {
    async fn new() -> Self {
        let room_id = room_id!("!a98sd12bjh:example.org");
        let (client, server) = logged_in_client_with_server().await;
        let sync_settings = SyncSettings::new().timeout(Duration::from_millis(3000));

        let mut sync_builder = SyncResponseBuilder::new();
        let event_id = event_id!("$a");
        sync_builder.add_joined_room(JoinedRoomBuilder::new(room_id));

        mock_sync(&server, sync_builder.build_json_sync_response(), None).await;
        let _response = client.sync_once(sync_settings.clone()).await.unwrap();
        server.reset().await;

        Self { event_id, room_id, client, server, sync_settings, sync_builder }
    }

    async fn timeline(&self) -> matrix_sdk_ui::Timeline {
        mock_encryption_state(&self.server, false).await;
        let room = self.client.get_room(self.room_id).unwrap();
        room.timeline().await.unwrap()
    }

    async fn reset_server(&self) {
        self.server.reset().await;
    }

    async fn mock_response(&self, response: ResponseTemplate) {
        Mock::given(method("PUT"))
            .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/m.room.pinned_events/.*?"))
            .and(header("authorization", "Bearer 1234"))
            .respond_with(response)
            .mount(&self.server)
            .await;
    }

    async fn mock_sync(&mut self, is_using_pinned_state_event: bool) {
        let f = EventFactory::new().sender(user_id!("@a:b.c"));
        let mut joined_room_builder = JoinedRoomBuilder::new(self.room_id)
            .add_timeline_event(f.text_msg("A").event_id(self.event_id).into_raw_sync());
        if is_using_pinned_state_event {
            joined_room_builder =
                joined_room_builder.add_state_event(StateTestEvent::RoomPinnedEvents);
        }
        self.sync_builder.add_joined_room(joined_room_builder);
        mock_sync(&self.server, self.sync_builder.build_json_sync_response(), None).await;
        let _response = self.client.sync_once(self.sync_settings.clone()).await.unwrap();
    }

    fn event_id(&self) -> &ruma::EventId {
        self.event_id
    }
}
