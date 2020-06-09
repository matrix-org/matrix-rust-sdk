use crate::api;
use crate::events::room::power_levels::PowerLevelsEventContent;
use crate::events::EventJson;
use crate::identifiers::{DeviceId, RoomId, UserId};
use api::r0::account::register;
use api::r0::account::register::RegistrationKind;
use api::r0::filter::RoomEventFilter;
use api::r0::membership::Invite3pid;
use api::r0::message::get_message_events::{self, Direction};
use api::r0::room::{
    create_room::{self, CreationContent, InitialStateEvent, RoomPreset},
    Visibility,
};
use api::r0::uiaa::AuthData;

use crate::js_int::UInt;

/// A builder used to create rooms.
///
/// # Examples
/// ```
/// # use std::convert::TryFrom;
/// # use matrix_sdk::{Client, RoomBuilder};
/// # use matrix_sdk::api::r0::room::Visibility;
/// # use matrix_sdk::identifiers::UserId;
/// # use url::Url;
/// # let homeserver = Url::parse("http://example.com").unwrap();
/// # let mut rt = tokio::runtime::Runtime::new().unwrap();
/// # rt.block_on(async {
/// let mut builder = RoomBuilder::default();
/// builder.creation_content(false)
///     .initial_state(vec![])
///     .visibility(Visibility::Public)
///     .name("name")
///     .room_version("v1.0");
/// let mut client = Client::new(homeserver).unwrap();
/// client.create_room(builder).await;
/// # })
/// ```
#[derive(Clone, Debug, Default)]
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
    pub fn initial_state(&mut self, state: Vec<InitialStateEvent>) -> &mut Self {
        self.initial_state = state;
        self
    }

    /// Set the vec of `UserId`s.
    pub fn invite(&mut self, invite: Vec<UserId>) -> &mut Self {
        self.invite = invite;
        self
    }

    /// Set the vec of `Invite3pid`s.
    pub fn invite_3pid(&mut self, invite: Vec<Invite3pid>) -> &mut Self {
        self.invite_3pid = invite;
        self
    }

    /// Set the vec of `Invite3pid`s.
    pub fn is_direct(&mut self, direct: bool) -> &mut Self {
        self.is_direct = Some(direct);
        self
    }

    /// Set the room name. A `m.room.name` event will be sent to the room.
    pub fn name<S: Into<String>>(&mut self, name: S) -> &mut Self {
        self.name = Some(name.into());
        self
    }

    /// Set the room's power levels.
    pub fn power_level_override(&mut self, power: PowerLevelsEventContent) -> &mut Self {
        self.power_level_content_override = Some(power);
        self
    }

    /// Convenience for setting various default state events based on a preset.
    pub fn preset(&mut self, preset: RoomPreset) -> &mut Self {
        self.preset = Some(preset);
        self
    }

    ///  The local part of a room alias.
    pub fn room_alias_name<S: Into<String>>(&mut self, alias: S) -> &mut Self {
        self.room_alias_name = Some(alias.into());
        self
    }

    /// Room version, defaults to homeserver's version if left unspecified.
    pub fn room_version<S: Into<String>>(&mut self, version: S) -> &mut Self {
        self.room_version = Some(version.into());
        self
    }

    /// If included, a `m.room.topic` event will be sent to the room.
    pub fn topic<S: Into<String>>(&mut self, topic: S) -> &mut Self {
        self.topic = Some(topic.into());
        self
    }

    /// A public visibility indicates that the room will be shown in the published
    /// room list. A private visibility will hide the room from the published room list.
    /// Rooms default to private visibility if this key is not included.
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
            power_level_content_override: self.power_level_content_override.map(EventJson::from),
            preset: self.preset,
            room_alias_name: self.room_alias_name,
            room_version: self.room_version,
            topic: self.topic,
            visibility: self.visibility,
        }
    }
}

