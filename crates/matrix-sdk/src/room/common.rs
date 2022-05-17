use std::{collections::BTreeMap, ops::Deref, sync::Arc};

#[cfg(feature = "experimental-timeline")]
use futures_core::stream::Stream;
use matrix_sdk_base::deserialized_responses::{MembersResponse, RoomEvent};
#[cfg(feature = "experimental-timeline")]
use matrix_sdk_base::{
    deserialized_responses::{SyncRoomEvent, TimelineSlice},
    TimelineStreamError,
};
use matrix_sdk_common::locks::Mutex;
#[cfg(feature = "experimental-timeline")]
use ruma::api::client::filter::LazyLoadOptions;
#[cfg(feature = "e2e-encryption")]
use ruma::events::{
    room::encrypted::OriginalSyncRoomEncryptedEvent, AnySyncMessageLikeEvent, AnySyncRoomEvent,
    SyncMessageLikeEvent,
};
use ruma::{
    api::client::{
        config::set_global_account_data,
        filter::RoomEventFilter,
        membership::{get_member_events, join_room_by_id, leave_room},
        message::get_message_events::{self, v3::Direction},
        room::get_room_event,
        tag::{create_tag, delete_tag},
    },
    assign,
    events::{
        direct::DirectEvent,
        room::{history_visibility::HistoryVisibility, MediaSource},
        tag::{TagInfo, TagName},
        AnyRoomAccountDataEvent, AnyStateEvent, AnySyncStateEvent, GlobalAccountDataEventType,
        RedactContent, RedactedEventContent, RoomAccountDataEvent, RoomAccountDataEventContent,
        RoomAccountDataEventType, StateEventContent, StateEventType, StaticEventContent,
        SyncStateEvent,
    },
    serde::Raw,
    uint, EventId, RoomId, UInt, UserId,
};

