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

use ruma::{EventId, OwnedTransactionId};

use super::EventSendState;

/// An item for an event that was created locally and not yet echoed back by
/// the homeserver.
#[derive(Debug, Clone)]
pub(in crate::timeline) struct LocalEventTimelineItem {
    /// The send state of this local event.
    pub send_state: EventSendState,
    /// The transaction ID.
    pub transaction_id: OwnedTransactionId,
}

impl LocalEventTimelineItem {
    /// Get the event ID of this item.
    ///
    /// Will be `Some` if and only if `send_state` is
    /// `EventSendState::Sent`.
    pub fn event_id(&self) -> Option<&EventId> {
        match &self.send_state {
            EventSendState::Sent { event_id } => Some(event_id),
            _ => None,
        }
    }

    /// Clone the current event item, and update its `send_state`.
    pub fn with_send_state(&self, send_state: EventSendState) -> Self {
        Self { send_state, ..self.clone() }
    }
}
