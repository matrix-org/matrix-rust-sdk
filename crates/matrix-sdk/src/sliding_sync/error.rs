//! Sliding Sync errors.

use thiserror::Error;

/// Internal representation of errors in Sliding Sync.
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// The response we've received from the server can't be parsed or doesn't
    /// match up with the current expectations on the client side. A
    /// `sync`-restart might be required.
    #[error("The sliding sync response could not be handled: {0}")]
    BadResponse(String),
    /// Called `.build()` on a builder type, but the given required field was
    /// missing.
    #[error("Required field missing: `{0}`")]
    BuildMissingField(&'static str),
}
