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

use std::collections::{BTreeSet, HashMap};

pub use language_tags;
use language_tags::LanguageTag;
use matrix_sdk_base::deserialized_responses::PrivOwnedStr;
use oauth2::{AsyncHttpClient, ClientId, HttpClientError, RequestTokenError};
use ruma::{
    SecondsSinceUnixEpoch,
    api::client::discovery::get_authorization_server_metadata::v1::{GrantType, ResponseType},
    serde::{Raw, StringEnum},
};
use serde::{Deserialize, Serialize, ser::SerializeMap};
use url::Url;

use super::{
    OAuthHttpClient,
    error::OAuthClientRegistrationError,
    http_client::{check_http_response_json_content_type, check_http_response_status_code},
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
    http_client: &OAuthHttpClient,
    registration_endpoint: &Url,
    client_metadata: &Raw<ClientMetadata>,
) -> Result<ClientRegistrationResponse, OAuthClientRegistrationError> {
    tracing::debug!("Registering client...");

    let body =
        serde_json::to_vec(client_metadata).map_err(OAuthClientRegistrationError::IntoJson)?;
    let request = http::Request::post(registration_endpoint.as_str())
        .header(http::header::CONTENT_TYPE, mime::APPLICATION_JSON.to_string())
        .body(body)
        .map_err(|err| RequestTokenError::Request(HttpClientError::Http(err)))?;

    let response = http_client.call(request).await.map_err(RequestTokenError::Request)?;

    check_http_response_status_code(&response)?;
    check_http_response_json_content_type(&response)?;

    let response = serde_json::from_slice(&response.into_body())
        .map_err(OAuthClientRegistrationError::FromJson)?;

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

/// The metadata necessary to register a client with an OAuth 2.0 authorization
/// server.
///
/// This is a simplified type, designed to avoid inconsistencies between fields.
/// Only the fields defined in [MSC2966] can be set with this type, and only if
/// different values are supported by this API. To set other fields, use your
/// own type or construct directly the JSON representation.
///
/// The original format is defined in [RFC 7591].
///
/// [MSC2966]: https://github.com/matrix-org/matrix-spec-proposals/pull/2966
/// [RFC 7591]: https://datatracker.ietf.org/doc/html/rfc7591
#[derive(Debug, Clone, Serialize)]
#[serde(into = "ClientMetadataSerializeHelper")]
pub struct ClientMetadata {
    /// The type of the application.
    pub application_type: ApplicationType,

    /// The grant types that the client will use at the token endpoint.
    ///
    /// This should match the login methods that the client can use.
    pub grant_types: Vec<OAuthGrantType>,

    /// URL of the home page of the client.
    pub client_uri: Localized<Url>,

    /// Name of the client to be presented to the end-user during authorization.
    pub client_name: Option<Localized<String>>,

    /// URL that references a logo for the client application.
    pub logo_uri: Option<Localized<Url>>,

    /// URL that the client provides to the end-user to read about the how the
    /// profile data will be used.
    pub policy_uri: Option<Localized<Url>>,

    /// URL that the client provides to the end-user to read about the client's
    /// terms of service.
    pub tos_uri: Option<Localized<Url>>,
}

impl ClientMetadata {
    /// Construct a `ClientMetadata` with only the required fields.
    pub fn new(
        application_type: ApplicationType,
        grant_types: Vec<OAuthGrantType>,
        client_uri: Localized<Url>,
    ) -> Self {
        Self {
            application_type,
            grant_types,
            client_uri,
            client_name: None,
            logo_uri: None,
            policy_uri: None,
            tos_uri: None,
        }
    }
}

/// The grant types that the user will use at the token endpoint.
///
/// The available variants match the methods supported by the [`OAuth`] API.
///
/// [`OAuth`]: super::OAuth
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum OAuthGrantType {
    /// The authorization code grant type, defined in [RFC 6749].
    ///
    /// This grant type is necessary to use [`OAuth::login()`].
    ///
    /// [RFC 6749]: https://datatracker.ietf.org/doc/html/rfc6749
    /// [`OAuth::login()`]: super::OAuth::login
    AuthorizationCode {
        /// Redirection URIs for the authorization endpoint.
        redirect_uris: Vec<Url>,
    },

    /// The device authorization grant, defined in [RFC 8628].
    ///
    /// This grant type is necessary to use [`OAuth::login_with_qr_code()`].
    ///
    /// [RFC 8628]: https://datatracker.ietf.org/doc/html/rfc8628
    /// [`OAuth::login_with_qr_code()`]: super::OAuth::login_with_qr_code
    DeviceCode,
}

/// The possible types of an application.
#[derive(Clone, StringEnum)]
#[ruma_enum(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ApplicationType {
    /// The application is a web client.
    ///
    /// This is a client executed within a user-agent on the device used by the
    /// user.
    Web,

    /// The application is a native client.
    ///
    /// This is a client installed and executed on the device used by the user.
    Native,

    #[doc(hidden)]
    _Custom(PrivOwnedStr),
}

