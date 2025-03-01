// Copyright 2022 Kévin Commaille
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

use std::borrow::Cow;

use oauth2::{
    basic::BasicClient as OauthClient, AuthUrl, CsrfToken, PkceCodeChallenge, RedirectUrl, Scope,
};
use ruma::{api::client::discovery::get_authorization_server_metadata::msc2965::Prompt, UserId};
use tracing::{info, instrument};
use url::Url;

use super::{Oidc, OidcError};
use crate::{authentication::oidc::AuthorizationValidationData, Result};

/// Builder type used to configure optional settings for authorization with an
/// OpenID Connect Provider via the Authorization Code flow.
///
/// Created with [`Oidc::login()`]. Finalized with [`Self::build()`].
#[allow(missing_debug_implementations)]
pub struct OidcAuthCodeUrlBuilder {
    oidc: Oidc,
    scopes: Vec<Scope>,
    redirect_uri: Url,
    prompt: Option<Vec<Prompt>>,
    login_hint: Option<String>,
}

impl OidcAuthCodeUrlBuilder {
    pub(super) fn new(oidc: Oidc, scopes: Vec<Scope>, redirect_uri: Url) -> Self {
        Self { oidc, scopes, redirect_uri, prompt: None, login_hint: None }
    }

    /// Set the [`Prompt`] of the authorization URL.
    ///
    /// If this is not set, it is assumed that the user wants to log into an
    /// existing account.
    ///
    /// [`Prompt::Create`] can be used to signify that the user wants to
    /// register a new account.
    pub fn prompt(mut self, prompt: Vec<Prompt>) -> Self {
        self.prompt = Some(prompt);
        self
    }

    /// Set the hint to the Authorization Server about the Matrix user ID the
    /// End-User might use to log in, as defined in [MSC4198].
    ///
    /// [MSC4198]: https://github.com/matrix-org/matrix-spec-proposals/pull/4198
    pub fn user_id_hint(mut self, user_id: &UserId) -> Self {
        self.login_hint = Some(format!("mxid:{user_id}"));
        self
    }

    /// Get the URL that should be presented to login via the Authorization Code
    /// flow.
    ///
    /// This URL should be presented to the user and once they are redirected to
    /// the `redirect_uri`, the authorization can be completed by calling
    /// [`Oidc::finish_authorization()`].
    ///
    /// Returns an error if the client registration was not restored, or if a
    /// request fails.
    #[instrument(target = "matrix_sdk::client", skip_all)]
    pub async fn build(self) -> Result<OidcAuthorizationData, OidcError> {
        let Self { oidc, scopes, redirect_uri, prompt, login_hint } = self;

        let data = oidc.data().ok_or(OidcError::NotAuthenticated)?;
        info!(
            issuer = data.issuer,
            ?scopes,
            "Authorizing scope via the OpenID Connect Authorization Code flow"
        );

        let provider_metadata = oidc.provider_metadata().await?;
        let auth_url = AuthUrl::from_url(provider_metadata.authorization_endpoint().clone());

        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        let redirect_uri = RedirectUrl::from_url(redirect_uri);

        let client = OauthClient::new(data.client_id.clone()).set_auth_uri(auth_url);
        let mut request = client
            .authorize_url(CsrfToken::new_random)
            .add_scopes(scopes)
            .set_pkce_challenge(pkce_challenge)
            .set_redirect_uri(Cow::Borrowed(&redirect_uri));

        if let Some(prompt) = prompt {
            // This should be a list of space separated values.
            let prompt_str = prompt.iter().map(Prompt::as_str).collect::<Vec<_>>().join(" ");
            request = request.add_extra_param("prompt", prompt_str);
        }

        if let Some(login_hint) = login_hint {
            request = request.add_extra_param("login_hint", login_hint);
        }

        let (url, state) = request.url();

        data.authorization_data
            .lock()
            .await
            .insert(state.clone(), AuthorizationValidationData { redirect_uri, pkce_verifier });

        Ok(OidcAuthorizationData { url, state })
    }
}

/// The data needed to perform authorization using OpenID Connect.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct OidcAuthorizationData {
    /// The URL that should be presented.
    pub url: Url,
    /// A unique identifier for the request, used to ensure the response
    /// originated from the authentication issuer.
    pub state: CsrfToken,
}

#[cfg(feature = "uniffi")]
#[matrix_sdk_ffi_macros::export]
impl OidcAuthorizationData {
    /// The login URL to use for authorization.
    pub fn login_url(&self) -> String {
        self.url.to_string()
    }
}
