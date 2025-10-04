// Copyright 2024 The Matrix.org Foundation C.I.C.
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

use imbl::Vector;
use matrix_sdk::deserialized_responses::TimelineEvent;
use ruma::{MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId};
use tracing::{debug, instrument, warn};

use super::TimelineItemContent;
use crate::timeline::{
    Error as TimelineError, MsgLikeContent, MsgLikeKind, PollState, TimelineEventItemId,
    TimelineItem,
    controller::TimelineMetadata,
    event_handler::{HandleAggregationKind, TimelineAction},
    event_item::{EventTimelineItem, Profile, TimelineDetails},
    traits::RoomDataProvider,
};

/// Details about an event being replied to.
#[derive(Clone, Debug)]
pub struct InReplyToDetails {
    /// The ID of the event.
    pub event_id: OwnedEventId,

    /// The details of the event.
    ///
    /// Use [`Timeline::fetch_details_for_event`] to fetch the data if it is
    /// unavailable.
    ///
    /// [`Timeline::fetch_details_for_event`]: crate::Timeline::fetch_details_for_event
    pub event: TimelineDetails<Box<EmbeddedEvent>>,
}

impl InReplyToDetails {
    pub fn new(
        event_id: OwnedEventId,
        timeline_items: &Vector<Arc<TimelineItem>>,
    ) -> InReplyToDetails {
        let event = timeline_items
            .iter()
            .filter_map(|it| it.as_event())
            .find(|it| it.event_id() == Some(&*event_id))
            .map(|item| Box::new(EmbeddedEvent::from_timeline_item(item)));

        InReplyToDetails { event_id, event: TimelineDetails::from_initial_value(event) }
    }
}

/// An event that is embedded in another event, such as a replied-to event, or a
/// thread latest event.
#[derive(Clone, Debug)]
pub struct EmbeddedEvent {
    /// The content of the embedded item.
    pub content: TimelineItemContent,
    /// The user ID of the sender of the related embedded event.
    pub sender: OwnedUserId,
    /// The profile of the sender of the related embedded event.
    pub sender_profile: TimelineDetails<Profile>,
    /// The timestamp of the event.
    pub timestamp: MilliSecondsSinceUnixEpoch,
    /// The unique identifier of this event.
    ///
    /// This is the transaction ID for a local echo that has not been sent and
    /// the event ID for a local echo that has been sent or a remote event.
    pub identifier: TimelineEventItemId,
}

impl EmbeddedEvent {
    /// Create a [`EmbeddedEvent`] from a loaded event timeline item.
    pub fn from_timeline_item(timeline_item: &EventTimelineItem) -> Self {
        Self {
            content: timeline_item.content.clone(),
            sender: timeline_item.sender.clone(),
            sender_profile: timeline_item.sender_profile.clone(),
            timestamp: timeline_item.timestamp,
            identifier: timeline_item.identifier(),
        }
    }

    #[instrument(skip_all)]
    pub(in crate::timeline) async fn try_from_timeline_event<P: RoomDataProvider>(
        timeline_event: TimelineEvent,
        room_data_provider: &P,
        meta: &TimelineMetadata,
    ) -> Result<Option<Self>, TimelineError> {
        let (raw_event, unable_to_decrypt_info) = match timeline_event.kind {
            matrix_sdk::deserialized_responses::TimelineEventKind::UnableToDecrypt {
                utd_info,
                event,
            } => (event, Some(utd_info)),
            _ => (timeline_event.kind.into_raw(), None),
        };

        let event = match raw_event.deserialize() {
            Ok(event) => event,
            Err(err) => {
                warn!("can't get details, event couldn't be deserialized: {err}");
                return Err(TimelineError::UnsupportedEvent);
            }
        };

        debug!(event_type = %event.event_type(), "got deserialized event");

        // We don't need to fill relation information or process metadata for an
        // embedded reply.
        let in_reply_to = None;
        let thread_root = None;
        let thread_summary = None;

        let sender = event.sender().to_owned();
        let timestamp = event.origin_server_ts();
        let identifier = TimelineEventItemId::EventId(event.event_id().to_owned());
        let action = TimelineAction::from_event(
            event,
            &raw_event,
            room_data_provider,
            unable_to_decrypt_info.map(|utd_info| (utd_info, meta.unable_to_decrypt_hook.as_ref())),
            in_reply_to,
            thread_root,
            thread_summary,
        )
        .await;

        match action {
            Some(TimelineAction::AddItem { content }) => {
                let sender_profile = TimelineDetails::from_initial_value(
                    room_data_provider.profile_from_user_id(&sender).await,
                );
                Ok(Some(Self { content, sender, sender_profile, timestamp, identifier }))
            }

            Some(TimelineAction::HandleAggregation { kind, .. }) => {
                // As an exception, edits are allowed to be embedded events.

                // For an embedded event, we don't need to fill a few fields; it's in an
                // embedded view context, so there's no strong need to show all detailed
                // information about it.
                let reactions = Default::default();
                let thread_root = None;
                let in_reply_to = None;
                let thread_summary = None;

                let content = match kind {
                    HandleAggregationKind::Edit { replacement } => {
                        let msg = replacement.new_content;
                        Some(TimelineItemContent::message(
                            msg.msgtype,
                            msg.mentions,
                            reactions,
                            thread_root,
                            in_reply_to,
                            thread_summary,
                        ))
                    }

                    HandleAggregationKind::PollEdit { replacement } => {
                        let msg = replacement.new_content;
                        let poll_state = PollState::new(msg.poll_start, msg.text);
                        Some(TimelineItemContent::MsgLike(MsgLikeContent {
                            kind: MsgLikeKind::Poll(poll_state),
                            reactions: Default::default(),
                            thread_root,
                            in_reply_to,
                            thread_summary,
                        }))
                    }

                    _ => {
                        // The event can't be represented as a standalone timeline item.
                        warn!("embedded event is an aggregation: {}", kind.debug_string());
                        None
                    }
                };

                if let Some(content) = content {
                    let sender_profile = TimelineDetails::from_initial_value(
                        room_data_provider.profile_from_user_id(&sender).await,
                    );
                    Ok(Some(Self { content, sender, sender_profile, timestamp, identifier }))
                } else {
                    Ok(None)
                }
            }

            None => {
                warn!("embedded event lead to no action (neither an aggregation nor a new item)");
                Ok(None)
            }
        }
    }
}
