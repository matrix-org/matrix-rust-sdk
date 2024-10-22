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

#![allow(dead_code)] // temporary

use std::fmt;

use ruma::events::{MessageLikeEventType, StateEventType, TimelineEventType, ToDeviceEventType};
use serde::{Deserialize, Serialize};

/// Different kinds of filters for timeline events.

// Refactor this to only have two methods: matches_request(RequestFilterInput) and matches_event(MatrixEventFilterInput).
// or only one matches(FilterInput), enum FilterInput{Event(MatrixEventFilterInput), Request(RequestFilterInput)} and from impls
// for FilterInput...
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub enum EventFilter {
    /// Filter for message-like events.
    MessageLike(MessageLikeEventFilter),
    /// Filter for state events.
    State(StateEventFilter),
    /// Filter for to device events.
    ToDevice(ToDeviceEventFilter),
}

impl EventAndRequestFilter for EventFilter {
    fn matches_event(&self, matrix_event: &MatrixEventFilterInput) -> bool {
        let filter: &dyn EventAndRequestFilter = match self {
            EventFilter::MessageLike(filter) => filter,
            EventFilter::State(filter) => filter,
            EventFilter::ToDevice(filter) => filter,
        };
        filter.matches_event(matrix_event)
    }

    fn matches_request(&self, event_type: &FilterEventType) -> bool {
        let filter: &dyn EventAndRequestFilter = match self {
            EventFilter::MessageLike(filter) => filter,
            EventFilter::State(filter) => filter,
            EventFilter::ToDevice(filter) => filter,
        };
        filter.matches_request(event_type)
    }
}

pub trait EventAndRequestFilter {
    fn matches_event(&self, matrix_event: &MatrixEventFilterInput) -> bool;
    fn matches_request(&self, event_type: &FilterEventType) -> bool;
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

impl EventAndRequestFilter for MessageLikeEventFilter {
    fn matches_event(&self, matrix_event: &MatrixEventFilterInput) -> bool {
        let MatrixEventFilterInput::Timeline(matrix_event) = matrix_event else {
            return false;
        };
        if matrix_event.state_key.is_some() {
            // State event doesn't match a message-like event filter.
            return false;
        }
        if let FilterEventType::Timeline(event_type) = &matrix_event.event_type {
            match self {
                MessageLikeEventFilter::WithType(filter_event_type) => {
                    *event_type == TimelineEventType::from(filter_event_type.clone())
                }
                MessageLikeEventFilter::RoomMessageWithMsgtype(msgtype) => {
                    *event_type == TimelineEventType::RoomMessage
                        && matrix_event.content.msgtype.as_ref() == Some(msgtype)
                }
            }
        } else {
            false
        }
    }