use crate::{
    media::{MediaFormat, MediaRequest},
    room::RoomType,
    BaseRoom, Client, Error, HttpError, HttpResult, Result, RoomMember,
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
    pub start: String,

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
        let request = leave_room::v3::Request::new(self.inner.room_id());
        let _response = self.client.send(request, None).await?;

        Ok(())
    }

    /// Join this room.
    ///
    /// Only invited and left rooms can be joined via this method
    pub(crate) async fn join(&self) -> Result<()> {
        let request = join_room_by_id::v3::Request::new(self.inner.room_id());
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
    /// let client = Client::new(homeserver).await.unwrap();
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
            let request = MediaRequest { source: MediaSource::Plain(url.to_owned()), format };
            Ok(Some(self.client.get_media_content(&request, true).await?))
        } else {
            Ok(None)
        }
    }

    /// Sends a request to `/_matrix/client/r0/rooms/{room_id}/messages` and
    /// returns a `Messages` struct that contains a chunk of room and state
    /// events (`RoomEvent` and `AnyStateEvent`).
    ///
    /// With the encryption feature, messages are decrypted if possible. If
    /// decryption fails for an individual message, that message is returned
    /// undecrypted.
    ///
    /// # Examples
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// use matrix_sdk::{room::MessagesOptions, Client};
    /// # use matrix_sdk::ruma::{
    /// #     api::client::filter::RoomEventFilter,
    /// #     room_id,
    /// # };
    /// # use url::Url;
    ///
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// let request = MessagesOptions::backward("t47429-4392820_219380_26003_2265");
    ///
    /// let mut client = Client::new(homeserver).await.unwrap();
    /// let room = client
    ///    .get_joined_room(room_id!("!roomid:example.com"))
    ///    .unwrap();
    /// assert!(room.messages(request).await.is_ok());
    /// # });
    /// ```
    pub async fn messages(&self, options: MessagesOptions<'_>) -> Result<Messages> {
        let room_id = self.inner.room_id();
        let request = options.into_request(room_id);
        let http_response = self.client.send(request, None).await?;

        #[allow(unused_mut)]
        let mut response = Messages {
            start: http_response.start,
            end: http_response.end,
            #[cfg(not(feature = "e2e-encryption"))]
            chunk: http_response
                .chunk
                .into_iter()
                .map(|event| RoomEvent { event, encryption_info: None })
                .collect(),
            #[cfg(feature = "e2e-encryption")]
            chunk: Vec::with_capacity(http_response.chunk.len()),
            state: http_response.state,
        };

        #[cfg(feature = "e2e-encryption")]
        if let Some(machine) = self.client.olm_machine().await {
            for event in http_response.chunk {
                let decrypted_event = if let Ok(AnySyncRoomEvent::MessageLike(
                    AnySyncMessageLikeEvent::RoomEncrypted(SyncMessageLikeEvent::Original(
                        encrypted_event,
                    )),
                )) = event.deserialize_as::<AnySyncRoomEvent>()
                {
                    if let Ok(event) = machine.decrypt_room_event(&encrypted_event, room_id).await {
                        event
                    } else {
                        RoomEvent { event, encryption_info: None }
                    }
                } else {
                    RoomEvent { event, encryption_info: None }
                };

                response.chunk.push(decrypted_event);
            }
        } else {
            response.chunk.extend(
                http_response
                    .chunk
                    .into_iter()
                    .map(|event| RoomEvent { event, encryption_info: None }),
            );
        }

        Ok(response)
    }

    /// Get a stream for the timeline of this `Room`
    ///
    /// The first stream is forward in time and second stream is backward in
    /// time. If the `Store` used implements message caching and the
    /// timeline is cached no request to the server is made.
    ///
    /// The streams make sure that no duplicated events are returned. If the
    /// event graph changed on the server existing streams will keep the
    /// previous event order. Streams created later will use the new event
    /// graph, and therefore will create a new local cache.
    ///
    /// The forward stream will only return `None` when a gapped sync was
    /// performed. In this case also the backward stream should be dropped,
    /// to make the best use of the message cache.
    ///
    /// The backward stream returns `None` once the first event of the room was
    /// reached. The backward stream may also return an Error when a request
    /// to the server failed. If the error is persistent new streams need to
    /// be created.
    ///
    /// With the encryption feature, messages are decrypted if possible. If
    /// decryption fails for an individual message, that message is returned
    /// undecrypted.
    ///
    /// # Examples
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// use matrix_sdk::{room::MessagesOptions, Client};
    /// # use matrix_sdk::ruma::{
    /// #     api::client::filter::RoomEventFilter,
    /// #     room_id,
    /// # };
    /// # use url::Url;
    /// # use futures::StreamExt;
    /// # use futures_util::pin_mut;
    ///
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://example.com")?;
    ///
    /// let mut client = Client::new(homeserver).await?;
    ///
    /// if let Some(room) = client.get_joined_room(room_id!("!roomid:example.com")) {
    ///   let (forward_stream, backward_stream) = room.timeline().await?;
    ///
    ///   tokio::spawn(async move {
    ///       pin_mut!(backward_stream);
    ///
    ///       while let Some(item) = backward_stream.next().await {
    ///           match item {
    ///               Ok(event) => println!("{:?}", event),
    ///               Err(_) => println!("Some error occurred!"),
    ///           }
    ///       }
    ///   });
    ///
    ///   pin_mut!(forward_stream);
    ///
    ///   while let Some(event) = forward_stream.next().await {
    ///       println!("{:?}", event);
    ///   }
    /// }
    ///
    /// # Result::<_, matrix_sdk::Error>::Ok(())
    /// # });
    /// ```
    #[cfg(feature = "experimental-timeline")]
    pub async fn timeline(
        &self,
    ) -> Result<(impl Stream<Item = SyncRoomEvent>, impl Stream<Item = Result<SyncRoomEvent>>)>
    {
        let (forward_store, backward_store) = self.inner.timeline().await?;

        let room = self.to_owned();
        let backward = async_stream::stream! {
            for await item in backward_store {
                match item {
                    Ok(event) => yield Ok(event),
                    Err(TimelineStreamError::EndCache { fetch_more_token }) => if let Err(error) = room.request_messages(&fetch_more_token).await {
                        yield Err(error);
                    },
                    Err(TimelineStreamError::Store(error)) => yield Err(error.into()),
                }
            }
        };

        Ok((forward_store, backward))
    }

    /// Create a stream that returns all events of the room's timeline forward
    /// in time.
    ///
    /// The stream makes sure that no duplicated events are returned.
    ///
    /// The stream will only return `None` when a gapped sync was
    /// performed.
    ///
    /// With the encryption feature, messages are decrypted if possible. If
    /// decryption fails for an individual message, that message is returned
    /// undecrypted.
    ///
    /// If you need also a backward stream you should use
    /// [`timeline`][`crate::room::Common::timeline`]
    ///
    /// # Examples
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// use matrix_sdk::{room::MessagesOptions, Client};
    /// # use matrix_sdk::ruma::{
    /// #     api::client::filter::RoomEventFilter,
    /// #     room_id,
    /// # };
    /// # use url::Url;
    /// # use futures::StreamExt;
    /// # use futures_util::pin_mut;
    ///
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://example.com")?;
    ///
    /// let mut client = Client::new(homeserver).await?;
    ///
    /// if let Some(room) = client.get_joined_room(room_id!("!roomid:example.com")) {
    ///   let forward_stream = room.timeline_forward().await?;
    ///
    ///   pin_mut!(forward_stream);
    ///
    ///   while let Some(event) = forward_stream.next().await {
    ///       println!("{:?}", event);
    ///   }
    /// }
    ///
    /// # Result::<_, matrix_sdk::Error>::Ok(())
    /// # });
    /// ```
    #[cfg(feature = "experimental-timeline")]
    pub async fn timeline_forward(&self) -> Result<impl Stream<Item = SyncRoomEvent>> {
        Ok(self.inner.timeline_forward().await?)
    }

    /// Create a stream that returns all events of the room's timeline backward
    /// in time.
    ///
    /// If the `Store` used implements message caching and the
    /// timeline is cached no request to the server is made.
    ///
    /// The stream makes sure that no duplicated events are returned. If the
    /// event graph changed on the server existing streams will keep the
    /// previous event order. Streams created later will use the new event
    /// graph, and therefore will create a new local cache.
    ///
    /// The stream returns `None` once the first event of the room was
    /// reached. The backward stream may also return an Error when a request
    /// to the server failed. If the error is persistent a new stream needs to
    /// be created.
    ///
    /// With the encryption feature, messages are decrypted if possible. If
    /// decryption fails for an individual message, that message is returned
    /// undecrypted.
    ///
    /// If you need also a backward stream you should use
    /// [`timeline`][`crate::room::Common::timeline`]
    ///
    /// # Examples
    /// ```no_run
    /// # use std::convert::TryFrom;
    /// use matrix_sdk::{room::MessagesOptions, Client};
    /// # use matrix_sdk::ruma::{
    /// #     api::client::filter::RoomEventFilter,
    /// #     room_id,
    /// # };
    /// # use url::Url;
    /// # use futures::StreamExt;
    /// # use futures_util::pin_mut;
    ///
    /// # use futures::executor::block_on;
    /// # block_on(async {
    /// # let homeserver = Url::parse("http://example.com")?;
    ///
    /// let mut client = Client::new(homeserver).await?;
    ///
    /// if let Some(room) = client.get_joined_room(room_id!("!roomid:example.com")) {
    ///   let backward_stream = room.timeline_backward().await?;
    ///
    ///   tokio::spawn(async move {
    ///       pin_mut!(backward_stream);
    ///
    ///       while let Some(item) = backward_stream.next().await {
    ///           match item {
    ///               Ok(event) => println!("{:?}", event),
    ///               Err(_) => println!("Some error occurred!"),
    ///           }
    ///       }
    ///   });
    /// }
    ///
    /// # Result::<_, matrix_sdk::Error>::Ok(())
    /// # });
    /// ```
    #[cfg(feature = "experimental-timeline")]
    pub async fn timeline_backward(&self) -> Result<impl Stream<Item = Result<SyncRoomEvent>>> {
        let backward_store = self.inner.timeline_backward().await?;

        let room = self.to_owned();
        let backward = async_stream::stream! {
            for await item in backward_store {
                match item {
                    Ok(event) => yield Ok(event),
                    Err(TimelineStreamError::EndCache { fetch_more_token }) => if let Err(error) = room.request_messages(&fetch_more_token).await {
                        yield Err(error);
                    },
                    Err(TimelineStreamError::Store(error)) => yield Err(error.into()),
                }
            }
        };

        Ok(backward)
    }

    #[cfg(feature = "experimental-timeline")]
    async fn request_messages(&self, token: &str) -> Result<()> {
        let filter = assign!(RoomEventFilter::default(), {
            lazy_load_options: LazyLoadOptions::Enabled { include_redundant_members: false },
        });
        let options = assign!(MessagesOptions::backward(token), {
            limit: uint!(10),
            filter,
        });
        let messages = self.messages(options).await?;
        let timeline = TimelineSlice::new(
            messages.chunk.into_iter().map(SyncRoomEvent::from).collect(),
            messages.start,
            messages.end,
            false,
            false,
        );

        self.inner.add_timeline_slice(&timeline).await;
        self.client.base_client().receive_messages(self.room_id(), timeline).await?;

        Ok(())
    }

    /// Fetch the event with the given `EventId` in this room.
    pub async fn event(&self, event_id: &EventId) -> Result<RoomEvent> {
        let request = get_room_event::v3::Request::new(self.room_id(), event_id);
        let event = self.client.send(request, None).await?.event;

        #[cfg(feature = "e2e-encryption")]
        {
            if let Ok(AnySyncRoomEvent::MessageLike(AnySyncMessageLikeEvent::RoomEncrypted(
                SyncMessageLikeEvent::Original(encrypted_event),
            ))) = event.deserialize_as::<AnySyncRoomEvent>()
            {
                if let Ok(event) = self.decrypt_event(&encrypted_event).await {
                    return Ok(event);
                }
            }
            Ok(RoomEvent { event, encryption_info: None })
        }

        #[cfg(not(feature = "e2e-encryption"))]
        Ok(RoomEvent { event, encryption_info: None })
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

            let request = get_member_events::v3::Request::new(self.inner.room_id());
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
        event_type: StateEventType,
    ) -> Result<Vec<Raw<AnySyncStateEvent>>> {
        self.client.store().get_state_events(self.room_id(), event_type).await.map_err(Into::into)
    }

    /// Get all state events of a given statically-known type in this room.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # async {
    /// # let room: matrix_sdk::room::Common = todo!();
    /// use matrix_sdk::ruma::{events::room::member::SyncRoomMemberEvent, serde::Raw};
    ///
    /// let room_members: Vec<Raw<SyncRoomMemberEvent>> = room.get_state_events_static().await?;
    /// # anyhow::Ok(())
    /// # };
    /// ```
    pub async fn get_state_events_static<C>(&self) -> Result<Vec<Raw<SyncStateEvent<C>>>>
    where
        C: StaticEventContent + StateEventContent + RedactContent,
        C::Redacted: StateEventContent + RedactedEventContent,
    {
        // FIXME: Could be more efficient, if we had streaming store accessor functions
        Ok(self.get_state_events(C::TYPE.into()).await?.into_iter().map(Raw::cast).collect())
    }

    /// Get a specific state event in this room.
    pub async fn get_state_event(
        &self,
        event_type: StateEventType,
        state_key: &str,
    ) -> Result<Option<Raw<AnySyncStateEvent>>> {
        self.client
            .store()
            .get_state_event(self.room_id(), event_type, state_key)
            .await
            .map_err(Into::into)
    }

    /// Get a specific state event of statically-known type in this room.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # async {
    /// # let room: matrix_sdk::room::Common = todo!();
    /// use matrix_sdk::ruma::events::room::power_levels::SyncRoomPowerLevelsEvent;
    ///
    /// let power_levels: SyncRoomPowerLevelsEvent = room
    ///     .get_state_event_static("").await?
    ///     .expect("every room has a power_levels event")
    ///     .deserialize()?;
    /// # anyhow::Ok(())
    /// # };
    /// ```
    pub async fn get_state_event_static<C>(
        &self,
        state_key: &str,
    ) -> Result<Option<Raw<SyncStateEvent<C>>>>
    where
        C: StaticEventContent + StateEventContent + RedactContent,
        C::Redacted: StateEventContent + RedactedEventContent,
    {
        Ok(self.get_state_event(C::TYPE.into(), state_key).await?.map(Raw::cast))
    }

    /// Get account data in this room.
    pub async fn account_data(
        &self,
        data_type: RoomAccountDataEventType,
    ) -> Result<Option<Raw<AnyRoomAccountDataEvent>>> {
        self.client
            .store()
            .get_room_account_data_event(self.room_id(), data_type)
            .await
            .map_err(Into::into)
    }

    /// Get account data of statically-known type in this room.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # async {
    /// # let room: matrix_sdk::room::Common = todo!();
    /// use matrix_sdk::ruma::events::fully_read::FullyReadEventContent;
    ///
    /// match room.account_data_static::<FullyReadEventContent>().await? {
    ///     Some(fully_read) => println!("Found read marker: {:?}", fully_read.deserialize()?),
    ///     None => println!("No read marker for this room"),
    /// }
    /// # anyhow::Ok(())
    /// # };
    /// ```
    pub async fn account_data_static<C>(&self) -> Result<Option<Raw<RoomAccountDataEvent<C>>>>
    where
        C: StaticEventContent + RoomAccountDataEventContent,
    {
        Ok(self.account_data(C::TYPE.into()).await?.map(Raw::cast))
    }

    /// Check if all members of this room are verified and all their devices are
    /// verified.
    ///
    /// Returns true if all devices in the room are verified, otherwise false.
    #[cfg(feature = "e2e-encryption")]
    pub async fn contains_only_verified_devices(&self) -> Result<bool> {
        let user_ids = self.client.store().get_user_ids(self.room_id()).await?;

        for user_id in user_ids {
            let devices = self.client.encryption().get_user_devices(&user_id).await?;
            let any_unverified = devices.devices().any(|d| !d.verified());

            if any_unverified {
                return Ok(false);
            }
        }

        Ok(true)
    }

    /// Adds a tag to the room, or updates it if it already exists.
    ///
    /// Returns the [`create_tag::v3::Response`] from the server.
    ///
    /// # Arguments
    /// * `tag` - The tag to add or update.
    ///
    /// * `tag_info` - Information about the tag, generally containing the
    ///   `order` parameter.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use std::str::FromStr;
    /// # use ruma::events::tag::{TagInfo, TagName, UserTagName};
    /// # futures::executor::block_on(async {
    /// # let homeserver = url::Url::parse("http://localhost:8080")?;
    /// # let mut client = matrix_sdk::Client::new(homeserver).await?;
    /// # let room_id = matrix_sdk::ruma::room_id!("!test:localhost");
    /// use matrix_sdk::ruma::events::tag::TagInfo;
    ///
    /// if let Some(room) = client.get_joined_room(&room_id) {
    ///     let mut tag_info = TagInfo::new();
    ///     tag_info.order = Some(0.9);
    ///     let user_tag = UserTagName::from_str("u.work")?;
    ///
    ///     room.set_tag(TagName::User(user_tag), tag_info ).await?;
    /// }
    /// # Result::<_, matrix_sdk::Error>::Ok(()) });
    /// ```
    pub async fn set_tag(
        &self,
        tag: TagName,
        tag_info: TagInfo,
    ) -> HttpResult<create_tag::v3::Response> {
        let user_id = self.client.user_id().await.ok_or(HttpError::AuthenticationRequired)?;
        let request =
            create_tag::v3::Request::new(&user_id, self.inner.room_id(), tag.as_ref(), tag_info);
        self.client.send(request, None).await
    }

    /// Removes a tag from the room.
    ///
    /// Returns the [`delete_tag::v3::Response`] from the server.
    ///
    /// # Arguments
    /// * `tag` - The tag to remove.
    pub async fn remove_tag(&self, tag: TagName) -> HttpResult<delete_tag::v3::Response> {
        let user_id = self.client.user_id().await.ok_or(HttpError::AuthenticationRequired)?;
        let request = delete_tag::v3::Request::new(&user_id, self.inner.room_id(), tag.as_ref());
        self.client.send(request, None).await
    }

    /// Sets whether this room is a DM.
    ///
    /// When setting this room as DM, it will be marked as DM for all active
    /// members of the room. When unsetting this room as DM, it will be
    /// unmarked as DM for all users, not just the members.
    ///
    /// # Arguments
    /// * `is_direct` - Whether to mark this room as direct.
    pub async fn set_is_direct(&self, is_direct: bool) -> Result<()> {
        let user_id = self
            .client
            .user_id()
            .await
            .ok_or_else(|| Error::from(HttpError::AuthenticationRequired))?;

        let mut content = self
            .client
            .store()
            .get_account_data_event(GlobalAccountDataEventType::Direct)
            .await?
            .map(|e| e.deserialize_as::<DirectEvent>())
            .transpose()?
            .map(|e| e.content)
            .unwrap_or_else(|| ruma::events::direct::DirectEventContent(BTreeMap::new()));

        let this_room_id = self.inner.room_id();

        if is_direct {
            let room_members = self.active_members().await?;
            for member in room_members {
                let entry = content.entry(member.user_id().to_owned()).or_default();
                if !entry.iter().any(|room_id| room_id == this_room_id) {
                    entry.push(this_room_id.to_owned());
                }
            }
        } else {
            for (_, list) in content.iter_mut() {
                list.retain(|room_id| *room_id != this_room_id);
            }

            // Remove user ids that don't have any room marked as DM
            content.retain(|_, list| !list.is_empty());
        }

        let request = set_global_account_data::v3::Request::new(&content, &user_id)?;

        self.client.send(request, None).await?;
        Ok(())
    }

    /// Tries to decrypt a room event.
    ///
    /// # Arguments
    /// * `event` - The room event to be decrypted.
    ///
    /// Returns the decrypted event.
    #[cfg(feature = "e2e-encryption")]
    pub async fn decrypt_event(&self, event: &OriginalSyncRoomEncryptedEvent) -> Result<RoomEvent> {
        if let Some(machine) = self.client.olm_machine().await {
            Ok(machine.decrypt_room_event(event, self.inner.room_id()).await?)
        } else {
            Err(Error::NoOlmMachine)
        }
    }
}

