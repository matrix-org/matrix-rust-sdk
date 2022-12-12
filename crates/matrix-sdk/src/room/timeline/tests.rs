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
use matrix_sdk_base::crypto::OlmMachine;
use matrix_sdk_test::async_test;
use once_cell::sync::Lazy;
use ruma::{
    assign, event_id,
    events::{
        reaction::ReactionEventContent,
        relation::{Annotation, Replacement},
        room::{
            encrypted::{
                EncryptedEventScheme, MegolmV1AesSha2ContentInit, RoomEncryptedEventContent,
            },
            message::{self, MessageType, RoomMessageEventContent},
            redaction::OriginalSyncRoomRedactionEvent,
        },
        AnyMessageLikeEventContent, MessageLikeEventContent, MessageLikeEventType,
        OriginalSyncMessageLikeEvent, StateEventType,
    },
    room_id,
    serde::Raw,
    server_name, uint, user_id, EventId, MilliSecondsSinceUnixEpoch, OwnedTransactionId,
    OwnedUserId, TransactionId, UserId,
};
use serde_json::{json, Value as JsonValue};

use super::{
    EncryptedMessage, TimelineInner, TimelineItem, TimelineItemContent, TimelineKey,
    VirtualTimelineItem,
};

static ALICE: Lazy<&UserId> = Lazy::new(|| user_id!("@alice:server.name"));
static BOB: Lazy<&UserId> = Lazy::new(|| user_id!("@bob:other.server"));

#[async_test]
async fn reaction_redaction() {
    let timeline = TestTimeline::new(&ALICE);
    let mut stream = timeline.stream();

    timeline.handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("hi!")).await;
    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let event = item.as_event().unwrap();
    assert_eq!(event.reactions().len(), 0);

    let msg_event_id = event.event_id().unwrap();

    let rel = Annotation::new(msg_event_id.to_owned(), "+1".to_owned());
    timeline.handle_live_message_event(&BOB, ReactionEventContent::new(rel)).await;
    let item =
        assert_matches!(stream.next().await, Some(VecDiff::UpdateAt { index: 0, value }) => value);
    let event = item.as_event().unwrap();
    assert_eq!(event.reactions().len(), 1);

    // TODO: After adding raw timeline items, check for one here

    let reaction_event_id = event.event_id().unwrap();

    timeline.handle_live_redaction(&BOB, reaction_event_id).await;
    let item =
        assert_matches!(stream.next().await, Some(VecDiff::UpdateAt { index: 0, value }) => value);
    let event = item.as_event().unwrap();
    assert_eq!(event.reactions().len(), 0);
}

#[async_test]
async fn invalid_edit() {
    let timeline = TestTimeline::new(&ALICE);
    let mut stream = timeline.stream();

    timeline.handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("test")).await;
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
    timeline.handle_live_message_event(&BOB, edit).await;

    // Can't easily test the non-arrival of an item using the stream. Instead
    // just assert that there is still just a single item in the timeline.
    assert_eq!(timeline.inner.items.lock_ref().len(), 1);
}

#[async_test]
async fn edit_redacted() {
    let timeline = TestTimeline::new(&ALICE);
    let mut stream = timeline.stream();

    // Ruma currently fails to serialize most redacted events correctly
    timeline
        .handle_live_custom_event(json!({
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
        }))
        .await;
    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);

    let redacted_event_id = item.as_event().unwrap().event_id().unwrap();

    let edit = assign!(RoomMessageEventContent::text_plain(" * test"), {
        relates_to: Some(message::Relation::Replacement(Replacement::new(
            redacted_event_id.to_owned(),
            MessageType::text_plain("test"),
        ))),
    });
    timeline.handle_live_message_event(&ALICE, edit).await;

    assert_eq!(timeline.inner.items.lock_ref().len(), 1);
}

