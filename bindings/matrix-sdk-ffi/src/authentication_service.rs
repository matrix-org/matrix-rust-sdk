use std::{
    collections::HashMap,
    sync::{Arc, RwLock as StdRwLock},
};

use matrix_sdk::{
    encryption::BackupDownloadStrategy,
    oidc::{
        registrations::OidcRegistrationsError,
        types::{
            iana::oauth::OAuthClientAuthenticationMethod,
            oidc::ApplicationType,
            registration::{ClientMetadata, Localized, VerifiedClientMetadata},
            requests::GrantType,
        },
        OidcError,
    },
    ClientBuildError as MatrixClientBuildError, HttpError, RumaApiError,
};
use ruma::api::error::{DeserializationError, FromHttpResponseError};
use tokio::sync::RwLock as AsyncRwLock;
use url::Url;
use zeroize::Zeroize;

use super::{client::Client, client_builder::ClientBuilder};
use crate::{
    client::ClientSessionDelegate,
    client_builder::{CertificateBytes, ClientBuildError},
    error::ClientError,
};

#[derive(uniffi::Object)]
pub struct AuthenticationService {
    session_path: String,
    passphrase: Option<String>,
    user_agent: Option<String>,
    client: AsyncRwLock<Option<Client>>,
    homeserver_details: StdRwLock<Option<Arc<HomeserverLoginDetails>>>,
    oidc_configuration: Option<OidcConfiguration>,
    custom_sliding_sync_proxy: Option<String>,
    cross_process_refresh_lock_id: Option<String>,
    session_delegate: Option<Arc<dyn ClientSessionDelegate>>,
    additional_root_certificates: Vec<CertificateBytes>,
    proxy: Option<String>,
}

impl Drop for AuthenticationService {
    fn drop(&mut self) {
        self.passphrase.zeroize();
    }
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
#[uniffi(flat_error)]
pub enum AuthenticationError {
    #[error("A successful call to configure_homeserver must be made first.")]
    ClientMissing,

    #[error("The supplied server name is invalid.")]
    InvalidServerName,
    #[error(transparent)]
    ServerUnreachable(HttpError),
    #[error(transparent)]
    WellKnownLookupFailed(RumaApiError),
    #[error(transparent)]
    WellKnownDeserializationError(DeserializationError),
    #[error("The homeserver doesn't provide a trusted sliding sync proxy in its well-known configuration.")]
    SlidingSyncNotAvailable,

    #[error("Login was successful but is missing a valid Session to configure the file store.")]
    SessionMissing,

    #[error(
        "The homeserver doesn't provide an authentication issuer in its well-known configuration."
    )]
    OidcNotSupported,
    #[error("Unable to use OIDC as no client metadata has been supplied.")]
    OidcMetadataMissing,
    #[error("Unable to use OIDC as the supplied client metadata is invalid.")]
    OidcMetadataInvalid,
    #[error("Failed to use the supplied registrations file path.")]
    OidcRegistrationsPathInvalid,
    #[error("The supplied callback URL used to complete OIDC is invalid.")]
    OidcCallbackUrlInvalid,
    #[error("The OIDC login was cancelled by the user.")]
    OidcCancelled,
    #[error("An error occurred with OIDC: {message}")]
    OidcError { message: String },

    #[error("An error occurred: {message}")]
    Generic { message: String },
}

impl From<anyhow::Error> for AuthenticationError {
    fn from(e: anyhow::Error) -> AuthenticationError {
        AuthenticationError::Generic { message: e.to_string() }
    }
}

impl From<ClientBuildError> for AuthenticationError {
    fn from(e: ClientBuildError) -> AuthenticationError {
        match e {
            ClientBuildError::Sdk(MatrixClientBuildError::InvalidServerName) => {
                AuthenticationError::InvalidServerName
            }

            ClientBuildError::Sdk(MatrixClientBuildError::Http(e)) => {
                AuthenticationError::ServerUnreachable(e)
            }

            ClientBuildError::Sdk(MatrixClientBuildError::AutoDiscovery(
                FromHttpResponseError::Server(e),
            )) => AuthenticationError::WellKnownLookupFailed(e),

            ClientBuildError::Sdk(MatrixClientBuildError::AutoDiscovery(
                FromHttpResponseError::Deserialization(e),
            )) => AuthenticationError::WellKnownDeserializationError(e),

            _ => AuthenticationError::Generic { message: e.to_string() },
        }
    }
}

impl From<OidcRegistrationsError> for AuthenticationError {
    fn from(e: OidcRegistrationsError) -> AuthenticationError {
        match e {
            OidcRegistrationsError::InvalidFilePath => {
                AuthenticationError::OidcRegistrationsPathInvalid
            }
            _ => AuthenticationError::OidcError { message: e.to_string() },
        }
    }
}

