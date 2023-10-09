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

use std::{collections::BTreeMap, sync::Arc};

use assert_matches::assert_matches;
use async_trait::async_trait;
use eyeball_im::VectorDiff;
use futures_core::Stream;
use futures_util::{FutureExt, StreamExt};
use indexmap::IndexMap;
use matrix_sdk::deserialized_responses::{SyncTimelineEvent, TimelineEvent};
use matrix_sdk_base::latest_event::LatestEvent;
use matrix_sdk_test::{EventBuilder, ALICE, BOB};
use ruma::{
    event_id,
    events::{
        receipt::{Receipt, ReceiptThread, ReceiptType},
        relation::Annotation,
        room::redaction::RoomRedactionEventContent,
        AnyMessageLikeEventContent, AnySyncTimelineEvent, AnyTimelineEvent, EmptyStateKey,
        MessageLikeEventContent, RedactedMessageLikeEventContent, RedactedStateEventContent,
        StaticStateEventContent,
    },
    int,
    power_levels::NotificationPowerLevels,
    push::{PushConditionRoomCtx, Ruleset},
    room_id,
    serde::Raw,
    server_name, uint, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId,
    OwnedUserId, RoomId, RoomVersionId, TransactionId, UserId,
};

use super::{
    event_item::EventItemIdentifier,
    inner::{ReactionAction, TimelineInnerSettings},
    reactions::ReactionToggleResult,
    traits::RoomDataProvider,
    EventTimelineItem, Profile, TimelineInner, TimelineItem,
};

mod basic;
mod echo;
mod edit;
#[cfg(feature = "e2e-encryption")]
mod encryption;
mod event_filter;
mod invalid;
mod polls;
mod reaction_group;
mod reactions;
mod read_receipts;
mod redaction;
mod virt;

struct TestTimeline {
    inner: TimelineInner<TestRoomDataProvider>,
    event_builder: EventBuilder,
}

impl TestTimeline {
    fn new() -> Self {
        Self { inner: TimelineInner::new(TestRoomDataProvider), event_builder: EventBuilder::new() }
    }

    fn with_settings(mut self, settings: TimelineInnerSettings) -> Self {
        self.inner = self.inner.with_settings(settings);
        self
    }

    async fn subscribe(&self) -> impl Stream<Item = VectorDiff<Arc<TimelineItem>>> {
        let (items, stream) = self.inner.subscribe().await;
        assert_eq!(items.len(), 0, "Please subscribe to TestTimeline before adding items to it");
        stream
    }

    async fn subscribe_events(&self) -> impl Stream<Item = VectorDiff<EventTimelineItem>> {
        let (items, stream) =
            self.inner.subscribe_filter_map(|item| item.as_event().cloned()).await;
        assert_eq!(items.len(), 0, "Please subscribe to TestTimeline before adding items to it");
        stream
    }

    async fn len(&self) -> usize {
        self.inner.items().await.len()
    }

    async fn handle_live_message_event<C>(&self, sender: &UserId, content: C)
    where
        C: MessageLikeEventContent,
    {
        let ev = self.event_builder.make_sync_message_event(sender, content);
        self.handle_live_event(Raw::new(&ev).unwrap().cast()).await;
    }

    async fn handle_live_redacted_message_event<C>(&self, sender: &UserId, content: C)
    where
        C: RedactedMessageLikeEventContent,
    {
        let ev = self.event_builder.make_sync_redacted_message_event(sender, content);
        self.handle_live_event(Raw::new(&ev).unwrap().cast()).await;
    }

    async fn handle_live_state_event<C>(&self, sender: &UserId, content: C, prev_content: Option<C>)
    where
        C: StaticStateEventContent<StateKey = EmptyStateKey>,
    {
        let ev = self.event_builder.make_sync_state_event(sender, "", content, prev_content);
        self.handle_live_event(ev).await;
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
        let ev = self.event_builder.make_sync_state_event(
            sender,
            state_key.as_ref(),
            content,
            prev_content,
        );
        self.handle_live_event(Raw::new(&ev).unwrap().cast()).await;
    }

    async fn handle_live_redacted_state_event<C>(&self, sender: &UserId, content: C)
    where
        C: RedactedStateEventContent<StateKey = EmptyStateKey>,
    {
        let ev = self.event_builder.make_sync_redacted_state_event(sender, "", content);
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
        let ev =
            self.event_builder.make_sync_redacted_state_event(sender, state_key.as_ref(), content);
        self.handle_live_event(Raw::new(&ev).unwrap().cast()).await;
    }

