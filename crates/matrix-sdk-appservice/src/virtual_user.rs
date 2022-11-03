// Copyright 2022 Famedly GmbH
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

//! Virtual users.

use matrix_sdk::{config::RequestConfig, Client, ClientBuildError, ClientBuilder, Session};
use ruma::{
    api::client::{session::login, uiaa::UserIdentifier},
    assign, DeviceId, OwnedDeviceId, UserId,
};
use tracing::warn;

use crate::{AppService, Result};

/// Builder for a virtual user
#[derive(Debug)]
pub struct VirtualUserBuilder<'a> {
    appservice: &'a AppService,
    localpart: &'a str,
    device_id: Option<OwnedDeviceId>,
    client_builder: ClientBuilder,
    log_in: bool,
    restored_session: Option<Session>,
}

impl<'a> VirtualUserBuilder<'a> {
    /// Create a new virtual user builder
    /// # Arguments
    ///
    /// * `localpart` - The localpart of the virtual user
    pub fn new(appservice: &'a AppService, localpart: &'a str) -> Self {
        Self {
            appservice,
            localpart,
            device_id: None,
            client_builder: Client::builder(),
            log_in: false,
            restored_session: None,
        }
    }

    /// Set the device ID of the virtual user
    pub fn device_id(mut self, device_id: Option<OwnedDeviceId>) -> Self {
        self.device_id = device_id;
        self
    }

    /// Sets the client builder to use for the virtual user
    pub fn client_builder(mut self, client_builder: ClientBuilder) -> Self {
        self.client_builder = client_builder;
        self
    }

    /// Log in as the virtual user
    ///
    /// In some cases it is necessary to log in as the virtual user, such as to
    /// upload device keys
    pub fn login(mut self) -> Self {
        self.log_in = true;
        self
    }

    /// Restore a persisted session
    ///
    /// This is primarily useful if you enable
    /// [`VirtualUserBuilder::login()`] and want to restore a session
    /// from a previous run.
    pub fn restored_session(mut self, session: Session) -> Self {
        self.restored_session = Some(session);
        self
    }

    /// Build the virtual user
    ///
    /// # Errors
    /// This function returns an error if an invalid localpart is provided.
    pub async fn build(self) -> Result<Client> {
        if let Some(client) = self.appservice.clients.get(self.localpart) {
            return Ok(client.clone());
        }

        let user_id = UserId::parse_with_server_name(self.localpart, &self.appservice.server_name)?;
        if !(self.appservice.user_id_is_in_namespace(&user_id)
            || self.localpart == self.appservice.registration.sender_localpart)
        {
            warn!("Virtual client id '{user_id}' is not in the namespace")
        }

        let mut builder = self.client_builder;

        if !self.log_in && self.localpart != self.appservice.registration.sender_localpart {
            builder = builder.assert_identity();
        }

        let client = builder
            .homeserver_url(self.appservice.homeserver_url.clone())
            .appservice_mode()
            .build()
            .await
            .map_err(ClientBuildError::assert_valid_builder_args)?;

        let session = if let Some(session) = self.restored_session {
            session
        } else if self.log_in && self.localpart != self.appservice.registration.sender_localpart {
            let login_info =
                login::v3::LoginInfo::ApplicationService(login::v3::ApplicationService::new(
                    UserIdentifier::UserIdOrLocalpart(self.localpart),
                ));

            let request = assign!(login::v3::Request::new(login_info), {
                device_id: self.device_id.as_ref().map(|v| v.as_ref()),
                initial_device_display_name: None,
            });

            let response =
                client.send(request, Some(RequestConfig::short_retry().force_auth())).await?;

            Session {
                access_token: response.access_token,
                refresh_token: response.refresh_token,
                user_id: response.user_id,
                device_id: response.device_id,
            }
        } else {
            // Don’t log in
            Session {
                access_token: self.appservice.registration.as_token.clone(),
                refresh_token: None,
                user_id: user_id.clone(),
                device_id: self.device_id.unwrap_or_else(DeviceId::new),
            }
        };

        client.restore_session(session).await?;

        self.appservice.clients.insert(self.localpart.to_owned(), client.clone());

        Ok(client)
    }
}
