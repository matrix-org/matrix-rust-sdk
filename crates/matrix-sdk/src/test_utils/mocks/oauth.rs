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

//! Helpers to mock an OAuth 2.0 server for the purpose of integration tests.

use mas_oidc_client::types::{
    iana::{jose::JsonWebSignatureAlg, oauth::OAuthAuthorizationEndpointResponseType},
    oidc::{ProviderMetadata, SubjectType},
};
use serde_json::json;
use url::Url;
use wiremock::{
    matchers::{method, path_regex},
    Mock, MockServer, ResponseTemplate,
};

use super::{MatrixMock, MockEndpoint};

/// A [`wiremock`] [`MockServer`] along with useful methods to help mocking
/// OAuth 2.0 API endpoints easily.
///
/// It implements mock endpoints, limiting the shared code as much as possible,
/// so the mocks are still flexible to use as scoped/unscoped mounts, named, and
/// so on.
///
/// It works like this:
///
/// * start by saying which endpoint you'd like to mock, e.g.
///   [`Self::mock_server_metadata()`]. This returns a specialized
///   [`MockEndpoint`] data structure, with its own impl. For this example, it's
///   `MockEndpoint<ServerMetadataEndpoint>`.
/// * configure the response on the endpoint-specific mock data structure. For
///   instance, if you want the sending to result in a transient failure, call
///   [`MockEndpoint::error500`]; if you want it to succeed and return the
///   metadata, call [`MockEndpoint::ok()`]. It's still possible to call
///   [`MockEndpoint::respond_with()`], as we do with wiremock MockBuilder, for
///   maximum flexibility when the helpers aren't sufficient.
/// * once the endpoint's response is configured, for any mock builder, you get
///   a [`MatrixMock`]; this is a plain [`wiremock::Mock`] with the server
///   curried, so one doesn't have to pass it around when calling
///   [`MatrixMock::mount()`] or [`MatrixMock::mount_as_scoped()`]. As such, it
///   mostly defers its implementations to [`wiremock::Mock`] under the hood.
pub struct OauthMockServer<'a> {
    server: &'a MockServer,
}

impl<'a> OauthMockServer<'a> {
    pub(super) fn new(server: &'a MockServer) -> Self {
        Self { server }
    }
}

// Specific mount endpoints.
impl OauthMockServer<'_> {
    /// Creates a prebuilt mock for the Matrix endpoint used to query the
    /// authorization server's metadata.
    ///
    /// Contrary to all the other endpoints of [`OauthMockServer`], this is an
    /// endpoint from the Matrix API, but it is only used in the context of the
    /// OAuth 2.0 API, which is why it is mocked here rather than on
    /// [`MatrixMockServer`].
    ///
    /// [`MatrixMockServer`]: super::MatrixMockServer
    pub fn mock_server_metadata(&self) -> MockEndpoint<'_, ServerMetadataEndpoint> {
        let mock = Mock::given(method("GET"))
            .and(path_regex(r"^/_matrix/client/unstable/org.matrix.msc2965/auth_metadata"));
        MockEndpoint { mock, server: self.server, endpoint: ServerMetadataEndpoint }
    }

    /// Creates a prebuilt mock for the OAuth 2.0 endpoint used to register a
    /// new client.
    pub fn mock_registration(&self) -> MockEndpoint<'_, RegistrationEndpoint> {
        let mock = Mock::given(method("POST")).and(path_regex(r"^/oauth2/registration"));
        MockEndpoint { mock, server: self.server, endpoint: RegistrationEndpoint }
    }

    /// Creates a prebuilt mock for the OAuth 2.0 endpoint used to authorize a
    /// device.
    pub fn mock_device_authorization(&self) -> MockEndpoint<'_, DeviceAuthorizationEndpoint> {
        let mock = Mock::given(method("POST")).and(path_regex(r"^/oauth2/device"));
        MockEndpoint { mock, server: self.server, endpoint: DeviceAuthorizationEndpoint }
    }

    /// Creates a prebuilt mock for the OAuth 2.0 endpoint used to request an
    /// access token.
    pub fn mock_token(&self) -> MockEndpoint<'_, TokenEndpoint> {
        let mock = Mock::given(method("POST")).and(path_regex(r"^/oauth2/token"));
        MockEndpoint { mock, server: self.server, endpoint: TokenEndpoint }
    }

    /// Creates a prebuilt mock for the OAuth 2.0 endpoint used to revoke a
    /// token.
    pub fn mock_revocation(&self) -> MockEndpoint<'_, RevocationEndpoint> {
        let mock = Mock::given(method("POST")).and(path_regex(r"^/oauth2/revoke"));
        MockEndpoint { mock, server: self.server, endpoint: RevocationEndpoint }
    }
}

/// A prebuilt mock for a `GET /auth_metadata` request.
pub struct ServerMetadataEndpoint;

impl<'a> MockEndpoint<'a, ServerMetadataEndpoint> {
    /// Returns a successful metadata response with all the supported endpoints.
    pub fn ok(self) -> MatrixMock<'a> {
        let metadata = MockServerMetadataBuilder::new(&self.server.uri()).build();
        let mock = self.mock.respond_with(ResponseTemplate::new(200).set_body_json(metadata));

        MatrixMock { server: self.server, mock }
    }

    /// Returns a successful metadata response without the device authorization
    /// endpoint.
    pub fn ok_without_device_authorization(self) -> MatrixMock<'a> {
        let metadata = MockServerMetadataBuilder::new(&self.server.uri())
            .without_device_authorization()
            .build();
        let mock = self.mock.respond_with(ResponseTemplate::new(200).set_body_json(metadata));

        MatrixMock { server: self.server, mock }
    }

    /// Returns a successful metadata response without the registration
    /// endpoint.
    pub fn ok_without_registration(self) -> MatrixMock<'a> {
        let metadata =
            MockServerMetadataBuilder::new(&self.server.uri()).without_registration().build();
        let mock = self.mock.respond_with(ResponseTemplate::new(200).set_body_json(metadata));

        MatrixMock { server: self.server, mock }
    }
}