#[cfg(not(target_arch = "wasm32"))]
#[async_test]
async fn unable_to_decrypt() {
    use std::{io::Cursor, iter};

    use matrix_sdk_base::crypto::decrypt_room_key_export;

    const SESSION_ID: &str = "gM8i47Xhu0q52xLfgUXzanCMpLinoyVyH7R58cBuVBU";
    const SESSION_KEY: &[u8] = b"\
        -----BEGIN MEGOLM SESSION DATA-----\n\
        ASKcWoiAVUM97482UAi83Avce62hSLce7i5JhsqoF6xeAAAACqt2Cg3nyJPRWTTMXxXH7TXnkfdlmBXbQtq5\
        bpHo3LRijcq2Gc6TXilESCmJN14pIsfKRJrWjZ0squ/XsoTFytuVLWwkNaW3QF6obeg2IoVtJXLMPdw3b2vO\
        vgwGY3OMP0XafH13j1vcb6YLzvgLkZQLnYvd47hv3yK/9GmKS9tokuaQ7dCVYckYcIOS09EDTs70YdxUd5WG\
        rQynATCLFP1p/NAGv70r9MK7Cy/mNpjD0r4qC7UEDIoi1kOWzHgnLo19wtvwsb8Fg8ATxcs3Wmtj8hIUYpDx\
        ia4sM10zbytUuaPUAfCDf42IyxdmOnGe1CueXhgI71y+RW0s0argNqUt7jB70JT0o9CyX6UBGRaqLk2MPY9T\
        hUu5J8X3UgIa6rcbWigzohzWm9rdbEHFrSWqjpfQYMaAKQQgETrjSy4XTrp2RhC2oNqG/hylI4ab+F4X6fpH\
        DYP1NqNMP5g36xNu7LhDnrUB5qsPjYOmWORxGLfudpF3oLYCSlr3DgHqEIB6HjQblLZ3KQuPBse3zxyROTnS\
        AhdPH4a/z1wioFtKNVph3hecsiKEdqnz4Y2coSIdhz58mJ9JWNQoFAENE5CSsoEZAGvafYZVpW4C75YY2zq1\
        wIeiFi1dT43/jLAUGkslsi1VvnyfUu8qO404RxYO3XHoGLMFoFLOO+lZ+VGci2Vz10AhxJhEBHxRKxw4k2uB\
        HztoSJUr/2Y\n\
        -----END MEGOLM SESSION DATA-----";

    let timeline = TestTimeline::new(&ALICE);
    let mut stream = timeline.stream();

    timeline
        .handle_live_message_event(
            &BOB,
            RoomEncryptedEventContent::new(
                EncryptedEventScheme::MegolmV1AesSha2(
                    MegolmV1AesSha2ContentInit {
                        ciphertext: "\
                            AwgAEtABPRMavuZMDJrPo6pGQP4qVmpcuapuXtzKXJyi3YpEsjSWdzuRKIgJzD4P\
                            cSqJM1A8kzxecTQNJsC5q22+KSFEPxPnI4ltpm7GFowSoPSW9+bFdnlfUzEP1jPq\
                            YevHAsMJp2fRKkzQQbPordrUk1gNqEpGl4BYFeRqKl9GPdKFwy45huvQCLNNueql\
                            CFZVoYMuhxrfyMiJJAVNTofkr2um2mKjDTlajHtr39pTG8k0eOjSXkLOSdZvNOMz\
                            hGhSaFNeERSA2G2YbeknOvU7MvjiO0AKuxaAe1CaVhAI14FCgzrJ8g0y5nly+n7x\
                            QzL2G2Dn8EoXM5Iqj8W99iokQoVsSrUEnaQ1WnSIfewvDDt4LCaD/w7PGETMCQ"
                            .to_owned(),
                        sender_key: "DeHIg4gwhClxzFYcmNntPNF9YtsdZbmMy8+3kzCMXHA".to_owned(),
                        device_id: "NLAZCWIOCO".into(),
                        session_id: SESSION_ID.into(),
                    }
                    .into(),
                ),
                None,
            ),
        )
        .await;

    assert_eq!(timeline.inner.items.lock_ref().len(), 1);

    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let event = item.as_event().unwrap();
    let session_id = assert_matches!(
        event.content(),
        TimelineItemContent::UnableToDecrypt(
            EncryptedMessage::MegolmV1AesSha2 { session_id, .. },
        ) => session_id
    );
    assert_eq!(session_id, SESSION_ID);

    let own_user_id = user_id!("@example:morheus.localhost");
    let exported_keys = decrypt_room_key_export(Cursor::new(SESSION_KEY), "1234").unwrap();

    let olm_machine = OlmMachine::new(own_user_id, "SomeDeviceId".into()).await;
    olm_machine.import_room_keys(exported_keys, false, |_, _| {}).await.unwrap();

    timeline
        .inner
        .retry_event_decryption(
            room_id!("!DovneieKSTkdHKpIXy:morpheus.localhost"),
            &olm_machine,
            iter::once(SESSION_ID).collect(),
            own_user_id,
        )
        .await;

    assert_eq!(timeline.inner.items.lock_ref().len(), 1);

    let item =
        assert_matches!(stream.next().await, Some(VecDiff::UpdateAt { index: 0, value }) => value);
    let event = item.as_event().unwrap();
    assert_matches!(&event.encryption_info, Some(_));
    let text = assert_matches!(event.content(), TimelineItemContent::Message(msg) => msg.body());
    assert_eq!(text, "It's a secret to everybody");
}

