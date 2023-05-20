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

    /// Someone has tried to modify a sliding sync list's ranges, but only the
    /// `Selective` sync mode does allow that.
    #[error("The chosen sync mode for the list `{0}` doesn't allow to modify the ranges; only `Selective` allows that")]
    CannotModifyRanges(String),

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
    #[error("A caching request was made but a storage key is missing in sliding sync")]
    MissingStorageKeyForCaching,

    /// The internal channel of `SlidingSync` seems to be broken.
    #[error("SlidingSync's internal channel is broken")]
    InternalChannelIsBroken,
}
