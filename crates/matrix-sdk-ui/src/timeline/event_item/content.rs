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

use std::{collections::{BTreeMap, HashMap}, fmt, ops::Deref, sync::Arc};

use imbl::{vector, Vector};
use indexmap::IndexMap;
use itertools::Itertools;
use matrix_sdk::{deserialized_responses::TimelineEvent, Result};
use matrix_sdk_base::latest_event::{is_suitable_for_latest_event, PossibleLatestEvent};
use ruma::{
    assign, EventId,
    events::{
        policy::rule::{
            room::PolicyRuleRoomEventContent, server::PolicyRuleServerEventContent,
            user::PolicyRuleUserEventContent,
        },
        poll::{start::PollKind, unstable_start::UnstablePollStartContentBlock},
        relation::InReplyTo,
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
        AnySyncTimelineEvent, AnyTimelineEvent, BundledMessageLikeRelations, FullStateEventContent,
        MessageLikeEventType, OriginalSyncMessageLikeEvent, StateEventType,
    },
    MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedEventId, OwnedMxcUri, OwnedTransactionId,
    OwnedUserId, RoomVersionId, uint, UserId
};
use tracing::{debug, error, warn};

use super::{EventItemIdentifier, EventTimelineItem, Profile, TimelineDetails};
use crate::timeline::{
    traits::RoomDataProvider, Error as TimelineError, ReactionSenderData, TimelineItem,
    DEFAULT_SANITIZER_MODE,
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

    /// An `m.poll.start` event.
    Poll(PollState),

    /// An `m.poll.end` event.
    PollEnd(PollEnd),

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
    /// If the supplied event is suitable to be used as a latest_event in a
    /// message preview, extract its contents and wrap it as a
    /// TimelineItemContent.
    pub(crate) fn from_latest_event_content(
        event: AnySyncTimelineEvent,
    ) -> Option<TimelineItemContent> {
        match is_suitable_for_latest_event(&event) {
            PossibleLatestEvent::YesMessageLike(m) => Self::from_suitable_latest_event_content(m),
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
            PossibleLatestEvent::NoRedacted => {
                warn!("Found a redacted event cached as latest_event! ID={}", event.event_id());
                None
            }
        }
    }

    /// Given some message content that is from an event that we have already
    /// determined is suitable for use as a latest event in a message preview,
    /// extract its contents and wrap it as a TimelineItemContent.
    fn from_suitable_latest_event_content(
        message: &OriginalSyncMessageLikeEvent<RoomMessageEventContent>,
    ) -> Option<TimelineItemContent> {
        // Grab the content of this event
        let event_content = message.content.clone();

        // We don't have access to any relations via the AnySyncTimelineEvent (I think -
        // andyb) so we pretend there are none. This might be OK for the message preview
        // use case.
        let relations = BundledMessageLikeRelations::new();

        // If this message is a reply, we would look up in this list the message it was
        // replying to. Since we probably won't show this in the message preview,
        // it's probably OK to supply an empty list here.
        // Message::from_event marks the original event as Unavailable if it can't be
        // found inside the timeline_items.
        let timeline_items = Vector::new();
        Some(TimelineItemContent::Message(Message::from_event(
            event_content,
            relations,
            &timeline_items,
        )))
    }

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

    // These constructors could also be `From` implementations, but that would
    // allow users to call them directly, which should not be supported
    pub(crate) fn message(
        c: RoomMessageEventContent,
        relations: BundledMessageLikeRelations<AnySyncMessageLikeEvent>,
        timeline_items: &Vector<Arc<TimelineItem>>,
    ) -> Self {
        Self::Message(Message::from_event(c, relations, timeline_items))
    }

    pub(crate) fn unable_to_decrypt(content: RoomEncryptedEventContent) -> Self {
        TimelineItemContent::UnableToDecrypt(content.into())
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
                    TimelineItemContent::ProfileChange(MemberProfileChange {
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

                    TimelineItemContent::MembershipChange(RoomMembershipChange {
                        user_id,
                        content: full_content,
                        change: Some(change),
                    })
                }
            }
            FullStateEventContent::Redacted(_) => {
                TimelineItemContent::MembershipChange(RoomMembershipChange {
                    user_id,
                    content: full_content,
                    change: None,
                })
            }
        }
    }

    pub(in crate::timeline) fn redact(&self, room_version: &RoomVersionId) -> Self {
        match self {
            Self::Message(_)
            | Self::Poll(_)
            | Self::PollEnd(_)
            | Self::RedactedMessage
            | Self::Sticker(_)
            | Self::UnableToDecrypt(_) => Self::RedactedMessage,
            Self::MembershipChange(ev) => Self::MembershipChange(ev.redact(room_version)),
            Self::ProfileChange(ev) => Self::ProfileChange(ev.redact()),
            Self::OtherState(ev) => Self::OtherState(ev.redact(room_version)),
            Self::FailedToParseMessageLike { .. } | Self::FailedToParseState { .. } => self.clone(),
        }
    }
}

