// Copyright 2022 The Matrix.org Foundation C.I.C.
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

use std::{fmt, ops::Deref, sync::Arc};

use indexmap::IndexMap;
use matrix_sdk_base::deserialized_responses::{EncryptionInfo, TimelineEvent};
use ruma::{
    events::{
        policy::rule::{
            room::PolicyRuleRoomEventContent, server::PolicyRuleServerEventContent,
            user::PolicyRuleUserEventContent,
        },
        receipt::Receipt,
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
            message::{self, MessageType, Relation},
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
        AnyFullStateEventContent, AnyMessageLikeEventContent, AnySyncTimelineEvent,
        AnyTimelineEvent, FullStateEventContent, MessageLikeEventType, StateEventType,
    },
    serde::Raw,
    EventId, MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedEventId, OwnedMxcUri,
    OwnedTransactionId, OwnedUserId, TransactionId, UserId,
};

use super::inner::RoomDataProvider;
use crate::{Error, Result};

/// An item in the timeline that represents at least one event.
///
/// There is always one main event that gives the `EventTimelineItem` its
/// identity but in many cases, additional events like reactions and edits are
/// also part of the item.
#[derive(Debug, Clone)]
pub enum EventTimelineItem {
    /// An event item that has been sent, but not yet acknowledged by the
    /// server.
    Local(LocalEventTimelineItem),
    /// An event item that has eben sent _and_ acknowledged by the server.
    Remote(RemoteEventTimelineItem),
}

impl EventTimelineItem {
    /// Get the `LocalEventTimelineItem` if `self` is `Local`.
    pub fn as_local(&self) -> Option<&LocalEventTimelineItem> {
        match self {
            Self::Local(local_event_item) => Some(local_event_item),
            Self::Remote(_) => None,
        }
    }

    /// Get the `RemoteEventTimelineItem` if `self` is `Remote`.
    pub fn as_remote(&self) -> Option<&RemoteEventTimelineItem> {
        match self {
            Self::Local(_) => None,
            Self::Remote(remote_event_item) => Some(remote_event_item),
        }
    }

    /// Get a unique identifier to identify the event item, either by using
    /// transaction ID or event ID in case of a local event, or by event ID in
    /// case of a remote event.
    pub fn unique_identifier(&self) -> String {
        match self {
            Self::Local(LocalEventTimelineItem { transaction_id, send_state, .. }) => {
                match send_state {
                    EventSendState::Sent { event_id } => event_id.to_string(),
                    _ => transaction_id.to_string(),
                }
            }

            Self::Remote(RemoteEventTimelineItem { event_id, .. }) => event_id.to_string(),
        }
    }

    /// Get the transaction ID of this item.
    ///
    /// The transaction ID is only kept until the remote echo for a local event
    /// is received, at which point the `EventTimelineItem::Local` is
    /// transformed to `EventTimelineItem::Remote` and the transaction ID
    /// discarded.
    pub fn transaction_id(&self) -> Option<&TransactionId> {
        match self {
            Self::Local(local) => Some(&local.transaction_id),
            Self::Remote(_) => None,
        }
    }

    /// Get the event ID of this item.
    ///
    /// If this returns `Some(_)`, the event was successfully created by the
    /// server.
    ///
    /// Even if this is a [`Local`](Self::Local) event,, this can be `Some(_)`
    /// as the event ID can be known not just from the remote echo via
    /// `sync_events`, but also from the response of the send request that
    /// created the event.
    pub fn event_id(&self) -> Option<&EventId> {
        match self {
            Self::Local(local_event) => local_event.event_id(),
            Self::Remote(remote_event) => Some(&remote_event.event_id),
        }
    }

    /// Get the sender of this item.
    pub fn sender(&self) -> &UserId {
        match self {
            Self::Local(local_event) => &local_event.sender,
            Self::Remote(remote_event) => &remote_event.sender,
        }
    }

    /// Get the profile of the sender.
    pub fn sender_profile(&self) -> &TimelineDetails<Profile> {
        match self {
            Self::Local(local_event) => &local_event.sender_profile,
            Self::Remote(remote_event) => &remote_event.sender_profile,
        }
    }

    /// Get the content of this item.
    pub fn content(&self) -> &TimelineItemContent {
        match self {
            Self::Local(local_event) => &local_event.content,
            Self::Remote(remote_event) => &remote_event.content,
        }
    }

    /// Get the timestamp of this item.
    ///
    /// If this event hasn't been echoed back by the server yet, returns the
    /// time the local event was created. Otherwise, returns the origin
    /// server timestamp.
    pub fn timestamp(&self) -> MilliSecondsSinceUnixEpoch {
        match self {
            Self::Local(local_event) => local_event.timestamp,
            Self::Remote(remote_event) => remote_event.timestamp,
        }
    }

