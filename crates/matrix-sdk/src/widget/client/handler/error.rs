use std::{borrow::Cow, error::Error as StdError, result::Result as StdResult};

use thiserror::Error as ThisError;

pub(crate) type Result<T, E = Error> = StdResult<T, E>;

#[derive(Debug, ThisError)]
pub enum Error {
    #[error("The connection to the widget has been lost")]
    WidgetDisconnected,
    #[error("Widget replied with an error: {0}")]
    WidgetErrorReply(String),
    #[error("{0}")]
    Other(Cow<'static, str>),
}

impl Error {
    pub fn other(err: impl StdError) -> Self {
        Self::Other(err.to_string().into())
    }

    pub fn custom(message: impl Into<Cow<'static, str>>) -> Self {
        Self::Other(message.into())
    }
}
