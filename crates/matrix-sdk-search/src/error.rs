#![forbid(missing_docs)]

use tantivy::{
    directory::error::OpenDirectoryError as TantivyOpenDirectoryError,
    query::QueryParserError as TantivyQueryParserError,
};
use thiserror::Error;

/// Internal representation of Search Index errors.
#[derive(Error, Debug)]
pub enum IndexError {
    /// Tantivy Error
    #[error(transparent)]
    TantivyError(tantivy::TantivyError),

    /// Open Directory Error
    #[error(transparent)]
    OpenDirectoryError(TantivyOpenDirectoryError),

    /// Query Parse Error
    #[error(transparent)]
    QueryParserError(TantivyQueryParserError),

    /// Schema Error
    #[error(transparent)]
    IndexSchemaError(IndexSchemaError),

    /// Write Error
    #[error(transparent)]
    IndexWriteError(IndexWriteError),

    /// Search Error
    #[error(transparent)]
    IndexSearchError(IndexSearchError),

    /// Add Event Error
    #[error("Failed to add event to index")]
    EventNotAdded,

    /// Message Type Error
    #[error("Message type not supported")]
    MessageTypeNotSupported,

    /// Indexing Redacted Message Error
    #[error("Cannot index redacted message")]
    CannotIndexRedactedMessage,

    /// Indexing Empty Message Error
    #[error("Cannot index empty message")]
    EmptyMessage,
}

impl From<tantivy::TantivyError> for IndexError {
    fn from(err: tantivy::TantivyError) -> IndexError {
        IndexError::TantivyError(err)
    }
}

impl From<TantivyOpenDirectoryError> for IndexError {
    fn from(err: TantivyOpenDirectoryError) -> IndexError {
        IndexError::OpenDirectoryError(err)
    }
}

impl From<TantivyQueryParserError> for IndexError {
    fn from(err: TantivyQueryParserError) -> IndexError {
        IndexError::QueryParserError(err)
    }
}

impl From<IndexSchemaError> for IndexError {
    fn from(err: IndexSchemaError) -> IndexError {
        IndexError::IndexSchemaError(err)
    }
}

impl From<IndexWriteError> for IndexError {
    fn from(err: IndexWriteError) -> IndexError {
        IndexError::IndexWriteError(err)
    }
}

impl From<IndexSearchError> for IndexError {
    fn from(err: IndexSearchError) -> IndexError {
        IndexError::IndexSearchError(err)
    }
}

/// Internal representation of Schema errors.
#[derive(Error, Debug)]
pub enum IndexSchemaError {
    /// Tantivy Error
    #[error(transparent)]
    TantivyError(tantivy::TantivyError),
}

impl From<tantivy::TantivyError> for IndexSchemaError {
    fn from(err: tantivy::TantivyError) -> IndexSchemaError {
        IndexSchemaError::TantivyError(err)
    }
}

/// Internal representation of Writer errors.
#[derive(Error, Debug)]
pub enum IndexWriteError {
    /// Tantivy Error
    #[error(transparent)]
    TantivyError(tantivy::TantivyError),
}

impl From<tantivy::TantivyError> for IndexWriteError {
    fn from(err: tantivy::TantivyError) -> IndexWriteError {
        IndexWriteError::TantivyError(err)
    }
}

/// Internal representation of Search errors.
#[derive(Error, Debug)]
pub enum IndexSearchError {
    /// Tantivy Error
    #[error(transparent)]
    TantivyError(tantivy::TantivyError),
}

impl From<tantivy::TantivyError> for IndexSearchError {
    fn from(err: tantivy::TantivyError) -> IndexSearchError {
        IndexSearchError::TantivyError(err)
    }
}
