//! Matrix identifiers.

use std::{hash::Hash, sync::LazyLock};

use indexmap::Equivalent;
use ruma::{
    OwnedUserId, UserId,
    events::direct::{DirectUserIdentifier, OwnedDirectUserIdentifier},
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
#[derive(Debug)]
pub enum User {
    /// `@alice:server.name`
    Alice,
    /// `@bob:other.server`
    Bob,
    /// `@carol:other.server`
    Carol,
}

/// Alice's user ID.
static ALICE_USER_ID: LazyLock<OwnedUserId> = LazyLock::new(|| crate::ALICE.to_owned());
/// Bob's user ID.
static BOB_USER_ID: LazyLock<OwnedUserId> = LazyLock::new(|| crate::BOB.to_owned());
/// Carol's user ID.
static CAROL_USER_ID: LazyLock<OwnedUserId> = LazyLock::new(|| crate::CAROL.to_owned());

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
