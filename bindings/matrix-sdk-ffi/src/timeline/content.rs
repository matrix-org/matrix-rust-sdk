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

use std::{collections::HashMap, sync::Arc};

use matrix_sdk_ui::timeline::{PollResult, TimelineDetails};
use tracing::warn;

use super::ProfileDetails;
use crate::ruma::{ImageInfo, MessageType, PollKind};

#[derive(Clone, uniffi::Object)]
pub struct TimelineItemContent(pub(crate) matrix_sdk_ui::timeline::TimelineItemContent);

#[uniffi::export]
impl TimelineItemContent {
    pub fn kind(&self) -> TimelineItemContentKind {
        use matrix_sdk_ui::timeline::TimelineItemContent as Content;

        match &self.0 {
            Content::Message(_) => TimelineItemContentKind::Message,
            Content::RedactedMessage => TimelineItemContentKind::RedactedMessage,
            Content::Sticker(sticker) => {
                let content = sticker.content();
                TimelineItemContentKind::Sticker {
                    body: content.body.clone(),
                    info: (&content.info).into(),
                    url: content.url.to_string(),
                }
            }
            Content::Poll(poll_state) => TimelineItemContentKind::from(poll_state.results()),
            Content::UnableToDecrypt(msg) => {
                TimelineItemContentKind::UnableToDecrypt { msg: EncryptedMessage::new(msg) }
            }
            Content::MembershipChange(membership) => TimelineItemContentKind::RoomMembership {
                user_id: membership.user_id().to_string(),
                change: membership.change().map(Into::into),
            },
            Content::ProfileChange(profile) => {
                let (display_name, prev_display_name) = profile
                    .displayname_change()
                    .map(|change| (change.new.clone(), change.old.clone()))
                    .unzip();
                let (avatar_url, prev_avatar_url) = profile
                    .avatar_url_change()
                    .map(|change| {
                        (
                            change.new.as_ref().map(ToString::to_string),
                            change.old.as_ref().map(ToString::to_string),
                        )
                    })
                    .unzip();
                TimelineItemContentKind::ProfileChange {
                    display_name: display_name.flatten(),
                    prev_display_name: prev_display_name.flatten(),
                    avatar_url: avatar_url.flatten(),
                    prev_avatar_url: prev_avatar_url.flatten(),
                }
            }
            Content::OtherState(state) => TimelineItemContentKind::State {
                state_key: state.state_key().to_owned(),
                content: state.content().into(),
            },
            Content::FailedToParseMessageLike { event_type, error } => {
                TimelineItemContentKind::FailedToParseMessageLike {
                    event_type: event_type.to_string(),
                    error: error.to_string(),
                }
            }
            Content::FailedToParseState { event_type, state_key, error } => {
                TimelineItemContentKind::FailedToParseState {
                    event_type: event_type.to_string(),
                    state_key: state_key.to_string(),
                    error: error.to_string(),
                }
            }
        }
    }

    pub fn as_message(self: Arc<Self>) -> Option<Arc<Message>> {
        use matrix_sdk_ui::timeline::TimelineItemContent as Content;
        unwrap_or_clone_arc_into_variant!(self, .0, Content::Message(msg) => Arc::new(Message(msg)))
    }
}

#[derive(uniffi::Enum)]
pub enum TimelineItemContentKind {
    Message,
    RedactedMessage,
    Sticker {
        body: String,
        info: ImageInfo,
        url: String,
    },
    Poll {
        question: String,
        kind: PollKind,
        max_selections: u64,
        answers: Vec<PollAnswer>,
        votes: HashMap<String, Vec<String>>,
        end_time: Option<u64>,
        has_been_edited: bool,
    },
    UnableToDecrypt {
        msg: EncryptedMessage,
    },
    RoomMembership {
        user_id: String,
        change: Option<MembershipChange>,
    },
    ProfileChange {
        display_name: Option<String>,
        prev_display_name: Option<String>,
        avatar_url: Option<String>,
        prev_avatar_url: Option<String>,
    },
    State {
        state_key: String,
        content: OtherState,
    },
    FailedToParseMessageLike {
        event_type: String,
        error: String,
    },
    FailedToParseState {
        event_type: String,
        state_key: String,
        error: String,
    },
}

#[derive(Clone, uniffi::Object)]
pub struct Message(matrix_sdk_ui::timeline::Message);

