// Copyright 2020 Damir Jelić
// Copyright 2020 The Matrix.org Foundation C.I.C.
// Copyright 2022 Kévin Commaille
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

use std::io::Read;

use matrix_sdk_base::media::{MediaFormat, MediaRequest};
use mime::Mime;
use ruma::{
    api::client::{
        account::{
            add_3pid, change_password, deactivate, delete_3pid, get_3pids,
            request_3pid_management_token_via_email, request_3pid_management_token_via_msisdn,
        },
        profile::{
            get_avatar_url, get_display_name, get_profile, set_avatar_url, set_display_name,
        },
        uiaa::AuthData,
    },
    assign,
    events::room::MediaSource,
    thirdparty::Medium,
    ClientSecret, MxcUri, OwnedMxcUri, SessionId, UInt,
};

use crate::{config::RequestConfig, Client, Error, Result};

/// A high-level API to manage the client owner's account.
///
/// All the methods on this struct send a request to the homeserver.
#[derive(Debug, Clone)]
pub struct Account {
    /// The underlying HTTP client.
    client: Client,
}

impl Account {
    pub(crate) fn new(client: Client) -> Self {
        Self { client }
    }

    /// Get the display name of the account.
    ///
    /// # Example
    /// ```no_run
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// let user = "example";
    /// let client = Client::new(homeserver).await?;
    /// client.login(user, "password", None, None).await?;
    ///
    /// if let Some(name) = client.account().get_display_name().await? {
    ///     println!("Logged in as user '{}' with display name '{}'", user, name);
    /// }
    /// # anyhow::Ok(()) });
    /// ```
    pub async fn get_display_name(&self) -> Result<Option<String>> {
        let user_id = self.client.user_id().await.ok_or(Error::AuthenticationRequired)?;
        let request = get_display_name::v3::Request::new(&user_id);
        let response = self.client.send(request, None).await?;
        Ok(response.displayname)
    }

    /// Set the display name of the account.
    ///
    /// # Example
    /// ```no_run
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// let user = "example";
    /// let client = Client::new(homeserver).await?;
    /// client.login(user, "password", None, None).await?;
    ///
    /// client.account().set_display_name(Some("Alice")).await?;
    /// # anyhow::Ok(()) });
    /// ```
    pub async fn set_display_name(&self, name: Option<&str>) -> Result<()> {
        let user_id = self.client.user_id().await.ok_or(Error::AuthenticationRequired)?;
        let request = set_display_name::v3::Request::new(&user_id, name);
        self.client.send(request, None).await?;
        Ok(())
    }

    /// Get the MXC URI of the account's avatar, if set.
    ///
    /// # Example
    /// ```no_run
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let user = "example";
    /// let client = Client::new(homeserver).await?;
    /// client.login(user, "password", None, None).await?;
    ///
    /// if let Some(url) = client.account().get_avatar_url().await? {
    ///     println!("Your avatar's mxc url is {}", url);
    /// }
    /// # anyhow::Ok(()) });
    /// ```
    pub async fn get_avatar_url(&self) -> Result<Option<OwnedMxcUri>> {
        let user_id = self.client.user_id().await.ok_or(Error::AuthenticationRequired)?;
        let request = get_avatar_url::v3::Request::new(&user_id);

        let config = Some(RequestConfig::new().force_auth());

        let response = self.client.send(request, config).await?;
        Ok(response.avatar_url)
    }

    /// Set the MXC URI of the account's avatar.
    ///
    /// The avatar is unset if `url` is `None`.
    pub async fn set_avatar_url(&self, url: Option<&MxcUri>) -> Result<()> {
        let user_id = self.client.user_id().await.ok_or(Error::AuthenticationRequired)?;
        let request = set_avatar_url::v3::Request::new(&user_id, url);
        self.client.send(request, None).await?;
        Ok(())
    }

