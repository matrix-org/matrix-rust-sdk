// Copyright 2026 The Matrix.org Foundation C.I.C.
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

use std::sync::Arc;

use api::{
    download::{
        encrypted::DownloadAndScanEncryptedMediaRequest, unencrypted::DownloadAndScanMediaRequest,
    },
    public_server_key::PublicServerKeyRequest,
};
use matrix_sdk::{
    BoxFuture, Client, Error, IdParseError,
    encryption::vodozemac::pk_encryption::Message,
    locks::Mutex,
    media::{MediaFetcher, MediaRequestParameters},
    ruma::events::room::MediaSource,
};
use matrix_sdk_crypto::olm::Curve25519PublicKey;
use tracing::trace;

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();

use crate::api::{
    DownloadAndScanMediaResponse,
};

mod api;

/// A helper component to download and scan media from a content scanner server.
#[derive(Debug)]
pub struct ContentScanner {
    scanner_url: String,
    public_server_key: Arc<Mutex<Option<String>>>,
}

impl ContentScanner {
    /// Instantiate a new [`ContentScanner`] using the `scanner_url`.
    pub fn new(scanner_url: impl Into<String>) -> Self {
        Self { scanner_url: scanner_url.into(), public_server_key: Arc::new(Mutex::new(None)) }
    }

    pub(crate) async fn fetch_public_server_key(&self, client: &Client) -> Result<String, Error> {
        let response = client.send(PublicServerKeyRequest::new(self.scanner_url.clone())).await?;
        Ok(response.public_key)
    }

    async fn get_or_fetch_public_server_key(&self, client: &Client) -> Option<Curve25519PublicKey> {
        let public_server_key =
            if let Some(public_server_key) = (*self.public_server_key.lock()).clone() {
                trace!("Using cached public server key");
                Some(public_server_key)
            } else {
                trace!("Using cached public server key");
                let ret = self.fetch_public_server_key(client).await.ok();

                if let Some(public_server_key) = &ret {
                    trace!("Saved new public server key");
                    let mut guard = self.public_server_key.lock();
                    let _ = guard.insert(public_server_key.clone());
                }

                ret
            };

        public_server_key.and_then(|key| Curve25519PublicKey::from_base64(&key).ok())
    }

    pub(crate) async fn get_media(
        &self,
        client: &Client,
        media_source: &MediaSource,
    ) -> Result<DownloadAndScanMediaResponse, Error> {
        match &media_source {
            MediaSource::Encrypted(encrypted) => {
                // Get the public server key if we don't have it yet.
                let public_server_key = self.get_or_fetch_public_server_key(client).await;

                Ok(client
                    .send(DownloadAndScanEncryptedMediaRequest::new(
                        self.scanner_url.clone(),
                        public_server_key,
                        *encrypted.clone(),
                    ))
                    .await?)
            }
            MediaSource::Plain(mxc) => {
                let (server_name, media_id) =
                    mxc.parts().map_err(|e| Error::Identifier(IdParseError::InvalidMxcUri(e)))?;
                Ok(client
                    .send(DownloadAndScanMediaRequest::new(
                        &self.scanner_url,
                        server_name.as_str(),
                        media_id,
                    ))
                    .await?)
            }
        }
    }

#[derive(Debug, Clone, Serialize)]
struct EncryptedBody {
    ciphertext: String,
    mac: String,
    ephemeral: String,
}

impl From<Message> for EncryptedBody {
    fn from(value: Message) -> Self {
        Self {
            ciphertext: Base64::<Standard>::new(value.ciphertext).to_string(),
            mac: Base64::<Standard>::new(value.mac).to_string(),
            ephemeral: value.ephemeral_key.to_base64(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct EncryptedFileRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<EncryptedFile>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encrypted_body: Option<EncryptedBody>,
}

impl EncryptedFileRequest {
    pub(crate) fn from_file_info(file_info: EncryptedFile) -> Self {
        Self { file: Some(file_info), encrypted_body: None }
    }

    pub(crate) fn from_encrypted_body(encrypted_body: EncryptedBody) -> Self {
        Self { file: None, encrypted_body: Some(encrypted_body) }
    }
}

/// A media fetcher that uses the content scanner to download and scan media.
pub struct ContentScannerMediaFetcher {
    pub content_scanner: ContentScanner,
}

impl ContentScannerMediaFetcher {
    /// Instantiate a new [`MediaFetcher`] using the provided `scanner_url`.
    pub fn new(scanner_url: impl Into<String>) -> Self {
        Self { content_scanner: ContentScanner::new(scanner_url.into()) }
    }
}

impl MediaFetcher for ContentScannerMediaFetcher {
    fn fetch_media_content<'a>(
        &'a self,
        client: &'a Client,
        request: &'a MediaRequestParameters,
    ) -> BoxFuture<'a, matrix_sdk::Result<Vec<u8>, Error>> {
        Box::pin(async move {
            Ok(self.content_scanner.get_media(client, &request.source).await?.content)
        })
    }
}

/// A content scanner error.
#[derive(Debug, Deserialize)]
pub struct ContentScannerError {
    pub info: String,
    pub reason: ErrorReason,
}

/// The reason for the content scanner error.
#[allow(non_camel_case_types)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Clone, Debug, Deserialize)]
pub enum ErrorReason {
    /// The JSON file is malformed.
    MCS_MALFORMED_JSON,
    /// The media could not be decrypted.
    MCS_MEDIA_FAILED_TO_DECRYPT,
    /// No access token was provided.
    M_MISSING_TOKEN,
    /// The access token provided is invalid.
    M_UNKNOWN_TOKEN,
    /// The media was not found.
    M_NOT_FOUND,
    /// The media has some potentially dangerous content.
    MCS_MEDIA_NOT_CLEAN,
    /// The media has been blocked by the server because of its mime type.
    MCS_MIME_TYPE_FORBIDDEN,
    /// The used public key is wrong.
    MCS_BAD_DECRYPTION,
    /// An unknown error occurred.
    M_UNKNOWN,
    /// The server failed to request media from the media repo.
    MCS_MEDIA_REQUEST_FAILED,
}

#[cfg(test)]
mod tests {
    use std::ops::Not;

