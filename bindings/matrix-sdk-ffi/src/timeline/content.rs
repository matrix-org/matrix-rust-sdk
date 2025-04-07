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

use matrix_sdk::room::power_levels::power_level_user_changes;
use matrix_sdk_ui::timeline::{MsgLikeContent, MsgLikeKind, RoomPinnedEventsChange};
use ruma::events::{room::MediaSource as RumaMediaSource, EventContent, FullStateEventContent};

use crate::{
    ruma::{ImageInfo, MediaSource, MediaSourceExt, MessageType, PollKind},
    timeline::msg_like::{EncryptedMessage, MessageContent, PollAnswer},
    utils::Timestamp,
};

impl From<matrix_sdk_ui::timeline::TimelineItemContent> for TimelineItemContent {
    fn from(value: matrix_sdk_ui::timeline::TimelineItemContent) -> Self {
        use matrix_sdk_ui::timeline::TimelineItemContent as Content;

        match value {
            Content::MsgLike(MsgLikeContent {
                kind: MsgLikeKind::Message(message),
                thread_root,
                in_reply_to,
                ..
            }) => {
                let message_type_string = message.msgtype().msgtype().to_owned();

                match TryInto::<MessageType>::try_into(message.msgtype().clone()) {
                    Ok(message_type) => TimelineItemContent::Message {
                        content: MessageContent {
                            msg_type: message_type,
                            body: message.body().to_owned(),
                            in_reply_to: in_reply_to.map(|r| Arc::new(r.into())),
                            is_edited: message.is_edited(),
                            thread_root: thread_root.map(|id| id.to_string()),
                            mentions: message.mentions().cloned().map(|m| m.into()),
                        },
                    },
                    Err(error) => TimelineItemContent::FailedToParseMessageLike {
                        event_type: message_type_string,
                        error: error.to_string(),
                    },
                }
            }

            Content::MsgLike(MsgLikeContent { kind: MsgLikeKind::Redacted, .. }) => {
                TimelineItemContent::RedactedMessage
            }

            Content::MsgLike(MsgLikeContent { kind: MsgLikeKind::Sticker(sticker), .. }) => {
                let content = sticker.content();

                let media_source = RumaMediaSource::from(content.source.clone());

                if let Err(error) = media_source.verify() {
                    return TimelineItemContent::FailedToParseMessageLike {
                        event_type: sticker.content().event_type().to_string(),
                        error: error.to_string(),
                    };
                }

                match TryInto::<ImageInfo>::try_into(&content.info) {
                    Ok(info) => TimelineItemContent::Sticker {
                        body: content.body.clone(),
                        info,
                        source: Arc::new(MediaSource { media_source }),
                    },
                    Err(error) => TimelineItemContent::FailedToParseMessageLike {
                        event_type: sticker.content().event_type().to_string(),
                        error: error.to_string(),
                    },
                }
            }

            Content::MsgLike(MsgLikeContent { kind: MsgLikeKind::Poll(poll_state), .. }) => {
                TimelineItemContent::from(poll_state.results())
            }

            Content::CallInvite => TimelineItemContent::CallInvite,

            Content::CallNotify => TimelineItemContent::CallNotify,

            Content::MsgLike(MsgLikeContent {
                kind: MsgLikeKind::UnableToDecrypt(msg), ..
            }) => TimelineItemContent::UnableToDecrypt { msg: EncryptedMessage::new(&msg) },

            Content::MembershipChange(membership) => {
                let reason = match membership.content() {
                    FullStateEventContent::Original { content, .. } => content.reason.clone(),
                    _ => None,
                };
                TimelineItemContent::RoomMembership {
                    user_id: membership.user_id().to_string(),
                    user_display_name: membership.display_name(),
                    change: membership.change().map(Into::into),
                    reason,
                }
            }

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
                TimelineItemContent::ProfileChange {
                    display_name: display_name.flatten(),
                    prev_display_name: prev_display_name.flatten(),
                    avatar_url: avatar_url.flatten(),
                    prev_avatar_url: prev_avatar_url.flatten(),
                }
            }

            Content::OtherState(state) => TimelineItemContent::State {
                state_key: state.state_key().to_owned(),
                content: state.content().into(),
            },

            Content::FailedToParseMessageLike { event_type, error } => {
                TimelineItemContent::FailedToParseMessageLike {
                    event_type: event_type.to_string(),
                    error: error.to_string(),
                }
            }

            Content::FailedToParseState { event_type, state_key, error } => {
                TimelineItemContent::FailedToParseState {
                    event_type: event_type.to_string(),
                    state_key,
                    error: error.to_string(),
                }
            }
        }
    }
}

#[derive(Clone, uniffi::Enum)]
pub enum TimelineItemContent {
    Message {
        content: MessageContent,
    },
    RedactedMessage,
    Sticker {
        body: String,
        info: ImageInfo,
        source: Arc<MediaSource>,
    },
    Poll {
        question: String,
        kind: PollKind,
        max_selections: u64,
        answers: Vec<PollAnswer>,
        votes: HashMap<String, Vec<String>>,
        end_time: Option<Timestamp>,
        has_been_edited: bool,
    },
    CallInvite,
    CallNotify,
    UnableToDecrypt {
        msg: EncryptedMessage,
    },
    RoomMembership {
        user_id: String,
        user_display_name: Option<String>,
        change: Option<MembershipChange>,
        reason: Option<String>,
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

#[derive(Clone, uniffi::Record)]
pub struct Reaction {
    pub key: String,
    pub senders: Vec<ReactionSenderData>,
}

#[derive(Clone, uniffi::Record)]
pub struct ReactionSenderData {
    pub sender_id: String,
    pub timestamp: Timestamp,
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
    RoomPinnedEvents { change: RoomPinnedEventsChange },
    RoomPowerLevels { users: HashMap<String, i64>, previous: Option<HashMap<String, i64>> },
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
            Content::RoomPinnedEvents(c) => Self::RoomPinnedEvents { change: c.into() },
            Content::RoomPowerLevels(c) => match c {
                FullContent::Original { content, prev_content } => Self::RoomPowerLevels {
                    users: power_level_user_changes(content, prev_content)
                        .iter()
                        .map(|(k, v)| (k.to_string(), *v))
                        .collect(),
                    previous: prev_content.as_ref().map(|prev_content| {
                        prev_content.users.iter().map(|(k, &v)| (k.to_string(), v.into())).collect()
                    }),
                },
                FullContent::Redacted(_) => {
                    Self::RoomPowerLevels { users: Default::default(), previous: None }
                }
            },
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
