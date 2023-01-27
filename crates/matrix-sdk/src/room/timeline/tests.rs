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
use async_trait::async_trait;
use chrono::{Datelike, Local, TimeZone};
use futures_core::Stream;
use futures_signals::signal_vec::{SignalVecExt, VecDiff};
use futures_util::StreamExt;
use matrix_sdk_base::{crypto::OlmMachine, deserialized_responses::SyncTimelineEvent};
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
            member::{MembershipState, RedactedRoomMemberEventContent, RoomMemberEventContent},
            message::{self, MessageType, RoomMessageEventContent},
            name::RoomNameEventContent,
            topic::RedactedRoomTopicEventContent,
        },
        AnyMessageLikeEventContent, EmptyStateKey, FullStateEventContent, MessageLikeEventContent,
        MessageLikeEventType, RedactedStateEventContent, StateEventContent, StateEventType,
        StaticStateEventContent,
    },
    room_id,
    serde::Raw,
    server_name, uint, user_id, EventId, MilliSecondsSinceUnixEpoch, OwnedTransactionId,
    TransactionId, UserId,
};
use serde_json::{json, Value as JsonValue};

use super::{
    event_item::AnyOtherFullStateEventContent, inner::ProfileProvider, EncryptedMessage,
    EventTimelineItem, MembershipChange, Profile, TimelineInner, TimelineItem, TimelineItemContent,
    TimelineKey, VirtualTimelineItem,
};

static ALICE: Lazy<&UserId> = Lazy::new(|| user_id!("@alice:server.name"));
static BOB: Lazy<&UserId> = Lazy::new(|| user_id!("@bob:other.server"));

#[async_test]
async fn reaction_redaction() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.stream();

    timeline.handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("hi!")).await;
    let _day_divider = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let event = item.as_event().unwrap().as_remote().unwrap();
    assert_eq!(event.reactions().len(), 0);

    let msg_event_id = &event.event_id;

    let rel = Annotation::new(msg_event_id.to_owned(), "+1".to_owned());
    timeline.handle_live_message_event(&BOB, ReactionEventContent::new(rel)).await;
    let item =
        assert_matches!(stream.next().await, Some(VecDiff::UpdateAt { index: 1, value }) => value);
    let event = item.as_event().unwrap().as_remote().unwrap();
    assert_eq!(event.reactions().len(), 1);

    // TODO: After adding raw timeline items, check for one here

    let reaction_event_id = event.event_id.as_ref();

    timeline.handle_live_redaction(&BOB, reaction_event_id).await;
    let item =
        assert_matches!(stream.next().await, Some(VecDiff::UpdateAt { index: 1, value }) => value);
    let event = item.as_event().unwrap().as_remote().unwrap();
    assert_eq!(event.reactions().len(), 0);
}

#[async_test]
async fn invalid_edit() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.stream();

    timeline.handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("test")).await;
    let _day_divider = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let event = item.as_event().unwrap().as_remote().unwrap();
    let msg = event.content.as_message().unwrap();
    assert_eq!(msg.body(), "test");

    let msg_event_id = &event.event_id;

    let edit = assign!(RoomMessageEventContent::text_plain(" * fake"), {
        relates_to: Some(message::Relation::Replacement(Replacement::new(
            msg_event_id.to_owned(),
            MessageType::text_plain("fake"),
        ))),
    });
    // Edit is from a different user than the previous event
    timeline.handle_live_message_event(&BOB, edit).await;

    // Can't easily test the non-arrival of an item using the stream. Instead
    // just assert that there is still just a couple items in the timeline.
    assert_eq!(timeline.inner.items().len(), 2);
}