#[uniffi::export]
impl Message {
    pub fn msgtype(&self) -> MessageType {
        self.0.msgtype().clone().into()
    }

    pub fn body(&self) -> String {
        self.0.msgtype().body().to_owned()
    }

    pub fn in_reply_to(&self) -> Option<InReplyToDetails> {
        self.0.in_reply_to().map(InReplyToDetails::from)
    }

    pub fn is_threaded(&self) -> bool {
        self.0.is_threaded()
    }

    pub fn is_edited(&self) -> bool {
        self.0.is_edited()
    }
}

#[derive(uniffi::Record)]
pub struct InReplyToDetails {
    event_id: String,
    event: RepliedToEventDetails,
}

impl From<&matrix_sdk_ui::timeline::InReplyToDetails> for InReplyToDetails {
    fn from(inner: &matrix_sdk_ui::timeline::InReplyToDetails) -> Self {
        let event_id = inner.event_id.to_string();
        let event = match &inner.event {
            TimelineDetails::Unavailable => RepliedToEventDetails::Unavailable,
            TimelineDetails::Pending => RepliedToEventDetails::Pending,
            TimelineDetails::Ready(event) => RepliedToEventDetails::Ready {
                content: Arc::new(TimelineItemContent(event.content().to_owned())),
                sender: event.sender().to_string(),
                sender_profile: event.sender_profile().into(),
            },
            TimelineDetails::Error(err) => {
                RepliedToEventDetails::Error { message: err.to_string() }
            }
        };

        Self { event_id, event }
    }
}

#[derive(uniffi::Enum)]
pub enum RepliedToEventDetails {
    Unavailable,
    Pending,
    Ready { content: Arc<TimelineItemContent>, sender: String, sender_profile: ProfileDetails },
    Error { message: String },
}

#[derive(Clone, uniffi::Enum)]
pub enum EncryptedMessage {
    OlmV1Curve25519AesSha2 {
        /// The Curve25519 key of the sender.
        sender_key: String,
    },
    // Other fields not included because UniFFI doesn't have the concept of
    // deprecated fields right now.
    MegolmV1AesSha2 {
        /// The ID of the session used to encrypt the message.
        session_id: String,
    },
    Unknown,
}

