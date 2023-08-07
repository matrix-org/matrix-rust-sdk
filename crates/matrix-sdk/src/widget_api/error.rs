use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error, Clone)]
pub enum Error {
    #[error("Unexpected widget disconnect")]
    WidgetDied,
    #[error("Widget error: {0}")]
    WidgetError(String),
}
