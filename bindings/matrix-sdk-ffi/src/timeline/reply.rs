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

use matrix_sdk_ui::timeline::{EmbeddedEvent, TimelineDetails};

use super::{content::TimelineItemContent, ProfileDetails};
use crate::{event::EventOrTransactionId, utils::Timestamp};

#[derive(Clone, uniffi::Object)]
pub struct InReplyToDetails {
    event_id: String,
    event: EmbeddedEventDetails,
}

impl InReplyToDetails {
    pub(crate) fn new(event_id: String, event: EmbeddedEventDetails) -> Self {
        Self { event_id, event }
    }
}

#[matrix_sdk_ffi_macros::export]
impl InReplyToDetails {
    pub fn event_id(&self) -> String {
        self.event_id.clone()
    }

    pub fn event(&self) -> EmbeddedEventDetails {
        self.event.clone()
    }
}

impl From<matrix_sdk_ui::timeline::InReplyToDetails> for InReplyToDetails {
    fn from(inner: matrix_sdk_ui::timeline::InReplyToDetails) -> Self {
        Self { event_id: inner.event_id.to_string(), event: inner.event.into() }
    }
}

#[derive(Clone, uniffi::Enum)]
#[allow(clippy::large_enum_variant)]
pub enum EmbeddedEventDetails {
    Unavailable,
    Pending,
    Ready {
        content: TimelineItemContent,
        sender: String,
        sender_profile: ProfileDetails,
        timestamp: Timestamp,
        event_or_transaction_id: EventOrTransactionId,
    },
    Error {
        message: String,
    },
}

impl From<TimelineDetails<Box<EmbeddedEvent>>> for EmbeddedEventDetails {
    fn from(event: TimelineDetails<Box<EmbeddedEvent>>) -> Self {
        match event {
            TimelineDetails::Unavailable => EmbeddedEventDetails::Unavailable,
            TimelineDetails::Pending => EmbeddedEventDetails::Pending,
            TimelineDetails::Ready(event) => EmbeddedEventDetails::Ready {
                content: event.content.into(),
                sender: event.sender.to_string(),
                sender_profile: event.sender_profile.into(),
                timestamp: event.timestamp.into(),
                event_or_transaction_id: event.identifier.into(),
            },
            TimelineDetails::Error(err) => EmbeddedEventDetails::Error { message: err.to_string() },
        }
    }
}
