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

//! Types and functions related to authentication in Matrix.

use std::sync::Arc;

use as_variant::as_variant;
use matrix_sdk_base::SessionMeta;
use tokio::sync::{broadcast, Mutex, OnceCell};

pub mod matrix;
pub mod oidc;

use self::{
    matrix::{MatrixAuth, MatrixAuthData},
    oidc::{Oidc, OidcAuthData, OidcCtx},
};
use crate::{Client, RefreshTokenError, SessionChange};

#[cfg(all(feature = "e2e-encryption", not(target_arch = "wasm32")))]
pub mod qrcode;

/// Session tokens, for any kind of authentication.
#[allow(missing_debug_implementations, clippy::large_enum_variant)]
pub enum SessionTokens {
    /// Tokens for a [`matrix`] session.
    Matrix(matrix::MatrixSessionTokens),
    /// Tokens for an [`oidc`] session.
    Oidc(oidc::OidcSessionTokens),
}

pub(crate) type SessionCallbackError = Box<dyn std::error::Error + Send + Sync>;
pub(crate) type SaveSessionCallback =
    dyn Fn(Client) -> Result<(), SessionCallbackError> + Send + Sync;
pub(crate) type ReloadSessionCallback =
    dyn Fn(Client) -> Result<SessionTokens, SessionCallbackError> + Send + Sync;

/// All the data relative to authentication, and that must be shared between a
/// client and all its children.
pub(crate) struct AuthCtx {
    pub(crate) oidc: OidcCtx,

    /// Whether to try to refresh the access token automatically when an
    /// `M_UNKNOWN_TOKEN` error is encountered.
    pub(crate) handle_refresh_tokens: bool,

    /// Lock making sure we're only doing one token refresh at a time.
    pub(crate) refresh_token_lock: Arc<Mutex<Result<(), RefreshTokenError>>>,

    /// Session change publisher. Allows the subscriber to handle changes to the
    /// session such as logging out when the access token is invalid or
    /// persisting updates to the access/refresh tokens.
    pub(crate) session_change_sender: broadcast::Sender<SessionChange>,

    /// Authentication data to keep in memory.
    pub(crate) auth_data: OnceCell<AuthData>,

    /// A callback called whenever we need an absolute source of truth for the
    /// current session tokens.
    ///
    /// This is required only in multiple processes setups.
    pub(crate) reload_session_callback: OnceCell<Box<ReloadSessionCallback>>,

    /// A callback to save a session back into the app's secure storage.
    ///
    /// This is always called, independently of the presence of a cross-process
    /// lock.
    ///
    /// Internal invariant: this must be called only after `set_session_tokens`
    /// has been called, not before.
    pub(crate) save_session_callback: OnceCell<Box<SaveSessionCallback>>,
}

/// An enum over all the possible authentication APIs.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AuthApi {
    /// The native Matrix authentication API.
    Matrix(MatrixAuth),

    /// The OpenID Connect API.
    Oidc(Oidc),
}

/// A user session using one of the available authentication APIs.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AuthSession {
    /// A session using the native Matrix authentication API.
    Matrix(matrix::MatrixSession),

    /// A session using the OpenID Connect API.
    Oidc(Box<oidc::OidcSession>),
}

impl AuthSession {
    /// Get the matrix user information of this session.
    pub fn meta(&self) -> &SessionMeta {
        match self {
            AuthSession::Matrix(session) => &session.meta,
            AuthSession::Oidc(session) => &session.user.meta,
        }
    }

    /// Take the matrix user information of this session.
    pub fn into_meta(self) -> SessionMeta {
        match self {
            AuthSession::Matrix(session) => session.meta,
            AuthSession::Oidc(session) => session.user.meta,
        }
    }

    /// Get the access token of this session.
    pub fn access_token(&self) -> &str {
        match self {
            AuthSession::Matrix(session) => &session.tokens.access_token,
            AuthSession::Oidc(session) => &session.user.tokens.access_token,
        }
    }

    /// Get the refresh token of this session.
    pub fn get_refresh_token(&self) -> Option<&str> {
        match self {
            AuthSession::Matrix(session) => session.tokens.refresh_token.as_deref(),
            AuthSession::Oidc(session) => session.user.tokens.refresh_token.as_deref(),
        }
    }
}

impl From<matrix::MatrixSession> for AuthSession {
    fn from(session: matrix::MatrixSession) -> Self {
        Self::Matrix(session)
    }
}

impl From<oidc::OidcSession> for AuthSession {
    fn from(session: oidc::OidcSession) -> Self {
        Self::Oidc(session.into())
    }
}

/// Data for an authentication API.
#[derive(Debug)]
pub(crate) enum AuthData {
    /// Data for the native Matrix authentication API.
    Matrix(MatrixAuthData),
    /// Data for the OpenID Connect API.
    Oidc(OidcAuthData),
}

impl AuthData {
    pub(crate) fn as_matrix(&self) -> Option<&MatrixAuthData> {
        as_variant!(self, Self::Matrix)
    }

    pub(crate) fn access_token(&self) -> Option<String> {
        let token = match self {
            Self::Matrix(d) => d.tokens.get().access_token,
            Self::Oidc(d) => d.tokens.get()?.get().access_token,
        };

        Some(token)
    }
}
