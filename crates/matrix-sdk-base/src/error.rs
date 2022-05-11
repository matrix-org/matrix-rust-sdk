// Copyright 2020 Damir Jelić
// Copyright 2020 The Matrix.org Foundation C.I.C.
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

//! Error conditions.

use std::io::Error as IoError;

#[cfg(feature = "e2e-encryption")]
use matrix_sdk_crypto::{CryptoStoreError, MegolmError, OlmError};
use serde_json::Error as JsonError;
use thiserror::Error;

/// Result type of the rust-sdk.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Internal representation of errors.
#[non_exhaustive]
#[derive(Error, Debug)]
pub enum Error {
    /// Queried endpoint requires authentication but was called on an anonymous
    /// client.
    #[error("the queried endpoint requires authentication but was called before logging in")]
    AuthenticationRequired,

    /// Attempting to restore a session after the olm-machine has already been
    /// set up fails
    #[cfg(feature = "e2e-encryption")]
    #[error("The olm machine has already been initialized")]
    BadCryptoStoreState,

    /// A generic error returned when the state store fails not due to
    /// IO or (de)serialization.
    #[error(transparent)]
    StateStore(#[from] crate::store::StoreError),

    /// An error when (de)serializing JSON.
    #[error(transparent)]
    SerdeJson(#[from] JsonError),

    /// An error representing IO errors.
    #[error(transparent)]
    IoError(#[from] IoError),

    /// An error occurred in the crypto store.
    #[cfg(feature = "e2e-encryption")]
    #[error(transparent)]
    CryptoStore(#[from] CryptoStoreError),

    /// An error occurred during a E2EE operation.
    #[cfg(feature = "e2e-encryption")]
    #[error(transparent)]
    OlmError(#[from] OlmError),

    /// An error occurred during a E2EE group operation.
    #[cfg(feature = "e2e-encryption")]
    #[error(transparent)]
    MegolmError(#[from] MegolmError),
}
