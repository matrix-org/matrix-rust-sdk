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

//! Unit tests (based on private methods) for the timeline API.

use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering::SeqCst},
};

use ruma::{
    events::{
        receipt::{Receipt, ReceiptEventContent, ReceiptThread, ReceiptType},
        relation::Annotation,
        AnySyncTimelineEvent, MessageLikeEventContent, RedactedMessageLikeEventContent,
        RedactedStateEventContent, StateEventContent,
    },
    serde::Raw,
    server_name, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId, UserId,
};
use serde_json::{json, Value as JsonValue};

#[derive(Default)]
pub struct EventBuilder {
    next_ts: AtomicU64,
}

impl EventBuilder {
    pub fn new() -> EventBuilder {
        Self { next_ts: AtomicU64::new(0) }
    }

    pub fn next_server_ts(&self) -> MilliSecondsSinceUnixEpoch {
        MilliSecondsSinceUnixEpoch(
            self.next_ts
                .fetch_add(1, SeqCst)
                .try_into()
                .expect("server timestamp should fit in js_int::UInt"),
        )
    }

    /// Set the next server timestamp.
    ///
    /// Timestamps will continue to increase by 1 (millisecond) from that value.
    pub fn set_next_ts(&self, value: u64) {
        self.next_ts.store(value, SeqCst);
    }

    pub fn make_message_event<C: MessageLikeEventContent>(
        &self,
        sender: &UserId,
        content: C,
    ) -> Raw<AnySyncTimelineEvent> {
        self.make_message_event_with_id(
            sender,
            content,
            &EventId::new(server_name!("dummy.server")),
        )
    }

    pub fn make_message_event_with_id<C: MessageLikeEventContent>(
        &self,
        sender: &UserId,
        content: C,
        event_id: &EventId,
    ) -> Raw<AnySyncTimelineEvent> {
        sync_timeline_event!({
            "type": content.event_type(),
            "content": content,
            "event_id": event_id,
            "sender": sender,
            "origin_server_ts": self.next_server_ts(),
        })
    }

    pub fn make_redacted_message_event<C: RedactedMessageLikeEventContent>(
        &self,
        sender: &UserId,
        content: C,
    ) -> Raw<AnySyncTimelineEvent> {
        sync_timeline_event!({
            "type": content.event_type(),
            "content": content,
            "event_id": EventId::new(server_name!("dummy.server")),
            "sender": sender,
            "origin_server_ts": self.next_server_ts(),
            "unsigned": self.make_redacted_unsigned(sender),
        })
    }

    pub fn make_state_event<C: StateEventContent>(
        &self,
        sender: &UserId,
        state_key: &str,
        content: C,
        prev_content: Option<C>,
    ) -> Raw<AnySyncTimelineEvent> {
        let unsigned = if let Some(prev_content) = prev_content {
            json!({ "prev_content": prev_content })
        } else {
            json!({})
        };

        sync_timeline_event!({
            "type": content.event_type(),
            "state_key": state_key,
            "content": content,
            "event_id": EventId::new(server_name!("dummy.server")),
            "sender": sender,
            "origin_server_ts": self.next_server_ts(),
            "unsigned": unsigned,
        })
    }

    pub fn make_redacted_state_event<C: RedactedStateEventContent>(
        &self,
        sender: &UserId,
        state_key: &str,
        content: C,
    ) -> Raw<AnySyncTimelineEvent> {
        sync_timeline_event!({
            "type": content.event_type(),
            "state_key": state_key,
            "content": content,
            "event_id": EventId::new(server_name!("dummy.server")),
            "sender": sender,
            "origin_server_ts": self.next_server_ts(),
            "unsigned": self.make_redacted_unsigned(sender),
        })
    }

    pub fn make_reaction(
        &self,
        sender: &UserId,
        annotation: &Annotation,
        timestamp: MilliSecondsSinceUnixEpoch,
    ) -> Raw<AnySyncTimelineEvent> {
        sync_timeline_event!({
            "event_id": EventId::new(server_name!("dummy.server")),
            "content": {
                "m.relates_to": {
                    "event_id": annotation.event_id,
                    "key": annotation.key,
                    "rel_type": "m.annotation"
                }
            },
            "sender": sender,
            "type": "m.reaction",
            "origin_server_ts": timestamp
        })
    }

    pub fn make_receipt_event_content(
        &self,
        receipts: impl IntoIterator<Item = (OwnedEventId, ReceiptType, OwnedUserId, ReceiptThread)>,
    ) -> ReceiptEventContent {
        let mut ev_content = ReceiptEventContent(BTreeMap::new());
        for (event_id, receipt_type, user_id, thread) in receipts {
            let event_map = ev_content.entry(event_id).or_default();
            let receipt_map = event_map.entry(receipt_type).or_default();

            let mut receipt = Receipt::new(self.next_server_ts());
            receipt.thread = thread;

            receipt_map.insert(user_id, receipt);
        }

        ev_content
    }

    fn make_redacted_unsigned(&self, sender: &UserId) -> JsonValue {
        json!({
            "redacted_because": {
                "content": {},
                "event_id": EventId::new(server_name!("dummy.server")),
                "sender": sender,
                "origin_server_ts": self.next_server_ts(),
                "type": "m.room.redaction",
            },
        })
    }
}
