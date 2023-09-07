use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use futures_util::future::join;
use matrix_sdk::{
    oidc::{
        types::{
            client_credentials::ClientCredentials,
            errors::ClientErrorCode::AccessDenied,
            iana::oauth::OAuthClientAuthenticationMethod,
            oidc::ApplicationType,
            registration::{ClientMetadata, Localized, VerifiedClientMetadata},
            requests::{GrantType, Prompt},
        },
        AuthorizationResponse, Oidc, OidcError, RegisteredClientData,
    },
    AuthSession,
};
use matrix_sdk_ui::authentication::oidc::{ClientId, OidcRegistrations, OidcRegistrationsError};
use ruma::{
    api::client::discovery::discover_homeserver::AuthenticationServerInfo, IdParseError,
    OwnedUserId,
};
use url::Url;
use zeroize::Zeroize;

use super::{client::Client, client_builder::ClientBuilder, RUNTIME};
use crate::{client_builder::UrlScheme, error::ClientError};

#[derive(uniffi::Object)]
pub struct AuthenticationService {
    base_path: String,
    passphrase: Option<String>,
    user_agent: Option<String>,
    client: RwLock<Option<Arc<Client>>>,
    homeserver_details: RwLock<Option<Arc<HomeserverLoginDetails>>>,
    oidc_configuration: Option<OidcConfiguration>,
    custom_sliding_sync_proxy: RwLock<Option<String>>,
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
    #[error("{message}")]
    InvalidServerName { message: String },
    #[error("The homeserver doesn't provide a trusted sliding sync proxy in its well-known configuration.")]
    SlidingSyncNotAvailable,
    #[error("Login was successful but is missing a valid Session to configure the file store.")]
    SessionMissing,
    #[error("Failed to use the supplied base path.")]
    InvalidBasePath,
    #[error(
        "The homeserver doesn't provide an authentication issuer in its well-known configuration."
    )]
    OidcNotSupported,
    #[error("Unable to use OIDC as no client metadata has been supplied.")]
    OidcMetadataMissing,
    #[error("Unable to use OIDC as the supplied client metadata is invalid.")]
    OidcMetadataInvalid,
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

impl From<IdParseError> for AuthenticationError {
    fn from(e: IdParseError) -> AuthenticationError {
        AuthenticationError::InvalidServerName { message: e.to_string() }
    }
}

