use matrix_sdk_common::{
    api::r0::{
        account::register::{self, RegistrationKind},
        directory::get_public_rooms_filtered::{self, Filter, RoomNetwork},
        filter::RoomEventFilter,
        membership::Invite3pid,
        message::get_message_events::{self, Direction},
        room::{
            create_room::{self, CreationContent, InitialStateEvent, RoomPreset},
            Visibility,
        },
        uiaa::AuthData,
    },
    events::{room::power_levels::PowerLevelsEventContent, EventJson},
    identifiers::{DeviceId, RoomId, UserId},
    js_int::UInt,
};

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
/// builder.creation_content(false, None)
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
    creation_content: Option<CreationContent>,
    initial_state: Vec<InitialStateEvent>,
    invite: Vec<UserId>,
    invite_3pid: Vec<Invite3pid>,
    is_direct: Option<bool>,
    name: Option<String>,
    power_level_content_override: Option<PowerLevelsEventContent>,
    preset: Option<RoomPreset>,
    room_alias_name: Option<String>,
    room_version: Option<String>,
    topic: Option<String>,
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
    pub fn creation_content(
        &mut self,
        federate: bool,
        predecessor: Option<PreviousRoom>,
    ) -> &mut Self {
        self.creation_content = Some(CreationContent {
            federate,
            predecessor,
        });
        self
    }

    /// Sets a list of state events to send to the new room.
    ///
    /// Takes precedence over events set by preset, but gets overriden by
    /// name and topic keys.
    pub fn initial_state(&mut self, state: Vec<InitialStateEvent>) -> &mut Self {
        self.initial_state = state;
        self
    }

    /// Sets a list of user IDs to invite to the room.
    ///
    /// This will tell the server to invite everyone in the list to the newly created room.
    pub fn invite(&mut self, invite: Vec<UserId>) -> &mut Self {
        self.invite = invite;
        self
    }

    /// Sets a list of third party IDs of users to invite.
    pub fn invite_3pid(&mut self, invite: Vec<Invite3pid>) -> &mut Self {
        self.invite_3pid = invite;
        self
    }

    /// If set, this sets the `is_direct` flag on room invites.
    pub fn is_direct(&mut self, direct: bool) -> &mut Self {
        self.is_direct = Some(direct);
        self
    }

    /// If this is included, an `m.room.name` event will be sent into the room to indicate
    /// the name of the room.
    pub fn name<S: Into<String>>(&mut self, name: S) -> &mut Self {
        self.name = Some(name.into());
        self
    }

    /// Power level content to override in the default power level event.
    pub fn power_level_override(&mut self, power: PowerLevelsEventContent) -> &mut Self {
        self.power_level_content_override = Some(power);
        self
    }

    /// Convenience parameter for setting various default state events based on a preset.
    pub fn preset(&mut self, preset: RoomPreset) -> &mut Self {
        self.preset = Some(preset);
        self
    }

    /// The desired room alias local part.
    pub fn room_alias_name<S: Into<String>>(&mut self, alias: S) -> &mut Self {
        self.room_alias_name = Some(alias.into());
        self
    }

    /// Room version to set for the room. Defaults to homeserver's default if not specified.
    pub fn room_version<S: Into<String>>(&mut self, version: S) -> &mut Self {
        self.room_version = Some(version.into());
        self
    }

    /// If this is included, an `m.room.topic` event will be sent into the room to indicate
    /// the topic for the room.
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
/// let mut builder = MessagesRequestBuilder::new(
///     RoomId::try_from("!roomid:example.com").unwrap(),
///     "t47429-4392820_219380_26003_2265".to_string(),
/// );
///
/// builder.to("t4357353_219380_26003_2265".to_string())
///     .direction(Direction::Backward)
///     .limit(10);
///
/// client.room_messages(builder).await.is_err();
/// # })
/// ```
#[derive(Clone, Debug)]
pub struct MessagesRequestBuilder {
    room_id: RoomId,
    from: String,
    to: Option<String>,
    direction: Option<Direction>,
    limit: Option<u32>,
    filter: Option<RoomEventFilter>,
}

impl MessagesRequestBuilder {
    /// Create a `MessagesRequestBuilder` builder to make a `get_message_events::Request`.
    ///
    /// # Arguments
    ///
    /// * `room_id` -  The id of the room that is being requested.
    ///
    /// * `from` - The token to start returning events from. This token can be obtained from
    /// a `prev_batch` token from a sync response, or a start or end token from a previous request
    /// to this endpoint.
    pub fn new(room_id: RoomId, from: String) -> Self {
        Self {
            room_id,
            from,
            to: None,
            direction: None,
            limit: None,
            filter: None,
        }
    }