    /// Get the account's avatar, if set.
    ///
    /// Returns the avatar.
    ///
    /// If a thumbnail is requested no guarantee on the size of the image is
    /// given.
    ///
    /// # Arguments
    ///
    /// * `format` - The desired format of the avatar.
    ///
    /// # Example
    /// ```no_run
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::room_id;
    /// # use matrix_sdk::media::MediaFormat;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://example.com")?;
    /// # let user = "example";
    /// let client = Client::new(homeserver).await?;
    /// client.login(user, "password", None, None).await?;
    ///
    /// if let Some(avatar) = client.account().get_avatar(MediaFormat::File).await? {
    ///     std::fs::write("avatar.png", avatar);
    /// }
    /// # anyhow::Ok(()) });
    /// ```
    pub async fn get_avatar(&self, format: MediaFormat) -> Result<Option<Vec<u8>>> {
        if let Some(url) = self.get_avatar_url().await? {
            let request = MediaRequest { source: MediaSource::Plain(url), format };
            Ok(Some(self.client.get_media_content(&request, true).await?))
        } else {
            Ok(None)
        }
    }

    /// Upload and set the account's avatar.
    ///
    /// This will upload the data produced by the reader to the homeserver's
    /// content repository, and set the user's avatar to the MXC URI for the
    /// uploaded file.
    ///
    /// This is a convenience method for calling [`Client::upload()`],
    /// followed by [`Account::set_avatar_url()`].
    ///
    /// Returns the MXC URI of the uploaded avatar.
    ///
    /// # Example
    /// ```no_run
    /// # use std::{path::Path, fs::File, io::Read};
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// let path = Path::new("/home/example/selfie.jpg");
    /// let mut image = File::open(&path)?;
    ///
    /// client.account().upload_avatar(&mime::IMAGE_JPEG, &mut image).await?;
    /// # anyhow::Ok(()) });
    /// ```
    pub async fn upload_avatar<R: Read>(
        &self,
        content_type: &Mime,
        reader: &mut R,
    ) -> Result<OwnedMxcUri> {
        let upload_response = self.client.upload(content_type, reader).await?;
        self.set_avatar_url(Some(&upload_response.content_uri)).await?;
        Ok(upload_response.content_uri)
    }

    /// Get the profile of the account.
    ///
    /// Allows to get both the display name and avatar URL in a single call.
    ///
    /// # Example
    /// ```no_run
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// if let profile = client.account().get_profile().await? {
    ///     println!(
    ///         "You are '{:?}' with avatar '{:?}'",
    ///         profile.displayname,
    ///         profile.avatar_url
    ///     );
    /// }
    /// # anyhow::Ok(()) });
    /// ```
    pub async fn get_profile(&self) -> Result<get_profile::v3::Response> {
        let user_id = self.client.user_id().await.ok_or(Error::AuthenticationRequired)?;
        let request = get_profile::v3::Request::new(&user_id);
        Ok(self.client.send(request, None).await?)
    }

