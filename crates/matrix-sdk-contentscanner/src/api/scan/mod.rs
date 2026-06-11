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

use matrix_sdk::RumaApiError;
use ruma::{
    api::{EndpointError, IncomingResponse, error::FromHttpResponseError},
    exports::{http::Response, serde_json},
};
use serde::Deserialize;

pub mod encrypted;
pub mod unencrypted;

/// A media scan response containing the result of the scan.
/// Spec: <https://github.com/element-hq/matrix-content-scanner-python/blob/main/docs/api.md#get-_matrixmedia_proxyunstablescanservernamemediaid>
#[derive(Debug, Deserialize)]
pub struct MediaScanResponse {
    /// Whether the media is clean or contained something dangerous.
    pub clean: bool,
    /// Extra information about the scan.
    pub info: String,
}

impl IncomingResponse for MediaScanResponse {
    type EndpointError = RumaApiError;

    fn try_from_http_response<T: AsRef<[u8]>>(
        response: Response<T>,
    ) -> Result<Self, FromHttpResponseError<Self::EndpointError>> {
        if response.status().is_success() {
            let content = response.body().as_ref().to_vec();
            let media_scan_response: MediaScanResponse = serde_json::from_slice(&content)?;
            Ok(media_scan_response)
        } else {
            Err(FromHttpResponseError::Server(Self::EndpointError::from_http_response(response)))
        }
    }
}
