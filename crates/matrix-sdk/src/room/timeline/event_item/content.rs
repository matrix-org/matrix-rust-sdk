use std::{fmt, ops::Deref, sync::Arc};

use imbl::{vector, Vector};
use indexmap::IndexMap;
use matrix_sdk_base::deserialized_responses::TimelineEvent;
use ruma::{
    events::{
        policy::rule::{
            room::PolicyRuleRoomEventContent, server::PolicyRuleServerEventContent,
            user::PolicyRuleUserEventContent,
        },
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
            message::{
                self, sanitize::RemoveReplyFallback, MessageType, Relation,
                RoomMessageEventContent, SyncRoomMessageEvent,
            },
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
        AnyFullStateEventContent, AnyMessageLikeEventContent, AnySyncMessageLikeEvent,
        AnyTimelineEvent, BundledMessageLikeRelations, FullStateEventContent, MessageLikeEventType,
        StateEventType,
    },
    OwnedDeviceId, OwnedEventId, OwnedMxcUri, OwnedTransactionId, OwnedUserId, UserId,
};
use tracing::{debug, error};

use super::{EventTimelineItem, Profile, TimelineDetails};
use crate::{
    room::timeline::{
        inner::RoomDataProvider, Error as TimelineError, TimelineItem, DEFAULT_SANITIZER_MODE,
    },
    Result,
};

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

    pub(crate) fn is_redacted(&self) -> bool {
        matches!(self, Self::RedactedMessage)
    }
}

/// An `m.room.message` event or extensible event, including edits.
#[derive(Clone)]
pub struct Message {
    pub(in crate::room::timeline) msgtype: MessageType,
    pub(in crate::room::timeline) in_reply_to: Option<InReplyToDetails>,
    pub(in crate::room::timeline) edited: bool,
}

impl Message {
    /// Construct a `Message` from a `m.room.message` event.
    pub(in crate::room::timeline) fn from_event(
        c: RoomMessageEventContent,
        relations: BundledMessageLikeRelations<AnySyncMessageLikeEvent>,
        timeline_items: &Vector<Arc<TimelineItem>>,
    ) -> Self {
        let edited = relations.has_replacement();
        let edit = relations.replace.and_then(|r| match *r {
            AnySyncMessageLikeEvent::RoomMessage(SyncRoomMessageEvent::Original(ev)) => match ev
                .content
                .relates_to
            {
                Some(Relation::Replacement(re)) => Some(re),
                _ => {
                    error!("got m.room.message event with an edit without a valid m.replace relation");
                    None
                }
            },
            AnySyncMessageLikeEvent::RoomMessage(SyncRoomMessageEvent::Redacted(_)) => None,
            _ => {
                error!("got m.room.message event with an edit of a different event type");
                None
            }
        });

        let in_reply_to = c.relates_to.and_then(|relation| match relation {
            message::Relation::Reply { in_reply_to } => {
                let event_id = in_reply_to.event_id;
                let event = timeline_items
                    .iter()
                    .filter_map(|it| it.as_event())
                    .find(|it| it.event_id() == Some(&*event_id))
                    .and_then(RepliedToEvent::from_timeline_item)
                    .map(Box::new);

                Some(InReplyToDetails {
                    event_id,
                    event: TimelineDetails::from_initial_value(event),
                })
            }
            _ => None,
        });

        let msgtype = match edit {
            Some(mut e) => {
                // Edit's content is never supposed to contain the reply fallback.
                e.new_content.sanitize(DEFAULT_SANITIZER_MODE, RemoveReplyFallback::No);
                e.new_content
            }
            None => {
                let remove_reply_fallback = if in_reply_to.is_some() {
                    RemoveReplyFallback::Yes
                } else {
                    RemoveReplyFallback::No
                };

                let mut msgtype = c.msgtype;
                msgtype.sanitize(DEFAULT_SANITIZER_MODE, remove_reply_fallback);
                msgtype
            }
        };

        Self { msgtype, in_reply_to, edited }
    }

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

