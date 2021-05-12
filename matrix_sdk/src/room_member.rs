use std::ops::Deref;

use matrix_sdk_common::api::r0::media::{get_content, get_content_thumbnail};

use crate::{BaseRoomMember, Client, Result};

/// The high-level `RoomMember` representation
#[derive(Debug, Clone)]
pub struct RoomMember {
    inner: BaseRoomMember,
    pub(crate) client: Client,
}

impl Deref for RoomMember {
    type Target = BaseRoomMember;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl RoomMember {
    pub(crate) fn new(client: Client, member: BaseRoomMember) -> Self {
        Self {
            inner: member,
            client,
        }
    }

    /// Gets the avatar of this member, if set.
    ///
    /// Returns the avatar. No guarantee on the size of the image is given.
    /// If no size is given the full-sized avatar will be returned.
    ///
    /// # Arguments
    ///
    /// * `width` - The desired width of the avatar.
    ///
    /// * `height` - The desired height of the avatar.
    ///
    /// # Example
    /// ```no_run
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::identifiers::room_id;
    /// # use matrix_sdk::RoomMember;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # block_on(async {
    /// # let user = "example";
    /// let client = Client::new(homeserver).unwrap();
    /// client.login(user, "password", None, None).await.unwrap();
    /// let room_id = room_id!("!roomid:example.com");
    /// let room = client
    ///     .get_joined_room(&room_id)
    ///     .unwrap();
    /// let members = room.members().await.unwrap();
    /// let member = members.first().unwrap();
    /// if let Some(avatar) = member.avatar(Some(96), Some(96)).await.unwrap() {
    ///     std::fs::write("avatar.png", avatar);
    /// }
    /// # })
    /// ```
    pub async fn avatar(&self, width: Option<u32>, height: Option<u32>) -> Result<Option<Vec<u8>>> {
        // TODO: try to offer the avatar from cache, requires avatar cache
        if let Some(url) = self.avatar_url() {
            if let (Some(width), Some(height)) = (width, height) {
                let request =
                    get_content_thumbnail::Request::from_url(&url, width.into(), height.into())?;
                let response = self.client.send(request, None).await?;
                Ok(Some(response.file))
            } else {
                let request = get_content::Request::from_url(url)?;
                let response = self.client.send(request, None).await?;
                Ok(Some(response.file))
            }
        } else {
            Ok(None)
        }
    }
}