/// Options for [`messages`][Common::messages].
///
/// See that method for details.
#[derive(Debug)]
#[non_exhaustive]
pub struct MessagesOptions<'a> {
    /// The token to start returning events from.
    ///
    /// This token can be obtained from a `prev_batch` token returned for each
    /// room from the sync API, or from a start or end token returned by a
    /// previous `messages` call.
    pub from: &'a str,

    /// The token to stop returning events at.
    ///
    /// This token can be obtained from a `prev_batch` token returned for each
    /// room by the sync API, or from a start or end token returned by a
    /// previous `messages` call.
    pub to: Option<&'a str>,

    /// The direction to return events in.
    pub dir: Direction,

    /// The maximum number of events to return.
    ///
    /// Default: 10.
    pub limit: UInt,

    /// A [`RoomEventFilter`] to filter returned events with.
    pub filter: RoomEventFilter<'a>,
}

impl<'a> MessagesOptions<'a> {
    /// Creates `MessagesOptions` with the given start token and direction.
    ///
    /// All other parameters will be defaulted.
    pub fn new(from: &'a str, dir: Direction) -> Self {
        Self { from, to: None, dir, limit: uint!(10), filter: RoomEventFilter::default() }
    }

    /// Creates `MessagesOptions` with the given start token, and `dir` set to
    /// `Backward`.
    pub fn backward(from: &'a str) -> Self {
        Self::new(from, Direction::Backward)
    }

    /// Creates `MessagesOptions` with the given start token, and `dir` set to
    /// `Forward`.
    pub fn forward(from: &'a str) -> Self {
        Self::new(from, Direction::Forward)
    }

    fn into_request(self, room_id: &'a RoomId) -> get_message_events::v3::Request<'_> {
        assign!(get_message_events::v3::Request::new(room_id, self.from, self.dir), {
            to: self.to,
            limit: self.limit,
            filter: self.filter,
        })
    }
}
