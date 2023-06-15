// Copyright 2023 Kévin Commaille
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

use matrix_sdk_base::SessionMeta;
use serde::{Deserialize, Serialize};

use crate::matrix_auth::{self, MatrixAuth, MatrixAuthData};

/// An enum over all the possible authentication APIs.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AuthApi {
    /// The native Matrix authentication API.
    Matrix(MatrixAuth),
}

/// A user session using one of the available authentication APIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum AuthSession {
    /// A session using the native Matrix authentication API.
    Matrix(matrix_auth::Session),
}

impl AuthSession {
    /// Get the matrix user information of this session.
    pub fn meta(&self) -> &SessionMeta {
        match self {
            AuthSession::Matrix(session) => &session.meta,
        }
    }

    /// Get the access token of this session.
    pub fn access_token(&self) -> &str {
        match self {
            AuthSession::Matrix(session) => &session.tokens.access_token,
        }
    }
}

impl From<matrix_auth::Session> for AuthSession {
    fn from(session: matrix_auth::Session) -> Self {
        Self::Matrix(session)
    }
}

/// Data for an authentication API.
#[derive(Clone, Debug)]
pub(crate) enum AuthData {
    /// Data for the native Matrix authentication API.
    Matrix(MatrixAuthData),
}

impl AuthData {
    pub(crate) fn as_matrix(&self) -> Option<&MatrixAuthData> {
        match self {
            AuthData::Matrix(d) => Some(d),
        }
    }

    pub(crate) fn access_token(&self) -> String {
        match self {
            AuthData::Matrix(d) => d.tokens.get().access_token,
        }
    }
}
