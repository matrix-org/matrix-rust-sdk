use std::{borrow::Borrow, collections::BTreeMap, future::Future, ops::Deref, sync::Arc};

use futures_channel::mpsc;
#[cfg(feature = "experimental-timeline")]
use futures_core::stream::Stream;
use futures_util::{SinkExt, TryStreamExt};
use matrix_sdk_base::{
    deserialized_responses::{MembersResponse, RoomEvent},
    store::StateStoreExt,
};
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
        membership::{get_member_events, leave_room},
        message::get_message_events::{self, v3::Direction},
        room::get_room_event,
        tag::{create_tag, delete_tag},
    },
    assign,
    events::{
        direct::DirectEvent,
        room::{
            history_visibility::HistoryVisibility,
            member::{MembershipState, RoomMemberEventContent},
            server_acl::RoomServerAclEventContent,
            MediaSource,
        },
        tag::{TagInfo, TagName},
        AnyRoomAccountDataEvent, AnyStateEvent, AnySyncStateEvent, EmptyStateKey,
        GlobalAccountDataEventType, RedactContent, RedactedEventContent, RoomAccountDataEvent,
        RoomAccountDataEventContent, RoomAccountDataEventType, StateEventContent, StateEventType,
        StaticEventContent, SyncStateEvent,
    },
    serde::Raw,
    uint, EventId, MatrixToUri, MatrixUri, OwnedEventId, OwnedServerName, RoomId, UInt, UserId,
};
use serde::de::DeserializeOwned;
use tracing::{debug, warn};

