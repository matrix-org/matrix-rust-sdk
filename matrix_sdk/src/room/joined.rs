use crate::{room::Common, Client, Room, RoomType};
use std::ops::Deref;

/// A room in the joined state.
///
/// The `JoinedRoom` contains all methodes specific to a `Room` with type `RoomType::Joined`.
/// Operations may fail once the underlaying `Room` changes `RoomType`.
#[derive(Debug, Clone)]
pub struct Joined {
    inner: Common,
}

impl Deref for Joined {
    type Target = Common;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Joined {
    /// Create a new `room::Joined` if the underlaying `Room` has type `RoomType::Joined`.
    ///
    /// # Arguments
    /// * `client` - The client used to make requests.
    ///
    /// * `room` - The underlaying room.
    pub fn new(client: Client, room: Room) -> Option<Self> {
        if room.room_type() == RoomType::Joined {
            Some(Self {
                inner: Common::new(client, room),
            })
        } else {
            None
        }
    }
}