impl From<OidcRegistrationsError> for AuthenticationError {
    fn from(e: OidcRegistrationsError) -> AuthenticationError {
        match e {
            OidcRegistrationsError::InvalidBasePath => AuthenticationError::InvalidBasePath,
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
}

/// The data required to authenticate against an OIDC server.
#[derive(uniffi::Object)]
pub struct OidcAuthenticationData {
    /// The underlying URL for authentication.
    url: Url,
    /// A unique identifier for the request, used to ensure the response
    /// originated from the authentication issuer.
    state: String,
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
    supports_oidc_login: bool,
    supports_password_login: bool,
}

#[uniffi::export]
impl HomeserverLoginDetails {
    /// The URL of the currently configured homeserver.
    pub fn url(&self) -> String {
        self.url.clone()
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

#[uniffi::export]
impl AuthenticationService {
    /// Creates a new service to authenticate a user with.
    #[uniffi::constructor]
    pub fn new(
        base_path: String,
        passphrase: Option<String>,
        user_agent: Option<String>,
        oidc_configuration: Option<OidcConfiguration>,
        custom_sliding_sync_proxy: Option<String>,
    ) -> Arc<Self> {
        Arc::new(AuthenticationService {
            base_path,
            passphrase,
            user_agent,
            client: RwLock::new(None),
            homeserver_details: RwLock::new(None),
            oidc_configuration,
            custom_sliding_sync_proxy: RwLock::new(custom_sliding_sync_proxy),
        })
    }

    pub fn homeserver_details(&self) -> Option<Arc<HomeserverLoginDetails>> {
        self.homeserver_details.read().unwrap().clone()
    }

    /// Updates the service to authenticate with the homeserver for the
    /// specified address.
    pub fn configure_homeserver(
        &self,
        server_name_or_homeserver_url: String,
    ) -> Result<(), AuthenticationError> {
        let mut builder = self.new_client_builder();

        // Attempt discovery as a server name first.
        let result = matrix_sdk::sanitize_server_name(&server_name_or_homeserver_url);

        match result {
            Ok(server_name) => {
                let protocol = if server_name_or_homeserver_url.starts_with("http://") {
                    UrlScheme::Http
                } else {
                    UrlScheme::Https
                };
                builder = builder.server_name_with_protocol(server_name.to_string(), protocol);
            }

            Err(e) => {
                // When the input isn't a valid server name check it is a URL.
                // If this is the case, build the client with a homeserver URL.
                if Url::parse(&server_name_or_homeserver_url).is_ok() {
                    builder = builder.homeserver_url(server_name_or_homeserver_url.clone());
                } else {
                    return Err(e.into());
                }
            }
        }

        let client = builder.build_inner().or_else(|e| {
            if !server_name_or_homeserver_url.starts_with("http://")
                && !server_name_or_homeserver_url.starts_with("https://")
            {
                return Err(e);
            }
            // When discovery fails, fallback to the homeserver URL if supplied.
            let mut builder = self.new_client_builder();
            builder = builder.homeserver_url(server_name_or_homeserver_url);
            builder.build_inner()
        })?;

        let details = RUNTIME.block_on(self.details_from_client(&client))?;

        // Now we've verified that it's a valid homeserver, make sure
        // there's a sliding sync proxy available one way or another.
        if self.custom_sliding_sync_proxy.read().unwrap().is_none()
            && client.discovered_sliding_sync_proxy().is_none()
        {
            return Err(AuthenticationError::SlidingSyncNotAvailable);
        }

        *self.client.write().unwrap() = Some(client);
        *self.homeserver_details.write().unwrap() = Some(Arc::new(details));

        Ok(())
    }

    /// Performs a password login using the current homeserver.
    pub fn login(
        &self,
        username: String,
        password: String,
        initial_device_name: Option<String>,
        device_id: Option<String>,
    ) -> Result<Arc<Client>, AuthenticationError> {
        let Some(client) = self.client.read().unwrap().clone() else {
            return Err(AuthenticationError::ClientMissing);
        };

        // Login and ask the server for the full user ID as this could be different from
        // the username that was entered.
        client.login(username, password, initial_device_name, device_id).map_err(|e| match e {
            ClientError::Generic { msg } => AuthenticationError::Generic { message: msg },
        })?;
        let whoami = client.whoami()?;
        let session =
            client.inner.matrix_auth().session().ok_or(AuthenticationError::SessionMissing)?;

        self.finalize_client(client, session, whoami.user_id)
    }

    /// Requests the URL needed for login in a web view using OIDC. Once the web
    /// view has succeeded, call `login_with_oidc_callback` with the callback it
    /// returns.
    pub fn url_for_oidc_login(&self) -> Result<Arc<OidcAuthenticationData>, AuthenticationError> {
        let Some(client) = self.client.read().unwrap().clone() else {
            return Err(AuthenticationError::ClientMissing);
        };

        let Some(authentication_server) = client.discovered_authentication_server() else {
            return Err(AuthenticationError::OidcNotSupported);
        };

        let Some(oidc_configuration) = &self.oidc_configuration else {
            return Err(AuthenticationError::OidcMetadataMissing);
        };

        let redirect_url = Url::parse(&oidc_configuration.redirect_uri)
            .map_err(|_e| AuthenticationError::OidcMetadataInvalid)?;

        let oidc = client.inner.oidc();

        RUNTIME.block_on(async {
            self.configure_oidc(&oidc, authentication_server, oidc_configuration).await?;

            let mut data_builder = oidc.login(redirect_url, None)?;
            // TODO: Add a check for the Consent prompt when MAS is updated.
            data_builder = data_builder.prompt(vec![Prompt::Consent]);
            let data = data_builder.build().await?;

            Ok(Arc::new(OidcAuthenticationData { url: data.url, state: data.state }))
        })
    }

    /// Completes the OIDC login process.
    pub fn login_with_oidc_callback(
        &self,
        authentication_data: Arc<OidcAuthenticationData>,
        callback_url: String,
    ) -> Result<Arc<Client>, AuthenticationError> {
        let Some(client) = self.client.read().unwrap().clone() else {
            return Err(AuthenticationError::ClientMissing);
        };

        let oidc = client.inner.oidc();

        let url =
            Url::parse(&callback_url).map_err(|_| AuthenticationError::OidcCallbackUrlInvalid)?;

        let response = AuthorizationResponse::parse_uri(&url)
            .map_err(|_| AuthenticationError::OidcCallbackUrlInvalid)?;

        let code = match response {
            AuthorizationResponse::Success(code) => code,
            AuthorizationResponse::Error(err) => {
                if err.error.error == AccessDenied {
                    // The user cancelled the login in the web view.
                    return Err(AuthenticationError::OidcCancelled);
                }
                return Err(AuthenticationError::OidcError {
                    message: err.error.error.to_string(),
                });
            }
        };

        if code.state != authentication_data.state {
            return Err(AuthenticationError::OidcCallbackUrlInvalid);
        };

        RUNTIME.block_on(async move {
            oidc.finish_authorization(code).await?;

            oidc.finish_login()
                .await
                .map_err(|e| AuthenticationError::OidcError { message: e.to_string() })
        })?;

        let user_id = client.inner.user_id().unwrap().to_owned();
        let session =
            client.inner.oidc().full_session().ok_or(AuthenticationError::SessionMissing)?;
        self.finalize_client(client, session, user_id)
    }
}

impl AuthenticationService {
    /// A new client builder pre-configured with the service's base path and
    /// user agent if specified
    fn new_client_builder(&self) -> Arc<ClientBuilder> {
        let mut builder = ClientBuilder::new().base_path(self.base_path.clone());

        if let Some(user_agent) = self.user_agent.clone() {
            builder = builder.user_agent(user_agent);
        }

        builder
    }

    /// Get the homeserver login details from a client.
    async fn details_from_client(
        &self,
        client: &Arc<Client>,
    ) -> Result<HomeserverLoginDetails, AuthenticationError> {
        let login_details = join(client.async_homeserver(), client.supports_password_login()).await;

        let url = login_details.0;
        let supports_oidc_login = client.discovered_authentication_server().is_some();
        let supports_password_login = login_details.1.ok().unwrap_or(false);

        Ok(HomeserverLoginDetails { url, supports_oidc_login, supports_password_login })
    }

    /// Handle any necessary configuration in order for login via OIDC to
    /// succeed. This includes performing dynamic client registration against
    /// the homeserver's issuer or restoring a previous registration if one has
    /// been stored.
    async fn configure_oidc(
        &self,
        oidc: &Oidc,
        authentication_server: AuthenticationServerInfo,
        configuration: &OidcConfiguration,
    ) -> Result<(), AuthenticationError> {
        if oidc.client_credentials().is_some() {
            tracing::info!("OIDC is already configured.");
            return Ok(());
        };

        let oidc_metadata = self.oidc_metadata(configuration)?;

        if self.load_client_registration(oidc, &authentication_server, oidc_metadata.clone()).await
        {
            tracing::info!("OIDC configuration loaded from disk.");
            return Ok(());
        }

        tracing::info!("Registering this client for OIDC.");
        let registration_response = oidc
            .register_client(&authentication_server.issuer, oidc_metadata.clone(), None)
            .await?;

        let client_data = RegisteredClientData {
            // The format of the credentials changes according to the client metadata that was sent.
            // Public clients only get a client ID.
            credentials: ClientCredentials::None {
                client_id: registration_response.client_id.clone(),
            },
            metadata: oidc_metadata,
        };
        oidc.restore_registered_client(authentication_server, client_data).await;

        tracing::info!("Persisting OIDC registration data.");
        self.store_client_registration(oidc).await?;

        Ok(())
    }

    /// Stores the current OIDC dynamic client registration so it can be re-used
    /// if we ever log in via the same issuer again.
    async fn store_client_registration(&self, oidc: &Oidc) -> Result<(), AuthenticationError> {
        let issuer = Url::parse(oidc.issuer().ok_or(AuthenticationError::OidcNotSupported)?)
            .map_err(|_| AuthenticationError::OidcError {
                message: String::from("Failed to parse issuer URL."),
            })?;
        let client_id = oidc
            .client_credentials()
            .ok_or(AuthenticationError::OidcError {
                message: String::from("Missing client registration."),
            })?
            .client_id()
            .to_owned();

        let metadata = oidc.client_metadata().ok_or(AuthenticationError::OidcError {
            message: String::from("Missing client metadata."),
        })?;

        let registrations = OidcRegistrations::new(
            &self.base_path,
            metadata.clone(),
            self.oidc_static_registrations(),
        )?;
        registrations.set_and_write_client_id(ClientId(client_id), issuer)?;

        Ok(())
    }

    /// Attempts to load an existing OIDC dynamic client registration for the
    /// currently configured issuer.
    async fn load_client_registration(
        &self,
        oidc: &Oidc,
        authentication_server: &AuthenticationServerInfo,
        oidc_metadata: VerifiedClientMetadata,
    ) -> bool {
        let Ok(issuer) = Url::parse(&authentication_server.issuer) else {
            tracing::error!("Failed to parse {:?}", authentication_server.issuer);
            return false;
        };
        let Some(registrations) = OidcRegistrations::new(
            &self.base_path,
            oidc_metadata.clone(),
            self.oidc_static_registrations(),
        )
        .ok() else {
            return false;
        };
        let Some(client_id) = registrations.client_id(&issuer) else {
            return false;
        };

        let client_data = RegisteredClientData {
            credentials: ClientCredentials::None { client_id: client_id.0 },
            metadata: oidc_metadata,
        };

        oidc.restore_registered_client(authentication_server.clone(), client_data).await;

        true
    }

    /// Creates and verifies OIDC client metadata for the supplied OIDC
    /// configuration.
    fn oidc_metadata(
        &self,
        configuration: &OidcConfiguration,
    ) -> Result<VerifiedClientMetadata, AuthenticationError> {
        let redirect_uri = Url::parse(&configuration.redirect_uri)
            .map_err(|_| AuthenticationError::OidcCallbackUrlInvalid)?;
        let client_name =
            configuration.client_name.as_ref().map(|n| Localized::new(n.to_owned(), []));
        let client_uri = configuration.client_uri.localized_url()?;
        let logo_uri = configuration.logo_uri.localized_url()?;
        let policy_uri = configuration.policy_uri.localized_url()?;
        let tos_uri = configuration.tos_uri.localized_url()?;
        let contacts = configuration.contacts.clone();

        ClientMetadata {
            application_type: Some(ApplicationType::Native),
            redirect_uris: Some(vec![redirect_uri]),
            grant_types: Some(vec![GrantType::RefreshToken, GrantType::AuthorizationCode]),
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

    fn oidc_static_registrations(&self) -> HashMap<Url, ClientId> {
        let registrations = self
            .oidc_configuration
            .as_ref()
            .map(|c| c.static_registrations.clone())
            .unwrap_or_default();
        registrations
            .iter()
            .filter_map(|(issuer, client_id)| {
                let Ok(issuer) = Url::parse(issuer) else {
                    tracing::error!("Failed to parse {:?}", issuer);
                    return None;
                };
                Some((issuer, ClientId(client_id.clone())))
            })
            .collect()
    }

    /// Creates a new client to setup the store path now the user ID is known.
    fn finalize_client(
        &self,
        client: Arc<Client>,
        session: impl Into<AuthSession>,
        user_id: OwnedUserId,
    ) -> Result<Arc<Client>, AuthenticationError> {
        let homeserver_url = client.homeserver();

        let sliding_sync_proxy: Option<String>;
        if let Some(custom_proxy) = self.custom_sliding_sync_proxy.read().unwrap().clone() {
            sliding_sync_proxy = Some(custom_proxy);
        } else if let Some(discovered_proxy) = client.discovered_sliding_sync_proxy() {
            sliding_sync_proxy = Some(discovered_proxy.to_string());
        } else {
            sliding_sync_proxy = None;
        }

        let client = self
            .new_client_builder()
            .passphrase(self.passphrase.clone())
            .homeserver_url(homeserver_url)
            .sliding_sync_proxy(sliding_sync_proxy)
            .username(user_id.to_string())
            .build_inner()?;

        // Restore the client using the session from the login request.
        client.restore_session_inner(session)?;

        Ok(client)
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
