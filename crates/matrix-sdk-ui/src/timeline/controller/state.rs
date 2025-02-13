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
    events::{
        poll::unstable_start::NewUnstablePollStartEventContentWithoutRelation,
        relation::Replacement, room::message::RoomMessageEventContentWithoutRelation,
        AnySyncEphemeralRoomEvent, AnySyncTimelineEvent,
    },
    serde::Raw,
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId,
    RoomVersionId, UserId,
};
use tracing::{instrument, trace, warn};

use super::{
    super::{
        date_dividers::DateDividerAdjuster,
        event_handler::{
            Flow, TimelineEventContext, TimelineEventHandler, TimelineEventKind,
            TimelineItemPosition,
        },
        event_item::RemoteEventOrigin,
        traits::RoomDataProvider,
        Profile, TimelineItem,
    },
    metadata::EventMeta,
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
        should_add_new_items: bool,
        date_divider_mode: DateDividerMode,
        txn_id: OwnedTransactionId,
        send_handle: Option<SendHandle>,
        content: TimelineEventKind,
    ) {
        let ctx = TimelineEventContext {
            sender: own_user_id,
            sender_profile: own_profile,
            timestamp: MilliSecondsSinceUnixEpoch::now(),
            is_own_event: true,
            read_receipts: Default::default(),
            // An event sent by ourselves is never matched against push rules.
            is_highlighted: false,
            flow: Flow::Local { txn_id, send_handle },
            should_add_new_items,
        };

        let mut txn = self.transaction();

        let mut date_divider_adjuster = DateDividerAdjuster::new(date_divider_mode);

        TimelineEventHandler::new(&mut txn, ctx)
            .handle_event(&mut date_divider_adjuster, content)
            .await;

        txn.adjust_date_dividers(date_divider_adjuster);

        txn.commit();
    }

    pub(super) async fn retry_event_decryption<P: RoomDataProvider, Fut>(
        &mut self,
        retry_one: impl Fn(Arc<TimelineItem>) -> Fut,
        retry_indices: Vec<usize>,
        push_rules_context: Option<(ruma::push::Ruleset, ruma::push::PushConditionRoomCtx)>,
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
            let Some(mut event) = retry_one(txn.items[idx].clone()).await else {
                continue;
            };

            event.push_actions = push_rules_context.as_ref().map(|(push_rules, push_context)| {
                push_rules.get_actions(event.raw(), push_context).to_owned()
            });

            let handle_one_res = txn
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
            if handle_one_res.item_removed {
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
        own_user_id: &UserId,
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
        TimelineStateTransaction::new(&mut self.items, &mut self.meta, self.timeline_focus)
    }
}

#[derive(Clone)]
pub(in crate::timeline) enum PendingEditKind {
    RoomMessage(Replacement<RoomMessageEventContentWithoutRelation>),
    Poll(Replacement<NewUnstablePollStartEventContentWithoutRelation>),
}

#[derive(Clone)]
pub(in crate::timeline) struct PendingEdit {
    pub kind: PendingEditKind,
    pub event_json: Raw<AnySyncTimelineEvent>,
}

impl PendingEdit {
    pub fn edited_event(&self) -> &EventId {
        match &self.kind {
            PendingEditKind::RoomMessage(Replacement { event_id, .. })
            | PendingEditKind::Poll(Replacement { event_id, .. }) => event_id,
        }
    }
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for PendingEdit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            PendingEditKind::RoomMessage(_) => {
                f.debug_struct("RoomMessage").finish_non_exhaustive()
            }
            PendingEditKind::Poll(_) => f.debug_struct("Poll").finish_non_exhaustive(),
        }
    }
}

/// Full metadata about an event.
///
/// Only used to group function parameters.
pub(crate) struct FullEventMeta<'a> {
    /// The ID of the event.
    pub event_id: &'a EventId,
    /// Whether the event is among the timeline items.
    pub visible: bool,
    /// The sender of the event.
    pub sender: Option<&'a UserId>,
    /// Whether this event was sent by our own user.
    pub is_own_event: bool,
    /// The timestamp of the event.
    pub timestamp: Option<MilliSecondsSinceUnixEpoch>,
}

impl FullEventMeta<'_> {
    pub(super) fn base_meta(&self) -> EventMeta {
        EventMeta {
            event_id: self.event_id.to_owned(),
            visible: self.visible,
            timeline_item_index: None,
        }
    }
}
