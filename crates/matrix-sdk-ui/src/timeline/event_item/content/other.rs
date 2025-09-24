// Copyright 2025 The Matrix.org Foundation C.I.C.
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

//! Timeline item content for other message-like events created by the
//! EventContent macro from ruma.

use ruma::events::MessageLikeEventType;

/// A custom event created by the EventContent macro from ruma.
#[derive(Debug, Clone, PartialEq)]
pub struct OtherMessageLike {
    pub(in crate::timeline) event_type: MessageLikeEventType,
}

impl OtherMessageLike {
    pub fn from_event_type(event_type: MessageLikeEventType) -> Self {
        Self { event_type }
    }

    /// Get the event_type of this message.
    pub fn event_type(&self) -> &MessageLikeEventType {
        &self.event_type
    }
}
