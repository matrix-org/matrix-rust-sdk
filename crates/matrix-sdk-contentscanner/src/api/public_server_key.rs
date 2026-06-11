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
    api::{
        IncomingResponse, Metadata, OutgoingRequest,
        auth_scheme::{AuthScheme, NoAuthentication},
        error::{FromHttpResponseError, IntoHttpError},
        path_builder::PathBuilder,
    },
    exports::{
        bytes::BufMut,
        http::{Request, Response},
        serde_json,
    },
    metadata,
};
use serde::Deserialize;

metadata! {
    @for PublicServerKeyRequest,
    method: GET,
    rate_limited: false,
    authentication: NoAuthentication,
    history: {
        unstable => "/_matrix/media_proxy/unstable/public_key",
    },
}

/// The HTTP request body for fetching the public server key.
/// Spec: <https://github.com/element-hq/matrix-content-scanner-python/blob/main/docs/api.md#get-_matrixmedia_proxyunstablepublic_key>
#[derive(Debug, Clone)]
pub(crate) struct PublicServerKeyRequest {
    scanner_url: String,
}

impl PublicServerKeyRequest {
    pub(crate) fn new(scanner_url: impl Into<String>) -> Self {
        Self { scanner_url: scanner_url.into() }
    }
}

impl OutgoingRequest for PublicServerKeyRequest {
    type EndpointError = RumaApiError;
    type IncomingResponse = PublicServerKeyResponse;

    fn try_into_http_request<T: Default + BufMut + AsRef<[u8]>>(
        self,
        _base_url: &str,
        _authentication_input: <Self::Authentication as AuthScheme>::Input<'_>,
        path_builder_input: <Self::PathBuilder as PathBuilder>::Input<'_>,
    ) -> Result<Request<T>, IntoHttpError> {
        let url = Self::make_endpoint_url(path_builder_input, &self.scanner_url, &[], "")?;
        Ok(Request::builder().method(Self::METHOD).uri(url).body(T::default())?)
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct PublicServerKeyResponse {
    pub(crate) public_key: String,
}

impl IncomingResponse for PublicServerKeyResponse {
    type EndpointError = RumaApiError;

    fn try_from_http_response<T: AsRef<[u8]>>(
        response: Response<T>,
    ) -> Result<Self, FromHttpResponseError<Self::EndpointError>> {
        Ok(serde_json::from_slice::<PublicServerKeyResponse>(response.body().as_ref())?)
    }
}
