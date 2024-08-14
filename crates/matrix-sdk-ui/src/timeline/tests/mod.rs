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
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use assert_matches2::assert_let;
use async_trait::async_trait;
use eyeball_im::VectorDiff;
use futures_core::Stream;
use futures_util::{FutureExt, StreamExt};
use indexmap::IndexMap;
use matrix_sdk::{
    config::RequestConfig,
    deserialized_responses::{SyncTimelineEvent, TimelineEvent},
    event_cache::paginator::{PaginableRoom, PaginatorError},
    room::{EventWithContextResponse, Messages, MessagesOptions},
    send_queue::RoomSendQueueUpdate,
    test_utils::events::EventFactory,
};
use matrix_sdk_base::latest_event::LatestEvent;
use matrix_sdk_test::{EventBuilder, ALICE, BOB};
use ruma::{
    event_id,
    events::{
        receipt::{Receipt, ReceiptThread, ReceiptType},
        relation::Annotation,
        AnyMessageLikeEventContent, AnyTimelineEvent, EmptyStateKey,
        RedactedMessageLikeEventContent, RedactedStateEventContent, StaticStateEventContent,
    },
    int,
    power_levels::NotificationPowerLevels,
    push::{PushConditionPowerLevelsCtx, PushConditionRoomCtx, Ruleset},
    room_id,
    serde::Raw,
    uint, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId,
    RoomVersionId, TransactionId, UInt, UserId,
};

use super::{
    event_handler::TimelineEventKind,
    event_item::RemoteEventOrigin,
    inner::{TimelineEnd, TimelineInnerSettings},
    reactions::{ReactionAction, ReactionToggleResult},
    traits::RoomDataProvider,
    EventTimelineItem, Profile, TimelineFocus, TimelineInner, TimelineItem,
};
use crate::{
    timeline::pinned_events_loader::PinnedEventsRoom, unable_to_decrypt_hook::UtdHookManager,
};

mod basic;
mod echo;
mod edit;
#[cfg(feature = "e2e-encryption")]
mod encryption;
mod event_filter;
mod invalid;
mod polls;
mod reactions;
mod read_receipts;
mod redaction;
mod shields;
mod virt;

struct TestTimeline {
    inner: TimelineInner<TestRoomDataProvider>,
    event_builder: EventBuilder,
    /// An [`EventFactory`] that can be used for creating events in this
    /// timeline.
    pub factory: EventFactory,
}

impl TestTimeline {
    fn new() -> Self {
        Self::with_room_data_provider(TestRoomDataProvider::default())
    }

    fn with_internal_id_prefix(prefix: String) -> Self {
        Self {
            inner: TimelineInner::new(
                TestRoomDataProvider::default(),
                TimelineFocus::Live,
                Some(prefix),
                None,
                false,
            ),
            event_builder: EventBuilder::new(),
            factory: EventFactory::new(),
        }
    }

    fn with_room_data_provider(room_data_provider: TestRoomDataProvider) -> Self {
        Self {
            inner: TimelineInner::new(room_data_provider, TimelineFocus::Live, None, None, false),
            event_builder: EventBuilder::new(),
            factory: EventFactory::new(),
        }
    }

    fn with_unable_to_decrypt_hook(hook: Arc<UtdHookManager>) -> Self {
        Self {
            inner: TimelineInner::new(
                TestRoomDataProvider::default(),
                TimelineFocus::Live,
                None,
                Some(hook),
                true,
            ),
            event_builder: EventBuilder::new(),
            factory: EventFactory::new(),
        }
    }

