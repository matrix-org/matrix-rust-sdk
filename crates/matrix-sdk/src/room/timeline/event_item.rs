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

use std::{fmt, sync::Arc};

use indexmap::IndexMap;
use matrix_sdk_base::deserialized_responses::EncryptionInfo;
use ruma::{
    events::{
        policy::rule::{
            room::PolicyRuleRoomEventContent, server::PolicyRuleServerEventContent,
            user::PolicyRuleUserEventContent,
        },
        relation::{AnnotationChunk, AnnotationType},
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
            member::{MembershipChange, RoomMemberEventContent},
            message::MessageType,
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
        AnyFullStateEventContent, AnySyncTimelineEvent, FullStateEventContent,
        MessageLikeEventType, StateEventType,
    },
    serde::Raw,
    uint, EventId, MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedEventId, OwnedMxcUri,
    OwnedTransactionId, OwnedUserId, TransactionId, UInt, UserId,
};

/// An item in the timeline that represents at least one event.
///
/// There is always one main event that gives the `EventTimelineItem` its
/// identity (see [key](Self::key)) but in many cases, additional events like
/// reactions and edits are also part of the item.
#[derive(Clone)]
pub struct EventTimelineItem {
    pub(super) key: TimelineKey,
    // If this item is a local echo that has been acknowledged by the server
    // but not remote-echoed yet, this field holds the event ID from the send
    // response.
    pub(super) event_id: Option<OwnedEventId>,
    pub(super) sender: OwnedUserId,
    pub(super) sender_profile: Profile,
    pub(super) content: TimelineItemContent,
    pub(super) reactions: BundledReactions,
    pub(super) timestamp: MilliSecondsSinceUnixEpoch,
    pub(super) is_own: bool,
    pub(super) encryption_info: Option<EncryptionInfo>,
    // FIXME: Expose the raw JSON of aggregated events somehow
    pub(super) raw: Option<Raw<AnySyncTimelineEvent>>,
}

impl fmt::Debug for EventTimelineItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EventTimelineItem")
            .field("key", &self.key)
            .field("event_id", &self.event_id)
            .field("sender", &self.sender)
            .field("content", &self.content)
            .field("reactions", &self.reactions)
            .field("timestamp", &self.timestamp)
            .field("is_own", &self.is_own)
            .field("encryption_info", &self.encryption_info)
            // skip raw, too noisy
            .finish_non_exhaustive()
    }
}

impl EventTimelineItem {
    /// Get the [`TimelineKey`] of this item.
    pub fn key(&self) -> &TimelineKey {
        &self.key
    }

    /// Get the event ID of this item.
    ///
    /// If this returns `Some(_)`, the event was successfully created by the
    /// server.
    ///
    /// Even if the [`key()`](Self::key) of this timeline item holds a
    /// transaction ID, this can be `Some(_)` as the event ID can be known not
    /// just from the remote echo via `sync_events`, but also from the response
    /// of the send request that created the event.
    pub fn event_id(&self) -> Option<&EventId> {
        match &self.key {
            TimelineKey::TransactionId(_) => self.event_id.as_deref(),
            TimelineKey::EventId(id) => Some(id),
        }
    }

    /// Get the sender of this item.
    pub fn sender(&self) -> &UserId {
        &self.sender
    }

    /// Get the profile of the sender.
    pub fn sender_profile(&self) -> &Profile {
        &self.sender_profile
    }

    /// Get the content of this item.
    pub fn content(&self) -> &TimelineItemContent {
        &self.content
    }

    /// Get the reactions of this item.
    pub fn reactions(&self) -> &IndexMap<String, ReactionDetails> {
        // FIXME: Find out the state of incomplete bundled reactions, adjust
        //        Ruma if necessary, return the whole BundledReactions field
        &self.reactions.bundled
    }

    /// Get the timestamp of this item.
    ///
    /// If this event hasn't been echoed back by the server yet, returns the
    /// time the local event was created. Otherwise, returns the origin
    /// server timestamp.
    pub fn timestamp(&self) -> MilliSecondsSinceUnixEpoch {
        self.timestamp
    }

    /// Whether this timeline item was sent by the logged-in user themselves.
    pub fn is_own(&self) -> bool {
        self.is_own
    }

