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
use std::fmt::{Display, Formatter, Result as FmtResult};

#[derive(Debug)]
pub enum SignatureError {
    NotAnObject,
    NoSignatureFound,
    CanonicalJsonError(CjsonError),
    VerificationError,
}

impl Display for SignatureError {
    fn fmt(&self, f: &mut Formatter) -> FmtResult {
        let message = match self {
            SignatureError::NotAnObject => "The provided JSON value isn't an object.",
            SignatureError::NoSignatureFound => {
                "The provided JSON object doesn't contain a signatures field."
            }
            SignatureError::CanonicalJsonError(_) => {
                "The provided JSON object can't be converted to a canonical representation."
            }
            SignatureError::VerificationError => "The signature didn't match the provided key.",
        };

        write!(f, "{}", message)
    }
}

impl From<CjsonError> for SignatureError {
    fn from(error: CjsonError) -> Self {
        Self::CanonicalJsonError(error)
    }
}
