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
use tracing::{info, instrument};
use url::Url;

use super::{Oidc, OidcError};
use crate::Result;

/// Builder type used to configure optional settings for authorization with an
/// OpenID Connect Provider via the Authorization Code flow.
///
/// Created with [`Oidc::authorize_scope()`] or [`Oidc::login()`]. Finalized
/// with [`Self::build()`].
#[allow(missing_debug_implementations)]
pub struct OidcAuthCodeUrlBuilder {
    oidc: Oidc,
    scope: Scope,
    redirect_uri: Url,
    prompt: Option<Vec<Prompt>>,
}

impl OidcAuthCodeUrlBuilder {
    pub(super) fn new(oidc: Oidc, scope: Scope, redirect_uri: Url) -> Self {
        Self { oidc, scope, redirect_uri, prompt: None }
    }

    /// Set the [`Prompt`] of the authorization URL.
    ///
    /// [`Prompt::Create`] can be used to signify that the user wants to
    /// register a new account.
    pub fn prompt(mut self, prompt: Vec<Prompt>) -> Self {
        self.prompt = Some(prompt);
        self
    }

    /// Get the URL that should be presented to login via the Authorization Code
    /// flow.
    ///
    /// This URL should be presented to the user and once they are redirected to
    /// the `redirect_uri`, the authorization can be completed by calling
    /// [`Oidc::finish_authorization()`].
    #[instrument(target = "matrix_sdk::client", skip_all)]
    pub async fn build(self) -> Result<OidcAuthorizationData, OidcError> {
        let Self { oidc, scope, redirect_uri, prompt } = self;

        let data = oidc.data().ok_or(OidcError::NotAuthenticated)?;
        info!(
            issuer = data.issuer_info.issuer,
            %scope, "Authorizing scope via the OpenID Connect Authorization Code flow"
        );

        let provider_metadata = oidc.provider_metadata().await?;

        // TODO: Add support for more parameters.
        let authorization_data = AuthorizationRequestData {
            client_id: data.credentials.client_id(),
            code_challenge_methods_supported: provider_metadata
                .code_challenge_methods_supported
                .as_deref(),
            scope: &scope,
            redirect_uri: &redirect_uri,
            prompt: prompt.as_deref(),
        };

        // TODO: use Pushed Authorization Request if possible.
        // <https://www.rfc-editor.org/rfc/rfc9126>

        let (url, validation_data) = build_authorization_url(
            provider_metadata.authorization_endpoint().clone(),
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
pub struct OidcAuthorizationData {
    /// The URL that should be presented.
    pub url: Url,
    /// A unique identifier for the request.
    pub state: String,
}
