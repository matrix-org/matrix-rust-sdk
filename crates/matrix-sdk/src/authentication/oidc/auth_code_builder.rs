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

use mas_oidc_client::{
    requests::authorization_code::{build_authorization_url, AuthorizationRequestData},
    types::{requests::Prompt, scope::Scope},
};
use ruma::UserId;
use tracing::{info, instrument};
use url::Url;

use super::{Oidc, OidcError};
use crate::Result;

/// Builder type used to configure optional settings for authorization with an
/// OpenID Connect Provider via the Authorization Code flow.
///
/// Created with [`Oidc::login()`]. Finalized with [`Self::build()`].
#[allow(missing_debug_implementations)]
pub struct OidcAuthCodeUrlBuilder {
    oidc: Oidc,
    scope: Scope,
    redirect_uri: Url,
    prompt: Option<Vec<Prompt>>,
    login_hint: Option<String>,
}

impl OidcAuthCodeUrlBuilder {
    pub(super) fn new(oidc: Oidc, scope: Scope, redirect_uri: Url) -> Self {
        Self { oidc, scope, redirect_uri, prompt: None, login_hint: None }
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
        let Self { oidc, scope, redirect_uri, prompt, login_hint } = self;

        let data = oidc.data().ok_or(OidcError::NotAuthenticated)?;
        info!(
            issuer = data.issuer,
            %scope, "Authorizing scope via the OpenID Connect Authorization Code flow"
        );

        let provider_metadata = oidc.provider_metadata().await?;

        let mut authorization_data =
            AuthorizationRequestData::new(data.client_id.0.clone(), scope, redirect_uri);
        authorization_data.code_challenge_methods_supported =
            provider_metadata.code_challenge_methods_supported.clone();
        authorization_data.prompt = prompt;
        authorization_data.login_hint = login_hint;

        let authorization_endpoint = provider_metadata.authorization_endpoint();

        let (url, validation_data) = build_authorization_url(
            authorization_endpoint.clone(),
            authorization_data,
            &mut super::rng()?,
        )?;

        let state = validation_data.state.clone();

        data.authorization_data.lock().await.insert(state.clone(), validation_data);

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
    pub state: String,
}

#[cfg(feature = "uniffi")]
#[matrix_sdk_ffi_macros::export]
impl OidcAuthorizationData {
    /// The login URL to use for authorization.
    pub fn login_url(&self) -> String {
        self.url.to_string()
    }
}
