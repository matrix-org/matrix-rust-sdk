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
    /// # Examples
    ///
    /// ```no_run
    /// use matrix_sdk::{
    ///     media::MediaFormat, room::RoomMember, ruma::room_id, Client,
    ///     RoomMemberships,
    /// };
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # async {
    /// # let user = "example";
    /// let client = Client::new(homeserver).await.unwrap();
    /// client.matrix_auth().login_username(user, "password").send().await.unwrap();
    /// let room_id = room_id!("!roomid:example.com");
    /// let room = client.get_room(&room_id).unwrap();
    /// let members = room.members(RoomMemberships::empty()).await.unwrap();
    /// let member = members.first().unwrap();
    /// if let Some(avatar) = member.avatar(MediaFormat::File).await.unwrap() {
    ///     std::fs::write("avatar.png", avatar);
    /// }
    /// # };
    /// ```
    pub async fn avatar(&self, format: MediaFormat) -> Result<Option<Vec<u8>>> {
        let Some(url) = self.avatar_url() else { return Ok(None) };
        let request = MediaRequest { source: MediaSource::Plain(url.to_owned()), format };
        Ok(Some(self.client.media().get_media_content(&request, true).await?))
    }

    /// Adds the room member to the current account data's ignore list
    /// which will ignore the user across all rooms.
    pub async fn ignore(&self) -> Result<()> {
        self.client.account().ignore_user(self.inner.user_id()).await
    }

    /// Removes the room member from the current account data's ignore list
    /// which will unignore the user across all rooms.
    pub async fn unignore(&self) -> Result<()> {
        self.client.account().unignore_user(self.inner.user_id()).await
    }

    /// Returns true if the member of the room is the user of the account
    pub fn is_account_user(&self) -> bool {
        match self.client.user_id() {
            Some(id) => id == self.inner.user_id(),
            None => false,
        }
    }
}
