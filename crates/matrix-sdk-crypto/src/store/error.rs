// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use std::{convert::Infallible, fmt::Debug, io::Error as IoError};

use ruma::{IdParseError, OwnedDeviceId, OwnedUserId};
use serde_json::Error as SerdeError;
use thiserror::Error;

use crate::olm::SessionCreationError;

/// A `CryptoStore` specific result type.
pub type Result<T, E = CryptoStoreError> = std::result::Result<T, E>;

/// The crypto store's error type.
#[derive(Debug, Error)]
pub enum CryptoStoreError {
    /// The account that owns the sessions, group sessions, and devices wasn't
    /// found.
    #[error("can't save/load sessions or group sessions in the store before an account is stored")]
    AccountUnset,

    /// The store doesn't support multiple accounts and data from another device
    /// was discovered.
    #[error(
        "the account in the store doesn't match the account in the constructor: \
        expected {}:{}, got {}:{}", .expected.0, .expected.1, .got.0, .got.1
    )]
    MismatchedAccount {
        /// The expected user/device id pair.
        expected: (OwnedUserId, OwnedDeviceId),
        /// The user/device id pair that was loaded from the store.
        got: (OwnedUserId, OwnedDeviceId),
    },

    /// An IO error occurred.
    #[error(transparent)]
    Io(#[from] IoError),

    /// Failed to decrypt an pickled object.
    #[error("An object failed to be decrypted while unpickling")]
    UnpicklingError,

    /// Failed to decrypt an pickled object.
    #[error(transparent)]
    Pickle(#[from] vodozemac::PickleError),

    /// The received room key couldn't be converted into a valid Megolm session.
    #[error(transparent)]
    SessionCreation(#[from] SessionCreationError),

    /// A Matrix identifier failed to be validated.
    #[error(transparent)]
    IdentifierValidation(#[from] IdParseError),

    /// The store failed to (de)serialize a data type.
    #[error(transparent)]
    Serialization(#[from] SerdeError),

    /// The database format has changed in a backwards incompatible way.
    #[error(
        "The database format changed in an incompatible way, current \
        version: {0}, latest version: {1}"
    )]
    UnsupportedDatabaseVersion(usize, usize),

    /// A problem with the underlying database backend
    #[error(transparent)]
    Backend(Box<dyn std::error::Error + Send + Sync>),

    /// An error due to an invalid generation in a cross-process locking scheme.
    #[error("invalid lock generation: {0}")]
    InvalidLockGeneration(String),
}

impl CryptoStoreError {
    /// Create a new [`Backend`][Self::Backend] error.
    ///
    /// Shorthand for `StoreError::Backend(Box::new(error))`.
    #[inline]
    pub fn backend<E>(error: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Backend(Box::new(error))
    }
}

impl From<Infallible> for CryptoStoreError {
    fn from(never: Infallible) -> Self {
        match never {}
    }
}
