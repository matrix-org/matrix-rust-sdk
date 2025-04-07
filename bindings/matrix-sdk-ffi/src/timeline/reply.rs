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

use matrix_sdk_ui::timeline::TimelineDetails;

use super::{content::TimelineItemContent, ProfileDetails};

#[derive(Clone, uniffi::Object)]
pub struct InReplyToDetails {
    event_id: String,
    event: RepliedToEventDetails,
}

impl InReplyToDetails {
    pub(crate) fn new(event_id: String, event: RepliedToEventDetails) -> Self {
        Self { event_id, event }
    }
}

#[matrix_sdk_ffi_macros::export]
impl InReplyToDetails {
    pub fn event_id(&self) -> String {
        self.event_id.clone()
    }

    pub fn event(&self) -> RepliedToEventDetails {
        self.event.clone()
    }
}

impl From<matrix_sdk_ui::timeline::InReplyToDetails> for InReplyToDetails {
    fn from(inner: matrix_sdk_ui::timeline::InReplyToDetails) -> Self {
        let event_id = inner.event_id.to_string();
        let event = match &inner.event {
            TimelineDetails::Unavailable => RepliedToEventDetails::Unavailable,
            TimelineDetails::Pending => RepliedToEventDetails::Pending,
            TimelineDetails::Ready(event) => RepliedToEventDetails::Ready {
                content: event.content().clone().into(),
                sender: event.sender().to_string(),
                sender_profile: event.sender_profile().into(),
            },
            TimelineDetails::Error(err) => {
                RepliedToEventDetails::Error { message: err.to_string() }
            }
        };

        Self { event_id, event }
    }
}

#[derive(Clone, uniffi::Enum)]
pub enum RepliedToEventDetails {
    Unavailable,
    Pending,
    Ready { content: TimelineItemContent, sender: String, sender_profile: ProfileDetails },
    Error { message: String },
}
