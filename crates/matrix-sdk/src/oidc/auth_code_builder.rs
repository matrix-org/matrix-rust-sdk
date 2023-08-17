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

use std::{collections::HashSet, num::NonZeroU32};

use chrono::Utc;
use language_tags::LanguageTag;
use mas_oidc_client::{
    requests::authorization_code::{
        build_authorization_url, build_par_authorization_url, AuthorizationRequestData,
    },
    types::{
        requests::{Display, Prompt},
        scope::Scope,
    },
};
use ruma::UserId;
use tracing::{error, info, instrument};
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
    display: Option<Display>,
    prompt: Option<Vec<Prompt>>,
    max_age: Option<NonZeroU32>,
    ui_locales: Option<Vec<LanguageTag>>,
    login_hint: Option<String>,
    acr_values: Option<HashSet<String>>,
}

impl OidcAuthCodeUrlBuilder {
    pub(super) fn new(oidc: Oidc, scope: Scope, redirect_uri: Url) -> Self {
        Self {
            oidc,
            scope,
            redirect_uri,
            display: None,
            prompt: None,
            max_age: None,
            ui_locales: None,
            login_hint: None,
            acr_values: None,
        }
    }

    /// Set how the Authorization Server should display the authentication and
    /// consent user interface pages to the End-User.
    pub fn display(mut self, display: Display) -> Self {
        self.display = Some(display);
        self
    }

    /// Set the [`Prompt`] of the authorization URL.
    ///
    /// [`Prompt::Create`] can be used to signify that the user wants to
    /// register a new account. If [`Prompt::None`] is used, it must be the only
    /// value.
    pub fn prompt(mut self, prompt: Vec<Prompt>) -> Self {
        self.prompt = Some(prompt);
        self
    }

    /// Set the allowable elapsed time in seconds since the last time the
    /// End-User was actively authenticated by the OpenID Provider.
    pub fn max_age(mut self, max_age: NonZeroU32) -> Self {
        self.max_age = Some(max_age);
        self
    }

    /// Set the preferred locales of the user.
    ///
    /// Must be ordered from the preferred locale to the least preferred locale.
    pub fn ui_locales(mut self, ui_locales: Vec<LanguageTag>) -> Self {
        self.ui_locales = Some(ui_locales);
        self
    }

    /// Set the hint to the Authorization Server about the login identifier the
    /// End-User might use to log in.
    ///
    /// To set a Matrix user ID as a login hint, use [`Self::user_id_hint()`].
    ///
    /// Erases any value set with [`Self::user_id_hint()`].
    pub fn login_hint(mut self, login_hint: String) -> Self {
        self.login_hint = Some(login_hint);
        self
    }

    /// Set the hint to the Authorization Server about the Matrix user ID the
    /// End-User might use to log in.
    ///
    /// To set another type of identifier as a login hint, use
    /// [`Self::login_hint()`].
    ///
    /// Erases any value set with [`Self::login_hint()`].
    pub fn user_id_hint(mut self, user_id: &UserId) -> Self {
        self.login_hint = Some(format!("mxid:{user_id}"));
        self
    }

    /// Set the requested Authentication Context Class Reference values.
    ///
    /// This is only necessary with specific providers.
    pub fn acr_values(mut self, acr_values: HashSet<String>) -> Self {
        self.acr_values = Some(acr_values);
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
        let Self {
            oidc,
            scope,
            redirect_uri,
            display,
            prompt,
            max_age,
            ui_locales,
            login_hint,
            acr_values,
        } = self;

        let data = oidc.data().ok_or(OidcError::NotAuthenticated)?;
        info!(
            issuer = data.issuer_info.issuer,
            %scope, "Authorizing scope via the OpenID Connect Authorization Code flow"
        );

        let provider_metadata = oidc.provider_metadata().await?;

        let mut authorization_data = AuthorizationRequestData::new(
            data.credentials.client_id().to_owned(),
            scope,
            redirect_uri,
        );
        authorization_data.code_challenge_methods_supported =
            provider_metadata.code_challenge_methods_supported.clone();
        authorization_data.display = display;
        authorization_data.prompt = prompt;
        authorization_data.max_age = max_age;
        authorization_data.ui_locales = ui_locales;
        authorization_data.login_hint = login_hint;
        authorization_data.acr_values = acr_values;

        if let Some(id_token) = oidc.latest_id_token() {
            authorization_data.id_token_hint = Some(id_token.into_string());
        }

        let authorization_endpoint = provider_metadata.authorization_endpoint();
        let mut rng = super::rng()?;

        // Try a pushed authorization request if the provider supports it.
        let (url, validation_data) = if let Some(par_endpoint) =
            &provider_metadata.pushed_authorization_request_endpoint
        {
            let client_credentials =
                oidc.client_credentials().ok_or(OidcError::NotAuthenticated)?;

            let res = build_par_authorization_url(
                &oidc.http_service(),
                client_credentials.clone(),
                par_endpoint,
                authorization_endpoint.clone(),
                authorization_data.clone(),
                Utc::now(),
                &mut rng,
            )
            .await;

            match res {
                Ok(res) => res,
                Err(error) => {
                    // Keycloak doesn't allow public clients to use the PAR endpoint, so we
                    // should try a regular authorization URL instead.
                    // See: <https://github.com/keycloak/keycloak/issues/8939>
                    let client_metadata =
                        oidc.client_metadata().ok_or(OidcError::NotAuthenticated)?;

                    // If the client said that PAR should be enforced, we should not try without
                    // it, so just return the error.
                    if client_metadata.require_pushed_authorization_requests.unwrap_or_default() {
                        return Err(error.into());
                    }

                    error!(
                        ?error,
                        "Error making a request to the Pushed Authorization Request endpoint. \
                        Falling back to a regular authorization URL"
                    );

                    build_authorization_url(
                        authorization_endpoint.clone(),
                        authorization_data,
                        &mut rng,
                    )?
                }
            }
        } else {
            build_authorization_url(authorization_endpoint.clone(), authorization_data, &mut rng)?
        };

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
