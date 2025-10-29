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

#![deny(unreachable_pub)]

use std::{fmt::Debug, future::IntoFuture};

use eyeball::{SharedObservable, Subscriber};
use js_int::UInt;
use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm, boxed_into_future};
use oauth2::{RequestTokenError, basic::BasicErrorResponseType};
use ruma::api::{
    OutgoingRequest,
    auth_scheme::{AuthScheme, SendAccessToken},
    client::{error::ErrorKind, media},
    error::FromHttpResponseError,
    path_builder::PathBuilder,
};
use tracing::{error, trace};

use super::super::Client;
use crate::{
    Error, RefreshTokenError, TransmissionProgress,
    authentication::oauth::OAuthError,
    config::RequestConfig,
    error::{HttpError, HttpResult},
    http_client::SupportedPathBuilder,
    media::MediaError,
};

/// `IntoFuture` returned by [`Client::send`].
#[allow(missing_debug_implementations)]
pub struct SendRequest<R> {
    pub(crate) client: Client,
    pub(crate) request: R,
    pub(crate) config: Option<RequestConfig>,
    pub(crate) send_progress: SharedObservable<TransmissionProgress>,
}

impl<R> SendRequest<R> {
    /// Replace the default `SharedObservable` used for tracking upload
    /// progress.
    ///
    /// Note that any subscribers obtained from
    /// [`subscribe_to_send_progress`][Self::subscribe_to_send_progress]
    /// will be invalidated by this.
    pub fn with_send_progress_observable(
        mut self,
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> Self {
        self.send_progress = send_progress;
        self
    }

    /// Use the given [`RequestConfig`] for this send request, instead of the
    /// one provided by default.
    pub fn with_request_config(mut self, request_config: impl Into<Option<RequestConfig>>) -> Self {
        self.config = request_config.into();
        self
    }

    /// Get a subscriber to observe the progress of sending the request
    /// body.
    pub fn subscribe_to_send_progress(&self) -> Subscriber<TransmissionProgress> {
        self.send_progress.subscribe()
    }
}

impl<R> IntoFuture for SendRequest<R>
where
    R: OutgoingRequest + Clone + Debug + SendOutsideWasm + SyncOutsideWasm + 'static,
    for<'a> R::Authentication: AuthScheme<Input<'a> = SendAccessToken<'a>>,
    R::PathBuilder: SupportedPathBuilder,
    for<'a> <R::PathBuilder as PathBuilder>::Input<'a>: SendOutsideWasm + SyncOutsideWasm,
    R::IncomingResponse: SendOutsideWasm + SyncOutsideWasm,
    HttpError: From<FromHttpResponseError<R::EndpointError>>,
{
    type Output = HttpResult<R::IncomingResponse>;
    boxed_into_future!();

    fn into_future(self) -> Self::IntoFuture {
        let Self { client, request, config, send_progress } = self;

        Box::pin(async move {
            let res =
                Box::pin(client.send_inner(request.clone(), config, send_progress.clone())).await;

            // An `M_UNKNOWN_TOKEN` error can potentially be fixed with a token refresh.
            if let Err(Some(ErrorKind::UnknownToken { soft_logout })) =
                res.as_ref().map_err(HttpError::client_api_error_kind)
            {
                trace!("Token refresh: Unknown token error received.");

                // If automatic token refresh isn't supported, there is nothing more to do.
                if !client.inner.auth_ctx.handle_refresh_tokens {
                    trace!("Token refresh: Automatic refresh disabled.");
                    client.broadcast_unknown_token(soft_logout);
                    return res;
                }

                // Try to refresh the token and retry the request.
                if let Err(refresh_error) = client.refresh_access_token().await {
                    match &refresh_error {
                        RefreshTokenError::RefreshTokenRequired => {
                            trace!("Token refresh: The session doesn't have a refresh token.");
                            // Refreshing access tokens is not supported by this `Session`, ignore.
                            client.broadcast_unknown_token(soft_logout);
                        }

                        RefreshTokenError::OAuth(oauth_error) => {
                            match &**oauth_error {
                                OAuthError::RefreshToken(RequestTokenError::ServerResponse(
                                    error_response,
                                )) if *error_response.error()
                                    == BasicErrorResponseType::InvalidGrant =>
                                {
                                    error!(
                                        "Token refresh: OAuth 2.0 refresh_token rejected \
                                         with invalid grant"
                                    );
                                    // The refresh was denied, signal to sign out the user.
                                    client.broadcast_unknown_token(soft_logout);
                                }
                                _ => {
                                    trace!(
                                        "Token refresh: OAuth 2.0 refresh encountered a problem."
                                    );
                                    // The refresh failed for other reasons, no
                                    // need to sign out.
                                }
                            }
                            return Err(HttpError::RefreshToken(refresh_error));
                        }

                        _ => {
                            trace!("Token refresh: Token refresh failed.");
                            // This isn't necessarily correct, but matches the behaviour when
                            // implementing OAuth 2.0.
                            client.broadcast_unknown_token(soft_logout);
                            return Err(HttpError::RefreshToken(refresh_error));
                        }
                    }
                } else {
                    trace!("Token refresh: Refresh succeeded, retrying request.");
                    return Box::pin(client.send_inner(request, config, send_progress)).await;
                }
            }

            res
        })
    }
}

/// `IntoFuture` used to send media upload requests. It wraps another
/// [`SendRequest`], checking its size will be accepted by the homeserver before
/// uploading.
#[allow(missing_debug_implementations)]
pub struct SendMediaUploadRequest {
    send_request: SendRequest<media::create_content::v3::Request>,
}

impl SendMediaUploadRequest {
    pub fn new(request: SendRequest<media::create_content::v3::Request>) -> Self {
        Self { send_request: request }
    }

    /// Replace the default `SharedObservable` used for tracking upload
    /// progress.
    ///
    /// Note that any subscribers obtained from
    /// [`subscribe_to_send_progress`][Self::subscribe_to_send_progress]
    /// will be invalidated by this.
    pub fn with_send_progress_observable(
        mut self,
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> Self {
        self.send_request = self.send_request.with_send_progress_observable(send_progress);
        self
    }

    /// Get a subscriber to observe the progress of sending the request
    /// body.
    pub fn subscribe_to_send_progress(&self) -> Subscriber<TransmissionProgress> {
        self.send_request.send_progress.subscribe()
    }
}

impl IntoFuture for SendMediaUploadRequest {
    type Output = Result<media::create_content::v3::Response, Error>;
    boxed_into_future!();

    fn into_future(self) -> Self::IntoFuture {
        let request_length = self.send_request.request.file.len();
        let client = self.send_request.client.clone();
        let send_request = self.send_request;

        Box::pin(async move {
            let max_upload_size = client.load_or_fetch_max_upload_size().await?;
            let request_length = UInt::new_wrapping(request_length as u64);
            if request_length > max_upload_size {
                return Err(Error::Media(MediaError::MediaTooLargeToUpload {
                    max: max_upload_size,
                    current: request_length,
                }));
            }

            send_request.into_future().await.map_err(Into::into)
        })
    }
}