    /// Change the password of the account.
    ///
    /// # Arguments
    ///
    /// * `new_password` - The new password to set.
    ///
    /// * `auth_data` - This request uses the [User-Interactive Authentication
    /// API][uiaa]. The first request needs to set this to `None` and will
    /// always fail with an [`UiaaResponse`]. The response will contain
    /// information for the interactive auth and the same request needs to be
    /// made but this time with some `auth_data` provided.
    ///
    /// # Returns
    ///
    /// This method might return an [`ErrorKind::WeakPassword`] error if the new
    /// password is considered insecure by the homeserver, with details about
    /// the strength requirements in the error's message.
    ///
    /// # Example
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::{
    /// #     api::client::{
    /// #         account::change_password::v3::{Request as ChangePasswordRequest},
    /// #         uiaa::{AuthData, Dummy},
    /// #     },
    /// #     assign,
    /// # };
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// client.account().change_password(
    ///     "myverysecretpassword",
    ///     Some(AuthData::Dummy(Dummy::new())),
    /// ).await?;
    /// # anyhow::Ok(()) });
    /// ```
    /// [uiaa]: https://spec.matrix.org/v1.2/client-server-api/#user-interactive-authentication-api
    /// [`UiaaResponse`]: ruma::api::client::uiaa::UiaaResponse
    /// [`ErrorKind::WeakPassword`]: ruma::api::client::error::ErrorKind::WeakPassword
    pub async fn change_password(
        &self,
        new_password: &str,
        auth_data: Option<AuthData<'_>>,
    ) -> Result<change_password::v3::Response> {
        let request = assign!(change_password::v3::Request::new(new_password), {
            auth: auth_data,
        });
        Ok(self.client.send(request, None).await?)
    }

    /// Deactivate this account definitively.
    ///
    /// # Arguments
    ///
    /// * `id_server` - The identity server from which to unbind the user’s
    /// [Third Party Identifiers][3pid].
    ///
    /// * `auth_data` - This request uses the [User-Interactive Authentication
    /// API][uiaa]. The first request needs to set this to `None` and will
    /// always fail with an [`UiaaResponse`]. The response will contain
    /// information for the interactive auth and the same request needs to be
    /// made but this time with some `auth_data` provided.
    ///
    /// # Example
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::{
    /// #     api::client::{
    /// #         account::change_password::v3::{Request as ChangePasswordRequest},
    /// #         uiaa::{AuthData, Dummy},
    /// #     },
    /// #     assign,
    /// # };
    /// # use futures::executor::block_on;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// # let account = client.account();
    /// let response = account.deactivate(None, None).await;
    ///
    /// // Proceed with UIAA.
    ///
    /// # anyhow::Ok(()) });
    /// ```
    /// [3pid]: https://spec.matrix.org/v1.2/appendices/#3pid-types
    /// [uiaa]: https://spec.matrix.org/v1.2/client-server-api/#user-interactive-authentication-api
    /// [`UiaaResponse`]: ruma::api::client::uiaa::UiaaResponse
    pub async fn deactivate(
        &self,
        id_server: Option<&str>,
        auth_data: Option<AuthData<'_>>,
    ) -> Result<deactivate::v3::Response> {
        let request = assign!(deactivate::v3::Request::new(), {
            id_server,
            auth: auth_data,
        });
        Ok(self.client.send(request, None).await?)
    }

    /// Get the registered [Third Party Identifiers][3pid] on the homeserver of
    /// the account.
    ///
    /// These 3PIDs may be used by the homeserver to authenticate the user
    /// during sensitive operations.
    ///
    /// # Example
    /// ```no_run
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// let threepids = client.account().get_3pids().await?.threepids;
    ///
    /// for threepid in threepids {
    ///     println!("Found 3PID '{}' of type '{}'", threepid.address, threepid.medium);
    /// }
    /// # anyhow::Ok(()) });
    /// ```
    /// [3pid]: https://spec.matrix.org/v1.2/appendices/#3pid-types
    pub async fn get_3pids(&self) -> Result<get_3pids::v3::Response> {
        let request = get_3pids::v3::Request::new();
        Ok(self.client.send(request, None).await?)
    }

    /// Request a token to validate an email address as a [Third Party
    /// Identifier][3pid].
    ///
    /// This is the first step in registering an email address as 3PID. Next,
    /// call [`Account::add_3pid()`] with the same `client_secret` and the
    /// returned `sid`.
    ///
    /// # Arguments
    ///
    /// * `client_secret` - A client-generated secret string used to protect
    /// this session.
    ///
    /// * `email` - The email address to validate.
    ///
    /// * `send_attempt` - The attempt number. This number needs to be
    /// incremented if you want to request another token for the same
    /// validation.
    ///
    /// # Returns
    ///
    /// * `sid` - The session ID to be used in following requests for
    /// this 3PID.
    ///
    /// * `submit_url` - If present, the user will submit the token to
    /// the client, that must send it to this URL. If not, the client will not
    /// be involved in the token submission.
    ///
    /// This method might return an [`ErrorKind::ThreepidInUse`] error if the
    /// email address is already registered for this account or another, or an
    /// [`ErrorKind::ThreepidDenied`] error if it is denied.
    ///
    /// # Example
    /// ```no_run
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::{ClientSecret, uint};
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// # let account = client.account();
    /// # let secret = ClientSecret::parse("secret")?;
    /// let token_response = account.request_3pid_email_token(
    ///     &secret,
    ///     "john@matrix.org",
    ///     uint!(0),
    /// ).await?;
    ///
    /// // Wait for the user to confirm that the token was submitted or prompt
    /// // the user for the token and send it to submit_url.
    ///
    /// let uiaa_response = account.add_3pid(
    ///     &secret,
    ///     &token_response.sid,
    ///     None
    /// ).await;
    ///
    /// // Proceed with UIAA.
    ///
    /// # anyhow::Ok(()) });
    /// ```
    /// [3pid]: https://spec.matrix.org/v1.2/appendices/#3pid-types
    /// [`ErrorKind::ThreepidInUse`]: ruma::api::client::error::ErrorKind::ThreepidInUse
    /// [`ErrorKind::ThreepidDenied`]: ruma::api::client::error::ErrorKind::ThreepidDenied
    pub async fn request_3pid_email_token(
        &self,
        client_secret: &ClientSecret,
        email: &str,
        send_attempt: UInt,
    ) -> Result<request_3pid_management_token_via_email::v3::Response> {
        let request = request_3pid_management_token_via_email::v3::Request::new(
            client_secret,
            email,
            send_attempt,
        );
        Ok(self.client.send(request, None).await?)
    }

    /// Request a token to validate a phone number as a [Third Party
    /// Identifier][3pid].
    ///
    /// This is the first step in registering a phone number as 3PID. Next,
    /// call [`Account::add_3pid()`] with the same `client_secret` and the
    /// returned `sid`.
    ///
    /// # Arguments
    ///
    /// * `client_secret` - A client-generated secret string used to protect
    /// this session.
    ///
    /// * `country` - The two-letter uppercase ISO-3166-1 alpha-2 country code
    /// that the number in phone_number should be parsed as if it were dialled
    /// from.
    ///
    /// * `phone_number` - The phone number to validate.
    ///
    /// * `send_attempt` - The attempt number. This number needs to be
    /// incremented if you want to request another token for the same
    /// validation.
    ///
    /// # Returns
    ///
    /// * `sid` - The session ID to be used in following requests for this 3PID.
    ///
    /// * `submit_url` - If present, the user will submit the token to the
    /// client, that must send it to this URL. If not, the client will not be
    /// involved in the token submission.
    ///
    /// This method might return an [`ErrorKind::ThreepidInUse`] error if the
    /// phone number is already registered for this account or another, or an
    /// [`ErrorKind::ThreepidDenied`] error if it is denied.
    ///
    /// # Example
    /// ```no_run
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::{ClientSecret, uint};
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// # let account = client.account();
    /// # let secret = ClientSecret::parse("secret")?;
    /// let token_response = account.request_3pid_msisdn_token(
    ///     &secret,
    ///     "FR",
    ///     "0123456789",
    ///     uint!(0),
    /// ).await?;
    ///
    /// // Wait for the user to confirm that the token was submitted or prompt
    /// // the user for the token and send it to submit_url.
    ///
    /// let uiaa_response = account.add_3pid(
    ///     &secret,
    ///     &token_response.sid,
    ///     None
    /// ).await;
    ///
    /// // Proceed with UIAA.
    ///
    /// # anyhow::Ok(()) });
    /// ```
    /// [3pid]: https://spec.matrix.org/v1.2/appendices/#3pid-types
    /// [`ErrorKind::ThreepidInUse`]: ruma::api::client::error::ErrorKind::ThreepidInUse
    /// [`ErrorKind::ThreepidDenied`]: ruma::api::client::error::ErrorKind::ThreepidDenied
    pub async fn request_3pid_msisdn_token(
        &self,
        client_secret: &ClientSecret,
        country: &str,
        phone_number: &str,
        send_attempt: UInt,
    ) -> Result<request_3pid_management_token_via_msisdn::v3::Response> {
        let request = request_3pid_management_token_via_msisdn::v3::Request::new(
            client_secret,
            country,
            phone_number,
            send_attempt,
        );
        Ok(self.client.send(request, None).await?)
    }

    /// Add a [Third Party Identifier][3pid] on the homeserver for this
    /// account.
    ///
    /// This 3PID may be used by the homeserver to authenticate the user
    /// during sensitive operations.
    ///
    /// This method should be called after
    /// [`Account::request_3pid_email_token()`] or
    /// [`Account::request_3pid_msisdn_token()`] to complete the 3PID
    ///
    /// # Arguments
    ///
    /// * `client_secret` - The same client secret used in
    /// [`Account::request_3pid_email_token()`] or
    /// [`Account::request_3pid_msisdn_token()`].
    ///
    /// * `sid` - The session ID returned in
    /// [`Account::request_3pid_email_token()`] or
    /// [`Account::request_3pid_msisdn_token()`].
    ///
    /// * `auth_data` - This request uses the [User-Interactive Authentication
    /// API][uiaa]. The first request needs to set this to `None` and will
    /// always fail with an [`UiaaResponse`]. The response will contain
    /// information for the interactive auth and the same request needs to be
    /// made but this time with some `auth_data` provided.
    ///
    /// [3pid]: https://spec.matrix.org/v1.2/appendices/#3pid-types
    /// [uiaa]: https://spec.matrix.org/v1.2/client-server-api/#user-interactive-authentication-api
    /// [`UiaaResponse`]: ruma::api::client::uiaa::UiaaResponse
    pub async fn add_3pid(
        &self,
        client_secret: &ClientSecret,
        sid: &SessionId,
        auth_data: Option<AuthData<'_>>,
    ) -> Result<add_3pid::v3::Response> {
        let request = assign!(add_3pid::v3::Request::new(client_secret, sid), {
            auth: auth_data,
        });
        Ok(self.client.send(request, None).await?)
    }

    /// Delete a [Third Party Identifier][3pid] from the homeserver for this
    /// account.
    ///
    /// # Arguments
    ///
    /// * `address` - The 3PID being removed.
    ///
    /// * `medium` - The type of the 3PID.
    ///
    /// * `id_server` - The identity server to unbind from. If not provided, the
    /// homeserver should unbind the 3PID from the identity server it was bound
    /// to previously.
    ///
    /// # Returns
    ///
    /// * [`ThirdPartyIdRemovalStatus::Success`] if the 3PID was also unbound
    /// from the identity server.
    ///
    /// * [`ThirdPartyIdRemovalStatus::NoSupport`] if the 3PID was not unbound
    /// from the identity server. This can also mean that the 3PID was not bound
    /// to an identity server in the first place.
    ///
    /// # Example
    /// ```no_run
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::thirdparty::Medium;
    /// # use matrix_sdk::ruma::api::client::account::ThirdPartyIdRemovalStatus;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// # let account = client.account();
    /// match account
    ///         .delete_3pid("paul@matrix.org", Medium::Email, None)
    ///         .await?
    ///         .id_server_unbind_result
    ///     {
    ///         ThirdPartyIdRemovalStatus::Success => {
    ///             println!("3PID unbound from the Identity Server");
    ///         }
    ///         _ => println!("Could not unbind 3PID from the Identity Server"),
    ///     }
    ///
    /// # anyhow::Ok(()) });
    /// ```
    /// [3pid]: https://spec.matrix.org/v1.2/appendices/#3pid-types
    /// [`ThirdPartyIdRemovalStatus::Success`]: ruma::api::client::account::ThirdPartyIdRemovalStatus::Success
    /// [`ThirdPartyIdRemovalStatus::NoSupport`]: ruma::api::client::account::ThirdPartyIdRemovalStatus::NoSupport
    pub async fn delete_3pid(
        &self,
        address: &str,
        medium: Medium,
        id_server: Option<&str>,
    ) -> Result<delete_3pid::v3::Response> {
        let request = assign!(delete_3pid::v3::Request::new(medium, address), {
            id_server: id_server,
        });
        Ok(self.client.send(request, None).await?)
    }
}
