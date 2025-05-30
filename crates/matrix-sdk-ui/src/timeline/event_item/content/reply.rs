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
use matrix_sdk::deserialized_responses::{TimelineEvent, UnsignedEventLocation};
use ruma::{OwnedEventId, OwnedUserId, UserId};
use tracing::{debug, instrument, warn};

use super::TimelineItemContent;
use crate::timeline::{
    controller::{TimelineFocusKind, TimelineMetadata},
    event_handler::TimelineAction,
    event_item::{EventTimelineItem, Profile, TimelineDetails},
    traits::RoomDataProvider,
    Error as TimelineError, TimelineItem,
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
    pub event: TimelineDetails<Box<RepliedToEvent>>,
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
            .map(|item| Box::new(RepliedToEvent::from_timeline_item(item)));

        InReplyToDetails { event_id, event: TimelineDetails::from_initial_value(event) }
    }
}

/// An event that is replied to.
#[derive(Clone, Debug)]
pub struct RepliedToEvent {
    content: TimelineItemContent,
    sender: OwnedUserId,
    sender_profile: TimelineDetails<Profile>,
}

impl RepliedToEvent {
    /// Get the message of this event.
    pub fn content(&self) -> &TimelineItemContent {
        &self.content
    }

    /// Get the sender of this event.
    pub fn sender(&self) -> &UserId {
        &self.sender
    }

    /// Get the profile of the sender.
    pub fn sender_profile(&self) -> &TimelineDetails<Profile> {
        &self.sender_profile
    }

    /// Create a [`RepliedToEvent`] from a loaded event timeline item.
    pub fn from_timeline_item(timeline_item: &EventTimelineItem) -> Self {
        Self {
            content: timeline_item.content.clone(),
            sender: timeline_item.sender.clone(),
            sender_profile: timeline_item.sender_profile.clone(),
        }
    }

    #[instrument(skip_all)]
    pub(in crate::timeline) async fn try_from_timeline_event<P: RoomDataProvider>(
        timeline_event: TimelineEvent,
        room_data_provider: &P,
        timeline_items: &Vector<Arc<TimelineItem>>,
        meta: &mut TimelineMetadata,
    ) -> Result<Option<Self>, TimelineError> {
        let bundled_edit_encryption_info =
            timeline_event.kind.unsigned_encryption_map().and_then(|map| {
                map.get(&UnsignedEventLocation::RelationsReplace)?.encryption_info().cloned()
            });

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

        let sender = event.sender().to_owned();
        let action = TimelineAction::from_event(
            event,
            &raw_event,
            room_data_provider,
            unable_to_decrypt_info,
            bundled_edit_encryption_info,
            timeline_items,
            meta,
            &TimelineFocusKind::Live, // BRILLIANT!
        )
        .await;

        let Some(TimelineAction::AddItem { content }) = action else {
            // The event can't be represented as a standalone timeline item.
            return Ok(None);
        };

        let sender_profile = TimelineDetails::from_initial_value(
            room_data_provider.profile_from_user_id(&sender).await,
        );

        Ok(Some(Self { content, sender, sender_profile }))
    }
}