    /// Flag indicating this timeline item can be edited by current user.
    pub fn is_editable(&self) -> bool {
        match &self.content {
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
    /// Returns `None` if this event hasn't been echoed back by the server yet.
    pub fn raw(&self) -> Option<&Raw<AnySyncTimelineEvent>> {
        self.raw.as_ref()
    }

    pub(super) fn to_redacted(&self) -> Self {
        Self {
            // FIXME: Change when we support state events
            content: TimelineItemContent::RedactedMessage,
            reactions: BundledReactions::default(),
            ..self.clone()
        }
    }

    pub(super) fn with_event_id(&self, event_id: Option<OwnedEventId>) -> Self {
        Self { event_id, ..self.clone() }
    }

    pub(super) fn with_content(&self, content: TimelineItemContent) -> Self {
        Self { content, ..self.clone() }
    }

    pub(super) fn with_reactions(&self, reactions: BundledReactions) -> Self {
        Self { reactions, ..self.clone() }
    }
}

/// A unique identifier for a timeline item.
///
/// This identifier is used to find the item in the timeline in order to update
/// its state.
///
/// When an event is created locally, the timeline reflects this with an item
/// that has a [`TransactionId`](Self::TransactionId) key. Once the server has
/// acknowledged the event and given it an ID, that item's key is replaced by
/// [`EventId`](Self::EventId) containing the new ID.
///
/// When an event related to the original event whose ID is stored in a
/// [`TimelineKey`] is received, the key is left untouched, but other parts of
/// the timeline item may be updated. Thus, the current data model is only able
/// to handle relations that reference the initial event that resulted in a
/// timeline item being created, not other related events. At the time of
/// writing, there is no relation that is meant to refer to other events that
/// only exist for their relation (e.g. edits, replies).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TimelineKey {
    /// Transaction ID, for an event that was created locally and hasn't been
    /// acknowledged by the server yet.
    TransactionId(OwnedTransactionId),
    /// Event ID, for an event that is synced with the server.
    EventId(OwnedEventId),
}

impl PartialEq<TransactionId> for TimelineKey {
    fn eq(&self, id: &TransactionId) -> bool {
        matches!(self, TimelineKey::TransactionId(txn_id) if txn_id == id)
    }
}

/// The display name and avatar URL of a room member.
#[derive(Clone, Debug)]
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

    /// An `m.room.member` event.
    RoomMember(RoomMember),

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
    // TODO: Add everything required to display the replied-to event, plus a
    // 'loading' state that is entered at first, until the user requests the
    // reply to be loaded.
    pub(super) in_reply_to: Option<OwnedEventId>,
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

    /// Get the event ID of the event this message is replying to, if any.
    pub fn in_reply_to(&self) -> Option<&EventId> {
        self.in_reply_to.as_deref()
    }

    /// Get the edit state of this message (has been edited: `true` / `false`).
    pub fn is_edited(&self) -> bool {
        self.edited
    }
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // since timeline items are logged, don't include all fields here so
        // people don't leak personal data in bug reports
        f.debug_struct("Message").field("edited", &self.edited).finish_non_exhaustive()
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

#[derive(Clone, Debug)]
pub struct BundledReactions {
    /// Whether all reactions are known, or some may be missing.
    ///
    /// If this is `false`, the remaining reactions can be fetched via **TODO**.
    pub complete: bool, // FIXME: Unclear whether this is needed

    /// The reactions.
    ///
    /// Key: The reaction, usually an emoji.\
    /// Value: The count.
    pub bundled: IndexMap<String, ReactionDetails>,
}

impl From<Box<AnnotationChunk>> for BundledReactions {
    fn from(ann: Box<AnnotationChunk>) -> Self {
        let bundled = ann
            .chunk
            .into_iter()
            .filter_map(|a| {
                (a.annotation_type == AnnotationType::Reaction).then(|| {
                    let details =
                        ReactionDetails { count: a.count, senders: TimelineDetails::Unavailable };
                    (a.key, details)
                })
            })
            .collect();

        BundledReactions { bundled, complete: ann.next_batch.is_none() }
    }
}

impl Default for BundledReactions {
    fn default() -> Self {
        Self { complete: true, bundled: IndexMap::new() }
    }
}

/// The details of a group of reaction events on the same event with the same
/// key.
#[derive(Clone, Debug)]
pub struct ReactionDetails {
    /// The amount of reactions with this key.
    pub count: UInt,

    /// The senders of the reactions.
    pub senders: TimelineDetails<Vec<OwnedUserId>>,
}

impl Default for ReactionDetails {
    fn default() -> Self {
        Self { count: uint!(0), senders: TimelineDetails::Ready(Vec::new()) }
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

/// An `m.room.member` event.
#[derive(Clone, Debug)]
pub struct RoomMember {
    pub(super) user_id: OwnedUserId,
    pub(super) content: FullStateEventContent<RoomMemberEventContent>,
    pub(super) sender: OwnedUserId,
}

impl RoomMember {
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
    pub fn membership_change(&self) -> Option<MembershipChange<'_>> {
        match &self.content {
            FullStateEventContent::Original { content, prev_content } => {
                Some(content.membership_change(
                    prev_content.as_ref().map(|c| c.details()),
                    &self.sender,
                    &self.user_id,
                ))
            }
            FullStateEventContent::Redacted(_) => None,
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
