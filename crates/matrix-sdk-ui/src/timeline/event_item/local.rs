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

use as_variant::as_variant;
use matrix_sdk::{send_queue::AbortSendHandle, Error};
use ruma::{EventId, OwnedEventId, OwnedTransactionId};

/// An item for an event that was created locally and not yet echoed back by
/// the homeserver.
#[derive(Debug, Clone)]
pub(in crate::timeline) struct LocalEventTimelineItem {
    /// The send state of this local event.
    pub send_state: EventSendState,
    /// The transaction ID.
    pub transaction_id: OwnedTransactionId,
    /// A handle to abort sending this event, if possible.
    pub abort_handle: Option<AbortSendHandle>,
}

impl LocalEventTimelineItem {
    /// Get the event ID of this item.
    ///
    /// Will be `Some` if and only if `send_state` is
    /// `EventSendState::Sent`.
    pub fn event_id(&self) -> Option<&EventId> {
        as_variant!(&self.send_state, EventSendState::Sent { event_id } => event_id)
    }

    /// Clone the current event item, and update its `send_state`.
    pub fn with_send_state(&self, send_state: EventSendState) -> Self {
        Self { send_state, ..self.clone() }
    }
}

/// This type represents the "send state" of a local event timeline item.
#[derive(Clone, Debug)]
pub enum EventSendState {
    /// The local event has not been sent yet.
    NotSentYet,
    /// The local event has been sent to the server, but unsuccessfully: The
    /// sending has failed.
    SendingFailed {
        /// Details about how sending the event failed.
        error: Arc<Error>,
    },
    /// The local event has been sent successfully to the server.
    Sent {
        /// The event ID assigned by the server.
        event_id: OwnedEventId,
    },
}
