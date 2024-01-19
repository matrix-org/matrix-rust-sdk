use std::{collections::HashMap, path::PathBuf, sync::Arc};

use anyhow::Context;
use matrix_sdk::{
    authentication::service::{
        AuthenticationError, AuthenticationService as SdkAuthenticationService,
        HomeserverLoginDetails,
    },
    oidc::{
        registrations::ClientId,
        types::{
            iana::oauth::OAuthClientAuthenticationMethod,
            oidc::ApplicationType,
            registration::{ClientMetadata, Localized},
            requests::GrantType,
        },
        OidcAuthorizationData,
    },
    AuthSession,
};
use ruma::OwnedUserId;
use url::Url;
use zeroize::Zeroize;

use super::{client::Client, client_builder::ClientBuilder, RUNTIME};
use crate::client::ClientSessionDelegate;

#[derive(uniffi::Object)]
pub struct AuthenticationService {
    inner: SdkAuthenticationService,
    passphrase: Option<String>,
    cross_process_refresh_lock_id: Option<String>,
    session_delegate: Option<Arc<dyn ClientSessionDelegate>>,
}

impl Drop for AuthenticationService {
    fn drop(&mut self) {
        self.passphrase.zeroize();
    }
}

/// The configuration to use when authenticating with OIDC.
#[derive(Clone, uniffi::Object)]
pub struct OidcConfiguration {
    pub client_metadata: ClientMetadata,
    pub static_registrations: HashMap<Url, ClientId>,
}

#[uniffi::export]
impl OidcConfiguration {
    /// Creates a new service to authenticate a user with.
    ///
    /// # Arguments
    ///
    /// * `client_name` - The name of the client that will be shown during OIDC
    ///   authentication.
    /// * `redirect_uri` - The redirect URI that will be used when OIDC
    ///   authentication is successful.
    /// * `client_uri` - A URI that contains information about the client.
    /// * `logo_uri` - A URI that contains the client's logo.
    /// * `tos_uri` - A URI that contains the client's terms of service.
    /// * `policy_uri` - A URI that contains the client's privacy policy.
    /// * `contacts` - An array of e-mail addresses of people responsible for
    ///   this client.
    /// * `static_registrations` - Pre-configured registrations for use with
    ///   issuers that don't support dynamic client registration.
    #[uniffi::constructor]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client_name: Option<String>,
        redirect_uri: String,
        client_uri: Option<String>,
        logo_uri: Option<String>,
        tos_uri: Option<String>,
        policy_uri: Option<String>,
        contacts: Option<Vec<String>>,
        static_registrations: HashMap<String, String>,
    ) -> Result<Self, AuthenticationError> {
        let redirect_uri = Url::parse(&redirect_uri).map_err(|_| {
            matrix_sdk::authentication::service::AuthenticationError::OidcCallbackUrlInvalid
        })?;
        let client_name = client_name.as_ref().map(|n| Localized::new(n.to_owned(), []));
        let client_uri = client_uri.localized_url()?;
        let logo_uri = logo_uri.localized_url()?;
        let policy_uri = policy_uri.localized_url()?;
        let tos_uri = tos_uri.localized_url()?;
        let contacts = contacts.clone();

        let client_metadata = ClientMetadata {
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
        };

        let static_registrations = static_registrations
            .iter()
            .filter_map(|(issuer, client_id)| {
                let Ok(issuer) = Url::parse(issuer) else {
                    tracing::error!("Failed to parse {:?}", issuer);
                    return None;
                };
                Some((issuer, ClientId(client_id.clone())))
            })
            .collect();

        Ok(Self { client_metadata, static_registrations })
    }
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

#[uniffi::export]
impl AuthenticationService {
    /// Creates a new service to authenticate a user with.
    #[uniffi::constructor]
    pub fn new(
        base_path: String,
        passphrase: Option<String>,
        user_agent: Option<String>,
        oidc_configuration: Option<Arc<OidcConfiguration>>,
        custom_sliding_sync_proxy: Option<String>,
        session_delegate: Option<Box<dyn ClientSessionDelegate>>,
        cross_process_refresh_lock_id: Option<String>,
    ) -> Arc<Self> {
        let (oidc_client_metadata, oidc_static_registrations) =
            if let Some(oidc_configuration) = oidc_configuration {
                let oidc_configuration = (*oidc_configuration).clone();
                (Some(oidc_configuration.client_metadata), oidc_configuration.static_registrations)
            } else {
                (None, HashMap::new())
            };

        let inner = SdkAuthenticationService::new(
            PathBuf::from(base_path),
            user_agent,
            oidc_client_metadata,
            oidc_static_registrations,
            custom_sliding_sync_proxy,
        );
        Arc::new(AuthenticationService {
            inner,
            passphrase,
            session_delegate: session_delegate.map(Into::into),
            cross_process_refresh_lock_id,
        })
    }

