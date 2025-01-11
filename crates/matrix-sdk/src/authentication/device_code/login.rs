// Copyright 2025 The Matrix.org Foundation C.I.C.
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

use mas_oidc_client::types::{registration::VerifiedClientMetadata, scope::ScopeToken};
use matrix_sdk_base::SessionMeta;
use openidconnect::core::CoreDeviceAuthorizationResponse;
use ruma::OwnedDeviceId;
use tracing::trace;

use super::{DeviceCodeFinishLoginError, DeviceCodeLoginError};
#[cfg(doc)]
use crate::oidc::Oidc;
use crate::{
    authentication::common_oidc::{oidc_client::OidcClient, DeviceAuhorizationOidcError},
    oidc::registrations::OidcRegistrations,
    Client,
};

#[derive(Debug)]
struct LoginData {
    account: String,
    pickle_key: Box<[u8; 32]>,
    oidc_client: OidcClient,
    auth_grant_response: CoreDeviceAuthorizationResponse,
}

/// State for the [`Oidc::login_with_device_code()`] method.
#[derive(Debug)]
pub struct LoginWithDeviceCode<'a> {
    client: &'a Client,
    client_metadata: VerifiedClientMetadata,
    registrations: OidcRegistrations,
    login_data: Option<LoginData>,
}

impl<'a> LoginWithDeviceCode<'a> {
    pub(crate) fn new(
        client: &'a Client,
        client_metadata: VerifiedClientMetadata,
        registrations: OidcRegistrations,
    ) -> LoginWithDeviceCode<'a> {
        LoginWithDeviceCode { client, client_metadata, registrations, login_data: None }
    }

    async fn register_client(&self) -> Result<OidcClient, DeviceAuhorizationOidcError> {
        let oidc = self.client.oidc();

        // Let's figure out the OIDC issuer, this fetches the info from the homeserver.
        let issuer = self
            .client
            .oidc()
            .fetch_authentication_issuer()
            .await
            .map_err(DeviceAuhorizationOidcError::AuthenticationIssuer)?;

        // Configure and register the client
        oidc.configure(issuer.clone(), self.client_metadata.clone(), self.registrations.clone())
            .await?;

        // client_credentials has to be Some because we just configured the client
        let credentials = oidc.client_credentials().unwrap();

        // configure sets the secret to None using
        let client_secret = None;

        OidcClient::new(
            credentials.client_id().to_owned(),
            issuer,
            self.client.inner.http_client.clone(),
            client_secret,
        )
        .await
    }

    fn pickle_key(&self) -> Result<Box<[u8; 32]>, rand::Error> {
        let mut rng = rand::thread_rng();

        let mut key = Box::new([0u8; 32]);
        rand::Fill::try_fill(key.as_mut_slice(), &mut rng)?;

        Ok(key)
    }

    /// Register client and return device code aswell as the url to use for
    /// authentication
    pub async fn device_code_for_login(
        &mut self,
        custom_scopes: Option<Vec<ScopeToken>>,
    ) -> Result<CoreDeviceAuthorizationResponse, DeviceCodeLoginError> {
        // Register the client with the OIDC provider.
        trace!("Registering the client with the OIDC provider.");
        let oidc_client = self.register_client().await?;

        // We want to use the Curve25519 public key for the device ID, so let's generate
        // a new vodozemac `Account` now.
        let account = vodozemac::olm::Account::new();
        let public_key = account.identity_keys().curve25519;
        let device_id = public_key;

        // Let's tell the OIDC provider that we want to log in using the device
        // authorization grant described in [RFC8628](https://datatracker.ietf.org/doc/html/rfc8628).
        trace!("Requesting device authorization.");
        let auth_grant_response =
            oidc_client.request_device_authorization(device_id, custom_scopes).await?;

        let pickle_key = self.pickle_key()?;

        self.login_data = Some(LoginData {
            account: account.to_libolm_pickle(pickle_key.as_ref())?,
            pickle_key,
            oidc_client,
            auth_grant_response: auth_grant_response.clone(),
        });

        Ok(auth_grant_response)
    }

    /// Wait for authentication and finish login by setting the session token on
    /// success
    pub async fn wait_finish_login(self) -> Result<(), DeviceCodeFinishLoginError> {
        let login_data = self.login_data.ok_or(DeviceCodeFinishLoginError::LoginFailure {})?;
        let account = vodozemac::olm::Account::from_libolm_pickle(
            &login_data.account,
            login_data.pickle_key.as_ref(),
        )?;
        let public_key = account.identity_keys().curve25519;
        let device_id = public_key;

        // Let's now wait for the access token to be provided to use by the OIDC
        // provider.
        trace!("Waiting for the OIDC provider to give us the access token.");
        let session_tokens =
            login_data.oidc_client.wait_for_tokens(&login_data.auth_grant_response).await?;

        self.client.oidc().set_session_tokens(session_tokens);

        // We only received an access token from the OIDC provider, we have no clue who
        // we are, so we need to figure out our user ID now.
        // TODO: This snippet is almost the same as the Oidc::finish_login_method(), why
        // is that method even a public method and not called as part of the set session
        // tokens method.
        trace!("Discovering our own user id.");
        let whoami_response =
            self.client.whoami().await.map_err(DeviceCodeFinishLoginError::UserIdDiscovery)?;
        self.client
            .set_session_meta(
                SessionMeta {
                    user_id: whoami_response.user_id,
                    device_id: OwnedDeviceId::from(device_id.to_base64()),
                },
                Some(account),
            )
            .await
            .map_err(DeviceCodeFinishLoginError::SessionTokens)?;

        self.client.oidc().enable_cross_process_lock().await?;

        #[cfg(feature = "e2e-encryption")]
        self.client.encryption().spawn_initialization_task(None);

        Ok(())
    }
}

