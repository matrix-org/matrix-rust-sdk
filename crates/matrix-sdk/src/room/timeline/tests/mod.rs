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

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering::SeqCst},
        Arc,
    },
};

use async_trait::async_trait;
use eyeball_im::VectorDiff;
use futures_core::Stream;
use indexmap::IndexMap;
use matrix_sdk_base::deserialized_responses::TimelineEvent;
use once_cell::sync::Lazy;
use ruma::{
    events::{
        receipt::{Receipt, ReceiptEventContent, ReceiptThread, ReceiptType},
        AnyMessageLikeEventContent, AnySyncTimelineEvent, EmptyStateKey, MessageLikeEventContent,
        RedactedMessageLikeEventContent, RedactedStateEventContent, StateEventContent,
        StaticStateEventContent,
    },
    int,
    power_levels::NotificationPowerLevels,
    push::{PushConditionRoomCtx, Ruleset},
    room_id,
    serde::Raw,
    server_name, uint, user_id, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId,
    OwnedTransactionId, OwnedUserId, TransactionId, UserId,
};
use serde_json::{json, Value as JsonValue};

use super::{inner::RoomDataProvider, Profile, TimelineInner, TimelineItem};

mod basic;
mod echo;
#[cfg(feature = "e2e-encryption")]
mod encryption;
mod invalid;
mod read_receipts;
mod virt;

static ALICE: Lazy<&UserId> = Lazy::new(|| user_id!("@alice:server.name"));
static BOB: Lazy<&UserId> = Lazy::new(|| user_id!("@bob:other.server"));

struct TestTimeline {
    inner: TimelineInner<TestRoomDataProvider>,
    next_ts: AtomicU64,
}

impl TestTimeline {
    fn new() -> Self {
        Self { inner: TimelineInner::new(TestRoomDataProvider), next_ts: AtomicU64::new(0) }
    }

    fn with_read_receipt_tracking(mut self) -> Self {
        self.inner = self.inner.with_read_receipt_tracking(true);
        self
    }

    async fn subscribe(&self) -> impl Stream<Item = VectorDiff<Arc<TimelineItem>>> {
        let (items, stream) = self.inner.subscribe().await;
        assert_eq!(items.len(), 0, "Please subscribe to TestTimeline before adding items to it");
        stream
    }

    async fn handle_live_message_event<C>(&self, sender: &UserId, content: C)
    where
        C: MessageLikeEventContent,
    {
        let ev = self.make_message_event(sender, content);
        self.handle_live_event(Raw::new(&ev).unwrap().cast()).await;
    }

    async fn handle_live_redacted_message_event<C>(&self, sender: &UserId, content: C)
    where
        C: RedactedMessageLikeEventContent,
    {
        let ev = self.make_redacted_message_event(sender, content);
        self.handle_live_event(Raw::new(&ev).unwrap().cast()).await;
    }

    async fn handle_live_state_event<C>(&self, sender: &UserId, content: C, prev_content: Option<C>)
    where
        C: StaticStateEventContent<StateKey = EmptyStateKey>,
    {
        let ev = self.make_state_event(sender, "", content, prev_content);
        self.handle_live_event(Raw::new(&ev).unwrap().cast()).await;
    }

    async fn handle_live_state_event_with_state_key<C>(
        &self,
        sender: &UserId,
        state_key: C::StateKey,
        content: C,
        prev_content: Option<C>,
    ) where
        C: StaticStateEventContent,
    {
        let ev = self.make_state_event(sender, state_key.as_ref(), content, prev_content);
        self.handle_live_event(Raw::new(&ev).unwrap().cast()).await;
    }

    async fn handle_live_redacted_state_event<C>(&self, sender: &UserId, content: C)
    where
        C: RedactedStateEventContent<StateKey = EmptyStateKey>,
    {
        let ev = self.make_redacted_state_event(sender, "", content);
        self.handle_live_event(Raw::new(&ev).unwrap().cast()).await;
    }

    async fn handle_live_redacted_state_event_with_state_key<C>(
        &self,
        sender: &UserId,
        state_key: C::StateKey,
        content: C,
    ) where
        C: RedactedStateEventContent,
    {
        let ev = self.make_redacted_state_event(sender, state_key.as_ref(), content);
        self.handle_live_event(Raw::new(&ev).unwrap().cast()).await;
    }

