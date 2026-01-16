// Copyright 2024 The Matrix.org Foundation C.I.C.
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

use as_variant::as_variant;
use ruma::OwnedEventId;

use super::{EmbeddedEvent, EncryptedMessage, InReplyToDetails, Message, PollState, Sticker};
use crate::timeline::{
    ReactionsByKeyBySender, TimelineDetails, event_item::content::other::OtherMessageLike,
};

#[derive(Clone, Debug)]
pub enum MsgLikeKind {
    /// An `m.room.message` event or extensible event, including edits.
    Message(Message),

    /// An `m.sticker` event.
    Sticker(Sticker),

    /// An `m.poll.start` event.
    Poll(PollState),

    /// A redacted message.
    Redacted,

    /// An `m.room.encrypted` event that could not be decrypted.
    UnableToDecrypt(EncryptedMessage),

    /// A custom message like event.
    Other(OtherMessageLike),
}

#[derive(Clone, Debug)]
pub struct ThreadSummary {
    pub latest_event: TimelineDetails<Box<EmbeddedEvent>>,

    /// The number of events in the thread, except for the thread root.
    ///
    /// This can be zero if all the events in the thread have been redacted.
    ///
    /// Note: this doesn't interact with the timeline filter; so opening a
    /// thread-focused timeline with the same timeline filter may result in
    /// *fewer* events than this number.
    pub num_replies: u32,

    /// The user's own public read receipt event id, for this particular thread.
    pub public_read_receipt_event_id: Option<OwnedEventId>,

    /// The user's own private read receipt event id, for this particular
    /// thread.
    pub private_read_receipt_event_id: Option<OwnedEventId>,
}

/// A special kind of [`super::TimelineItemContent`] that groups together
/// different room message types with their respective reactions and thread
/// information.
#[derive(Clone, Debug)]
pub struct MsgLikeContent {
    pub kind: MsgLikeKind,
    pub reactions: ReactionsByKeyBySender,
    /// The event this message is replying to, if any.
    pub in_reply_to: Option<InReplyToDetails>,
    /// Event ID of the thread root, if this is a message in a thread.
    pub thread_root: Option<OwnedEventId>,
    /// Information about the thread this message is the root of, if any.
    pub thread_summary: Option<ThreadSummary>,
}

impl MsgLikeContent {
    #[cfg(not(tarpaulin_include))] // debug-logging functionality
    pub(crate) fn debug_string(&self) -> &'static str {
        match self.kind {
            MsgLikeKind::Message(_) => "a message",
            MsgLikeKind::Sticker(_) => "a sticker",
            MsgLikeKind::Poll(_) => "a poll",
            MsgLikeKind::Redacted => "a redacted message",
            MsgLikeKind::UnableToDecrypt(_) => "an encrypted message we couldn't decrypt",
            MsgLikeKind::Other(_) => "a custom message-like event",
        }
    }

    pub fn redacted() -> Self {
        Self {
            kind: MsgLikeKind::Redacted,
            reactions: Default::default(),
            thread_root: None,
            in_reply_to: None,
            thread_summary: None,
        }
    }

    pub fn unable_to_decrypt(encrypted_message: EncryptedMessage) -> Self {
        Self {
            kind: MsgLikeKind::UnableToDecrypt(encrypted_message),
            reactions: Default::default(),
            thread_root: None,
            in_reply_to: None,
            thread_summary: None,
        }
    }

    /// Whether this item is part of a thread.
    pub fn is_threaded(&self) -> bool {
        self.thread_root.is_some()
    }

    pub fn with_in_reply_to(&self, in_reply_to: InReplyToDetails) -> Self {
        Self { in_reply_to: Some(in_reply_to), ..self.clone() }
    }

    pub fn with_kind(&self, kind: MsgLikeKind) -> Self {
        Self { kind, ..self.clone() }
    }

    /// If `kind` is of the [`MsgLikeKind`][MsgLikeKind::Message] variant,
    /// return the inner [`Message`].
    pub fn as_message(&self) -> Option<Message> {
        as_variant!(&self.kind, MsgLikeKind::Message(message) => message.clone())
    }
}
