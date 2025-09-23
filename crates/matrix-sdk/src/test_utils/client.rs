// Copyright 2024 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Augmented [`ClientBuilder`] that can set up an already logged-in user.

use matrix_sdk_base::{SessionMeta, store::RoomLoadSettings};
use ruma::{OwnedDeviceId, OwnedUserId, api::MatrixVersion, owned_device_id, owned_user_id};

use crate::{
    Client, ClientBuilder, SessionTokens, authentication::matrix::MatrixSession,
    config::RequestConfig,
};

/// An augmented [`ClientBuilder`] that also allows for handling session login.
#[allow(missing_debug_implementations)]
pub struct MockClientBuilder {
    builder: ClientBuilder,
    auth_state: AuthState,
    server_versions: ServerVersions,
}

impl MockClientBuilder {
    /// Create a new [`MockClientBuilder`] connected to the given homeserver,
    /// using Matrix V1.12, and which will not attempt any network retry (by
    /// default).
    ///
    /// If no homeserver is provided, `http://localhost` is used as a homeserver.
    pub fn new(homeserver: Option<&str>) -> Self {
        let homeserver = homeserver.unwrap_or("http://localhost");

        let default_builder = Client::builder()
            .homeserver_url(homeserver)
            .request_config(RequestConfig::new().disable_retry());

        Self {
            builder: default_builder,
            auth_state: AuthState::LoggedInWithMatrixAuth {
                token: None,
                user_id: None,
                device_id: None,
            },
            server_versions: ServerVersions::Default,
        }
    }

    /// Don't use an initial, cached server versions list in the client.
    pub fn no_server_versions(mut self) -> Self {
        self.server_versions = ServerVersions::None;
        self
    }

    /// Set the cached server versions in the client.
    pub fn server_versions(mut self, versions: Vec<MatrixVersion>) -> Self {
        self.server_versions = ServerVersions::Custom(versions);
        self
    }

    /// Doesn't log-in a user.
    ///
    /// Authenticated requests will fail if this is called.
    pub fn unlogged(mut self) -> Self {
        self.auth_state = AuthState::None;
        self
    }

    /// The client is registered with the OAuth 2.0 API.
    pub fn registered_with_oauth(mut self) -> Self {
        self.auth_state = AuthState::RegisteredWithOAuth;
        self
    }

    /// The user is already logged in with the OAuth 2.0 API.
    pub fn logged_in_with_oauth(mut self) -> Self {
        self.auth_state = AuthState::LoggedInWithOAuth;
        self
    }

    /// The user is already logged in with the Matrix Auth.
    pub fn logged_in_with_token(
        mut self,
        token: String,
        user_id: OwnedUserId,
        device_id: OwnedDeviceId,
    ) -> Self {
        self.auth_state = AuthState::LoggedInWithMatrixAuth {
            token: Some(token),
            user_id: Some(user_id),
            device_id: Some(device_id),
        };
        self
    }

    /// Apply changes to the underlying [`ClientBuilder`].
    ///
    /// ```
    /// # tokio_test::block_on(async {
    /// use matrix_sdk::test_utils::client::MockClientBuilder;
    ///
    /// MockClientBuilder::new(None)
    ///     .on_builder(|builder| {
    ///         // Here it's possible to modify the underlying `ClientBuilder`.
    ///         builder
    ///             .handle_refresh_tokens()
    ///             .cross_process_store_locks_holder_name("hodor".to_owned())
    ///     })
    ///     .build()
    ///     .await;
    /// # anyhow::Ok(()) });
    /// ```
    pub fn on_builder<F: FnOnce(ClientBuilder) -> ClientBuilder>(mut self, f: F) -> Self {
        self.builder = f(self.builder);
        self
    }

    /// Finish building the client into the final [`Client`] instance.
    pub async fn build(self) -> Client {
        let mut builder = self.builder;

        if let Some(versions) = self.server_versions.into_vec() {
            builder = builder.server_versions(versions);
        }

        let client = builder.build().await.expect("building client failed");

        self.auth_state.maybe_restore_client(&client).await;

        client
    }
}

/// The possible authentication states of a [`Client`] built with
/// [`MockClientBuilder`].
enum AuthState {
    /// The client is not logged in.
    None,
    /// The client is logged in with the native Matrix API.
    LoggedInWithMatrixAuth {
        token: Option<String>,
        user_id: Option<OwnedUserId>,
        device_id: Option<OwnedDeviceId>,
    },
    /// The client is registered with the OAuth 2.0 API.
    RegisteredWithOAuth,
    /// The client is logged in with the OAuth 2.0 API.
    LoggedInWithOAuth,
}