    /// Whether this timeline item was sent by the logged-in user themselves.
    pub fn is_own(&self) -> bool {
        match self {
            Self::Local(_) => true,
            Self::Remote(remote_event) => remote_event.is_own,
        }
    }

    /// Flag indicating this timeline item can be edited by current user.
    pub fn is_editable(&self) -> bool {
        match self.content() {
            TimelineItemContent::Message(message) => {
                self.is_own()
                    && matches!(message.msgtype(), MessageType::Text(_) | MessageType::Emote(_))
            }
            _ => false,
        }
    }

    /// Get the raw JSON representation of the initial event (the one that
    /// caused this timeline item to be created).
    ///
    /// Returns `None` if this event hasn't been echoed back by the server
    /// yet.
    pub fn raw(&self) -> Option<&Raw<AnySyncTimelineEvent>> {
        match self {
            Self::Local(_local_event) => None,
            Self::Remote(remote_event) => Some(&remote_event.raw),
        }
    }

    /// Clone the current event item, and update its `content`.
    pub(super) fn with_content(&self, content: TimelineItemContent) -> Self {
        match self {
            Self::Local(local_event_item) => {
                Self::Local(LocalEventTimelineItem { content, ..local_event_item.clone() })
            }
            Self::Remote(remote_event_item) => {
                Self::Remote(RemoteEventTimelineItem { content, ..remote_event_item.clone() })
            }
        }
    }

    /// Clone the current event item, and update its `sender_profile`.
    pub(super) fn with_sender_profile(&self, sender_profile: TimelineDetails<Profile>) -> Self {
        match self {
            EventTimelineItem::Local(item) => {
                Self::Local(LocalEventTimelineItem { sender_profile, ..item.clone() })
            }
            EventTimelineItem::Remote(item) => {
                Self::Remote(RemoteEventTimelineItem { sender_profile, ..item.clone() })
            }
        }
    }
}

/// This type represents the "send state" of a local event timeline item.
#[derive(Clone, Debug)]
pub enum EventSendState {
    /// The local event has not been sent yet.
    NotSentYet,
    /// The local event has been sent to the server, but unsuccessfully: The
    /// sending has failed.
    SendingFailed {
        /// Details about how sending the event failed.
        error: Arc<Error>,
    },
    /// The local event has been sent successfully to the server.
    Sent {
        /// The event ID assigned by the server.
        event_id: OwnedEventId,
    },
}

/// An item for an event that was created locally and not yet echoed back by the
/// homeserver.
#[derive(Debug, Clone)]
pub struct LocalEventTimelineItem {
    /// The send state of this local event.
    pub send_state: EventSendState,
    /// The transaction ID.
    pub transaction_id: OwnedTransactionId,
    /// The sender of the event.
    pub sender: OwnedUserId,
    /// The sender's profile of the event.
    pub sender_profile: TimelineDetails<Profile>,
    /// The timestamp of the event.
    pub timestamp: MilliSecondsSinceUnixEpoch,
    /// The content of the event.
    pub content: TimelineItemContent,
}

impl LocalEventTimelineItem {
    /// Get the event ID of this item.
    ///
    /// Will be `Some` if and only if `send_state` is `EventSendState::Sent`.
    pub fn event_id(&self) -> Option<&EventId> {
        match &self.send_state {
            EventSendState::Sent { event_id } => Some(event_id),
            _ => None,
        }
    }

    /// Clone the current event item, and update its `send_state`.
    pub(super) fn with_send_state(&self, send_state: EventSendState) -> Self {
        Self { send_state, ..self.clone() }
    }
}

impl From<LocalEventTimelineItem> for EventTimelineItem {
    fn from(value: LocalEventTimelineItem) -> Self {
        Self::Local(value)
    }
}

/// An item for an event that was received from the homeserver.
#[derive(Clone)]
pub struct RemoteEventTimelineItem {
    /// The event ID.
    pub event_id: OwnedEventId,
    /// The sender of the event.
    pub sender: OwnedUserId,
    /// The sender's profile of the event.
    pub sender_profile: TimelineDetails<Profile>,
    /// The timestamp of the event.
    pub timestamp: MilliSecondsSinceUnixEpoch,
    /// The content of the event.
    pub content: TimelineItemContent,
    /// All bundled reactions about the event.
    pub reactions: BundledReactions,
    /// All read receipts for the event.
    ///
    /// The key is the ID of a room member and the value are details about the
    /// read receipt.
    ///
    /// Note that currently this ignores threads.
    pub read_receipts: IndexMap<OwnedUserId, Receipt>,
    /// Whether the event has been sent by the the logged-in user themselves.
    pub is_own: bool,
    /// Encryption information.
    pub encryption_info: Option<EncryptionInfo>,
    // FIXME: Expose the raw JSON of aggregated events somehow
    pub raw: Raw<AnySyncTimelineEvent>,
}

