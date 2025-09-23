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
    AuthUrl, CsrfToken, PkceCodeChallenge, RedirectUrl, Scope, basic::BasicClient as OAuthClient,
};
use ruma::{
    OwnedDeviceId, UserId, api::client::discovery::get_authorization_server_metadata::v1::Prompt,
};
use tracing::{info, instrument};
use url::Url;

use super::{ClientRegistrationData, OAuth, OAuthError};
use crate::{Result, authentication::oauth::AuthorizationValidationData};

/// Builder type used to configure optional settings for authorization with an
/// OAuth 2.0 authorization server via the Authorization Code flow.
///
/// Created with [`OAuth::login()`]. Finalized with [`Self::build()`].
#[allow(missing_debug_implementations)]
pub struct OAuthAuthCodeUrlBuilder {
    oauth: OAuth,
    registration_data: Option<ClientRegistrationData>,
    scopes: Vec<Scope>,
    device_id: OwnedDeviceId,
    redirect_uri: Url,
    prompt: Option<Vec<Prompt>>,
    login_hint: Option<String>,
}

impl OAuthAuthCodeUrlBuilder {
    pub(super) fn new(
        oauth: OAuth,
        scopes: Vec<Scope>,
        device_id: OwnedDeviceId,
        redirect_uri: Url,
        registration_data: Option<ClientRegistrationData>,
    ) -> Self {
        Self {
            oauth,
            registration_data,
            scopes,
            device_id,
            redirect_uri,
            prompt: None,
            login_hint: None,
        }
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

    /// Set a generic login hint to help an identity provider pre-fill the login
    /// form.
    ///
    /// Note: This is not the same as the [`Self::user_id_hint()`] method, which
    /// is specifically designed to a) take a `UserId` and no other type of
    /// hint and b) be used directly by MAS and not the identity provider.
    ///
    /// The most likely use case for this method is to pre-fill the login page
    /// using a provisioning link provided by an external party such as
    /// `https://app.example.com/?server_name=example.org&login_hint=alice`
    /// In this instance it is up to the external party to make ensure that the
    /// hint is known to work with their identity provider. For more information
    /// see `login_hint` in <https://openid.net/specs/openid-connect-core-1_0.html#AuthRequest>
    ///
    /// The following methods are mutually exclusive: [`Self::login_hint()`] and
    /// [`Self::user_id_hint()`].
    pub fn login_hint(mut self, login_hint: String) -> Self {
        self.login_hint = Some(login_hint);
        self
    }

    /// Set the hint to the Authorization Server about the Matrix user ID the
    /// End-User might use to log in, as defined in [MSC4198].
    ///
    /// [MSC4198]: https://github.com/matrix-org/matrix-spec-proposals/pull/4198
    ///
    /// The following methods are mutually exclusive: [`Self::login_hint()`] and
    /// [`Self::user_id_hint()`].
    pub fn user_id_hint(mut self, user_id: &UserId) -> Self {
        self.login_hint = Some(format!("mxid:{user_id}"));
        self
    }

    /// Get the URL that should be presented to login via the Authorization Code
    /// flow.
    ///
    /// This URL should be presented to the user and once they are redirected to
    /// the `redirect_uri`, the login can be completed by calling
    /// [`OAuth::finish_login()`].
    ///
    /// Returns an error if the client registration was not restored, or if a
    /// request fails.
    #[instrument(target = "matrix_sdk::client", skip_all)]
    pub async fn build(self) -> Result<OAuthAuthorizationData, OAuthError> {
        let Self { oauth, registration_data, scopes, device_id, redirect_uri, prompt, login_hint } =
            self;

        let server_metadata = oauth.server_metadata().await?;

        oauth.use_registration_data(&server_metadata, registration_data.as_ref()).await?;

        let data = oauth.data().expect("OAuth 2.0 data should be set after registration");
        info!(
            issuer = server_metadata.issuer.as_str(),
            ?scopes,
            "Authorizing scope via the OAuth 2.0 Authorization Code flow"
        );

        let auth_url = AuthUrl::from_url(server_metadata.authorization_endpoint.clone());

        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        let redirect_uri = RedirectUrl::from_url(redirect_uri);

        let client = OAuthClient::new(data.client_id.clone()).set_auth_uri(auth_url);
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

        data.authorization_data.lock().await.insert(
            state.clone(),
            AuthorizationValidationData { server_metadata, device_id, redirect_uri, pkce_verifier },
        );

        Ok(OAuthAuthorizationData { url, state })
    }
}

/// The data needed to perform authorization using OAuth 2.0.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct OAuthAuthorizationData {
    /// The URL that should be presented.
    pub url: Url,
    /// A unique identifier for the request, used to ensure the response
    /// originated from the authentication issuer.
    pub state: CsrfToken,
}

#[cfg(feature = "uniffi")]
#[matrix_sdk_ffi_macros::export]
impl OAuthAuthorizationData {
    /// The login URL to use for authorization.
    pub fn login_url(&self) -> String {
        self.url.to_string()
    }
}
