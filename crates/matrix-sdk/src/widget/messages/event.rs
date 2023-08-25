use ruma::events::{MessageLikeEventType, StateEventType, TimelineEventType};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::widget::EventFilter;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MatrixEvent {
    #[serde(flatten)]
    pub event_type: EventType,
    pub sender: String,
    pub event_id: String,
    pub room_id: String,
    pub origin_server_ts: u32,
    pub unsigned: Unsigned,
    // TODO: Really not sure about this one, I feel like it should be a `AnyTimelineEvent` or
    // something like this. But @toger5 implemented it with this type originally, so let's
    // clarify it better.
    pub content: Value,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Unsigned {
    age: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)]
pub enum EventType {
    /// Message-like events.
    MessageLike {
        /// The type of the message-like event.
        #[serde(rename = "type")]
        event_type: MessageLikeEventType,
        /// An optional `msgtype` of the event.
        msgtype: Option<String>,
    },
    /// State events.
    State {
        /// The type of the state event.
        #[serde(rename = "type")]
        event_type: StateEventType,
        /// State key.
        state_key: String,
    },
}

impl EventType {
    pub fn event_type(&self) -> TimelineEventType {
        match self {
            EventType::MessageLike { event_type, .. } => event_type.clone().into(),
            EventType::State { event_type, .. } => event_type.clone().into(),
        }
    }
}

impl EventType {
    pub fn matches(&self, filter: &EventFilter) -> bool {
        match (self, filter) {
            (
                EventType::MessageLike { event_type, msgtype },
                EventFilter::MessageLike { event_type: filter_type, msgtype: filter_msgtype },
            ) => {
                event_type == filter_type
                    && msgtype.as_ref().zip(filter_msgtype.as_ref()).map_or(true, |(a, b)| a == b)
            }
            (
                EventType::State { event_type, state_key },
                EventFilter::State { event_type: filter_type, state_key: filter_key },
            ) => {
                event_type == filter_type
                    && filter_key
                        .as_ref()
                        .map_or(true, |filter_state_key| filter_state_key == state_key)
            }
            _ => false,
        }
    }
}
