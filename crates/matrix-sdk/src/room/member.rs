use std::ops::Deref;

use ruma::events::room::MediaSource;

use crate::{
    media::{MediaFormat, MediaRequest},
    BaseRoomMember, Client, Result,
};

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
        Self { inner: member, client }
    }

    /// Gets the avatar of this member, if set.
    ///
    /// Returns the avatar.
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
    /// # use matrix_sdk::{
    /// #     media::MediaFormat, room::RoomMember, ruma::room_id, Client,
    /// # };
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # block_on(async {
    /// # let user = "example";
    /// let client = Client::new(homeserver).await.unwrap();
    /// client.login(user, "password", None, None).await.unwrap();
    /// let room_id = room_id!("!roomid:example.com");
    /// let room = client.get_joined_room(&room_id).unwrap();
    /// let members = room.members().await.unwrap();
    /// let member = members.first().unwrap();
    /// if let Some(avatar) = member.avatar(MediaFormat::File).await.unwrap() {
    ///     std::fs::write("avatar.png", avatar);
    /// }
    /// # })
    /// ```
    pub async fn avatar(&self, format: MediaFormat) -> Result<Option<Vec<u8>>> {
        if let Some(url) = self.avatar_url() {
            let request = MediaRequest { source: MediaSource::Plain(url.to_owned()), format };
            Ok(Some(self.client.get_media_content(&request, true).await?))
        } else {
            Ok(None)
        }
    }
}
