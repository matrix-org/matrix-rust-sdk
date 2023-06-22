// Copyright 2020 The Matrix.org Foundation C.I.C.
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

use std::{
    any::type_name,
    fmt::Debug,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use bytes::{Bytes, BytesMut};
use bytesize::ByteSize;
use eyeball::shared::Observable as SharedObservable;
use ruma::{
    api::{
        error::{FromHttpResponseError, IntoHttpError},
        AuthScheme, MatrixVersion, OutgoingRequest, OutgoingRequestAppserviceExt, SendAccessToken,
    },
    UserId,
};
use tracing::{debug, field::debug, instrument, trace};

use crate::{config::RequestConfig, error::HttpError};

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(target_arch = "wasm32")]
mod wasm;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::HttpSettings;

pub(crate) const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
pub(crate) struct HttpClient {
    pub(crate) inner: reqwest::Client,
    pub(crate) request_config: RequestConfig,
    next_request_id: Arc<AtomicU64>,
}

impl HttpClient {
    pub(crate) fn new(inner: reqwest::Client, request_config: RequestConfig) -> Self {
        HttpClient { inner, request_config, next_request_id: AtomicU64::new(0).into() }
    }

    fn get_request_id(&self) -> String {
        let request_id = self.next_request_id.fetch_add(1, Ordering::SeqCst);
        format!("REQ-{request_id}")
    }

    fn serialize_request<R>(
        &self,
        request: R,
        config: RequestConfig,
        homeserver: String,
        access_token: Option<&str>,
        user_id: Option<&UserId>,
        server_versions: &[MatrixVersion],
    ) -> Result<http::Request<Bytes>, IntoHttpError>
    where
        R: OutgoingRequest + Debug,
    {
        trace!(request_type = type_name::<R>(), "Serializing request");

        // We can't assert the identity without a user_id.
        let request = if let Some((access_token, user_id)) =
            access_token.filter(|_| config.assert_identity).zip(user_id)
        {
            request.try_into_http_request_with_user_id::<BytesMut>(
                &homeserver,
                SendAccessToken::Always(access_token),
                user_id,
                server_versions,
            )?
        } else {
            let send_access_token = match access_token {
                Some(access_token) => {
                    if config.force_auth {
                        SendAccessToken::Always(access_token)
                    } else {
                        SendAccessToken::IfRequired(access_token)
                    }
                }
                None => SendAccessToken::None,
            };

            request.try_into_http_request::<BytesMut>(
                &homeserver,
                send_access_token,
                server_versions,
            )?
        };

        let request = request.map(|body| body.freeze());

        Ok(request)
    }

    #[allow(clippy::too_many_arguments)]
    #[instrument(
        skip(self, access_token, config, request, user_id, send_progress),
        fields(
            config,
            path,
            user_id,
            request_size,
            request_body,
            request_id,
            status,
            response_size,
        )
    )]
    pub async fn send<R>(
        &self,
        request: R,
        config: Option<RequestConfig>,
        homeserver: String,
        access_token: Option<&str>,
        user_id: Option<&UserId>,
        server_versions: &[MatrixVersion],
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> Result<R::IncomingResponse, HttpError>
    where
        R: OutgoingRequest + Debug,
        HttpError: From<FromHttpResponseError<R::EndpointError>>,
    {
        let config = match config {
            Some(config) => config,
            None => self.request_config,
        };

        // Keep some local variables in a separate scope so the compiler doesn't include
        // them in the future type. https://github.com/rust-lang/rust/issues/57478
        let request = {
            let request_id = self.get_request_id();
            let span = tracing::Span::current();

            // At this point in the code, the config isn't behind an Option anymore, that's
            // why we record it here, instead of in the #[instrument] macro.
            span.record("config", debug(config)).record("request_id", request_id);

            // The user ID is only used if we're an app-service. Only log the user_id if
            // it's `Some` and if assert_identity is set.
            if config.assert_identity {
                span.record("user_id", user_id.map(debug));
            }

            let auth_scheme = R::METADATA.authentication;
            if !matches!(auth_scheme, AuthScheme::AccessToken | AuthScheme::None) {
                return Err(HttpError::NotClientRequest);
            }

            let request = self.serialize_request(
                request,
                config,
                homeserver,
                access_token,
                user_id,
                server_versions,
            )?;

            let request_size = ByteSize(request.body().len().try_into().unwrap_or(u64::MAX));
            span.record("request_size", request_size.to_string_as(true));

            // Since sliding sync is experimental, and the proxy might not do what we expect
            // it to do given a specific request body, it's useful to log the
            // request body here. This doesn't contain any personal information.
            // TODO: Remove this once sliding sync isn't experimental anymore.
            #[cfg(feature = "experimental-sliding-sync")]
            if type_name::<R>() == "ruma_client_api::sync::sync_events::v4::Request" {
                span.record("request_body", debug(request.body()));
                span.record("path", request.uri().path_and_query().map(|p| p.as_str()));
            } else {
                span.record("path", request.uri().path());
            }

            #[cfg(not(feature = "experimental-sliding-sync"))]
            span.record("path", request.uri().path());

            request
        };

        debug!("Sending request");

        // There's a bunch of state in send_request, factor out a pinned inner
        // future to reduce this size of futures that await this function.
        match Box::pin(self.send_request::<R>(request, config, send_progress)).await {
            Ok(response) => {
                debug!("Got response");
                Ok(response)
            }
            Err(e) => {
                debug!("Error while sending request: {e:?}");
                Err(e)
            }
        }
    }
}

/// Progress of sending or receiving a payload.
#[derive(Clone, Copy, Debug, Default)]
pub struct TransmissionProgress {
    /// How many bytes were already transferred.
    pub current: usize,
    /// How many bytes there are in total.
    pub total: usize,
}

async fn response_to_http_response(
    mut response: reqwest::Response,
) -> Result<http::Response<Bytes>, reqwest::Error> {
    let status = response.status();

    let mut http_builder = http::Response::builder().status(status);
    let headers = http_builder.headers_mut().expect("Can't get the response builder headers");

    for (k, v) in response.headers_mut().drain() {
        if let Some(key) = k {
            headers.insert(key, v);
        }
    }

    let body = response.bytes().await?;

    Ok(http_builder.body(body).expect("Can't construct a response using the given body"))
}
