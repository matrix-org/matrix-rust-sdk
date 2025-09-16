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

use ruma::{
    events::{
        AnyMessageLikeEvent, AnyStateEvent, AnyTimelineEvent, AnyToDeviceEvent,
        MessageLikeEventType, StateEventType, ToDeviceEventType,
    },
    serde::{JsonCastable, Raw},
};
use serde::Deserialize;
use tracing::debug;

use super::machine::{SendEventRequest, SendToDeviceRequest};

/// A Filter for Matrix events. It is used to decide if a given event can be
/// sent to the widget and if a widget is allowed to send an event to a
/// Matrix room.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub enum Filter {
    /// Filter for message-like events.
    MessageLike(MessageLikeEventFilter),
    /// Filter for state events.
    State(StateEventFilter),
    /// Filter for to device events.
    ToDevice(ToDeviceEventFilter),
}

impl Filter {
    /// Checks if this filter matches with the given filter_input.
    /// A filter input can be create by using the `From` trait on FilterInput
    /// for [`Raw<AnyTimelineEvent>`] or [`SendEventRequest`].
    pub(super) fn matches(&self, filter_input: &FilterInput<'_>) -> bool {
        match self {
            Self::MessageLike(filter) => filter.matches(filter_input),
            Self::State(filter) => filter.matches(filter_input),
            Self::ToDevice(filter) => filter.matches(filter_input),
        }
    }
    /// Returns the event type that this filter is configured to match.
    ///
    /// This method provides a string representation of the event type
    /// associated with the filter.
    pub(super) fn filter_event_type(&self) -> String {
        match self {
            Self::MessageLike(filter) => filter.filter_event_type(),
            Self::State(filter) => filter.filter_event_type(),
            Self::ToDevice(filter) => filter.event_type.to_string(),
        }
    }
}

/// Filter for message-like events.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub enum MessageLikeEventFilter {
    /// Matches message-like events with the given `type`.
    WithType(MessageLikeEventType),
    /// Matches `m.room.message` events with the given `msgtype`.
    RoomMessageWithMsgtype(String),
}

impl<'a> MessageLikeEventFilter {
    fn matches(&self, filter_input: &FilterInput<'a>) -> bool {
        let FilterInput::MessageLike(message_like_filter_input) = filter_input else {
            return false;
        };
        match self {
            Self::WithType(filter_event_type) => {
                message_like_filter_input.event_type == filter_event_type.to_string()
            }
            Self::RoomMessageWithMsgtype(msgtype) => {
                message_like_filter_input.event_type == "m.room.message"
                    && message_like_filter_input.content.msgtype == Some(msgtype)
            }
        }
    }

    fn filter_event_type(&self) -> String {
        match self {
            Self::WithType(filter_event_type) => filter_event_type.to_string(),
            Self::RoomMessageWithMsgtype(_) => MessageLikeEventType::RoomMessage.to_string(),
        }
    }
}

/// Filter for state events.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub enum StateEventFilter {
    /// Matches state events with the given `type`, regardless of `state_key`.
    WithType(StateEventType),
    /// Matches state events with the given `type` and `state_key`.
    WithTypeAndStateKey(StateEventType, String),
}

impl<'a> StateEventFilter {
    fn matches(&self, filter_input: &FilterInput<'a>) -> bool {
        let FilterInput::State(state_filter_input) = filter_input else {
            return false;
        };

        match self {
            StateEventFilter::WithType(filter_type) => {
                state_filter_input.event_type == filter_type.to_string()
            }
            StateEventFilter::WithTypeAndStateKey(event_type, filter_state_key) => {
                state_filter_input.event_type == event_type.to_string()
                    && state_filter_input.state_key == *filter_state_key
            }
        }
    }
    fn filter_event_type(&self) -> String {
        match self {
            Self::WithType(filter_event_type) => filter_event_type.to_string(),
            Self::WithTypeAndStateKey(event_type, _) => event_type.to_string(),
        }
    }
}

/// Filter for to-device events.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub struct ToDeviceEventFilter {
    /// The event type this to-device-filter filters for.
    pub event_type: ToDeviceEventType,
}

impl ToDeviceEventFilter {
    /// Create a new `ToDeviceEventFilter` with the given event type.
    pub fn new(event_type: ToDeviceEventType) -> Self {
        Self { event_type }
    }

    fn matches(&self, filter_input: &FilterInput<'_>) -> bool {
        matches!(filter_input,FilterInput::ToDevice(f_in) if f_in.event_type == self.event_type.to_string())
    }
}

// Filter input:

