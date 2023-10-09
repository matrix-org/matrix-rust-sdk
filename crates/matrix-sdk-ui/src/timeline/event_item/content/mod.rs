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

use std::sync::Arc;

use as_variant::as_variant;
use imbl::Vector;
use matrix_sdk_base::latest_event::{is_suitable_for_latest_event, PossibleLatestEvent};
use ruma::{
    events::{
        policy::rule::{
            room::PolicyRuleRoomEventContent, server::PolicyRuleServerEventContent,
            user::PolicyRuleUserEventContent,
        },
        poll::unstable_start::{NewUnstablePollStartEventContent, SyncUnstablePollStartEvent},
        room::{
            aliases::RoomAliasesEventContent,
            avatar::RoomAvatarEventContent,
            canonical_alias::RoomCanonicalAliasEventContent,
            create::RoomCreateEventContent,
            encrypted::{EncryptedEventScheme, MegolmV1AesSha2Content, RoomEncryptedEventContent},
            encryption::RoomEncryptionEventContent,
            guest_access::RoomGuestAccessEventContent,
            history_visibility::RoomHistoryVisibilityEventContent,
            join_rules::RoomJoinRulesEventContent,
            member::{Change, RoomMemberEventContent},
            message::{RoomMessageEventContent, SyncRoomMessageEvent},
            name::RoomNameEventContent,
            pinned_events::RoomPinnedEventsEventContent,
            power_levels::RoomPowerLevelsEventContent,
            server_acl::RoomServerAclEventContent,
            third_party_invite::RoomThirdPartyInviteEventContent,
            tombstone::RoomTombstoneEventContent,
            topic::RoomTopicEventContent,
        },
        space::{child::SpaceChildEventContent, parent::SpaceParentEventContent},
        sticker::StickerEventContent,
        AnyFullStateEventContent, AnySyncMessageLikeEvent, AnySyncTimelineEvent,
        BundledMessageLikeRelations, FullStateEventContent, MessageLikeEventType, StateEventType,
    },
    OwnedDeviceId, OwnedMxcUri, OwnedUserId, RoomVersionId, UserId,
};
use tracing::warn;

use crate::timeline::{polls::PollState, TimelineItem};

mod message;

pub use self::message::{InReplyToDetails, Message, RepliedToEvent};

/// The content of an [`EventTimelineItem`][super::EventTimelineItem].
#[derive(Clone, Debug)]
pub enum TimelineItemContent {
    /// An `m.room.message` event or extensible event, including edits.
    Message(Message),

    /// A redacted message.
    RedactedMessage,

    /// An `m.sticker` event.
    Sticker(Sticker),

    /// An `m.room.encrypted` event that could not be decrypted.
    UnableToDecrypt(EncryptedMessage),

    /// A room membership change.
    MembershipChange(RoomMembershipChange),

    /// A room member profile change.
    ProfileChange(MemberProfileChange),

    /// Another state event.
    OtherState(OtherState),

    /// A message-like event that failed to deserialize.
    FailedToParseMessageLike {
        /// The event `type`.
        event_type: MessageLikeEventType,

        /// The deserialization error.
        error: Arc<serde_json::Error>,
    },

    /// A state event that failed to deserialize.
    FailedToParseState {
        /// The event `type`.
        event_type: StateEventType,

        /// The state key.
        state_key: String,

        /// The deserialization error.
        error: Arc<serde_json::Error>,
    },

    /// An `m.poll.start` event.
    Poll(PollState),
}

