use matrix_sdk_common::{
    api::r0::{
        media::{get_content, get_content_thumbnail},
        membership::{get_member_events, join_room_by_id, leave_room},
        message::get_message_events,
    },
    locks::Mutex,
};

use std::{ops::Deref, sync::Arc};

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

    /// Gets the avatar of this room, if set.
    ///
    /// Returns the avatar. No guarantee on the size of the image is given.
    /// If no size is given the full-sized avatar will be returned.
    ///
    /// # Arguments
    ///
    /// * `width` - The desired width of the avatar.
    ///
    /// * `height` - The desired height of the avatar.
    ///
    /// # Example
    /// ```no_run
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::identifiers::room_id;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # block_on(async {
    /// # let user = "example";
    /// let client = Client::new(homeserver).unwrap();
    /// client.login(user, "password", None, None).await.unwrap();
    /// let room_id = room_id!("!roomid:example.com");
    /// let room = client
    ///     .get_joined_room(&room_id)
    ///     .unwrap();
    /// if let Some(avatar) = room.avatar(Some(96), Some(96)).await.unwrap() {
    ///     std::fs::write("avatar.png", avatar);
    /// }
    /// # })
    /// ```
    pub async fn avatar(&self, width: Option<u32>, height: Option<u32>) -> Result<Option<Vec<u8>>> {
        // TODO: try to offer the avatar from cache, requires avatar cache
        if let Some(url) = self.avatar_url() {
            if let (Some(width), Some(height)) = (width, height) {
                let request =
                    get_content_thumbnail::Request::from_url(&url, width.into(), height.into());
                let response = self.client.send(request, None).await?;
                Ok(Some(response.file))
            } else {
                let request = get_content::Request::from_url(&url);
                let response = self.client.send(request, None).await?;
                Ok(Some(response.file))
            }
        } else {
            Ok(None)
        }
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
        #[allow(clippy::map_clone)]
        if let Some(mutex) = self
            .client
            .members_request_locks
            .get(self.inner.room_id())
            .map(|m| m.clone())
        {
            mutex.lock().await;
        } else {
            let mutex = Arc::new(Mutex::new(()));
            self.client
                .members_request_locks
                .insert(self.inner.room_id().clone(), mutex.clone());

            let _guard = mutex.lock().await;

            let request = get_member_events::Request::new(self.inner.room_id());
            let response = self.client.send(request, None).await?;

            self.client
                .base_client
                .receive_members(self.inner.room_id(), &response)
                .await?;

            self.client
                .members_request_locks
                .remove(self.inner.room_id());
        }

        Ok(())
    }

    /// Get active members for this room, includes invited, joined members.
    pub async fn active_members(&self) -> Result<Vec<RoomMember>> {
        if !self.are_members_synced() {
            self.request_members().await?;
        }
        Ok(self
            .inner
            .active_members()
            .await?
            .into_iter()
            .map(|member| RoomMember::new(self.client.clone(), member))
            .collect())
    }

    /// Get all members for this room, includes invited, joined and left members.
    pub async fn members(&self) -> Result<Vec<RoomMember>> {
        if !self.are_members_synced() {
            self.request_members().await?;
        }
        Ok(self
            .inner
            .members()
            .await?
            .into_iter()
            .map(|member| RoomMember::new(self.client.clone(), member))
            .collect())
    }
}