/// An `m.room.message` event or extensible event, including edits.
#[derive(Clone)]
pub struct Message {
    pub(in crate::timeline) msgtype: MessageType,
    pub(in crate::timeline) in_reply_to: Option<InReplyToDetails>,
    pub(in crate::timeline) edited: bool,
}

impl Message {
    /// Construct a `Message` from a `m.room.message` event.
    pub(in crate::timeline) fn from_event(
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
                Some(InReplyToDetails::new(in_reply_to.event_id, timeline_items))
            }
            message::Relation::Thread(thread) => thread
                .in_reply_to
                .map(|in_reply_to| InReplyToDetails::new(in_reply_to.event_id, timeline_items)),
            _ => None,
        });

        let msgtype = match edit {
            Some(mut e) => {
                // Edit's content is never supposed to contain the reply fallback.
                e.new_content.msgtype.sanitize(DEFAULT_SANITIZER_MODE, RemoveReplyFallback::No);
                e.new_content.msgtype
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

    pub(in crate::timeline) fn with_in_reply_to(&self, in_reply_to: InReplyToDetails) -> Self {
        Self { in_reply_to: Some(in_reply_to), ..self.clone() }
    }
}

impl From<Message> for RoomMessageEventContent {
    fn from(msg: Message) -> Self {
        let relates_to = msg.in_reply_to.map(|details| message::Relation::Reply {
            in_reply_to: InReplyTo::new(details.event_id),
        });
        assign!(Self::new(msg.msgtype), { relates_to })
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
    /// [`Timeline::fetch_details_for_event`]: crate::Timeline::fetch_details_for_event
    pub event: TimelineDetails<Box<RepliedToEvent>>,
}

impl InReplyToDetails {
    pub fn new(
        event_id: OwnedEventId,
        timeline_items: &Vector<Arc<TimelineItem>>,
    ) -> InReplyToDetails {
        let event = timeline_items
            .iter()
            .filter_map(|it| it.as_event())
            .find(|it| it.event_id() == Some(&*event_id))
            .map(|item| Box::new(RepliedToEvent::from_timeline_item(item)));

        InReplyToDetails { event_id, event: TimelineDetails::from_initial_value(event) }
    }
}

/// An event that is replied to.
#[derive(Clone, Debug)]
pub struct RepliedToEvent {
    pub(in crate::timeline) content: TimelineItemContent,
    pub(in crate::timeline) sender: OwnedUserId,
    pub(in crate::timeline) sender_profile: TimelineDetails<Profile>,
}

impl RepliedToEvent {
    /// Get the message of this event.
    pub fn content(&self) -> &TimelineItemContent {
        &self.content
    }

    /// Get the sender of this event.
    pub fn sender(&self) -> &UserId {
        &self.sender
    }

    /// Get the profile of the sender.
    pub fn sender_profile(&self) -> &TimelineDetails<Profile> {
        &self.sender_profile
    }

    fn from_timeline_item(timeline_item: &EventTimelineItem) -> Self {
        Self {
            content: timeline_item.content.clone(),
            sender: timeline_item.sender.clone(),
            sender_profile: timeline_item.sender_profile.clone(),
        }
    }

    pub(in crate::timeline) fn redact(&self, room_version: &RoomVersionId) -> Self {
        Self {
            content: self.content.redact(room_version),
            sender: self.sender.clone(),
            sender_profile: self.sender_profile.clone(),
        }
    }

    pub(in crate::timeline) async fn try_from_timeline_event<P: RoomDataProvider>(
        timeline_event: TimelineEvent,
        room_data_provider: &P,
    ) -> Result<Self, TimelineError> {
        let event = match timeline_event.event.deserialize() {
            Ok(AnyTimelineEvent::MessageLike(event)) => event,
            _ => {
                return Err(TimelineError::UnsupportedEvent);
            }
        };

        let Some(AnyMessageLikeEventContent::RoomMessage(c)) = event.original_content() else {
            return Err(TimelineError::UnsupportedEvent);
        };

        let content =
            TimelineItemContent::Message(Message::from_event(c, event.relations(), &vector![]));
        let sender = event.sender().to_owned();
        let sender_profile =
            TimelineDetails::from_initial_value(room_data_provider.profile(&sender).await);

        Ok(Self { content, sender, sender_profile })
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
pub struct ReactionGroup(pub(in crate::timeline) IndexMap<EventItemIdentifier, ReactionSenderData>);

impl ReactionGroup {
    /// The (deduplicated) senders of the reactions in this group.
    pub fn senders(&self) -> impl Iterator<Item = &ReactionSenderData> {
        self.values().unique_by(|v| &v.sender_id)
    }

    /// All reactions within this reaction group that were sent by the given
    /// user.
    ///
    /// Note that it is possible for multiple reactions by the same user to
    /// have arrived over federation.
    pub fn by_sender<'a>(
        &'a self,
        user_id: &'a UserId,
    ) -> impl Iterator<Item = (Option<&OwnedTransactionId>, Option<&OwnedEventId>)> + 'a {
        self.iter().filter_map(move |(k, v)| {
            (v.sender_id == user_id).then_some(match k {
                EventItemIdentifier::TransactionId(txn_id) => (Some(txn_id), None),
                EventItemIdentifier::EventId(event_id) => (None, Some(event_id)),
            })
        })
    }
}

impl Deref for ReactionGroup {
    type Target = IndexMap<EventItemIdentifier, ReactionSenderData>;

    fn deref(&self) -> &Self::Target {
        &self.0
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

#[derive(PartialEq, Eq, Hash, Clone, Debug)]
pub struct PollAnswerId(pub String);

/// State for a poll start event and related answers and close event, if any.
#[derive(Clone, Debug)]
pub struct PollState {
    pub(in crate::timeline) question: String,
    pub(in crate::timeline) disclosed: bool,
    pub(in crate::timeline) max_selections: u64,
    pub(in crate::timeline) answers: Vec<(PollAnswerId, String)>,
    pub(in crate::timeline) end_time: Option<MilliSecondsSinceUnixEpoch>,
    pub(in crate::timeline) votes: Vec<(OwnedUserId, Vec<PollAnswerId>, MilliSecondsSinceUnixEpoch)>,
}

impl PollState {
    pub fn from(content: UnstablePollStartContentBlock) -> Self {
        Self {
            question: content.question.text,
            disclosed: content.kind == PollKind::Disclosed,
            max_selections: content.max_selections.into(),
            answers: content.answers.iter().map(|a| (PollAnswerId(a.id.clone()), a.text.clone())).collect(),
            end_time: None,
            votes: Vec::new(),
        }
    }

    /// Gets the question that was asked in this poll.
    pub fn question(&self) -> String {
        self.question.clone()
    }

    /// Whether the poll results should be disclosed (shown to the user before the poll ends).
    pub fn disclosed(&self) -> bool {
        self.disclosed
    }

    /// Gets the maximum number of votes each user may cast.
    pub fn max_selections(&self) -> u64 {
        self.max_selections
    }

    /// Gets the list of answers that were provided for this poll.
    pub fn answers(&self) -> Vec<(PollAnswerId, String)> {
        self.answers.clone()
    }

    /// Get the time this poll was ended, or None if it's still running.
    pub fn end_time(&self) -> Option<MilliSecondsSinceUnixEpoch> {
        self.end_time
    }

    /// Aggregates all known votes for this poll, handling late and spoiled votes.
    pub fn calculate_poll_results(&self) -> HashMap<PollAnswerId, Vec<OwnedUserId>> {
        let cut_off_time = self.end_time.unwrap_or_else(MilliSecondsSinceUnixEpoch::now);
        let answer_ids = self.answers.iter().map(|(a, _)| a.clone()).collect::<Vec<_>>();
        let max_choices = self.max_selections as usize;

        let results = self
            .votes
            .iter()
            // Filter out any vote that was made after the cut off time.
            .filter(|(_, _, timestamp)| *timestamp <= cut_off_time)
            // Collate the most recent vote for each user. Spoiled votes (i.e., those for invalid
            // options or for no options) count as retracting a vote, per MSC3381.
            .fold(BTreeMap::new(), |mut acc, (sender, choices, time)| {
                let response =
                    acc.entry(sender).or_insert((MilliSecondsSinceUnixEpoch(uint!(0)), Vec::new()));

                if response.0 < *time {
                    if choices.is_empty() {
                        debug!("Discarding poll vote: spoiled by selecting no options");
                        *response = (*time, Vec::new());
                    } else if choices.iter().any(|c| !answer_ids.contains(c)) {
                        debug!("Discarding poll vote: spoiled by selecting invalid option");
                        *response = (*time, Vec::new());
                    } else {
                        // Truncate their choices to the maximum number of selections.
                        *response = (*time, choices.iter().take(max_choices).collect());
                    }
                }

                acc
            })
            // Flatten the map into a list of (choice, sender) tuples.
            .into_iter()
            .flat_map(|(sender, (_, choices))| {
                choices.into_iter().map(|c| (c, sender.clone()))
            })
            // Collate them into (choice, [sender]) tuples.
            .into_group_map_by(|(choice, _)| *choice)
            .into_iter()
            .map(|(k, v)| (k.clone(), v.into_iter().map(|(_, sender)| sender).collect::<Vec<_>>()));

        // Make sure all answers are included in the results, even if no one voted for them.
        answer_ids.into_iter().map(|i| (i, Vec::new())).chain(results).collect()
    }
}

#[cfg(test)]
mod tests {
    use ruma::{MilliSecondsSinceUnixEpoch, owned_user_id, uint};
    use crate::timeline::{PollAnswerId, PollState};

    macro_rules! answer_id {
        ($s:literal) => {
            PollAnswerId($s.to_string())
        };
    }

    macro_rules! time {
        ($s:literal) => {
            MilliSecondsSinceUnixEpoch(uint!($s))
        };
    }

    macro_rules! closed_poll_with_votes {
    ($($x:tt)*) => {
            PollState {
                question: "Question".to_string(),
                disclosed: false,
                max_selections: 2,
                answers: vec![
                    (answer_id!("a"), "Answer A".to_string()),
                    (answer_id!("b"), "Answer B".to_string()),
                    (answer_id!("c"), "Answer C".to_string()),
                ],
                end_time: Some(time!(1000)),
                votes: vec![$($x)*],
            }
        }
    }

    #[test]
    fn test_calculate_poll_results_ignores_late_votes() {
        let state = closed_poll_with_votes!(
            // Before the end-time: allowed
            (owned_user_id!("@alice:example.org"), vec![answer_id!("a")], time!(900)),
            // Equal to the end-time: allowed
            (owned_user_id!("@bob:example.org"), vec![answer_id!("a")], time!(1000)),
            // After the end-time: ignored
            (owned_user_id!("@charles:example.org"), vec![answer_id!("a")], time!(1100))
        );

        let results = state.calculate_poll_results();

        assert_eq!(2, results[&answer_id!("a")].len());
        assert_eq!(owned_user_id!("@alice:example.org"), results[&answer_id!("a")][0]);
        assert_eq!(owned_user_id!("@bob:example.org"), results[&answer_id!("a")][1]);
    }

    #[test]
    fn test_calculate_poll_results_uses_latest_votes() {
        let state = closed_poll_with_votes!(
            (owned_user_id!("@alice:example.org"), vec![answer_id!("a")], time!(100)),
            (owned_user_id!("@alice:example.org"), vec![answer_id!("b")], time!(300)),
            (owned_user_id!("@alice:example.org"), vec![answer_id!("a")], time!(200)),
        );

        let results = state.calculate_poll_results();

        assert_eq!(1, results[&answer_id!("b")].len());
        assert_eq!(owned_user_id!("@alice:example.org"), results[&answer_id!("b")][0]);
    }

    #[test]
    fn test_calculate_poll_results_spoils_votes() {
        let state = closed_poll_with_votes!(
            // Legitimate votes
            (owned_user_id!("@alice:example.org"), vec![answer_id!("a")], time!(100)),
            (owned_user_id!("@bob:example.org"), vec![answer_id!("a")], time!(100)),
            // Spoiled due to invalid option
            (owned_user_id!("@alice:example.org"), vec![answer_id!("a"), answer_id!("not")], time!(200)),
            // Spoiled due to no options
            (owned_user_id!("@bob:example.org"), vec![], time!(200)),
        );

        let results = state.calculate_poll_results();

        // No votes, because a spoiled vote is treated as a retraction
        assert_eq!(0, results[&answer_id!("a")].len());
        assert_eq!(0, results[&answer_id!("b")].len());
        assert!(!results.contains_key(&answer_id!("not")));
    }


    #[test]
    fn test_calculate_poll_results_truncates_choices() {
        let state = closed_poll_with_votes!(
            // Too many choices: should be truncated to the first two
            (owned_user_id!("@alice:example.org"), vec![answer_id!("a"), answer_id!("b"), answer_id!("c")], time!(100)),
        );

        let results = state.calculate_poll_results();

        // No votes, because a spoiled vote is treated as a retraction
        assert_eq!(1, results[&answer_id!("a")].len());
        assert_eq!(1, results[&answer_id!("b")].len());
        assert_eq!(0, results[&answer_id!("c")].len());
    }

    #[test]
    fn test_calculate_poll_results_collates_votes_from_users() {
        let state = closed_poll_with_votes!(
            (owned_user_id!("@alice:example.org"), vec![answer_id!("a")], time!(100)),
            (owned_user_id!("@bob:example.org"), vec![answer_id!("a")], time!(100)),
            (owned_user_id!("@charles:example.org"), vec![answer_id!("a"), answer_id!("b")], time!(100)),
            (owned_user_id!("@dennis:example.org"), vec![answer_id!("a")], time!(100)),
        );

        let results = state.calculate_poll_results();

        assert_eq!(4, results[&answer_id!("a")].len());
        assert_eq!(owned_user_id!("@alice:example.org"), results[&answer_id!("a")][0]);
        assert_eq!(owned_user_id!("@bob:example.org"), results[&answer_id!("a")][1]);
        assert_eq!(owned_user_id!("@charles:example.org"), results[&answer_id!("a")][2]);
        assert_eq!(owned_user_id!("@dennis:example.org"), results[&answer_id!("a")][3]);

        assert_eq!(1, results[&answer_id!("b")].len());
        assert_eq!(owned_user_id!("@charles:example.org"), results[&answer_id!("b")][0]);
    }
}

#[derive(Clone, Debug)]
pub struct PollEnd {
    pub(in crate::timeline) start_event: OwnedEventId,
}

impl PollEnd {
    pub fn start_event(&self) -> &EventId {
        &self.start_event
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