impl TimelineItemContent {
    /// If the supplied event is suitable to be used as a `latest_event` in a
    /// message preview, extract its contents and wrap it as a
    /// `TimelineItemContent`.
    pub(crate) fn from_latest_event_content(
        event: AnySyncTimelineEvent,
    ) -> Option<TimelineItemContent> {
        match is_suitable_for_latest_event(&event) {
            PossibleLatestEvent::YesRoomMessage(m) => {
                Some(Self::from_suitable_latest_event_content(m))
            }
            PossibleLatestEvent::YesPoll(poll) => {
                Some(Self::from_suitable_latest_poll_event_content(poll))
            }
            PossibleLatestEvent::NoUnsupportedEventType => {
                // TODO: when we support state events in message previews, this will need change
                warn!("Found a state event cached as latest_event! ID={}", event.event_id());
                None
            }
            PossibleLatestEvent::NoUnsupportedMessageLikeType => {
                // TODO: When we support reactions in message previews, this will need to change
                warn!(
                    "Found an event cached as latest_event, but I don't know how \
                        to wrap it in a TimelineItemContent. type={}, ID={}",
                    event.event_type().to_string(),
                    event.event_id()
                );
                None
            }
            PossibleLatestEvent::NoEncrypted => {
                warn!("Found an encrypted event cached as latest_event! ID={}", event.event_id());
                None
            }
        }
    }

    /// Given some message content that is from an event that we have already
    /// determined is suitable for use as a latest event in a message preview,
    /// extract its contents and wrap it as a `TimelineItemContent`.
    fn from_suitable_latest_event_content(event: &SyncRoomMessageEvent) -> TimelineItemContent {
        match event {
            SyncRoomMessageEvent::Original(event) => {
                // Grab the content of this event
                let event_content = event.content.clone();

                // We don't have access to any relations via the `AnySyncTimelineEvent` (I think
                // - andyb) so we pretend there are none. This might be OK for
                // the message preview use case.
                let relations = BundledMessageLikeRelations::new();

                // If this message is a reply, we would look up in this list the message it was
                // replying to. Since we probably won't show this in the message preview,
                // it's probably OK to supply an empty list here.
                // `Message::from_event` marks the original event as `Unavailable` if it can't
                // be found inside the timeline_items.
                let timeline_items = Vector::new();
                TimelineItemContent::Message(Message::from_event(
                    event_content,
                    relations,
                    &timeline_items,
                ))
            }
            SyncRoomMessageEvent::Redacted(_) => TimelineItemContent::RedactedMessage,
        }
    }

    /// Extracts a `TimelineItemContent` from a poll start event for use as a
    /// latest event in a message preview.
    fn from_suitable_latest_poll_event_content(
        event: &SyncUnstablePollStartEvent,
    ) -> TimelineItemContent {
        match event {
            SyncUnstablePollStartEvent::Original(event) => {
                TimelineItemContent::Poll(PollState::new(NewUnstablePollStartEventContent::new(
                    event.content.poll_start().clone(),
                )))
            }
            SyncUnstablePollStartEvent::Redacted(_) => TimelineItemContent::RedactedMessage,
        }
    }

    /// If `self` is of the [`Message`][Self::Message] variant, return the inner
    /// [`Message`].
    pub fn as_message(&self) -> Option<&Message> {
        as_variant!(self, Self::Message)
    }

    /// If `self` is of the [`UnableToDecrypt`][Self::UnableToDecrypt] variant,
    /// return the inner [`EncryptedMessage`].
    pub fn as_unable_to_decrypt(&self) -> Option<&EncryptedMessage> {
        as_variant!(self, Self::UnableToDecrypt)
    }

    pub(crate) fn is_redacted(&self) -> bool {
        matches!(self, Self::RedactedMessage)
    }

    // These constructors could also be `From` implementations, but that would
    // allow users to call them directly, which should not be supported
    pub(crate) fn message(
        c: RoomMessageEventContent,
        relations: BundledMessageLikeRelations<AnySyncMessageLikeEvent>,
        timeline_items: &Vector<Arc<TimelineItem>>,
    ) -> Self {
        Self::Message(Message::from_event(c, relations, timeline_items))
    }

