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

use cjson::Error as CjsonError;
use olm_rs::errors::{OlmGroupSessionError, OlmSessionError};
use serde_json::Error as SerdeError;
use thiserror::Error;

use super::store::CryptoStoreError;

pub type Result<T> = std::result::Result<T, OlmError>;

#[derive(Error, Debug)]
pub enum OlmError {
    #[error("signature verification failed")]
    Signature(#[from] SignatureError),
    #[error("failed to read or write to the crypto store {0}")]
    Store(#[from] CryptoStoreError),
    #[error("decryption failed likely because a Olm session was wedged")]
    SessionWedged,
    #[error("the Olm message has a unsupported type")]
    UnsupportedOlmType,
    #[error("the Encrypted message has been encrypted with a unsupported algorithm.")]
    UnsupportedAlgorithm,
    #[error("the Encrypted message doesn't contain a ciphertext for our device")]
    MissingCiphertext,
    #[error("decryption failed because the session to decrypt the message is missing")]
    MissingSession,
    #[error("can't finish Olm Session operation {0}")]
    OlmSession(#[from] OlmSessionError),
    #[error("can't finish Olm Session operation {0}")]
    OlmGroupSession(#[from] OlmGroupSessionError),
    #[error("error deserializing a string to json")]
    JsonError(#[from] SerdeError),
    #[error("the provided JSON value isn't an object")]
    NotAnObject,
}

pub type VerificationResult<T> = std::result::Result<T, SignatureError>;

#[derive(Error, Debug)]
pub enum SignatureError {
    #[error("the provided JSON value isn't an object")]
    NotAnObject,
    #[error("the provided JSON object doesn't contain a signatures field")]
    NoSignatureFound,
    #[error("the provided JSON object can't be converted to a canonical representation")]
    CanonicalJsonError(CjsonError),
    #[error("the signature didn't match the provided key")]
    VerificationError,
}

impl From<CjsonError> for SignatureError {
    fn from(error: CjsonError) -> Self {
        Self::CanonicalJsonError(error)
    }
}