#[async_test]
async fn update_read_marker() {
    let timeline = TestTimeline::new(&ALICE);
    let mut stream = timeline.stream();

    timeline.handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("A")).await;
    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let event_id = item.as_event().unwrap().event_id().unwrap().to_owned();

    timeline.inner.set_fully_read_event(event_id).await;
    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    assert_matches!(item.as_virtual(), Some(VirtualTimelineItem::ReadMarker));

    timeline.handle_live_message_event(&BOB, RoomMessageEventContent::text_plain("B")).await;
    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let event_id = item.as_event().unwrap().event_id().unwrap().to_owned();

    timeline.inner.set_fully_read_event(event_id.clone()).await;
    assert_matches!(stream.next().await, Some(VecDiff::Move { old_index: 1, new_index: 2 }));

    // Nothing should happen if the fully read event isn't found.
    timeline.inner.set_fully_read_event(event_id!("$fake_event_id").to_owned()).await;

    // Nothing should happen if the fully read event is set back to the same event
    // as before.
    timeline.inner.set_fully_read_event(event_id).await;

    timeline.handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("C")).await;
    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let event_id = item.as_event().unwrap().event_id().unwrap().to_owned();

    timeline.inner.set_fully_read_event(event_id).await;
    assert_matches!(stream.next().await, Some(VecDiff::Move { old_index: 2, new_index: 3 }));
}

#[async_test]
async fn invalid_event_content() {
    let timeline = TestTimeline::new(&ALICE);
    let mut stream = timeline.stream();

    // m.room.message events must have a msgtype and body in content, so this
    // event with an empty content object should fail to deserialize.
    timeline
        .handle_live_custom_event(json!({
            "content": {},
            "event_id": "$eeG0HA0FAZ37wP8kXlNkxx3I",
            "origin_server_ts": 10,
            "sender": "@alice:example.org",
            "type": "m.room.message",
        }))
        .await;

    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let event_item = item.as_event().unwrap();
    assert_eq!(event_item.sender(), "@alice:example.org");
    assert_eq!(
        *event_item.key(),
        TimelineKey::EventId(event_id!("$eeG0HA0FAZ37wP8kXlNkxx3I").to_owned())
    );
    assert_eq!(event_item.origin_server_ts(), Some(MilliSecondsSinceUnixEpoch(uint!(10))));
    let event_type = assert_matches!(
        event_item.content(),
        TimelineItemContent::FailedToParseMessageLike { event_type, .. } => event_type
    );
    assert_eq!(*event_type, MessageLikeEventType::RoomMessage);

    // Similar to above, the m.room.member state event must also not have an
    // empty content object.
    timeline
        .handle_live_custom_event(json!({
            "content": {},
            "event_id": "$d5G0HA0FAZ37wP8kXlNkxx3I",
            "origin_server_ts": 2179,
            "sender": "@alice:example.org",
            "type": "m.room.member",
            "state_key": "@alice:example.org",
        }))
        .await;

    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let event_item = item.as_event().unwrap();
    assert_eq!(event_item.sender(), "@alice:example.org");
    assert_eq!(
        *event_item.key(),
        TimelineKey::EventId(event_id!("$d5G0HA0FAZ37wP8kXlNkxx3I").to_owned())
    );
    assert_eq!(event_item.origin_server_ts(), Some(MilliSecondsSinceUnixEpoch(uint!(2179))));
    let (event_type, state_key) = assert_matches!(
        event_item.content(),
        TimelineItemContent::FailedToParseState {
            event_type,
            state_key,
            ..
        } => (event_type, state_key)
    );
    assert_eq!(*event_type, StateEventType::RoomMember);
    assert_eq!(*state_key, "@alice:example.org");
}

