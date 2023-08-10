//! Sliding Sync errors.

use thiserror::Error;
use tokio::task::JoinError;

/// Internal representation of errors in Sliding Sync.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// The response we've received from the server can't be parsed or doesn't
    /// match up with the current expectations on the client side. A
    /// `sync`-restart might be required.
    #[error("The sliding sync response could not be handled: {0}")]
    BadResponse(String),

    /// The response we've received from the server has already been received in
    /// the past because it has a `pos` that we have recently seen.
    #[error("The sliding sync response has already been received: `pos={pos:?}`")]
    ResponseAlreadyReceived {
        /// The `pos`ition that has been received.
        pos: Option<String>,
    },

    /// A `SlidingSyncListRequestGenerator` has been used without having been
    /// initialized. It happens when a response is handled before a request has
    /// been sent. It usually happens when testing.
    #[error("The sliding sync list `{0}` is handling a response, but its request generator has not been initialized")]
    RequestGeneratorHasNotBeenInitialized(String),

    /// Ranges have a `start` bound greater than `end`.
    #[error("Ranges have invalid bounds: `{start}..{end}`")]
    InvalidRange {
        /// Start bound.
        start: u32,
        /// End bound.
        end: u32,
    },

    /// We tried to read the user id of a client but it was missing.
    #[error("Unauthenticated user in sliding sync")]
    UnauthenticatedUser,

    /// The internal channel of `SlidingSync` seems to be broken.
    #[error("SlidingSync's internal channel is broken")]
    InternalChannelIsBroken,

    /// The name of the Sliding Sync instance is too long.
    #[error("The Sliding Sync instance's identifier must be less than 16 chars long")]
    InvalidSlidingSyncIdentifier,

    /// A task failed to execute to completion.
    #[error("A task failed to execute to completion; task description: {task_description}")]
    JoinError {
        /// Task description.
        task_description: String,
        /// The original `JoinError`.
        error: JoinError,
    },
}