#[async_test]
async fn edit_redacted() {
    let timeline = TestTimeline::new();
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
    let _day_divider = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);

    let redacted_event_id = item.as_event().unwrap().event_id().unwrap();

    let edit = assign!(RoomMessageEventContent::text_plain(" * test"), {
        relates_to: Some(message::Relation::Replacement(Replacement::new(
            redacted_event_id.to_owned(),
            MessageType::text_plain("test"),
        ))),
    });
    timeline.handle_live_message_event(&ALICE, edit).await;

    assert_eq!(timeline.inner.items().len(), 2);
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

    let timeline = TestTimeline::new();
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

    assert_eq!(timeline.inner.items().len(), 2);

    let _day_divider = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
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
        )
        .await;

    assert_eq!(timeline.inner.items().len(), 2);

    let item =
        assert_matches!(stream.next().await, Some(VecDiff::UpdateAt { index: 1, value }) => value);
    let event = item.as_event().unwrap().as_remote().unwrap();
    assert_matches!(&event.encryption_info, Some(_));
    let text = assert_matches!(&event.content, TimelineItemContent::Message(msg) => msg.body());
    assert_eq!(text, "It's a secret to everybody");
}

#[async_test]
async fn update_read_marker() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.stream();

    timeline.handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("A")).await;
    let _day_divider = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let event_id = item.as_event().unwrap().event_id().unwrap().to_owned();

    timeline.inner.set_fully_read_event(event_id).await;
    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    assert_matches!(item.as_virtual(), Some(VirtualTimelineItem::ReadMarker));

    timeline.handle_live_message_event(&BOB, RoomMessageEventContent::text_plain("B")).await;
    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let event_id = item.as_event().unwrap().event_id().unwrap().to_owned();

    timeline.inner.set_fully_read_event(event_id.clone()).await;
    assert_matches!(stream.next().await, Some(VecDiff::Move { old_index: 2, new_index: 3 }));

    // Nothing should happen if the fully read event is set back to the same event
    // as before.
    timeline.inner.set_fully_read_event(event_id.clone()).await;

    // Nothing should happen if the fully read event isn't found.
    timeline.inner.set_fully_read_event(event_id!("$fake_event_id").to_owned()).await;

    // Nothing should happen if the fully read event is referring to an old event
    // that has already been marked as fully read.
    timeline.inner.set_fully_read_event(event_id).await;

    timeline.handle_live_message_event(&ALICE, RoomMessageEventContent::text_plain("C")).await;
    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let event_id = item.as_event().unwrap().event_id().unwrap().to_owned();

    timeline.inner.set_fully_read_event(event_id).await;
    assert_matches!(stream.next().await, Some(VecDiff::Move { old_index: 3, new_index: 4 }));
}

#[async_test]
async fn invalid_event_content() {
    let timeline = TestTimeline::new();
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

    let _day_divider = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let event_item = item.as_event().unwrap().as_remote().unwrap();
    assert_eq!(event_item.sender, "@alice:example.org");
    assert_eq!(event_item.event_id, event_id!("$eeG0HA0FAZ37wP8kXlNkxx3I").to_owned());
    assert_eq!(event_item.timestamp, MilliSecondsSinceUnixEpoch(uint!(10)));
    let event_type = assert_matches!(
        &event_item.content,
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
    let event_item = item.as_event().unwrap().as_remote().unwrap();
    assert_eq!(event_item.sender, "@alice:example.org");
    assert_eq!(event_item.event_id, event_id!("$d5G0HA0FAZ37wP8kXlNkxx3I").to_owned());
    assert_eq!(event_item.timestamp, MilliSecondsSinceUnixEpoch(uint!(2179)));
    let (event_type, state_key) = assert_matches!(
        &event_item.content,
        TimelineItemContent::FailedToParseState {
            event_type,
            state_key,
            ..
        } => (event_type, state_key)
    );
    assert_eq!(*event_type, StateEventType::RoomMember);
    assert_eq!(state_key, "@alice:example.org");
}

#[async_test]
async fn invalid_event() {
    let timeline = TestTimeline::new();

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
    assert_eq!(timeline.inner.items().len(), 0);
}

#[async_test]
async fn remote_echo_without_txn_id() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.stream();

    // Given a local event…
    let txn_id = timeline
        .handle_local_event(AnyMessageLikeEventContent::RoomMessage(
            RoomMessageEventContent::text_plain("echo"),
        ))
        .await;

    let _day_divider = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);

    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    assert_matches!(item.as_event().unwrap(), EventTimelineItem::Local(_));

    // That has an event ID assigned already (from the response to sending it)…
    let event_id = event_id!("$W6mZSLWMmfuQQ9jhZWeTxFIM");
    timeline.inner.add_event_id(&txn_id, event_id.to_owned());

    let item =
        assert_matches!(stream.next().await, Some(VecDiff::UpdateAt { value, index: 1 }) => value);
    assert_matches!(item.as_event().unwrap(), EventTimelineItem::Local(_));

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

    // The local echo is removed
    assert_matches!(stream.next().await, Some(VecDiff::Pop { .. }));
    // This day divider shouldn't be present, or the previous one should be
    // removed. There being a two day dividers in a row is a bug, but it's
    // non-trivial to fix and rare enough that we can fix it later (only happens
    // when the first message on a given day is a local echo).
    let _day_divider = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);

    // … and the remote echo is added.
    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    assert_matches!(item.as_event().unwrap(), EventTimelineItem::Remote(_));
}