    #[cfg(not(tarpaulin_include))] // debug-logging functionality
    pub(crate) fn debug_string(&self) -> &'static str {
        match self {
            TimelineItemContent::Message(_) => "a message",
            TimelineItemContent::RedactedMessage => "a redacted messages",
            TimelineItemContent::Sticker(_) => "a sticker",
            TimelineItemContent::UnableToDecrypt(_) => "a poll",
            TimelineItemContent::MembershipChange(_) => "a membership change",
            TimelineItemContent::ProfileChange(_) => "a profile change",
            TimelineItemContent::OtherState(_) => "a state event",
            TimelineItemContent::FailedToParseMessageLike { .. }
            | TimelineItemContent::FailedToParseState { .. } => "an event that couldn't be parsed",
            TimelineItemContent::Poll(_) => "a poll",
        }
    }

    pub(crate) fn unable_to_decrypt(content: RoomEncryptedEventContent) -> Self {
        Self::UnableToDecrypt(content.into())
    }

    pub(crate) fn room_member(
        user_id: OwnedUserId,
        full_content: FullStateEventContent<RoomMemberEventContent>,
        sender: OwnedUserId,
    ) -> Self {
        use ruma::events::room::member::MembershipChange as MChange;
        match &full_content {
            FullStateEventContent::Original { content, prev_content } => {
                let membership_change = content.membership_change(
                    prev_content.as_ref().map(|c| c.details()),
                    &sender,
                    &user_id,
                );

                if let MChange::ProfileChanged { displayname_change, avatar_url_change } =
                    membership_change
                {
                    Self::ProfileChange(MemberProfileChange {
                        user_id,
                        displayname_change: displayname_change.map(|c| Change {
                            new: c.new.map(ToOwned::to_owned),
                            old: c.old.map(ToOwned::to_owned),
                        }),
                        avatar_url_change: avatar_url_change.map(|c| Change {
                            new: c.new.map(ToOwned::to_owned),
                            old: c.old.map(ToOwned::to_owned),
                        }),
                    })
                } else {
                    let change = match membership_change {
                        MChange::None => MembershipChange::None,
                        MChange::Error => MembershipChange::Error,
                        MChange::Joined => MembershipChange::Joined,
                        MChange::Left => MembershipChange::Left,
                        MChange::Banned => MembershipChange::Banned,
                        MChange::Unbanned => MembershipChange::Unbanned,
                        MChange::Kicked => MembershipChange::Kicked,
                        MChange::Invited => MembershipChange::Invited,
                        MChange::KickedAndBanned => MembershipChange::KickedAndBanned,
                        MChange::InvitationAccepted => MembershipChange::InvitationAccepted,
                        MChange::InvitationRejected => MembershipChange::InvitationRejected,
                        MChange::InvitationRevoked => MembershipChange::InvitationRevoked,
                        MChange::Knocked => MembershipChange::Knocked,
                        MChange::KnockAccepted => MembershipChange::KnockAccepted,
                        MChange::KnockRetracted => MembershipChange::KnockRetracted,
                        MChange::KnockDenied => MembershipChange::KnockDenied,
                        MChange::ProfileChanged { .. } => unreachable!(),
                        _ => MembershipChange::NotImplemented,
                    };

                    Self::MembershipChange(RoomMembershipChange {
                        user_id,
                        content: full_content,
                        change: Some(change),
                    })
                }
            }
            FullStateEventContent::Redacted(_) => Self::MembershipChange(RoomMembershipChange {
                user_id,
                content: full_content,
                change: None,
            }),
        }
    }

    pub(in crate::timeline) fn redact(&self, room_version: &RoomVersionId) -> Self {
        match self {
            Self::Message(_)
            | Self::RedactedMessage
            | Self::Sticker(_)
            | Self::Poll(_)
            | Self::UnableToDecrypt(_) => Self::RedactedMessage,
            Self::MembershipChange(ev) => Self::MembershipChange(ev.redact(room_version)),
            Self::ProfileChange(ev) => Self::ProfileChange(ev.redact()),
            Self::OtherState(ev) => Self::OtherState(ev.redact(room_version)),
            Self::FailedToParseMessageLike { .. } | Self::FailedToParseState { .. } => self.clone(),
        }
    }
}

/// Metadata about an `m.room.encrypted` event that could not be decrypted.
#[derive(Clone, Debug)]
pub enum EncryptedMessage {
    /// Metadata about an event using the `m.olm.v1.curve25519-aes-sha2`
    /// algorithm.
    OlmV1Curve25519AesSha2 {
        /// The Curve25519 key of the sender.
        sender_key: String,
    },
    /// Metadata about an event using the `m.megolm.v1.aes-sha2` algorithm.
    MegolmV1AesSha2 {
        /// The Curve25519 key of the sender.
        #[deprecated = "this field still needs to be sent but should not be used when received"]
        #[doc(hidden)] // Included for Debug formatting only
        sender_key: String,

        /// The ID of the sending device.
        #[deprecated = "this field still needs to be sent but should not be used when received"]
        #[doc(hidden)] // Included for Debug formatting only
        device_id: OwnedDeviceId,

        /// The ID of the session used to encrypt the message.
        session_id: String,
    },
    /// No metadata because the event uses an unknown algorithm.
    Unknown,
}

