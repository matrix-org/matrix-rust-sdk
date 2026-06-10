// Copyright 2025 The Matrix.org Foundation C.I.C.
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
// See the License for that specific language governing permissions and
// limitations under the License.

use std::{
    collections::HashMap,
    fmt::{self, Debug},
    sync::Arc,
};

use matrix_sdk::{
    Error,
    authentication::oauth::{
        ClientId, ClientRegistrationData, OAuthError as SdkOAuthError,
        error::OAuthAuthorizationCodeError,
        registration::{ApplicationType, ClientMetadata, Localized, OAuthGrantType},
    },
};
use ruma::serde::Raw;
use url::Url;

use crate::client::{Client, OAuthPrompt, SlidingSyncVersion};

#[derive(uniffi::Object)]
pub struct HomeserverLoginDetails {
    pub(crate) url: String,
    pub(crate) sliding_sync_version: SlidingSyncVersion,
    pub(crate) supports_oauth_login: bool,
    pub(crate) supported_oauth_prompts: Vec<OAuthPrompt>,
    pub(crate) supports_sso_login: bool,
    pub(crate) supports_password_login: bool,
}

#[matrix_sdk_ffi_macros::export]
impl HomeserverLoginDetails {
    /// The URL of the currently configured homeserver.
    pub fn url(&self) -> String {
        self.url.clone()
    }

    /// The sliding sync version.
    pub fn sliding_sync_version(&self) -> SlidingSyncVersion {
        self.sliding_sync_version.clone()
    }

    /// Whether the current homeserver supports login using OAuth.
    pub fn supports_oauth_login(&self) -> bool {
        self.supports_oauth_login
    }

    /// The prompts advertised by the authentication issuer for use in the login
    /// URL.
    pub fn supported_oauth_prompts(&self) -> Vec<OAuthPrompt> {
        self.supported_oauth_prompts.clone()
    }

    /// Whether the current homeserver supports login using legacy SSO.
    pub fn supports_sso_login(&self) -> bool {
        self.supports_sso_login
    }

    /// Whether the current homeserver supports the password login flow.
    pub fn supports_password_login(&self) -> bool {
        self.supports_password_login
    }
}

/// An object encapsulating the SSO login flow
#[derive(uniffi::Object)]
pub struct SsoHandler {
    /// The wrapped Client.
    pub(crate) client: Arc<Client>,

    /// The underlying URL for authentication.
    pub(crate) url: String,
}

#[matrix_sdk_ffi_macros::export]
impl SsoHandler {
    /// Returns the URL for starting SSO authentication. The URL should be
    /// opened in a web view. Once the web view succeeds, call `finish` with
    /// the callback URL.
    pub fn url(&self) -> String {
        self.url.clone()
    }

    /// Completes the SSO login process.
    pub async fn finish(&self, callback_url: String) -> Result<(), SsoError> {
        let auth = self.client.inner.matrix_auth();
        let url = Url::parse(&callback_url).map_err(|_| SsoError::CallbackUrlInvalid)?;
        let builder =
            auth.login_with_sso_callback(url.into()).map_err(|_| SsoError::CallbackUrlInvalid)?;
        builder.await.map_err(|_| SsoError::LoginWithTokenFailed)?;

        Ok(())
    }
}

impl Debug for SsoHandler {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        fmt.debug_struct("SsoHandler").field("url", &self.url).finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum SsoError {
    #[error("The supplied callback URL used to complete SSO is invalid.")]
    CallbackUrlInvalid,
    #[error("Logging in with the token from the supplied callback URL failed.")]
    LoginWithTokenFailed,

    #[error("An error occurred: {message}")]
    Generic { message: String },
}

/// The configuration to use when authenticating with OAuth.
#[derive(uniffi::Record)]
pub struct OAuthConfiguration {
    /// The name of the client that will be shown during OAuth authentication.
    pub client_name: Option<String>,
    /// The redirect URI that will be used when OAuth authentication is
    /// successful.
    pub redirect_uri: String,
    /// A URI that contains information about the client.
    pub client_uri: String,
    /// A URI that contains the client's logo.
    pub logo_uri: Option<String>,
    /// A URI that contains the client's terms of service.
    pub tos_uri: Option<String>,
    /// A URI that contains the client's privacy policy.
    pub policy_uri: Option<String>,