#[cfg(test)]
mod test {
    use std::collections::HashMap;

    use assert_matches2::assert_let;
    use mas_oidc_client::types::{
        iana::oauth::OAuthClientAuthenticationMethod,
        oidc::ApplicationType,
        registration::{ClientMetadata, Localized},
        requests::GrantType,
    };
    use matrix_sdk_test::{async_test, test_json};
    use openidconnect::DeviceCodeErrorResponseType;
    use serde_json::{json, Value};
    use tempfile::tempdir;
    use url::Url;
    use wiremock::{
        matchers::{method, path},
        Mock, MockServer, ResponseTemplate,
    };

    use super::*;
    use crate::config::RequestConfig;

    fn client_metadata() -> VerifiedClientMetadata {
        let client_uri = Url::parse("https://github.com/matrix-org/matrix-rust-sdk")
            .expect("Couldn't parse client URI");

        ClientMetadata {
            application_type: Some(ApplicationType::Native),
            redirect_uris: None,
            grant_types: Some(vec![GrantType::DeviceCode]),
            token_endpoint_auth_method: Some(OAuthClientAuthenticationMethod::None),
            client_name: Some(Localized::new("test-matrix-rust-sdk-qrlogin".to_owned(), [])),
            contacts: Some(vec!["root@127.0.0.1".to_owned()]),
            client_uri: Some(Localized::new(client_uri.clone(), [])),
            policy_uri: Some(Localized::new(client_uri.clone(), [])),
            tos_uri: Some(Localized::new(client_uri, [])),
            ..Default::default()
        }
        .validate()
        .unwrap()
    }

