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
