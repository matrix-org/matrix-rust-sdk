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

//! Login with the OAuth 2.0 Device Authorization Grant ([RFC 8628]), as
//! specified for Matrix in [MSC4341].
//!
//! [RFC 8628]: https://datatracker.ietf.org/doc/html/rfc8628
//! [MSC4341]: https://github.com/matrix-org/matrix-spec-proposals/pull/4341

use std::time::Duration;

use oauth2::{Scope, StandardDeviceAuthorizationResponse};
use ruma::{
    DeviceId, OwnedDeviceId,
    api::client::discovery::get_authorization_server_metadata::v1::{
        AuthorizationServerMetadata, GrantType,
    },
};
use tracing::{trace, warn};
use url::Url;

use super::{
    ClientRegistrationData, OAuth, OAuthError, error::DeviceCodeLoginError,
    registration::ensure_grant_type,
};

/// A login with the Device Authorization Grant that is waiting for the
/// end-user to grant the authorization, returned by
/// [`OAuth::login_with_device_code()`].
///
/// The verification URI and the user code must be presented to the end-user,
/// who must open the URI in a browser on another device and enter the user
/// code if necessary. Then [`DeviceCodeLogin::finish()`] must be called to
/// wait for the authorization and complete the login.
#[derive(Debug)]
pub struct DeviceCodeLogin {
    oauth: OAuth,
    server_metadata: AuthorizationServerMetadata,
    device_id: OwnedDeviceId,
    response: StandardDeviceAuthorizationResponse,
    verification_uri_complete: Option<Url>,
}

impl DeviceCodeLogin {
    /// Register the client if necessary, and request the device
    /// authorization from the OAuth 2.0 authorization server.
    pub(super) async fn start(
        oauth: OAuth,
        scopes: Vec<Scope>,
        device_id: OwnedDeviceId,
        mut registration_data: Option<ClientRegistrationData>,
    ) -> Result<Self, DeviceCodeLoginError> {
        trace!("Fetching the OAuth 2.0 server metadata.");
        let server_metadata = oauth.server_metadata().await.map_err(OAuthError::from)?;

        // Fail early, before registering the client, if the server doesn't
        // support the device authorization grant.
        if server_metadata.device_authorization_endpoint.is_none() {
            return Err(DeviceCodeLoginError::NoDeviceAuthorizationEndpoint);
        }

        // The client must declare the grant types that it uses during
        // registration.
        if let Some(data) = &mut registration_data {
            data.metadata = ensure_grant_type(&data.metadata, GrantType::DeviceCode);
        }

        trace!("Registering the client with the OAuth 2.0 authorization server.");
        oauth.use_registration_data(&server_metadata, registration_data.as_ref()).await?;

        trace!("Requesting device authorization.");
        let response = oauth.request_device_authorization(&server_metadata, scopes).await?;

        let verification_uri_complete =
            response.verification_uri_complete().and_then(|uri| match Url::parse(uri.secret()) {
                Ok(uri) => Some(uri),
                Err(error) => {
                    warn!("Ignoring invalid `verification_uri_complete`: {error}");
                    None
                }
            });

        Ok(Self { oauth, server_metadata, device_id, response, verification_uri_complete })
    }

    /// The device ID that will be associated with the session.
    ///
    /// This is either the device ID that was passed to
    /// [`OAuth::login_with_device_code()`], or the one that was generated if
    /// none was provided.
    pub fn device_id(&self) -> &DeviceId {
        &self.device_id
    }

    /// The end-user verification URI on the authorization server.
    ///
    /// The end-user should open this URI in a browser and enter the
    /// [`user_code()`](Self::user_code).
    pub fn verification_uri(&self) -> &Url {
        self.response.verification_uri().url()
    }

    /// A verification URI that includes the [`user_code()`](Self::user_code),
    /// designed for non-textual transmission, for example in a QR code.
    ///
    /// This is optional, and complements the
    /// [`verification_uri()`](Self::verification_uri) and the
    /// [`user_code()`](Self::user_code) rather than replacing them: if it is
    /// available, it can be presented in addition to them, so the end-user
    /// doesn't need to enter the user code manually. The verification URI and
    /// the user code should still be displayed, as a fallback for an end-user
    /// that cannot use this URI, and so the user code can be compared with the
    /// one shown by the authorization server.
    ///
    /// See [RFC 8628 section 3.3.1] for more details.
    ///
    /// [RFC 8628 section 3.3.1]: https://datatracker.ietf.org/doc/html/rfc8628#section-3.3.1
    pub fn verification_uri_complete(&self) -> Option<&Url> {
        self.verification_uri_complete.as_ref()
    }

    /// The end-user verification code.
    pub fn user_code(&self) -> &str {
        self.response.user_code().secret()
    }

    /// The lifetime of the user code, from the moment it was received.
    ///
    /// If the end-user doesn't grant the authorization before this delay,
    /// [`DeviceCodeLogin::finish()`] fails with
    /// [`DeviceCodeLoginError::ExpiredToken`].
    pub fn expires_in(&self) -> Duration {
        self.response.expires_in()
    }

    /// Wait for the end-user to grant the authorization, and complete the
    /// login.
    ///
    /// This polls the OAuth 2.0 authorization server until the authorization
    /// is granted, denied, or until it expires. When the authorization is
    /// granted, the session is loaded into the client, like with
    /// [`OAuth::finish_login()`].
    ///
    /// Dropping the returned future before it completes cancels the login:
    /// the client stops polling the authorization server and no session is
    /// set.
    pub async fn finish(self) -> Result<(), DeviceCodeLoginError> {
        let Self { oauth, server_metadata, device_id, response, .. } = self;

        trace!("Waiting for the OAuth 2.0 authorization server to give us the access token.");
        oauth.exchange_device_code(&server_metadata, &response).await?;

        trace!("Loading the session.");
        oauth.load_session(device_id).await?;

        trace!("Successfully logged in with the device authorization grant.");

        Ok(())
    }
}
