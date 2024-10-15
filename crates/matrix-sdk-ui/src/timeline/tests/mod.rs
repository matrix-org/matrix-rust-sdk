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
    future::ready,
    sync::Arc,
};

use eyeball::{SharedObservable, Subscriber};
use eyeball_im::VectorDiff;
use futures_core::Stream;
use indexmap::IndexMap;
use matrix_sdk::{
    config::RequestConfig,
    deserialized_responses::{SyncTimelineEvent, TimelineEvent},
    event_cache::paginator::{PaginableRoom, PaginatorError},
    executor::{BoxFuture, BoxFutureExt},
    room::{EventWithContextResponse, Messages, MessagesOptions},
    send_queue::RoomSendQueueUpdate,
    test_utils::events::EventFactory,
};
use matrix_sdk_base::{latest_event::LatestEvent, RoomInfo, RoomState};
use matrix_sdk_test::{EventBuilder, ALICE, BOB, DEFAULT_TEST_ROOM_ID};
use ruma::{
    event_id,
    events::{
        reaction::ReactionEventContent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        relation::{Annotation, RelationType},
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
use tokio::sync::RwLock;

use super::{
    controller::{TimelineEnd, TimelineSettings},
    event_handler::TimelineEventKind,
    event_item::RemoteEventOrigin,
    traits::RoomDataProvider,
    EventTimelineItem, Profile, TimelineController, TimelineFocus, TimelineItem,
};
use crate::{
    timeline::pinned_events_loader::PinnedEventsRoom, unable_to_decrypt_hook::UtdHookManager,
};

mod basic;
mod echo;
mod edit;
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
    controller: TimelineController<TestRoomDataProvider>,
    event_builder: EventBuilder,
    /// An [`EventFactory`] that can be used for creating events in this
    /// timeline.
    pub factory: EventFactory,
}

impl TestTimeline {
    fn new() -> Self {
        Self::with_room_data_provider(TestRoomDataProvider::default())
    }

    /// Returns the associated inner data from that [`TestTimeline`].
    fn data(&self) -> &TestRoomDataProvider {
        &self.controller.room_data_provider
    }

    fn with_internal_id_prefix(prefix: String) -> Self {
        Self {
            controller: TimelineController::new(
                TestRoomDataProvider::default(),
                TimelineFocus::Live,
                Some(prefix),
                None,
                Some(false),
            ),
            event_builder: EventBuilder::new(),
            factory: EventFactory::new(),
        }
    }

    fn with_room_data_provider(room_data_provider: TestRoomDataProvider) -> Self {
        Self {
            controller: TimelineController::new(
                room_data_provider,
                TimelineFocus::Live,
                None,
                None,
                Some(false),
            ),
            event_builder: EventBuilder::new(),
            factory: EventFactory::new(),
        }
    }

    fn with_unable_to_decrypt_hook(hook: Arc<UtdHookManager>) -> Self {
        Self {
            controller: TimelineController::new(
                TestRoomDataProvider::default(),
                TimelineFocus::Live,
                None,
                Some(hook),
                Some(true),
            ),
            event_builder: EventBuilder::new(),
            factory: EventFactory::new(),
        }
    }

    // TODO: this is wrong, see also #3850.
    fn with_is_room_encrypted(encrypted: bool) -> Self {
        Self {
            controller: TimelineController::new(
                TestRoomDataProvider::default(),
                TimelineFocus::Live,
                None,
                None,
                Some(encrypted),
            ),
            event_builder: EventBuilder::new(),
            factory: EventFactory::new(),
        }
    }

    fn with_settings(mut self, settings: TimelineSettings) -> Self {
        self.controller = self.controller.with_settings(settings);
        self
    }

    async fn subscribe(&self) -> impl Stream<Item = VectorDiff<Arc<TimelineItem>>> {
        let (items, stream) = self.controller.subscribe().await;
        assert_eq!(items.len(), 0, "Please subscribe to TestTimeline before adding items to it");
        stream
    }

    async fn subscribe_events(&self) -> impl Stream<Item = VectorDiff<EventTimelineItem>> {
        let (items, stream) =
            self.controller.subscribe_filter_map(|item| item.as_event().cloned()).await;
        assert_eq!(items.len(), 0, "Please subscribe to TestTimeline before adding items to it");
        stream
    }

    async fn len(&self) -> usize {
        self.controller.items().await.len()
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
        self.controller
            .add_events_at(vec![event], TimelineEnd::Back, RemoteEventOrigin::Sync)
            .await;
    }

    async fn handle_local_event(&self, content: AnyMessageLikeEventContent) -> OwnedTransactionId {
        let txn_id = TransactionId::new();
        self.controller
            .handle_local_event(
                txn_id.clone(),
                TimelineEventKind::Message { content, relations: Default::default() },
                None,
            )
            .await;
        txn_id
    }

    async fn handle_back_paginated_event(&self, event: Raw<AnyTimelineEvent>) {
        let timeline_event = TimelineEvent::new(event.cast());
        self.controller
            .add_events_at(vec![timeline_event], TimelineEnd::Front, RemoteEventOrigin::Pagination)
            .await;
    }

    async fn handle_read_receipts(
        &self,
        receipts: impl IntoIterator<Item = (OwnedEventId, ReceiptType, OwnedUserId, ReceiptThread)>,
    ) {
        let ev_content = self.event_builder.make_receipt_event_content(receipts);
        self.controller.handle_read_receipts(ev_content).await;
    }

    async fn toggle_reaction_local(&self, unique_id: &str, key: &str) -> Result<(), super::Error> {
        if self.controller.toggle_reaction_local(unique_id, key).await? {
            // TODO(bnjbvr): hacky?
            if let Some(event_id) = self
                .controller
                .items()
                .await
                .iter()
                .rfind(|item| item.unique_id() == unique_id)
                .and_then(|item| item.as_event()?.as_remote())
                .map(|event_item| event_item.event_id.clone())
            {
                // Fake a local echo, for new reactions.
                self.handle_local_event(
                    ReactionEventContent::new(Annotation::new(event_id, key.to_owned())).into(),
                )
                .await;
            }
        }
        Ok(())
    }

    async fn handle_room_send_queue_update(&self, update: RoomSendQueueUpdate) {
        self.controller.handle_room_send_queue_update(update).await
    }
}

type ReadReceiptMap =
    HashMap<ReceiptType, HashMap<ReceiptThread, HashMap<OwnedUserId, (OwnedEventId, Receipt)>>>;

#[derive(Clone, Default)]
struct TestRoomDataProvider {
    /// The initial list of user receipts for that room.
    ///
    /// Configurable at construction, static for the lifetime of the provider.
    initial_user_receipts: ReadReceiptMap,

    /// Event id of the event pointed to by the fully read marker.
    ///
    /// Configurable at construction, static for the lifetime of the provider.
    fully_read_marker: Option<OwnedEventId>,

    /// Events sent with that room data provider.
    pub sent_events: Arc<RwLock<Vec<AnyMessageLikeEventContent>>>,

    /// Events redacted with that room data providier.
    pub redacted: Arc<RwLock<Vec<OwnedEventId>>>,
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

impl PinnedEventsRoom for TestRoomDataProvider {
    fn load_event_with_relations<'a>(
        &'a self,
        _event_id: &'a EventId,
        _request_config: Option<RequestConfig>,
        _related_event_filters: Option<Vec<RelationType>>,
    ) -> BoxFuture<'a, Result<(SyncTimelineEvent, Vec<SyncTimelineEvent>), PaginatorError>> {
        unimplemented!();
    }

    fn pinned_event_ids(&self) -> Vec<OwnedEventId> {
        unimplemented!();
    }

    fn is_pinned_event(&self, _event_id: &EventId) -> bool {
        unimplemented!();
    }
}

