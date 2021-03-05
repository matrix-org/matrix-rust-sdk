use crate::{room::Common, Client, Room, RoomType};
use std::ops::Deref;

/// A room in the invited state.
///
/// This struct contains all methodes specific to a `Room` with type `RoomType::Invited`.
/// Operations may fail once the underlaying `Room` changes `RoomType`.
#[derive(Debug, Clone)]
pub struct Invited {
    inner: Common,
}

impl Invited {
    /// Create a new `room::Invited` if the underlaying `Room` has type `RoomType::Invited`.
    ///
    /// # Arguments
    /// * `client` - The client used to make requests.
    ///
    /// * `room` - The underlaying room.
    pub fn new(client: Client, room: Room) -> Option<Self> {
        if room.room_type() == RoomType::Invited {
            Some(Self {
                inner: Common::new(client, room),
            })
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
