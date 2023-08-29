use ruma::events::{MessageLikeEventType, StateEventType, TimelineEventType};
use serde::{de::Error, Deserialize};
use tracing::warn;

use super::messages::from_widget::SendEventRequest;

/// Different kinds of filter that could be applied to the timeline events.
#[derive(Debug, Clone)]
pub enum EventFilter {
    /// Message-like events.
    MessageLike(FilterScope<MessageFilterDescriptor>),
    /// State events.
    State(FilterScope<StateFilterDescriptor>),
}
impl EventFilter {
    pub(super) fn matches(&self, matrix_event: &MatrixEventFilterInput) -> bool {
        match (self, &matrix_event.state_key) {
            // message-like filter, message-like event
            (EventFilter::MessageLike(message_filter), None) => match message_filter {
                FilterScope::All => true,
                FilterScope::Custom(filter) => {
                    if matrix_event.event_type != TimelineEventType::from(filter.event_type.clone())
                    {
                        return false;
                    }

                    if filter.event_type != MessageLikeEventType::RoomMessage {
                        if filter.msgtype.is_some() {
                            warn!(
                                "Invalid filter: msgtype specified for a non-room-message filter"
                            );
                            return false;
                        }
                        return true;
                    }

                    match (filter.msgtype.clone(), matrix_event.content.msgtype.clone()) {
                        (Some(a), Some(b)) => a == b,
                        (Some(_), None) => false,
                        (None, Some(_)) | (None, None) => true,
                    }
                }
            },

            // state filter, state event
            (EventFilter::State(state_filter), Some(state_key)) => match state_filter {
                FilterScope::All => true,
                FilterScope::Custom(filter) => {
                    matrix_event.event_type == TimelineEventType::from(filter.event_type.clone())
                        && filter
                            .state_key
                            .as_ref()
                            .map_or(true, |filter_state_key| filter_state_key == state_key)
                }
            },

            // something else
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub enum FilterScope<T: FilterDescriptor> {
    All,
    Custom(T),
}
impl<T: FilterDescriptor> FilterScope<T> {
    /// Convert a `FilterScope` into an extension string.
    /// Example:
    /// ```
    /// FilterScope::Custom(
    ///     StateFilterDescriptor{
    ///         event_type:"m.room.member",
    ///         state_key: Some("@username:server.domain")
    ///     }
    /// )
    /// ```
    /// has the extension `m.room.member#@username:server.domain`
    pub(crate) fn get_ext(&self) -> String {
        match self {
            FilterScope::All => "".to_owned(),
            FilterScope::Custom(descriptor) => {
                let (p, s) = descriptor.get_prefix_suffix();
                let s = s.map(|s| format!("#{}", s)).unwrap_or("".to_owned());
                format!("{}{}", p, s)
            }
        }
    }

    /// Creates a `FilterScope` from an extension string.
    /// Example:
    /// `org.matrix.msc2762.m.send.event:m.room.message#m.text` has the extension `m.room.message#m.text`
    pub(crate) fn from_ext(ext: &str) -> Result<Self, serde_json::Error> {
        match ext.clone().split("#").collect::<Vec<_>>().as_slice() {
            [""] => Ok(FilterScope::All),
            [perfix] => Ok(FilterScope::Custom(T::from_prefix_suffix((*perfix).to_owned(), None))),
            [perfix, suffix] => Ok(FilterScope::Custom(T::from_prefix_suffix(
                (*perfix).to_owned(),
                Some((*suffix).to_owned()),
            ))),
            _ => Err(serde_json::Error::custom("could not parse capability")),
        }
    }
}
pub trait FilterDescriptor {
    fn get_prefix_suffix(&self) ->  (String, Option<String>);
    fn from_prefix_suffix(prefix: String, suffix: Option<String>) -> Self;
}
#[derive(Debug, Clone)]
pub struct StateFilterDescriptor {
    /// The type of the state event.
    event_type: StateEventType,
    /// State key that could be `None`, `None` means "any state key".
    state_key: Option<String>,
}
impl FilterDescriptor for StateFilterDescriptor {
    fn get_prefix_suffix(&self) -> (String, Option<String>) {
        (self.event_type.to_string(), self.state_key.as_ref().map(|s|s.clone()))
    }
    fn from_prefix_suffix(prefix: String, suffix: Option<String>) -> Self {
        StateFilterDescriptor { event_type: prefix.into(), state_key: suffix }
    }
}
#[derive(Debug, Clone)]
pub struct MessageFilterDescriptor {
    /// The type of the message-like event.
    event_type: MessageLikeEventType,
    /// Additional filter for the msgtype, currently only used for
    /// `m.room.message`. `None` means "any msgtype"
    msgtype: Option<String>,
}
impl FilterDescriptor for MessageFilterDescriptor {
    fn get_prefix_suffix(&self) ->  (String, Option<String>) {
        (self.event_type.to_string(), self.msgtype.as_ref().map(|s|s.clone()))
    }
    fn from_prefix_suffix(prefix: String, suffix: Option<String>) -> Self {
        MessageFilterDescriptor { event_type: prefix.into(), msgtype: suffix }
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct MatrixEventFilterInput {
    #[serde(rename = "type")]
    pub(super) event_type: TimelineEventType,
    pub(super) state_key: Option<String>,
    pub(super) content: MatrixEventContent,
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct MatrixEventContent {
    pub(super) msgtype: Option<String>,
}

impl MatrixEventFilterInput {
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