/// The input data for the filter. This can either be constructed from a
/// [`Raw<AnyTimelineEvent>`] or a [`SendEventRequest`].
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum FilterInput<'a> {
    #[serde(borrow)]
    // The order is important.
    // We first need to check if we can deserialize as a state (state_key exists)
    State(FilterInputState<'a>),
    // only then we can check if we can deserialize as a message-like.
    MessageLike(FilterInputMessageLike<'a>),
    // ToDevice will need to be done explicitly since it looks the same as a message-like.
    ToDevice(FilterInputToDevice<'a>),
}

impl<'a> FilterInput<'a> {
    pub fn message_like(event_type: &'a str) -> Self {
        Self::MessageLike(FilterInputMessageLike {
            event_type,
            content: MessageLikeFilterEventContent { msgtype: None },
        })
    }

    pub(super) fn message_with_msgtype(msgtype: &'a str) -> Self {
        Self::MessageLike(FilterInputMessageLike {
            event_type: "m.room.message",
            content: MessageLikeFilterEventContent { msgtype: Some(msgtype) },
        })
    }

    pub fn state(event_type: &'a str, state_key: &'a str) -> Self {
        Self::State(FilterInputState { event_type, state_key })
    }
}

/// Filter input data that is used for a [`FilterInput::State`] filter.
#[derive(Debug, Deserialize)]
pub struct FilterInputState<'a> {
    #[serde(rename = "type")]
    // TODO: This wants to be `StateEventType` but we need a type which supports `as_str()`
    // as soon as ruma supports `as_str()` on `StateEventType` we can use it here.
    pub(super) event_type: &'a str,
    pub(super) state_key: &'a str,
}

// Filter input message like:
#[derive(Debug, Default, Deserialize)]
pub(super) struct MessageLikeFilterEventContent<'a> {
    #[serde(borrow)]
    pub(super) msgtype: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
pub struct FilterInputMessageLike<'a> {
    // TODO: This wants to be `StateEventType` but we need a type which supports `as_str()`
    // as soon as ruma supports `as_str()` on `StateEventType` we can use it here.
    #[serde(rename = "type")]
    pub(super) event_type: &'a str,
    pub(super) content: MessageLikeFilterEventContent<'a>,
}

/// Create a filter input based on [`AnyTimelineEvent`].
/// This will create a [`FilterInput::State`] or [`FilterInput::MessageLike`]
/// depending on the event type.
impl<'a> TryFrom<&'a Raw<AnyTimelineEvent>> for FilterInput<'a> {
    type Error = serde_json::Error;

    fn try_from(raw_event: &'a Raw<AnyTimelineEvent>) -> Result<Self, Self::Error> {
        // FilterInput first checks if it can deserialize as a state event (state_key
        // exists) and then as a message-like event.
        raw_event.deserialize_as()
    }
}

/// Create a filter input based on [`AnyStateEvent`].
/// This will create a [`FilterInput::State`].
impl<'a> TryFrom<&'a Raw<AnyStateEvent>> for FilterInput<'a> {
    type Error = serde_json::Error;

    fn try_from(raw_event: &'a Raw<AnyStateEvent>) -> Result<Self, Self::Error> {
        raw_event.deserialize_as()
    }
}

impl<'a> JsonCastable<FilterInput<'a>> for AnyTimelineEvent {}

impl<'a> JsonCastable<FilterInput<'a>> for AnyStateEvent {}

impl<'a> JsonCastable<FilterInput<'a>> for AnyMessageLikeEvent {}

#[derive(Debug, Deserialize)]
pub struct FilterInputToDevice<'a> {
    #[serde(rename = "type")]
    pub(super) event_type: &'a str,
}

/// Create a filter input of type [`FilterInput::ToDevice`]`.
impl<'a> TryFrom<&'a Raw<AnyToDeviceEvent>> for FilterInput<'a> {
    type Error = serde_json::Error;
    fn try_from(raw_event: &'a Raw<AnyToDeviceEvent>) -> Result<Self, Self::Error> {
        // deserialize_as::<FilterInput> will first try state, message-like and then
        // to-device. The `AnyToDeviceEvent` would match message like first, so
        // we need to explicitly deserialize as `FilterInputToDevice`.
        raw_event.deserialize_as::<FilterInputToDevice<'a>>().map(FilterInput::ToDevice)
    }
}

impl<'a> JsonCastable<FilterInputToDevice<'a>> for AnyToDeviceEvent {}

impl<'a> From<&'a SendToDeviceRequest> for FilterInput<'a> {
    fn from(request: &'a SendToDeviceRequest) -> Self {
        FilterInput::ToDevice(FilterInputToDevice { event_type: &request.event_type })
    }
}

