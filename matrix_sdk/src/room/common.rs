use matrix_sdk_common::api::r0::{
    membership::{get_member_events, join_room_by_id, leave_room},
    message::get_message_events,
};
use std::ops::Deref;

use crate::{BaseRoom, Client, Result, RoomMember};

/// A struct containing methodes that are common for Joined, Invited and Left Rooms
#[derive(Debug, Clone)]
pub struct Common {
    inner: BaseRoom,
    pub(crate) client: Client,
}

impl Deref for Common {
    type Target = BaseRoom;

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
    pub fn new(client: Client, room: BaseRoom) -> Self {
        // TODO: Make this private
        Self {
            inner: room,
            client,
        }
    }

    /// Leave this room.
    ///
    /// Only invited and joined rooms can be left
    pub(crate) async fn leave(&self) -> Result<()> {
        let request = leave_room::Request::new(self.inner.room_id());
        let _response = self.client.send(request, None).await?;

        Ok(())
    }

    /// Join this room.
    ///
    /// Only invited and left rooms can be joined via this method
    pub(crate) async fn join(&self) -> Result<()> {
        let request = join_room_by_id::Request::new(self.inner.room_id());
        let _resposne = self.client.send(request, None).await?;

        Ok(())
    }

    /// Sends a request to `/_matrix/client/r0/rooms/{room_id}/messages` and returns
    /// a `get_message_events::Response` that contains a chunk of room and state events
    /// (`AnyRoomEvent` and `AnyStateEvent`).
    ///
    /// # Arguments
    ///
    /// * `request` - The easiest way to create this request is using the
    /// `get_message_events::Request` itself.
    ///
    /// # Examples
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// use matrix_sdk::Client;
    /// # use matrix_sdk::identifiers::room_id;
    /// # use matrix_sdk::api::r0::filter::RoomEventFilter;
    /// # use matrix_sdk::api::r0::message::get_message_events::Request as MessagesRequest;
    /// # use url::Url;
    ///
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// let room_id = room_id!("!roomid:example.com");
    /// let request = MessagesRequest::backward(&room_id, "t47429-4392820_219380_26003_2265");
    ///
    /// let mut client = Client::new(homeserver).unwrap();
    /// # let room = client
    /// #    .get_joined_room(&room_id)
    /// #    .unwrap();
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// assert!(room.messages(request).await.is_ok());
    /// # });
    /// ```
    pub async fn messages(
        &self,
        request: impl Into<get_message_events::Request<'_>>,
    ) -> Result<get_message_events::Response> {
        let request = request.into();
        self.client.send(request, None).await
    }

    pub(crate) async fn request_members(&self) -> Result<()> {
        // TODO: don't send a request if a request is being sent
        let request = get_member_events::Request::new(self.inner.room_id());
        let response = self.client.send(request, None).await?;

        self.client
            .base_client
            .receive_members(self.inner.room_id(), &response)
            .await?;

        Ok(())
    }

    /// Get active members for this room, includes invited, joined members.
    pub async fn active_members(&self) -> Result<Vec<RoomMember>> {
        if !self.are_members_synced() {
            self.request_members().await?;
        }
        Ok(self.inner.active_members().await?)
    }

    /// Get all members for this room, includes invited, joined and left members.
    pub async fn members(&self) -> Result<Vec<RoomMember>> {
        if !self.are_members_synced() {
            self.request_members().await?;
        }
        Ok(self.inner.members().await?)
    }
}
