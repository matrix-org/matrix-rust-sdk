use std::ops::Deref;

use matrix_sdk_common::api::r0::membership::forget_room;

use crate::{room::Common, BaseRoom, Client, Result, RoomType};

/// A room in the left state.
///
/// This struct contains all methodes specific to a `Room` with type
/// `RoomType::Left`. Operations may fail once the underlying `Room` changes
/// `RoomType`.
#[derive(Debug, Clone)]
pub struct Left {
    pub(crate) inner: Common,
}

impl Left {
    /// Create a new `room::Left` if the underlying `Room` has type
    /// `RoomType::Left`.
    ///
    /// # Arguments
    /// * `client` - The client used to make requests.
    ///
    /// * `room` - The underlying room.
    pub fn new(client: Client, room: BaseRoom) -> Option<Self> {
        // TODO: Make this private
        if room.room_type() == RoomType::Left {
            Some(Self { inner: Common::new(client, room) })
        } else {
            None
        }
    }

    /// Join this room.
    pub async fn join(&self) -> Result<()> {
        self.inner.join().await
    }

    /// Forget this room.
    ///
    /// This communicates to the homeserver that it should forget the room.
    pub async fn forget(&self) -> Result<()> {
        let request = forget_room::Request::new(self.inner.room_id());
        let _response = self.client.send(request, None).await?;

        Ok(())
    }
}

impl Deref for Left {
    type Target = Common;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