    fn matches_request(&self, event_type: &FilterEventType) -> bool {
        let FilterEventType::Timeline(event_type) = event_type else {
            return false;
        };

        match self {
            MessageLikeEventFilter::WithType(filter_event_type) => {
                TimelineEventType::from(filter_event_type.clone()) == *event_type
            }
            MessageLikeEventFilter::RoomMessageWithMsgtype(_) => {
                &TimelineEventType::RoomMessage == event_type
            }
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

impl EventAndRequestFilter for StateEventFilter {
    fn matches_event(&self, matrix_event: &MatrixEventFilterInput) -> bool {
        let MatrixEventFilterInput::Timeline(matrix_event) = matrix_event else {
            return false;
        };
        let Some(state_key) = &matrix_event.state_key else {
            // Message-like event doesn't match a state event filter.
            return false;
        };

        match self {
            StateEventFilter::WithType(event_type) => {
                matrix_event.event_type
                    == FilterEventType::Timeline(TimelineEventType::from(event_type.clone()))
            }
            StateEventFilter::WithTypeAndStateKey(event_type, filter_state_key) => {
                matrix_event.event_type
                    == FilterEventType::Timeline(TimelineEventType::from(event_type.clone()))
                    && state_key == filter_state_key
            }
        }
    }

    fn matches_request(&self, event_type: &FilterEventType) -> bool {
        matches!(self, Self::WithType(ty) if FilterEventType::Timeline(TimelineEventType::from(ty.clone())) == *event_type)
    }
}

/// Filter for to-device events.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq))]

pub struct ToDeviceEventFilter(pub ToDeviceEventType);

impl EventAndRequestFilter for ToDeviceEventFilter {
    fn matches_event(&self, matrix_event: &MatrixEventFilterInput) -> bool {
        let MatrixEventFilterInput::ToDevice(matrix_event) = matrix_event else {
            return false;
        };
        match self {
            ToDeviceEventFilter(event_type) => {
                matrix_event.event_type == FilterEventType::ToDevice(event_type.clone())
            }
        }
    }

    fn matches_request(&self, _: &FilterEventType) -> bool {
        // There is no way to request events. We will only need to run checks on sending and receiving already existing
        // events.
        false
    }
}

impl fmt::Display for ToDeviceEventFilter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{0}", self.0)
    }
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
#[serde(untagged)]
pub enum FilterEventType {
    Timeline(TimelineEventType),
    ToDevice(ToDeviceEventType),
}

impl From<TimelineEventType> for FilterEventType {
    fn from(value: TimelineEventType) -> Self {
        Self::Timeline(value)
    }
}

impl From<ToDeviceEventType> for FilterEventType {
    fn from(value: ToDeviceEventType) -> Self {
        Self::ToDevice(value)
    }
}
impl From<StateEventType> for FilterEventType {
    fn from(value: StateEventType) -> Self {
        TimelineEventType::from(value).into()
    }
}
impl From<MessageLikeEventType> for FilterEventType {
    fn from(value: MessageLikeEventType) -> Self {
        TimelineEventType::from(value).into()
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct MatrixEventFilterInputData {
    #[serde(rename = "type")]
    pub(super) event_type: FilterEventType,
    pub(super) state_key: Option<String>,
    pub(super) content: MatrixEventContent,
}

#[derive(Debug)]
pub(crate) enum MatrixEventFilterInput {
    Timeline(MatrixEventFilterInputData),
    ToDevice(MatrixEventFilterInputData),
}

#[derive(Debug, Default, Deserialize)]
pub(super) struct MatrixEventContent {
    pub(super) msgtype: Option<String>,
}

#[cfg(test)]
mod tests {
    use ruma::events::{MessageLikeEventType, StateEventType, TimelineEventType};

    use crate::widget::filter::EventAndRequestFilter;

    use super::{
        EventFilter, MatrixEventContent, MatrixEventFilterInput, MatrixEventFilterInputData,
        MessageLikeEventFilter, StateEventFilter,
    };

    fn message_event(event_type: TimelineEventType) -> MatrixEventFilterInput {
        MatrixEventFilterInput::Timeline(MatrixEventFilterInputData {
            event_type: event_type.into(),
            state_key: None,
            content: Default::default(),
        })
    }

    fn message_event_with_msgtype(
        event_type: TimelineEventType,
        msgtype: String,
    ) -> MatrixEventFilterInput {
        MatrixEventFilterInput::Timeline(MatrixEventFilterInputData {
            event_type: event_type.into(),
            state_key: None,
            content: MatrixEventContent { msgtype: Some(msgtype) },
        })
    }

    fn state_event(event_type: TimelineEventType, state_key: String) -> MatrixEventFilterInput {
        MatrixEventFilterInput::Timeline(MatrixEventFilterInputData {
            event_type: event_type.into(),
            state_key: Some(state_key),
            content: Default::default(),
        })
    }

    // Tests against an `m.room.message` filter with `msgtype = m.text`
    fn room_message_text_event_filter() -> EventFilter {
        EventFilter::MessageLike(MessageLikeEventFilter::RoomMessageWithMsgtype(
            "m.text".to_owned(),
        ))
    }

    #[test]
    fn text_event_filter_matches_text_event() {
        assert!(room_message_text_event_filter().matches_event(&message_event_with_msgtype(
            TimelineEventType::RoomMessage,
            "m.text".to_owned()
        )));
    }

    #[test]
    fn text_event_filter_does_not_match_image_event() {
        assert!(!room_message_text_event_filter().matches_event(&message_event_with_msgtype(
            TimelineEventType::RoomMessage,
            "m.image".to_owned()
        )));
    }

    #[test]
    fn text_event_filter_does_not_match_custom_event_with_msgtype() {
        assert!(!room_message_text_event_filter().matches_event(&message_event_with_msgtype(
            "io.element.message".into(),
            "m.text".to_owned()
        )));
    }

    // Tests against an `m.reaction` filter
    fn reaction_event_filter() -> EventFilter {
        EventFilter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::Reaction))
    }

    #[test]
    fn reaction_event_filter_matches_reaction() {
        assert!(reaction_event_filter().matches_event(&message_event(TimelineEventType::Reaction)));
    }

    #[test]
    fn reaction_event_filter_does_not_match_room_message() {
        assert!(!reaction_event_filter().matches_event(&message_event_with_msgtype(
            TimelineEventType::RoomMessage,
            "m.text".to_owned()
        )));
    }

    #[test]
    fn reaction_event_filter_does_not_match_state_event() {
        assert!(!reaction_event_filter().matches_event(&state_event(
            // Use the `m.reaction` event type to make sure the event would pass
            // the filter without state event checks, even though in practice
            // that event type won't be used for a state event.
            TimelineEventType::Reaction,
            "".to_owned()
        )));
    }

    #[test]
    fn reaction_event_filter_does_not_match_state_event_any_key() {
        assert!(
            !reaction_event_filter().matches_request(&StateEventType::from("m.reaction").into())
        );
    }

    // Tests against an `m.room.member` filter with `state_key = "@self:example.me"`
    fn self_member_event_filter() -> EventFilter {
        EventFilter::State(StateEventFilter::WithTypeAndStateKey(
            StateEventType::RoomMember,
            "@self:example.me".to_owned(),
        ))
    }

    #[test]
    fn self_member_event_filter_matches_self_member_event() {
        assert!(self_member_event_filter().matches_event(&state_event(
            TimelineEventType::RoomMember,
            "@self:example.me".to_owned()
        )));
    }

    #[test]
    fn self_member_event_filter_does_not_match_somebody_elses_member_event() {
        assert!(!self_member_event_filter().matches_event(&state_event(
            TimelineEventType::RoomMember,
            "@somebody_else.example.me".to_owned()
        )));
    }

    #[test]
    fn self_member_event_filter_does_not_match_unrelated_state_event_with_same_state_key() {
        assert!(!self_member_event_filter().matches_event(&state_event(
            TimelineEventType::from("io.element.test_state_event"),
            "@self.example.me".to_owned()
        )));
    }

    #[test]
    fn self_member_event_filter_does_not_match_reaction_event() {
        assert!(
            !self_member_event_filter().matches_event(&message_event(TimelineEventType::Reaction))
        );
    }

    #[test]
    fn self_member_event_filter_only_matches_specific_state_key() {
        assert!(!self_member_event_filter().matches_request(&StateEventType::RoomMember.into()));
    }

    // Tests against an `m.room.member` filter with any `state_key`.
    fn member_event_filter() -> EventFilter {
        EventFilter::State(StateEventFilter::WithType(StateEventType::RoomMember))
    }

    #[test]
    fn member_event_filter_matches_some_member_event() {
        assert!(member_event_filter()
            .matches_event(&state_event(TimelineEventType::RoomMember, "@foo.bar.baz".to_owned())));
    }

    #[test]
    fn member_event_filter_does_not_match_room_name_event() {
        assert!(!member_event_filter()
            .matches_event(&state_event(TimelineEventType::RoomName, "".to_owned())));
    }

    #[test]
    fn member_event_filter_does_not_match_reaction_event() {
        assert!(!member_event_filter().matches_event(&message_event(TimelineEventType::Reaction)));
    }

    #[test]
    fn member_event_filter_matches_any_state_key() {
        assert!(member_event_filter().matches_request(&StateEventType::RoomMember.into()));
    }

    // Tests against an `m.room.topic` filter with `state_key = ""`
    fn topic_event_filter() -> EventFilter {
        EventFilter::State(StateEventFilter::WithTypeAndStateKey(
            StateEventType::RoomTopic,
            "".to_owned(),
        ))
    }

    #[test]
    fn topic_event_filter_does_not_match_any_state_key() {
        assert!(!topic_event_filter().matches_request(&StateEventType::RoomTopic.into()));
    }

    // Tests against an `m.room.message` filter with `msgtype = m.custom`
    fn room_message_custom_event_filter() -> EventFilter {
        EventFilter::MessageLike(MessageLikeEventFilter::RoomMessageWithMsgtype(
            "m.custom".to_owned(),
        ))
    }

    // Tests against an `m.room.message` filter without a `msgtype`
    fn room_message_filter() -> EventFilter {
        EventFilter::MessageLike(MessageLikeEventFilter::WithType(
            MessageLikeEventType::RoomMessage,
        ))
    }

    #[test]
    fn room_message_event_type_matches_room_message_text_event_filter() {
        assert!(room_message_text_event_filter()
            .matches_request(&MessageLikeEventType::RoomMessage.into()));
    }

    #[test]
    fn reaction_event_type_does_not_match_room_message_text_event_filter() {
        assert!(!room_message_text_event_filter()
            .matches_request(&MessageLikeEventType::Reaction.into()));
    }

    #[test]
    fn room_message_event_type_matches_room_message_custom_event_filter() {
        assert!(room_message_custom_event_filter()
            .matches_request(&MessageLikeEventType::RoomMessage.into()));
    }

    #[test]
    fn reaction_event_type_does_not_match_room_message_custom_event_filter() {
        assert!(!room_message_custom_event_filter()
            .matches_request(&MessageLikeEventType::Reaction.into()));
    }

    #[test]
    fn room_message_event_type_matches_room_message_event_filter() {
        assert!(room_message_filter().matches_request(&MessageLikeEventType::RoomMessage.into()));
    }

    #[test]
    fn reaction_event_type_does_not_match_room_message_event_filter() {
        assert!(!room_message_filter().matches_request(&MessageLikeEventType::Reaction.into()));
    }
}
