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

use matrix_sdk::crypto::types::events::UtdCause;
use matrix_sdk_ui::timeline::PollResult;

use super::{content::TimelineItemContent, reply::InReplyToDetails};
use crate::ruma::{Mentions, MessageType, PollKind};

#[derive(Clone, uniffi::Record)]
pub struct MessageContent {
    pub msg_type: MessageType,
    pub body: String,
    pub in_reply_to: Option<Arc<InReplyToDetails>>,
    pub thread_root: Option<String>,
    pub is_edited: bool,
    pub mentions: Option<Mentions>,
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

impl From<PollResult> for TimelineItemContent {
    fn from(value: PollResult) -> Self {
        TimelineItemContent::Poll {
            question: value.question,
            kind: PollKind::from(value.kind),
            max_selections: value.max_selections,
            answers: value
                .answers
                .into_iter()
                .map(|i| PollAnswer { id: i.id, text: i.text })
                .collect(),
            votes: value.votes,
            end_time: value.end_time.map(|t| t.into()),
            has_been_edited: value.has_been_edited,
        }
    }
}
