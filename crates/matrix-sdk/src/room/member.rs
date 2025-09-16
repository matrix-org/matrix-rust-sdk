use std::ops::Deref;

use ruma::{
    events::room::{MediaSource, power_levels::UserPowerLevel},
    int,
};

use crate::{
    BaseRoomMember, Client, Result,
    media::{MediaFormat, MediaRequestParameters},
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
    ///     Client, RoomMemberships, media::MediaFormat, room::RoomMember,
    ///     ruma::room_id,
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
        let request = MediaRequestParameters { source: MediaSource::Plain(url.to_owned()), format };
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
    /// The member is a creator.
    ///
    /// A creator has an infinite power level and cannot be demoted, so this
    /// role is immutable. A room can have several creators.
    ///
    /// It is available in room versions where
    /// `explicitly_privilege_room_creators` in [`AuthorizationRules`] is set to
    /// `true`.
    ///
    /// [`AuthorizationRules`]: ruma::room_version_rules::AuthorizationRules
    Creator,
    /// The member is an administrator.
    Administrator,
    /// The member is a moderator.
    Moderator,
    /// The member is a regular user.
    User,
}

impl RoomMemberRole {
    /// Creates the suggested role for a given power level.
    pub fn suggested_role_for_power_level(power_level: UserPowerLevel) -> Self {
        match power_level {
            UserPowerLevel::Infinite => RoomMemberRole::Creator,
            UserPowerLevel::Int(value) => {
                if value >= int!(100) {
                    Self::Administrator
                } else if value >= int!(50) {
                    Self::Moderator
                } else {
                    Self::User
                }
            }
            // This branch is only necessary because the enum is non-exhaustive.
            // TODO: Use the `non_exhaustive_omitted_patterns` lint when it becomes stable to be
            // warned when a variant is added.
            // Tracking issue: https://github.com/rust-lang/rust/issues/89554
            _ => unimplemented!(),
        }
    }

    /// Get the suggested power level for this role.
    pub fn suggested_power_level(&self) -> UserPowerLevel {
        match self {
            Self::Creator => UserPowerLevel::Infinite,
            Self::Administrator => UserPowerLevel::Int(int!(100)),
            Self::Moderator => UserPowerLevel::Int(int!(50)),
            Self::User => UserPowerLevel::Int(int!(0)),
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
            RoomMemberRole::suggested_role_for_power_level(int!(100).into())
        );
        assert_eq!(
            RoomMemberRole::Moderator,
            RoomMemberRole::suggested_role_for_power_level(int!(50).into())
        );
        assert_eq!(
            RoomMemberRole::User,
            RoomMemberRole::suggested_role_for_power_level(int!(0).into())
        );
    }

    #[test]
    fn test_unexpected_power_levels() {
        assert_eq!(
            RoomMemberRole::Administrator,
            RoomMemberRole::suggested_role_for_power_level(int!(200).into())
        );
        assert_eq!(
            RoomMemberRole::Administrator,
            RoomMemberRole::suggested_role_for_power_level(int!(101).into())
        );
        assert_eq!(
            RoomMemberRole::Moderator,
            RoomMemberRole::suggested_role_for_power_level(int!(99).into())
        );
        assert_eq!(
            RoomMemberRole::Moderator,
            RoomMemberRole::suggested_role_for_power_level(int!(51).into())
        );
        assert_eq!(
            RoomMemberRole::User,
            RoomMemberRole::suggested_role_for_power_level(int!(-1).into())
        );
        assert_eq!(
            RoomMemberRole::User,
            RoomMemberRole::suggested_role_for_power_level(int!(-100).into())
        );
    }

    #[test]
    fn test_default_power_levels() {
        assert_eq!(int!(100), RoomMemberRole::Administrator.suggested_power_level());
        assert_eq!(int!(50), RoomMemberRole::Moderator.suggested_power_level());
        assert_eq!(int!(0), RoomMemberRole::User.suggested_power_level());

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
