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

//! Requests for [OpenID Connect Provider Discovery].
//!
//! [OpenID Connect Provider Discovery]: https://openid.net/specs/openid-connect-discovery-1_0.html

use oauth2::{AsyncHttpClient, HttpClientError, RequestTokenError};
use ruma::{
    api::client::discovery::get_authorization_server_metadata::msc2965::AuthorizationServerMetadata,
    serde::Raw,
};
use url::Url;

use super::{
    error::OAuthDiscoveryError,
    http_client::{check_http_response_json_content_type, check_http_response_status_code},
    OAuthHttpClient,
};

/// Fetch the OpenID Connect provider metadata.
pub(super) async fn discover(
    http_client: &OAuthHttpClient,
    issuer: &str,
) -> Result<Raw<AuthorizationServerMetadata>, OAuthDiscoveryError> {
    tracing::debug!("Fetching OpenID Connect provider metadata...");

    let mut url = Url::parse(issuer)?;

    // If the path doesn't end with a slash, the last segment is removed when
    // using `join`.
    if !url.path().ends_with('/') {
        let mut path = url.path().to_owned();
        path.push('/');
        url.set_path(&path);
    }

    let config_url = url.join(".well-known/openid-configuration")?;

    let request = http::Request::get(config_url.as_str())
        .body(Vec::new())
        .map_err(|err| RequestTokenError::Request(HttpClientError::Http(err)))?;
    let response = http_client.call(request).await.map_err(RequestTokenError::Request)?;

    check_http_response_status_code(&response)?;
    check_http_response_json_content_type(&response)?;

    let metadata = serde_json::from_slice(&response.into_body())?;

    tracing::debug!(?metadata);

    Ok(metadata)
}
