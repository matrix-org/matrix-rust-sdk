//! Sliding Sync errors.

use thiserror::Error;

/// Internal representation of errors in Sliding Sync.
#[derive(Error, Debug)]
#[cfg_attr(test, derive(PartialEq))]
#[non_exhaustive]
pub enum Error {
    /// The response we've received from the server can't be parsed or doesn't
    /// match up with the current expectations on the client side. A
    /// `sync`-restart might be required.
    #[error("The sliding sync response could not be handled: {0}")]
    BadResponse(String),

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

    /// Missing storage key when asking to deserialize some sub-state of sliding
    /// sync.
    #[error(
        "A caching request was made but caching was not enabled in this instance of sliding sync"
    )]
    CacheDisabled,

    /// We tried to read the user id of a client but it was missing.
    #[error("Unauthenticated user in sliding sync")]
    UnauthenticatedUser,

    /// The internal channel of `SlidingSync` seems to be broken.
    #[error("SlidingSync's internal channel is broken")]
    InternalChannelIsBroken,

    /// The name of the Sliding Sync instance is too long.
    #[error("The Sliding Sync instance's identifier must be less than 16 chars long")]
    InvalidSlidingSyncIdentifier,
}