    /// A `next_batch` token or `start` or `end` from a previous `get_message_events` request.
    ///
    /// This token signals when to stop receiving events.
    pub fn to<S: Into<String>>(&mut self, to: S) -> &mut Self {
        self.to = Some(to.into());
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
    ///
    /// The default is 10.
    pub fn limit(&mut self, limit: u32) -> &mut Self {
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
            room_id: self.room_id,
            from: self.from,
            to: self.to,
            dir: self.direction.unwrap_or(Direction::Backward),
            limit: self.limit.map(UInt::from),
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
    pub fn new() -> Self {
        Self::default()
    }

    /// The desired password for the account.
    ///
    /// May be empty for accounts that should not be able to log in again
    /// with a password, e.g., for guest or application service accounts.
    pub fn password<S: Into<String>>(&mut self, password: S) -> &mut Self {
        self.password = Some(password.into());
        self
    }

    /// local part of the desired Matrix ID.
    ///
    /// If omitted, the homeserver MUST generate a Matrix ID local part.
    pub fn username<S: Into<String>>(&mut self, username: S) -> &mut Self {
        self.username = Some(username.into());
        self
    }

    /// ID of the client device.
    ///
    /// If this does not correspond to a known client device, a new device will be created.
    /// The server will auto-generate a device_id if this is not specified.
    pub fn device_id<S: Into<String>>(&mut self, device_id: S) -> &mut Self {
        self.device_id = Some(device_id.into());
        self
    }

    /// A display name to assign to the newly-created device.
    ///
    /// Ignored if `device_id` corresponds to a known device.
    pub fn initial_device_display_name<S: Into<String>>(
        &mut self,
        initial_device_display_name: S,
    ) -> &mut Self {
        self.initial_device_display_name = Some(initial_device_display_name.into());
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

/// Create a builder for making get_public_rooms_filtered requests.
///
/// # Examples
/// ```
/// # use std::convert::TryFrom;
/// # use matrix_sdk::{Client, RoomListFilterBuilder};
/// # use matrix_sdk::api::r0::directory::get_public_rooms_filtered::{self, RoomNetwork, Filter};
/// # use url::Url;
/// # let homeserver = Url::parse("http://example.com").unwrap();
/// # let mut rt = tokio::runtime::Runtime::new().unwrap();
/// # rt.block_on(async {
/// # let last_sync_token = "".to_string();
/// let mut client = Client::new(homeserver).unwrap();
///
/// let generic_search_term = Some("matrix-rust-sdk".to_string());
/// let mut builder = RoomListFilterBuilder::new();
/// builder
///     .filter(Filter { generic_search_term, })
///     .since(last_sync_token)
///     .room_network(RoomNetwork::Matrix);
///
/// client.public_rooms_filtered(builder).await.is_err();
/// # })
/// ```
#[derive(Clone, Debug, Default)]
pub struct RoomListFilterBuilder {
    server: Option<String>,
    limit: Option<u32>,
    since: Option<String>,
    filter: Option<Filter>,
    room_network: Option<RoomNetwork>,
}

impl RoomListFilterBuilder {
    /// Create a `RoomListFilterBuilder` builder to make a `get_message_events::Request`.
    pub fn new() -> Self {
        Self::default()
    }

    /// The server to fetch the public room lists from.
    ///
    /// `None` means the server this request is sent to.
    pub fn server<S: Into<String>>(&mut self, server: S) -> &mut Self {
        self.server = Some(server.into());
        self
    }

    /// Limit for the number of results to return.
    pub fn limit(&mut self, limit: u32) -> &mut Self {
        self.limit = Some(limit);
        self
    }

    /// Pagination token from a previous request.
    pub fn since<S: Into<String>>(&mut self, since: S) -> &mut Self {
        self.since = Some(since.into());
        self
    }

    /// Filter to apply to the results.
    pub fn filter(&mut self, filter: Filter) -> &mut Self {
        self.filter = Some(filter);
        self
    }

    /// Network to fetch the public room lists from.
    ///
    /// Defaults to the Matrix network.
    pub fn room_network(&mut self, room_network: RoomNetwork) -> &mut Self {
        self.room_network = Some(room_network);
        self
    }
}

impl Into<get_public_rooms_filtered::Request> for RoomListFilterBuilder {
    fn into(self) -> get_public_rooms_filtered::Request {
        get_public_rooms_filtered::Request {
            room_network: self.room_network.unwrap_or_default(),
            limit: self.limit.map(UInt::from),
            server: self.server,
            since: self.since,
            filter: self.filter,
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

    use matrix_sdk_test::test_json;
    use mockito::{mock, Matcher};
    use std::convert::TryFrom;
    use url::Url;

    #[tokio::test]
    async fn create_room_builder() {
        let homeserver = Url::parse(&mockito::server_url()).unwrap();

        let _m = mock("POST", "/_matrix/client/r0/createRoom")
            .with_status(200)
            .with_body(test_json::ROOM_ID.to_string())
            .create();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let mut builder = RoomBuilder::new();
        builder
            .creation_content(false, None)
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
        .with_body(test_json::ROOM_MESSAGES.to_string())
        .create();

        let session = Session {
            access_token: "1234".to_owned(),
            user_id: UserId::try_from("@example:localhost").unwrap(),
            device_id: "DEVICEID".to_owned(),
        };

        let mut builder = MessagesRequestBuilder::new(
            RoomId::try_from("!roomid:example.com").unwrap(),
            "t47429-4392820_219380_26003_2265".to_string(),
        );
        builder
            .to("t4357353_219380_26003_2265".to_string())
            .direction(Direction::Backward)
            .limit(10)
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
