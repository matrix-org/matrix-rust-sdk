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
use ruma::api::client::discovery::discover_homeserver::AuthenticationServerInfo;
use tokio::sync::{broadcast, Mutex, OnceCell};

#[cfg(feature = "experimental-oidc")]
use crate::oidc::{self, Oidc, OidcAuthData};
use crate::{
    matrix_auth::{self, MatrixAuth, MatrixAuthData},
    RefreshTokenError, SessionChange,
};

/// All the data relative to authentication, and that must be shared between a
/// client and all its children.
pub(crate) struct AuthCtx {
    /// The authentication server info discovered from the homeserver.
    #[cfg_attr(not(feature = "experimental-oidc"), allow(dead_code))]
    pub(crate) authentication_server_info: Option<AuthenticationServerInfo>,

    /// Whether to try to refresh the access token automatically when an
    /// `M_UNKNOWN_TOKEN` error is encountered.
    pub(crate) handle_refresh_tokens: bool,

    /// Lock making sure we're only doing one token refresh at a time.
    pub(crate) refresh_token_lock: Mutex<Result<(), RefreshTokenError>>,

    /// Session change publisher. Allows the subscriber to handle changes to the
    /// session such as logging out when the access token is invalid or
    /// persisting updates to the access/refresh tokens.
    pub(crate) session_change_sender: broadcast::Sender<SessionChange>,

    /// Authentication data to keep in memory.
    pub(crate) auth_data: OnceCell<AuthData>,
}

/// An enum over all the possible authentication APIs.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AuthApi {
    /// The native Matrix authentication API.
    Matrix(MatrixAuth),

    /// The OpenID Connect API.
    #[cfg(feature = "experimental-oidc")]
    Oidc(Oidc),
}

/// A user session using one of the available authentication APIs.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AuthSession {
    /// A session using the native Matrix authentication API.
    Matrix(matrix_auth::Session),

    /// A session using the OpenID Connect API.
    #[cfg(feature = "experimental-oidc")]
    Oidc(oidc::FullSession),
}

impl AuthSession {
    /// Get the matrix user information of this session.
    pub fn meta(&self) -> &SessionMeta {
        match self {
            AuthSession::Matrix(session) => &session.meta,
            #[cfg(feature = "experimental-oidc")]
            AuthSession::Oidc(session) => &session.user.meta,
        }
    }

    /// Take the matrix user information of this session.
    pub fn into_meta(self) -> SessionMeta {
        match self {
            AuthSession::Matrix(session) => session.meta,
            #[cfg(feature = "experimental-oidc")]
            AuthSession::Oidc(session) => session.user.meta,
        }
    }

    /// Get the access token of this session.
    pub fn access_token(&self) -> &str {
        match self {
            AuthSession::Matrix(session) => &session.tokens.access_token,
            #[cfg(feature = "experimental-oidc")]
            AuthSession::Oidc(session) => &session.user.tokens.access_token,
        }
    }

    /// Get the refresh token of this session.
    pub fn get_refresh_token(&self) -> Option<&str> {
        match self {
            AuthSession::Matrix(session) => session.tokens.refresh_token.as_deref(),
            #[cfg(feature = "experimental-oidc")]
            AuthSession::Oidc(session) => session.user.tokens.refresh_token.as_deref(),
        }
    }
}

impl From<matrix_auth::Session> for AuthSession {
    fn from(session: matrix_auth::Session) -> Self {
        Self::Matrix(session)
    }
}

#[cfg(feature = "experimental-oidc")]
impl From<oidc::FullSession> for AuthSession {
    fn from(session: oidc::FullSession) -> Self {
        Self::Oidc(session)
    }
}

/// Data for an authentication API.
#[derive(Debug)]
pub(crate) enum AuthData {
    /// Data for the native Matrix authentication API.
    Matrix(MatrixAuthData),
    /// Data for the OpenID Connect API.
    #[cfg(feature = "experimental-oidc")]
    Oidc(OidcAuthData),
}

impl AuthData {
    pub(crate) fn as_matrix(&self) -> Option<&MatrixAuthData> {
        match self {
            AuthData::Matrix(d) => Some(d),
            #[cfg(feature = "experimental-oidc")]
            _ => None,
        }
    }

    pub(crate) fn access_token(&self) -> Option<String> {
        let token = match self {
            AuthData::Matrix(d) => d.tokens.get().access_token,
            #[cfg(feature = "experimental-oidc")]
            AuthData::Oidc(d) => d.tokens.get()?.get().access_token,
        };

        Some(token)
    }
}