impl RoomDataProvider for TestRoomDataProvider {
    fn own_user_id(&self) -> &UserId {
        &ALICE
    }

    fn room_version(&self) -> RoomVersionId {
        RoomVersionId::V10
    }

    fn profile_from_user_id<'a>(&'a self, _user_id: &'a UserId) -> BoxFuture<'a, Option<Profile>> {
        ready(None).box_future()
    }

    fn profile_from_latest_event(&self, _latest_event: &LatestEvent) -> Option<Profile> {
        None
    }

    fn load_user_receipt(
        &self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> BoxFuture<'_, Option<(OwnedEventId, Receipt)>> {
        ready(
            self.initial_user_receipts
                .get(&receipt_type)
                .and_then(|thread_map| thread_map.get(&thread))
                .and_then(|user_map| user_map.get(user_id))
                .cloned(),
        )
        .box_future()
    }

    fn load_event_receipts(
        &self,
        event_id: &EventId,
    ) -> BoxFuture<'_, IndexMap<OwnedUserId, Receipt>> {
        ready(if event_id == event_id!("$event_with_bob_receipt") {
            [(BOB.to_owned(), Receipt::new(MilliSecondsSinceUnixEpoch(uint!(10))))].into()
        } else {
            IndexMap::new()
        })
        .box_future()
    }

    fn push_rules_and_context(&self) -> BoxFuture<'_, Option<(Ruleset, PushConditionRoomCtx)>> {
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

        ready(Some((push_rules, push_context))).box_future()
    }

    fn load_fully_read_marker(&self) -> BoxFuture<'_, Option<OwnedEventId>> {
        ready(self.fully_read_marker.clone()).box_future()
    }

    fn send(&self, content: AnyMessageLikeEventContent) -> BoxFuture<'_, Result<(), super::Error>> {
        async move {
            self.sent_events.write().await.push(content);
            Ok(())
        }
        .box_future()
    }

    fn redact<'a>(
        &'a self,
        event_id: &'a EventId,
        _reason: Option<&'a str>,
        _transaction_id: Option<OwnedTransactionId>,
    ) -> BoxFuture<'a, Result<(), super::Error>> {
        async move {
            self.redacted.write().await.push(event_id.to_owned());
            Ok(())
        }
        .box_future()
    }

    fn room_info(&self) -> Subscriber<RoomInfo> {
        let info = RoomInfo::new(*DEFAULT_TEST_ROOM_ID, RoomState::Joined);
        SharedObservable::new(info).subscribe()
    }
}