impl EncryptedMessage {
    fn new(msg: &matrix_sdk_ui::timeline::EncryptedMessage) -> Self {
        use matrix_sdk_ui::timeline::EncryptedMessage as Message;

        match msg {
            Message::OlmV1Curve25519AesSha2 { sender_key } => {
                let sender_key = sender_key.clone();
                Self::OlmV1Curve25519AesSha2 { sender_key }
            }
            Message::MegolmV1AesSha2 { session_id, .. } => {
                let session_id = session_id.clone();
                Self::MegolmV1AesSha2 { session_id }
            }
            Message::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, uniffi::Record)]
pub struct Reaction {
    pub key: String,
    pub count: u64,
    pub senders: Vec<ReactionSenderData>,
}

#[derive(Clone, uniffi::Record)]
pub struct ReactionSenderData {
    pub sender_id: String,
    pub timestamp: u64,
}

#[derive(Clone, uniffi::Enum)]
pub enum MembershipChange {
    None,
    Error,
    Joined,
    Left,
    Banned,
    Unbanned,
    Kicked,
    Invited,
    KickedAndBanned,
    InvitationAccepted,
    InvitationRejected,
    InvitationRevoked,
    Knocked,
    KnockAccepted,
    KnockRetracted,
    KnockDenied,
    NotImplemented,
}

impl From<matrix_sdk_ui::timeline::MembershipChange> for MembershipChange {
    fn from(membership_change: matrix_sdk_ui::timeline::MembershipChange) -> Self {
        use matrix_sdk_ui::timeline::MembershipChange as Change;
        match membership_change {
            Change::None => Self::None,
            Change::Error => Self::Error,
            Change::Joined => Self::Joined,
            Change::Left => Self::Left,
            Change::Banned => Self::Banned,
            Change::Unbanned => Self::Unbanned,
            Change::Kicked => Self::Kicked,
            Change::Invited => Self::Invited,
            Change::KickedAndBanned => Self::KickedAndBanned,
            Change::InvitationAccepted => Self::InvitationAccepted,
            Change::InvitationRejected => Self::InvitationRejected,
            Change::InvitationRevoked => Self::InvitationRevoked,
            Change::Knocked => Self::Knocked,
            Change::KnockAccepted => Self::KnockAccepted,
            Change::KnockRetracted => Self::KnockRetracted,
            Change::KnockDenied => Self::KnockDenied,
            Change::NotImplemented => Self::NotImplemented,
        }
    }
}

#[derive(Clone, uniffi::Enum)]
pub enum OtherState {
    PolicyRuleRoom,
    PolicyRuleServer,
    PolicyRuleUser,
    RoomAliases,
    RoomAvatar { url: Option<String> },
    RoomCanonicalAlias,
    RoomCreate,
    RoomEncryption,
    RoomGuestAccess,
    RoomHistoryVisibility,
    RoomJoinRules,
    RoomName { name: Option<String> },
    RoomPinnedEvents,
    RoomPowerLevels,
    RoomServerAcl,
    RoomThirdPartyInvite { display_name: Option<String> },
    RoomTombstone,
    RoomTopic { topic: Option<String> },
    SpaceChild,
    SpaceParent,
    Custom { event_type: String },
}

impl From<&matrix_sdk_ui::timeline::AnyOtherFullStateEventContent> for OtherState {
    fn from(content: &matrix_sdk_ui::timeline::AnyOtherFullStateEventContent) -> Self {
        use matrix_sdk::ruma::events::FullStateEventContent as FullContent;
        use matrix_sdk_ui::timeline::AnyOtherFullStateEventContent as Content;

        match content {
            Content::PolicyRuleRoom(_) => Self::PolicyRuleRoom,
            Content::PolicyRuleServer(_) => Self::PolicyRuleServer,
            Content::PolicyRuleUser(_) => Self::PolicyRuleUser,
            Content::RoomAliases(_) => Self::RoomAliases,
            Content::RoomAvatar(c) => {
                let url = match c {
                    FullContent::Original { content, .. } => {
                        content.url.as_ref().map(ToString::to_string)
                    }
                    FullContent::Redacted(_) => None,
                };
                Self::RoomAvatar { url }
            }
            Content::RoomCanonicalAlias(_) => Self::RoomCanonicalAlias,
            Content::RoomCreate(_) => Self::RoomCreate,
            Content::RoomEncryption(_) => Self::RoomEncryption,
            Content::RoomGuestAccess(_) => Self::RoomGuestAccess,
            Content::RoomHistoryVisibility(_) => Self::RoomHistoryVisibility,
            Content::RoomJoinRules(_) => Self::RoomJoinRules,
            Content::RoomName(c) => {
                let name = match c {
                    FullContent::Original { content, .. } => Some(content.name.clone()),
                    FullContent::Redacted(_) => None,
                };
                Self::RoomName { name }
            }
            Content::RoomPinnedEvents(_) => Self::RoomPinnedEvents,
            Content::RoomPowerLevels(_) => Self::RoomPowerLevels,
            Content::RoomServerAcl(_) => Self::RoomServerAcl,
            Content::RoomThirdPartyInvite(c) => {
                let display_name = match c {
                    FullContent::Original { content, .. } => Some(content.display_name.clone()),
                    FullContent::Redacted(_) => None,
                };
                Self::RoomThirdPartyInvite { display_name }
            }
            Content::RoomTombstone(_) => Self::RoomTombstone,
            Content::RoomTopic(c) => {
                let topic = match c {
                    FullContent::Original { content, .. } => Some(content.topic.clone()),
                    FullContent::Redacted(_) => None,
                };
                Self::RoomTopic { topic }
            }
            Content::SpaceChild(_) => Self::SpaceChild,
            Content::SpaceParent(_) => Self::SpaceParent,
            Content::_Custom { event_type, .. } => Self::Custom { event_type: event_type.clone() },
        }
    }
}

#[derive(uniffi::Record)]
pub struct PollAnswer {
    pub id: String,
    pub text: String,
}

impl From<PollResult> for TimelineItemContentKind {
    fn from(value: PollResult) -> Self {
        TimelineItemContentKind::Poll {
            question: value.question,
            kind: PollKind::from(value.kind),
            max_selections: value.max_selections,
            answers: value
                .answers
                .into_iter()
                .map(|i| PollAnswer { id: i.id, text: i.text })
                .collect(),
            votes: value.votes,
            end_time: value.end_time,
            has_been_edited: value.has_been_edited,
        }
    }
}