    async fn handle_live_custom_event(&self, event: JsonValue) {
        let raw = Raw::new(&event).unwrap().cast();
        self.handle_live_event(raw).await;
    }

    async fn handle_live_redaction(&self, sender: &UserId, redacts: &EventId) {
        let ev = json!({
            "type": "m.room.redaction",
            "content": {},
            "redacts": redacts,
            "event_id": EventId::new(server_name!("dummy.server")),
            "sender": sender,
            "origin_server_ts": self.next_server_ts(),
        });
        let raw = Raw::new(&ev).unwrap().cast();
        self.handle_live_event(raw).await;
    }

    async fn handle_live_event(&self, raw: Raw<AnySyncTimelineEvent>) {
        self.inner.handle_live_event(raw, None, vec![]).await
    }

    async fn handle_local_event(&self, content: AnyMessageLikeEventContent) -> OwnedTransactionId {
        let txn_id = TransactionId::new();
        self.inner.handle_local_event(txn_id.clone(), content).await;
        txn_id
    }

    async fn handle_back_paginated_custom_event(&self, event: JsonValue) {
        let timeline_event = TimelineEvent::new(Raw::new(&event).unwrap().cast());
        self.inner.handle_back_paginated_event(timeline_event).await;
    }

    async fn handle_read_receipts(
        &self,
        receipts: impl IntoIterator<Item = (OwnedEventId, ReceiptType, OwnedUserId, ReceiptThread)>,
    ) {
        let ev_content = self.make_receipt_event_content(receipts);
        self.inner.handle_read_receipts(ev_content).await;
    }

    /// Set the next server timestamp.
    ///
    /// Timestamps will continue to increase by 1 (millisecond) from that value.
    fn set_next_ts(&self, value: u64) {
        self.next_ts.store(value, SeqCst);
    }

    fn make_message_event<C: MessageLikeEventContent>(
        &self,
        sender: &UserId,
        content: C,
    ) -> JsonValue {
        json!({
            "type": content.event_type(),
            "content": content,
            "event_id": EventId::new(server_name!("dummy.server")),
            "sender": sender,
            "origin_server_ts": self.next_server_ts(),
        })
    }

    fn make_redacted_message_event<C: RedactedMessageLikeEventContent>(
        &self,
        sender: &UserId,
        content: C,
    ) -> JsonValue {
        json!({
            "type": content.event_type(),
            "content": content,
            "event_id": EventId::new(server_name!("dummy.server")),
            "sender": sender,
            "origin_server_ts": self.next_server_ts(),
            "unsigned": self.make_redacted_unsigned(sender),
        })
    }

    fn make_state_event<C: StateEventContent>(
        &self,
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
            "origin_server_ts": self.next_server_ts(),
            "unsigned": unsigned,
        })
    }

    fn make_redacted_state_event<C: RedactedStateEventContent>(
        &self,
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
            "origin_server_ts": self.next_server_ts(),
            "unsigned": self.make_redacted_unsigned(sender),
        })
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

    fn make_receipt_event_content(
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

    fn next_server_ts(&self) -> MilliSecondsSinceUnixEpoch {
        MilliSecondsSinceUnixEpoch(
            self.next_ts
                .fetch_add(1, SeqCst)
                .try_into()
                .expect("server timestamp should fit in js_int::UInt"),
        )
    }
}

struct TestRoomDataProvider;

#[async_trait]
impl RoomDataProvider for TestRoomDataProvider {
    fn own_user_id(&self) -> &UserId {
        &ALICE
    }

    async fn profile(&self, _user_id: &UserId) -> Option<Profile> {
        None
    }

    async fn read_receipts_for_event(&self, _event_id: &EventId) -> IndexMap<OwnedUserId, Receipt> {
        IndexMap::new()
    }

    async fn push_rules_and_context(&self) -> Option<(Ruleset, PushConditionRoomCtx)> {
        let push_rules = Ruleset::server_default(&ALICE);
        let push_context = PushConditionRoomCtx {
            room_id: room_id!("!my_room:server.name").to_owned(),
            member_count: uint!(2),
            user_id: ALICE.to_owned(),
            user_display_name: "Alice".to_owned(),
            users_power_levels: BTreeMap::new(),
            default_power_level: int!(0),
            notification_power_levels: NotificationPowerLevels::new(),
        };

        Some((push_rules, push_context))
    }
}
