// Copyright 2022 The Matrix.org Foundation C.I.C.
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

//! Unit tests (based on private methods) for the timeline API.

use std::sync::{
    atomic::{AtomicU32, Ordering::SeqCst},
    Arc,
};

use assert_matches::assert_matches;
use futures_core::Stream;
use futures_signals::signal_vec::{SignalVecExt, VecDiff};
use futures_util::StreamExt;
use matrix_sdk_test::async_test;
use once_cell::sync::Lazy;
use ruma::{
    assign,
    events::{
        reaction::{self, ReactionEventContent},
        room::{
            encrypted::{
                EncryptedEventScheme, MegolmV1AesSha2ContentInit, RoomEncryptedEventContent,
            },
            message::{self, MessageType, Replacement, RoomMessageEventContent},
            redaction::OriginalSyncRoomRedactionEvent,
        },
        MessageLikeEventContent, OriginalSyncMessageLikeEvent,
    },
    serde::Raw,
    server_name, user_id, EventId, MilliSecondsSinceUnixEpoch, OwnedUserId, UserId,
};
use serde_json::{json, Value as JsonValue};

use super::{EncryptedMessage, TimelineInner, TimelineItem, TimelineItemContent};

static ALICE: Lazy<&UserId> = Lazy::new(|| user_id!("@alice:server.name"));
static BOB: Lazy<&UserId> = Lazy::new(|| user_id!("@bob:other.server"));

#[async_test]
async fn reaction_redaction() {
    let timeline = TestTimeline::new(&ALICE);
    let mut stream = timeline.stream();

    timeline.handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("hi!"));
    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let event = item.as_event().unwrap();
    assert_eq!(event.reactions().len(), 0);

    let msg_event_id = event.event_id().unwrap();

    let rel = reaction::Relation::new(msg_event_id.to_owned(), "+1".to_owned());
    timeline.handle_live_message_event(&BOB, ReactionEventContent::new(rel));
    let item =
        assert_matches!(stream.next().await, Some(VecDiff::UpdateAt { index: 0, value }) => value);
    let event = item.as_event().unwrap();
    assert_eq!(event.reactions().len(), 1);

    // TODO: After adding raw timeline items, check for one here

    let reaction_event_id = event.event_id().unwrap();

    timeline.handle_live_redaction(&BOB, reaction_event_id);
    let item =
        assert_matches!(stream.next().await, Some(VecDiff::UpdateAt { index: 0, value }) => value);
    let event = item.as_event().unwrap();
    assert_eq!(event.reactions().len(), 0);
}

#[async_test]
async fn invalid_edit() {
    let timeline = TestTimeline::new(&ALICE);
    let mut stream = timeline.stream();

    timeline.handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("test"));
    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let event = item.as_event().unwrap();
    let msg = event.content.as_message().unwrap();
    assert_eq!(msg.body(), "test");

    let msg_event_id = event.event_id().unwrap();

    let edit = assign!(RoomMessageEventContent::text_plain(" * fake"), {
        relates_to: Some(message::Relation::Replacement(Replacement::new(
            msg_event_id.to_owned(),
            MessageType::text_plain("fake"),
        ))),
    });
    // Edit is from a different user than the previous event
    timeline.handle_live_message_event(&BOB, edit);

    // Can't easily test the non-arrival of an item using the stream. Instead
    // just assert that there is still just a single item in the timeline.
    assert_eq!(timeline.inner.items.lock_ref().len(), 1);
}

#[async_test]
async fn edit_redacted() {
    let timeline = TestTimeline::new(&ALICE);
    let mut stream = timeline.stream();

    // Ruma currently fails to serialize most redacted events correctly
    timeline.handle_live_custom_event(json!({
        "content": {},
        "event_id": "$eeG0HA0FAZ37wP8kXlNkxx3I",
        "origin_server_ts": 10,
        "sender": "@alice:example.org",
        "type": "m.room.message",
        "unsigned": {
            "redacted_because": {
                "content": {},
                "redacts": "$eeG0HA0FAZ37wP8kXlNkxx3K",
                "event_id": "$N6eUCBc3vu58PL8TobGaVQzM",
                "sender": "@alice:example.org",
                "origin_server_ts": 5,
                "type": "m.room.redaction",
            },
        },
    }));
    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);

    let redacted_event_id = item.as_event().unwrap().event_id().unwrap();

    let edit = assign!(RoomMessageEventContent::text_plain(" * test"), {
        relates_to: Some(message::Relation::Replacement(Replacement::new(
            redacted_event_id.to_owned(),
            MessageType::text_plain("test"),
        ))),
    });
    timeline.handle_live_message_event(&ALICE, edit);

    assert_eq!(timeline.inner.items.lock_ref().len(), 1);
}

#[async_test]
async fn unable_to_decrypt() {
    let timeline = TestTimeline::new(&ALICE);
    timeline.handle_live_message_event(
        &BOB,
        RoomEncryptedEventContent::new(
            EncryptedEventScheme::MegolmV1AesSha2(
                MegolmV1AesSha2ContentInit {
                    ciphertext: "This can't be decrypted".to_owned(),
                    sender_key: "whatever".to_owned(),
                    device_id: "MyDevice".into(),
                    session_id: "MySession".into(),
                }
                .into(),
            ),
            None,
        ),
    );
    let timeline_items = timeline.inner.items.lock_ref();
    assert_eq!(timeline_items.len(), 1);
    let event = timeline_items[0].as_event().unwrap();
    let session_id = assert_matches!(
        event.content(),
        TimelineItemContent::UnableToDecrypt(
            EncryptedMessage::MegolmV1AesSha2 { session_id, .. },
        ) => session_id
    );
    assert_eq!(session_id, "MySession");
}

struct TestTimeline {
    own_user_id: OwnedUserId,
    inner: TimelineInner,
}

impl TestTimeline {
    fn new(own_user_id: &UserId) -> Self {
        Self { own_user_id: own_user_id.to_owned(), inner: Default::default() }
    }

    fn stream(&self) -> impl Stream<Item = VecDiff<Arc<TimelineItem>>> {
        self.inner.items.signal_vec_cloned().to_stream()
    }

    fn handle_live_message_event<C>(&self, sender: &UserId, content: C)
    where
        C: MessageLikeEventContent,
    {
        let ev = OriginalSyncMessageLikeEvent {
            content,
            event_id: EventId::new(server_name!("dummy.server")),
            sender: sender.to_owned(),
            origin_server_ts: next_server_ts(),
            unsigned: Default::default(),
        };
        let raw = Raw::new(&ev).unwrap().cast();
        self.inner.handle_live_event(raw, None, &self.own_user_id);
    }

    fn handle_live_custom_event(&self, event: JsonValue) {
        let raw = Raw::new(&event).unwrap().cast();
        self.inner.handle_live_event(raw, None, &self.own_user_id);
    }

    fn handle_live_redaction(&self, sender: &UserId, redacts: &EventId) {
        let ev = OriginalSyncRoomRedactionEvent {
            content: Default::default(),
            redacts: redacts.to_owned(),
            event_id: EventId::new(server_name!("dummy.server")),
            sender: sender.to_owned(),
            origin_server_ts: next_server_ts(),
            unsigned: Default::default(),
        };
        let raw = Raw::new(&ev).unwrap().cast();
        self.inner.handle_live_event(raw, None, &self.own_user_id);
    }
}

fn next_server_ts() -> MilliSecondsSinceUnixEpoch {
    static NEXT_TS: AtomicU32 = AtomicU32::new(0);
    MilliSecondsSinceUnixEpoch(NEXT_TS.fetch_add(1, SeqCst).into())
}
