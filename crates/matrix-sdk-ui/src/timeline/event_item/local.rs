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
use matrix_sdk::{
    Error,
    send_queue::{AbstractProgress, SendHandle},
};
use ruma::{EventId, OwnedEventId, OwnedTransactionId};

use super::TimelineEventItemId;

/// An item for an event that was created locally and not yet echoed back by
/// the homeserver.
#[derive(Debug, Clone)]
pub(in crate::timeline) struct LocalEventTimelineItem {
    /// The send state of this local event.
    pub send_state: EventSendState,
    /// The transaction ID.
    pub transaction_id: OwnedTransactionId,
    /// A handle to manipulate this event before it is sent, if possible.
    pub send_handle: Option<SendHandle>,
}

impl LocalEventTimelineItem {
    /// Get the unique identifier of this item.
    ///
    /// Returns the transaction ID for a local echo item that has not been sent
    /// and the event ID for a local echo item that has been sent.
    pub(crate) fn identifier(&self) -> TimelineEventItemId {
        if let Some(event_id) =
            as_variant!(&self.send_state, EventSendState::Sent { event_id } => event_id)
        {
            TimelineEventItemId::EventId(event_id.clone())
        } else {
            TimelineEventItemId::TransactionId(self.transaction_id.clone())
        }
    }

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
    NotSentYet {
        /// The progress of the sending operation, if the event involves a media
        /// upload.
        progress: Option<MediaUploadProgress>,
    },
    /// The local event has been sent to the server, but unsuccessfully: The
    /// sending has failed.
    SendingFailed {
        /// Details about how sending the event failed.
        error: Arc<Error>,
        /// Whether the error is considered recoverable or not.
        ///
        /// An error that's recoverable will disable the room's send queue,
        /// while an unrecoverable error will be parked, until the user
        /// decides to cancel sending it.
        is_recoverable: bool,
    },
    /// The local event has been sent successfully to the server.
    Sent {
        /// The event ID assigned by the server.
        event_id: OwnedEventId,
    },
}

/// This type represents the progress of a media (consisting of a file and
/// possibly a thumbnail) being uploaded.
#[derive(Clone, Debug)]
pub struct MediaUploadProgress {
    /// The index of the media within the transaction. A file and its
    /// thumbnail share the same index. Will always be 0 for non-gallery
    /// media uploads.
    pub index: u64,

    /// The combined upload progress across the file and, if existing, its
    /// thumbnail. For gallery uploads, the progress is reported per indexed
    /// gallery item.
    pub progress: AbstractProgress,
}
