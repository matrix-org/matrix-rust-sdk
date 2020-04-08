# Matrix Rust SDK

## Design and Layout

#### Async Client
The highest level structure that ties the other pieces of functionality together. The client is responsible for the Request/Response cycle. It can be thought of as a thin layer atop the `BaseClient` passing requests along for the `BaseClient` to handle. A user should be able to write their own `AsyncClient` using the `BaseClient`. It knows how to 
  - login
  - send messages
  - encryption ...
  - sync client state with the server
  - make raw Http requests

#### Base Client/Client State Machine
In addition to Http the `AsyncClient` passes along methods from the `BaseClient` that deal with `Room`s and `RoomMember`s. This allows the client to keep track of more complicated information that needs to be calculated in some way.
  - human readable room names
  - power level?
  - ignored list?
  - push rulesset?
  - more?

#### Crypto State Machine
Given a Matrix response the crypto machine will update it's internal state, along with encryption information this means keeping track of when to encrypt. It has knowledge of when encryption needs to happen and can be asked from the `BaseClient`. The crypto state machine is given responses that relate to encryption and can create encrypted request bodies for encryption related requests. Basically it tells the `BaseClient` to send a to-device messages out and the `BaseClient` is responsible for notifying the crypto state machine when it sent the message so crypto can update state.

#### Client State/Room and RoomMember
The `BaseClient` is responsible for keeping state in sync through the `IncomingResponse`s of `AsyncClient` or querying the `StateStore`. By processing and then delegating incoming `RoomEvent`s, `StateEvent`s, `PresenceEvent`, `IncomingAccountData` and `EphemeralEvent`s to the correct `Room` in the base clients `HashMap<RoomId, Room>` or further to `Room`'s `RoomMember` via the members `HashMap<UserId, RoomMember>`. The `BaseClient` is also responsible for emitting the incoming events to the `EventEmitter` trait.

```rust
/// A Matrix room.
pub struct Room {
    /// The unique id of the room.
    pub room_id: RoomId,
    /// The name of the room, clients use this to represent a room.
    pub room_name: RoomName,
    /// The mxid of our own user.
    pub own_user_id: UserId,
    /// The mxid of the room creator.
    pub creator: Option<UserId>,
    /// The map of room members.
    pub members: HashMap<UserId, RoomMember>,
    /// A list of users that are currently typing.
    pub typing_users: Vec<UserId>,
    /// The power level requirements for specific actions in this room
    pub power_levels: Option<PowerLevels>,
    // TODO when encryption events are handled we store algorithm used and rotation time.
    /// A flag indicating if the room is encrypted.
    pub encrypted: bool,
    /// Number of unread notifications with highlight flag set.
    pub unread_highlight: Option<UInt>,
    /// Number of unread notifications.
    pub unread_notifications: Option<UInt>,
}
```

```rust
pub struct RoomMember {
    /// The unique mxid of the user.
    pub user_id: UserId,
    /// The human readable name of the user.
    pub display_name: Option<String>,
    /// The matrix url of the users avatar.
    pub avatar_url: Option<String>,
    /// The time, in ms, since the user interacted with the server.
    pub last_active_ago: Option<UInt>,
    /// If the user should be considered active.
    pub currently_active: Option<bool>,
    /// The unique id of the room.
    pub room_id: Option<String>,
    /// If the member is typing.
    pub typing: Option<bool>,
    /// The presence of the user, if found.
    pub presence: Option<PresenceState>,
    /// The presence status message, if found.
    pub status_msg: Option<String>,
    /// The users power level.
    pub power_level: Option<Int>,
    /// The normalized power level of this `RoomMember` (0-100).
    pub power_level_norm: Option<Int>,
    /// The `MembershipState` of this `RoomMember`.
    pub membership: MembershipState,
    /// The human readable name of this room member.
    pub name: String,
    /// The events that created the state of this room member.
    pub events: Vec<Event>,
    /// The `PresenceEvent`s connected to this user.
    pub presence_events: Vec<PresenceEvent>,
}
```

#### State Store
The `BaseClient` also has access to a `dyn StateStore` this is an abstraction around a "database" to keep client state without requesting a full sync from the server on start up. A default implementation that serializes/deserializes json to files in a specified directory can be used. The user can also implement `StateStore` to fit any storage solution they choose.
  - load
  - store/save
  - update ??

#### Event Emitter
The consumer of this crate can implement the `EventEmitter` trait for full control over how incoming events are handled by their client. If that isn't enough it is possible to receive every incoming response with the `AsyncClient::sync_forever` callback.
  - list the methods for `EventEmitter`?
