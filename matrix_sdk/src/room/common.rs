use crate::{Client, Room};
use std::ops::Deref;

/// A struct containing methodes that are common for Joined, Invited and Left Rooms
#[derive(Debug, Clone)]
pub struct Common {
    inner: Room,
    client: Client,
}

impl Deref for Common {
    type Target = Room;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl Common {
    /// Create a new `room::Common`
    ///
    /// # Arguments
    /// * `client` - The client used to make requests.
    ///
    /// * `room` - The underlaying room.
    pub fn new(client: Client, room: Room) -> Self {
        Self {
            inner: room,
            client,
        }
    }

    // TODO: add common mehtods e.g forget_room()
}
