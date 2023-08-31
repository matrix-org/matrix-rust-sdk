#[cfg(feature = "experimental-oidc")]
use std::{
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    ops::Deref,
};
use std::{
    fmt::Debug,
    future::{Future, IntoFuture},
    pin::Pin,
};

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
    #[cfg(target_arch = "wasm32")]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output>>>;
    #[cfg(not(target_arch = "wasm32"))]
    type IntoFuture = Pin<Box<dyn Future<Output = Self::Output> + Send>>;

    fn into_future(self) -> Self::IntoFuture {
        let Self { client, request, config, send_progress } = self;
        Box::pin(async move {
            let res =
                Box::pin(client.send_inner(request.clone(), config, None, send_progress.clone()))
                    .await;

            // An `M_UNKNOWN_TOKEN` error can potentially be fixed with a token refresh.
            if let Err(Some(ErrorKind::UnknownToken { soft_logout })) =
                res.as_ref().map_err(HttpError::client_api_error_kind)
            {
                trace!("Token refresh: Unknown token error received.");
                // If automatic token refresh isn't supported, there is nothing more to do.
                if !client.inner.handle_refresh_tokens {
                    trace!("Token refresh: Automatic refresh disabled.");
                    client.broadcast_unknown_token(soft_logout);
                    return res;
                }

                #[cfg(feature = "experimental-oidc")]
                let refresh_token = client
                    .session()
                    .as_ref()
                    .and_then(|s| s.get_refresh_token().map(ToOwned::to_owned));

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
                            let oidc_error = oidc_error.deref();
                            match oidc_error {
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
                                    let hash = refresh_token.map(|t| {
                                        let mut hasher = DefaultHasher::new();
                                        t.hash(&mut hasher);
                                        hasher.finish()
                                    });

                                    error!("Token refresh: OIDC refresh_token rejected {:?}", hash);
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
                    return Box::pin(client.send_inner(request, config, None, send_progress)).await;
                }
            }

            res
        })
    }
}
