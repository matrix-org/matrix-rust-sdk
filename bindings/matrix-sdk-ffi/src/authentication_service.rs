use std::sync::{Arc, RwLock};

use futures_util::future::join;
use matrix_sdk::{
    matrix_auth::{Session, SessionTokens},
    ruma::{IdParseError, OwnedDeviceId, UserId},
    SessionMeta,
};
use url::Url;
use zeroize::Zeroize;

use super::{client::Client, client_builder::ClientBuilder, RUNTIME};
use crate::{client_builder::Protocol, error::ClientError};

#[derive(uniffi::Object)]
pub struct AuthenticationService {
    base_path: String,
    passphrase: Option<String>,
    user_agent: Option<String>,
    client: RwLock<Option<Arc<Client>>>,
    homeserver_details: RwLock<Option<Arc<HomeserverLoginDetails>>>,
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
    #[error("The homeserver doesn't provide a trusted a sliding sync proxy in its well-known configuration.")]
    SlidingSyncNotAvailable,
    #[error("Login was successful but is missing a valid Session to configure the file store.")]
    SessionMissing,
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

#[derive(uniffi::Object)]
pub struct HomeserverLoginDetails {
    url: String,
    authentication_issuer: Option<String>,
    supports_password_login: bool,
}

#[uniffi::export]
impl HomeserverLoginDetails {
    /// The URL of the currently configured homeserver.
    pub fn url(&self) -> String {
        self.url.clone()
    }

    /// The OIDC Provider that is trusted by the homeserver. `None` when
    /// not configured.
    pub fn authentication_issuer(&self) -> Option<String> {
        self.authentication_issuer.clone()
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
        custom_sliding_sync_proxy: Option<String>,
    ) -> Arc<Self> {
        Arc::new(AuthenticationService {
            base_path,
            passphrase,
            user_agent,
            client: RwLock::new(None),
            homeserver_details: RwLock::new(None),
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
                    Protocol::Http
                } else {
                    Protocol::Https
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

        // Create a new client to setup the store path now the user ID is known.
        let homeserver_url = client.homeserver();
        let session =
            client.inner.matrix_auth().session().ok_or(AuthenticationError::SessionMissing)?;

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
            .username(whoami.user_id.to_string())
            .build_inner()?;

        // Restore the client using the session from the login request.
        client.restore_session_inner(session)?;
        Ok(client)
    }

    /// Restore an existing session on the current homeserver using an access
    /// token issued by an authentication server.
    /// # Arguments
    ///
    /// * `token` - The access token issued by the authentication server.
    ///
    /// * `device_id` - The device ID that the access token was scoped for.
    pub fn restore_with_access_token(
        &self,
        token: String,
        device_id: String,
    ) -> Result<Arc<Client>, AuthenticationError> {
        let Some(client) = self.client.read().unwrap().clone() else {
            return Err(AuthenticationError::ClientMissing);
        };

        // Restore the client and ask the server for the full user ID as this
        // could be different from the username that was entered.
        let discovery_user_id = UserId::parse("@unknown:unknown")
            .map_err(|e| AuthenticationError::Generic { message: e.to_string() })?;
        let device_id: OwnedDeviceId = device_id.as_str().into();

        let discovery_session = Session {
            meta: SessionMeta { user_id: discovery_user_id, device_id: device_id.clone() },
            tokens: SessionTokens { access_token: token.clone(), refresh_token: None },
        };

        client.restore_session_inner(discovery_session)?;
        let whoami = client.whoami()?;

        // Create the actual client with a store path from the user ID.
        let homeserver_url = client.homeserver();
        let session = Session {
            meta: SessionMeta { user_id: whoami.user_id.clone(), device_id },
            tokens: SessionTokens { access_token: token, refresh_token: None },
        };
        let client = self
            .new_client_builder()
            .passphrase(self.passphrase.clone())
            .homeserver_url(homeserver_url)
            .username(whoami.user_id.to_string())
            .build_inner()?;

        // Restore the client using the session.
        client.restore_session_inner(session)?;
        Ok(client)
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
        let supports_password_login = login_details.1?;
        let authentication_issuer = client.authentication_issuer();

        Ok(HomeserverLoginDetails { url, authentication_issuer, supports_password_login })
    }
}