#[async_test]
async fn remote_echo_new_position() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.stream();

    // Given a local event…
    let txn_id = timeline
        .handle_local_event(AnyMessageLikeEventContent::RoomMessage(
            RoomMessageEventContent::text_plain("echo"),
        ))
        .await;

    let _day_divider = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);

    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let txn_id_from_event = item.as_event().unwrap().as_local().unwrap();
    assert_eq!(txn_id, *txn_id_from_event.transaction_id);

    // … and another event that comes back before the remote echo
    timeline.handle_live_message_event(&BOB, RoomMessageEventContent::text_plain("test")).await;
    let _day_divider = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let _bob_message = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);

    // When the remote echo comes in…
    timeline
        .handle_live_custom_event(json!({
            "content": {
                "body": "echo",
                "msgtype": "m.text",
            },
            "sender": &*ALICE,
            "event_id": "$eeG0HA0FAZ37wP8kXlNkxx3I",
            "origin_server_ts": 6,
            "type": "m.room.message",
            "unsigned": {
                "transaction_id": txn_id,
            },
        }))
        .await;

    // … the local echo should be removed
    assert_matches!(stream.next().await, Some(VecDiff::RemoveAt { index: 1 }));

    // … and the remote echo added
    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    assert_matches!(item.as_event().unwrap(), EventTimelineItem::Remote(_));
}

#[async_test]
async fn day_divider() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.stream();

    timeline
        .handle_live_custom_event(json!({
            "content": {
                "msgtype": "m.text",
                "body": "This is a first message on the first day"
            },
            "event_id": "$eeG0HA0FAZ37wP8kXlNkxx3I",
            "origin_server_ts": 1669897395000u64,
            "sender": "@alice:example.org",
            "type": "m.room.message",
        }))
        .await;

    let day_divider = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let ts = assert_matches!(
        day_divider.as_virtual().unwrap(),
        VirtualTimelineItem::DayDivider(ts) => *ts
    );
    let date = Local.timestamp_millis_opt(ts.0.into()).single().unwrap();
    assert_eq!(date.year(), 2022);
    assert_eq!(date.month(), 12);
    assert_eq!(date.day(), 1);

    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    item.as_event().unwrap();

    timeline
        .handle_live_custom_event(json!({
            "content": {
                "msgtype": "m.text",
                "body": "This is a second message on the first day"
            },
            "event_id": "$feG0HA0FAZ37wP8kXlNkxx3I",
            "origin_server_ts": 1669906604000u64,
            "sender": "@alice:example.org",
            "type": "m.room.message",
        }))
        .await;

    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    item.as_event().unwrap();

    timeline
        .handle_live_custom_event(json!({
            "content": {
                "msgtype": "m.text",
                "body": "This is a first message on the next day"
            },
            "event_id": "$geG0HA0FAZ37wP8kXlNkxx3I",
            "origin_server_ts": 1669992963000u64,
            "sender": "@alice:example.org",
            "type": "m.room.message",
        }))
        .await;

    let day_divider = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let ts = assert_matches!(
        day_divider.as_virtual().unwrap(),
        VirtualTimelineItem::DayDivider(ts) => *ts
    );
    let date = Local.timestamp_millis_opt(ts.0.into()).single().unwrap();
    assert_eq!(date.year(), 2022);
    assert_eq!(date.month(), 12);
    assert_eq!(date.day(), 2);

    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    item.as_event().unwrap();

    let _ = timeline
        .handle_local_event(AnyMessageLikeEventContent::RoomMessage(
            RoomMessageEventContent::text_plain("A message I'm sending just now"),
        ))
        .await;

    // The other events are in the past so a local event always creates a new day
    // divider.
    let day_divider = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    assert_matches!(day_divider.as_virtual().unwrap(), VirtualTimelineItem::DayDivider { .. });

    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    item.as_event().unwrap();
}

