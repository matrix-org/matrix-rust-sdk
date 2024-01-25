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

    /// Get the suggested role of this member based on their power level.
    pub fn suggested_role_for_power_level(&self) -> RoomMemberRole {
        RoomMemberRole::suggested_role_for_power_level(self.power_level())
    }
}

/// The role of a member in a room.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
pub enum RoomMemberRole {
    /// The member is an administrator.
    Administrator,
    /// The member is a moderator.
    Moderator,
    /// The member is a regular user.
    User,
}

impl RoomMemberRole {
    /// Creates the suggested role for a given power level.
    fn suggested_role_for_power_level(power_level: i64) -> Self {
        if power_level >= 100 {
            Self::Administrator
        } else if power_level >= 50 {
            Self::Moderator
        } else {
            Self::User
        }
    }

    /// Get the suggested power level for this role.
    pub fn suggested_power_level(&self) -> i64 {
        match self {
            Self::Administrator => 100,
            Self::Moderator => 50,
            Self::User => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_suggested_roles() {
        assert_eq!(
            RoomMemberRole::Administrator,
            RoomMemberRole::suggested_role_for_power_level(100)
        );
        assert_eq!(RoomMemberRole::Moderator, RoomMemberRole::suggested_role_for_power_level(50));
        assert_eq!(RoomMemberRole::User, RoomMemberRole::suggested_role_for_power_level(0));
    }

    #[test]
    fn test_unexpected_power_levels() {
        assert_eq!(
            RoomMemberRole::Administrator,
            RoomMemberRole::suggested_role_for_power_level(200)
        );
        assert_eq!(
            RoomMemberRole::Administrator,
            RoomMemberRole::suggested_role_for_power_level(101)
        );
        assert_eq!(RoomMemberRole::Moderator, RoomMemberRole::suggested_role_for_power_level(99));
        assert_eq!(RoomMemberRole::Moderator, RoomMemberRole::suggested_role_for_power_level(51));
        assert_eq!(RoomMemberRole::User, RoomMemberRole::suggested_role_for_power_level(-1));
        assert_eq!(RoomMemberRole::User, RoomMemberRole::suggested_role_for_power_level(-100));
    }

    #[test]
    fn test_default_power_levels() {
        assert_eq!(100, RoomMemberRole::Administrator.suggested_power_level());
        assert_eq!(50, RoomMemberRole::Moderator.suggested_power_level());
        assert_eq!(0, RoomMemberRole::User.suggested_power_level());

        assert_eq!(
            RoomMemberRole::Administrator,
            RoomMemberRole::suggested_role_for_power_level(
                RoomMemberRole::Administrator.suggested_power_level()
            )
        );
        assert_eq!(
            RoomMemberRole::Moderator,
            RoomMemberRole::suggested_role_for_power_level(
                RoomMemberRole::Moderator.suggested_power_level()
            )
        );
        assert_eq!(
            RoomMemberRole::User,
            RoomMemberRole::suggested_role_for_power_level(
                RoomMemberRole::User.suggested_power_level()
            )
        );
    }
}
