use std::sync::Arc;

use futures_util::future::join3;
use matrix_sdk::{
    ruma::{OwnedDeviceId, UserId},
    Session,
};
use parking_lot::RwLock;

use super::{client::Client, client_builder::ClientBuilder, RUNTIME};

pub struct AuthenticationService {
    base_path: String,
    client: RwLock<Option<Arc<Client>>>,
    homeserver_details: RwLock<Option<Arc<HomeserverLoginDetails>>>,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthenticationError {
    #[error("A successful call to use_server must be made first.")]
    ClientMissing,
    #[error("An error occurred: {message}")]
    Generic { message: String },
}

impl From<anyhow::Error> for AuthenticationError {
    fn from(e: anyhow::Error) -> AuthenticationError {
        AuthenticationError::Generic { message: e.to_string() }
    }
}

pub struct HomeserverLoginDetails {
    url: String,
    authentication_issuer: Option<String>,
    supports_password_login: bool,
}

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
    pub fn new(base_path: String) -> Self {
        AuthenticationService {
            base_path,
            client: RwLock::new(None),
            homeserver_details: RwLock::new(None),
        }
    }

    pub fn homeserver_details(&self) -> Option<Arc<HomeserverLoginDetails>> {
        self.homeserver_details.read().clone()
    }

    /// Updates the service to authenticate with the homeserver for the
    /// specified address.
    pub fn configure_homeserver(&self, server_name: String) -> Result<(), AuthenticationError> {
        // Construct a username as the builder currently requires one.
        let username = format!("@auth:{server_name}");

        let mut builder =
            Arc::new(ClientBuilder::new()).base_path(self.base_path.clone()).username(username);

        if server_name.starts_with("http://") || server_name.starts_with("https://") {
            builder = builder.homeserver_url(server_name)
        }

        let client = builder.build().map_err(AuthenticationError::from)?;

        RUNTIME.block_on(async move {
            let details = Arc::new(self.details_from_client(&client).await?);

            *self.client.write() = Some(client);
            *self.homeserver_details.write() = Some(details);

            Ok(())
        })
    }

    /// Performs a password login using the current homeserver.
    pub fn login(
        &self,
        username: String,
        password: String,
    ) -> Result<Arc<Client>, AuthenticationError> {
        match self.client.read().as_ref() {
            Some(client) => {
                let homeserver_url = client.homeserver();

                // Create a new client to setup the store path for the username
                let client = Arc::new(ClientBuilder::new())
                    .base_path(self.base_path.clone())
                    .homeserver_url(homeserver_url)
                    .username(username.clone())
                    .build()
                    .map_err(AuthenticationError::from)?;

                client
                    .login(username, password)
                    .map(|_| client.clone())
                    .map_err(AuthenticationError::from)
            }
            None => Err(AuthenticationError::ClientMissing),
        }
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
        match self.client.read().as_ref() {
            Some(client) => {
                // Restore the client and ask the server for the full user ID as this
                // could be different from the username that was entered.
                let discovery_user_id = UserId::parse("@unknown:unknown")
                    .map_err(|e| AuthenticationError::Generic { message: e.to_string() })?;
                let device_id: OwnedDeviceId = device_id.as_str().into();

                let discovery_session = Session {
                    access_token: token.clone(),
                    user_id: discovery_user_id,
                    device_id: device_id.clone(),
                };

                client.restore_session(discovery_session).map_err(AuthenticationError::from)?;
                let whoami = client.whoami()?;

                // Create the actual client with a store path from the user ID.
                let homeserver_url = client.homeserver();
                let session =
                    Session { access_token: token, user_id: whoami.user_id.clone(), device_id };
                let client = Arc::new(ClientBuilder::new())
                    .base_path(self.base_path.clone())
                    .homeserver_url(homeserver_url)
                    .username(whoami.user_id.to_string())
                    .build()
                    .map_err(AuthenticationError::from)?;

                // Restore the client using the session.
                client
                    .restore_session(session)
                    .map(|_| client.clone())
                    .map_err(AuthenticationError::from)
            }
            None => Err(AuthenticationError::ClientMissing),
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
