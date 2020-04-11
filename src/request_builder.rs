use crate::events::room::power_levels::PowerLevelsEventContent;
use crate::identifiers::UserId;
use crate::api;
use api::r0::membership::Invite3pid;
use api::r0::room::{
    create_room::{self, CreationContent, InitialStateEvent, RoomPreset},
    Visibility,
};

/// A builder used to create rooms.
///
/// # Examples
/// ```
/// # use matrix_sdk::{AsyncClient, RoomBuilder};
/// # use matrix_sdk::api::r0::room::Visibility;
/// # use url::Url;
/// # let homeserver = Url::parse("http://example.com").unwrap();
/// let mut bldr = RoomBuilder::default();
/// bldr.creation_content(false)
///     .initial_state(vec![])
///     .visibility(Visibility::Public)
///     .name("name")
///     .room_version("v1.0");
/// let mut cli = AsyncClient::new(homeserver, None).unwrap();
/// # use futures::executor::block_on;
/// # block_on(async {
/// assert!(cli.create_room(bldr).await.is_err());
/// # })
/// ```
#[derive(Clone, Default)]
pub struct RoomBuilder {
    /// Extra keys to be added to the content of the `m.room.create`.
    creation_content: Option<CreationContent>,
    /// List of state events to send to the new room.
    ///
    /// Takes precedence over events set by preset, but gets overriden by
    /// name and topic keys.
    initial_state: Vec<InitialStateEvent>,
    /// A list of user IDs to invite to the room.
    ///
    /// This will tell the server to invite everyone in the list to the newly created room.
    invite: Vec<UserId>,
    /// List of third party IDs of users to invite.
    invite_3pid: Vec<Invite3pid>,
    /// If set, this sets the `is_direct` flag on room invites.
    is_direct: Option<bool>,
    /// If this is included, an `m.room.name` event will be sent into the room to indicate
    /// the name of the room.
    name: Option<String>,
    /// Power level content to override in the default power level event.
    power_level_content_override: Option<PowerLevelsEventContent>,
    /// Convenience parameter for setting various default state events based on a preset.
    preset: Option<RoomPreset>,
    /// The desired room alias local part.
    room_alias_name: Option<String>,
    /// Room version to set for the room. Defaults to homeserver's default if not specified.
    room_version: Option<String>,
    /// If this is included, an `m.room.topic` event will be sent into the room to indicate
    /// the topic for the room.
    topic: Option<String>,
    /// A public visibility indicates that the room will be shown in the published room
    /// list. A private visibility will hide the room from the published room list. Rooms
    /// default to private visibility if this key is not included.
    visibility: Option<Visibility>,
}

impl RoomBuilder {
    /// Returns an empty `RoomBuilder` for creating rooms.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the `CreationContent`.
    ///
    /// Weather users on other servers can join this room.
    pub fn creation_content(&mut self, federate: bool) -> &mut Self {
        let federate = Some(federate);
        self.creation_content = Some(CreationContent { federate });
        self
    }

    /// Set the `InitialStateEvent` vector.
    ///
    pub fn initial_state(&mut self, state: Vec<InitialStateEvent>) -> &mut Self {
        self.initial_state = state;
        self
    }

    /// Set the vec of `UserId`s.
    ///
    pub fn invite(&mut self, invite: Vec<UserId>) -> &mut Self {
        self.invite = invite;
        self
    }

    /// Set the vec of `Invite3pid`s.
    ///
    pub fn invite_3pid(&mut self, invite: Vec<Invite3pid>) -> &mut Self {
        self.invite_3pid = invite;
        self
    }

    /// Set the vec of `Invite3pid`s.
    ///
    pub fn is_direct(&mut self, direct: bool) -> &mut Self {
        self.is_direct = Some(direct);
        self
    }

    /// Set the room name. A `m.room.name` event will be sent to the room.
    ///
    pub fn name<S: Into<String>>(&mut self, name: S) -> &mut Self {
        self.name = Some(name.into());
        self
    }

    /// Set the room's power levels.
    ///
    pub fn power_level_override(&mut self, power: PowerLevelsEventContent) -> &mut Self {
        self.power_level_content_override = Some(power);
        self
    }

    /// Convenience for setting various default state events based on a preset.
    ///
    pub fn preset(&mut self, preset: RoomPreset) -> &mut Self {
        self.preset = Some(preset);
        self
    }

    ///  The local part of a room alias.
    ///
    pub fn room_alias_name<S: Into<String>>(&mut self, alias: S) -> &mut Self {
        self.room_alias_name = Some(alias.into());
        self
    }

    /// Room version, defaults to homeserver's version if left unspecified.
    ///
    pub fn room_version<S: Into<String>>(&mut self, version: S) -> &mut Self {
        self.room_version = Some(version.into());
        self
    }

    /// If included, a `m.room.topic` event will be sent to the room.
    ///
    pub fn topic<S: Into<String>>(&mut self, topic: S) -> &mut Self {
        self.topic = Some(topic.into());
        self
    }

    /// A public visibility indicates that the room will be shown in the published
    /// room list. A private visibility will hide the room from the published room list.
    /// Rooms default to private visibility if this key is not included.
    ///
    pub fn visibility(&mut self, vis: Visibility) -> &mut Self {
        self.visibility = Some(vis);
        self
    }
}

impl Into<create_room::Request> for RoomBuilder {
    fn into(self) -> create_room::Request {
        create_room::Request {
            creation_content: self.creation_content,
            initial_state: self.initial_state,
            invite: self.invite,
            invite_3pid: self.invite_3pid,
            is_direct: self.is_direct,
            name: self.name,
            power_level_content_override: self.power_level_content_override,
            preset: self.preset,
            room_alias_name: self.room_alias_name,
            room_version: self.room_version,
            topic: self.topic,
            visibility: self.visibility,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    use crate::{AsyncClient, Session};
    use mockito::mock;
    use url::Url;
    use std::convert::TryFrom;

    #[tokio::test]
    async fn create_room_builder() {
        let homeserver = Url::parse(&mockito::server_url()).unwrap();

        let _m = mock("POST", "/_matrix/client/r0/createRoom")
            .with_status(200)
            .with_body_from_file("./tests/data/room_id.json")
            .create();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let mut bldr = RoomBuilder::default();
        bldr.creation_content(false)
            .initial_state(vec![])
            .visibility(Visibility::Public)
            .name("name")
            .room_version("v1.0");
        let mut cli = AsyncClient::new(homeserver, Some(session)).unwrap();
        assert!(cli.create_room(bldr).await.is_ok());
    }
}
