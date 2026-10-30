//! Matrix identifiers.

use std::{fmt, hash::Hash, sync::LazyLock};

use indexmap::Equivalent;
use ruma::{
    OwnedRoomId, OwnedUserId, RoomId, UserId,
    events::direct::{DirectUserIdentifier, OwnedDirectUserIdentifier},
    owned_user_id,
};
use serde::Serialize;

/// A Matrix user.
///
/// This can be converted to the following types with `.into()`:
///
/// - `&'static UserId`
/// - `OwnedUserId`
/// - `&'static DirectUserIdentifier`
/// - `OwnedDirectUserIdentifier`
///
/// It implements `PartialEq` with all the ID types above, as well as with
/// `serde_json::Value` to use with the `assert_eq!` macro.
///
/// It implements `Serialize` and `Into<String>` to use it with the `json!`
/// macro.
///
/// It implements `Hash` and `Equivalent<OwnedUserId>` as well to be able to use
/// it with methods like `IndexMap::get()` or `IndexMap::contains_key()`.
///
/// It can also be converted to a borrowed string with `.as_str()` to use it
/// with methods like `HashMap::get()` or `HashSet::contains()`.
#[derive(Debug, Clone, Copy)]
pub enum User {
    /// `@alice:server.name`
    Alice,
    /// `@bob:other.server`
    Bob,
    /// `@carol:other.server`
    Carol,
}

/// Alice's user ID.
static ALICE_USER_ID: LazyLock<OwnedUserId> =
    LazyLock::new(|| owned_user_id!("@alice:server.name"));
/// Bob's user ID.
static BOB_USER_ID: LazyLock<OwnedUserId> = LazyLock::new(|| owned_user_id!("@bob:other.server"));
/// Carol's user ID.
static CAROL_USER_ID: LazyLock<OwnedUserId> =
    LazyLock::new(|| owned_user_id!("@carol:other.server"));

impl User {
    /// The user's ID.
    ///
    /// This unconventional return type allows to either clone the owned ID or
    /// dereference it.
    fn id(&self) -> &'static OwnedUserId {
        match self {
            User::Alice => &ALICE_USER_ID,
            User::Bob => &BOB_USER_ID,
            User::Carol => &CAROL_USER_ID,
        }
    }

    /// The user's ID as a `&str`.
    pub fn as_str(&self) -> &'static str {
        self.id().as_str()
    }
}

impl From<User> for OwnedUserId {
    fn from(user: User) -> Self {
        user.id().clone()
    }
}

impl From<User> for &'static UserId {
    fn from(user: User) -> Self {
        user.id()
    }
}

impl From<User> for OwnedDirectUserIdentifier {
    fn from(user: User) -> Self {
        user.id().clone().into()
    }
}

impl From<User> for &'static DirectUserIdentifier {
    fn from(user: User) -> Self {
        <&UserId>::from(user).into()
    }
}

impl From<User> for String {
    fn from(user: User) -> Self {
        user.id().as_str().to_owned()
    }
}

impl PartialEq<OwnedUserId> for User {
    fn eq(&self, other: &OwnedUserId) -> bool {
        self.id().eq(other)
    }
}

impl PartialEq<User> for OwnedUserId {
    fn eq(&self, other: &User) -> bool {
        self.eq(other.id())
    }
}

impl PartialEq<&OwnedUserId> for User {
    fn eq(&self, other: &&OwnedUserId) -> bool {
        self.id().eq(*other)
    }
}

impl PartialEq<User> for &OwnedUserId {
    fn eq(&self, other: &User) -> bool {
        (*self).eq(other.id())
    }
}

impl PartialEq<&UserId> for User {
    fn eq(&self, other: &&UserId) -> bool {
        self.id().eq(other)
    }
}

impl PartialEq<User> for &UserId {
    fn eq(&self, other: &User) -> bool {
        self.eq(other.id())
    }
}

impl PartialEq<serde_json::Value> for User {
    fn eq(&self, other: &serde_json::Value) -> bool {
        self.id().as_str().eq(other)
    }
}

