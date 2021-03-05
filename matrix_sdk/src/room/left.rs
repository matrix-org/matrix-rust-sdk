use crate::{room::Common, Client, Room, RoomType};
use std::ops::Deref;

/// A room in the left state.
///
/// This struct contains all methodes specific to a `Room` with type `RoomType::Left`.
/// Operations may fail once the underlaying `Room` changes `RoomType`.
#[derive(Debug, Clone)]
pub struct Left {
    inner: Common,
}

impl Left {
    /// Create a new `room::Left` if the underlaying `Room` has type `RoomType::Left`.
    ///
    /// # Arguments
    /// * `client` - The client used to make requests.
    ///
    /// * `room` - The underlaying room.
    pub fn new(client: Client, room: Room) -> Option<Self> {
        if room.room_type() == RoomType::Left {
            Some(Self {
                inner: Common::new(client, room),
            })
        } else {
            None
        }
    }
}

impl Deref for Left {
    type Target = Common;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
