// Copyright 2026 Nordeck IT + Consulting GmbH <info@nordeck.net>
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

//! Login with the OAuth 2.0 Device Authorization Grant ([RFC 8628]).
//!
//! [RFC 8628]: https://datatracker.ietf.org/doc/html/rfc8628

use std::{future::IntoFuture, time::Duration};

use eyeball::SharedObservable;
use futures_core::Stream;
use matrix_sdk_base::boxed_into_future;
use oauth2::{Scope, StandardDeviceAuthorizationResponse};
use ruma::{
    DeviceId, OwnedDeviceId,
    api::client::discovery::get_authorization_server_metadata::v1::GrantType,
};
use tracing::{trace, warn};
use url::Url;

use super::{
    ClientRegistrationData, OAuth, OAuthError, error::DeviceCodeLoginError,
    registration::ensure_grant_type,
};

/// The progress of a login with the Device Authorization Grant, using
/// [`OAuth::login_with_device_code()`].
// The enum is only constructed a few times per login, so its size is not a
// concern.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, Default)]
pub enum DeviceCodeLoginProgress {
    /// We're just starting up, this is the default and initial state.
    #[default]
    Starting,

    /// We have received the device authorization from the OAuth 2.0
    /// authorization server, and we are waiting for the end-user to grant the
    /// authorization.
    ///
    /// The verification URI and the user code must be presented to the
    /// end-user, who must open the URI in a browser and enter the user code
    /// if necessary.
    WaitingForToken {
        /// The end-user verification URI on the authorization server.
        ///
        /// The end-user should open this URI in a browser and enter the
        /// `user_code`.
        verification_uri: Url,

        /// A verification URI that includes the `user_code`, designed for
        /// non-textual transmission, for example in a QR code.
        ///
        /// If this is available, it can be presented to the end-user instead
        /// of the `verification_uri`, so they don't need to enter the
        /// `user_code` manually.
        verification_uri_complete: Option<Url>,

        /// The end-user verification code.
        user_code: String,

        /// The lifetime of the `user_code`, from the moment it was received.
        ///
        /// If the end-user doesn't grant the authorization before this delay,
        /// the login fails with [`DeviceCodeLoginError::ExpiredToken`].
        expires_in: Duration,
    },

    /// The authorization was granted, and we are loading the session.
    LoadingSession,

    /// The login process has completed.
    Done,
}

impl DeviceCodeLoginProgress {
    fn waiting_for_token(response: &StandardDeviceAuthorizationResponse) -> Self {
        let verification_uri_complete =
            response.verification_uri_complete().and_then(|uri| match Url::parse(uri.secret()) {
                Ok(uri) => Some(uri),
                Err(error) => {
                    warn!("Ignoring invalid `verification_uri_complete`: {error}");
                    None
                }
            });

        Self::WaitingForToken {
            verification_uri: response.verification_uri().url().clone(),
            verification_uri_complete,
            user_code: response.user_code().secret().clone(),
            expires_in: response.expires_in(),
        }
    }
}

/// Named future for logging in with the Device Authorization Grant, returned by
/// [`OAuth::login_with_device_code()`].
///
/// The login is performed when this is awaited. Use
/// [`LoginWithDeviceCode::subscribe_to_progress()`] beforehand to get the
/// verification URI and user code to present to the end-user.
///
/// Dropping the future before it completes cancels the login: the client stops
/// polling the authorization server and no session is set.
#[derive(Debug)]
pub struct LoginWithDeviceCode {
    oauth: OAuth,
    scopes: Vec<Scope>,
    device_id: OwnedDeviceId,
    registration_data: Option<ClientRegistrationData>,
    state: SharedObservable<DeviceCodeLoginProgress>,
}

impl LoginWithDeviceCode {
    pub(super) fn new(
        oauth: OAuth,
        scopes: Vec<Scope>,
        device_id: OwnedDeviceId,
        registration_data: Option<ClientRegistrationData>,
    ) -> Self {
        Self { oauth, scopes, device_id, registration_data, state: Default::default() }
    }

    /// The device ID that will be associated with the session.
    ///
    /// This is either the device ID that was passed to
    /// [`OAuth::login_with_device_code()`], or the one that was generated if
    /// none was provided.
    pub fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    /// Subscribe to the progress of the login.
    ///
    /// It is necessary to subscribe to this to get the verification URI and
    /// user code to present to the end-user, with the
    /// [`DeviceCodeLoginProgress::WaitingForToken`] variant.
    ///
    /// The stream only yields the latest state, so a slow subscriber might
    /// skip intermediate states, and it ends when the login future completes,
    /// possibly before [`DeviceCodeLoginProgress::Done`] is observed. The
    /// output of the future is the authoritative result of the login.
    pub fn subscribe_to_progress(&self) -> impl Stream<Item = DeviceCodeLoginProgress> + use<> {
        self.state.subscribe()
    }
}

impl IntoFuture for LoginWithDeviceCode {
    type Output = Result<(), DeviceCodeLoginError>;
    boxed_into_future!();

    fn into_future(self) -> Self::IntoFuture {
        Box::pin(async move {
            let Self { oauth, scopes, device_id, registration_data, state } = self;

            trace!("Fetching the OAuth 2.0 server metadata.");
            let server_metadata = oauth.server_metadata().await.map_err(OAuthError::from)?;

            // Fail early, before registering the client, if the server doesn't
            // support the device authorization grant.
            if server_metadata.device_authorization_endpoint.is_none() {
                return Err(DeviceCodeLoginError::NoDeviceAuthorizationEndpoint);
            }

            // The client must declare the grant types that it uses during
            // registration.
            let registration_data = registration_data.map(|mut data| {
                data.metadata = ensure_grant_type(&data.metadata, GrantType::DeviceCode);
                data
            });

            trace!("Registering the client with the OAuth 2.0 authorization server.");
            oauth.use_registration_data(&server_metadata, registration_data.as_ref()).await?;

            trace!("Requesting device authorization.");
            let response = oauth.request_device_authorization(&server_metadata, scopes).await?;

            state.set(DeviceCodeLoginProgress::waiting_for_token(&response));

            trace!("Waiting for the OAuth 2.0 authorization server to give us the access token.");
            oauth.exchange_device_code(&server_metadata, &response).await?;

            state.set(DeviceCodeLoginProgress::LoadingSession);

            trace!("Loading the session.");
            oauth.load_session(device_id).await?;

            trace!("Successfully logged in with the device authorization grant.");
            state.set(DeviceCodeLoginProgress::Done);

            Ok(())
        })
    }
}