impl PartialEq<User> for serde_json::Value {
    fn eq(&self, other: &User) -> bool {
        self.eq(other.id().as_str())
    }
}

impl Equivalent<OwnedUserId> for User {
    fn equivalent(&self, key: &OwnedUserId) -> bool {
        key.eq(self.id())
    }
}

impl Hash for User {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id().hash(state);
    }
}

impl Serialize for User {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.id().serialize(serializer)
    }
}

impl fmt::Display for User {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.id().fmt(f)
    }
}

/// A Matrix test room.
///
/// This can be converted to the following types with `.into()`:
///
/// - `&'static RoomId`
/// - `OwnedRoomId`
///
/// It implements `PartialEq` with all the ID types above, as well as with
/// `serde_json::Value` to use with the `assert_eq!` macro.
///
/// It implements `Serialize` and `Into<String>` to use it with the `json!`
/// macro.
///
/// It implements `Hash` and `Equivalent<OwnedUserId>` as well to be able to use
/// it with methods like `IndexMap::get()` or `IndexMap::contains_key()`.
///
/// It can also be converted to a borrowed string with `.as_str()` to use it
/// with methods like `HashMap::get()` or `HashSet::contains()`.
#[derive(Debug, Default, Clone, Copy)]
pub enum TestRoom {
    /// The default room ID, `!SVkFJHzfwvuaIEawgC:localhost`.
    #[default]
    Default,
}

/// Default room ID.
static DEFAULT_ROOM_ID: LazyLock<OwnedRoomId> =
    LazyLock::new(|| crate::DEFAULT_TEST_ROOM_ID.to_owned());

impl TestRoom {
    /// The room's ID.
    ///
    /// This unconventional return type allows to either clone the owned ID or
    /// dereference it.
    fn id(&self) -> &'static OwnedRoomId {
        match self {
            Self::Default => &DEFAULT_ROOM_ID,
        }
    }

    /// The room's ID as a `&str`.
    pub fn as_str(&self) -> &'static str {
        self.id().as_str()
    }
}

impl From<TestRoom> for OwnedRoomId {
    fn from(room: TestRoom) -> Self {
        room.id().clone()
    }
}

impl From<TestRoom> for &'static RoomId {
    fn from(room: TestRoom) -> Self {
        room.id()
    }
}

impl From<TestRoom> for String {
    fn from(room: TestRoom) -> Self {
        room.as_str().to_owned()
    }
}

impl PartialEq<OwnedRoomId> for TestRoom {
    fn eq(&self, other: &OwnedRoomId) -> bool {
        self.id().eq(other)
    }
}

impl PartialEq<TestRoom> for OwnedRoomId {
    fn eq(&self, other: &TestRoom) -> bool {
        self.eq(other.id())
    }
}

impl PartialEq<&OwnedRoomId> for TestRoom {
    fn eq(&self, other: &&OwnedRoomId) -> bool {
        self.id().eq(*other)
    }
}

impl PartialEq<TestRoom> for &OwnedRoomId {
    fn eq(&self, other: &TestRoom) -> bool {
        (*self).eq(other.id())
    }
}

impl PartialEq<&RoomId> for TestRoom {
    fn eq(&self, other: &&RoomId) -> bool {
        self.id().eq(other)
    }
}

impl PartialEq<TestRoom> for &RoomId {
    fn eq(&self, other: &TestRoom) -> bool {
        self.eq(other.id())
    }
}

impl PartialEq<serde_json::Value> for TestRoom {
    fn eq(&self, other: &serde_json::Value) -> bool {
        self.as_str().eq(other)
    }
}

impl PartialEq<TestRoom> for serde_json::Value {
    fn eq(&self, other: &TestRoom) -> bool {
        self.eq(other.as_str())
    }
}

impl Equivalent<OwnedRoomId> for TestRoom {
    fn equivalent(&self, key: &OwnedRoomId) -> bool {
        key.eq(self.id())
    }
}

impl Hash for TestRoom {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id().hash(state);
    }
}

impl Serialize for TestRoom {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.id().serialize(serializer)
    }
}

impl fmt::Display for TestRoom {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.id().fmt(f)
    }
}
