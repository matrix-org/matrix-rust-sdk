use std::ops::Deref;

use super::Joined;
use crate::{room::Common, BaseRoom, Client, Result, RoomState};

/// A room in the left state.
///
/// This struct contains all methods specific to a `Room` with
/// `RoomState::Left`. Operations may fail once the underlying `Room` changes
/// `RoomState`.
#[derive(Debug, Clone)]
pub struct Left {
    pub(crate) inner: Common,
}

impl Left {
    /// Create a new `room::Left` if the underlying `Room` has
    /// `RoomState::Left`.
    ///
    /// # Arguments
    /// * `client` - The client used to make requests.
    ///
    /// * `room` - The underlying room.
    pub(crate) fn new(client: &Client, room: BaseRoom) -> Option<Self> {
        if room.state() == RoomState::Left {
            Some(Self { inner: Common::new(client.clone(), room) })
        } else {
            None
        }
    }

    /// Join this room.
    pub async fn join(&self) -> Result<Joined> {
        self.inner.join().await
    }
}

impl Deref for Left {
    type Target = Common;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
