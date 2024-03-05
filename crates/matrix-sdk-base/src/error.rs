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

#[cfg(feature = "e2e-encryption")]
use matrix_sdk_crypto::{CryptoStoreError, MegolmError, OlmError};
use thiserror::Error;

/// Result type of the rust-sdk.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Internal representation of errors.
#[non_exhaustive]
#[derive(Error, Debug)]
pub enum Error {
    /// Attempting to restore a session after the olm-machine has already been
    /// set up fails
    #[cfg(feature = "e2e-encryption")]
    #[error("The olm machine has already been initialized")]
    BadCryptoStoreState,

    /// The room where a group session should be shared is not encrypted.
    #[cfg(feature = "e2e-encryption")]
    #[error("The room where a group session should be shared is not encrypted")]
    EncryptionNotEnabled,

    /// A generic error returned when the state store fails not due to
    /// IO or (de)serialization.
    #[error(transparent)]
    StateStore(#[from] crate::store::StoreError),

    /// An error occurred in the crypto store.
    #[cfg(feature = "e2e-encryption")]
    #[error(transparent)]
    CryptoStore(#[from] CryptoStoreError),

    /// An error occurred during a E2EE operation.
    #[cfg(feature = "e2e-encryption")]
    #[error(transparent)]
    OlmError(#[from] OlmError),

    /// An error occurred during a group E2EE operation.
    #[cfg(feature = "e2e-encryption")]
    #[error(transparent)]
    MegolmError(#[from] MegolmError),

    /// An error caused by calling matrix-rust-sdk functions with invalid parameters
    #[error("matrix-rust-sdk function was called with invalid parameters")]
    ApiMisuse,
}