    // TODO: this is wrong, see also #3850.
    fn with_is_room_encrypted(encrypted: bool) -> Self {
        Self {
            inner: TimelineInner::new(
                TestRoomDataProvider::default(),
                TimelineFocus::Live,
                None,
                None,
                encrypted,
            ),
            event_builder: EventBuilder::new(),
            factory: EventFactory::new(),
        }
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

    async fn handle_live_event(&self, event: impl Into<SyncTimelineEvent>) {
        let event = event.into();
        self.inner.add_events_at(vec![event], TimelineEnd::Back, RemoteEventOrigin::Sync).await;
    }

    async fn handle_local_event(&self, content: AnyMessageLikeEventContent) -> OwnedTransactionId {
        let txn_id = TransactionId::new();
        self.inner
            .handle_local_event(
                txn_id.clone(),
                TimelineEventKind::Message { content, relations: Default::default() },
                None,
            )
            .await;
        txn_id
    }

    async fn handle_local_redaction_event(&self, redacts: &EventId) -> OwnedTransactionId {
        let txn_id = TransactionId::new();
        self.inner
            .handle_local_event(
                txn_id.clone(),
                TimelineEventKind::Redaction { redacts: redacts.to_owned() },
                None,
            )
            .await;
        txn_id
    }

    async fn handle_back_paginated_event(&self, event: Raw<AnyTimelineEvent>) {
        let timeline_event = TimelineEvent::new(event.cast());
        self.inner
            .add_events_at(vec![timeline_event], TimelineEnd::Front, RemoteEventOrigin::Pagination)
            .await;
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

    async fn handle_room_send_queue_update(&self, update: RoomSendQueueUpdate) {
        self.inner.handle_room_send_queue_update(update).await
    }
}

type ReadReceiptMap =
    HashMap<ReceiptType, HashMap<ReceiptThread, HashMap<OwnedUserId, (OwnedEventId, Receipt)>>>;

#[derive(Clone, Default)]
struct TestRoomDataProvider {
    initial_user_receipts: ReadReceiptMap,
    fully_read_marker: Option<OwnedEventId>,
}

impl TestRoomDataProvider {
    fn with_initial_user_receipts(mut self, initial_user_receipts: ReadReceiptMap) -> Self {
        self.initial_user_receipts = initial_user_receipts;
        self
    }
    fn with_fully_read_marker(mut self, event_id: OwnedEventId) -> Self {
        self.fully_read_marker = Some(event_id);
        self
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl PaginableRoom for TestRoomDataProvider {
    async fn event_with_context(
        &self,
        _event_id: &EventId,
        _lazy_load_members: bool,
        _num_events: UInt,
    ) -> Result<EventWithContextResponse, PaginatorError> {
        unimplemented!();
    }

    async fn messages(&self, _opts: MessagesOptions) -> Result<Messages, PaginatorError> {
        unimplemented!();
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl PinnedEventsRoom for TestRoomDataProvider {
    async fn load_event(
        &self,
        _event_id: &EventId,
        _config: Option<RequestConfig>,
    ) -> Result<SyncTimelineEvent, PaginatorError> {
        unimplemented!();
    }

    fn pinned_event_ids(&self) -> Vec<OwnedEventId> {
        unimplemented!();
    }

    fn is_pinned_event(&self, _event_id: &EventId) -> bool {
        unimplemented!();
    }
}

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

    async fn load_user_receipt(
        &self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> Option<(OwnedEventId, Receipt)> {
        self.initial_user_receipts
            .get(&receipt_type)
            .and_then(|thread_map| thread_map.get(&thread))
            .and_then(|user_map| user_map.get(user_id))
            .cloned()
    }

    async fn load_event_receipts(&self, event_id: &EventId) -> IndexMap<OwnedUserId, Receipt> {
        if event_id == event_id!("$event_with_bob_receipt") {
            [(BOB.to_owned(), Receipt::new(MilliSecondsSinceUnixEpoch(uint!(10))))].into()
        } else {
            IndexMap::new()
        }
    }

    async fn push_rules_and_context(&self) -> Option<(Ruleset, PushConditionRoomCtx)> {
        let push_rules = Ruleset::server_default(&ALICE);
        let power_levels = PushConditionPowerLevelsCtx {
            users: BTreeMap::new(),
            users_default: int!(0),
            notifications: NotificationPowerLevels::new(),
        };
        let push_context = PushConditionRoomCtx {
            room_id: room_id!("!my_room:server.name").to_owned(),
            member_count: uint!(2),
            user_id: ALICE.to_owned(),
            user_display_name: "Alice".to_owned(),
            power_levels: Some(power_levels),
        };

        Some((push_rules, push_context))
    }

    async fn load_fully_read_marker(&self) -> Option<OwnedEventId> {
        self.fully_read_marker.clone()
    }
}

pub(super) async fn assert_event_is_updated(
    stream: &mut (impl Stream<Item = VectorDiff<Arc<TimelineItem>>> + Unpin),
    event_id: &EventId,
    index: usize,
) -> EventTimelineItem {
    assert_let!(Some(VectorDiff::Set { index: i, value: event }) = stream.next().await);
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
