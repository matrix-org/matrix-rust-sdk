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

use ruma::events::{MessageLikeEventType, StateEventType, TimelineEventType};
use serde::Deserialize;

/// Different kinds of filters for timeline events.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(PartialEq))]
pub enum Filter {
    /// Filter for message-like events.
    MessageLike(MessageLikeEventFilter),
    /// Filter for state events.
    State(StateEventFilter),
}

impl Filter {
    pub(super) fn matches(&self, matrix_event: &FilterInput) -> bool {
        match self {
            Filter::MessageLike(message_filter) => message_filter.matches(matrix_event),
            Filter::State(state_filter) => state_filter.matches(matrix_event),
        }
    }

    pub(super) fn matches_state_event_with_any_state_key(
        &self,
        event_type: &StateEventType,
    ) -> bool {
        matches!(
            self,
            Self::State(filter) if filter.matches_state_event_with_any_state_key(event_type)
        )
    }

    pub(super) fn matches_message_like_event_type(
        &self,
        event_type: &MessageLikeEventType,
    ) -> bool {
        matches!(
            self,
            Self::MessageLike(filter) if filter.matches_message_like_event_type(event_type)
        )
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
    fn matches(&self, matrix_event: &FilterInput) -> bool {
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

    fn matches_message_like_event_type(&self, event_type: &MessageLikeEventType) -> bool {
        match self {
            MessageLikeEventFilter::WithType(filter_event_type) => filter_event_type == event_type,
            MessageLikeEventFilter::RoomMessageWithMsgtype(_) => {
                event_type == &MessageLikeEventType::RoomMessage
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
    fn matches(&self, matrix_event: &FilterInput) -> bool {
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

    fn matches_state_event_with_any_state_key(&self, event_type: &StateEventType) -> bool {
        matches!(self, Self::WithType(ty) if ty == event_type)
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct FilterInput {
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
        Filter, FilterInput, MatrixEventContent, MessageLikeEventFilter, StateEventFilter,
    };

    fn message_event(event_type: TimelineEventType) -> FilterInput {
        FilterInput { event_type, state_key: None, content: Default::default() }
    }

    fn message_event_with_msgtype(event_type: TimelineEventType, msgtype: String) -> FilterInput {
        FilterInput {
            event_type,
            state_key: None,
            content: MatrixEventContent { msgtype: Some(msgtype) },
        }
    }

    fn state_event(event_type: TimelineEventType, state_key: String) -> FilterInput {
        FilterInput { event_type, state_key: Some(state_key), content: Default::default() }
    }

    // Tests against an `m.room.message` filter with `msgtype = m.text`
    fn room_message_text_event_filter() -> Filter {
        Filter::MessageLike(MessageLikeEventFilter::RoomMessageWithMsgtype("m.text".to_owned()))
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
    fn reaction_event_filter() -> Filter {
        Filter::MessageLike(MessageLikeEventFilter::WithType(MessageLikeEventType::Reaction))
    }

    #[test]
    fn test_reaction_event_filter_matches_reaction() {
        assert!(reaction_event_filter()
            .matches(&message_event(&MessageLikeEventType::Reaction.to_string())));
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
        assert!(!self_member_event_filter()
            .matches(&FilterInput::state("io.element.test_state_event", "@self.example.me")));
    }

    #[test]
    fn test_self_member_event_filter_does_not_match_reaction_event() {
        assert!(!self_member_event_filter()
            .matches(&message_event(&MessageLikeEventType::Reaction.to_string())));
    }

    #[test]
    fn test_self_member_event_filter_only_matches_specific_state_key() {
        assert!(!self_member_event_filter()
            .matches(&FilterInput::state(&StateEventType::RoomMember.to_string(), "")));
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
        assert!(!member_event_filter()
            .matches(&FilterInput::state(&TimelineEventType::RoomName.to_string(), "")));
    }

    #[test]
    fn test_member_event_filter_does_not_match_reaction_event() {
        assert!(!member_event_filter()
            .matches(&message_event(&MessageLikeEventType::Reaction.to_string())));
    }

    #[test]
    fn test_member_event_filter_matches_any_state_key() {
        assert!(member_event_filter()
            .matches(&FilterInput::state(&StateEventType::RoomMember.to_string(), "")));
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
        assert!(topic_event_filter()
            .matches(&FilterInput::state(&StateEventType::RoomTopic.to_string(), "")));
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
        assert!(!room_message_text_event_filter()
            .matches(&FilterInput::message_like(&MessageLikeEventType::Reaction.to_string())));
    }

    #[test]
    fn test_room_message_event_without_msgtype_does_not_match_custom_msgtype_filter() {
        assert!(!room_message_custom_event_filter()
            .matches(&FilterInput::message_like(&MessageLikeEventType::RoomMessage.to_string())));
    }

    #[test]
    fn test_reaction_event_type_does_not_match_room_message_custom_event_filter() {
        assert!(!room_message_custom_event_filter()
            .matches(&FilterInput::message_like(&MessageLikeEventType::Reaction.to_string())));
    }

    #[test]
    fn test_room_message_event_type_matches_room_message_event_filter() {
        assert!(room_message_filter()
            .matches(&FilterInput::message_like(&MessageLikeEventType::RoomMessage.to_string())));
    }

    #[test]
    fn reaction_event_type_does_not_match_room_message_event_filter() {
        assert!(
            !room_message_filter().matches_message_like_event_type(&MessageLikeEventType::Reaction)
        );
    }
}