#[async_test]
async fn sticker() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.stream();

    timeline
        .handle_live_custom_event(json!({
            "content": {
                "body": "Happy sticker",
                "info": {
                    "h": 398,
                    "mimetype": "image/jpeg",
                    "size": 31037,
                    "w": 394
                },
                "url": "mxc://server.name/JWEIFJgwEIhweiWJE",
            },
            "event_id": "$143273582443PhrSn",
            "origin_server_ts": 143273582,
            "sender": "@alice:server.name",
            "type": "m.sticker",
        }))
        .await;

    let _day_divider = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);

    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    assert_matches!(item.as_event().unwrap().content(), TimelineItemContent::Sticker(_));
}

#[async_test]
async fn initial_events() {
    let timeline = TestTimeline::with_initial_events([
        (*ALICE, RoomMessageEventContent::text_plain("A").into()),
        (*BOB, RoomMessageEventContent::text_plain("B").into()),
    ])
    .await;
    let mut stream = timeline.stream();

    let items = assert_matches!(stream.next().await, Some(VecDiff::Replace { values }) => values);
    assert_eq!(items.len(), 3);
    assert_matches!(items[0].as_virtual().unwrap(), VirtualTimelineItem::DayDivider { .. });
    assert_eq!(items[1].as_event().unwrap().sender(), *ALICE);
    assert_eq!(items[2].as_event().unwrap().sender(), *BOB);
}

#[async_test]
async fn other_state() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.stream();

    timeline
        .handle_live_original_state_event(
            &ALICE,
            RoomNameEventContent::new(Some("Alice's room".to_owned())),
            None,
        )
        .await;

    let _day_divider = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);

    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let ev = assert_matches!(item.as_event().unwrap().content(), TimelineItemContent::OtherState(ev) => ev);
    let full_content =
        assert_matches!(ev.content(), AnyOtherFullStateEventContent::RoomName(c) => c);
    let (content, prev_content) = assert_matches!(full_content, FullStateEventContent::Original { content, prev_content } => (content, prev_content));
    assert_eq!(content.name.as_ref().unwrap(), "Alice's room");
    assert_matches!(prev_content, None);

    timeline.handle_live_redacted_state_event(&ALICE, RedactedRoomTopicEventContent::new()).await;

    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let ev = assert_matches!(item.as_event().unwrap().content(), TimelineItemContent::OtherState(ev) => ev);
    let full_content =
        assert_matches!(ev.content(), AnyOtherFullStateEventContent::RoomTopic(c) => c);
    assert_matches!(full_content, FullStateEventContent::Redacted(_));
}

