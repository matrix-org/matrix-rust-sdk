use std::sync::Arc;

use parking_lot::RwLock;

use super::{client::Client, client_builder::ClientBuilder};

pub struct AuthenticationService {
    base_path: String,
    client: RwLock<Option<Arc<Client>>>,
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

impl AuthenticationService {
    /// Creates a new service to authenticate a user with.
    pub fn new(base_path: String) -> Self {
        AuthenticationService { base_path, client: RwLock::new(None) }
    }

    /// The currently configured homeserver.
    pub fn homeserver(&self) -> Result<String, AuthenticationError> {
        self.client
            .read()
            .as_ref()
            .ok_or(AuthenticationError::ClientMissing)
            .map(|client| client.homeserver())
    }

    /// The OIDC Provider that is trusted by the homeserver. `None` when
    /// not configured.
    pub fn authentication_issuer(&self) -> Result<Option<String>, AuthenticationError> {
        self.client
            .read()
            .as_ref()
            .ok_or(AuthenticationError::ClientMissing)
            .map(|client| client.authentication_issuer())
    }

    /// Whether the current homeserver supports the password login flow.
    pub fn supports_password_login(&self) -> Result<bool, AuthenticationError> {
        self.client
            .read()
            .as_ref()
            .ok_or(AuthenticationError::ClientMissing)
            .and_then(|client| client.supports_password_login().map_err(AuthenticationError::from))
    }

    /// Updates the server to authenticate with the specified homeserver.
    pub fn use_server(&self, server_name: String) -> Result<(), AuthenticationError> {
        // Construct a username as the builder currently requires one.
        let username = format!("@auth:{}", server_name);
        let client = Arc::new(ClientBuilder::new())
            .base_path(self.base_path.clone())
            .username(username)
            .build()
            .map_err(AuthenticationError::from)?;

        *self.client.write() = Some(client);
        Ok(())
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
}