impl From<RoomEncryptedEventContent> for EncryptedMessage {
    fn from(c: RoomEncryptedEventContent) -> Self {
        match c.scheme {
            EncryptedEventScheme::OlmV1Curve25519AesSha2(s) => {
                Self::OlmV1Curve25519AesSha2 { sender_key: s.sender_key }
            }
            #[allow(deprecated)]
            EncryptedEventScheme::MegolmV1AesSha2(s) => {
                let MegolmV1AesSha2Content { sender_key, device_id, session_id, .. } = s;
                Self::MegolmV1AesSha2 { sender_key, device_id, session_id }
            }
            _ => Self::Unknown,
        }
    }
}

/// An `m.sticker` event.
#[derive(Clone, Debug)]
pub struct Sticker {
    pub(in crate::timeline) content: StickerEventContent,
}

impl Sticker {
    /// Get the data of this sticker.
    pub fn content(&self) -> &StickerEventContent {
        &self.content
    }
}

/// An event changing a room membership.
#[derive(Clone, Debug)]
pub struct RoomMembershipChange {
    pub(in crate::timeline) user_id: OwnedUserId,
    pub(in crate::timeline) content: FullStateEventContent<RoomMemberEventContent>,
    pub(in crate::timeline) change: Option<MembershipChange>,
}

impl RoomMembershipChange {
    /// The ID of the user whose membership changed.
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    /// The full content of the event.
    pub fn content(&self) -> &FullStateEventContent<RoomMemberEventContent> {
        &self.content
    }

    /// The membership change induced by this event.
    ///
    /// If this returns `None`, it doesn't mean that there was no change, but
    /// that the change could not be computed. This is currently always the case
    /// with redacted events.
    // FIXME: Fetch the prev_content when missing so we can compute this with
    // redacted events?
    pub fn change(&self) -> Option<MembershipChange> {
        self.change
    }

    fn redact(&self, room_version: &RoomVersionId) -> Self {
        Self {
            user_id: self.user_id.clone(),
            content: FullStateEventContent::Redacted(self.content.clone().redact(room_version)),
            change: self.change,
        }
    }
}

/// An enum over all the possible room membership changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MembershipChange {
    /// No change.
    None,

    /// Must never happen.
    Error,

    /// User joined the room.
    Joined,

    /// User left the room.
    Left,

    /// User was banned.
    Banned,

    /// User was unbanned.
    Unbanned,

    /// User was kicked.
    Kicked,

    /// User was invited.
    Invited,

    /// User was kicked and banned.
    KickedAndBanned,

    /// User accepted the invite.
    InvitationAccepted,

    /// User rejected the invite.
    InvitationRejected,

    /// User had their invite revoked.
    InvitationRevoked,

    /// User knocked.
    Knocked,

    /// User had their knock accepted.
    KnockAccepted,

    /// User retracted their knock.
    KnockRetracted,

    /// User had their knock denied.
    KnockDenied,

    /// Not implemented.
    NotImplemented,
}

/// An event changing a member's profile.
///
/// Note that profile changes only occur in the timeline when the user's
/// membership is already `join`.
#[derive(Clone, Debug)]
pub struct MemberProfileChange {
    pub(in crate::timeline) user_id: OwnedUserId,
    pub(in crate::timeline) displayname_change: Option<Change<Option<String>>>,
    pub(in crate::timeline) avatar_url_change: Option<Change<Option<OwnedMxcUri>>>,
}

impl MemberProfileChange {
    /// The ID of the user whose profile changed.
    pub fn user_id(&self) -> &UserId {
        &self.user_id
    }

