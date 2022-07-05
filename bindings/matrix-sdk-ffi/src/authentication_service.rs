use std::sync::Arc;

use parking_lot::RwLock;

use super::{client::Client, client_builder::ClientBuilder};

pub struct AuthenticationService {
    base_path: String,
    client_container: RwLock<ClientContainer>,
}

struct ClientContainer {
    client: Option<Arc<Client>>,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthenticationError {
    #[error("A successfull call to use_server must be made first.")]
    ClientMissing,
    #[error("The client could not be built: {message}")]
    ClientBuilderFailed { message: String },
    #[error("Unable to get the supported login flows: {message}")]
    GetLoginFlowsFailed { message: String },
    #[error("Login was unsuccessful: {message}")]
    LoginFailed { message: String },
}

impl From<anyhow::Error> for AuthenticationError {
    fn from(e: anyhow::Error) -> AuthenticationError {
        AuthenticationError::LoginFailed { message: e.to_string() }
    }
}

impl AuthenticationService {
    /// Creates a new service to authenticate with the specified server.
    pub fn new(base_path: String) -> Self {
        AuthenticationService {
            base_path,
            client_container: RwLock::new(ClientContainer { client: None }),
        }
    }

    /// The currently configured homeserver.
    pub fn homeserver(&self) -> Result<String, AuthenticationError> {
        self.client_container
            .read()
            .client
            .as_ref()
            .ok_or(AuthenticationError::ClientMissing)
            .and_then(|client| Ok(client.homeserver()))
    }

    /// The OIDC Provider that is trusted by the homeserver.
    pub fn authentication_issuer(&self) -> Result<Option<String>, AuthenticationError> {
        self.client_container
            .read()
            .client
            .as_ref()
            .ok_or(AuthenticationError::ClientMissing)
            .and_then(|client| Ok(client.authentication_issuer()))
    }

    /// Whether the current homeserver supports the password login flow.
    pub fn supports_password_login(&self) -> Result<bool, AuthenticationError> {
        self.client_container
            .read()
            .client
            .as_ref()
            .ok_or(AuthenticationError::ClientMissing)
            .and_then(|client| {
                client.supports_password_login().map_err(|error| {
                    AuthenticationError::GetLoginFlowsFailed { message: error.to_string() }
                })
            })
    }

    /// Updates the server to authenticate with the specified homeserver.
    pub fn use_server(&self, server_name: String) -> Result<(), AuthenticationError> {
        // Construct a username as the builder currently requires one.
        let username = format!("@auth:{}", server_name);
        let client = Arc::new(ClientBuilder::new())
            .base_path(self.base_path.clone())
            .username(username)
            .build();

        match client {
            Ok(client) => {
                let mut client_containter = self.client_container.write();
                client_containter.client = Some(client);
                Ok(())
            }
            Err(error) => {
                Err(AuthenticationError::ClientBuilderFailed { message: error.to_string() })
            }
        }
    }

    /// Performs a password login using the current homeserver.
    pub fn login(
        &self,
        username: String,
        password: String,
    ) -> Result<Arc<Client>, AuthenticationError> {
        match self.client_container.read().client.as_ref() {
            Some(client) => client
                .login(username, password)
                .and_then(|_| Ok(client.clone()))
                .map_err(|error| AuthenticationError::from(error)),
            None => Err(AuthenticationError::ClientMissing),
        }
    }
}
