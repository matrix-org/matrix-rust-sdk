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

use matrix_sdk::locks::Mutex;
use matrix_sdk::{
    BoxFuture, Client, Error, IdParseError,
    locks::Mutex,
    media::{MediaFetcher, MediaRequestParameters},
};

#[cfg(feature = "uniffi")]
uniffi::setup_scaffolding!();


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
        _client: &'a Client,
        _request: &'a MediaRequestParameters,
    ) -> BoxFuture<'a, matrix_sdk::Result<Vec<u8>, Error>> {
        Box::pin(async move {
            todo!("Implement fetch_media_content")
        })
    }
}
}
