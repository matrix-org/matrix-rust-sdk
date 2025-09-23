// Copyright 2023 The Matrix.org Foundation C.I.C.
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

use std::fmt::Debug;

use bytes::Bytes;
use bytesize::ByteSize;
use eyeball::SharedObservable;
use ruma::api::{IncomingResponse, OutgoingRequest, error::FromHttpResponseError};

use super::{HttpClient, TransmissionProgress, response_to_http_response};
use crate::{config::RequestConfig, error::HttpError};

impl HttpClient {
    pub(super) async fn send_request<R>(
        &self,
        request: http::Request<Bytes>,
        _config: RequestConfig,
        _send_progress: SharedObservable<TransmissionProgress>,
    ) -> Result<R::IncomingResponse, HttpError>
    where
        R: OutgoingRequest + Debug,
        HttpError: From<FromHttpResponseError<R::EndpointError>>,
    {
        tracing::debug!("Sending request");

        let request = reqwest::Request::try_from(request)?;

        let before = ruma::time::Instant::now();

        let response = response_to_http_response(self.inner.execute(request).await?).await?;

        let request_duration = ruma::time::Instant::now().saturating_duration_since(before);
        let status_code = response.status();
        let response_size = ByteSize(response.body().len().try_into().unwrap_or(u64::MAX));

        tracing::Span::current()
            .record("status", status_code.as_u16())
            .record("response_size", response_size.display().si_short().to_string())
            .record("request_duration", tracing::field::debug(request_duration));

        Ok(R::IncomingResponse::try_from_http_response(response)?)
    }
}
