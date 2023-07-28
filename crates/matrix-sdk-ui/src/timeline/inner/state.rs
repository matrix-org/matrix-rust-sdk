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

use std::{collections::HashMap, sync::Arc};

use eyeball_im::ObservableVector;
use indexmap::IndexMap;
use matrix_sdk::{deserialized_responses::SyncTimelineEvent, sync::Timeline};
use ruma::{
    events::receipt::{Receipt, ReceiptType},
    push::Action,
    OwnedEventId, OwnedUserId, RoomVersionId,
};
use tracing::{debug, instrument, trace, warn};

use super::{ReactionState, TimelineInnerSettings};
use crate::{
    events::SyncTimelineEventWithoutContent,
    timeline::{
        event_handler::{
            update_read_marker, Flow, HandleEventResult, TimelineEventHandler, TimelineEventKind,
            TimelineEventMetadata, TimelineItemPosition,
        },
        reactions::Reactions,
        traits::RoomDataProvider,
        AnnotationKey, TimelineItem,
    },
};

#[derive(Debug)]
pub(in crate::timeline) struct TimelineInnerState {
    pub items: ObservableVector<Arc<TimelineItem>>,
    pub next_internal_id: u64,
    pub reactions: Reactions,
    pub fully_read_event: Option<OwnedEventId>,
    /// Whether the fully-read marker item should try to be updated when an
    /// event is added.
    /// This is currently `true` in two cases:
    /// - The fully-read marker points to an event that is not in the timeline,
    /// - The fully-read marker item would be the last item in the timeline.
    pub event_should_update_fully_read_marker: bool,
    /// User ID => Receipt type => Read receipt of the user of the given
    /// type.
    pub users_read_receipts: HashMap<OwnedUserId, HashMap<ReceiptType, (OwnedEventId, Receipt)>>,
    /// the local reaction request state that is queued next
    pub reaction_state: IndexMap<AnnotationKey, ReactionState>,
    /// the in flight reaction request state that is ongoing
    pub in_flight_reaction: IndexMap<AnnotationKey, ReactionState>,
    pub room_version: RoomVersionId,
}

impl TimelineInnerState {
    pub(super) fn new(room_version: RoomVersionId) -> Self {
        Self {
            items: Default::default(),
            next_internal_id: Default::default(),
            reactions: Default::default(),
            fully_read_event: Default::default(),
            event_should_update_fully_read_marker: Default::default(),
            users_read_receipts: Default::default(),
            reaction_state: Default::default(),
            in_flight_reaction: Default::default(),
            room_version,
        }
    }

    #[instrument(skip_all)]
    pub async fn handle_sync_timeline<P: RoomDataProvider>(
        &mut self,
        timeline: Timeline,
        room_data_provider: &P,
        settings: &TimelineInnerSettings,
    ) {
        if timeline.limited {
            debug!("Got limited sync response, resetting timeline");
            self.clear();
        }

        let num_events = timeline.events.len();
        for (i, event) in timeline.events.into_iter().enumerate() {
            trace!("Handling event {i} out of {num_events}");
            self.handle_live_event(event, room_data_provider, settings).await;
        }
    }

    /// Handle a live remote event.
    ///
    /// Shorthand for `handle_remote_event` with a `position` of
    /// `TimelineItemPosition::End { from_cache: false }`.
    pub(super) async fn handle_live_event<P: RoomDataProvider>(
        &mut self,
        event: SyncTimelineEvent,
        room_data_provider: &P,
        settings: &TimelineInnerSettings,
    ) -> HandleEventResult {
        self.handle_remote_event(
            event,
            TimelineItemPosition::End { from_cache: false },
            room_data_provider,
            settings,
        )
        .await
    }

    /// Handle a remote event.
    ///
    /// Returns the number of timeline updates that were made.
    pub(super) async fn handle_remote_event<P: RoomDataProvider>(
        &mut self,
        event: SyncTimelineEvent,
        position: TimelineItemPosition,
        room_data_provider: &P,
        settings: &TimelineInnerSettings,
    ) -> HandleEventResult {
        let should_add_event = &*settings.event_filter;
        let raw = event.event;
        let (event_id, sender, timestamp, txn_id, event_kind, should_add) = match raw.deserialize()
        {
            Ok(event) => {
                let should_add = should_add_event(&event);
                (
                    event.event_id().to_owned(),
                    event.sender().to_owned(),
                    event.origin_server_ts(),
                    event.transaction_id().map(ToOwned::to_owned),
                    event.into(),
                    should_add,
                )
            }
            Err(e) => match raw.deserialize_as::<SyncTimelineEventWithoutContent>() {
                Ok(event) if settings.add_failed_to_parse => (
                    event.event_id().to_owned(),
                    event.sender().to_owned(),
                    event.origin_server_ts(),
                    event.transaction_id().map(ToOwned::to_owned),
                    TimelineEventKind::failed_to_parse(event, e),
                    true,
                ),
                Ok(event) => {
                    let event_type = event.event_type();
                    let event_id = event.event_id();
                    warn!(%event_type, %event_id, "Failed to deserialize timeline event: {e}");
                    return HandleEventResult::default();
                }
                Err(e) => {
                    let event_type: Option<String> = raw.get_field("type").ok().flatten();
                    let event_id: Option<String> = raw.get_field("event_id").ok().flatten();
                    warn!(event_type, event_id, "Failed to deserialize timeline event: {e}");
                    return HandleEventResult::default();
                }
            },
        };

        let is_own_event = sender == room_data_provider.own_user_id();
        let encryption_info = event.encryption_info;
        let sender_profile = room_data_provider.profile(&sender).await;
        let read_receipts = if settings.track_read_receipts {
            self.load_read_receipts_for_event(&event_id, room_data_provider).await
        } else {
            Default::default()
        };
        let is_highlighted = event.push_actions.iter().any(Action::is_highlight);
        let event_meta = TimelineEventMetadata {
            sender,
            sender_profile,
            timestamp,
            is_own_event,
            encryption_info,
            read_receipts,
            is_highlighted,
        };
        let flow = Flow::Remote { event_id, raw_event: raw, txn_id, position, should_add };

        TimelineEventHandler::new(event_meta, flow, self, settings.track_read_receipts)
            .handle_event(event_kind)
    }

    pub(super) fn clear(&mut self) {
        self.items.clear();
        self.reactions.clear();
        self.fully_read_event = None;
        self.event_should_update_fully_read_marker = false;
    }

    #[instrument(skip_all)]
    pub(super) fn set_fully_read_event(&mut self, fully_read_event_id: OwnedEventId) {
        // A similar event has been handled already. We can ignore it.
        if self.fully_read_event.as_ref().is_some_and(|id| *id == fully_read_event_id) {
            return;
        }

        self.fully_read_event = Some(fully_read_event_id);

        update_read_marker(
            &mut self.items,
            self.fully_read_event.as_deref(),
            &mut self.event_should_update_fully_read_marker,
        );
    }
}
