// Copyright 2024 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! The event cache is an abstraction layer, sitting between the Rust SDK and a
//! final client, that acts as a global observer of all the rooms, gathering and
//! inferring some extra useful information about each room. In particular, this
//! doesn't require subscribing to a specific room to get access to this
//! information.
//!
//! It's intended to be fast, robust and easy to maintain, having learned from
//! previous endeavours at implementing middle to high level features elsewhere
//! in the SDK, notably in the UI's Timeline object.
//!
//! See the [github issue](https://github.com/matrix-org/matrix-rust-sdk/issues/3058) for more
//! details about the historical reasons that led us to start writing this.

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

    /// IO error
    #[error(transparent)]
    IO(std::io::Error),
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

impl From<std::io::Error> for IndexError {
    fn from(err: std::io::Error) -> IndexError {
        IndexError::IO(err)
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
