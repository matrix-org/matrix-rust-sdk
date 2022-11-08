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

use std::fmt;

use indexmap::IndexMap;
use matrix_sdk_base::deserialized_responses::EncryptionInfo;
#[cfg(feature = "experimental-room-preview")]
use ruma::events::room::message::{OriginalSyncRoomMessageEvent, Relation};
use ruma::{
    events::{
        relation::{AnnotationChunk, AnnotationType},
        room::{
            encrypted::{EncryptedEventScheme, MegolmV1AesSha2Content, RoomEncryptedEventContent},
            message::MessageType,
        },
        AnySyncTimelineEvent,
    },
    serde::Raw,
    uint, EventId, MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedEventId, OwnedTransactionId,
    OwnedUserId, TransactionId, UInt, UserId,
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
    pub(super) content: TimelineItemContent,
    pub(super) reactions: BundledReactions,
    pub(super) origin_server_ts: Option<MilliSecondsSinceUnixEpoch>,
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
            .field("origin_server_ts", &self.origin_server_ts)
            .field("is_own", &self.is_own)
            .field("encryption_info", &self.encryption_info)
            // skip raw, too noisy
            .finish_non_exhaustive()
    }
}

macro_rules! build {
    (
        $ty:ident {
            $( $field:ident $(: $value:expr)?, )*
            ..$this:ident( $($this_field:ident),* $(,)? )
        }
    ) => {
        $ty {
            $( $field $(: $value)?, )*
            $( $this_field: $this.$this_field.clone() ),*
        }
    }
}

impl EventTimelineItem {
    #[cfg(feature = "experimental-room-preview")]
    #[doc(hidden)] // FIXME: Remove. Used for matrix-sdk-ffi temporarily.
    pub fn _new(ev: OriginalSyncRoomMessageEvent, raw: Raw<AnySyncTimelineEvent>) -> Self {
        let edited = ev.unsigned.relations.as_ref().map_or(false, |r| r.replace.is_some());
        let reactions = ev
            .unsigned
            .relations
            .and_then(|r| r.annotation)
            .map(BundledReactions::from)
            .unwrap_or_default();

        Self {
            key: TimelineKey::EventId(ev.event_id),
            event_id: None,
            sender: ev.sender,
            content: TimelineItemContent::Message(Message {
                msgtype: ev.content.msgtype,
                in_reply_to: ev.content.relates_to.and_then(|rel| match rel {
                    Relation::Reply { in_reply_to } => Some(in_reply_to.event_id),
                    _ => None,
                }),
                edited,
            }),
            reactions,
            origin_server_ts: Some(ev.origin_server_ts),
            is_own: false,         // FIXME: Potentially wrong
            encryption_info: None, // FIXME: Potentially wrong
            raw: Some(raw),
        }
    }

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

    /// Get the origin server timestamp of this item.
    ///
    /// Returns `None` if this event hasn't been echoed back by the server yet.
    pub fn origin_server_ts(&self) -> Option<MilliSecondsSinceUnixEpoch> {
        self.origin_server_ts
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
        build!(Self {
            // FIXME: Change when we support state events
            content: TimelineItemContent::RedactedMessage,
            reactions: BundledReactions::default(),
            ..self(key, event_id, sender, origin_server_ts, is_own, encryption_info, raw)
        })
    }

    pub(super) fn with_event_id(&self, event_id: Option<OwnedEventId>) -> Self {
        build!(Self {
            event_id,
            ..self(key, sender, content, reactions, origin_server_ts, is_own, encryption_info, raw,)
        })
    }

    #[rustfmt::skip]
    pub(super) fn with_content(&self, content: TimelineItemContent) -> Self {
        build!(Self {
            content,
            ..self(
                key, event_id, sender, reactions, origin_server_ts, is_own, encryption_info, raw,
            )
        })
    }

    #[rustfmt::skip]
    pub(super) fn with_reactions(&self, reactions: BundledReactions) -> Self {
        build!(Self {
            reactions,
            ..self(
                key, event_id, sender, content, origin_server_ts, is_own, encryption_info, raw,
            )
        })
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

impl PartialEq<TimelineKey> for &TransactionId {
    fn eq(&self, key: &TimelineKey) -> bool {
        matches!(key, TimelineKey::TransactionId(txn_id) if txn_id == self)
    }
}

impl PartialEq<TimelineKey> for &OwnedTransactionId {
    fn eq(&self, key: &TimelineKey) -> bool {
        matches!(key, TimelineKey::TransactionId(txn_id) if txn_id == *self)
    }
}

impl PartialEq<TimelineKey> for &EventId {
    fn eq(&self, key: &TimelineKey) -> bool {
        matches!(key, TimelineKey::EventId(event_id) if event_id == self)
    }
}

impl PartialEq<TimelineKey> for &OwnedEventId {
    fn eq(&self, key: &TimelineKey) -> bool {
        matches!(key, TimelineKey::EventId(event_id) if event_id == *self)
    }
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

    /// An `m.room.encrypted` event that could not be decrypted.
    UnableToDecrypt(EncryptedMessage),
}

impl TimelineItemContent {
    /// If `self` is of the [`Message`][Self:Message] variant, return the inner
    /// [`Message`].
    pub fn as_message(&self) -> Option<&Message> {
        match self {
            Self::Message(v) => Some(v),
            _ => None,
        }
    }
}

/// An `m.room.message` event or extensible event, including edits.
#[derive(Clone, Debug)]
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

impl From<AnnotationChunk> for BundledReactions {
    fn from(ann: AnnotationChunk) -> Self {
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

/// The result of a successful pagination request.
#[derive(Debug)]
// TODO: non-exhaustive breaks UniFFI bridge
//#[non_exhaustive]
pub struct PaginationOutcome {
    /// Whether there's more messages to be paginated.
    pub more_messages: bool,
}
