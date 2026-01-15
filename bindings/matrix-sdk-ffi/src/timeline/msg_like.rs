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

use matrix_sdk_base::crypto::types::events::UtdCause;
use ruma::events::{room::MediaSource as RumaMediaSource, MessageLikeEventContent};

use super::{
    content::Reaction,
    reply::{EmbeddedEventDetails, InReplyToDetails},
};
use crate::{
    error::ClientError,
    event::MessageLikeEventType,
    ruma::{ImageInfo, MediaSource, MediaSourceExt, Mentions, MessageType, PollKind},
    timeline::content::ReactionSenderData,
    utils::Timestamp,
};

#[derive(Clone, uniffi::Enum)]
pub enum MsgLikeKind {
    /// An `m.room.message` event or extensible event, including edits.
    Message { content: MessageContent },
    /// An `m.sticker` event.
    Sticker { body: String, info: ImageInfo, source: Arc<MediaSource> },
    /// An `m.poll.start` event.
    Poll {
        question: String,
        kind: PollKind,
        max_selections: u64,
        answers: Vec<PollAnswer>,
        votes: HashMap<String, Vec<String>>,
        end_time: Option<Timestamp>,
        has_been_edited: bool,
    },

    /// A redacted message.
    Redacted,

    /// An `m.room.encrypted` event that could not be decrypted.
    UnableToDecrypt { msg: EncryptedMessage },

    /// A custom message like event.
    Other { event_type: MessageLikeEventType },
}

/// A special kind of [`super::TimelineItemContent`] that groups together
/// different room message types with their respective reactions and thread
/// information.
#[derive(Clone, uniffi::Record)]
pub struct MsgLikeContent {
    pub kind: MsgLikeKind,
    pub reactions: Vec<Reaction>,
    /// The event this message is replying to, if any.
    pub in_reply_to: Option<Arc<InReplyToDetails>>,
    /// Event ID of the thread root, if this is a message in a thread.
    pub thread_root: Option<String>,
    /// Details about the thread this message is the root of.
    pub thread_summary: Option<Arc<ThreadSummary>>,
}

#[derive(Clone, uniffi::Record)]
pub struct MessageContent {
    pub msg_type: MessageType,
    pub body: String,
    pub is_edited: bool,
    pub mentions: Option<Mentions>,
}

impl TryFrom<matrix_sdk_ui::timeline::MsgLikeContent> for MsgLikeContent {
    type Error = (ClientError, String);

    fn try_from(value: matrix_sdk_ui::timeline::MsgLikeContent) -> Result<Self, Self::Error> {
        use matrix_sdk_ui::timeline::MsgLikeKind as Kind;

        let reactions = value
            .reactions
            .iter()
            .map(|(k, v)| Reaction {
                key: k.to_owned(),
                senders: v
                    .into_iter()
                    .map(|(sender_id, info)| ReactionSenderData {
                        sender_id: sender_id.to_string(),
                        timestamp: info.timestamp.into(),
                    })
                    .collect(),
            })
            .collect();

        let in_reply_to = value.in_reply_to.map(|r| Arc::new(r.into()));

        let thread_root = value.thread_root.map(|id| id.to_string());

        let thread_summary = value.thread_summary.map(|t| Arc::new(t.into()));

        Ok(match value.kind {
            Kind::Message(message) => {
                let msg_type = TryInto::<MessageType>::try_into(message.msgtype().clone())
                    .map_err(|e| (e, message.msgtype().msgtype().to_owned()))?;

                Self {
                    kind: MsgLikeKind::Message {
                        content: MessageContent {
                            msg_type,
                            body: message.body().to_owned(),
                            is_edited: message.is_edited(),
                            mentions: message.mentions().cloned().map(|m| m.into()),
                        },
                    },
                    reactions,
                    in_reply_to,
                    thread_root,
                    thread_summary,
                }
            }
            Kind::Sticker(sticker) => {
                let content = sticker.content();

                let media_source = RumaMediaSource::from(content.source.clone());
                media_source
                    .verify()
                    .map_err(|e| (e, sticker.content().event_type().to_string()))?;

                let image_info = TryInto::<ImageInfo>::try_into(&content.info)
                    .map_err(|e| (e, sticker.content().event_type().to_string()))?;

                Self {
                    kind: MsgLikeKind::Sticker {
                        body: content.body.clone(),
                        info: image_info,
                        source: Arc::new(MediaSource { media_source }),
                    },
                    reactions,
                    in_reply_to,
                    thread_root,
                    thread_summary,
                }
            }
            Kind::Poll(poll_state) => {
                let results = poll_state.results();

                Self {
                    kind: MsgLikeKind::Poll {
                        question: results.question,
                        kind: PollKind::from(results.kind),
                        max_selections: results.max_selections,
                        answers: results
                            .answers
                            .into_iter()
                            .map(|i| PollAnswer { id: i.id, text: i.text })
                            .collect(),
                        votes: results.votes,
                        end_time: results.end_time.map(|t| t.into()),
                        has_been_edited: results.has_been_edited,
                    },
                    reactions,
                    in_reply_to,
                    thread_root,
                    thread_summary,
                }
            }
            Kind::Redacted => Self {
                kind: MsgLikeKind::Redacted,
                reactions,
                in_reply_to,
                thread_root,
                thread_summary,
            },
            Kind::UnableToDecrypt(msg) => Self {
                kind: MsgLikeKind::UnableToDecrypt { msg: EncryptedMessage::new(&msg) },
                reactions,
                in_reply_to,
                thread_root,
                thread_summary,
            },
            Kind::Other(other) => Self {
                kind: MsgLikeKind::Other {
                    event_type: MessageLikeEventType::Other(other.event_type().to_string()),
                },
                reactions,
                in_reply_to,
                thread_root,
                thread_summary,
            },
        })
    }
}

