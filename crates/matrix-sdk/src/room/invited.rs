use std::ops::Deref;

use crate::{room::Common, BaseRoom, Client, RoomState};

/// A room in the invited state.
///
/// This struct contains all methods specific to a `Room` with
/// `RoomState::Invited`. Operations may fail once the underlying `Room` changes
/// `RoomState`.
#[derive(Debug, Clone)]
pub struct Invited {
    pub(crate) inner: Common,
}

impl Invited {
    /// Create a new `room::Invited` if the underlying `Room` has
    /// `RoomState::Invited`.
    ///
    /// # Arguments
    /// * `client` - The client used to make requests.
    ///
    /// * `room` - The underlying room.
    pub(crate) fn new(client: &Client, room: BaseRoom) -> Option<Self> {
        if room.state() == RoomState::Invited {
            Some(Self { inner: Common::new(client.clone(), room) })
        } else {
            None
        }
    }
}

impl Deref for Invited {
    type Target = Common;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
