use std::sync::{Arc, RwLock};

use futures_util::future::join3;
use matrix_sdk::{
    ruma::{OwnedDeviceId, UserId},
    Session,
};
use zeroize::Zeroize;

use super::{client::Client, client_builder::ClientBuilder, RUNTIME};

pub struct AuthenticationService {
    base_path: String,
    passphrase: Option<String>,
    client: RwLock<Option<Arc<Client>>>,
    homeserver_details: RwLock<Option<Arc<HomeserverLoginDetails>>>,
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

impl AuthenticationService {
    /// Creates a new service to authenticate a user with.
    pub fn new(base_path: String, passphrase: Option<String>) -> Self {
        AuthenticationService {
            base_path,
            passphrase,
            client: RwLock::new(None),
            homeserver_details: RwLock::new(None),
        }
    }

    /// Get the homeserver login details from a client.
    async fn details_from_client(
        &self,
        client: &Arc<Client>,
    ) -> Result<HomeserverLoginDetails, AuthenticationError> {
        let login_details = join3(
            client.async_homeserver(),
            client.authentication_issuer(),
            client.supports_password_login(),
        )
        .await;

        let url = login_details.0;
        let authentication_issuer = login_details.1;
        let supports_password_login = login_details.2.map_err(AuthenticationError::from)?;

        Ok(HomeserverLoginDetails { url, authentication_issuer, supports_password_login })
    }
}

#[uniffi::export]
impl AuthenticationService {
    pub fn homeserver_details(&self) -> Option<Arc<HomeserverLoginDetails>> {
        self.homeserver_details.read().unwrap().clone()
    }

    /// Updates the service to authenticate with the homeserver for the
    /// specified address.
    pub fn configure_homeserver(&self, server_name: String) -> Result<(), AuthenticationError> {
        let mut builder = Arc::new(ClientBuilder::new()).base_path(self.base_path.clone());

        if server_name.starts_with("http://") || server_name.starts_with("https://") {
            builder = builder.homeserver_url(server_name)
        } else {
            builder = builder.server_name(server_name);
        }

        let client = builder.build().map_err(AuthenticationError::from)?;

        RUNTIME.block_on(async move {
            if client.sliding_sync_proxy().await.is_none() {
                return Err(AuthenticationError::SlidingSyncNotAvailable);
            }

            let details = Arc::new(self.details_from_client(&client).await?);

            *self.client.write().unwrap() = Some(client);
            *self.homeserver_details.write().unwrap() = Some(details);

            Ok(())
        })
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
        client
            .login(username, password, initial_device_name, device_id)
            .map_err(AuthenticationError::from)?;
        let whoami = client.whoami()?;

        // Create a new client to setup the store path now the user ID is known.
        let homeserver_url = client.homeserver();
        let session = client.client.session().ok_or(AuthenticationError::SessionMissing)?;
        let client = Arc::new(ClientBuilder::new())
            .base_path(self.base_path.clone())
            .passphrase(self.passphrase.clone())
            .homeserver_url(homeserver_url)
            .username(whoami.user_id.to_string())
            .build()
            .map_err(AuthenticationError::from)?;

        // Restore the client using the session from the login request.
        client.restore_session_inner(session).map_err(AuthenticationError::from)?;
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
            access_token: token.clone(),
            refresh_token: None,
            user_id: discovery_user_id,
            device_id: device_id.clone(),
        };

        client.restore_session_inner(discovery_session).map_err(AuthenticationError::from)?;
        let whoami = client.whoami()?;

        // Create the actual client with a store path from the user ID.
        let homeserver_url = client.homeserver();
        let session = Session {
            access_token: token,
            refresh_token: None,
            user_id: whoami.user_id.clone(),
            device_id,
        };
        let client = Arc::new(ClientBuilder::new())
            .base_path(self.base_path.clone())
            .passphrase(self.passphrase.clone())
            .homeserver_url(homeserver_url)
            .username(whoami.user_id.to_string())
            .build()
            .map_err(AuthenticationError::from)?;

        // Restore the client using the session.
        client.restore_session_inner(session).map_err(AuthenticationError::from)?;
        Ok(client)
    }
}
