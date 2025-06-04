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

use matrix_sdk_base::{
    store::{RoomLoadSettings, StoreConfig},
    SessionMeta,
};
use ruma::{api::MatrixVersion, owned_device_id, owned_user_id, OwnedDeviceId, OwnedUserId};

use crate::{
    authentication::matrix::MatrixSession, config::RequestConfig, Client, ClientBuilder,
    SessionTokens,
};

/// An augmented [`ClientBuilder`] that also allows for handling session login.
#[allow(missing_debug_implementations)]
pub struct MockClientBuilder {
    builder: ClientBuilder,
    auth_state: AuthState,
}

impl MockClientBuilder {
    /// Create a new [`MockClientBuilder`] connected to the given homeserver,
    /// using Matrix V1.12, and which will not attempt any network retry (by
    /// default).
    pub(crate) fn new(homeserver: String) -> Self {
        let default_builder = Client::builder()
            .homeserver_url(&homeserver)
            .server_versions([MatrixVersion::V1_12])
            .request_config(RequestConfig::new().disable_retry());

        Self {
            builder: default_builder,
            auth_state: AuthState::LoggedInWithMatrixAuth {
                token: None,
                user_id: None,
                device_id: None,
            },
        }
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

    /// Provides another [`StoreConfig`] for the underlying [`ClientBuilder`].
    pub fn store_config(mut self, store_config: StoreConfig) -> Self {
        self.builder = self.builder.store_config(store_config);
        self
    }

    /// Use an SQLite store at the given path for the underlying
    /// [`ClientBuilder`].
    #[cfg(feature = "sqlite")]
    pub fn sqlite_store(mut self, path: impl AsRef<std::path::Path>) -> Self {
        self.builder = self.builder.sqlite_store(path, None);
        self
    }

    /// Handle refreshing access tokens automatically.
    pub fn handle_refresh_tokens(mut self) -> Self {
        self.builder = self.builder.handle_refresh_tokens();
        self
    }

    /// Finish building the client into the final [`Client`] instance.
    pub async fn build(self) -> Client {
        let client = self.builder.build().await.expect("building client failed");
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
        authentication::oauth::{
            registration::{ApplicationType, ClientMetadata, Localized, OAuthGrantType},
            ClientId, OAuthSession, UserSession,
        },
        SessionTokens,
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