#[async_test]
async fn invalid_event() {
    let timeline = TestTimeline::new(&ALICE);

    // This event is missing the sender field which the homeserver must add to
    // all timeline events. Because the event is malformed, it will be ignored.
    timeline
        .handle_live_custom_event(json!({
            "content": {
                "body": "hello world",
                "msgtype": "m.text"
            },
            "event_id": "$eeG0HA0FAZ37wP8kXlNkxx3I",
            "origin_server_ts": 10,
            "type": "m.room.message",
        }))
        .await;
    assert_eq!(timeline.inner.items.lock_ref().len(), 0);
}

#[async_test]
async fn remote_echo_without_txn_id() {
    let timeline = TestTimeline::new(&ALICE);
    let mut stream = timeline.stream();

    // Given a local event…
    let txn_id = timeline
        .handle_local_event(AnyMessageLikeEventContent::RoomMessage(
            RoomMessageEventContent::text_plain("echo"),
        ))
        .await;

    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    assert_matches!(item.as_event().unwrap().key(), TimelineKey::TransactionId(_));

    // That has an event ID assigned already (from the response to sending it)…
    let event_id = event_id!("$W6mZSLWMmfuQQ9jhZWeTxFIM");
    timeline.inner.add_event_id(&txn_id, event_id.to_owned());

    let item =
        assert_matches!(stream.next().await, Some(VecDiff::UpdateAt { value, index: 0 }) => value);
    assert_matches!(item.as_event().unwrap().key(), TimelineKey::TransactionId(_));

    // When an event with the same ID comes in…
    timeline
        .handle_live_custom_event(json!({
            "content": {
                "body": "echo",
                "msgtype": "m.text",
            },
            "sender": &*ALICE,
            "event_id": event_id,
            "origin_server_ts": 5,
            "type": "m.room.message",
        }))
        .await;

    let item =
        assert_matches!(stream.next().await, Some(VecDiff::UpdateAt { value, index: 0 }) => value);
    assert_matches!(item.as_event().unwrap().key(), TimelineKey::EventId(_));
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

    async fn handle_live_message_event<C>(&self, sender: &UserId, content: C)
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
        self.inner.handle_live_event(raw, None, &self.own_user_id).await;
    }

    async fn handle_live_custom_event(&self, event: JsonValue) {
        let raw = Raw::new(&event).unwrap().cast();
        self.inner.handle_live_event(raw, None, &self.own_user_id).await;
    }

    async fn handle_live_redaction(&self, sender: &UserId, redacts: &EventId) {
        let ev = OriginalSyncRoomRedactionEvent {
            content: Default::default(),
            redacts: redacts.to_owned(),
            event_id: EventId::new(server_name!("dummy.server")),
            sender: sender.to_owned(),
            origin_server_ts: next_server_ts(),
            unsigned: Default::default(),
        };
        let raw = Raw::new(&ev).unwrap().cast();
        self.inner.handle_live_event(raw, None, &self.own_user_id).await;
    }

    async fn handle_local_event(&self, content: AnyMessageLikeEventContent) -> OwnedTransactionId {
        let txn_id = TransactionId::new();
        self.inner.handle_local_event(txn_id.clone(), content, &self.own_user_id).await;
        txn_id
    }
}

fn next_server_ts() -> MilliSecondsSinceUnixEpoch {
    static NEXT_TS: AtomicU32 = AtomicU32::new(0);
    MilliSecondsSinceUnixEpoch(NEXT_TS.fetch_add(1, SeqCst).into())
}
