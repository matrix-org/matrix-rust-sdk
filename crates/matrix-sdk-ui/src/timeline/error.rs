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

use matrix_sdk::{
    event_cache::{paginator::PaginatorError, EventCacheError},
    send_queue::RoomSendQueueError,
    HttpError,
};
use thiserror::Error;

use crate::timeline::{pinned_events_loader::PinnedEventsLoaderError, TimelineEventItemId};

/// Errors specific to the timeline.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// The requested event is not in the timeline.
    #[error("Event not found in timeline: {0:?}")]
    EventNotInTimeline(TimelineEventItemId),

    /// The event is currently unsupported for this use case..
    #[error("Unsupported event")]
    UnsupportedEvent,

    /// Couldn't read the attachment data from the given URL.
    #[error("Invalid attachment data")]
    InvalidAttachmentData,

    /// The attachment file name used as a body is invalid.
    #[error("Invalid attachment file name")]
    InvalidAttachmentFileName,

    /// The attachment could not be sent.
    #[error("Failed sending attachment")]
    FailedSendingAttachment,

    /// The reaction could not be toggled.
    #[error("Failed toggling reaction")]
    FailedToToggleReaction,

    /// Couldn't read the encryption state of the room.
    #[error("The room's encryption state is unknown.")]
    UnknownEncryptionState,

    /// Something went wrong with the room event cache.
    #[error(transparent)]
    EventCacheError(#[from] EventCacheError),

    /// An error happened during pagination.
    #[error(transparent)]
    PaginationError(#[from] PaginationError),

    /// An error happened during pagination.
    #[error(transparent)]
    PinnedEventsError(#[from] PinnedEventsLoaderError),

    /// An error happened while operating the room's send queue.
    #[error(transparent)]
    SendQueueError(#[from] RoomSendQueueError),

    /// An error happened while attempting to edit an event.
    #[error(transparent)]
    EditError(#[from] EditError),

    /// An error happened while attempting to redact an event.
    #[error(transparent)]
    RedactError(#[from] RedactError),
}

#[derive(Error, Debug)]
pub enum EditError {
    /// The content types have changed.
    #[error("the new content type ({new}) doesn't match that of the previous content ({original}")]
    ContentMismatch { original: String, new: String },

    /// The local echo we tried to edit has been lost.
    #[error("Invalid state: the local echo we tried to abort has been lost.")]
    InvalidLocalEchoState,

    /// An error happened at a lower level.
    #[error(transparent)]
    RoomError(#[from] matrix_sdk::room::edit::EditError),
}

#[derive(Error, Debug)]
pub enum RedactError {
    /// Local event to redact wasn't found for transaction id
    #[error("Event to redact wasn't found for item id {0:?}")]
    ItemNotFound(TimelineEventItemId),

    /// An error happened while attempting to redact an event.
    #[error(transparent)]
    HttpError(#[from] HttpError),

    /// The local echo we tried to abort has been lost.
    #[error("Invalid state: the local echo we tried to abort has been lost.")]
    InvalidLocalEchoState,
}

#[derive(Error, Debug)]
pub enum PaginationError {
    /// The timeline isn't in the event focus mode.
    #[error("The timeline isn't in the event focus mode")]
    NotEventFocusMode,

    /// An error occurred while paginating.
    #[error("Error when paginating.")]
    Paginator(#[source] PaginatorError),
}

#[derive(Debug, Error)]
pub enum UnsupportedReplyItem {
    #[error("local messages whose event ID is not known can't be replied to currently")]
    MissingEventId,
    #[error("redacted events whose JSON form isn't available can't be replied")]
    MissingJson,
    #[error("event to reply to not found")]
    MissingEvent,
    #[error("failed to deserialize event to reply to")]
    FailedToDeserializeEvent,
    #[error("tried to reply to a state event")]
    StateEvent,
}

#[derive(Debug, Error)]
pub enum UnsupportedEditItem {
    #[error("tried to edit a non-poll event")]
    NotPollEvent,
    #[error("tried to edit another user's event")]
    NotOwnEvent,
    #[error("event to edit not found")]
    MissingEvent,
}

#[derive(Debug, Error)]
pub enum SendEventError {
    #[error(transparent)]
    UnsupportedEditItem(#[from] UnsupportedEditItem),

    #[error(transparent)]
    RoomQueueError(#[from] RoomSendQueueError),
}
