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

use std::fmt;

use matrix_sdk::event_cache::{paginator::PaginatorError, EventCacheError};
use thiserror::Error;

/// Errors specific to the timeline.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// The requested event with a remote echo is not in the timeline.
    #[error("Event with remote echo not found in timeline")]
    RemoteEventNotInTimeline,

    /// Can't find an event with the given transaction ID, can't retry.
    #[error("Event not found, can't retry sending")]
    RetryEventNotInTimeline,

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

    /// The room is not in a joined state.
    #[error("Room is not joined")]
    RoomNotJoined,

    /// Could not get user.
    #[error("User ID is not available")]
    UserIdNotAvailable,

    /// Something went wrong with the room event cache.
    #[error("Something went wrong with the room event cache.")]
    EventCacheError(#[from] EventCacheError),

    /// An error happened during pagination.
    #[error("An error happened during pagination.")]
    PaginationError(#[from] PaginationError),
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

#[derive(Error)]
#[error("{0}")]
pub struct UnsupportedReplyItem(UnsupportedReplyItemInner);

impl UnsupportedReplyItem {
    pub(super) const MISSING_EVENT_ID: Self = Self(UnsupportedReplyItemInner::MissingEventId);
    pub(super) const MISSING_JSON: Self = Self(UnsupportedReplyItemInner::MissingJson);
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for UnsupportedReplyItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Error)]
enum UnsupportedReplyItemInner {
    #[error("local messages whose event ID is not known can't be replied to currently")]
    MissingEventId,
    #[error("redacted events whose JSON form isn't available can't be replied")]
    MissingJson,
}

#[derive(Error)]
#[error("{0}")]
pub struct UnsupportedEditItem(UnsupportedEditItemInner);

impl UnsupportedEditItem {
    pub(super) const MISSING_EVENT_ID: Self = Self(UnsupportedEditItemInner::MissingEventId);
    pub(super) const NOT_ROOM_MESSAGE: Self = Self(UnsupportedEditItemInner::NotRoomMessage);
    pub(super) const NOT_POLL_EVENT: Self = Self(UnsupportedEditItemInner::NotPollEvent);
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for UnsupportedEditItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Error)]
enum UnsupportedEditItemInner {
    #[error("local messages whose event ID is not known can't be edited currently")]
    MissingEventId,
    #[error("tried to edit a non-message event")]
    NotRoomMessage,
    #[error("tried to edit a non-poll event")]
    NotPollEvent,
}