    use assert_matches2::assert_matches;
    use matrix_sdk::{HttpError, RumaApiError, test_utils::mocks::MatrixMockServer};
    use matrix_sdk_test::async_test;
    use ruma::{
        api::{
            MatrixVersion,
            error::{ErrorBody, FromHttpResponseError},
        },
        events::room::{
            EncryptedFile, EncryptedFileHash, EncryptedFileHashes, EncryptedFileInfo, MediaSource,
            V2EncryptedFileInfo,
        },
        exports::{http::StatusCode, serde_json::json},
        owned_mxc_uri,
        serde::Base64,
    };
    use serde::Deserialize;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{header_exists, method, path, path_regex},
    };

    use crate::ContentScanner;

    #[async_test]
    async fn test_fetch_public_key() {
        let server = MatrixMockServer::new().await;
        let client =
            server.client_builder().server_versions(vec![MatrixVersion::V1_11]).build().await;

        let content_scanner_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/_matrix/media_proxy/unstable/public_key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "public_key": "1234567890"
            })))
            .mount(&content_scanner_server)
            .await;

        let content_scanner = ContentScanner::new(content_scanner_server.uri());
        content_scanner.fetch_public_server_key(&client).await.expect("Load public key");
    }

    #[async_test]
    async fn test_get_media() {
        let server = MatrixMockServer::new().await;
        let client =
            server.client_builder().server_versions(vec![MatrixVersion::V1_11]).build().await;

        let content_scanner_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"/_matrix/media_proxy/unstable/download/.+/.+"))
            .and(header_exists("Authorization"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3, 4, 5, 6, 7, 8, 9]),
            )
            .mount(&content_scanner_server)
            .await;

        let content_scanner = ContentScanner::new(content_scanner_server.uri());
        let media_source =
            MediaSource::Plain(owned_mxc_uri!("mxc://matrix.org/RhfpOXOzAwzkuqcmbgMwQUrJ"));
        content_scanner.get_media(&client, &media_source).await.expect("Get media");
    }

    #[async_test]
    async fn test_get_media_unsupported() {
        let server = MatrixMockServer::new().await;
        let client =
            server.client_builder().server_versions(vec![MatrixVersion::V1_11]).build().await;

        let content_scanner_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path_regex(r"/_matrix/media_proxy/unstable/download/.+/.+"))
            .and(header_exists("Authorization"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "reason": "MCS_MIME_TYPE_FORBIDDEN",
                "info": "File type: application/octet-stream not allowed",
            })))
            .mount(&content_scanner_server)
            .await;

        let content_scanner = ContentScanner::new(content_scanner_server.uri());
        let media_source =
            MediaSource::Plain(owned_mxc_uri!("mxc://matrix.org/ckTaStcNnFXLzKApkBmgRDoC"));
        let err =
            content_scanner.get_media(&client, &media_source).await.expect_err("Get media error");
        let client_error = err.as_client_api_error().expect("Get client error");
        assert_eq!(client_error.status_code, StatusCode::FORBIDDEN);
        assert_eq!(
            client_error.to_string(),
            "[403] {\"info\":\"File type: application/octet-stream not allowed\",\"reason\":\"MCS_MIME_TYPE_FORBIDDEN\"}"
        );
    }

    #[async_test]
    async fn test_get_encrypted_media() {
        let server = MatrixMockServer::new().await;
        let client =
            server.client_builder().server_versions(vec![MatrixVersion::V1_11]).build().await;

        let content_scanner_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/_matrix/media_proxy/unstable/download_encrypted"))
            .and(header_exists("Authorization"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(vec![1, 2, 3, 4, 5, 6, 7, 8, 9]),
            )
            .mount(&content_scanner_server)
            .await;

        let content_scanner = ContentScanner::new(content_scanner_server.uri());
        let file_info = EncryptedFileInfo::V2(V2EncryptedFileInfo::new(
            Base64::parse("9lpOscZyMOZRCF3v867nPPo3WPNMZt9JXMsuYiWRszc".as_bytes()).expect("k"),
            Base64::parse("czvdfKSjfLEAAAAAAAAAAA".as_bytes()).expect("iv"),
        ));
        let mut hashes = EncryptedFileHashes::new();
        hashes.insert(EncryptedFileHash::Sha256(
            Base64::parse("SBbJ3hINT2LgwXK8ev82enjnhubUy5UuKGDF3SezAhs".as_bytes()).expect("hash"),
        ));
        let media_source = MediaSource::Encrypted(Box::new(EncryptedFile::new(
            owned_mxc_uri!(
                "mxc://element.io/b50f38aa8ae820c75992370e4e944a045481e3932057062074730676224"
            ),
            file_info,
            hashes,
        )));
        content_scanner.get_media(&client, &media_source).await.expect("Get media");
    }

    #[async_test]
    async fn test_get_encrypted_media_unsupported() {
        let server = MatrixMockServer::new().await;
        let client =
            server.client_builder().server_versions(vec![MatrixVersion::V1_11]).build().await;

        let content_scanner_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/_matrix/media_proxy/unstable/download_encrypted"))
            .and(header_exists("Authorization"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "reason": "MCS_MIME_TYPE_FORBIDDEN",
                "info": "File type: application/octet-stream not allowed",
            })))
            .mount(&content_scanner_server)
            .await;

        let content_scanner = ContentScanner::new(content_scanner_server.uri());
        let file_info = EncryptedFileInfo::V2(V2EncryptedFileInfo::new(
            Base64::parse("tdHdCI5mc-g29IYfhYx2wkA5o-bILP9-nXY6Np1uSnM".as_bytes()).expect("k"),
            Base64::parse("IBFdH65KqhoAAAAAAAAAAA".as_bytes()).expect("iv"),
        ));
        let mut hashes = EncryptedFileHashes::new();
        hashes.insert(EncryptedFileHash::Sha256(
            Base64::parse("HSkkamvMSvF3Q30HInorh0ccPrxjgu+wp1vyUOmov/8".as_bytes()).expect("hash"),
        ));
        let media_source = MediaSource::Encrypted(Box::new(EncryptedFile::new(
            owned_mxc_uri!("mxc://matrix.org/WlfuejQQdpvWiWVpAGwfIKJL"),
            file_info,
            hashes,
        )));
        let err = content_scanner
            .get_media(&client, &media_source)
            .await
            .expect_err("Invalid type error");
        let client_error = err.as_client_api_error().expect("Invalid error");
        assert_eq!(client_error.status_code, StatusCode::FORBIDDEN);
        assert_eq!(
            client_error.to_string(),
            "[403] {\"info\":\"File type: application/octet-stream not allowed\",\"reason\":\"MCS_MIME_TYPE_FORBIDDEN\"}"
        );
    }
}
