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

use matrix_sdk::media::MediaFetcher;
use matrix_sdk_contentscanner::{ContentScannerMediaFetcher, MediaScanResponse};

use crate::{client::Client, error::ClientError, ruma::MediaSource};

#[derive(Debug, uniffi::Object)]
pub struct ContentScanner {
    pub(crate) inner: Arc<matrix_sdk_contentscanner::ContentScanner>,
}

impl ContentScanner {
    pub(crate) fn media_fetcher(&self) -> Arc<dyn MediaFetcher> {
        Arc::new(ContentScannerMediaFetcher::with_content_scanner(self.inner.clone()))
    }
}

#[matrix_sdk_ffi_macros::export]
impl ContentScanner {
    /// Instantiate a new [`ContentScanner`] using the `scanner_url`.
    #[uniffi::constructor]
    pub fn new(scanner_url: String) -> Arc<Self> {
        Arc::new(Self {
            inner: Arc::new(matrix_sdk_contentscanner::ContentScanner::new(scanner_url)),
        })
    }

    /// Scan a media source, returning a [`MediaScanResponse`] with the scan
    /// result, or an error if something failed when trying to scan the media.
    pub async fn scan(
        &self,
        client: Arc<Client>,
        media_source: Arc<MediaSource>,
    ) -> Result<MediaScanResponse, ClientError> {
        Ok(self.inner.scan(&client.inner, &media_source.media_source).await?)
    }
}
