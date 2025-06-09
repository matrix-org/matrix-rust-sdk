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

use std::{future::Future, sync::Arc};

use eyeball_im::VectorDiff;
use matrix_sdk::{deserialized_responses::TimelineEvent, send_queue::SendHandle};
#[cfg(test)]
use ruma::events::receipt::ReceiptEventContent;
use ruma::{
    events::{AnyMessageLikeEventContent, AnySyncEphemeralRoomEvent},
    serde::Raw,
    MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId, RoomVersionId,
};
use tracing::{instrument, trace, warn};

use super::{
    super::{
        date_dividers::DateDividerAdjuster,
        event_handler::{
            Flow, TimelineAction, TimelineEventContext, TimelineEventHandler, TimelineItemPosition,
        },
        event_item::RemoteEventOrigin,
        traits::RoomDataProvider,
        Profile, TimelineItem,
    },
    observable_items::ObservableItems,
    DateDividerMode, TimelineFocusKind, TimelineMetadata, TimelineSettings,
    TimelineStateTransaction,
};
use crate::unable_to_decrypt_hook::UtdHookManager;

#[derive(Debug)]
pub(in crate::timeline) struct TimelineState {
    pub items: ObservableItems,
    pub meta: TimelineMetadata,

    /// The kind of focus of this timeline.
    pub timeline_focus: TimelineFocusKind,
}

impl TimelineState {
    pub(super) fn new(
        timeline_focus: TimelineFocusKind,
        own_user_id: OwnedUserId,
        room_version: RoomVersionId,
        internal_id_prefix: Option<String>,
        unable_to_decrypt_hook: Option<Arc<UtdHookManager>>,
        is_room_encrypted: bool,
    ) -> Self {
        Self {
            items: ObservableItems::new(),
            meta: TimelineMetadata::new(
                own_user_id,
                room_version,
                internal_id_prefix,
                unable_to_decrypt_hook,
                is_room_encrypted,
            ),
            timeline_focus,
        }
    }

    /// Handle updates on events as [`VectorDiff`]s.
    pub(super) async fn handle_remote_events_with_diffs<RoomData>(
        &mut self,
        diffs: Vec<VectorDiff<TimelineEvent>>,
        origin: RemoteEventOrigin,
        room_data: &RoomData,
        settings: &TimelineSettings,
    ) where
        RoomData: RoomDataProvider,
    {
        if diffs.is_empty() {
            return;
        }

        let mut transaction = self.transaction();
        transaction.handle_remote_events_with_diffs(diffs, origin, room_data, settings).await;
        transaction.commit();
    }

    /// Handle remote aggregations on events as [`VectorDiff`]s.
    pub(super) async fn handle_remote_aggregations<RoomData>(
        &mut self,
        diffs: Vec<VectorDiff<TimelineEvent>>,
        origin: RemoteEventOrigin,
        room_data: &RoomData,
        settings: &TimelineSettings,
    ) where
        RoomData: RoomDataProvider,
    {
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
    pub(super) async fn handle_ephemeral_events<P: RoomDataProvider>(
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

        let (in_reply_to, thread_root) =
            txn.meta.process_content_relations(&content, None, &txn.items, &txn.timeline_focus);

        // TODO merge with other should_add, one way or another?
        let should_add_new_items = match &txn.timeline_focus {
            TimelineFocusKind::Live { hide_threaded_events } => {
                thread_root.is_none() || !hide_threaded_events
            }
            TimelineFocusKind::Thread { root_event_id } => {
                thread_root.as_ref().is_some_and(|r| r == root_event_id)
            }
            TimelineFocusKind::Event { .. } | TimelineFocusKind::PinnedEvents => {
                // Don't add new items to these timelines; aggregations are added independently
                // of the `should_add_new_items` value.
                false
            }
        };

        let ctx = TimelineEventContext {
            sender: own_user_id,
            sender_profile: own_profile,
            timestamp: MilliSecondsSinceUnixEpoch::now(),
            read_receipts: Default::default(),
            // An event sent by ourselves is never matched against push rules.
            is_highlighted: false,
            flow: Flow::Local { txn_id, send_handle },
            should_add_new_items,
        };

        if let Some(timeline_action) =
            TimelineAction::from_content(content, in_reply_to, thread_root, None)
        {
            TimelineEventHandler::new(&mut txn, ctx)
                .handle_event(&mut date_divider_adjuster, timeline_action)
                .await;
            txn.adjust_date_dividers(date_divider_adjuster);
        }

        txn.commit();
    }

    pub(super) async fn retry_event_decryption<P: RoomDataProvider, Fut>(
        &mut self,
        retry_one: impl Fn(Arc<TimelineItem>) -> Fut,
        retry_indices: Vec<usize>,
        room_data_provider: &P,
        settings: &TimelineSettings,
    ) where
        Fut: Future<Output = Option<TimelineEvent>>,
    {
        let mut txn = self.transaction();

        let mut date_divider_adjuster =
            DateDividerAdjuster::new(settings.date_divider_mode.clone());

        // Loop through all the indices, in order so we don't decrypt edits
        // before the event being edited, if both were UTD. Keep track of
        // index change as UTDs are removed instead of updated.
        let mut offset = 0;
        for idx in retry_indices {
            let idx = idx - offset;
            let Some(event) = retry_one(txn.items[idx].clone()).await else {
                continue;
            };

            let removed_item = txn
                .handle_remote_event(
                    event,
                    TimelineItemPosition::UpdateAt { timeline_item_index: idx },
                    room_data_provider,
                    settings,
                    &mut date_divider_adjuster,
                )
                .await;

            // If the UTD was removed rather than updated, offset all
            // subsequent loop iterations.
            if removed_item {
                offset += 1;
            }
        }

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
    pub(super) async fn replace_with_remote_events<Events, RoomData>(
        &mut self,
        events: Events,
        origin: RemoteEventOrigin,
        room_data_provider: &RoomData,
        settings: &TimelineSettings,
    ) where
        Events: IntoIterator,
        Events::Item: Into<TimelineEvent>,
        RoomData: RoomDataProvider,
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

    pub(super) fn transaction(&mut self) -> TimelineStateTransaction<'_> {
        TimelineStateTransaction::new(&mut self.items, &mut self.meta, self.timeline_focus.clone())
    }
}
