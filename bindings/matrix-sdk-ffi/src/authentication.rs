use std::{
    collections::HashMap,
    fmt::{self, Debug},
    sync::Arc,
};

use matrix_sdk::{
    oidc::{
        registrations::OidcRegistrationsError,
        types::{
            iana::oauth::OAuthClientAuthenticationMethod,
            oidc::ApplicationType,
            registration::{ClientMetadata, Localized, VerifiedClientMetadata},
            requests::GrantType,
        },
        OidcError as SdkOidcError,
    },
    Error,
};
use url::Url;

use crate::client::{Client, SlidingSyncVersion};

#[derive(uniffi::Object)]
pub struct HomeserverLoginDetails {
    pub(crate) url: String,
    pub(crate) sliding_sync_version: SlidingSyncVersion,
    pub(crate) supports_oidc_login: bool,
    pub(crate) supports_password_login: bool,
}

#[uniffi::export]
impl HomeserverLoginDetails {
    /// The URL of the currently configured homeserver.
    pub fn url(&self) -> String {
        self.url.clone()
    }

    /// The sliding sync version.
    pub fn sliding_sync_version(&self) -> SlidingSyncVersion {
        self.sliding_sync_version.clone()
    }

    /// Whether the current homeserver supports login using OIDC.
    pub fn supports_oidc_login(&self) -> bool {
        self.supports_oidc_login
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

#[uniffi::export(async_runtime = "tokio")]
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
            auth.login_with_sso_callback(url).map_err(|_| SsoError::CallbackUrlInvalid)?;
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

/// The configuration to use when authenticating with OIDC.
#[derive(uniffi::Record)]
pub struct OidcConfiguration {
    /// The name of the client that will be shown during OIDC authentication.
    pub client_name: Option<String>,
    /// The redirect URI that will be used when OIDC authentication is
    /// successful.
    pub redirect_uri: String,
    /// A URI that contains information about the client.
    pub client_uri: Option<String>,
    /// A URI that contains the client's logo.
    pub logo_uri: Option<String>,
    /// A URI that contains the client's terms of service.
    pub tos_uri: Option<String>,
    /// A URI that contains the client's privacy policy.
    pub policy_uri: Option<String>,
    /// An array of e-mail addresses of people responsible for this client.
    pub contacts: Option<Vec<String>>,

    /// Pre-configured registrations for use with issuers that don't support
    /// dynamic client registration.
    pub static_registrations: HashMap<String, String>,

    /// A file path where any dynamic registrations should be stored.
    ///
    /// Suggested value: `{base_path}/oidc/registrations.json`
    pub dynamic_registrations_file: String,
}

impl TryInto<VerifiedClientMetadata> for &OidcConfiguration {
    type Error = OidcError;

    fn try_into(self) -> Result<VerifiedClientMetadata, Self::Error> {
        let redirect_uri =
            Url::parse(&self.redirect_uri).map_err(|_| OidcError::CallbackUrlInvalid)?;
        let client_name = self.client_name.as_ref().map(|n| Localized::new(n.to_owned(), []));
        let client_uri = self.client_uri.localized_url()?;
        let logo_uri = self.logo_uri.localized_url()?;
        let policy_uri = self.policy_uri.localized_url()?;
        let tos_uri = self.tos_uri.localized_url()?;
        let contacts = self.contacts.clone();

        ClientMetadata {
            application_type: Some(ApplicationType::Native),
            redirect_uris: Some(vec![redirect_uri]),
            grant_types: Some(vec![
                GrantType::RefreshToken,
                GrantType::AuthorizationCode,
                GrantType::DeviceCode,
            ]),
            // A native client shouldn't use authentication as the credentials could be intercepted.
            token_endpoint_auth_method: Some(OAuthClientAuthenticationMethod::None),
            // The server should display the following fields when getting the user's consent.
            client_name,
            contacts,
            client_uri,
            logo_uri,
            policy_uri,
            tos_uri,
            ..Default::default()
        }
        .validate()
        .map_err(|_| OidcError::MetadataInvalid)
    }
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum OidcError {
    #[error(
        "The homeserver doesn't provide an authentication issuer in its well-known configuration."
    )]
    NotSupported,
    #[error("Unable to use OIDC as the supplied client metadata is invalid.")]
    MetadataInvalid,
    #[error("Failed to use the supplied registrations file path.")]
    RegistrationsPathInvalid,
    #[error("The supplied callback URL used to complete OIDC is invalid.")]
    CallbackUrlInvalid,
    #[error("The OIDC login was cancelled by the user.")]
    Cancelled,

    #[error("An error occurred: {message}")]
    Generic { message: String },
}

impl From<SdkOidcError> for OidcError {
    fn from(e: SdkOidcError) -> OidcError {
        match e {
            SdkOidcError::MissingAuthenticationIssuer => OidcError::NotSupported,
            SdkOidcError::MissingRedirectUri => OidcError::MetadataInvalid,
            SdkOidcError::InvalidCallbackUrl => OidcError::CallbackUrlInvalid,
            SdkOidcError::InvalidState => OidcError::CallbackUrlInvalid,
            SdkOidcError::CancelledAuthorization => OidcError::Cancelled,
            _ => OidcError::Generic { message: e.to_string() },
        }
    }
}

impl From<OidcRegistrationsError> for OidcError {
    fn from(e: OidcRegistrationsError) -> OidcError {
        match e {
            OidcRegistrationsError::InvalidFilePath => OidcError::RegistrationsPathInvalid,
            _ => OidcError::Generic { message: e.to_string() },
        }
    }
}

impl From<Error> for OidcError {
    fn from(e: Error) -> OidcError {
        match e {
            Error::Oidc(e) => e.into(),
            _ => OidcError::Generic { message: e.to_string() },
        }
    }
}

/* Helpers */

trait OptionExt {
    /// Convenience method to convert a string to a URL and returns it as a
    /// Localized URL. No localization is actually performed.
    fn localized_url(&self) -> Result<Option<Localized<Url>>, OidcError>;
}

impl OptionExt for Option<String> {
    fn localized_url(&self) -> Result<Option<Localized<Url>>, OidcError> {
        self.as_deref()
            .map(|uri| -> Result<Localized<Url>, OidcError> {
                Ok(Localized::new(Url::parse(uri).map_err(|_| OidcError::MetadataInvalid)?, []))
            })
            .transpose()
    }
}