    fn open_id_configuration(server: &MockServer) -> Value {
        let issuer_url =
            Url::parse(&server.uri()).expect("We should be able to parse the example homeserver");
        let account_management_uri = issuer_url.join("account").unwrap();
        let authorization_endpoint = issuer_url.join("authorize").unwrap();
        let device_authorization_endpoint = issuer_url.join("oauth2/device").unwrap();
        let jwks_url = issuer_url.join("oauth2/keys.json").unwrap();
        let registration_endpoint = issuer_url.join("oauth2/registration").unwrap();
        let token_endpoint = issuer_url.join("oauth2/token").unwrap();

        json!({
            "account_management_actions_supported": [
                "org.matrix.profile",
                "org.matrix.sessions_list",
                "org.matrix.session_view",
                "org.matrix.session_end",
                "org.matrix.cross_signing_reset"
            ],
            "account_management_uri": account_management_uri,
            "authorization_endpoint": authorization_endpoint,
            "claim_types_supported": [
                "normal"
            ],
            "claims_parameter_supported": false,
            "claims_supported": [
                "iss",
                "sub",
                "aud",
                "iat",
                "exp",
                "nonce",
                "auth_time",
                "at_hash",
                "c_hash"
            ],
            "code_challenge_methods_supported": [
                "plain",
                "S256"
            ],
            "device_authorization_endpoint": device_authorization_endpoint,
            "display_values_supported": [
                "page"
            ],
            "grant_types_supported": [
                "authorization_code",
                "refresh_token",
                "client_credentials",
                "urn:ietf:params:oauth:grant-type:device_code"
            ],
            "id_token_signing_alg_values_supported": [
                "RS256",
                "RS384",
                "RS512",
                "ES256",
                "ES384",
                "PS256",
                "PS384",
                "PS512",
                "ES256K"
            ],
            "issuer": issuer_url.to_string().trim_end_matches("/"),
            "jwks_uri": jwks_url,
            "prompt_values_supported": [
                "none",
                "login",
                "create"
            ],
            "registration_endpoint": registration_endpoint,
            "request_parameter_supported": false,
            "request_uri_parameter_supported": false,
            "response_modes_supported": [
                "form_post",
                "query",
                "fragment"
            ],
            "response_types_supported": [
                "code",
                "id_token",
                "code id_token"
            ],
            "scopes_supported": [
                "openid",
                "email"
            ],
            "subject_types_supported": [
                "public"
            ],
            "token_endpoint": token_endpoint,
            "token_endpoint_auth_methods_supported": [
                "client_secret_basic",
                "client_secret_post",
                "client_secret_jwt",
                "private_key_jwt",
                "none"
            ],
        })
    }

    fn keys_json() -> Value {
        json!({
            "keys": [
                {
                    "e": "AQAB",
                    "kid": "hxdHWoF9mn",
                    "kty": "RSA",
                    "n": "u4op7tDV41j-f_-DqsqjjCObiySB0q2CGS1JVjJXbV5jctHP6Wp_oMb2aIImMdHDcnTvxaID\
                        WwuKA8o-0SBfkHFifMHHRvePz_l7NxxUMyGX8Bfu_EVkECe50BXpFydcEEl1eIIsPW-F0WJKFYR\
                        5cscmBgRX3zv_w7WFbaOLh711S9DNu21epdSvFSrKRe9oG_FbeOFfDl-YU7BLGFvEozg9Z3hKF\
                        SomOlz-t3ABvRUweGuLCpHFKsI6yhGCoqPyS7o5gpfenizdfHLqq-l7kgyr7lSbW_mTSyYutby\
                        DpQ_HM98Lt-4a9zwlGfiqPS3svkH6KSd1mBcayCI0Cm9FuQ",
                    "use": "sig"
                },
                {
                    "crv": "P-256",
                    "kid": "IRbxoGCBjs",
                    "kty": "EC",
                    "use": "sig",
                    "x": "1AYfsklcgvscvJiNZ1Og7vQePzIBf-flJKlANWJ7D4g",
                    "y": "L4b-jMZVZlnLhXCpV0EOc6zdEz1e6ONgKQZVE3jOBhY"
                },
                {
                    "crv": "P-384",
                    "kid": "FjEZp4JjqW",
                    "kty": "EC",
                    "use": "sig",
                    "x": "bZP2bPUEQGeGaDICINswZSTCHdoVmDD3LIJE1Szxw27ruCJBW-sy_lY3dhA2FjWm",
                    "y": "3HMgAu___-4JG9IXZFXwzr5nU_GUPvmWJHqgS7vzK1S91s0v1GXiqQMHwYA0keYG"
                },
                {
                    "crv": "secp256k1",
                    "kid": "7ohCuHzgqB",
                    "kty": "EC",
                    "use": "sig",
                    "x": "80KXhBY8JBy8qO9-wMBaGtgOgtagowHJ4dDGfVr4eVw",
                    "y": "0ALeT-J40AjdIS4S1YDgMrPkyE_rnw9wVm7Dvz_9Np4"
                }
            ]
        })
    }

