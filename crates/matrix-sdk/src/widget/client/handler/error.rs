//! Message handler errors.

use std::{borrow::Cow, error::Error as StdError, result::Result as StdResult};

use thiserror::Error as ThisError;

pub(crate) type Result<T, E = Error> = StdResult<T, E>;

/// Errors that could be returned when processing incoming requests from a
/// widget.
#[derive(Debug, ThisError)]
pub(crate) enum Error {
    /// Widget has been disconnected (no further communication with a widget
    /// possible).
    #[error("The connection to the widget has been lost")]
    WidgetDisconnected,
    /// Widget replied with an error to one of our (outgoing) requests. The
    /// error type is just a `String`, since we don't really have any special
    /// handling of errors and can't do anything once the error is returned
    /// by a widget except than logging it.
    #[error("Widget replied with an error: {0}")]
    WidgetErrorReply(String),
    /// Something went wrong. We are not very specific here since we don't
    /// differentiate between error cases during the processing as
    /// the only thing that we can do in such case is to simply inform a widget
    /// that something went wrong. These are normally just logged on both sides.
    #[error("{0}")]
    Other(Cow<'static, str>),
}

impl Error {
    /// Converts a given error into a [`Self::Other`].
    pub(crate) fn other(err: impl StdError) -> Self {
        Self::Other(err.to_string().into())
    }

    /// Creates a new [`Self::Other`] error with a given message.
    pub(crate) fn custom(message: impl Into<Cow<'static, str>>) -> Self {
        Self::Other(message.into())
    }
}