/// Helper struct to construct a `ProviderMetadata` for integration tests.
#[derive(Debug, Clone)]
pub struct MockServerMetadataBuilder {
    issuer: Url,
    with_device_authorization: bool,
    with_registration: bool,
}

impl MockServerMetadataBuilder {
    /// Construct a `MockServerMetadataBuilder` that will generate all the
    /// supported fields.
    pub fn new(issuer: &str) -> Self {
        let issuer = Url::parse(issuer).expect("We should be able to parse the issuer");

        Self { issuer, with_device_authorization: true, with_registration: true }
    }

    /// Don't generate the field for the device authorization endpoint.
    fn without_device_authorization(mut self) -> Self {
        self.with_device_authorization = false;
        self
    }

    /// Don't generate the field for the registration endpoint.
    fn without_registration(mut self) -> Self {
        self.with_registration = false;
        self
    }

    /// The authorization endpoint of this server.
    fn authorization_endpoint(&self) -> Url {
        self.issuer.join("oauth2/authorize").unwrap()
    }

    /// The token endpoint of this server.
    fn token_endpoint(&self) -> Url {
        self.issuer.join("oauth2/token").unwrap()
    }

    /// The JWKS URI of this server.
    fn jwks_uri(&self) -> Url {
        self.issuer.join("oauth2/keys.json").unwrap()
    }

    /// The registration endpoint of this server.
    fn registration_endpoint(&self) -> Url {
        self.issuer.join("oauth2/registration").unwrap()
    }

    /// The account management URI of this server.
    fn account_management_uri(&self) -> Url {
        self.issuer.join("account").unwrap()
    }

    /// The device authorization endpoint of this server.
    fn device_authorization_endpoint(&self) -> Url {
        self.issuer.join("oauth2/device").unwrap()
    }

    /// The revocation endpoint of this server.
    fn revocation_endpoint(&self) -> Url {
        self.issuer.join("oauth2/revoke").unwrap()
    }

    /// Build the server metadata.
    pub fn build(&self) -> ProviderMetadata {
        let device_authorization_endpoint =
            self.with_device_authorization.then(|| self.device_authorization_endpoint());
        let registration_endpoint = self.with_registration.then(|| self.registration_endpoint());

        ProviderMetadata {
            issuer: Some(self.issuer.to_string()),
            authorization_endpoint: Some(self.authorization_endpoint()),
            token_endpoint: Some(self.token_endpoint()),
            jwks_uri: Some(self.jwks_uri()),
            registration_endpoint,
            revocation_endpoint: Some(self.revocation_endpoint()),
            account_management_uri: Some(self.account_management_uri()),
            device_authorization_endpoint,
            response_types_supported: Some(vec![
                OAuthAuthorizationEndpointResponseType::Code.into()
            ]),
            subject_types_supported: Some(vec![SubjectType::Public]),
            id_token_signing_alg_values_supported: Some(vec![JsonWebSignatureAlg::Rs256]),
            ..Default::default()
        }
    }
}

/// A prebuilt mock for a `POST /oauth/registration` request.
pub struct RegistrationEndpoint;

impl<'a> MockEndpoint<'a, RegistrationEndpoint> {
    /// Returns a successful registration response.
    pub fn ok(self) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "client_id": "test_client_id",
            "client_id_issued_at": 1716375696,
        })));

        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for a `POST /oauth/device` request.
pub struct DeviceAuthorizationEndpoint;

impl<'a> MockEndpoint<'a, DeviceAuthorizationEndpoint> {
    /// Returns a successful device authorization response.
    pub fn ok(self) -> MatrixMock<'a> {
        let issuer_url = Url::parse(&self.server.uri())
            .expect("We should be able to parse the wiremock server URI");
        let verification_uri = issuer_url.join("link").unwrap();
        let mut verification_uri_complete = issuer_url.join("link").unwrap();
        verification_uri_complete.set_query(Some("code=N32YVC"));

        let mock = self.mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "device_code": "N8NAYD9fOhMulpm37mSthx0xSw2p7vdR",
            "expires_in": 1200,
            "interval": 5,
            "user_code": "N32YVC",
            "verification_uri": verification_uri,
            "verification_uri_complete": verification_uri_complete,
        })));

        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for a `POST /oauth/token` request.
pub struct TokenEndpoint;

impl<'a> MockEndpoint<'a, TokenEndpoint> {
    /// Returns a successful token response.
    pub fn ok(self) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "access_token": "1234",
            "expires_in": 300,
            "refresh_token": "ZYXWV",
            "token_type": "Bearer"
        })));

        MatrixMock { server: self.server, mock }
    }

    /// Returns an error response when the request was invalid.
    pub fn access_denied(self) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "access_denied",
        })));

        MatrixMock { server: self.server, mock }
    }

    /// Returns an error response when the token in the request has expired.
    pub fn expired_token(self) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "error": "expired_token",
        })));

        MatrixMock { server: self.server, mock }
    }
}

/// A prebuilt mock for a `POST /oauth/revoke` request.
pub struct RevocationEndpoint;

impl<'a> MockEndpoint<'a, RevocationEndpoint> {
    /// Returns a successful revocation response.
    pub fn ok(self) -> MatrixMock<'a> {
        let mock = self.mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({})));

        MatrixMock { server: self.server, mock }
    }
}