impl From<OidcError> for AuthenticationError {
    fn from(e: OidcError) -> AuthenticationError {
        AuthenticationError::OidcError { message: e.to_string() }
    }
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

/// The data required to authenticate against an OIDC server.
#[derive(uniffi::Object)]
pub struct OidcAuthenticationData {
    /// The underlying URL for authentication.
    pub(crate) url: Url,
    /// A unique identifier for the request, used to ensure the response
    /// originated from the authentication issuer.
    pub(crate) state: String,
}

#[uniffi::export]
impl OidcAuthenticationData {
    /// The login URL to use for authentication.
    pub fn login_url(&self) -> String {
        self.url.to_string()
    }
}

#[derive(uniffi::Object)]
pub struct HomeserverLoginDetails {
    url: String,
    sliding_sync_proxy: Option<String>,
    supports_oidc_login: bool,
    supports_password_login: bool,
}

#[uniffi::export]
impl HomeserverLoginDetails {
    /// The URL of the currently configured homeserver.
    pub fn url(&self) -> String {
        self.url.clone()
    }

    /// The URL of the discovered or manually set sliding sync proxy,
    /// if any.
    pub fn sliding_sync_proxy(&self) -> Option<String> {
        self.sliding_sync_proxy.clone()
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

#[uniffi::export(async_runtime = "tokio")]
impl AuthenticationService {
    /// Creates a new service to authenticate a user with.
    ///
    /// # Arguments
    ///
    /// * `session_path` - A path to the directory where the session data will
    ///   be stored. A new directory **must** be given for each subsequent
    ///   session as the database isn't designed to be shared.
    ///
    /// * `passphrase` - An optional passphrase to use to encrypt the session
    ///   data.
    ///
    /// * `user_agent` - An optional user agent to use when making requests.
    ///
    /// * `additional_root_certificates` - Additional root certificates to trust
    ///   when making requests when built with rustls.
    ///
    /// * `proxy` - An optional HTTP(S) proxy URL to use when making requests.
    ///
    /// * `oidc_configuration` - Configuration data about the app to use during
    ///   OIDC authentication. This is required if OIDC authentication is to be
    ///   used.
    ///
    /// * `custom_sliding_sync_proxy` - An optional sliding sync proxy URL that
    ///   will override the proxy discovered from the homeserver's well-known.
    ///
    /// * `session_delegate` - A delegate that will handle token refresh etc.
    ///   when the cross-process lock is configured.
    ///
    /// * `cross_process_refresh_lock_id` - A process ID to use for
    ///   cross-process token refresh locks.
    #[uniffi::constructor]
    // TODO: This has too many arguments, even clippy agrees. Many of these methods are the same as
    // for the `ClientBuilder`. We should let people pass in a `ClientBuilder` and possibly convert
    // this to a builder pattern as well.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        session_path: String,
        passphrase: Option<String>,
        user_agent: Option<String>,
        additional_root_certificates: Vec<Vec<u8>>,
        proxy: Option<String>,
        oidc_configuration: Option<OidcConfiguration>,
        custom_sliding_sync_proxy: Option<String>,
        session_delegate: Option<Box<dyn ClientSessionDelegate>>,
        cross_process_refresh_lock_id: Option<String>,
    ) -> Arc<Self> {
        Arc::new(AuthenticationService {
            session_path,
            passphrase,
            user_agent,
            client: AsyncRwLock::new(None),
            homeserver_details: StdRwLock::new(None),
            oidc_configuration,
            custom_sliding_sync_proxy,
            session_delegate: session_delegate.map(Into::into),
            cross_process_refresh_lock_id,
            additional_root_certificates,
            proxy,
        })
    }

    pub fn homeserver_details(&self) -> Option<Arc<HomeserverLoginDetails>> {
        self.homeserver_details.read().unwrap().clone()
    }

    /// Updates the service to authenticate with the homeserver for the
    /// specified address.
    pub async fn configure_homeserver(
        &self,
        server_name_or_homeserver_url: String,
    ) -> Result<(), AuthenticationError> {
        let builder =
            self.new_client_builder()?.server_name_or_homeserver_url(server_name_or_homeserver_url);

        let client = builder.build_inner().await?;

        // Compute homeserver login details.
        let details = {
            let supports_oidc_login =
                client.inner.oidc().fetch_authentication_issuer().await.is_ok();
            let supports_password_login =
                client.supports_password_login().await.ok().unwrap_or(false);
            let sliding_sync_proxy =
                client.sliding_sync_proxy().map(|proxy_url| proxy_url.to_string());

            HomeserverLoginDetails {
                url: client.homeserver(),
                sliding_sync_proxy,
                supports_oidc_login,
                supports_password_login,
            }
        };

        // Make sure there's a sliding sync proxy available.
        if self.custom_sliding_sync_proxy.is_none() && details.sliding_sync_proxy().is_none() {
            return Err(AuthenticationError::SlidingSyncNotAvailable);
        }

        *self.client.write().await = Some(client);
        *self.homeserver_details.write().unwrap() = Some(Arc::new(details));

        Ok(())
    }

    /// Performs a password login using the current homeserver.
    pub async fn login(
        &self,
        username: String,
        password: String,
        initial_device_name: Option<String>,
        device_id: Option<String>,
    ) -> Result<Arc<Client>, AuthenticationError> {
        let client_guard = self.client.read().await;
        let Some(client) = client_guard.as_ref() else {
            return Err(AuthenticationError::ClientMissing);
        };

        // Login and ask the server for the full user ID as this could be different from
        // the username that was entered.
        client.login(username, password, initial_device_name, device_id).await.map_err(
            |e| match e {
                ClientError::Generic { msg } => AuthenticationError::Generic { message: msg },
            },
        )?;

        drop(client_guard);

        // Now that the client is logged in we can take ownership away from the service
        // to ensure there aren't two clients at any point later.
        let Some(client) = self.client.write().await.take() else {
            return Err(AuthenticationError::ClientMissing);
        };

        Ok(Arc::new(client))
    }

    /// Requests the URL needed for login in a web view using OIDC. Once the web
    /// view has succeeded, call `login_with_oidc_callback` with the callback it
    /// returns.
    pub async fn url_for_oidc_login(
        &self,
    ) -> Result<Arc<OidcAuthenticationData>, AuthenticationError> {
        let client_guard = self.client.read().await;
        let Some(client) = client_guard.as_ref() else {
            return Err(AuthenticationError::ClientMissing);
        };

        let Some(oidc_configuration) = &self.oidc_configuration else {
            return Err(AuthenticationError::OidcMetadataMissing);
        };

        client.url_for_oidc_login(oidc_configuration).await
    }

    /// Completes the OIDC login process.
    pub async fn login_with_oidc_callback(
        &self,
        authentication_data: Arc<OidcAuthenticationData>,
        callback_url: String,
    ) -> Result<Arc<Client>, AuthenticationError> {
        let client_guard = self.client.read().await;
        let Some(client) = client_guard.as_ref() else {
            return Err(AuthenticationError::ClientMissing);
        };

        client.login_with_oidc_callback(authentication_data, callback_url).await?;

        drop(client_guard);

        // Now that the client is logged in we can take ownership away from the service
        // to ensure there aren't two clients at any point later.
        let Some(client) = self.client.write().await.take() else {
            return Err(AuthenticationError::ClientMissing);
        };

        Ok(Arc::new(client))
    }
}

impl AuthenticationService {
    /// Create a new client builder that is pre-configured with the parameters
    /// passed to the service along with some other sensible defaults
    fn new_client_builder(&self) -> Result<Arc<ClientBuilder>, AuthenticationError> {
        let mut builder = ClientBuilder::new()
            .session_path(self.session_path.clone())
            .passphrase(self.passphrase.clone())
            .sliding_sync_proxy(self.custom_sliding_sync_proxy.clone())
            .auto_enable_cross_signing(true)
            .backup_download_strategy(BackupDownloadStrategy::AfterDecryptionFailure)
            .auto_enable_backups(true);

        if let Some(user_agent) = self.user_agent.clone() {
            builder = builder.user_agent(user_agent);
        }

        if let Some(proxy) = &self.proxy {
            builder = builder.proxy(proxy.to_owned())
        }

        builder = builder.add_root_certificates(self.additional_root_certificates.clone());

        if let Some(id) = &self.cross_process_refresh_lock_id {
            let Some(ref session_delegate) = self.session_delegate else {
                return Err(AuthenticationError::OidcError {
                    message: "cross-process refresh lock requires session delegate".to_owned(),
                });
            };
            builder = builder
                .enable_cross_process_refresh_lock_inner(id.clone(), session_delegate.clone());
        } else if let Some(ref session_delegate) = self.session_delegate {
            builder = builder.set_session_delegate_inner(session_delegate.clone());
        }

        Ok(builder)
    }
}

impl TryInto<VerifiedClientMetadata> for &OidcConfiguration {
    type Error = AuthenticationError;

    fn try_into(self) -> Result<VerifiedClientMetadata, Self::Error> {
        let redirect_uri = Url::parse(&self.redirect_uri)
            .map_err(|_| AuthenticationError::OidcCallbackUrlInvalid)?;
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
        .map_err(|_| AuthenticationError::OidcMetadataInvalid)
    }
}

trait OptionExt {
    /// Convenience method to convert a string to a URL and returns it as a
    /// Localized URL. No localization is actually performed.
    fn localized_url(&self) -> Result<Option<Localized<Url>>, AuthenticationError>;
}

impl OptionExt for Option<String> {
    fn localized_url(&self) -> Result<Option<Localized<Url>>, AuthenticationError> {
        self.as_deref()
            .map(|uri| -> Result<Localized<Url>, AuthenticationError> {
                Ok(Localized::new(
                    Url::parse(uri).map_err(|_| AuthenticationError::OidcMetadataInvalid)?,
                    [],
                ))
            })
            .transpose()
    }
}