impl RemoteEventTimelineItem {
    /// Clone the current event item, and update its `reactions`.
    pub(super) fn with_reactions(&self, reactions: BundledReactions) -> Self {
        Self { reactions, ..self.clone() }
    }

    /// Clone the current event item, and update its `content`.
    pub(super) fn with_content(&self, content: TimelineItemContent) -> Self {
        Self { content, ..self.clone() }
    }

    /// Clone the current event item, change its `content` to
    /// [`TimelineItemContent::RedactedMessage`], and reset its `reactions`.
    pub(super) fn to_redacted(&self) -> Self {
        Self {
            // FIXME: Change when we support state events
            content: TimelineItemContent::RedactedMessage,
            reactions: BundledReactions::default(),
            ..self.clone()
        }
    }

    /// Get the reactions of this item.
    pub fn reactions(&self) -> &BundledReactions {
        // FIXME: Find out the state of incomplete bundled reactions, adjust
        //        Ruma if necessary, return the whole BundledReactions field
        &self.reactions
    }
}

impl From<RemoteEventTimelineItem> for EventTimelineItem {
    fn from(value: RemoteEventTimelineItem) -> Self {
        Self::Remote(value)
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for RemoteEventTimelineItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteEventTimelineItem")
            .field("event_id", &self.event_id)
            .field("sender", &self.sender)
            .field("timestamp", &self.timestamp)
            .field("content", &self.content)
            .field("reactions", &self.reactions)
            .field("is_own", &self.is_own)
            .field("encryption_info", &self.encryption_info)
            // skip raw, too noisy
            .finish_non_exhaustive()
    }
}

/// The display name and avatar URL of a room member.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    /// The display name, if set.
    pub display_name: Option<String>,
    /// Whether the display name is ambiguous.
    ///
    /// Note that in rooms with lazy-loading enabled, this could be `false` even
    /// though the display name is actually ambiguous if not all member events
    /// have been seen yet.
    pub display_name_ambiguous: bool,
    /// The avatar URL, if set.
    pub avatar_url: Option<OwnedMxcUri>,
}

/// Some details of an [`EventTimelineItem`] that may require server requests
/// other than just the regular
/// [`sync_events`][ruma::api::client::sync::sync_events].
#[derive(Clone, Debug)]
pub enum TimelineDetails<T> {
    /// The details are not available yet, and have not been request from the
    /// server.
    Unavailable,

    /// The details are not available yet, but have been requested.
    Pending,

    /// The details are available.
    Ready(T),

    /// An error occurred when fetching the details.
    Error(Arc<Error>),
}

impl<T> TimelineDetails<T> {
    pub(crate) fn from_initial_value(value: Option<T>) -> Self {
        match value {
            Some(v) => Self::Ready(v),
            None => Self::Unavailable,
        }
    }

    pub(crate) fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable)
    }

    pub(crate) fn contains<U>(&self, value: &U) -> bool
    where
        T: PartialEq<U>,
    {
        matches!(self, Self::Ready(v) if v == value)
    }
}

/// The content of an [`EventTimelineItem`].
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
}

impl TimelineItemContent {
    /// If `self` is of the [`Message`][Self::Message] variant, return the inner
    /// [`Message`].
    pub fn as_message(&self) -> Option<&Message> {
        match self {
            Self::Message(v) => Some(v),
            _ => None,
        }
    }

    /// If `self` is of the [`UnableToDecrypt`][Self::UnableToDecrypt] variant,
    /// return the inner [`EncryptedMessage`].
    pub fn as_unable_to_decrypt(&self) -> Option<&EncryptedMessage> {
        match self {
            Self::UnableToDecrypt(v) => Some(v),
            _ => None,
        }
    }
}

/// An `m.room.message` event or extensible event, including edits.
#[derive(Clone)]
pub struct Message {
    pub(super) msgtype: MessageType,
    pub(super) in_reply_to: Option<InReplyToDetails>,
    pub(super) edited: bool,
}

impl Message {
    /// Get the `msgtype`-specific data of this message.
    pub fn msgtype(&self) -> &MessageType {
        &self.msgtype
    }

    /// Get a reference to the message body.
    ///
    /// Shorthand for `.msgtype().body()`.
    pub fn body(&self) -> &str {
        self.msgtype.body()
    }

    /// Get the event this message is replying to, if any.
    pub fn in_reply_to(&self) -> Option<&InReplyToDetails> {
        self.in_reply_to.as_ref()
    }