/// Create a builder for making get_message_event requests.
///
/// # Examples
/// ```
/// # use std::convert::TryFrom;
/// # use matrix_sdk::{Client, MessagesRequestBuilder};
/// # use matrix_sdk::api::r0::message::get_message_events::{self, Direction};
/// # use matrix_sdk::identifiers::RoomId;
/// # use url::Url;
/// # let homeserver = Url::parse("http://example.com").unwrap();
/// # let mut rt = tokio::runtime::Runtime::new().unwrap();
/// # rt.block_on(async {
/// # let room_id = RoomId::try_from("!test:localhost").unwrap();
/// # let last_sync_token = "".to_string();
/// let mut client = Client::new(homeserver).unwrap();
///
/// let mut builder = MessagesRequestBuilder::new();
/// builder.room_id(room_id)
///     .from(last_sync_token)
///     .direction(Direction::Forward);
///
/// client.room_messages(builder).await.is_err();
/// # })
/// ```
#[derive(Clone, Debug, Default)]
pub struct MessagesRequestBuilder {
    /// The room to get events from.
    room_id: Option<RoomId>,
    /// The token to start returning events from.
    ///
    /// This token can be obtained from a
    /// prev_batch token returned for each room by the sync API, or from a start or end token
    /// returned by a previous request to this endpoint.
    from: Option<String>,
    /// The token to stop returning events at.
    ///
    /// This token can be obtained from a prev_batch
    /// token returned for each room by the sync endpoint, or from a start or end token returned
    /// by a previous request to this endpoint.
    to: Option<String>,
    /// The direction to return events from.
    direction: Option<Direction>,
    /// The maximum number of events to return.
    ///
    /// Default: 10.
    limit: Option<UInt>,
    /// A filter of the returned events with.
    filter: Option<RoomEventFilter>,
}

impl MessagesRequestBuilder {
    /// Create a `MessagesRequestBuilder` builder to make a `get_message_events::Request`.
    ///
    /// The `room_id` and `from`` fields **need to be set** to create the request.
    pub fn new() -> Self {
        Self::default()
    }

    /// RoomId is required to create a `get_message_events::Request`.
    pub fn room_id(&mut self, room_id: RoomId) -> &mut Self {
        self.room_id = Some(room_id);
        self
    }

    /// A `next_batch` token or `start` or `end` from a previous `get_message_events` request.
    ///
    /// This is required to create a `get_message_events::Request`.
    pub fn from(&mut self, from: String) -> &mut Self {
        self.from = Some(from);
        self
    }

    /// A `next_batch` token or `start` or `end` from a previous `get_message_events` request.
    ///
    /// This token signals when to stop receiving events.
    pub fn to(&mut self, to: String) -> &mut Self {
        self.to = Some(to);
        self
    }

    /// The direction to return events from.
    ///
    /// If not specified `Direction::Backward` is used.
    pub fn direction(&mut self, direction: Direction) -> &mut Self {
        self.direction = Some(direction);
        self
    }

    /// The maximum number of events to return.
    pub fn limit(&mut self, limit: UInt) -> &mut Self {
        self.limit = Some(limit);
        self
    }

    /// Filter events by the given `RoomEventFilter`.
    pub fn filter(&mut self, filter: RoomEventFilter) -> &mut Self {
        self.filter = Some(filter);
        self
    }
}

impl Into<get_message_events::Request> for MessagesRequestBuilder {
    fn into(self) -> get_message_events::Request {
        get_message_events::Request {
            room_id: self.room_id.expect("`room_id` and `from` need to be set"),
            from: self.from.expect("`room_id` and `from` need to be set"),
            to: self.to,
            dir: self.direction.unwrap_or(Direction::Backward),
            limit: self.limit,
            filter: self.filter,
        }
    }
}

/// A builder used to register users.
///
/// # Examples
/// ```
/// # use std::convert::TryFrom;
/// # use matrix_sdk::{Client, RegistrationBuilder};
/// # use matrix_sdk::api::r0::account::register::RegistrationKind;
/// # use matrix_sdk::identifiers::DeviceId;
/// # use url::Url;
/// # let homeserver = Url::parse("http://example.com").unwrap();
/// # let mut rt = tokio::runtime::Runtime::new().unwrap();
/// # rt.block_on(async {
/// let mut builder = RegistrationBuilder::default();
/// builder.password("pass")
///     .username("user")
///     .kind(RegistrationKind::User);
/// let mut client = Client::new(homeserver).unwrap();
/// client.register_user(builder).await;
/// # })
/// ```
#[derive(Clone, Debug, Default)]
pub struct RegistrationBuilder {
    password: Option<String>,
    username: Option<String>,
    device_id: Option<DeviceId>,
    initial_device_display_name: Option<String>,
    auth: Option<AuthData>,
    kind: Option<RegistrationKind>,
    inhibit_login: bool,
}

impl RegistrationBuilder {
    /// Create a `RegistrationBuilder` builder to make a `register::Request`.
    ///
    /// The `room_id` and `from`` fields **need to be set** to create the request.
    pub fn new() -> Self {
        Self::default()
    }

    /// The desired password for the account.
    ///
    /// May be empty for accounts that should not be able to log in again
    /// with a password, e.g., for guest or application service accounts.
    pub fn password(&mut self, password: &str) -> &mut Self {
        self.password = Some(password.to_string());
        self
    }

    /// local part of the desired Matrix ID.
    ///
    /// If omitted, the homeserver MUST generate a Matrix ID local part.
    pub fn username(&mut self, username: &str) -> &mut Self {
        self.username = Some(username.to_string());
        self
    }