    /// Pre-configured registrations for use with homeservers that don't support
    /// dynamic client registration.
    ///
    /// The keys of the map should be the URLs of the homeservers, but keys
    /// using `issuer` URLs are also supported.
    pub static_registrations: HashMap<String, String>,
}

impl OAuthConfiguration {
    pub(crate) fn redirect_uri(&self) -> Result<Url, OAuthError> {
        Url::parse(&self.redirect_uri).map_err(|_| OAuthError::CallbackUrlInvalid)
    }

    pub(crate) fn client_metadata(&self) -> Result<Raw<ClientMetadata>, OAuthError> {
        let redirect_uri = self.redirect_uri()?;
        let client_name = self.client_name.as_ref().map(|n| Localized::new(n.to_owned(), []));
        let client_uri = self.client_uri.localized_url()?;
        let logo_uri = self.logo_uri.localized_url()?;
        let policy_uri = self.policy_uri.localized_url()?;
        let tos_uri = self.tos_uri.localized_url()?;

        let metadata = ClientMetadata {
            // The server should display the following fields when getting the user's consent.
            client_name,
            logo_uri,
            policy_uri,
            tos_uri,
            ..ClientMetadata::new(
                ApplicationType::Native,
                vec![
                    OAuthGrantType::AuthorizationCode { redirect_uris: vec![redirect_uri] },
                    OAuthGrantType::DeviceCode,
                ],
                client_uri,
            )
        };

        Raw::new(&metadata).map_err(|_| OAuthError::MetadataInvalid)
    }

    pub(crate) fn registration_data(&self) -> Result<ClientRegistrationData, OAuthError> {
        let client_metadata = self.client_metadata()?;

        let mut registration_data = ClientRegistrationData::new(client_metadata);

        if !self.static_registrations.is_empty() {
            let static_registrations = self
                .static_registrations
                .iter()
                .filter_map(|(issuer, client_id)| {
                    let Ok(issuer) = Url::parse(issuer) else {
                        tracing::error!("Failed to parse {issuer:?}");
                        return None;
                    };
                    Some((issuer, ClientId::new(client_id.clone())))
                })
                .collect();

            registration_data.static_registrations = Some(static_registrations);
        }

        Ok(registration_data)
    }
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum OAuthError {
    #[error(
        "The homeserver doesn't provide an authentication issuer in its well-known configuration."
    )]
    NotSupported,
    #[error("Unable to use OAuth as the supplied client metadata is invalid.")]
    MetadataInvalid,
    #[error("The supplied callback URL used to complete OAuth is invalid.")]
    CallbackUrlInvalid,
    #[error("The OAuth login was cancelled by the user.")]
    Cancelled,

    #[error("An error occurred: {message}")]
    Generic { message: String },
}

impl From<SdkOAuthError> for OAuthError {
    fn from(e: SdkOAuthError) -> OAuthError {
        match e {
            SdkOAuthError::Discovery(error) if error.is_not_supported() => OAuthError::NotSupported,
            SdkOAuthError::AuthorizationCode(OAuthAuthorizationCodeError::RedirectUri(_))
            | SdkOAuthError::AuthorizationCode(OAuthAuthorizationCodeError::InvalidState) => {
                OAuthError::CallbackUrlInvalid
            }
            SdkOAuthError::AuthorizationCode(OAuthAuthorizationCodeError::Cancelled) => {
                OAuthError::Cancelled
            }
            _ => OAuthError::Generic { message: e.to_string() },
        }
    }
}

impl From<Error> for OAuthError {
    fn from(e: Error) -> OAuthError {
        match e {
            Error::OAuth(e) => (*e).into(),
            _ => OAuthError::Generic { message: e.to_string() },
        }
    }
}

/* Helpers */

trait OptionExt {
    /// Convenience method to convert an `Option<String>` to a URL and returns
    /// it as a Localized URL. No localization is actually performed.
    fn localized_url(&self) -> Result<Option<Localized<Url>>, OAuthError>;
}

impl OptionExt for Option<String> {
    fn localized_url(&self) -> Result<Option<Localized<Url>>, OAuthError> {
        self.as_deref().map(StrExt::localized_url).transpose()
    }
}

trait StrExt {
    /// Convenience method to convert a string to a URL and returns it as a
    /// Localized URL. No localization is actually performed.
    fn localized_url(&self) -> Result<Localized<Url>, OAuthError>;
}

impl StrExt for str {
    fn localized_url(&self) -> Result<Localized<Url>, OAuthError> {
        Ok(Localized::new(Url::parse(self).map_err(|_| OAuthError::MetadataInvalid)?, []))
    }
}