use crate::{
    event_handler::{EventHandler, EventHandlerHandle, EventHandlerResult, SyncEvent},
    media::{MediaFormat, MediaRequest},
    room::{Joined, Left, Room, RoomType},
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
    pub(crate) fn new(client: Client, room: BaseRoom) -> Self {
        Self { inner: room, client }
    }

    /// Leave this room.
    ///
    /// Returns a [`Result`] containing an instance of [`Left`] if successful.
    ///
    /// Only invited and joined rooms can be left
    #[doc = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/docs/sync_running.md"))]
    pub(crate) async fn leave(&self) -> Result<Left> {
        let request = leave_room::v3::Request::new(self.inner.room_id());

        let (tx, mut rx) = mpsc::channel::<Result<Left>>(1);

        let user_id = self.client.user_id().ok_or(Error::AuthenticationRequired)?.to_owned();

        let handle = self.add_event_handler({
            move |event: SyncStateEvent<RoomMemberEventContent>, room: Room| {
                let mut tx = tx.clone();
                let user_id = user_id.clone();

                async move {
                    if *event.membership() == MembershipState::Leave
                        && *event.state_key() == user_id
                    {
                        debug!("received RoomMemberEvent corresponding to requested leave");

                        let left_result = if let Room::Left(left_room) = room {
                            Ok(left_room)
                        } else {
                            warn!("Corresponding Room not in state: left");
                            Err(Error::InconsistentState)
                        };

                        if let Err(e) = tx.send(left_result).await {
                            debug!(
                                "Sending event from event_handler failed, \
                                 receiver not ready: {}",
                                e
                            );
                        }
                    }
                }
            }
        });

        let _guard = self.client.event_handler_drop_guard(handle);

        self.client.send(request, None).await?;

        let option = TryStreamExt::try_next(&mut rx).await?;

        Ok(option.expect("receive joined room result from event handler"))
    }

    /// Join this room.
    ///
    /// Returns a [`Result`] containing an instance of [`Joined`][room::Joined]
    /// if successful.
    /// Only invited and left rooms can be joined via this method
    #[doc = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/docs/sync_running.md"))]
    pub(crate) async fn join(&self) -> Result<Joined> {
        self.client.join_room_by_id(self.room_id()).await
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
    /// let room = client.get_joined_room(&room_id).unwrap();
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
    /// let options =
    ///     MessagesOptions::backward().from("t47429-4392820_219380_26003_2265");
    ///
    /// let mut client = Client::new(homeserver).await.unwrap();
    /// let room = client.get_joined_room(room_id!("!roomid:example.com")).unwrap();
    /// assert!(room.messages(options).await.is_ok());
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
        if let Some(machine) = self.client.olm_machine() {
            for event in http_response.chunk {
                let decrypted_event = if let Ok(AnySyncRoomEvent::MessageLike(
                    AnySyncMessageLikeEvent::RoomEncrypted(SyncMessageLikeEvent::Original(_)),
                )) = event.deserialize_as::<AnySyncRoomEvent>()
                {
                    if let Ok(event) = machine.decrypt_room_event(event.cast_ref(), room_id).await {
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

    /// Register a handler for events of a specific type, within this room.
    ///
    /// This method works the same way as [`Client::add_event_handler`], except
    /// that the handler will only be called for events within this room. See
    /// that method for more details on event handler functions.
    ///
    /// `room.add_event_handler(hdl)` is equivalent to
    /// `client.add_room_event_handler(room_id, hdl)`. Use whichever one is more
    /// convenient in your use case.
    pub fn add_event_handler<Ev, Ctx, H>(&self, handler: H) -> EventHandlerHandle
    where
        Ev: SyncEvent + DeserializeOwned + Send + 'static,
        H: EventHandler<Ev, Ctx>,
        <H::Future as Future>::Output: EventHandlerResult,
    {
        self.client.add_room_event_handler(self.room_id(), handler)
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
    /// use matrix_sdk::Client;
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
    /// if let Some(room) = client.get_joined_room(room_id!("!roomid:example.com"))
    /// {
    ///     let (forward_stream, backward_stream) = room.timeline().await?;
    ///
    ///     tokio::spawn(async move {
    ///         pin_mut!(backward_stream);
    ///
    ///         while let Some(item) = backward_stream.next().await {
    ///             match item {
    ///                 Ok(event) => println!("{:?}", event),
    ///                 Err(_) => println!("Some error occurred!"),
    ///             }
    ///         }
    ///     });
    ///
    ///     pin_mut!(forward_stream);
    ///
    ///     while let Some(event) = forward_stream.next().await {
    ///         println!("{:?}", event);
    ///     }
    /// }
    ///
    /// # anyhow::Ok(())
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
                    Err(TimelineStreamError::EndCache { fetch_more_token }) => {
                        if let Err(error) = room.request_messages(&fetch_more_token).await {
                            yield Err(error);
                        }
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
    /// use matrix_sdk::Client;
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
    /// if let Some(room) = client.get_joined_room(room_id!("!roomid:example.com"))
    /// {
    ///     let forward_stream = room.timeline_forward().await?;
    ///
    ///     pin_mut!(forward_stream);
    ///
    ///     while let Some(event) = forward_stream.next().await {
    ///         println!("{:?}", event);
    ///     }
    /// }
    ///
    /// # anyhow::Ok(())
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
    /// use matrix_sdk::Client;
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
    /// if let Some(room) = client.get_joined_room(room_id!("!roomid:example.com"))
    /// {
    ///     let backward_stream = room.timeline_backward().await?;
    ///
    ///     tokio::spawn(async move {
    ///         pin_mut!(backward_stream);
    ///
    ///         while let Some(item) = backward_stream.next().await {
    ///             match item {
    ///                 Ok(event) => println!("{:?}", event),
    ///                 Err(_) => println!("Some error occurred!"),
    ///             }
    ///         }
    ///     });
    /// }
    ///
    /// # anyhow::Ok(())
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
                    Err(TimelineStreamError::EndCache { fetch_more_token }) => {
                        if let Err(error) = room.request_messages(&fetch_more_token).await {
                            yield Err(error);
                        }
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
        let options = assign!(MessagesOptions::backward(), {
            from: Some(token),
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
                SyncMessageLikeEvent::Original(_),
            ))) = event.deserialize_as::<AnySyncRoomEvent>()
            {
                if let Ok(event) = self.decrypt_event(event.cast_ref()).await {
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
    /// use matrix_sdk::ruma::{
    ///     events::room::member::SyncRoomMemberEvent, serde::Raw,
    /// };
    ///
    /// let room_members: Vec<Raw<SyncRoomMemberEvent>> =
    ///     room.get_state_events_static().await?;
    /// # anyhow::Ok(())
    /// # };
    /// ```
    pub async fn get_state_events_static<C>(&self) -> Result<Vec<Raw<SyncStateEvent<C>>>>
    where
        C: StaticEventContent + StateEventContent + RedactContent,
        C::Redacted: StateEventContent + RedactedEventContent,
    {
        Ok(self.client.store().get_state_events_static(self.room_id()).await?)
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

    /// Get a specific state event of statically-known type with an empty state
    /// key in this room.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # async {
    /// # let room: matrix_sdk::room::Common = todo!();
    /// use matrix_sdk::ruma::events::room::power_levels::SyncRoomPowerLevelsEvent;
    ///
    /// let power_levels: SyncRoomPowerLevelsEvent = room
    ///     .get_state_event_static()
    ///     .await?
    ///     .expect("every room has a power_levels event")
    ///     .deserialize()?;
    /// # anyhow::Ok(())
    /// # };
    /// ```
    pub async fn get_state_event_static<C>(&self) -> Result<Option<Raw<SyncStateEvent<C>>>>
    where
        C: StaticEventContent + StateEventContent<StateKey = EmptyStateKey> + RedactContent,
        C::Redacted: StateEventContent + RedactedEventContent,
    {
        self.get_state_event_static_for_key(&EmptyStateKey).await
    }

    /// Get a specific state event of statically-known type in this room.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # async {
    /// # let room: matrix_sdk::room::Common = todo!();
    /// use matrix_sdk::ruma::{
    ///     events::room::member::SyncRoomMemberEvent, serde::Raw, user_id,
    /// };
    ///
    /// let member_event: Option<Raw<SyncRoomMemberEvent>> = room
    ///     .get_state_event_static_for_key(user_id!("@alice:example.org"))
    ///     .await?;
    /// # anyhow::Ok(())
    /// # };
    /// ```
    pub async fn get_state_event_static_for_key<C, K>(
        &self,
        state_key: &K,
    ) -> Result<Option<Raw<SyncStateEvent<C>>>>
    where
        C: StaticEventContent + StateEventContent + RedactContent,
        C::StateKey: Borrow<K>,
        C::Redacted: StateEventContent + RedactedEventContent,
        K: AsRef<str> + ?Sized + Sync,
    {
        Ok(self.client.store().get_state_event_static_for_key(self.room_id(), state_key).await?)
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
    ///     Some(fully_read) => {
    ///         println!("Found read marker: {:?}", fully_read.deserialize()?)
    ///     }
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
    ///     room.set_tag(TagName::User(user_tag), tag_info).await?;
    /// }
    /// # anyhow::Ok(()) });
    /// ```
    pub async fn set_tag(
        &self,
        tag: TagName,
        tag_info: TagInfo,
    ) -> HttpResult<create_tag::v3::Response> {
        let user_id = self.client.user_id().ok_or(HttpError::AuthenticationRequired)?;
        let request =
            create_tag::v3::Request::new(user_id, self.inner.room_id(), tag.as_ref(), tag_info);
        self.client.send(request, None).await
    }

    /// Removes a tag from the room.
    ///
    /// Returns the [`delete_tag::v3::Response`] from the server.
    ///
    /// # Arguments
    /// * `tag` - The tag to remove.
    pub async fn remove_tag(&self, tag: TagName) -> HttpResult<delete_tag::v3::Response> {
        let user_id = self.client.user_id().ok_or(HttpError::AuthenticationRequired)?;
        let request = delete_tag::v3::Request::new(user_id, self.inner.room_id(), tag.as_ref());
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
        let user_id =
            self.client.user_id().ok_or_else(|| Error::from(HttpError::AuthenticationRequired))?;

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

        let request = set_global_account_data::v3::Request::new(user_id, &content)?;

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
    pub async fn decrypt_event(
        &self,
        event: &Raw<OriginalSyncRoomEncryptedEvent>,
    ) -> Result<RoomEvent> {
        if let Some(machine) = self.client.olm_machine() {
            Ok(machine.decrypt_room_event(event.cast_ref(), self.inner.room_id()).await?)
        } else {
            Err(Error::NoOlmMachine)
        }
    }

    /// Get a list of servers that should know this room.
    ///
    /// Uses the synced members of the room and the suggested [routing
    /// algorithm] from the Matrix spec.
    ///
    /// Returns at most three servers.
    ///
    /// [routing algorithm]: https://spec.matrix.org/v1.3/appendices/#routing
    pub async fn route(&self) -> Result<Vec<OwnedServerName>> {
        let acl_ev = self
            .get_state_event_static::<RoomServerAclEventContent>()
            .await?
            .and_then(|ev| ev.deserialize().ok());
        let acl = acl_ev.as_ref().and_then(|ev| ev.as_original()).map(|ev| &ev.content);

        // Filter out server names that:
        // - Are blocked due to server ACLs
        // - Are IP addresses
        let members: Vec<_> = self
            .joined_members_no_sync()
            .await?
            .into_iter()
            .filter(|member| {
                let server = member.user_id().server_name();
                acl.filter(|acl| !acl.is_allowed(server)).is_none() && !server.is_ip_literal()
            })
            .collect();

        // Get the server of the highest power level user in the room, provided
        // they are at least power level 50.
        let max = members
            .iter()
            .max_by_key(|member| member.power_level())
            .filter(|max| max.power_level() >= 50)
            .map(|member| member.user_id().server_name());

        // Sort the servers by population.
        let servers = members
            .iter()
            .map(|member| member.user_id().server_name())
            .filter(|server| max.filter(|max| max == server).is_none())
            .fold(BTreeMap::<_, u32>::new(), |mut servers, server| {
                *servers.entry(server).or_default() += 1;
                servers
            });
        let mut servers: Vec<_> = servers.into_iter().collect();
        servers.sort_unstable_by(|(_, count_a), (_, count_b)| count_b.cmp(count_a));

        Ok(max
            .into_iter()
            .chain(servers.into_iter().map(|(name, _)| name))
            .take(3)
            .map(ToOwned::to_owned)
            .collect())
    }

    /// Get a `matrix.to` permalink to this room.
    ///
    /// If this room has an alias, we use it. Otherwise, we try to use the
    /// synced members in the room for [routing] the room ID.
    ///
    /// [routing]: https://spec.matrix.org/v1.3/appendices/#routing
    pub async fn matrix_to_permalink(&self) -> Result<MatrixToUri> {
        if let Some(alias) = self.canonical_alias().or_else(|| self.alt_aliases().pop()) {
            return Ok(alias.matrix_to_uri());
        }

        let via = self.route().await?;
        Ok(self.room_id().matrix_to_uri_via(via))
    }

    /// Get a `matrix:` permalink to this room.
    ///
    /// If this room has an alias, we use it. Otherwise, we try to use the
    /// synced members in the room for [routing] the room ID.
    ///
    /// # Arguments
    ///
    /// * `join` - Whether the user should join the room.
    ///
    /// [routing]: https://spec.matrix.org/v1.3/appendices/#routing
    pub async fn matrix_permalink(&self, join: bool) -> Result<MatrixUri> {
        if let Some(alias) = self.canonical_alias().or_else(|| self.alt_aliases().pop()) {
            return Ok(alias.matrix_uri(join));
        }

        let via = self.route().await?;
        Ok(self.room_id().matrix_uri_via(via, join))
    }

    /// Get a `matrix.to` permalink to an event in this room.
    ///
    /// We try to use the synced members in the room for [routing] the room ID.
    ///
    /// *Note*: This method does not check if the given event ID is actually
    /// part of this room. It needs to be checked before calling this method
    /// otherwise the permalink won't work.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The ID of the event.
    ///
    /// [routing]: https://spec.matrix.org/v1.3/appendices/#routing
    pub async fn matrix_to_event_permalink(
        &self,
        event_id: impl Into<OwnedEventId>,
    ) -> Result<MatrixToUri> {
        // Don't use the alias because an event is tied to a room ID, but an
        // alias might point to another room, e.g. after a room upgrade.
        let via = self.route().await?;
        Ok(self.room_id().matrix_to_event_uri_via(event_id, via))
    }

    /// Get a `matrix:` permalink to an event in this room.
    ///
    /// We try to use the synced members in the room for [routing] the room ID.
    ///
    /// *Note*: This method does not check if the given event ID is actually
    /// part of this room. It needs to be checked before calling this method
    /// otherwise the permalink won't work.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The ID of the event.
    ///
    /// [routing]: https://spec.matrix.org/v1.3/appendices/#routing
    pub async fn matrix_event_permalink(
        &self,
        event_id: impl Into<OwnedEventId>,
    ) -> Result<MatrixUri> {
        // Don't use the alias because an event is tied to a room ID, but an
        // alias might point to another room, e.g. after a room upgrade.
        let via = self.route().await?;
        Ok(self.room_id().matrix_event_uri_via(event_id, via))
    }
}

/// Options for [`messages`][Common::messages].
///
/// See that method and
/// <https://spec.matrix.org/v1.3/client-server-api/#get_matrixclientv3roomsroomidmessages>
/// for details.
#[derive(Debug)]
#[non_exhaustive]
pub struct MessagesOptions<'a> {
    /// The token to start returning events from.
    ///
    /// This token can be obtained from a `prev_batch` token returned for each
    /// room from the sync API, or from a start or end token returned by a
    /// previous `messages` call.
    ///
    /// If `from` isn't provided the homeserver shall return a list of messages
    /// from the first or last (per the value of the dir parameter) visible
    /// event in the room history for the requesting user.
    pub from: Option<&'a str>,

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
    /// Creates `MessagesOptions` with the given direction.
    ///
    /// All other parameters will be defaulted.
    pub fn new(dir: Direction) -> Self {
        Self { from: None, to: None, dir, limit: uint!(10), filter: RoomEventFilter::default() }
    }

    /// Creates `MessagesOptions` with `dir` set to `Backward`.
    ///
    /// If no `from` token is set afterwards, pagination will start at the
    /// end of (the accessible part of) the room timeline.
    pub fn backward() -> Self {
        Self::new(Direction::Backward)
    }

    /// Creates `MessagesOptions` with `dir` set to `Forward`.
    ///
    /// If no `from` token is set afterwards, pagination will start at the
    /// beginning of (the accessible part of) the room timeline.
    pub fn forward() -> Self {
        Self::new(Direction::Forward)
    }

    /// Creates a new `MessagesOptions` from `self` with the `from` field set to
    /// the given value.
    ///
    /// Since the field is public, you can also assign to it directly. This
    /// method merely acts as a shorthand for that, because it is very
    /// common to set this field.
    pub fn from(self, from: impl Into<Option<&'a str>>) -> Self {
        Self { from: from.into(), ..self }
    }

    fn into_request(self, room_id: &'a RoomId) -> get_message_events::v3::Request<'_> {
        assign!(get_message_events::v3::Request::new(room_id, self.dir), {
            from: self.from,
            to: self.to,
            limit: self.limit,
            filter: self.filter,
        })
    }
}
