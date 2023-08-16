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

use language_tags::LanguageTag;
use mas_oidc_client::{
    error::TokenRevokeError,
    requests::rp_initiated_logout::{build_end_session_url, LogoutData},
};
use tracing::instrument;
use url::Url;

use super::{Oidc, OidcError};
use crate::Result;

/// Builder type used to configure optional settings for constructing an
/// [RP-Initiated Logout] URL with an OpenID Connect provider.
///
/// Created with [`Oidc::logout()`]. Finalized with [`Self::build()`].
///
/// [RP-Initiated Logout]: https://openid.net/specs/openid-connect-rpinitiated-1_0.html
#[allow(missing_debug_implementations)]
pub struct OidcEndSessionUrlBuilder {
    oidc: Oidc,
    end_session_endpoint: Url,
    client_id: String,
    post_logout_redirect_uri: Option<Url>,
    ui_locales: Option<Vec<LanguageTag>>,
}

impl OidcEndSessionUrlBuilder {
    pub(super) fn new(oidc: Oidc, end_session_endpoint: Url, client_id: String) -> Self {
        Self {
            oidc,
            end_session_endpoint,
            client_id,
            post_logout_redirect_uri: None,
            ui_locales: None,
        }
    }

    /// Set the URI where the user will be redirected after logging out.
    ///
    /// Must be one of the `post_logout_redirect_uris` registered in the client
    /// metadata.
    pub fn post_logout_redirect_uri(mut self, redirect_uri: Url) -> Self {
        self.post_logout_redirect_uri = Some(redirect_uri);
        self
    }

    /// Set the preferred locales of the user.
    ///
    /// Must be ordered from the preferred locale to the least preferred locale.
    pub fn ui_locales(mut self, ui_locales: Vec<LanguageTag>) -> Self {
        self.ui_locales = Some(ui_locales);
        self
    }

    /// Get the URL that should be presented to log out from the OIDC provider's
    /// interface.
    ///
    /// If a `post_logout_redirect_uri` was provided, the user will be
    /// redirected to it after logging out with a `state` query parameter that
    /// is the same as the one in the `OidcEndSessionData`.
    #[instrument(target = "matrix_sdk::client", skip_all)]
    pub fn build(self) -> Result<OidcEndSessionData, OidcError> {
        let Self { oidc, end_session_endpoint, client_id, post_logout_redirect_uri, ui_locales } =
            self;

        // We only need one of those.
        let (id_token_hint, logout_hint) = if let Some(id_token) = oidc.latest_id_token() {
            (Some(id_token.into_string()), None)
        } else {
            let logout_hint = oidc.client.user_id().map(|user_id| format!("mxid:{user_id}"));
            (None, logout_hint)
        };

        let logout_data = LogoutData {
            id_token_hint,
            logout_hint,
            client_id: Some(client_id),
            post_logout_redirect_uri,
            ui_locales,
        };

        let (url, state) =
            build_end_session_url(end_session_endpoint, logout_data, &mut super::rng()?)
                .map_err(TokenRevokeError::from)?;

        Ok(OidcEndSessionData { url, state })
    }
}

/// Data for the user to log out from their account in the issuer's interface.
#[derive(Debug, Clone)]
pub struct OidcEndSessionData {
    /// The URL that should be presented.
    pub url: Url,
    /// A unique identifier for the request, if the user is to be redirected to
    /// the client after logging out.
    pub state: Option<String>,
}