    pub fn homeserver_details(&self) -> Option<Arc<HomeserverLoginDetails>> {
        self.inner.homeserver_details().map(Arc::new)
    }

    /// Updates the service to authenticate with the homeserver for the
    /// specified address.
    pub fn configure_homeserver(
        &self,
        server_name_or_homeserver_url: String,
    ) -> Result<(), AuthenticationError> {
        RUNTIME.block_on(self.inner.configure_homeserver(server_name_or_homeserver_url))
    }

    /// Performs a password login using the current homeserver.
    pub fn login(
        &self,
        username: String,
        password: String,
        initial_device_name: Option<String>,
        device_id: Option<String>,
    ) -> Result<Arc<Client>, AuthenticationError> {
        // Login and ask the server for the full user ID as this could be different from
        // the username that was entered.
        let client = RUNTIME.block_on(self.inner.login(
            username,
            password,
            initial_device_name,
            device_id,
        ))?;
        let whoami = RUNTIME
            .block_on(client.whoami())
            .map_err(|error| AuthenticationError::Generic { message: error.to_string() })?;
        let session = client.matrix_auth().session().context(
            "Login was successful but is missing a valid Session to configure the file store.",
        )?;

        self.finalize_client(client, session, whoami.user_id)
    }

    /// Requests the URL needed for login in a web view using OIDC. Once the web
    /// view has succeeded, call `login_with_oidc_callback` with the callback it
    /// returns.
    pub fn url_for_oidc_login(&self) -> Result<Arc<OidcAuthenticationData>, AuthenticationError> {
        let data = RUNTIME.block_on(self.inner.url_for_oidc_login())?;
        Ok(Arc::new(OidcAuthenticationData { url: data.url, state: data.state }))
    }

    /// Completes the OIDC login process.
    pub fn login_with_oidc_callback(
        &self,
        authentication_data: Arc<OidcAuthenticationData>,
        callback_url: String,
    ) -> Result<Arc<Client>, AuthenticationError> {
        let callback_url =
            Url::parse(&callback_url).map_err(|_| AuthenticationError::OidcCallbackUrlInvalid)?;

        // Login and get the user ID that was resolved by the authentication server.
        let client = RUNTIME.block_on(self.inner.login_with_oidc_callback(
            OidcAuthorizationData {
                url: authentication_data.url.clone(),
                state: authentication_data.state.clone(),
            },
            callback_url,
        ))?;

        let user_id = client.user_id().unwrap().to_owned();
        let session = client.oidc().full_session().context(
            "Login was successful but is missing a valid Session to configure the file store.",
        )?;
        self.finalize_client(client, session, user_id)
    }
}

impl AuthenticationService {
    /// Creates a new client to setup the store path now the user ID is known.
    fn finalize_client(
        &self,
        client: matrix_sdk::Client,
        session: impl Into<AuthSession>,
        user_id: OwnedUserId,
    ) -> Result<Arc<Client>, AuthenticationError> {
        let homeserver_url = client.homeserver();

        let sliding_sync_proxy: Option<String>;
        if let Some(custom_proxy) = self.inner.custom_sliding_sync_proxy.read().unwrap().clone() {
            sliding_sync_proxy = Some(custom_proxy);
        } else if let Some(discovered_proxy) = client.sliding_sync_proxy() {
            sliding_sync_proxy = Some(discovered_proxy.to_string());
        } else {
            sliding_sync_proxy = None;
        }

        let base_path =
            self.inner.base_path.to_str().ok_or(AuthenticationError::InvalidBasePath)?.to_owned();

        let mut builder = ClientBuilder::new()
            .base_path(base_path)
            .passphrase(self.passphrase.clone())
            .homeserver_url(homeserver_url.to_string())
            .sliding_sync_proxy(sliding_sync_proxy)
            .username(user_id.to_string());

        if let Some(user_agent) = self.inner.user_agent.clone() {
            builder = builder.user_agent(user_agent);
        }

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

        let client = builder.build_inner()?;

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
