// Copyright 2025 The Matrix.org Foundation C.I.C.
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

//!  Facilities to handle incoming calls.

use ruma::{
    events::{
        rtc::decline::{RtcDeclineEventContent, SyncRtcDeclineEvent},
        AnySyncMessageLikeEvent, AnySyncTimelineEvent,
    },
    EventId, OwnedUserId, UserId,
};
use thiserror::Error;
use tokio::sync::broadcast;
use tracing::instrument;

use crate::{event_handler::EventHandlerDropGuard, room::EventSource, Room};

/// An error occurring while interacting with a call/rtc event.
#[derive(Debug, Error)]
pub enum CallError {
    /// We couldn't fetch the remote notification event.
    #[error("Couldn't fetch the remote event: {0}")]
    Fetch(Box<crate::Error>),

    /// We tried to decline an event wich is not of type m.rtc.notification.
    #[error("You cannot decline this event type.")]
    BadEventType,

    /// We tried to decline a call started by ourselves.
    #[error("You cannot decline your own call.")]
    DeclineOwnCall,

    /// We couldn't properly deserialize the target event.
    #[error(transparent)]
    Deserialize(#[from] serde_json::Error),
}

impl Room {
    /// Create a new decline call event for the target notification event id .
    ///
    /// The event can then be sent with [`Room::send`] or a
    /// [`crate::send_queue::RoomSendQueue`].
    #[instrument(skip(self), fields(room = %self.room_id()))]
    pub async fn make_decline_call_event(
        &self,
        notification_event_id: &EventId,
    ) -> Result<RtcDeclineEventContent, CallError> {
        make_call_decline_event(self, self.own_user_id(), notification_event_id).await
    }

    /// Subscribe to decline call evvent for this room.
    ///
    /// The returned receiver will receive the sender UserID for each decline
    /// for the matching notify event.
    pub fn subscribe_to_call_decline_events(
        &self,
        notification_event_id: &EventId,
    ) -> (EventHandlerDropGuard, broadcast::Receiver<OwnedUserId>) {
        let (sender, receiver) = broadcast::channel(16);

        let decline_call_event_handler_handle =
            self.client.add_room_event_handler(self.room_id(), {
                let own_notification_event_id = notification_event_id.to_owned();
                move |event: SyncRtcDeclineEvent| async move {
                    // Ignore decline for other unrelated notification events.
                    if let Some(declined_event_id) =
                        event.as_original().map(|ev| ev.content.relates_to.event_id.clone())
                    {
                        if declined_event_id == own_notification_event_id {
                            let _ = sender.send(event.sender().to_owned());
                        }
                    }
                }
            });
        let drop_guard = self.client().event_handler_drop_guard(decline_call_event_handler_handle);
        (drop_guard, receiver)
    }
}

async fn make_call_decline_event<S: EventSource>(
    source: S,
    own_user_id: &UserId,
    notification_event_id: &EventId,
) -> Result<RtcDeclineEventContent, CallError> {
    let target = source
        .get_event(notification_event_id)
        .await
        .map_err(|err| CallError::Fetch(Box::new(err)))?;

    let event = target.raw().deserialize().map_err(CallError::Deserialize)?;

    // The event must be CallNotify-like.
    let AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::CallNotify(notify)) = event
    else {
        return Err(CallError::BadEventType);
    };

    if notify.sender() == own_user_id {
        return Err(CallError::DeclineOwnCall);
    }

    Ok(RtcDeclineEventContent::new(notification_event_id))
}
