use ruma::events::{MessageLikeEventType, StateEventType, TimelineEventType};
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::messages::from_widget::SendEventRequest;

/// Different kinds of filters that could be applied to the timeline events.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub enum EventFilter {
    /// Message-like events.
    MessageLike {
        /// The type of the message-like event.
        event_type: MessageLikeEventType,
        /// Additional filter for the msgtype, currently only used for
        /// `m.room.message`.
        msgtype: Option<String>,
    },
    /// State events.
    State {
        /// The type of the state event.
        event_type: StateEventType,
        /// State key that could be `None`, `None` means "any state key".
        state_key: Option<String>,
    },
}

// TODO: all-message-like and all-state?

impl EventFilter {
    pub(super) fn matches(&self, de_helper: &EventDeHelperForFilter) -> bool {
        match (self, &de_helper.state_key) {
            // message-like filter, message-like event
            (
                EventFilter::MessageLike { event_type: filter_type, msgtype: filter_msgtype },
                None,
            ) => {
                if de_helper.event_type != TimelineEventType::from(filter_type.clone()) {
                    return false;
                }

                if *filter_type != MessageLikeEventType::RoomMessage {
                    if filter_msgtype.is_some() {
                        warn!("Invalid filter: msgtype specified for a non-room-message filter");
                        return false;
                    }
                    return true;
                }

                match (filter_msgtype, &de_helper.content.msgtype) {
                    (Some(a), Some(b)) => a == b,
                    (Some(_), None) => false,
                    (None, Some(_)) | (None, None) => true,
                }
            }

            // state filter, state event
            (
                EventFilter::State { event_type: filter_type, state_key: filter_key },
                Some(state_key),
            ) => {
                de_helper.event_type == TimelineEventType::from(filter_type.clone())
                    && filter_key
                        .as_ref()
                        .map_or(true, |filter_state_key| filter_state_key == state_key)
            }

            // something else
            _ => false,
        }
    }
}

// TODO: Custom (de)serialize impls for EventFilter

#[derive(Debug, Deserialize)]
pub(super) struct EventDeHelperForFilter {
    #[serde(rename = "type")]
    pub(super) event_type: TimelineEventType,
    pub(super) state_key: Option<String>,
    pub(super) content: MatrixEventContent,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct MatrixEventContent {
    pub(super) msgtype: Option<String>,
}

impl EventDeHelperForFilter {
    pub(super) fn from_send_event_request(req: SendEventRequest) -> Self {
        let SendEventRequest { event_type, state_key, content } = req;
        Self {
            event_type,
            state_key,
            // If content fails to deserialize (msgtype is not a string),
            // pretend that there is no msgtype as far as filters are concerned
            content: serde_json::from_value(content).unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    // TODO: Write tests for EventFilter::matches
}