/// A collection of localized variants.
///
/// Always includes one non-localized variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Localized<T> {
    non_localized: T,
    localized: HashMap<LanguageTag, T>,
}

impl<T> Localized<T> {
    /// Constructs a new `Localized` with the given non-localized and localized
    /// variants.
    pub fn new(non_localized: T, localized: impl IntoIterator<Item = (LanguageTag, T)>) -> Self {
        Self { non_localized, localized: localized.into_iter().collect() }
    }

    /// Get the non-localized variant.
    pub fn non_localized(&self) -> &T {
        &self.non_localized
    }

    /// Get the variant corresponding to the given language, if it exists.
    pub fn get(&self, language: Option<&LanguageTag>) -> Option<&T> {
        match language {
            Some(lang) => self.localized.get(lang),
            None => Some(&self.non_localized),
        }
    }
}

impl<T> From<(T, HashMap<LanguageTag, T>)> for Localized<T> {
    fn from(t: (T, HashMap<LanguageTag, T>)) -> Self {
        Localized { non_localized: t.0, localized: t.1 }
    }
}

#[derive(Serialize)]
struct ClientMetadataSerializeHelper {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    redirect_uris: Vec<Url>,
    token_endpoint_auth_method: &'static str,
    grant_types: BTreeSet<GrantType>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    response_types: Vec<ResponseType>,
    application_type: ApplicationType,
    #[serde(flatten)]
    localized: ClientMetadataLocalizedFields,
}

impl From<ClientMetadata> for ClientMetadataSerializeHelper {
    fn from(value: ClientMetadata) -> Self {
        let ClientMetadata {
            application_type,
            grant_types: oauth_grant_types,
            client_uri,
            client_name,
            logo_uri,
            policy_uri,
            tos_uri,
        } = value;

        let mut redirect_uris = None;
        let mut response_types = None;
        let mut grant_types = BTreeSet::new();

        // Support for refresh tokens is mandatory.
        grant_types.insert(GrantType::RefreshToken);

        for oauth_grant_type in oauth_grant_types {
            match oauth_grant_type {
                OAuthGrantType::AuthorizationCode { redirect_uris: uris } => {
                    redirect_uris = Some(uris);
                    response_types = Some(vec![ResponseType::Code]);
                    grant_types.insert(GrantType::AuthorizationCode);
                }
                OAuthGrantType::DeviceCode => {
                    grant_types.insert(GrantType::DeviceCode);
                }
            }
        }

        ClientMetadataSerializeHelper {
            redirect_uris: redirect_uris.unwrap_or_default(),
            // We only support public clients.
            token_endpoint_auth_method: "none",
            grant_types,
            response_types: response_types.unwrap_or_default(),
            application_type,
            localized: ClientMetadataLocalizedFields {
                client_uri,
                client_name,
                logo_uri,
                policy_uri,
                tos_uri,
            },
        }
    }
}

/// Helper type for serialization of `Localized` fields.
///
/// Those fields require to be serialized as one field per language so we need
/// to use a custom `Serialize` implementation.
struct ClientMetadataLocalizedFields {
    client_uri: Localized<Url>,
    client_name: Option<Localized<String>>,
    logo_uri: Option<Localized<Url>>,
    policy_uri: Option<Localized<Url>>,
    tos_uri: Option<Localized<Url>>,
}

impl Serialize for ClientMetadataLocalizedFields {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        fn serialize_localized_into_map<M: SerializeMap, T: Serialize>(
            map: &mut M,
            field_name: &str,
            value: &Localized<T>,
        ) -> Result<(), M::Error> {
            map.serialize_entry(field_name, &value.non_localized)?;

            for (lang, localized) in &value.localized {
                map.serialize_entry(&format!("{field_name}#{lang}"), localized)?;
            }

            Ok(())
        }

        let mut map = serializer.serialize_map(None)?;

        serialize_localized_into_map(&mut map, "client_uri", &self.client_uri)?;

