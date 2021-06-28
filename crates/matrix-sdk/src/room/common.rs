use std::{cmp::min, convert::TryFrom, ops::Deref, sync::Arc};

use matrix_sdk_base::{
    deserialized_responses::{MembersResponse, RoomEvent, SyncRoomEvent},
    StoredTimelineSlice,
};
use matrix_sdk_common::locks::Mutex;
use ruma::{
    api::client::r0::{
        context::get_context,
        membership::{get_member_events, join_room_by_id, leave_room},
        message::{get_message_events, get_message_events::Direction},
        room::get_room_event,
    },
    events::{
        room::history_visibility::HistoryVisibility, AnyRoomEvent, AnyStateEvent,
        AnySyncStateEvent, EventType,
    },
    serde::Raw,
    EventId, UInt, UserId,
};
use tracing::trace;

use crate::{
    media::{MediaFormat, MediaRequest, MediaType},
    room::RoomType,
    BaseRoom, Client, HttpResult, Result, RoomMember,
};

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

/// The result of a `Room::messages` call.
///
/// In short, this is a possibly decrypted version of the response of a
/// `room/messages` api call.
#[derive(Debug)]
pub struct Messages {
    /// The token the pagination starts from.
    pub start: Option<String>,

    /// The token the pagination ends at.
    pub end: Option<String>,

    /// A list of room events.
    pub chunk: Vec<RoomEvent>,

    /// A list of state events relevant to showing the `chunk`.
    pub state: Vec<Raw<AnyStateEvent>>,
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
    /// Returns the avatar.
    /// If a thumbnail is requested no guarantee on the size of the image is
    /// given.
    ///
    /// # Arguments
    ///
    /// * `format` - The desired format of the avatar.
    ///
    /// # Example
    /// ```no_run
    /// # use futures::executor::block_on;
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::room_id;
    /// # use matrix_sdk::media::MediaFormat;
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
    /// if let Some(avatar) = room.avatar(MediaFormat::File).await.unwrap() {
    ///     std::fs::write("avatar.png", avatar);
    /// }
    /// # })
    /// ```
    pub async fn avatar(&self, format: MediaFormat) -> Result<Option<Vec<u8>>> {
        if let Some(url) = self.avatar_url() {
            let request = MediaRequest { media_type: MediaType::Uri(url.clone()), format };
            Ok(Some(self.client.get_media_content(&request, true).await?))
        } else {
            Ok(None)
        }
    }