    pub(in crate::room::timeline) fn with_in_reply_to(
        &self,
        in_reply_to: InReplyToDetails,
    ) -> Self {
        Self { in_reply_to: Some(in_reply_to), ..self.clone() }
    }
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { msgtype: _, in_reply_to, edited } = self;
        // since timeline items are logged, don't include all fields here so
        // people don't leak personal data in bug reports
        f.debug_struct("Message")
            .field("in_reply_to", in_reply_to)
            .field("edited", edited)
            .finish_non_exhaustive()
    }
}

/// Details about an event being replied to.
#[derive(Clone, Debug)]
pub struct InReplyToDetails {
    /// The ID of the event.
    pub event_id: OwnedEventId,

    /// The details of the event.
    ///
    /// Use [`Timeline::fetch_details_for_event`] to fetch the data if it is
    /// unavailable.
    ///
    /// [`Timeline::fetch_details_for_event`]: crate::room::timeline::Timeline::fetch_details_for_event
    pub event: TimelineDetails<Box<RepliedToEvent>>,
}

/// An event that is replied to.
#[derive(Clone, Debug)]
pub struct RepliedToEvent {
    pub(in crate::room::timeline) message: Message,
    pub(in crate::room::timeline) sender: OwnedUserId,
    pub(in crate::room::timeline) sender_profile: TimelineDetails<Profile>,
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

    fn from_timeline_item(timeline_item: &EventTimelineItem) -> Option<Self> {
        let message = match &timeline_item.content {
            TimelineItemContent::Message(msg) => msg.to_owned(),
            // FIXME: Handle redacted messages
            _ => {
                debug!("Replied-to event is not a message, discarding");
                return None;
            }
        };

        Some(Self {
            message,
            sender: timeline_item.sender.clone(),
            sender_profile: timeline_item.sender_profile.clone(),
        })
    }

    pub(in crate::room::timeline) async fn try_from_timeline_event<P: RoomDataProvider>(
        timeline_event: TimelineEvent,
        room_data_provider: &P,
    ) -> Result<Self> {
        let event = match timeline_event.event.deserialize() {
            Ok(AnyTimelineEvent::MessageLike(event)) => event,
            _ => {
                return Err(TimelineError::UnsupportedEvent.into());
            }
        };

        let Some(AnyMessageLikeEventContent::RoomMessage(c)) = event.original_content() else {
            return Err(TimelineError::UnsupportedEvent.into());
        };

        let message = Message::from_event(c, event.relations(), &vector![]);
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

// The long type after a long visibility specified trips up rustfmt currently.
// This works around. Report: https://github.com/rust-lang/rustfmt/issues/5703
type ReactionGroupInner = IndexMap<(Option<OwnedTransactionId>, Option<OwnedEventId>), OwnedUserId>;

/// A group of reaction events on the same event with the same key.
///
/// This is a map of the event ID or transaction ID of the reactions to the ID
/// of the sender of the reaction.
#[derive(Clone, Debug, Default)]
pub struct ReactionGroup(pub(in crate::room::timeline) ReactionGroupInner);

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
    pub(in crate::room::timeline) content: StickerEventContent,
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
    pub(in crate::room::timeline) user_id: OwnedUserId,
    pub(in crate::room::timeline) content: FullStateEventContent<RoomMemberEventContent>,
    pub(in crate::room::timeline) change: Option<MembershipChange>,
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
    pub(in crate::room::timeline) user_id: OwnedUserId,
    pub(in crate::room::timeline) displayname_change: Option<Change<Option<String>>>,
    pub(in crate::room::timeline) avatar_url_change: Option<Change<Option<OwnedMxcUri>>>,
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
    pub(in crate::room::timeline) state_key: String,
    pub(in crate::room::timeline) content: AnyOtherFullStateEventContent,
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