        if let Some(client_name) = &self.client_name {
            serialize_localized_into_map(&mut map, "client_name", client_name)?;
        }

        if let Some(logo_uri) = &self.logo_uri {
            serialize_localized_into_map(&mut map, "logo_uri", logo_uri)?;
        }

        if let Some(policy_uri) = &self.policy_uri {
            serialize_localized_into_map(&mut map, "policy_uri", policy_uri)?;
        }

        if let Some(tos_uri) = &self.tos_uri {
            serialize_localized_into_map(&mut map, "tos_uri", tos_uri)?;
        }

        map.end()
    }
}

#[cfg(test)]
mod tests {
    use language_tags::LanguageTag;
    use serde_json::json;
    use url::Url;

    use super::{ApplicationType, ClientMetadata, Localized, OAuthGrantType};

    #[test]
    fn test_serialize_minimal_client_metadata() {
        let metadata = ClientMetadata::new(
            ApplicationType::Native,
            vec![OAuthGrantType::AuthorizationCode {
                redirect_uris: vec![Url::parse("http://127.0.0.1/").unwrap()],
            }],
            Localized::new(
                Url::parse("https://github.com/matrix-org/matrix-rust-sdk").unwrap(),
                [],
            ),
        );

        assert_eq!(
            serde_json::to_value(metadata).unwrap(),
            json!({
                "application_type": "native",
                "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none",
                "redirect_uris": ["http://127.0.0.1/"],
                "client_uri": "https://github.com/matrix-org/matrix-rust-sdk",
            }),
        );
    }

    #[test]
    fn test_serialize_full_client_metadata() {
        let lang_fr = LanguageTag::parse("fr").unwrap();
        let lang_mas = LanguageTag::parse("mas").unwrap();

        let mut metadata = ClientMetadata::new(
            ApplicationType::Web,
            vec![
                OAuthGrantType::AuthorizationCode {
                    redirect_uris: vec![
                        Url::parse("http://127.0.0.1/").unwrap(),
                        Url::parse("http://[::1]/").unwrap(),
                    ],
                },
                OAuthGrantType::DeviceCode,
            ],
            Localized::new(
                Url::parse("https://example.org/matrix-client").unwrap(),
                [
                    (lang_fr.clone(), Url::parse("https://example.org/fr/matrix-client").unwrap()),
                    (
                        lang_mas.clone(),
                        Url::parse("https://example.org/mas/matrix-client").unwrap(),
                    ),
                ],
            ),
        );

        metadata.client_name = Some(Localized::new(
            "My Matrix client".to_owned(),
            [(lang_fr.clone(), "Mon client Matrix".to_owned())],
        ));
        metadata.logo_uri =
            Some(Localized::new(Url::parse("https://example.org/logo.svg").unwrap(), []));
        metadata.policy_uri = Some(Localized::new(
            Url::parse("https://example.org/policy").unwrap(),
            [
                (lang_fr.clone(), Url::parse("https://example.org/fr/policy").unwrap()),
                (lang_mas.clone(), Url::parse("https://example.org/mas/policy").unwrap()),
            ],
        ));
        metadata.tos_uri = Some(Localized::new(
            Url::parse("https://example.org/tos").unwrap(),
            [
                (lang_fr, Url::parse("https://example.org/fr/tos").unwrap()),
                (lang_mas, Url::parse("https://example.org/mas/tos").unwrap()),
            ],
        ));

        assert_eq!(
            serde_json::to_value(metadata).unwrap(),
            json!({
                "application_type": "web",
                "grant_types": [
                    "authorization_code",
                    "refresh_token",
                    "urn:ietf:params:oauth:grant-type:device_code",
                ],
                "response_types": ["code"],
                "token_endpoint_auth_method": "none",
                "redirect_uris": ["http://127.0.0.1/", "http://[::1]/"],
                "client_uri": "https://example.org/matrix-client",
                "client_uri#fr": "https://example.org/fr/matrix-client",
                "client_uri#mas": "https://example.org/mas/matrix-client",
                "client_name": "My Matrix client",
                "client_name#fr": "Mon client Matrix",
                "logo_uri": "https://example.org/logo.svg",
                "policy_uri": "https://example.org/policy",
                "policy_uri#fr": "https://example.org/fr/policy",
                "policy_uri#mas": "https://example.org/mas/policy",
                "tos_uri": "https://example.org/tos",
                "tos_uri#fr": "https://example.org/fr/tos",
                "tos_uri#mas": "https://example.org/mas/tos",
            }),
        );
    }
}