#[async_test]
async fn room_member() {
    let timeline = TestTimeline::new();
    let mut stream = timeline.stream();

    let mut first_room_member_content = RoomMemberEventContent::new(MembershipState::Invite);
    first_room_member_content.displayname = Some("Alice".to_owned());
    timeline
        .handle_live_original_state_event_with_state_key(
            &BOB,
            ALICE.to_owned(),
            first_room_member_content.clone(),
            None,
        )
        .await;

    let _day_divider = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);

    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let membership = assert_matches!(item.as_event().unwrap().content(), TimelineItemContent::MembershipChange(ev) => ev);
    assert_matches!(membership.content(), FullStateEventContent::Original { .. });
    assert_matches!(membership.change(), Some(MembershipChange::Invited));

    let mut second_room_member_content = RoomMemberEventContent::new(MembershipState::Join);
    second_room_member_content.displayname = Some("Alice".to_owned());
    timeline
        .handle_live_original_state_event_with_state_key(
            &ALICE,
            ALICE.to_owned(),
            second_room_member_content.clone(),
            Some(first_room_member_content),
        )
        .await;

    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let membership = assert_matches!(item.as_event().unwrap().content(), TimelineItemContent::MembershipChange(ev) => ev);
    assert_matches!(membership.content(), FullStateEventContent::Original { .. });
    assert_matches!(membership.change(), Some(MembershipChange::InvitationAccepted));

    let mut third_room_member_content = RoomMemberEventContent::new(MembershipState::Join);
    third_room_member_content.displayname = Some("Alice In Wonderland".to_owned());
    timeline
        .handle_live_original_state_event_with_state_key(
            &ALICE,
            ALICE.to_owned(),
            third_room_member_content,
            Some(second_room_member_content),
        )
        .await;

    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let profile = assert_matches!(item.as_event().unwrap().content(), TimelineItemContent::ProfileChange(ev) => ev);
    assert_matches!(profile.displayname_change(), Some(_));
    assert_matches!(profile.avatar_url_change(), None);

    timeline
        .handle_live_redacted_state_event_with_state_key(
            &ALICE,
            ALICE.to_owned(),
            RedactedRoomMemberEventContent::new(MembershipState::Join),
        )
        .await;

    let item = assert_matches!(stream.next().await, Some(VecDiff::Push { value }) => value);
    let membership = assert_matches!(item.as_event().unwrap().content(), TimelineItemContent::MembershipChange(ev) => ev);
    assert_matches!(membership.content(), FullStateEventContent::Redacted(_));
    assert_matches!(membership.change(), None);
}

struct TestTimeline {
    inner: TimelineInner<TestProfileProvider>,
}

impl TestTimeline {
    fn new() -> Self {
        Self { inner: TimelineInner::new(TestProfileProvider) }
    }

    async fn with_initial_events<'a>(
        events: impl IntoIterator<Item = (&'a UserId, AnyMessageLikeEventContent)>,
    ) -> Self {
        let mut inner = TimelineInner::new(TestProfileProvider);
        inner
            .add_initial_events(
                events
                    .into_iter()
                    .map(|(sender, content)| {
                        let event =
                            serde_json::from_value(make_message_event(sender, content)).unwrap();
                        SyncTimelineEvent { event, encryption_info: None }
                    })
                    .collect(),
            )
            .await;

        Self { inner }
    }

    fn stream(&self) -> impl Stream<Item = VecDiff<Arc<TimelineItem>>> {
        self.inner.items_signal().to_stream()
    }

    async fn handle_live_message_event<C>(&self, sender: &UserId, content: C)
    where
        C: MessageLikeEventContent,
    {
        let ev = make_message_event(sender, content);
        let raw = Raw::new(&ev).unwrap().cast();
        self.inner.handle_live_event(raw, None).await;
    }

    async fn handle_live_original_state_event<C>(
        &self,
        sender: &UserId,
        content: C,
        prev_content: Option<C>,
    ) where
        C: StaticStateEventContent<StateKey = EmptyStateKey>,
    {
        let ev = make_state_event(sender, "", content, prev_content);
        let raw = Raw::new(&ev).unwrap().cast();
        self.inner.handle_live_event(raw, None).await;
    }

    async fn handle_live_original_state_event_with_state_key<C>(
        &self,
        sender: &UserId,
        state_key: C::StateKey,
        content: C,
        prev_content: Option<C>,
    ) where
        C: StaticStateEventContent,
    {
        let ev = make_state_event(sender, state_key.as_ref(), content, prev_content);
        let raw = Raw::new(&ev).unwrap().cast();
        self.inner.handle_live_event(raw, None).await;
    }

