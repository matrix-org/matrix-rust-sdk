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
use ruma::events::{
    room::history_visibility::HistoryVisibility as RumaHistoryVisibility, FullStateEventContent,
};

use crate::{
    client::JoinRule, event::TimelineEventType, timeline::msg_like::MsgLikeContent,
    utils::Timestamp,
};

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

            Content::RtcNotification => TimelineItemContent::RtcNotification,

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

#[derive(Debug, Clone, uniffi::Enum)]
pub enum HistoryVisibility {
    /// Previous events are accessible to newly joined members from the point
    /// they were invited onwards.
    ///
    /// Events stop being accessible when the member' state changes to
    /// something other than *invite* or *join*.
    Invited,

    /// Previous events are accessible to newly joined members from the point
    /// they joined the room onwards.
    /// Events stop being accessible when the member' state changes to
    /// something other than *join*.
    Joined,

    /// Previous events are always accessible to newly joined members.
    ///
    /// All events in the room are accessible, even those sent when the member
    /// was not a part of the room.
    Shared,

    /// All events while this is the `HistoryVisibility` value may be shared by
    /// any participating homeserver with anyone, regardless of whether they
    /// have ever joined the room.
    WorldReadable,

    /// A custom history visibility, up for interpretation by the consumer.
    Custom {
        /// The string representation for this custom history visibility.
        repr: String,
    },
}

impl From<RumaHistoryVisibility> for HistoryVisibility {
    fn from(value: RumaHistoryVisibility) -> Self {
        match value {
            RumaHistoryVisibility::Invited => Self::Invited,
            RumaHistoryVisibility::Joined => Self::Joined,
            RumaHistoryVisibility::Shared => Self::Shared,
            RumaHistoryVisibility::WorldReadable => Self::WorldReadable,
            _ => Self::Custom { repr: value.to_string() },
        }
    }
}

#[derive(Clone, uniffi::Enum)]
// A note about this `allow(clippy::large_enum_variant)`.
// In order to reduce the size of `TimelineItemContent`, we would need to
// put some parts in a `Box`, or an `Arc`. Sadly, it doesn't play well with
// UniFFI. We would need to change the `uniffi::Record` of the subtypes into
// `uniffi::Object`, which is a radical change. It would simplify the memory
// usage, but it would slow down the performance around the FFI border. Thus,
// let's consider this is a false-positive lint in this particular case.
#[allow(clippy::large_enum_variant)]
pub enum TimelineItemContent {
    MsgLike {
        content: MsgLikeContent,
    },
    CallInvite,
    RtcNotification,
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

#[derive(Clone, uniffi::Record)]
pub struct PowerLevelChanges {
    ban: i64,
    kick: i64,
    events_default: i64,
    invite: i64,
    redact: i64,
    state_default: i64,
    users_default: i64,
    notifications: i64,
}

#[derive(Clone, uniffi::Enum)]
#[allow(clippy::large_enum_variant)]
// Added because the RoomPowerLevels variant is quite large.
// This is the same issue than for TimelineItemContent.
pub enum OtherState {
    PolicyRuleRoom,
    PolicyRuleServer,
    PolicyRuleUser,
    RoomAliases,
    RoomAvatar {
        url: Option<String>,
    },
    RoomCanonicalAlias,
    RoomCreate {
        federate: Option<bool>,
    },
    RoomEncryption,
    RoomGuestAccess,
    RoomHistoryVisibility {
        history_visibility: Option<HistoryVisibility>,
    },
    RoomJoinRules {
        join_rule: Option<JoinRule>,
    },
    RoomName {
        name: Option<String>,
    },
    RoomPinnedEvents {
        change: RoomPinnedEventsChange,
    },
    RoomPowerLevels {
        events: HashMap<TimelineEventType, i64>,
        previous_events: Option<HashMap<TimelineEventType, i64>>,
        users: HashMap<String, i64>,
        previous_users: Option<HashMap<String, i64>>,
        thresholds: Option<PowerLevelChanges>,
        previous_thresholds: Option<PowerLevelChanges>,
    },
    RoomServerAcl,
    RoomThirdPartyInvite {
        display_name: Option<String>,
    },
    RoomTombstone,
    RoomTopic {
        topic: Option<String>,
    },
    SpaceChild,
    SpaceParent,
    Custom {
        event_type: String,
    },
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
            Content::RoomCreate(c) => {
                let federate = match c {
                    FullContent::Original { content, .. } => Some(content.federate),
                    FullContent::Redacted(_) => None,
                };
                Self::RoomCreate { federate }
            }
            Content::RoomEncryption(_) => Self::RoomEncryption,
            Content::RoomGuestAccess(_) => Self::RoomGuestAccess,
            Content::RoomHistoryVisibility(c) => {
                let history_visibility = match c {
                    FullContent::Original { content, .. } => {
                        Some(content.history_visibility.clone().into())
                    }
                    FullContent::Redacted(_) => None,
                };
                Self::RoomHistoryVisibility { history_visibility }
            }
            Content::RoomJoinRules(c) => {
                let join_rule = match c {
                    FullContent::Original { content, .. } => {
                        match content.join_rule.clone().try_into() {
                            Ok(jr) => Some(jr),
                            Err(err) => {
                                tracing::error!("Failed to convert join rule: {}", err);
                                None
                            }
                        }
                    }
                    FullContent::Redacted(_) => None,
                };
                Self::RoomJoinRules { join_rule }
            }
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
                    events: content
                        .events
                        .iter()
                        .map(|(k, &v)| (k.clone().into(), v.into()))
                        .collect(),
                    previous_events: prev_content.as_ref().map(|prev_content| {
                        prev_content
                            .events
                            .iter()
                            .map(|(k, &v)| (k.clone().into(), v.into()))
                            .collect()
                    }),
                    thresholds: Some(PowerLevelChanges {
                        ban: content.ban.into(),
                        kick: content.kick.into(),
                        events_default: content.events_default.into(),
                        invite: content.invite.into(),
                        redact: content.redact.into(),
                        state_default: content.state_default.into(),
                        users_default: content.users_default.into(),
                        notifications: content.notifications.room.into(),
                    }),
                    previous_thresholds: prev_content.as_ref().map(|prev_content| {
                        PowerLevelChanges {
                            ban: prev_content.ban.into(),
                            kick: prev_content.kick.into(),
                            events_default: prev_content.events_default.into(),
                            invite: prev_content.invite.into(),
                            redact: prev_content.redact.into(),
                            state_default: prev_content.state_default.into(),
                            users_default: prev_content.users_default.into(),
                            notifications: prev_content.notifications.room.into(),
                        }
                    }),
                    users: power_level_user_changes(content, prev_content)
                        .iter()
                        .map(|(k, v)| (k.to_string(), *v))
                        .collect(),
                    previous_users: prev_content.as_ref().map(|prev_content| {
                        prev_content.users.iter().map(|(k, &v)| (k.to_string(), v.into())).collect()
                    }),
                },
                FullContent::Redacted(_) => Self::RoomPowerLevels {
                    events: Default::default(),
                    previous_events: None,
                    users: Default::default(),
                    previous_users: None,
                    thresholds: None,
                    previous_thresholds: None,
                },
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