    /// The display name change induced by this event.
    pub fn displayname_change(&self) -> Option<&Change<Option<String>>> {
        self.displayname_change.as_ref()
    }

    /// The avatar URL change induced by this event.
    pub fn avatar_url_change(&self) -> Option<&Change<Option<OwnedMxcUri>>> {
        self.avatar_url_change.as_ref()
    }

    fn redact(&self) -> Self {
        Self {
            user_id: self.user_id.clone(),
            // FIXME: This isn't actually right, the profile is reset to an
            // empty one when the member event is redacted. This can't be
            // implemented without further architectural changes and is a
            // somewhat rare edge case, so it should be fine for now.
            displayname_change: None,
            avatar_url_change: None,
        }
    }
}

/// An enum over all the full state event contents that don't have their own
/// `TimelineItemContent` variant.
#[derive(Clone, Debug)]
pub enum AnyOtherFullStateEventContent {
    /// m.policy.rule.room
    PolicyRuleRoom(FullStateEventContent<PolicyRuleRoomEventContent>),

    /// m.policy.rule.server
    PolicyRuleServer(FullStateEventContent<PolicyRuleServerEventContent>),

    /// m.policy.rule.user
    PolicyRuleUser(FullStateEventContent<PolicyRuleUserEventContent>),

    /// m.room.aliases
    RoomAliases(FullStateEventContent<RoomAliasesEventContent>),

    /// m.room.avatar
    RoomAvatar(FullStateEventContent<RoomAvatarEventContent>),

    /// m.room.canonical_alias
    RoomCanonicalAlias(FullStateEventContent<RoomCanonicalAliasEventContent>),

    /// m.room.create
    RoomCreate(FullStateEventContent<RoomCreateEventContent>),

    /// m.room.encryption
    RoomEncryption(FullStateEventContent<RoomEncryptionEventContent>),

    /// m.room.guest_access
    RoomGuestAccess(FullStateEventContent<RoomGuestAccessEventContent>),

    /// m.room.history_visibility
    RoomHistoryVisibility(FullStateEventContent<RoomHistoryVisibilityEventContent>),

    /// m.room.join_rules
    RoomJoinRules(FullStateEventContent<RoomJoinRulesEventContent>),

    /// m.room.name
    RoomName(FullStateEventContent<RoomNameEventContent>),

    /// m.room.pinned_events
    RoomPinnedEvents(FullStateEventContent<RoomPinnedEventsEventContent>),

    /// m.room.power_levels
    RoomPowerLevels(FullStateEventContent<RoomPowerLevelsEventContent>),

    /// m.room.server_acl
    RoomServerAcl(FullStateEventContent<RoomServerAclEventContent>),

    /// m.room.third_party_invite
    RoomThirdPartyInvite(FullStateEventContent<RoomThirdPartyInviteEventContent>),

    /// m.room.tombstone
    RoomTombstone(FullStateEventContent<RoomTombstoneEventContent>),

    /// m.room.topic
    RoomTopic(FullStateEventContent<RoomTopicEventContent>),

    /// m.space.child
    SpaceChild(FullStateEventContent<SpaceChildEventContent>),

    /// m.space.parent
    SpaceParent(FullStateEventContent<SpaceParentEventContent>),

    #[doc(hidden)]
    _Custom { event_type: String },
}

