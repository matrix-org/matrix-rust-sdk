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

use std::sync::Arc;

use eyeball_im::VectorDiff;
use matrix_sdk::{deserialized_responses::TimelineEvent, send_queue::SendHandle};
#[cfg(test)]
use ruma::events::receipt::ReceiptEventContent;
use ruma::{
    MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId,
    events::{AnyMessageLikeEventContent, AnySyncEphemeralRoomEvent},
    room_version_rules::RoomVersionRules,
    serde::Raw,
};
use tracing::{instrument, trace, warn};

use super::{
    super::{
        Profile,
        date_dividers::DateDividerAdjuster,
        event_handler::{Flow, TimelineAction, TimelineEventContext, TimelineEventHandler},
        event_item::RemoteEventOrigin,
        traits::RoomDataProvider,
    },
    DateDividerMode, TimelineMetadata, TimelineSettings, TimelineStateTransaction,
    observable_items::ObservableItems,
};
use crate::{timeline::controller::TimelineFocusKind, unable_to_decrypt_hook::UtdHookManager};

#[derive(Debug)]
pub(in crate::timeline) struct TimelineState<P: RoomDataProvider> {
    pub items: ObservableItems,
    pub meta: TimelineMetadata,

    /// The kind of focus of this timeline.
    pub(super) focus: Arc<TimelineFocusKind<P>>,
}

impl<P: RoomDataProvider> TimelineState<P> {
    pub(super) fn new(
        focus: Arc<TimelineFocusKind<P>>,
        own_user_id: OwnedUserId,
        room_version_rules: RoomVersionRules,
        internal_id_prefix: Option<String>,
        unable_to_decrypt_hook: Option<Arc<UtdHookManager>>,
        is_room_encrypted: bool,
    ) -> Self {
        Self {
            items: ObservableItems::new(),
            meta: TimelineMetadata::new(
                own_user_id,
                room_version_rules,
                internal_id_prefix,
                unable_to_decrypt_hook,
                is_room_encrypted,
            ),
            focus,
        }
    }

    /// Handle updates on events as [`VectorDiff`]s.
    pub(super) async fn handle_remote_events_with_diffs(
        &mut self,
        diffs: Vec<VectorDiff<TimelineEvent>>,
        origin: RemoteEventOrigin,
        room_data: &P,
        settings: &TimelineSettings,
    ) {
        if diffs.is_empty() {
            return;
        }

        let mut transaction = self.transaction();
        transaction.handle_remote_events_with_diffs(diffs, origin, room_data, settings).await;
        transaction.commit();
    }

    /// Handle remote aggregations on events as [`VectorDiff`]s.
    pub(super) async fn handle_remote_aggregations(
        &mut self,
        diffs: Vec<VectorDiff<TimelineEvent>>,
        origin: RemoteEventOrigin,
        room_data: &P,
        settings: &TimelineSettings,
    ) {
        if diffs.is_empty() {
            return;
        }

        let mut transaction = self.transaction();
        transaction.handle_remote_aggregations(diffs, origin, room_data, settings).await;
        transaction.commit();
    }

    /// Marks the given event as fully read, using the read marker received from
    /// sync.
    pub(super) fn handle_fully_read_marker(&mut self, fully_read_event_id: OwnedEventId) {
        let mut txn = self.transaction();
        txn.set_fully_read_event(fully_read_event_id);
        txn.commit();
    }

    #[instrument(skip_all)]
    pub(super) async fn handle_ephemeral_events(
        &mut self,
        events: Vec<Raw<AnySyncEphemeralRoomEvent>>,
        room_data_provider: &P,
    ) {
        if events.is_empty() {
            return;
        }

        let mut txn = self.transaction();

        trace!("Handling ephemeral room events");
        let own_user_id = room_data_provider.own_user_id();
        for raw_event in events {
            match raw_event.deserialize() {
                Ok(AnySyncEphemeralRoomEvent::Receipt(ev)) => {
                    txn.handle_explicit_read_receipts(ev.content, own_user_id);
                }
                Ok(_) => {}
                Err(e) => {
                    let event_type = raw_event.get_field::<String>("type").ok().flatten();
                    warn!(event_type, "Failed to deserialize ephemeral event: {e}");
                }
            }
        }

        txn.commit();
    }

