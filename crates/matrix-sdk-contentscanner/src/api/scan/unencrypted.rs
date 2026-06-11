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

use matrix_sdk::{RumaApiError, bytes::BufMut};
use ruma::{
    api::{
        Metadata, OutgoingRequest,
        auth_scheme::{AccessTokenOptional, AuthScheme, SendAccessToken},
        error::IntoHttpError,
        path_builder::PathBuilder,
    },
    exports::http::Request,
    metadata,
};

use crate::api::scan::MediaScanResponse;

metadata! {
    @for MediaScanRequest,
    method: GET,
    rate_limited: false,
    authentication: AccessTokenOptional,
    history: {
        unstable => "/_matrix/media_proxy/unstable/scan/{server_name}/{media_id}",
    },
}

/// A request to scan an unencrypted media file using the content scanner.
/// Spec: <https://github.com/element-hq/matrix-content-scanner-python/blob/main/docs/api.md#get-_matrixmedia_proxyunstablescanservernamemediaid>
#[derive(Debug, Clone)]
pub struct MediaScanRequest {
    scanner_url: String,
    server_name: String,
    media_id: String,
}

impl MediaScanRequest {
    pub fn new(scanner_url: String, server_name: String, media_id: String) -> Self {
        Self { scanner_url, server_name, media_id }
    }
}

impl OutgoingRequest for MediaScanRequest {
    type EndpointError = RumaApiError;
    type IncomingResponse = MediaScanResponse;

    fn try_into_http_request<T: Default + BufMut + AsRef<[u8]>>(
        self,
        _base_url: &str,
        authentication_input: <Self::Authentication as AuthScheme>::Input<'_>,
        path_builder_input: <Self::PathBuilder as PathBuilder>::Input<'_>,
    ) -> Result<Request<T>, IntoHttpError> {
        let url = Self::make_endpoint_url(
            path_builder_input,
            &self.scanner_url,
            &[&self.server_name, &self.media_id],
            "",
        )?;

        let mut request = Request::builder().method(Self::METHOD).uri(url).body(T::default())?;

        if let Some(access_token) = authentication_input.get_required_for_endpoint() {
            Self::Authentication::add_authentication(
                &mut request,
                SendAccessToken::IfRequired(access_token),
            )?
        }

        Ok(request)
    }
}