    fn device_code(server: &MockServer) -> Value {
        let issuer_url =
            Url::parse(&server.uri()).expect("We should be able to parse the example homeserver");
        let verification_uri = issuer_url.join("link").unwrap();
        let mut verification_uri_complete = issuer_url.join("link").unwrap();
        verification_uri_complete.set_query(Some("code=N32YVC"));

        json!({
            "device_code": "N8NAYD9fOhMulpm37mSthx0xSw2p7vdR",
            "expires_in": 1200,
            "interval": 5,
            "user_code": "N32YVC",
            "verification_uri": verification_uri,
            "verification_uri_complete": verification_uri_complete,
        })
    }

    fn token() -> Value {
        json!({
            "access_token": "mat_z65RpDAbvR5aTr7MzD0aPw40xFbwch_09xTgn",
            "expires_in": 300,
            "id_token": "eyJhbGciOiJSUzI1NiIsImtpZCI6Imh4ZEhXb0Y5bW4ifQ.eyJhdWQiOiIwMUhZRlpEQ1\
                BTV1dCREVWWkQyRlRBUVlFViIsInN1YiI6IjAxSFYxNzNTSjQxUDBGMFgxQ0FRU1lBVENQIiwiaWF0IjoxN\
                zE2Mzc1NzIwLCJpc3MiOiJodHRwczovL2F1dGgtb2lkYy5sYWIuZWxlbWVudC5kZXYvIiwiZXhwIjoxNzE2\
                Mzc5MzIwLCJhdF9oYXNoIjoieGZIS21qQW83cEVCRmUwTkM5ODJEQSJ9.HQs7Si5gU_5tm2hYaCa3jg0kPO\
                MXGNdpV88MWzG6N9x3yXK0ZGgn58i38HiQTbiyPuhw8OH6baMSjbcVP-KXSDpsSPZbkmp7Ozb50dC0eIebD\
                aVK0EyZ35KQRVc5BFPQBPbq0r_TrcUgjoLRKpoexvdmjfEb2dE-kKse25jfs-bTHKP6jeAyFgR9Emn0RfVx\
                32He32-bRP1NfkBnPNnJse32tF1o8gs7zG-cm7kSUx1wiQbvfSGfETx_mJ-aFGABbVGKQlTrCe32HUTvNbp\
                tT2WXa1t7d3eDuEV_6hZS9LFRdIXhgEcGIZMz_ss3WQsSOKN8Yq2NC8_bNxRAQ-1J3A",
            "refresh_token": "mar_CHFh124AMHsdishuHgLSx1svdKMVQA_080gj2",
            "scope": "openid \
                urn:matrix:org.matrix.msc2967.client:api:* \
                urn:matrix:org.matrix.msc2967.client:device:\
                lKa+6As0PSFtqOMKALottO6hlt3gCpZtaVfHanSUnEE",
            "token_type": "Bearer"
        })
    }

