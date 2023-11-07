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

use cfg_vis::cfg_vis;
use eyeball::SharedObservable;
#[cfg(not(target_arch = "wasm32"))]
use eyeball::Subscriber;
#[cfg(feature = "experimental-oidc")]
use mas_oidc_client::{
    error::{
        Error as OidcClientError, ErrorBody as OidcErrorBody, HttpError as OidcHttpError,
        TokenRefreshError, TokenRequestError,
    },
    types::errors::ClientErrorCode,
};
use matrix_sdk_common::boxed_into_future;
use ruma::api::{client::error::ErrorKind, error::FromHttpResponseError, OutgoingRequest};
#[cfg(feature = "experimental-oidc")]
use tracing::error;
use tracing::trace;

use super::super::Client;
#[cfg(feature = "experimental-oidc")]
use crate::oidc::OidcError;
use crate::{
    config::RequestConfig,
    error::{HttpError, HttpResult},
    RefreshTokenError, TransmissionProgress,
};

/// `IntoFuture` returned by [`Client::send`].
#[allow(missing_debug_implementations)]
pub struct SendRequest<R> {
    pub(crate) client: Client,
    pub(crate) sliding_sync_proxy_url: Option<String>,
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
    #[cfg_vis(target_arch = "wasm32", pub(crate))]
    pub fn with_send_progress_observable(
        mut self,
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> Self {
        self.send_progress = send_progress;
        self
    }

    /// Get a subscriber to observe the progress of sending the request
    /// body.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn subscribe_to_send_progress(&self) -> Subscriber<TransmissionProgress> {
        self.send_progress.subscribe()
    }
}

impl<R> IntoFuture for SendRequest<R>
where
    R: OutgoingRequest + Clone + Debug + Send + Sync + 'static,
    R::IncomingResponse: Send + Sync,
    HttpError: From<FromHttpResponseError<R::EndpointError>>,
{
    type Output = HttpResult<R::IncomingResponse>;
    boxed_into_future!();

    fn into_future(self) -> Self::IntoFuture {
        let Self { client, request, config, send_progress, sliding_sync_proxy_url } = self;

        Box::pin(async move {
            let res = Box::pin(client.send_inner(
                request.clone(),
                config,
                sliding_sync_proxy_url.clone(),
                send_progress.clone(),
            ))
            .await;

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

                        #[cfg(feature = "experimental-oidc")]
                        RefreshTokenError::Oidc(oidc_error) => {
                            match **oidc_error {
                                OidcError::Oidc(OidcClientError::TokenRefresh(
                                    TokenRefreshError::Token(TokenRequestError::Http(
                                        OidcHttpError {
                                            body:
                                                Some(OidcErrorBody {
                                                    error: ClientErrorCode::InvalidGrant,
                                                    ..
                                                }),
                                            ..
                                        },
                                    )),
                                )) => {
                                    error!("Token refresh: OIDC refresh_token rejected with invalid grant");
                                    // The refresh was denied, signal to sign out the user.
                                    client.broadcast_unknown_token(soft_logout);
                                }
                                _ => {
                                    trace!("Token refresh: OIDC refresh encountered a problem.");
                                    // The refresh failed for other reasons, no
                                    // need to sign out.
                                }
                            };
                            return Err(refresh_error.into());
                        }

                        _ => {
                            trace!("Token refresh: Token refresh failed.");
                            // This isn't necessarily correct, but matches the behaviour when
                            // implementing OIDC.
                            client.broadcast_unknown_token(soft_logout);
                            return Err(refresh_error.into());
                        }
                    }
                } else {
                    trace!("Token refresh: Refresh succeeded, retrying request.");
                    return Box::pin(client.send_inner(
                        request,
                        config,
                        sliding_sync_proxy_url,
                        send_progress,
                    ))
                    .await;
                }
            }

            res
        })
    }
}
