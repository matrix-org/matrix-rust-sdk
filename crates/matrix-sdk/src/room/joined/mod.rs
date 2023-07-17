use std::ops::Deref;

use crate::{room::Common, BaseRoom, Client, RoomState};

/// A room in the joined state.
///
/// The `JoinedRoom` contains all methods specific to a `Room` with
/// `RoomState::Joined`. Operations may fail once the underlying `Room` changes
/// `RoomState`.
#[derive(Debug, Clone)]
pub struct Joined {
    pub(crate) inner: Common,
}

impl Deref for Joined {
    type Target = Common;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Joined {
    /// Create a new `room::Joined` if the underlying `BaseRoom` has
    /// `RoomState::Joined`.
    ///
    /// # Arguments
    /// * `client` - The client used to make requests.
    ///
    /// * `room` - The underlying room.
    pub(crate) fn new(client: &Client, room: BaseRoom) -> Option<Self> {
        if room.state() == RoomState::Joined {
            Some(Self { inner: Common::new(client.clone(), room) })
        } else {
            None
        }
    }
}