    async fn mock_oidc_provider(server: &MockServer, token_response: ResponseTemplate) {
        Mock::given(method("GET"))
            .and(path("/_matrix/client/unstable/org.matrix.msc2965/auth_issuer"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issuer": server.uri(),

            })))
            .expect(1)
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path("/.well-known/openid-configuration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(open_id_configuration(server)))
            .expect(1..)
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path("/oauth2/registration"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "client_id": "01HYFZDCPSWWBDEVZD2FTAQYEV",
                "client_id_issued_at": 1716375696
            })))
            .expect(1)
            .mount(server)
            .await;

        Mock::given(method("GET"))
            .and(path("/oauth2/keys.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(keys_json()))
            .expect(1)
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path("/oauth2/device"))
            .respond_with(ResponseTemplate::new(200).set_body_json(device_code(server)))
            .expect(1)
            .mount(server)
            .await;

        Mock::given(method("POST"))
            .and(path("/oauth2/token"))
            .respond_with(token_response)
            .mount(server)
            .await;
    }

    #[async_test]
    async fn test_device_code_login() {
        let server = MockServer::start().await;

        mock_oidc_provider(&server, ResponseTemplate::new(200).set_body_json(token())).await;

        Mock::given(method("GET"))
            .and(path("/_matrix/client/r0/account/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::WHOAMI))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::VERSIONS))
            .mount(&server)
            .await;

        let client = Client::builder()
            .server_name_or_homeserver_url(server.uri())
            .request_config(RequestConfig::new().disable_retry())
            .build()
            .await
            .expect("We should be able to build the Client object from the server uri");

        let dir = tempdir().unwrap();
        let registrations_file = dir.path().join("oidc").join("registrations.json");

        let static_registrations = HashMap::new();

        let registrations =
            OidcRegistrations::new(&registrations_file, client_metadata(), static_registrations)
                .unwrap();

        let oidc = client.oidc();
        let mut login_device_code = oidc.login_with_device_code(client_metadata(), registrations);

        login_device_code
            .device_code_for_login(None)
            .await
            .expect("Client should be able to register and get a device code");

        login_device_code.wait_finish_login().await.expect("Client should finish the login");
    }

    async fn test_failure(
        token_response: ResponseTemplate,
    ) -> Result<(), DeviceCodeFinishLoginError> {
        let server = MockServer::start().await;

        mock_oidc_provider(&server, token_response).await;

        Mock::given(method("GET"))
            .and(path("/_matrix/client/r0/account/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::WHOAMI))
            .mount(&server)
            .await;

        Mock::given(method("GET"))
            .and(path("/_matrix/client/versions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&*test_json::VERSIONS))
            .mount(&server)
            .await;

        let client = Client::builder()
            .server_name_or_homeserver_url(server.uri())
            .request_config(RequestConfig::new().disable_retry())
            .build()
            .await
            .expect("We should be able to build the Client object from the server uri");

        let dir = tempdir().unwrap();
        let registrations_file = dir.path().join("oidc").join("registrations.json");

        let static_registrations = HashMap::new();

        let registrations =
            OidcRegistrations::new(&registrations_file, client_metadata(), static_registrations)
                .unwrap();

        let oidc = client.oidc();
        let mut login_device_code = oidc.login_with_device_code(client_metadata(), registrations);

        login_device_code
            .device_code_for_login(None)
            .await
            .expect("Client should be able to register and get a device code");

        login_device_code.wait_finish_login().await
    }

    #[async_test]
    async fn test_device_code_login_refused_access_token() {
        let result = test_failure(ResponseTemplate::new(400).set_body_json(json!({
            "error": "access_denied",
        })))
        .await;

        assert_let!(Err(DeviceCodeFinishLoginError::Oidc(e)) = result);
        assert_eq!(
            e.as_request_token_error(),
            Some(&DeviceCodeErrorResponseType::AccessDenied),
            "The server should have told us that access has been denied."
        );
    }

    #[async_test]
    async fn test_device_code_login_expired_token() {
        let result = test_failure(ResponseTemplate::new(400).set_body_json(json!({
            "error": "expired_token",
        })))
        .await;

        assert_let!(Err(DeviceCodeFinishLoginError::Oidc(e)) = result);
        assert_eq!(
            e.as_request_token_error(),
            Some(&DeviceCodeErrorResponseType::ExpiredToken),
            "The server should have told us that access has been denied."
        );
    }
}
