use std::io::Read;

use matrix_sdk_base::media::{MediaFormat, MediaRequest, MediaType};
use mime::Mime;
use ruma::{
    api::client::r0::profile::{
        get_avatar_url, get_display_name, set_avatar_url, set_display_name,
    },
    MxcUri,
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
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # block_on(async {
    /// let user = "example";
    /// let client = Client::new(homeserver).await.unwrap();
    /// client.login(user, "password", None, None).await.unwrap();
    ///
    /// if let Some(name) = client.account().get_display_name().await.unwrap() {
    ///     println!("Logged in as user '{}' with display name '{}'", user, name);
    /// }
    /// # })
    /// ```
    pub async fn get_display_name(&self) -> Result<Option<String>> {
        let user_id = self.client.user_id().await.ok_or(Error::AuthenticationRequired)?;
        let request = get_display_name::Request::new(&user_id);
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
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # block_on(async {
    /// let user = "example";
    /// let client = Client::new(homeserver).await.unwrap();
    /// client.login(user, "password", None, None).await.unwrap();
    ///
    /// client.account().set_display_name(Some("Alice")).await.expect("Failed setting display name");
    /// # })
    /// ```
    pub async fn set_display_name(&self, name: Option<&str>) -> Result<()> {
        let user_id = self.client.user_id().await.ok_or(Error::AuthenticationRequired)?;
        let request = set_display_name::Request::new(&user_id, name);
        self.client.send(request, None).await?;
        Ok(())
    }

    /// Get the MXC avatar url of the account, if set.
    ///
    /// # Example
    /// ```no_run
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # block_on(async {
    /// # let user = "example";
    /// let client = Client::new(homeserver).await.unwrap();
    /// client.login(user, "password", None, None).await.unwrap();
    ///
    /// if let Some(url) = client.account().get_avatar_url().await.unwrap() {
    ///     println!("Your avatar's mxc url is {}", url);
    /// }
    /// # })
    /// ```
    pub async fn get_avatar_url(&self) -> Result<Option<Box<MxcUri>>> {
        let user_id = self.client.user_id().await.ok_or(Error::AuthenticationRequired)?;
        let request = get_avatar_url::Request::new(&user_id);

        let config = Some(RequestConfig::new().force_auth());

        let response = self.client.send(request, config).await?;
        Ok(response.avatar_url)
    }

    /// Set the MXC avatar url of the account.
    ///
    /// The avatar is unset if `url` is `None`.
    pub async fn set_avatar_url(&self, url: Option<&MxcUri>) -> Result<()> {
        let user_id = self.client.user_id().await.ok_or(Error::AuthenticationRequired)?;
        let request = set_avatar_url::Request::new(&user_id, url);
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
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # block_on(async {
    /// # let user = "example";
    /// let client = Client::new(homeserver).await.unwrap();
    /// client.login(user, "password", None, None).await.unwrap();
    ///
    /// if let Some(avatar) = client.account().get_avatar(MediaFormat::File).await.unwrap() {
    ///     std::fs::write("avatar.png", avatar);
    /// }
    /// # })
    /// ```
    pub async fn get_avatar(&self, format: MediaFormat) -> Result<Option<Vec<u8>>> {
        if let Some(url) = self.get_avatar_url().await? {
            let request = MediaRequest { media_type: MediaType::Uri(url), format };
            Ok(Some(self.client.get_media_content(&request, true).await?))
        } else {
            Ok(None)
        }
    }

    /// Upload and set the account's avatar.
    ///
    /// This will upload the data produced by the reader to the homeserver's
    /// content repository, and set the user's avatar to the MXC url for the
    /// uploaded file.
    ///
    /// This is a convenience method for calling [`Client::upload()`],
    /// followed by [`set_avatar_url()`](#method.set_avatar_url).
    ///
    /// # Example
    /// ```no_run
    /// # use std::{path::Path, fs::File, io::Read};
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use url::Url;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://localhost:8080").unwrap();
    /// # let client = Client::new(homeserver).await.unwrap();
    /// let path = Path::new("/home/example/selfie.jpg");
    /// let mut image = File::open(&path).unwrap();
    ///
    /// client.account().upload_avatar(&mime::IMAGE_JPEG, &mut image).await.expect("Can't set avatar");
    /// # })
    /// ```
    pub async fn upload_avatar<R: Read>(&self, content_type: &Mime, reader: &mut R) -> Result<()> {
        let upload_response = self.client.upload(content_type, reader).await?;
        self.set_avatar_url(Some(&upload_response.content_uri)).await?;
        Ok(())
    }
}
