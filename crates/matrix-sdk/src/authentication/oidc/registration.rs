// Copyright 2025 Kévin Commaille
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

//! Types and functions for OAuth 2.0 Dynamic Client Registration ([RFC 7591]).
//!
//! [RFC 7591]: http://tools.ietf.org/html/rfc7591

use mas_oidc_client::types::registration::VerifiedClientMetadata;
use oauth2::{AsyncHttpClient, ClientId, HttpClientError, RequestTokenError};
use ruma::SecondsSinceUnixEpoch;
use serde::Deserialize;
use url::Url;

use super::{
    error::OauthClientRegistrationError,
    http_client::{check_http_response_json_content_type, check_http_response_status_code},
    OauthHttpClient,
};

/// Register a client with an OAuth 2.0 authorization server.
///
/// # Arguments
///
/// * `http_service` - The service to use for making HTTP requests.
///
/// * `registration_endpoint` - The URL of the issuer's Registration endpoint.
///
/// * `client_metadata` - The metadata to register with the issuer.
///
/// * `software_statement` - A JWT that asserts metadata values about the client
///   software that should be signed.
///
/// # Errors
///
/// Returns an error if the request fails or the response is invalid.
#[tracing::instrument(skip_all, fields(registration_endpoint))]
pub(super) async fn register_client(
    http_client: &OauthHttpClient,
    registration_endpoint: &Url,
    client_metadata: &VerifiedClientMetadata,
) -> Result<ClientRegistrationResponse, OauthClientRegistrationError> {
    tracing::debug!("Registering client...");

    let body =
        serde_json::to_vec(client_metadata).map_err(OauthClientRegistrationError::IntoJson)?;
    let request = http::Request::post(registration_endpoint.as_str())
        .body(body)
        .map_err(|err| RequestTokenError::Request(HttpClientError::Http(err)))?;

    let response = http_client.call(request).await.map_err(RequestTokenError::Request)?;

    check_http_response_status_code(&response)?;
    check_http_response_json_content_type(&response)?;

    let response = serde_json::from_slice(&response.into_body())
        .map_err(OauthClientRegistrationError::FromJson)?;

    Ok(response)
}

/// A successful response to OAuth 2.0 Dynamic Client Registration ([RFC 7591]).
///
/// [RFC 7591]: http://tools.ietf.org/html/rfc7591
#[derive(Debug, Clone, Deserialize)]
pub struct ClientRegistrationResponse {
    /// The ID issued for the client by the authorization server.
    pub client_id: ClientId,

    /// The timestamp at which the client identifier was issued.
    pub client_id_issued_at: Option<SecondsSinceUnixEpoch>,
}