    /// Get the edit state of this message (has been edited: `true` / `false`).
    pub fn is_edited(&self) -> bool {
        self.edited
    }

    pub(super) fn with_in_reply_to(&self, in_reply_to: InReplyToDetails) -> Self {
        Self { in_reply_to: Some(in_reply_to), ..self.clone() }
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // since timeline items are logged, don't include all fields here so
        // people don't leak personal data in bug reports
        f.debug_struct("Message").field("edited", &self.edited).finish_non_exhaustive()
    }
}

/// Details about an event being replied to.
#[derive(Clone, Debug)]
pub struct InReplyToDetails {
    /// The ID of the event.
    pub event_id: OwnedEventId,

    /// The details of the event.
    ///
    /// Use [`Timeline::fetch_item_details`] to fetch the data if it is
    /// unavailable. The `replies_nesting_level` field in
    /// [`TimelineDetailsSettings`] decides if this should be fetched.
    ///
    /// [`Timeline::fetch_item_details`]: super::Timeline::fetch_item_details
    /// [`TimelineDetailsSettings`]: super::TimelineDetailsSettings
    pub details: TimelineDetails<Box<RepliedToEvent>>,
}

impl InReplyToDetails {
    pub(super) fn from_relation<C>(relation: Relation<C>) -> Option<Self> {
        match relation {
            message::Relation::Reply { in_reply_to } => {
                Some(Self { event_id: in_reply_to.event_id, details: TimelineDetails::Unavailable })
            }
            _ => None,
        }
    }
}

/// An event that is replied to.
#[derive(Clone, Debug)]
pub struct RepliedToEvent {
    pub(super) message: Message,
    pub(super) sender: OwnedUserId,
    pub(super) sender_profile: TimelineDetails<Profile>,
}

impl RepliedToEvent {
    /// Get the message of this event.
    pub fn message(&self) -> &Message {
        &self.message
    }

    /// Get the sender of this event.
    pub fn sender(&self) -> &UserId {
        &self.sender
    }

    /// Get the profile of the sender.
    pub fn sender_profile(&self) -> &TimelineDetails<Profile> {
        &self.sender_profile
    }

    pub(super) async fn try_from_timeline_event<P: RoomDataProvider>(
        timeline_event: TimelineEvent,
        room_data_provider: &P,
    ) -> Result<Self> {
        let event = match timeline_event.event.deserialize() {
            Ok(AnyTimelineEvent::MessageLike(event)) => event,
            _ => {
                return Err(super::Error::UnsupportedEvent.into());
            }
        };

        let Some(AnyMessageLikeEventContent::RoomMessage(c)) = event.original_content() else {
            return Err(super::Error::UnsupportedEvent.into());
        };

        let message = Message {
            msgtype: c.msgtype,
            in_reply_to: c.relates_to.and_then(InReplyToDetails::from_relation),
            edited: event.relations().replace.is_some(),
        };
        let sender = event.sender().to_owned();
        let sender_profile =
            TimelineDetails::from_initial_value(room_data_provider.profile(&sender).await);

        Ok(Self { message, sender, sender_profile })
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

/// The reactions grouped by key.
///
/// Key: The reaction, usually an emoji.\
/// Value: The group of reactions.
pub type BundledReactions = IndexMap<String, ReactionGroup>;

/// A group of reaction events on the same event with the same key.
///
/// This is a map of the event ID or transaction ID of the reactions to the ID
/// of the sender of the reaction.
#[derive(Clone, Debug, Default)]
pub struct ReactionGroup(
    pub(super) IndexMap<(Option<OwnedTransactionId>, Option<OwnedEventId>), OwnedUserId>,
);

impl ReactionGroup {
    /// The senders of the reactions in this group.
    pub fn senders(&self) -> impl Iterator<Item = &UserId> {
        self.values().map(AsRef::as_ref)
    }
}

impl Deref for ReactionGroup {
    type Target = IndexMap<(Option<OwnedTransactionId>, Option<OwnedEventId>), OwnedUserId>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// An `m.sticker` event.
#[derive(Clone, Debug)]
pub struct Sticker {
    pub(super) content: StickerEventContent,
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
    pub(super) user_id: OwnedUserId,
    pub(super) content: FullStateEventContent<RoomMemberEventContent>,
    pub(super) change: Option<MembershipChange>,
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
    pub(super) user_id: OwnedUserId,
    pub(super) displayname_change: Option<Change<Option<String>>>,
    pub(super) avatar_url_change: Option<Change<Option<OwnedMxcUri>>>,
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
}

/// A state event that doesn't have its own variant.
#[derive(Clone, Debug)]
pub struct OtherState {
    pub(super) state_key: String,
    pub(super) content: AnyOtherFullStateEventContent,
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
}
