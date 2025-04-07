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

use std::collections::HashMap;

use matrix_sdk::room::power_levels::power_level_user_changes;
use matrix_sdk_ui::timeline::RoomPinnedEventsChange;
use ruma::events::FullStateEventContent;

use crate::{timeline::msg_like::MsgLikeContent, utils::Timestamp};

impl From<matrix_sdk_ui::timeline::TimelineItemContent> for TimelineItemContent {
    fn from(value: matrix_sdk_ui::timeline::TimelineItemContent) -> Self {
        use matrix_sdk_ui::timeline::TimelineItemContent as Content;

        match value {
            Content::MsgLike(msg_like) => match msg_like.try_into() {
                Ok(content) => TimelineItemContent::MsgLike { content },
                Err((error, event_type)) => TimelineItemContent::FailedToParseMessageLike {
                    event_type,
                    error: error.to_string(),
                },
            },

            Content::CallInvite => TimelineItemContent::CallInvite,

            Content::CallNotify => TimelineItemContent::CallNotify,

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
    MsgLike {
        content: MsgLikeContent,
    },
    CallInvite,
    CallNotify,
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
