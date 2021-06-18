use std::{cmp::min, convert::TryFrom, ops::Deref, sync::Arc};

use matrix_sdk_base::deserialized_responses::{MembersResponse, SyncRoomEvent};
use matrix_sdk_common::locks::Mutex;
use ruma::{
    api::client::r0::{
        context::get_context,
        media::{get_content, get_content_thumbnail},
        membership::{get_member_events, join_room_by_id, leave_room},
        message::{get_message_events, get_message_events::Direction},
    },
    events::AnyRoomEvent,
    serde::Raw,
    EventId, UserId,
};

use crate::{BaseRoom, Client, Result, RoomMember, UInt};

/// A struct containing methods that are common for Joined, Invited and Left
/// Rooms
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
    /// * `room` - The underlying room.
    pub fn new(client: Client, room: BaseRoom) -> Self {
        // TODO: Make this private
        Self { inner: room, client }
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
        let _response = self.client.send(request, None).await?;

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
                    get_content_thumbnail::Request::from_url(&url, width.into(), height.into())?;
                let response = self.client.send(request, None).await?;
                Ok(Some(response.file))
            } else {
                let request = get_content::Request::from_url(&url)?;
                let response = self.client.send(request, None).await?;
                Ok(Some(response.file))
            }
        } else {
            Ok(None)
        }
    }

    /// Gets a slice of the timeline of this room
    ///
    /// Returns a slice of the timeline between `start` and `end`, no longer
    /// then `limit`. If the number of events is fewer then `limit` it means
    /// that in the given direction no more events exist.
    /// If the timeline doesn't contain an event with the given `start` `None`
    /// is returned.
    ///
    /// # Arguments
    ///
    /// * `start` - An `EventId` that indicates the start of the slice.
    ///
    /// * `end` - An `EventId` that indicates the end of the slice.
    ///
    /// * `limit` - The maximum number of events that should be returned.
    ///
    /// * `direction` - The direction of the search and returned events.
    ///
    /// # Examples
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// use matrix_sdk::Client;
    /// # use matrix_sdk::identifiers::{event_id, room_id};
    /// # use matrix_sdk::api::r0::message::get_message_events::Direction;
    /// # use url::Url;
    ///
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// let room_id = room_id!("!roomid:example.com");
    ///
    /// let mut client = Client::new(homeserver).unwrap();
    /// # let room = client
    /// #    .get_joined_room(&room_id)
    /// #    .unwrap();
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// assert!(room.messages(&event_id!("$xxxxxx:example.org"), None, 10, Direction::Backward).await.is_ok());
    /// # });
    /// ```
    pub async fn messages(
        &self,
        start: &EventId,
        end: Option<&EventId>,
        limit: usize,
        direction: Direction,
    ) -> Result<Option<Vec<SyncRoomEvent>>> {
        let room_id = self.inner.room_id();
        let events = if let Some(mut stored) = self
            .client
            .store()
            .get_timeline(room_id, Some(start), end, Some(limit), direction.clone())
            .await?
        {
            // We found a gab or the end of the stored timeline.
            if let Some(token) = stored.token {
                let mut request = get_message_events::Request::new(
                    self.inner.room_id(),
                    &token,
                    direction.clone(),
                );
                request.limit =
                    UInt::try_from((limit - stored.events.len()) as u64).unwrap_or(UInt::MAX);

                let response = self.client.send(request, None).await?;

                // FIXME: we may recevied an invalied server response that ruma considers valid
                // See https://github.com/ruma/ruma/issues/644
                if response.end.is_none() && response.start.is_none() {
                    return Ok(Some(stored.events));
                }

                let response_events = self
                    .client
                    .base_client
                    .receive_messages(room_id, &direction, &response)
                    .await?;

                let mut response_events = if let Some(end) = end {
                    if let Some(position) =
                        response_events.iter().position(|event| &event.event_id() == end)
                    {
                        response_events.into_iter().take(position + 1).collect()
                    } else {
                        response_events
                    }
                } else {
                    response_events
                };

                match direction {
                    Direction::Forward => {
                        response_events.append(&mut stored.events);
                        stored.events = response_events;
                    }
                    Direction::Backward => stored.events.append(&mut response_events),
                }
            }
            stored.events
        } else {
            // Fallback to context API because we don't know the start event
            let mut request = get_context::Request::new(room_id, start);

            // We need to take limit twice because the context api returns events before
            // and after the given event
            request.limit = UInt::try_from((limit * 2) as u64).unwrap_or(UInt::MAX);

            let mut context = self.client.send(request, None).await?;

            let event = if let Some(event) = context.event {
                event
            } else {
                return Ok(None);
            };

            let mut response = get_message_events::Response::new();
            response.start = context.start;
            response.end = context.end;
            let before_length = context.events_before.len();
            let after_length = context.events_after.len();
            let mut events: Vec<Raw<AnyRoomEvent>> =
                context.events_after.into_iter().rev().collect();
            events.push(event);
            events.append(&mut context.events_before);
            response.chunk = events;
            response.state = context.state;
            let response_events = self
                .client
                .base_client
                .receive_messages(room_id, &Direction::Backward, &response)
                .await?;

            let response_events: Vec<SyncRoomEvent> = match direction {
                Direction::Forward => {
                    let lower_bound = if before_length > limit { before_length - limit } else { 0 };
                    response_events[lower_bound..=before_length].to_vec()
                }
                Direction::Backward => response_events
                    [after_length..min(response_events.len(), after_length + limit)]
                    .to_vec(),
            };

            if let Some(end) = end {
                if let Some(position) =
                    response_events.iter().position(|event| &event.event_id() == end)
                {
                    response_events.into_iter().take(position + 1).collect()
                } else {
                    response_events
                }
            } else {
                response_events
            }
        };

        Ok(Some(events))
    }

    pub(crate) async fn request_members(&self) -> Result<Option<MembersResponse>> {
        #[allow(clippy::map_clone)]
        if let Some(mutex) =
            self.client.members_request_locks.get(self.inner.room_id()).map(|m| m.clone())
        {
            mutex.lock().await;

            Ok(None)
        } else {
            let mutex = Arc::new(Mutex::new(()));
            self.client.members_request_locks.insert(self.inner.room_id().clone(), mutex.clone());

            let _guard = mutex.lock().await;

            let request = get_member_events::Request::new(self.inner.room_id());
            let response = self.client.send(request, None).await?;

            let response =
                self.client.base_client.receive_members(self.inner.room_id(), &response).await?;

            self.client.members_request_locks.remove(self.inner.room_id());

            Ok(Some(response))
        }
    }

    async fn ensure_members(&self) -> Result<()> {
        if !self.are_members_synced() {
            self.request_members().await?;
        }

        Ok(())
    }

    /// Sync the member list with the server.
    ///
    /// This method will de-duplicate requests if it is called multiple times in
    /// quick succession, in that case the return value will be `None`.
    pub async fn sync_members(&self) -> Result<Option<MembersResponse>> {
        self.request_members().await
    }

    /// Get active members for this room, includes invited, joined members.
    ///
    /// *Note*: This method will fetch the members from the homeserver if the
    /// member list isn't synchronized due to member lazy loading. Because of
    /// that, it might panic if it isn't run on a tokio thread.
    ///
    /// Use [active_members_no_sync()](#method.active_members_no_sync) if you
    /// want a method that doesn't do any requests.
    pub async fn active_members(&self) -> Result<Vec<RoomMember>> {
        self.ensure_members().await?;
        self.active_members_no_sync().await
    }

    /// Get active members for this room, includes invited, joined members.
    ///
    /// *Note*: This method will fetch the members from the homeserver if the
    /// member list isn't synchronized due to member lazy loading. Because of
    /// that, it might panic if it isn't run on a tokio thread.
    ///
    /// Use [active_members()](#method.active_members) if you want to ensure to
    /// always get the full member list.
    pub async fn active_members_no_sync(&self) -> Result<Vec<RoomMember>> {
        Ok(self
            .inner
            .active_members()
            .await?
            .into_iter()
            .map(|member| RoomMember::new(self.client.clone(), member))
            .collect())
    }

    /// Get all the joined members of this room.
    ///
    /// *Note*: This method will fetch the members from the homeserver if the
    /// member list isn't synchronized due to member lazy loading. Because of
    /// that it might panic if it isn't run on a tokio thread.
    ///
    /// Use [joined_members_no_sync()](#method.joined_members_no_sync) if you
    /// want a method that doesn't do any requests.
    pub async fn joined_members(&self) -> Result<Vec<RoomMember>> {
        self.ensure_members().await?;
        self.joined_members_no_sync().await
    }

    /// Get all the joined members of this room.
    ///
    /// *Note*: This method will not fetch the members from the homeserver if
    /// the member list isn't synchronized due to member lazy loading. Thus,
    /// members could be missing from the list.
    ///
    /// Use [joined_members()](#method.joined_members) if you want to ensure to
    /// always get the full member list.
    pub async fn joined_members_no_sync(&self) -> Result<Vec<RoomMember>> {
        Ok(self
            .inner
            .joined_members()
            .await?
            .into_iter()
            .map(|member| RoomMember::new(self.client.clone(), member))
            .collect())
    }

    /// Get a specific member of this room.
    ///
    /// *Note*: This method will fetch the members from the homeserver if the
    /// member list isn't synchronized due to member lazy loading. Because of
    /// that it might panic if it isn't run on a tokio thread.
    ///
    /// Use [get_member_no_sync()](#method.get_member_no_sync) if you want a
    /// method that doesn't do any requests.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The ID of the user that should be fetched out of the
    /// store.
    pub async fn get_member(&self, user_id: &UserId) -> Result<Option<RoomMember>> {
        self.ensure_members().await?;
        self.get_member_no_sync(user_id).await
    }

    /// Get a specific member of this room.
    ///
    /// *Note*: This method will not fetch the members from the homeserver if
    /// the member list isn't synchronized due to member lazy loading. Thus,
    /// members could be missing.
    ///
    /// Use [get_member()](#method.get_member) if you want to ensure to always
    /// have the full member list to chose from.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The ID of the user that should be fetched out of the
    /// store.
    pub async fn get_member_no_sync(&self, user_id: &UserId) -> Result<Option<RoomMember>> {
        Ok(self
            .inner
            .get_member(user_id)
            .await?
            .map(|member| RoomMember::new(self.client.clone(), member)))
    }

    /// Get all members for this room, includes invited, joined and left
    /// members.
    ///
    /// *Note*: This method will fetch the members from the homeserver if the
    /// member list isn't synchronized due to member lazy loading. Because of
    /// that it might panic if it isn't run on a tokio thread.
    ///
    /// Use [members_no_sync()](#method.members_no_sync) if you want a
    /// method that doesn't do any requests.
    pub async fn members(&self) -> Result<Vec<RoomMember>> {
        self.ensure_members().await?;
        self.members_no_sync().await
    }

    /// Get all members for this room, includes invited, joined and left
    /// members.
    ///
    /// *Note*: This method will not fetch the members from the homeserver if
    /// the member list isn't synchronized due to member lazy loading. Thus,
    /// members could be missing.
    ///
    /// Use [members()](#method.members) if you want to ensure to always get
    /// the full member list.
    pub async fn members_no_sync(&self) -> Result<Vec<RoomMember>> {
        Ok(self
            .inner
            .members()
            .await?
            .into_iter()
            .map(|member| RoomMember::new(self.client.clone(), member))
            .collect())
    }
}
