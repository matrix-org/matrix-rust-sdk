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

    /// The event is currently unsupported for this use case.
    #[error("Unsupported event")]
    UnsupportedEvent,

    /// Couldn't read the attachment data from the given URL
    #[error("Invalid attachment data")]
    InvalidAttachmentData,

    /// The attachment file name used as a body is invalid
    #[error("Invalid attachment file name")]
    InvalidAttachmentFileName,

    /// The attachment could not be sent
    #[error("Failed sending attachment")]
    FailedSendingAttachment,

    /// The reaction could not be toggled
    #[error("Failed toggling reaction")]
    FailedToToggleReaction,

    /// The room is not in a joined state.
    #[error("Room is not joined")]
    RoomNotJoined,

    /// Could not get user
    #[error("User ID is not available")]
    UserIdNotAvailable,
}