impl<'a> From<&'a SendEventRequest> for FilterInput<'a> {
    fn from(request: &'a SendEventRequest) -> Self {
        match &request.state_key {
            None => match request.event_type.as_str() {
                "m.room.message" => {
                    if let Some(msgtype) =
                        serde_json::from_str::<MessageLikeFilterEventContent<'a>>(
                            request.content.get(),
                        )
                        .unwrap_or_else(|e| {
                            debug!("Failed to deserialize event content for filter: {e}");
                            // Fallback to empty content is safe.
                            // If we do have a filter matching any content type, it will match
                            // independent of the body.
                            // Any filter that does only match a specific content type will not
                            // match the empty content.
                            Default::default()
                        })
                        .msgtype
                    {
                        FilterInput::message_with_msgtype(msgtype)
                    } else {
                        FilterInput::message_like("m.room.message")
                    }
                }
                _ => FilterInput::message_like(&request.event_type),
            },
            Some(state_key) => FilterInput::state(&request.event_type, state_key),
        }
    }
}

#[cfg(test)]
mod tests {
    use ruma::{
        events::{AnyTimelineEvent, MessageLikeEventType, StateEventType, TimelineEventType},
        serde::Raw,
    };

    use super::{
        Filter, FilterInput, FilterInputMessageLike, MessageLikeEventFilter, StateEventFilter,
    };
    use crate::widget::filter::{
        FilterInputToDevice, MessageLikeFilterEventContent, ToDeviceEventFilter,
    };

