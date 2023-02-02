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

use async_trait::async_trait;
use futures_core::Stream;
use futures_signals::signal_vec::{SignalVecExt, VecDiff};
use matrix_sdk_base::deserialized_responses::SyncTimelineEvent;
use once_cell::sync::Lazy;
use ruma::{
    events::{
        AnyMessageLikeEventContent, EmptyStateKey, MessageLikeEventContent,
        RedactedStateEventContent, StateEventContent, StaticStateEventContent,
    },
    serde::Raw,
    server_name, user_id, EventId, MilliSecondsSinceUnixEpoch, OwnedTransactionId, TransactionId,
    UserId,
};
use serde_json::{json, Value as JsonValue};

use super::{inner::ProfileProvider, Profile, TimelineInner, TimelineItem};

mod basic;
mod echo;
mod encryption;
mod invalid;
mod virt;

static ALICE: Lazy<&UserId> = Lazy::new(|| user_id!("@alice:server.name"));
static BOB: Lazy<&UserId> = Lazy::new(|| user_id!("@bob:other.server"));

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
        let ev = json!({
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

    async fn profile(&self, _user_id: &UserId) -> Option<Profile> {
        None
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