    /// ID of the client device.
    ///
    /// If this does not correspond to a known client device, a new device will be created.
    /// The server will auto-generate a device_id if this is not specified.
    pub fn device_id(&mut self, device_id: &str) -> &mut Self {
        self.device_id = Some(device_id.to_string());
        self
    }

    /// A display name to assign to the newly-created device.
    ///
    /// Ignored if `device_id` corresponds to a known device.
    pub fn initial_device_display_name(&mut self, initial_device_display_name: &str) -> &mut Self {
        self.initial_device_display_name = Some(initial_device_display_name.to_string());
        self
    }

    /// Additional authentication information for the user-interactive authentication API.
    ///
    /// Note that this information is not used to define how the registered user should be
    /// authenticated, but is instead used to authenticate the register call itself.
    /// It should be left empty, or omitted, unless an earlier call returned an response
    /// with status code 401.
    pub fn auth(&mut self, auth: AuthData) -> &mut Self {
        self.auth = Some(auth);
        self
    }

    /// Kind of account to register
    ///
    /// Defaults to `User` if omitted.
    pub fn kind(&mut self, kind: RegistrationKind) -> &mut Self {
        self.kind = Some(kind);
        self
    }

    /// If `true`, an `access_token` and `device_id` should not be returned
    /// from this call, therefore preventing an automatic login.
    pub fn inhibit_login(&mut self, inhibit_login: bool) -> &mut Self {
        self.inhibit_login = inhibit_login;
        self
    }
}

impl Into<register::Request> for RegistrationBuilder {
    fn into(self) -> register::Request {
        register::Request {
            password: self.password,
            username: self.username,
            device_id: self.device_id,
            initial_device_display_name: self.initial_device_display_name,
            auth: self.auth,
            kind: self.kind,
            inhibit_login: self.inhibit_login,
        }
    }
}

#[cfg(test)]
mod test {
    use std::collections::BTreeMap;

    use super::*;
    use crate::api::r0::filter::{LazyLoadOptions, RoomEventFilter};
    use crate::events::room::power_levels::NotificationPowerLevels;
    use crate::js_int::Int;
    use crate::{identifiers::RoomId, Client, Session};

    use mockito::{mock, Matcher};
    use std::convert::TryFrom;
    use url::Url;

    #[tokio::test]
    async fn create_room_builder() {
        let homeserver = Url::parse(&mockito::server_url()).unwrap();

        let _m = mock("POST", "/_matrix/client/r0/createRoom")
            .with_status(200)
            .with_body_from_file("../test_data/room_id.json")
            .create();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let mut builder = RoomBuilder::new();
        builder
            .creation_content(false)
            .initial_state(vec![])
            .visibility(Visibility::Public)
            .name("room_name")
            .room_version("v1.0")
            .invite_3pid(vec![])
            .is_direct(true)
            .power_level_override(PowerLevelsEventContent {
                ban: Int::MAX,
                events: BTreeMap::default(),
                events_default: Int::MIN,
                invite: Int::MIN,
                kick: Int::MIN,
                redact: Int::MAX,
                state_default: Int::MIN,
                users_default: Int::MIN,
                notifications: NotificationPowerLevels { room: Int::MIN },
                users: BTreeMap::default(),
            })
            .preset(RoomPreset::PrivateChat)
            .room_alias_name("room_alias")
            .topic("room topic")
            .visibility(Visibility::Private);
        let cli = Client::new(homeserver).unwrap();
        cli.restore_login(session).await.unwrap();
        assert!(cli.create_room(builder).await.is_ok());
    }

    #[tokio::test]
    async fn get_message_events() {
        let homeserver = Url::parse(&mockito::server_url()).unwrap();

        let _m = mock(
            "GET",
            Matcher::Regex(r"^/_matrix/client/r0/rooms/.*/messages".to_string()),
        )
        .with_status(200)
        .with_body_from_file("../test_data/room_messages.json")
        .create();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let mut builder = MessagesRequestBuilder::new();
        builder
            .room_id(RoomId::try_from("!roomid:example.com").unwrap())
            .from("t47429-4392820_219380_26003_2265".to_string())
            .to("t4357353_219380_26003_2265".to_string())
            .direction(Direction::Backward)
            .limit(UInt::new(10).unwrap())
            .filter(RoomEventFilter {
                lazy_load_options: LazyLoadOptions::Enabled {
                    include_redundant_members: false,
                },
                ..Default::default()
            });

        let cli = Client::new(homeserver).unwrap();
        cli.restore_login(session).await.unwrap();
        assert!(cli.room_messages(builder).await.is_ok());
    }
}
