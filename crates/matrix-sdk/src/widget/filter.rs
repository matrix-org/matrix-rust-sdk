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

use ruma::events::{MessageLikeEventType, StateEventType, TimelineEventType};
use serde::Deserialize;

/// Different kinds of filters for timeline events.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub enum EventFilter {
    /// Filter for message-like events.
    MessageLike(MessageLikeEventFilter),
    /// Filter for state events.
    State(StateEventFilter),
}

impl EventFilter {
    pub(super) fn matches(&self, matrix_event: &MatrixEventFilterInput) -> bool {
        match self {
            EventFilter::MessageLike(message_filter) => message_filter.matches(matrix_event),
            EventFilter::State(state_filter) => state_filter.matches(matrix_event),
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

impl MessageLikeEventFilter {
    fn matches(&self, matrix_event: &MatrixEventFilterInput) -> bool {
        if matrix_event.state_key.is_some() {
            // State event doesn't match a message-like event filter.
            return false;
        }

        match self {
            MessageLikeEventFilter::WithType(event_type) => {
                matrix_event.event_type == TimelineEventType::from(event_type.clone())
            }
            MessageLikeEventFilter::RoomMessageWithMsgtype(msgtype) => {
                matrix_event.event_type == TimelineEventType::RoomMessage
                    && matrix_event.content.msgtype.as_ref() == Some(msgtype)
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

impl StateEventFilter {
    fn matches(&self, matrix_event: &MatrixEventFilterInput) -> bool {
        let Some(state_key) = &matrix_event.state_key else {
            // Message-like event doesn't match a state event filter.
            return false;
        };

        match self {
            StateEventFilter::WithType(event_type) => {
                matrix_event.event_type == TimelineEventType::from(event_type.clone())
            }
            StateEventFilter::WithTypeAndStateKey(event_type, filter_state_key) => {
                matrix_event.event_type == TimelineEventType::from(event_type.clone())
                    && state_key == filter_state_key
            }
        }
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

#[cfg(test)]
mod tests {
    use ruma::events::{MessageLikeEventType, StateEventType, TimelineEventType};

    use super::{
        EventFilter, MatrixEventContent, MatrixEventFilterInput, MessageLikeEventFilter,
        StateEventFilter,
    };

    fn message_event(event_type: TimelineEventType) -> MatrixEventFilterInput {
        MatrixEventFilterInput { event_type, state_key: None, content: Default::default() }
    }

    fn message_event_with_msgtype(
        event_type: TimelineEventType,
        msgtype: String,
    ) -> MatrixEventFilterInput {
        MatrixEventFilterInput {
            event_type,
            state_key: None,
            content: MatrixEventContent { msgtype: Some(msgtype) },
        }
    }

    fn state_event(event_type: TimelineEventType, state_key: String) -> MatrixEventFilterInput {
        MatrixEventFilterInput {
            event_type,
            state_key: Some(state_key),
            content: Default::default(),
        }
    }

    // Tests against an `m.room.message` filter with `msgtype = m.text`
    fn room_message_text_event_filter() -> EventFilter {
        EventFilter::MessageLike(MessageLikeEventFilter::RoomMessageWithMsgtype(
            "m.text".to_owned(),
        ))
    }

    #[test]
    fn text_event_filter_matches_text_event() {
        assert!(room_message_text_event_filter().matches(&message_event_with_msgtype(
            TimelineEventType::RoomMessage,
            "m.text".to_owned()
        )));
    }

    #[test]
    fn text_event_filter_does_not_match_image_event() {
        assert!(!room_message_text_event_filter().matches(&message_event_with_msgtype(
            TimelineEventType::RoomMessage,
            "m.image".to_owned()
        )));
    }

    #[test]
    fn text_event_filter_does_not_match_custom_event_with_msgtype() {
        assert!(!room_message_text_event_filter().matches(&message_event_with_msgtype(
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
        assert!(reaction_event_filter().matches(&message_event(TimelineEventType::Reaction)));
    }

    #[test]
    fn reaction_event_filter_does_not_match_room_message() {
        assert!(!reaction_event_filter().matches(&message_event_with_msgtype(
            TimelineEventType::RoomMessage,
            "m.text".to_owned()
        )));
    }

    #[test]
    fn reaction_event_filter_does_not_match_state_event() {
        assert!(!reaction_event_filter().matches(&state_event(
            // Use the `m.reaction` event type to make sure the event would pass
            // the filter without state event checks, even though in practice
            // that event type won't be used for a state event.
            TimelineEventType::Reaction,
            "".to_owned()
        )));
    }

    // Tests against an `m.room.member` filter with `state_key = @self:example.me`
    fn self_member_event_filter() -> EventFilter {
        EventFilter::State(StateEventFilter::WithTypeAndStateKey(
            StateEventType::RoomMember,
            "@self:example.me".to_owned(),
        ))
    }

    #[test]
    fn self_member_event_filter_matches_self_member_event() {
        assert!(self_member_event_filter()
            .matches(&state_event(TimelineEventType::RoomMember, "@self:example.me".to_owned())));
    }

    #[test]
    fn self_member_event_filter_does_not_match_somebody_elses_member_event() {
        assert!(!self_member_event_filter().matches(&state_event(
            TimelineEventType::RoomMember,
            "@somebody_else.example.me".to_owned()
        )));
    }

    #[test]
    fn self_member_event_filter_does_not_match_unrelated_state_event_with_same_state_key() {
        assert!(!self_member_event_filter().matches(&state_event(
            TimelineEventType::from("io.element.test_state_event"),
            "@self.example.me".to_owned()
        )));
    }

    #[test]
    fn self_member_event_filter_does_not_match_reaction_event() {
        assert!(!self_member_event_filter().matches(&message_event(TimelineEventType::Reaction)));
    }

    // Tests against an `m.room.member` filter with any `state_key`.
    fn member_event_filter() -> EventFilter {
        EventFilter::State(StateEventFilter::WithType(StateEventType::RoomMember))
    }

    #[test]
    fn member_event_filter_matches_some_member_event() {
        assert!(member_event_filter()
            .matches(&state_event(TimelineEventType::RoomMember, "@foo.bar.baz".to_owned())));
    }

    #[test]
    fn member_event_filter_does_not_match_room_name_event() {
        assert!(!member_event_filter()
            .matches(&state_event(TimelineEventType::RoomName, "".to_owned())));
    }

    #[test]
    fn member_event_filter_does_not_match_reaction_event() {
        assert!(!member_event_filter().matches(&message_event(TimelineEventType::Reaction)));
    }
}