    fn message_event(event_type: &str) -> FilterInput<'_> {
        FilterInput::MessageLike(FilterInputMessageLike { event_type, content: Default::default() })
    }

    // Tests against a `m.room.message` filter with `msgtype = m.text`
    fn room_message_text_event_filter() -> Filter {
        Filter::MessageLike(MessageLikeEventFilter::RoomMessageWithMsgtype("m.text".to_owned()))
    }

    #[test]
    fn test_text_event_filter_matches_text_event() {
        assert!(
            room_message_text_event_filter().matches(&FilterInput::message_with_msgtype("m.text")),
        );
    }

    #[test]
    fn test_text_event_filter_does_not_match_image_event() {
        assert!(
            !room_message_text_event_filter()
                .matches(&FilterInput::message_with_msgtype("m.image"))
        );
    }

    #[test]
    fn test_text_event_filter_does_not_match_custom_event_with_msgtype() {
        assert!(!room_message_text_event_filter().matches(&FilterInput::MessageLike(
            FilterInputMessageLike {
                event_type: "io.element.message",
                content: MessageLikeFilterEventContent { msgtype: Some("m.text") }
            }
        )));
    }

    // Tests against an `m.reaction` filter
    fn reaction_event_filter() -> Filter {
        Filter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::Reaction))
    }

    #[test]
    fn test_reaction_event_filter_matches_reaction() {
        assert!(
            reaction_event_filter()
                .matches(&message_event(&MessageLikeEventType::Reaction.to_string()))
        );
    }

    #[test]
    fn test_reaction_event_filter_does_not_match_room_message() {
        assert!(!reaction_event_filter().matches(&FilterInput::message_with_msgtype("m.text")));
    }

    #[test]
    fn test_reaction_event_filter_does_not_match_state_event_any_key() {
        assert!(!reaction_event_filter().matches(&FilterInput::state("m.reaction", "")));
    }

    // Tests against an `m.room.member` filter with `state_key = "@self:example.me"`
    fn self_member_event_filter() -> Filter {
        Filter::State(StateEventFilter::WithTypeAndStateKey(
            StateEventType::RoomMember,
            "@self:example.me".to_owned(),
        ))
    }

    #[test]
    fn test_self_member_event_filter_matches_self_member_event() {
        assert!(self_member_event_filter().matches(&FilterInput::state(
            &TimelineEventType::RoomMember.to_string(),
            "@self:example.me"
        )));
    }

    #[test]
    fn test_self_member_event_filter_does_not_match_somebody_elses_member_event() {
        assert!(!self_member_event_filter().matches(&FilterInput::state(
            &TimelineEventType::RoomMember.to_string(),
            "@somebody_else.example.me"
        )));
    }

    #[test]
    fn self_member_event_filter_does_not_match_unrelated_state_event_with_same_state_key() {
        assert!(
            !self_member_event_filter()
                .matches(&FilterInput::state("io.element.test_state_event", "@self.example.me"))
        );
    }

    #[test]
    fn test_self_member_event_filter_does_not_match_reaction_event() {
        assert!(
            !self_member_event_filter()
                .matches(&message_event(&MessageLikeEventType::Reaction.to_string()))
        );
    }

    #[test]
    fn test_self_member_event_filter_only_matches_specific_state_key() {
        assert!(
            !self_member_event_filter()
                .matches(&FilterInput::state(&StateEventType::RoomMember.to_string(), ""))
        );
    }

    // Tests against an `m.room.member` filter with any `state_key`.
    fn member_event_filter() -> Filter {
        Filter::State(StateEventFilter::WithType(StateEventType::RoomMember))
    }

    #[test]
    fn test_member_event_filter_matches_some_member_event() {
        assert!(member_event_filter().matches(&FilterInput::state(
            &TimelineEventType::RoomMember.to_string(),
            "@foo.bar.baz"
        )));
    }

    #[test]
    fn test_member_event_filter_does_not_match_room_name_event() {
        assert!(
            !member_event_filter()
                .matches(&FilterInput::state(&TimelineEventType::RoomName.to_string(), ""))
        );
    }

    #[test]
    fn test_member_event_filter_does_not_match_reaction_event() {
        assert!(
            !member_event_filter()
                .matches(&message_event(&MessageLikeEventType::Reaction.to_string()))
        );
    }

    #[test]
    fn test_member_event_filter_matches_any_state_key() {
        assert!(
            member_event_filter()
                .matches(&FilterInput::state(&StateEventType::RoomMember.to_string(), ""))
        );
    }

    // Tests against an `m.room.topic` filter with `state_key = ""`
    fn topic_event_filter() -> Filter {
        Filter::State(StateEventFilter::WithTypeAndStateKey(
            StateEventType::RoomTopic,
            "".to_owned(),
        ))
    }

    #[test]
    fn test_topic_event_filter_does_match() {
        assert!(
            topic_event_filter()
                .matches(&FilterInput::state(&StateEventType::RoomTopic.to_string(), ""))
        );
    }

    // Tests against an `m.room.message` filter with `msgtype = m.custom`
    fn room_message_custom_event_filter() -> Filter {
        Filter::MessageLike(MessageLikeEventFilter::RoomMessageWithMsgtype("m.custom".to_owned()))
    }

    // Tests against an `m.room.message` filter without a `msgtype`
    fn room_message_filter() -> Filter {
        Filter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::RoomMessage))
    }

    #[test]
    fn test_reaction_event_type_does_not_match_room_message_text_event_filter() {
        assert!(
            !room_message_text_event_filter()
                .matches(&FilterInput::message_like(&MessageLikeEventType::Reaction.to_string()))
        );
    }

    #[test]
    fn test_room_message_event_without_msgtype_does_not_match_custom_msgtype_filter() {
        assert!(
            !room_message_custom_event_filter().matches(&FilterInput::message_like(
                &MessageLikeEventType::RoomMessage.to_string()
            ))
        );
    }

    #[test]
    fn test_reaction_event_type_does_not_match_room_message_custom_event_filter() {
        assert!(
            !room_message_custom_event_filter()
                .matches(&FilterInput::message_like(&MessageLikeEventType::Reaction.to_string()))
        );
    }

    #[test]
    fn test_room_message_event_type_matches_room_message_event_filter() {
        assert!(
            room_message_filter().matches(&FilterInput::message_like(
                &MessageLikeEventType::RoomMessage.to_string()
            ))
        );
    }

    #[test]
    fn test_reaction_event_type_does_not_match_room_message_event_filter() {
        assert!(
            !room_message_filter()
                .matches(&FilterInput::message_like(&MessageLikeEventType::Reaction.to_string()))
        );
    }
    #[test]
    fn test_convert_raw_event_into_message_like_filter_input() {
        let raw_event = &Raw::<AnyTimelineEvent>::from_json_string(
            r#"{"type":"m.room.message","content":{"msgtype":"m.text"}}"#.to_owned(),
        )
        .unwrap();
        let filter_input: FilterInput<'_> =
            raw_event.try_into().expect("convert to FilterInput failed");
        assert!(matches!(filter_input, FilterInput::MessageLike(_)));
        if let FilterInput::MessageLike(message_like) = filter_input {
            assert_eq!(message_like.event_type, "m.room.message");
            assert_eq!(message_like.content.msgtype, Some("m.text"));
        }
    }
    #[test]
    fn test_convert_raw_event_into_state_filter_input() {
        let raw_event = &Raw::<AnyTimelineEvent>::from_json_string(
            r#"{"type":"m.room.member","state_key":"@alice:example.com"}"#.to_owned(),
        )
        .unwrap();
        let filter_input: FilterInput<'_> =
            raw_event.try_into().expect("convert to FilterInput failed");
        assert!(matches!(filter_input, FilterInput::State(_)));
        if let FilterInput::State(state) = filter_input {
            assert_eq!(state.event_type, "m.room.member");
            assert_eq!(state.state_key, "@alice:example.com");
        }
    }

    #[test]
    fn test_to_device_filter_does_match() {
        let f = Filter::ToDevice(ToDeviceEventFilter::new("my.custom.to.device".into()));
        assert!(f.matches(&FilterInput::ToDevice(FilterInputToDevice {
            event_type: "my.custom.to.device",
        })));
    }
}