impl AnyOtherFullStateEventContent {
    /// Create an `AnyOtherFullStateEventContent` from an
    /// `AnyFullStateEventContent`.
    ///
    /// Panics if the event content does not match one of the variants.
    // This could be a `From` implementation but we don't want it in the public API.
    pub(crate) fn with_event_content(content: AnyFullStateEventContent) -> Self {
        let event_type = content.event_type();

        match content {
            AnyFullStateEventContent::PolicyRuleRoom(c) => Self::PolicyRuleRoom(c),
            AnyFullStateEventContent::PolicyRuleServer(c) => Self::PolicyRuleServer(c),
            AnyFullStateEventContent::PolicyRuleUser(c) => Self::PolicyRuleUser(c),
            AnyFullStateEventContent::RoomAliases(c) => Self::RoomAliases(c),
            AnyFullStateEventContent::RoomAvatar(c) => Self::RoomAvatar(c),
            AnyFullStateEventContent::RoomCanonicalAlias(c) => Self::RoomCanonicalAlias(c),
            AnyFullStateEventContent::RoomCreate(c) => Self::RoomCreate(c),
            AnyFullStateEventContent::RoomEncryption(c) => Self::RoomEncryption(c),
            AnyFullStateEventContent::RoomGuestAccess(c) => Self::RoomGuestAccess(c),
            AnyFullStateEventContent::RoomHistoryVisibility(c) => Self::RoomHistoryVisibility(c),
            AnyFullStateEventContent::RoomJoinRules(c) => Self::RoomJoinRules(c),
            AnyFullStateEventContent::RoomName(c) => Self::RoomName(c),
            AnyFullStateEventContent::RoomPinnedEvents(c) => Self::RoomPinnedEvents(c),
            AnyFullStateEventContent::RoomPowerLevels(c) => Self::RoomPowerLevels(c),
            AnyFullStateEventContent::RoomServerAcl(c) => Self::RoomServerAcl(c),
            AnyFullStateEventContent::RoomThirdPartyInvite(c) => Self::RoomThirdPartyInvite(c),
            AnyFullStateEventContent::RoomTombstone(c) => Self::RoomTombstone(c),
            AnyFullStateEventContent::RoomTopic(c) => Self::RoomTopic(c),
            AnyFullStateEventContent::SpaceChild(c) => Self::SpaceChild(c),
            AnyFullStateEventContent::SpaceParent(c) => Self::SpaceParent(c),
            AnyFullStateEventContent::RoomMember(_) => unreachable!(),
            _ => Self::_Custom { event_type: event_type.to_string() },
        }
    }

    /// Get the event's type, like `m.room.create`.
    pub fn event_type(&self) -> StateEventType {
        match self {
            Self::PolicyRuleRoom(c) => c.event_type(),
            Self::PolicyRuleServer(c) => c.event_type(),
            Self::PolicyRuleUser(c) => c.event_type(),
            Self::RoomAliases(c) => c.event_type(),
            Self::RoomAvatar(c) => c.event_type(),
            Self::RoomCanonicalAlias(c) => c.event_type(),
            Self::RoomCreate(c) => c.event_type(),
            Self::RoomEncryption(c) => c.event_type(),
            Self::RoomGuestAccess(c) => c.event_type(),
            Self::RoomHistoryVisibility(c) => c.event_type(),
            Self::RoomJoinRules(c) => c.event_type(),
            Self::RoomName(c) => c.event_type(),
            Self::RoomPinnedEvents(c) => c.event_type(),
            Self::RoomPowerLevels(c) => c.event_type(),
            Self::RoomServerAcl(c) => c.event_type(),
            Self::RoomThirdPartyInvite(c) => c.event_type(),
            Self::RoomTombstone(c) => c.event_type(),
            Self::RoomTopic(c) => c.event_type(),
            Self::SpaceChild(c) => c.event_type(),
            Self::SpaceParent(c) => c.event_type(),
            Self::_Custom { event_type } => event_type.as_str().into(),
        }
    }