    async fn handle_live_redacted_state_event<C>(&self, sender: &UserId, content: C)
    where
        C: RedactedStateEventContent<StateKey = EmptyStateKey>,
    {
        let ev = make_redacted_state_event(sender, "", content);
        let raw = Raw::new(&ev).unwrap().cast();
        self.inner.handle_live_event(raw, None).await;
    }

    async fn handle_live_redacted_state_event_with_state_key<C>(
        &self,
        sender: &UserId,
        state_key: C::StateKey,
        content: C,
    ) where
        C: RedactedStateEventContent,
    {
        let ev = make_redacted_state_event(sender, state_key.as_ref(), content);
        let raw = Raw::new(&ev).unwrap().cast();
        self.inner.handle_live_event(raw, None).await;
    }

    async fn handle_live_custom_event(&self, event: JsonValue) {
        let raw = Raw::new(&event).unwrap().cast();
        self.inner.handle_live_event(raw, None).await;
    }

    async fn handle_live_redaction(&self, sender: &UserId, redacts: &EventId) {
        let ev = json! ({
            "type": "m.room.redaction",
            "content": {},
            "redacts": redacts,
            "event_id": EventId::new(server_name!("dummy.server")),
            "sender": sender,
            "origin_server_ts": next_server_ts(),
        });
        let raw = Raw::new(&ev).unwrap().cast();
        self.inner.handle_live_event(raw, None).await;
    }

    async fn handle_local_event(&self, content: AnyMessageLikeEventContent) -> OwnedTransactionId {
        let txn_id = TransactionId::new();
        self.inner.handle_local_event(txn_id.clone(), content).await;
        txn_id
    }
}

struct TestProfileProvider;

#[async_trait]
impl ProfileProvider for TestProfileProvider {
    fn own_user_id(&self) -> &UserId {
        &ALICE
    }

    async fn profile(&self, _user_id: &UserId) -> Profile {
        Profile { display_name: None, display_name_ambiguous: false, avatar_url: None }
    }
}

fn make_message_event<C: MessageLikeEventContent>(sender: &UserId, content: C) -> JsonValue {
    json!({
        "type": content.event_type(),
        "content": content,
        "event_id": EventId::new(server_name!("dummy.server")),
        "sender": sender,
        "origin_server_ts": next_server_ts(),
    })
}

fn make_state_event<C: StateEventContent>(
    sender: &UserId,
    state_key: &str,
    content: C,
    prev_content: Option<C>,
) -> JsonValue {
    let unsigned = if let Some(prev_content) = prev_content {
        json!({ "prev_content": prev_content })
    } else {
        json!({})
    };

    json!({
        "type": content.event_type(),
        "state_key": state_key,
        "content": content,
        "event_id": EventId::new(server_name!("dummy.server")),
        "sender": sender,
        "origin_server_ts": next_server_ts(),
        "unsigned": unsigned,
    })
}

fn make_redacted_state_event<C: RedactedStateEventContent>(
    sender: &UserId,
    state_key: &str,
    content: C,
) -> JsonValue {
    json!({
        "type": content.event_type(),
        "state_key": state_key,
        "content": content,
        "event_id": EventId::new(server_name!("dummy.server")),
        "sender": sender,
        "origin_server_ts": next_server_ts(),
        "unsigned": make_redacted_unsigned(sender),
    })
}

fn make_redacted_unsigned(sender: &UserId) -> JsonValue {
    json!({
        "redacted_because": {
            "content": {},
            "event_id": EventId::new(server_name!("dummy.server")),
            "sender": sender,
            "origin_server_ts": next_server_ts(),
            "type": "m.room.redaction",
        },
    })
}

fn next_server_ts() -> MilliSecondsSinceUnixEpoch {
    static NEXT_TS: AtomicU32 = AtomicU32::new(0);
    MilliSecondsSinceUnixEpoch(NEXT_TS.fetch_add(1, SeqCst).into())
}
