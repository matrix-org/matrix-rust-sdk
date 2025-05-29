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
    ops::Sub,
    sync::Arc,
    time::{Duration, SystemTime},
};

use eyeball::{SharedObservable, Subscriber};
use eyeball_im::VectorDiff;
use futures_core::Stream;
use imbl::vector;
use indexmap::IndexMap;
use matrix_sdk::{
    config::RequestConfig,
    crypto::OlmMachine,
    deserialized_responses::{EncryptionInfo, TimelineEvent},
    event_cache::paginator::{PaginableRoom, PaginatorError},
    room::{EventWithContextResponse, Messages, MessagesOptions, PushContext, Relations},
    send_queue::RoomSendQueueUpdate,
    BoxFuture,
};
use matrix_sdk_base::{
    crypto::types::events::CryptoContextInfo, latest_event::LatestEvent, RoomInfo, RoomState,
};
use matrix_sdk_test::{event_factory::EventFactory, ALICE, DEFAULT_TEST_ROOM_ID};
use ruma::{
    events::{
        reaction::ReactionEventContent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        relation::{Annotation, RelationType},
        AnyMessageLikeEventContent, AnyTimelineEvent,
    },
    int,
    power_levels::NotificationPowerLevels,
    push::{PushConditionPowerLevelsCtx, PushConditionRoomCtx, Ruleset},
    room_id,
    serde::Raw,
    uint, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId, OwnedTransactionId,
    OwnedUserId, RoomVersionId, TransactionId, UInt, UserId,
};
use tokio::sync::RwLock;

