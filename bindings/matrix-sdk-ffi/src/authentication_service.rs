use std::sync::Arc;

use super::{client::Client, client_builder::ClientBuilder};

pub struct AuthenticationService {
    base_path: String,
    client: Arc<Client>,
}

impl AuthenticationService {
    /// Creates a new service to authenticate with the specified server.
    pub fn new(base_path: String, server_name: String) -> anyhow::Result<Self> {
        // Construct a username as the builder currently requires one.
        let username = format!("@auth:{}", server_name);
        let client =
            Arc::new(ClientBuilder::new()).base_path(base_path.clone()).username(username).build();

        client.and_then(|client| Ok(AuthenticationService { base_path, client }))
    }

    /// The currently configured homeserver.
    pub fn homeserver(&self) -> String {
        self.client.homeserver()
    }

    /// The authentication server to complete an OIDC login on the current
    /// homeserver.
    pub fn authentication_server(&self) -> Option<String> {
        self.client.authentication_server()
    }

    /// Whether the current homeserver supports the password login flow.
    pub fn supports_password_login(&self) -> anyhow::Result<bool> {
        self.client.supports_password_login()
    }

    /// Updates the server to authenticate with the specified homeserver.
    pub fn update(&self, server_name: String) -> anyhow::Result<()> {
        // Construct a username as the builder currently requires one.
        let username = format!("@auth:{}", server_name);
        let client = Arc::new(ClientBuilder::new())
            .base_path(self.base_path.clone())
            .username(username)
            .build();

        match client {
            Ok(client) => {
                self.client = client;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    /// Performs a password login using the current homeserver.
    pub fn login(&self, username: String, password: String) -> anyhow::Result<Arc<Client>> {
        let result = self.client.login(username, password);

        match result {
            Ok(_) => Ok(self.client.clone()),
            Err(e) => Err(e),
        }
    }
}