impl From<ruma::events::Mentions> for Mentions {
    fn from(value: ruma::events::Mentions) -> Self {
        Self {
            user_ids: value.user_ids.iter().map(|id| id.to_string()).collect(),
            room: value.room,
        }
    }
}

#[derive(Clone, uniffi::Enum)]
pub enum EncryptedMessage {
    OlmV1Curve25519AesSha2 {
        /// The Curve25519 key of the sender.
        sender_key: String,
    },
    // Other fields not included because UniFFI doesn't have the concept of
    // deprecated fields right now.
    MegolmV1AesSha2 {
        /// The ID of the session used to encrypt the message.
        session_id: String,

        /// What we know about what caused this UTD. E.g. was this event sent
        /// when we were not a member of this room?
        cause: UtdCause,
    },
    Unknown,
}

impl EncryptedMessage {
    pub(crate) fn new(msg: &matrix_sdk_ui::timeline::EncryptedMessage) -> Self {
        use matrix_sdk_ui::timeline::EncryptedMessage as Message;

        match msg {
            Message::OlmV1Curve25519AesSha2 { sender_key } => {
                let sender_key = sender_key.clone();
                Self::OlmV1Curve25519AesSha2 { sender_key }
            }
            Message::MegolmV1AesSha2 { session_id, cause, .. } => {
                let session_id = session_id.clone();
                Self::MegolmV1AesSha2 { session_id, cause: *cause }
            }
            Message::Unknown => Self::Unknown,
        }
    }
}

#[derive(Clone, uniffi::Record)]
pub struct PollAnswer {
    pub id: String,
    pub text: String,
}

#[derive(Clone, uniffi::Object)]
pub struct ThreadSummary {
    pub latest_event: EmbeddedEventDetails,
    pub num_replies: u32,
    /// The user's own public read receipt event id, for this particular thread.
    pub public_read_receipt_event_id: Option<String>,
    /// The user's own private read receipt event id, for this particular
    /// thread.
    pub private_read_receipt_event_id: Option<String>,
}

#[matrix_sdk_ffi_macros::export]
impl ThreadSummary {
    pub fn latest_event(&self) -> EmbeddedEventDetails {
        self.latest_event.clone()
    }

    pub fn num_replies(&self) -> u64 {
        self.num_replies as u64
    }
}

impl From<matrix_sdk_ui::timeline::ThreadSummary> for ThreadSummary {
    fn from(value: matrix_sdk_ui::timeline::ThreadSummary) -> Self {
        Self {
            latest_event: EmbeddedEventDetails::from(value.latest_event),
            num_replies: value.num_replies,
            public_read_receipt_event_id: value.public_read_receipt_event_id.map(|v| v.to_string()),
            private_read_receipt_event_id: value
                .private_read_receipt_event_id
                .map(|v| v.to_string()),
        }
    }
}