use super::{
    algorithms::rfind_event_by_item_id, controller::TimelineSettings,
    event_item::RemoteEventOrigin, traits::RoomDataProvider, EventTimelineItem, Profile,
    TimelineController, TimelineEventItemId, TimelineFocus, TimelineItem,
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

/// A timeline instance used only for testing purposes in unit tests.
#[derive(Default)]
struct TestTimelineBuilder {
    provider: Option<TestRoomDataProvider>,
    internal_id_prefix: Option<String>,
    utd_hook: Option<Arc<UtdHookManager>>,
    is_room_encrypted: bool,
    settings: Option<TimelineSettings>,
}

impl TestTimelineBuilder {
    fn new() -> Self {
        Self::default()
    }

    fn provider(mut self, provider: TestRoomDataProvider) -> Self {
        self.provider = Some(provider);
        self
    }

    fn internal_id_prefix(mut self, prefix: String) -> Self {
        self.internal_id_prefix = Some(prefix);
        self
    }

    fn unable_to_decrypt_hook(mut self, hook: Arc<UtdHookManager>) -> Self {
        self.utd_hook = Some(hook);
        // It only makes sense to have a UTD hook for an encrypted room.
        self.is_room_encrypted = true;
        self
    }

    fn room_encrypted(mut self, encrypted: bool) -> Self {
        self.is_room_encrypted = encrypted;
        self
    }

    fn settings(mut self, settings: TimelineSettings) -> Self {
        self.settings = Some(settings);
        self
    }

    fn build(self) -> TestTimeline {
        let mut controller = TimelineController::new(
            self.provider.unwrap_or_default(),
            TimelineFocus::Live,
            self.internal_id_prefix,
            self.utd_hook,
            self.is_room_encrypted,
        );
        if let Some(settings) = self.settings {
            controller = controller.with_settings(settings);
        }
        TestTimeline { controller, factory: EventFactory::new() }
    }
}

struct TestTimeline {
    controller: TimelineController<TestRoomDataProvider, (OlmMachine, OwnedRoomId)>,

    /// An [`EventFactory`] that can be used for creating events in this
    /// timeline.
    pub factory: EventFactory,
}

impl TestTimeline {
    fn new() -> Self {
        TestTimelineBuilder::new().build()
    }

    /// Returns the associated inner data from that [`TestTimeline`].
    fn data(&self) -> &TestRoomDataProvider {
        &self.controller.room_data_provider
    }

    async fn subscribe(&self) -> impl Stream<Item = VectorDiff<Arc<TimelineItem>>> {
        let (items, stream) = self.controller.subscribe_raw().await;
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

    async fn handle_live_event(&self, event: impl Into<TimelineEvent>) {
        self.controller
            .handle_remote_events_with_diffs(
                vec![VectorDiff::Append { values: vector![event.into()] }],
                RemoteEventOrigin::Sync,
            )
            .await;
    }

    async fn handle_local_event(&self, content: AnyMessageLikeEventContent) -> OwnedTransactionId {
        let txn_id = TransactionId::new();
        self.controller.handle_local_event(txn_id.clone(), content, None).await;
        txn_id
    }

    async fn handle_back_paginated_event(&self, event: Raw<AnyTimelineEvent>) {
        let timeline_event = TimelineEvent::new(event.cast());
        self.controller
            .handle_remote_events_with_diffs(
                vec![VectorDiff::PushFront { value: timeline_event }],
                RemoteEventOrigin::Pagination,
            )
            .await;
    }

    async fn handle_read_receipts(
        &self,
        receipts: impl IntoIterator<Item = (OwnedEventId, ReceiptType, OwnedUserId, ReceiptThread)>,
    ) {
        let mut read_receipt = self.factory.read_receipts();
        for (event_id, tyype, user_id, thread) in receipts {
            read_receipt = read_receipt.add(&event_id, &user_id, tyype, thread);
        }
        let ev_content = read_receipt.into_content();
        self.controller.handle_read_receipts(ev_content).await;
    }

    async fn toggle_reaction_local(
        &self,
        item_id: &TimelineEventItemId,
        key: &str,
    ) -> Result<(), super::Error> {
        if self.controller.toggle_reaction_local(item_id, key).await? {
            // TODO(bnjbvr): hacky?
            let items = self.controller.items().await;
            if let Some(event_id) = rfind_event_by_item_id(&items, item_id)
                .and_then(|(_pos, item)| item.event_id().map(ToOwned::to_owned))
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

    async fn handle_event_update(
        &self,
        diffs: Vec<VectorDiff<TimelineEvent>>,
        origin: RemoteEventOrigin,
    ) {
        self.controller.handle_remote_events_with_diffs(diffs, origin).await;
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

    /// The [`EncryptionInfo`] describing the Megolm sessions that were used to
    /// encrypt events.
    pub encryption_info: HashMap<String, Arc<EncryptionInfo>>,
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

    fn with_encryption_info(
        mut self,
        session_id: &str,
        encryption_info: Arc<EncryptionInfo>,
    ) -> TestRoomDataProvider {
        self.encryption_info.insert(session_id.to_owned(), encryption_info);
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
    ) -> BoxFuture<'a, Result<(TimelineEvent, Vec<TimelineEvent>), PaginatorError>> {
        unimplemented!();
    }

    fn pinned_event_ids(&self) -> Option<Vec<OwnedEventId>> {
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

    async fn crypto_context_info(&self) -> CryptoContextInfo {
        CryptoContextInfo {
            device_creation_ts: MilliSecondsSinceUnixEpoch::from_system_time(
                SystemTime::now().sub(Duration::from_secs(60 * 3)),
            )
            .unwrap_or(MilliSecondsSinceUnixEpoch::now()),
            is_backup_configured: false,
            this_device_is_verified: true,
            backup_exists_on_server: true,
        }
    }

    async fn profile_from_user_id<'a>(&'a self, _user_id: &'a UserId) -> Option<Profile> {
        None
    }

    fn profile_from_latest_event(&self, _latest_event: &LatestEvent) -> Option<Profile> {
        None
    }

    async fn load_user_receipt<'a>(
        &'a self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &'a UserId,
    ) -> Option<(OwnedEventId, Receipt)> {
        self.initial_user_receipts
            .get(&receipt_type)
            .and_then(|thread_map| thread_map.get(&thread))
            .and_then(|user_map| user_map.get(user_id))
            .cloned()
    }

    async fn load_event_receipts<'a>(
        &'a self,
        event_id: &'a EventId,
    ) -> IndexMap<OwnedUserId, Receipt> {
        let mut map = IndexMap::new();

        for (user_id, (receipt_event_id, receipt)) in
            self.initial_user_receipts.values().flat_map(|m| m.values()).flatten()
        {
            if receipt_event_id == event_id {
                map.insert(user_id.clone(), receipt.clone());
            }
        }

        map
    }

    async fn push_context(&self) -> Option<PushContext> {
        let push_rules = Ruleset::server_default(&ALICE);
        let power_levels = PushConditionPowerLevelsCtx {
            users: BTreeMap::new(),
            users_default: int!(0),
            notifications: NotificationPowerLevels::new(),
        };
        let push_condition_room_ctx = PushConditionRoomCtx {
            room_id: room_id!("!my_room:server.name").to_owned(),
            member_count: uint!(2),
            user_id: ALICE.to_owned(),
            user_display_name: "Alice".to_owned(),
            power_levels: Some(power_levels),
        };
        Some(PushContext::new(push_condition_room_ctx, push_rules))
    }

    async fn load_fully_read_marker(&self) -> Option<OwnedEventId> {
        self.fully_read_marker.clone()
    }

    async fn send(&self, content: AnyMessageLikeEventContent) -> Result<(), super::Error> {
        self.sent_events.write().await.push(content);
        Ok(())
    }

    async fn redact<'a>(
        &'a self,
        event_id: &'a EventId,
        _reason: Option<&'a str>,
        _transaction_id: Option<OwnedTransactionId>,
    ) -> Result<(), super::Error> {
        self.redacted.write().await.push(event_id.to_owned());
        Ok(())
    }

    fn room_info(&self) -> Subscriber<RoomInfo> {
        let info = RoomInfo::new(*DEFAULT_TEST_ROOM_ID, RoomState::Joined);
        SharedObservable::new(info).subscribe()
    }

    async fn get_encryption_info(
        &self,
        session_id: &str,
        _sender: &UserId,
    ) -> Option<Arc<EncryptionInfo>> {
        self.encryption_info.get(session_id).cloned()
    }

    async fn relations(
        &self,
        _event_id: OwnedEventId,
        _opts: matrix_sdk::room::RelationsOptions,
    ) -> Result<Relations, matrix_sdk::Error> {
        unimplemented!();
    }
}
