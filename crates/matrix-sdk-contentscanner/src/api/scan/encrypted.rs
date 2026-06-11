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

use matrix_sdk::{RumaApiError, bytes::BufMut, encryption::vodozemac::Curve25519PublicKey};
use ruma::{
    api::{
        Metadata, OutgoingRequest,
        auth_scheme::{AccessTokenOptional, AuthScheme, SendAccessToken},
        error::IntoHttpError,
        path_builder::PathBuilder,
    },
    events::room::EncryptedFile,
    exports::http::Request,
    metadata,
};

use crate::api::{encrypted_file_request_from, scan::MediaScanResponse};

metadata! {
    @for EncryptedMediaScanRequest,
    method: POST,
    rate_limited: false,
    authentication: AccessTokenOptional,
    history: {
        unstable => "/_matrix/media_proxy/unstable/scan_encrypted",
    },
}

/// A request to scan an encrypted media file using the content scanner.
/// Spec: <https://github.com/element-hq/matrix-content-scanner-python/blob/main/docs/api.md#post-_matrixmedia_proxyunstablescan_encrypted>
#[derive(Debug, Clone)]
pub struct EncryptedMediaScanRequest {
    scanner_url: String,
    public_key: Option<Curve25519PublicKey>,
    encrypted_file: EncryptedFile,
}

impl EncryptedMediaScanRequest {
    pub fn new(
        scanner_url: String,
        public_key: Option<Curve25519PublicKey>,
        encrypted_file: EncryptedFile,
    ) -> Self {
        Self { scanner_url, public_key, encrypted_file }
    }
}

impl OutgoingRequest for EncryptedMediaScanRequest {
    type EndpointError = RumaApiError;
    type IncomingResponse = MediaScanResponse;

    fn try_into_http_request<T: Default + BufMut + AsRef<[u8]>>(
        self,
        _base_url: &str,
        authentication_input: <Self::Authentication as AuthScheme>::Input<'_>,
        path_builder_input: <Self::PathBuilder as PathBuilder>::Input<'_>,
    ) -> Result<Request<T>, IntoHttpError> {
        let url = Self::make_endpoint_url(path_builder_input, &self.scanner_url, &[], "")?;

        let body = encrypted_file_request_from(self.public_key, &self.encrypted_file)?;
        let body = ruma::serde::json_to_buf(&body)?;

        let mut request = Request::builder().method(Self::METHOD).uri(url).body(body)?;

        if let Some(access_token) = authentication_input.get_required_for_endpoint() {
            Self::Authentication::add_authentication(
                &mut request,
                SendAccessToken::IfRequired(access_token),
            )?
        }

        Ok(request)
    }
}
