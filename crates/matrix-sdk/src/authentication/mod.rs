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

use std::{fmt, sync::Arc};

use matrix_sdk_base::{SessionMeta, locks::Mutex};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex as AsyncMutex, OnceCell, broadcast};

pub mod matrix;
pub mod oauth;

use self::{
    matrix::MatrixAuth,
    oauth::{OAuth, OAuthAuthData, OAuthCtx},
};
use crate::{Client, RefreshTokenError, SessionChange};

/// The tokens for a user session.
#[derive(Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
#[allow(missing_debug_implementations)]
pub struct SessionTokens {
    /// The access token used for this session.
    pub access_token: String,

    /// The token used for refreshing the access token, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for SessionTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionTokens").finish_non_exhaustive()
    }
}

/// The tokens for a user session and their state.
pub(crate) struct SessionTokensState {
    /// The inner tokens.
    inner: SessionTokens,

    /// Whether the access token is expired.
    ///
    /// We keep track of this information here, rather than dropping the access
    /// token, because we still want to make most requests with the expired
    /// access token to try to refresh it, or wait for it to be refreshed. If we
    /// make a request without the access token we will get the wrong error.
    access_token_expired: bool,
}

pub(crate) type SessionCallbackError = Box<dyn std::error::Error + Send + Sync>;

#[cfg(not(target_family = "wasm"))]
pub(crate) type SaveSessionCallback =
    dyn Fn(Client) -> Result<(), SessionCallbackError> + Send + Sync;
#[cfg(target_family = "wasm")]
pub(crate) type SaveSessionCallback = dyn Fn(Client) -> Result<(), SessionCallbackError>;

#[cfg(not(target_family = "wasm"))]
pub(crate) type ReloadSessionCallback =
    dyn Fn(Client) -> Result<SessionTokens, SessionCallbackError> + Send + Sync;
#[cfg(target_family = "wasm")]
pub(crate) type ReloadSessionCallback =
    dyn Fn(Client) -> Result<SessionTokens, SessionCallbackError>;

/// All the data relative to authentication, and that must be shared between a
/// client and all its children.
pub(crate) struct AuthCtx {
    oauth: OAuthCtx,

    /// Whether to try to refresh the access token automatically when an
    /// `M_UNKNOWN_TOKEN` error is encountered.
    pub(crate) handle_refresh_tokens: bool,

    /// Lock making sure we're only doing one token refresh at a time.
    refresh_token_lock: Arc<AsyncMutex<Result<(), RefreshTokenError>>>,

    /// Session change publisher. Allows the subscriber to handle changes to the
    /// session such as logging out when the access token is invalid or
    /// persisting updates to the access/refresh tokens.
    pub(crate) session_change_sender: broadcast::Sender<SessionChange>,

    /// Authentication data to keep in memory.
    pub(crate) auth_data: OnceCell<AuthData>,

    /// The current session tokens and their state.
    tokens: OnceCell<Mutex<SessionTokensState>>,

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

impl AuthCtx {
    /// Construct a new `AuthCtx` with the given settings.
    pub(crate) fn new(handle_refresh_tokens: bool, allow_insecure_oauth: bool) -> Self {
        Self {
            handle_refresh_tokens,
            refresh_token_lock: Arc::new(AsyncMutex::new(Ok(()))),
            session_change_sender: broadcast::Sender::new(1),
            auth_data: OnceCell::default(),
            tokens: OnceCell::default(),
            reload_session_callback: OnceCell::default(),
            save_session_callback: OnceCell::default(),
            oauth: OAuthCtx::new(allow_insecure_oauth),
        }
    }

    /// The current session tokens.
    pub(crate) fn session_tokens(&self) -> Option<SessionTokens> {
        Some(self.tokens.get()?.lock().inner.clone())
    }

    /// The current access token.
    pub(crate) fn access_token(&self) -> Option<String> {
        Some(self.tokens.get()?.lock().inner.access_token.clone())
    }

    /// Whether we have a valid session token.
    pub(crate) fn has_valid_access_token(&self) -> bool {
        self.tokens.get().is_some_and(|tokens| !tokens.lock().access_token_expired)
    }

    /// Set the current session tokens.
    pub(crate) fn set_session_tokens(&self, session_tokens: SessionTokens) {
        let session_tokens = SessionTokensState {
            inner: session_tokens,
            // We just got the tokens, so we assume that they are not expired.
            access_token_expired: false,
        };

        if let Some(tokens) = self.tokens.get() {
            *tokens.lock() = session_tokens;
        } else {
            let _ = self.tokens.set(Mutex::new(session_tokens));
        }
    }

    /// Set the given access token as expired.
    ///
    /// We take the value of the access token to make sure that we don't mark
    /// the wrong access token as expired.
    pub(crate) fn set_access_token_expired(&self, access_token: &str) {
        if let Some(tokens) = self.tokens.get() {
            let mut tokens = tokens.lock();

            if tokens.inner.access_token == access_token {
                tokens.access_token_expired = true;
            }
        }
    }
}

/// An enum over all the possible authentication APIs.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AuthApi {
    /// The native Matrix authentication API.
    Matrix(MatrixAuth),

    /// The OAuth 2.0 API.
    OAuth(OAuth),
}

/// A user session using one of the available authentication APIs.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum AuthSession {
    /// A session using the native Matrix authentication API.
    Matrix(matrix::MatrixSession),

    /// A session using the OAuth 2.0 API.
    OAuth(Box<oauth::OAuthSession>),
}

impl AuthSession {
    /// Get the matrix user information of this session.
    pub fn meta(&self) -> &SessionMeta {
        match self {
            AuthSession::Matrix(session) => &session.meta,
            AuthSession::OAuth(session) => &session.user.meta,
        }
    }

    /// Take the matrix user information of this session.
    pub fn into_meta(self) -> SessionMeta {
        match self {
            AuthSession::Matrix(session) => session.meta,
            AuthSession::OAuth(session) => session.user.meta,
        }
    }

    /// Get the access token of this session.
    pub fn access_token(&self) -> &str {
        match self {
            AuthSession::Matrix(session) => &session.tokens.access_token,
            AuthSession::OAuth(session) => &session.user.tokens.access_token,
        }
    }

    /// Get the refresh token of this session.
    pub fn get_refresh_token(&self) -> Option<&str> {
        match self {
            AuthSession::Matrix(session) => session.tokens.refresh_token.as_deref(),
            AuthSession::OAuth(session) => session.user.tokens.refresh_token.as_deref(),
        }
    }
}

impl From<matrix::MatrixSession> for AuthSession {
    fn from(session: matrix::MatrixSession) -> Self {
        Self::Matrix(session)
    }
}

impl From<oauth::OAuthSession> for AuthSession {
    fn from(session: oauth::OAuthSession) -> Self {
        Self::OAuth(session.into())
    }
}

/// Data for an authentication API.
#[derive(Debug)]
pub(crate) enum AuthData {
    /// Data for the native Matrix authentication API.
    Matrix,
    /// Data for the OAuth 2.0 API.
    OAuth(OAuthAuthData),
}
