//! Sliding Sync errors.

use matrix_sdk_common::executor::JoinError;
use thiserror::Error;

/// Internal representation of errors in Sliding Sync.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// Sliding sync has been configured with a missing version. See
    /// [`crate::sliding_sync::Version`].
    #[error("Sliding sync version is missing")]
    VersionIsMissing,

    /// The response we've received from the server can't be parsed or doesn't
    /// match up with the current expectations on the client side. A
    /// `sync`-restart might be required.
    #[error("The sliding sync response could not be handled: {0}")]
    BadResponse(String),

    /// A `SlidingSyncListRequestGenerator` has been used without having been
    /// initialized. It happens when a response is handled before a request has
    /// been sent. It usually happens when testing.
    #[error(
        "The sliding sync list `{0}` is handling a response, \
         but its request generator has not been initialized"
    )]
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
    #[error(
        "A task failed to execute to completion; \
         task description: {task_description}, error: {error}"
    )]
    JoinError {
        /// Task description.
        task_description: String,
        /// The original `JoinError`.
        error: JoinError,
    },

    /// No Olm machine.
    #[cfg(feature = "e2e-encryption")]
    #[error("The Olm machine is missing")]
    NoOlmMachine,

    /// An error occurred during a E2EE operation.
    #[cfg(feature = "e2e-encryption")]
    #[error(transparent)]
    CryptoStoreError(#[from] matrix_sdk_base::crypto::CryptoStoreError),
}
