use std::{
    fmt::Debug,
    future::{Future, IntoFuture},
    pin::Pin,
};

use cfg_vis::cfg_vis;
use eyeball::SharedObservable;
#[cfg(not(target_arch = "wasm32"))]
use eyeball::Subscriber;
use ruma::api::{client::error::ErrorKind, error::FromHttpResponseError, OutgoingRequest};

use super::super::Client;
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

            // If this is an `M_UNKNOWN_TOKEN` error and refresh token handling is active,
            // try to refresh the token and retry the request.
            if client.inner.handle_refresh_tokens {
                if let Err(Some(ErrorKind::UnknownToken { .. })) =
                    res.as_ref().map_err(HttpError::client_api_error_kind)
                {
                    if let Err(refresh_error) = client.refresh_access_token().await {
                        match refresh_error {
                            RefreshTokenError::RefreshTokenRequired => {
                                // Refreshing access tokens is not supported by
                                // this `Session`, ignore.
                            }
                            _ => {
                                return Err(refresh_error.into());
                            }
                        }
                    } else {
                        return Box::pin(client.send_inner(request, config, None, send_progress))
                            .await;
                    }
                }
            }

            res
        })
    }
}