    /// Sends a request to `/_matrix/client/r0/rooms/{room_id}/messages` and
    /// returns a `get_message_events::Response` that contains a chunk of
    /// room and state events (`AnyRoomEvent` and `AnyStateEvent`).
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
    /// # use matrix_sdk::ruma::room_id;
    /// # use matrix_sdk::ruma::api::client::r0::{
    /// #     filter::RoomEventFilter,
    /// #     message::get_message_events::Request as MessagesRequest,
    /// # };
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
    /// assert!(room.request_messages(request).await.is_ok());
    /// # });
    /// ```
    pub async fn request_messages(
        &self,
        request: impl Into<get_message_events::Request<'_>>,
    ) -> HttpResult<get_message_events::Response> {
        let request = request.into();
        self.client.send(request, None).await
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
    /// * `start` - An `EventId` that indicates the start of the slice. If
    ///   `None` the most recent
    /// events in the given direction are returned.
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
    /// # use matrix_sdk::ruma::{event_id, room_id};
    /// # use matrix_sdk::ruma::api::client::r0::{
    /// #     filter::RoomEventFilter,
    /// #     message::get_message_events::{Direction, Request as MessagesRequest},
    /// # };
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
    /// assert!(room.messages(Some(&event_id!("$xxxxxx:example.org")), None, 10, Direction::Backward).await.is_ok());
    /// # });
    /// ```
    pub async fn messages(
        &self,
        start: Option<&EventId>,
        end: Option<&EventId>,
        limit: u32,
        direction: Direction,
    ) -> Result<Option<Vec<SyncRoomEvent>>> {
        let room_id = self.inner.room_id();
        if let Some(stored) = self
            .client
            .store()
            .get_timeline(room_id, start, end, Some(limit as usize), direction.clone())
            .await?
        {
            // We found a gap or the end of the stored timeline.
            Ok(Some(self.request_missing_messages(stored, end, limit, &direction).await?))
        } else {
            // The start event wasn't found in the store fallback to the context api.
            self.request_missing_messages_via_context(start, end, limit, &direction).await
        }
    }

    fn context_to_message_response(
        &self,
        mut context: get_context::Response,
    ) -> Option<get_message_events::Response> {
        let mut response = get_message_events::Response::new();
        response.start = context.start;
        response.end = context.end;

        let mut events: Vec<Raw<AnyRoomEvent>> = context.events_after.into_iter().rev().collect();
        events.push(context.event?);
        events.append(&mut context.events_before);

        response.chunk = events;
        response.state = context.state;

        Some(response)
    }

    async fn request_missing_messages(
        &self,
        mut stored: StoredTimelineSlice,
        end: Option<&EventId>,
        limit: u32,
        direction: &Direction,
    ) -> Result<Vec<SyncRoomEvent>> {
        if let Some(token) = stored.token {
            trace!("A gap in the stored timeline was found. Request messages from token {}", token);

            let room_id = self.inner.room_id();
            let mut request = get_message_events::Request::new(room_id, &token, direction.clone());
            // The number of events found in the store is never more then `limit`.
            request.limit = UInt::from(limit - stored.events.len() as u32);

            let response = self.request_messages(request).await?;

            // FIXME: we may received an invalied server response that ruma considers valid
            // See https://github.com/ruma/ruma/issues/644
            if response.end.is_none() && response.start.is_none() {
                return Ok(stored.events);
            }

            let response_events =
                self.client.base_client().receive_messages(room_id, direction, &response).await?;

            // If our end event is part of our event list make sure we don't return past the
            // end, otherwise return the whole list.
            let mut response_events = if let Some(end) = end {
                if let Some(position) = response_events
                    .iter()
                    .position(|event| event.event_id().map_or(false, |event_id| &event_id == end))
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
        Ok(stored.events)
    }

    async fn request_missing_messages_via_context(
        &self,
        start: Option<&EventId>,
        end: Option<&EventId>,
        limit: u32,
        direction: &Direction,
    ) -> Result<Option<Vec<SyncRoomEvent>>> {
        let start = if let Some(start) = start {
            start
        } else {
            // If no start event was given it's not possible to find the context of it.
            return Ok(None);
        };

        trace!("The start event with id {} wasn't found in the stored timeline. Fallback to the context API", start);

        let room_id = self.inner.room_id();
        let mut request = get_context::Request::new(room_id, start);

        // We need to take limit twice because the context api returns events before
        // and after the given event
        request.limit = UInt::try_from(limit as u64 * 2u64).unwrap_or(UInt::MAX);

        let context = self.client.send(request, None).await?;

        let limit = limit as usize;
        let before_length = context.events_before.len();
        let after_length = context.events_after.len();
        let response = if let Some(response) = self.context_to_message_response(context) {
            response
        } else {
            return Ok(None);
        };
        let response_events = self
            .client
            .base_client()
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

        let result = if let Some(end) = end {
            if let Some(position) = response_events
                .iter()
                .position(|event| event.event_id().map_or(false, |event_id| &event_id == end))
            {
                response_events.into_iter().take(position + 1).collect()
            } else {
                response_events
            }
        } else {
            response_events
        };

        Ok(Some(result))
    }

    /// Fetch the event with the given `EventId` in this room.
    pub async fn event(&self, event_id: &EventId) -> Result<RoomEvent> {
        let request = get_room_event::Request::new(self.room_id(), event_id);

        let event = self.client.send(request, None).await?.event.deserialize()?;

        #[cfg(feature = "encryption")]
        return Ok(self.client.decrypt_room_event(&event).await);

        #[cfg(not(feature = "encryption"))]
        return Ok(RoomEvent { event: Raw::new(&event)?, encryption_info: None });
    }

    pub(crate) async fn request_members(&self) -> Result<Option<MembersResponse>> {
        if let Some(mutex) =
            self.client.inner.members_request_locks.get(self.inner.room_id()).map(|m| m.clone())
        {
            mutex.lock().await;

            Ok(None)
        } else {
            let mutex = Arc::new(Mutex::new(()));
            self.client
                .inner
                .members_request_locks
                .insert(self.inner.room_id().to_owned(), mutex.clone());

            let _guard = mutex.lock().await;

            let request = get_member_events::Request::new(self.inner.room_id());
            let response = self.client.send(request, None).await?;

            let response =
                self.client.base_client().receive_members(self.inner.room_id(), &response).await?;

            self.client.inner.members_request_locks.remove(self.inner.room_id());

            Ok(Some(response))
        }
    }

    async fn ensure_members(&self) -> Result<()> {
        if !self.are_events_visible() {
            return Ok(());
        }

        if !self.are_members_synced() {
            self.request_members().await?;
        }

        Ok(())
    }

    fn are_events_visible(&self) -> bool {
        if let RoomType::Invited = self.inner.room_type() {
            return matches!(
                self.inner.history_visibility(),
                HistoryVisibility::WorldReadable | HistoryVisibility::Invited
            );
        }

        true
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
    /// *Note*: This method will not fetch the members from the homeserver if
    /// the member list isn't synchronized due to member lazy loading. Thus,
    /// members could be missing from the list.
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

    /// Get all state events of a given type in this room.
    pub async fn get_state_events(
        &self,
        event_type: EventType,
    ) -> Result<Vec<Raw<AnySyncStateEvent>>> {
        self.client.store().get_state_events(self.room_id(), event_type).await.map_err(Into::into)
    }

    /// Get a specific state event in this room.
    pub async fn get_state_event(
        &self,
        event_type: EventType,
        state_key: &str,
    ) -> Result<Option<Raw<AnySyncStateEvent>>> {
        self.client
            .store()
            .get_state_event(self.room_id(), event_type, state_key)
            .await
            .map_err(Into::into)
    }

    /// Check if all members of this room are verified and all their devices are
    /// verified.
    ///
    /// Returns true if all devices in the room are verified, otherwise false.
    #[cfg(feature = "encryption")]
    pub async fn contains_only_verified_devices(&self) -> Result<bool> {
        let user_ids = self.client.store().get_user_ids(self.room_id()).await?;

        for user_id in user_ids {
            let devices = self.client.get_user_devices(&user_id).await?;
            let any_unverified = devices.devices().any(|d| !d.verified());

            if any_unverified {
                return Ok(false);
            }
        }

        Ok(true)
    }
}
