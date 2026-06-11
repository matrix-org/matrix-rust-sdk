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
        unencrypted::DownloadAndScanMediaRequest,
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
            MediaSource::Encrypted(_encrypted) => {
                todo!("Let's start with unencrypted media first.")
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
}
