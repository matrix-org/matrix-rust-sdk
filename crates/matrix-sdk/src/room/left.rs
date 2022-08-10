use std::ops::Deref;

use ruma::api::client::membership::forget_room;

use crate::{
    room::{Common, Joined},
    BaseRoom, Client, Result, RoomType,
};

/// A room in the left state.
///
/// This struct contains all methods specific to a `Room` with type
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
    #[doc = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/docs/sync_running.md"))]
    pub async fn join(&self) -> Result<Joined> {
        self.inner.join().await
    }

    /// Forget this room.
    ///
    /// This communicates to the homeserver that it should forget the room.
    pub async fn forget(&self) -> Result<()> {
        let request = forget_room::v3::Request::new(self.inner.room_id());
        let _response = self.client.send(request, None).await?;
        self.client.store().remove_room(self.inner.room_id()).await?;

        Ok(())
    }
}

impl Deref for Left {
    type Target = Common;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