    fn redact(&self, room_version: &RoomVersionId) -> Self {
        match self {
            Self::PolicyRuleRoom(c) => Self::PolicyRuleRoom(FullStateEventContent::Redacted(
                c.clone().redact(room_version),
            )),
            Self::PolicyRuleServer(c) => Self::PolicyRuleServer(FullStateEventContent::Redacted(
                c.clone().redact(room_version),
            )),
            Self::PolicyRuleUser(c) => Self::PolicyRuleUser(FullStateEventContent::Redacted(
                c.clone().redact(room_version),
            )),
            Self::RoomAliases(c) => {
                Self::RoomAliases(FullStateEventContent::Redacted(c.clone().redact(room_version)))
            }
            Self::RoomAvatar(c) => {
                Self::RoomAvatar(FullStateEventContent::Redacted(c.clone().redact(room_version)))
            }
            Self::RoomCanonicalAlias(c) => Self::RoomCanonicalAlias(
                FullStateEventContent::Redacted(c.clone().redact(room_version)),
            ),
            Self::RoomCreate(c) => {
                Self::RoomCreate(FullStateEventContent::Redacted(c.clone().redact(room_version)))
            }
            Self::RoomEncryption(c) => Self::RoomEncryption(FullStateEventContent::Redacted(
                c.clone().redact(room_version),
            )),
            Self::RoomGuestAccess(c) => Self::RoomGuestAccess(FullStateEventContent::Redacted(
                c.clone().redact(room_version),
            )),
            Self::RoomHistoryVisibility(c) => Self::RoomHistoryVisibility(
                FullStateEventContent::Redacted(c.clone().redact(room_version)),
            ),
            Self::RoomJoinRules(c) => {
                Self::RoomJoinRules(FullStateEventContent::Redacted(c.clone().redact(room_version)))
            }
            Self::RoomName(c) => {
                Self::RoomName(FullStateEventContent::Redacted(c.clone().redact(room_version)))
            }
            Self::RoomPinnedEvents(c) => Self::RoomPinnedEvents(FullStateEventContent::Redacted(
                c.clone().redact(room_version),
            )),
            Self::RoomPowerLevels(c) => Self::RoomPowerLevels(FullStateEventContent::Redacted(
                c.clone().redact(room_version),
            )),
            Self::RoomServerAcl(c) => {
                Self::RoomServerAcl(FullStateEventContent::Redacted(c.clone().redact(room_version)))
            }
            Self::RoomThirdPartyInvite(c) => Self::RoomThirdPartyInvite(
                FullStateEventContent::Redacted(c.clone().redact(room_version)),
            ),
            Self::RoomTombstone(c) => {
                Self::RoomTombstone(FullStateEventContent::Redacted(c.clone().redact(room_version)))
            }
            Self::RoomTopic(c) => {
                Self::RoomTopic(FullStateEventContent::Redacted(c.clone().redact(room_version)))
            }
            Self::SpaceChild(c) => {
                Self::SpaceChild(FullStateEventContent::Redacted(c.clone().redact(room_version)))
            }
            Self::SpaceParent(c) => {
                Self::SpaceParent(FullStateEventContent::Redacted(c.clone().redact(room_version)))
            }
            Self::_Custom { event_type } => Self::_Custom { event_type: event_type.clone() },
        }
    }
}

/// A state event that doesn't have its own variant.
#[derive(Clone, Debug)]
pub struct OtherState {
    pub(in crate::timeline) state_key: String,
    pub(in crate::timeline) content: AnyOtherFullStateEventContent,
}

impl OtherState {
    /// The state key of the event.
    pub fn state_key(&self) -> &str {
        &self.state_key
    }

    /// The content of the event.
    pub fn content(&self) -> &AnyOtherFullStateEventContent {
        &self.content
    }

    fn redact(&self, room_version: &RoomVersionId) -> Self {
        Self { state_key: self.state_key.clone(), content: self.content.redact(room_version) }
    }
}

#[cfg(test)]
mod tests {
    use assert_matches::assert_matches;
    use matrix_sdk_test::ALICE;
    use ruma::{
        assign,
        events::{
            room::member::{MembershipState, RoomMemberEventContent},
            FullStateEventContent,
        },
        RoomVersionId,
    };

    use super::{MembershipChange, RoomMembershipChange, TimelineItemContent};

    #[test]
    fn redact_membership_change() {
        let content = TimelineItemContent::MembershipChange(RoomMembershipChange {
            user_id: ALICE.to_owned(),
            content: FullStateEventContent::Original {
                content: assign!(RoomMemberEventContent::new(MembershipState::Ban), {
                    reason: Some("🤬".to_owned()),
                }),
                prev_content: Some(RoomMemberEventContent::new(MembershipState::Join)),
            },
            change: Some(MembershipChange::Banned),
        });

        let redacted = content.redact(&RoomVersionId::V11);
        let inner = assert_matches!(redacted, TimelineItemContent::MembershipChange(c) => c);
        assert_eq!(inner.change, Some(MembershipChange::Banned));
        let inner_content_redacted =
            assert_matches!(inner.content, FullStateEventContent::Redacted(r) => r);
        assert_eq!(inner_content_redacted.membership, MembershipState::Ban);
    }
}