impl AuthState {
    /// Restore the given [`Client`] according to this [`AuthState`], if
    /// necessary.
    async fn maybe_restore_client(self, client: &Client) {
        match self {
            AuthState::None => {}
            AuthState::LoggedInWithMatrixAuth { token, user_id, device_id } => {
                client
                    .matrix_auth()
                    .restore_session(
                        MatrixSession {
                            meta: SessionMeta {
                                user_id: user_id.unwrap_or(owned_user_id!("@example:localhost")),
                                device_id: device_id.unwrap_or(owned_device_id!("DEVICEID")),
                            },
                            tokens: SessionTokens {
                                access_token: token.unwrap_or("1234".to_owned()).to_owned(),
                                refresh_token: None,
                            },
                        },
                        RoomLoadSettings::default(),
                    )
                    .await
                    .unwrap();
            }
            AuthState::RegisteredWithOAuth => {
                client.oauth().restore_registered_client(oauth::mock_client_id());
            }
            AuthState::LoggedInWithOAuth => {
                client
                    .oauth()
                    .restore_session(
                        oauth::mock_session(mock_session_tokens_with_refresh()),
                        RoomLoadSettings::default(),
                    )
                    .await
                    .unwrap();
            }
        }
    }
}

/// The server versions cached during client creation.
enum ServerVersions {
    /// Cache the default server version.
    Default,
    /// Don't cache any server versions.
    None,
    /// Cache the given server versions.
    Custom(Vec<MatrixVersion>),
}

impl ServerVersions {
    /// Convert these `ServerVersions` to a list of matrix versions.
    ///
    /// Returns `None` if no server versions should be cached in the client.
    fn into_vec(self) -> Option<Vec<MatrixVersion>> {
        match self {
            Self::Default => Some(vec![MatrixVersion::V1_12]),
            Self::None => None,
            Self::Custom(versions) => Some(versions),
        }
    }
}

/// A [`SessionMeta`], for unit or integration tests.
pub fn mock_session_meta() -> SessionMeta {
    SessionMeta {
        user_id: owned_user_id!("@example:localhost"),
        device_id: owned_device_id!("DEVICEID"),
    }
}

/// A [`SessionTokens`] including only an access token, for unit or integration
/// tests.
pub fn mock_session_tokens() -> SessionTokens {
    SessionTokens { access_token: "1234".to_owned(), refresh_token: None }
}

/// A [`SessionTokens`] including an access token and a refresh token, for unit
/// or integration tests.
pub fn mock_session_tokens_with_refresh() -> SessionTokens {
    SessionTokens { access_token: "1234".to_owned(), refresh_token: Some("ZYXWV".to_owned()) }
}

/// Different session tokens than the ones returned by
/// [`mock_session_tokens_with_refresh()`].
pub fn mock_prev_session_tokens_with_refresh() -> SessionTokens {
    SessionTokens {
        access_token: "prev-access-token".to_owned(),
        refresh_token: Some("prev-refresh-token".to_owned()),
    }
}

/// A [`MatrixSession`], for unit or integration tests.
pub fn mock_matrix_session() -> MatrixSession {
    MatrixSession { meta: mock_session_meta(), tokens: mock_session_tokens() }
}

/// Mock client data for the OAuth 2.0 API.
pub mod oauth {
    use ruma::serde::Raw;
    use url::Url;

    use crate::{
        SessionTokens,
        authentication::oauth::{
            ClientId, OAuthSession, UserSession,
            registration::{ApplicationType, ClientMetadata, Localized, OAuthGrantType},
        },
    };

    /// An OAuth 2.0 `ClientId`, for unit or integration tests.
    pub fn mock_client_id() -> ClientId {
        ClientId::new("test_client_id".to_owned())
    }

    /// A redirect URI, for unit or integration tests.
    pub fn mock_redirect_uri() -> Url {
        Url::parse("http://127.0.0.1/").expect("redirect URI should be valid")
    }

    /// `VerifiedClientMetadata` that should be valid in most cases, for unit or
    /// integration tests.
    pub fn mock_client_metadata() -> Raw<ClientMetadata> {
        let client_uri = Url::parse("https://github.com/matrix-org/matrix-rust-sdk")
            .expect("client URI should be valid");

        let mut metadata = ClientMetadata::new(
            ApplicationType::Native,
            vec![
                OAuthGrantType::AuthorizationCode { redirect_uris: vec![mock_redirect_uri()] },
                OAuthGrantType::DeviceCode,
            ],
            Localized::new(client_uri, None),
        );
        metadata.client_name = Some(Localized::new("matrix-rust-sdk-test".to_owned(), None));

        Raw::new(&metadata).expect("client metadata should serialize successfully")
    }

    /// An [`OAuthSession`] to restore, for unit or integration tests.
    pub fn mock_session(tokens: SessionTokens) -> OAuthSession {
        OAuthSession {
            client_id: mock_client_id(),
            user: UserSession { meta: super::mock_session_meta(), tokens },
        }
    }
}
