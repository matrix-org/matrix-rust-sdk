use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error, Clone)]
pub enum Error {
    #[error("Unexpected widget disconnect")]
    WidgetDied,
    #[error("Widget error: {0}")]
    WidgetError(String),
    #[error("Capabilities has already been negotiated")]
    AlreadyLoaded,
    #[error("Invalid JSON")]
    InvalidJSON,
    #[error("Unexpected response")]
    UnexpectedResponse,
    #[error("Handler did not send a reply")]
    NoReply,
    #[error("Invalid permissions")]
    InvalidPermissions,
    #[error("Failed to perform an operation")]
    Other,
}