    /// Adds a local echo (for an event) to the timeline.
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip_all)]
    pub(super) async fn handle_local_event(
        &mut self,
        own_user_id: OwnedUserId,
        own_profile: Option<Profile>,
        date_divider_mode: DateDividerMode,
        txn_id: OwnedTransactionId,
        send_handle: Option<SendHandle>,
        content: AnyMessageLikeEventContent,
    ) {
        let mut txn = self.transaction();

        let mut date_divider_adjuster = DateDividerAdjuster::new(date_divider_mode);

        let is_thread_focus = txn.focus.is_thread();
        let (in_reply_to, thread_root) =
            txn.meta.process_content_relations(&content, None, &txn.items, is_thread_focus);

        // TODO merge with other should_add, one way or another?
        let should_add_new_items = match &txn.focus {
            TimelineFocusKind::Live { hide_threaded_events } => {
                thread_root.is_none() || !hide_threaded_events
            }
            TimelineFocusKind::Thread { root_event_id, .. } => {
                thread_root.as_ref().is_some_and(|r| r == root_event_id)
            }
            TimelineFocusKind::Event { .. } | TimelineFocusKind::PinnedEvents { .. } => {
                // Don't add new items to these timelines; aggregations are added independently
                // of the `should_add_new_items` value.
                false
            }
        };

        let ctx = TimelineEventContext {
            sender: own_user_id,
            sender_profile: own_profile,
            forwarder: None,
            forwarder_profile: None,
            timestamp: MilliSecondsSinceUnixEpoch::now(),
            read_receipts: Default::default(),
            // An event sent by ourselves is never matched against push rules.
            is_highlighted: false,
            flow: Flow::Local { txn_id, send_handle },
            should_add_new_items,
        };

        let timeline_action = TimelineAction::from_content(content, in_reply_to, thread_root, None);
        TimelineEventHandler::new(&mut txn, ctx)
            .handle_event(&mut date_divider_adjuster, timeline_action)
            .await;
        txn.adjust_date_dividers(date_divider_adjuster);

        txn.commit();
    }

    #[cfg(test)]
    pub(super) fn handle_read_receipts(
        &mut self,
        receipt_event_content: ReceiptEventContent,
        own_user_id: &ruma::UserId,
    ) {
        let mut txn = self.transaction();
        txn.handle_explicit_read_receipts(receipt_event_content, own_user_id);
        txn.commit();
    }

    pub(super) fn clear(&mut self) {
        let mut txn = self.transaction();
        txn.clear();
        txn.commit();
    }

    /// Replaces the existing events in the timeline with the given remote ones.
    ///
    /// Note: when the `position` is [`TimelineEnd::Front`], prepended events
    /// should be ordered in *reverse* topological order, that is, `events[0]`
    /// is the most recent.
    pub(super) async fn replace_with_remote_events<Events>(
        &mut self,
        events: Events,
        origin: RemoteEventOrigin,
        room_data_provider: &P,
        settings: &TimelineSettings,
    ) where
        Events: IntoIterator,
        Events::Item: Into<TimelineEvent>,
    {
        let mut txn = self.transaction();
        txn.clear();
        txn.handle_remote_events_with_diffs(
            vec![VectorDiff::Append { values: events.into_iter().map(Into::into).collect() }],
            origin,
            room_data_provider,
            settings,
        )
        .await;
        txn.commit();
    }

    pub(super) fn mark_all_events_as_encrypted(&mut self) {
        // When this transaction finishes, all items in the timeline will be emitted
        // again with the updated encryption value.
        let mut txn = self.transaction();
        txn.mark_all_events_as_encrypted();
        txn.commit();
    }

    pub(super) fn transaction(&mut self) -> TimelineStateTransaction<'_, P> {
        TimelineStateTransaction::new(&mut self.items, &mut self.meta, &*self.focus)
    }
}