    async fn handle_live_custom_event(&self, event: Raw<AnySyncTimelineEvent>) {
        self.handle_live_event(event).await;
    }

    async fn handle_live_redaction(&self, sender: &UserId, redacts: &EventId) {
        let ev = self.event_builder.make_redaction_event(sender, redacts);
        self.handle_live_event(ev).await;
    }

    async fn handle_live_reaction(&self, sender: &UserId, annotation: &Annotation) -> OwnedEventId {
        let event_id = EventId::new(server_name!("dummy.server"));
        let ev = self.event_builder.make_reaction_event(sender, &event_id, annotation);
        self.handle_live_event(ev).await;
        event_id
    }

    async fn handle_live_event(&self, event: Raw<AnySyncTimelineEvent>) {
        let event = SyncTimelineEvent { event, encryption_info: None, push_actions: vec![] };
        self.inner.handle_live_event(event).await
    }

    async fn handle_local_event(&self, content: AnyMessageLikeEventContent) -> OwnedTransactionId {
        let txn_id = TransactionId::new();
        self.inner.handle_local_event(txn_id.clone(), content).await;
        txn_id
    }

    async fn handle_local_redaction_event(
        &self,
        redacts: EventItemIdentifier,
        content: RoomRedactionEventContent,
    ) -> OwnedTransactionId {
        let txn_id = TransactionId::new();
        self.inner.handle_local_redaction(txn_id.clone(), redacts, content).await;
        txn_id
    }

    async fn handle_back_paginated_message_event_with_id<C>(
        &self,
        sender: &UserId,
        room_id: &RoomId,
        event_id: &EventId,
        content: C,
    ) where
        C: MessageLikeEventContent,
    {
        let ev = self.event_builder.make_message_event_with_id(sender, room_id, event_id, content);
        self.handle_back_paginated_custom_event(ev).await;
    }

    async fn handle_back_paginated_custom_event(&self, event: Raw<AnyTimelineEvent>) {
        let timeline_event = TimelineEvent::new(event.cast());
        self.inner
            .handle_back_paginated_events(vec![timeline_event], Default::default())
            .await
            .unwrap();
    }

    async fn handle_read_receipts(
        &self,
        receipts: impl IntoIterator<Item = (OwnedEventId, ReceiptType, OwnedUserId, ReceiptThread)>,
    ) {
        let ev_content = self.event_builder.make_receipt_event_content(receipts);
        self.inner.handle_read_receipts(ev_content).await;
    }

    async fn toggle_reaction_local(
        &self,
        annotation: &Annotation,
    ) -> Result<ReactionAction, super::Error> {
        self.inner.toggle_reaction_local(annotation).await
    }

    async fn handle_reaction_response(
        &self,
        annotation: &Annotation,
        result: &ReactionToggleResult,
    ) -> Result<ReactionAction, super::Error> {
        self.inner.resolve_reaction_response(annotation, result).await
    }
}

#[derive(Clone)]
struct TestRoomDataProvider;

#[async_trait]
impl RoomDataProvider for TestRoomDataProvider {
    fn own_user_id(&self) -> &UserId {
        &ALICE
    }

    fn room_version(&self) -> RoomVersionId {
        RoomVersionId::V10
    }

    async fn profile_from_user_id(&self, _user_id: &UserId) -> Option<Profile> {
        None
    }

    async fn profile_from_latest_event(&self, _latest_event: &LatestEvent) -> Option<Profile> {
        None
    }

    async fn read_receipts_for_event(&self, event_id: &EventId) -> IndexMap<OwnedUserId, Receipt> {
        if event_id == event_id!("$event_with_bob_receipt") {
            [(BOB.to_owned(), Receipt::new(MilliSecondsSinceUnixEpoch(uint!(10))))].into()
        } else {
            IndexMap::new()
        }
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

pub(super) async fn assert_event_is_updated(
    stream: &mut (impl Stream<Item = VectorDiff<Arc<TimelineItem>>> + Unpin),
    event_id: &EventId,
    index: usize,
) -> EventTimelineItem {
    let (i, event) = assert_matches!(
        stream.next().await,
        Some(VectorDiff::Set { index, value }) => (index, value)
    );
    assert_eq!(i, index);
    let event = event.as_event().unwrap();
    assert_eq!(event.event_id().unwrap(), event_id);
    event.to_owned()
}

pub(super) async fn assert_no_more_updates(
    stream: &mut (impl Stream<Item = VectorDiff<Arc<TimelineItem>>> + Unpin),
) {
    assert!(stream.next().now_or_never().is_none())
}
