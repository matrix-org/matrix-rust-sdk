use std::{borrow::Borrow, collections::BTreeMap, fmt, ops::Deref, sync::Arc};

use matrix_sdk_base::{
    deserialized_responses::{MembersResponse, TimelineEvent},
    store::StateStoreExt,
    RoomMemberships, StateChanges,
};
use matrix_sdk_common::debug::DebugStructExt;
#[cfg(feature = "e2e-encryption")]
use ruma::events::{
    room::encrypted::OriginalSyncRoomEncryptedEvent, AnySyncMessageLikeEvent, AnySyncTimelineEvent,
    SyncMessageLikeEvent,
};
use ruma::{
    api::{
        client::{
            config::set_global_account_data,
            error::ErrorKind,
            filter::RoomEventFilter,
            membership::{get_member_events, join_room_by_id, leave_room},
            message::get_message_events,
            room::get_room_event,
            state::get_state_events_for_key,
            tag::{create_tag, delete_tag},
        },
        Direction,
    },
    assign,
    events::{
        direct::DirectEventContent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        room::{
            encryption::RoomEncryptionEventContent, history_visibility::HistoryVisibility,
            power_levels::RoomPowerLevelsEventContent, server_acl::RoomServerAclEventContent,
            MediaSource,
        },
        tag::{TagInfo, TagName},
        AnyRoomAccountDataEvent, AnyStateEvent, AnySyncStateEvent, EmptyStateKey, RedactContent,
        RedactedStateEventContent, RoomAccountDataEvent, RoomAccountDataEventContent,
        RoomAccountDataEventType, StateEventType, StaticEventContent, StaticStateEventContent,
        SyncStateEvent,
    },
    push::{Action, PushConditionRoomCtx},
    serde::Raw,
    uint, EventId, MatrixToUri, MatrixUri, OwnedEventId, OwnedServerName, OwnedUserId, RoomId,
    UInt, UserId,
};
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;
use tracing::{debug, instrument};

use super::Joined;
use crate::{
    event_handler::{EventHandler, EventHandlerHandle, SyncEvent},
    media::{MediaFormat, MediaRequest},
    room::{Left, RoomMember, RoomState},
    BaseRoom, Client, Error, HttpError, HttpResult, Result,
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
    pub chunk: Vec<TimelineEvent>,

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
    /// Only invited and joined rooms can be left.
    pub(crate) async fn leave(&self) -> Result<Left> {
        let request = leave_room::v3::Request::new(self.inner.room_id().to_owned());
        self.client.send(request, None).await?;

        let base_room = self.client.base_client().room_left(self.room_id()).await?;
        Left::new(&self.client, base_room).ok_or(Error::InconsistentState)
    }

    /// Join this room.
    ///
    /// Only invited and left rooms can be joined via this method.
    pub(crate) async fn join(&self) -> Result<Joined> {
        let request = join_room_by_id::v3::Request::new(self.inner.room_id().to_owned());
        let response = self.client.send(request, None).await?;
        let base_room = self.client.base_client().room_joined(&response.room_id).await?;
        Joined::new(&self.client, base_room).ok_or(Error::InconsistentState)
    }

    /// Get the inner client saved in this room instance.
    ///
    /// Returns the client this room is part of.
    pub fn client(&self) -> Client {
        self.client.clone()
    }

    /// Get the sync state of this room, i.e. whether it was fully synced with
    /// the server.
    pub fn is_synced(&self) -> bool {
        self.inner.is_state_fully_synced()
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
    /// client.login_username(user, "password").send().await.unwrap();
    /// let room_id = room_id!("!roomid:example.com");
    /// let room = client.get_joined_room(&room_id).unwrap();
    /// if let Some(avatar) = room.avatar(MediaFormat::File).await.unwrap() {
    ///     std::fs::write("avatar.png", avatar);
    /// }
    /// # })
    /// ```
    pub async fn avatar(&self, format: MediaFormat) -> Result<Option<Vec<u8>>> {
        let Some(url) = self.avatar_url() else { return Ok(None) };
        let request = MediaRequest { source: MediaSource::Plain(url.to_owned()), format };
        Ok(Some(self.client.media().get_media_content(&request, true).await?))
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
    #[instrument(skip_all, fields(room_id = ?self.inner.room_id(), ?options))]
    pub async fn messages(&self, options: MessagesOptions) -> Result<Messages> {
        let room_id = self.inner.room_id();
        let request = options.into_request(room_id);
        let http_response = self.client.send(request, None).await?;

        #[allow(unused_mut)]
        let mut response = Messages {
            start: http_response.start,
            end: http_response.end,
            #[cfg(not(feature = "e2e-encryption"))]
            chunk: http_response.chunk.into_iter().map(TimelineEvent::new).collect(),
            #[cfg(feature = "e2e-encryption")]
            chunk: Vec::with_capacity(http_response.chunk.len()),
            state: http_response.state,
        };

        #[cfg(feature = "e2e-encryption")]
        if let Some(machine) = self.client.olm_machine() {
            for event in http_response.chunk {
                let decrypted_event = if let Ok(AnySyncTimelineEvent::MessageLike(
                    AnySyncMessageLikeEvent::RoomEncrypted(SyncMessageLikeEvent::Original(_)),
                )) = event.deserialize_as::<AnySyncTimelineEvent>()
                {
                    if let Ok(event) = machine.decrypt_room_event(event.cast_ref(), room_id).await {
                        event
                    } else {
                        TimelineEvent::new(event)
                    }
                } else {
                    TimelineEvent::new(event)
                };

                response.chunk.push(decrypted_event);
            }
        } else {
            response.chunk.extend(http_response.chunk.into_iter().map(TimelineEvent::new));
        }

        if let Some(push_context) = self.push_context().await? {
            let push_rules = self.client().account().push_rules().await?;

            for event in &mut response.chunk {
                event.push_actions = push_rules.get_actions(&event.event, &push_context).to_owned();
            }
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
    {
        self.client.add_room_event_handler(self.room_id(), handler)
    }

    /// Fetch the event with the given `EventId` in this room.
    pub async fn event(&self, event_id: &EventId) -> Result<TimelineEvent> {
        let request =
            get_room_event::v3::Request::new(self.room_id().to_owned(), event_id.to_owned());
        let event = self.client.send(request, None).await?.event;

        #[cfg(feature = "e2e-encryption")]
        if let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomEncrypted(
            SyncMessageLikeEvent::Original(_),
        ))) = event.deserialize_as::<AnySyncTimelineEvent>()
        {
            if let Ok(event) = self.decrypt_event(event.cast_ref()).await {
                return Ok(event);
            }
        }

        let push_actions = self.event_push_actions(&event).await?;

        Ok(TimelineEvent { event, encryption_info: None, push_actions })
    }

    pub(crate) async fn request_members(&self) -> Result<Option<MembersResponse>> {
        let mut map = self.client.inner.members_request_locks.lock().await;

        if let Some(mutex) = map.get(self.inner.room_id()).cloned() {
            // If a member request is already going on, await the release of
            // the lock.
            drop(map);
            _ = mutex.lock().await;

            Ok(None)
        } else {
            let mutex = Arc::new(Mutex::new(()));
            map.insert(self.inner.room_id().to_owned(), mutex.clone());

            let _guard = mutex.lock().await;
            drop(map);

            let request = get_member_events::v3::Request::new(self.inner.room_id().to_owned());
            let response = self.client.send(request, None).await?;

            let response = Box::pin(
                self.client.base_client().receive_members(self.inner.room_id(), &response),
            )
            .await?;

            self.client.inner.members_request_locks.lock().await.remove(self.inner.room_id());

            Ok(Some(response))
        }
    }

    async fn request_encryption_state(&self) -> Result<Option<RoomEncryptionEventContent>> {
        if let Some(mutex) = self
            .client
            .inner
            .encryption_state_request_locks
            .get(self.inner.room_id())
            .map(|m| m.clone())
        {
            // If a encryption state request is already going on, await the release of
            // the lock.
            _ = mutex.lock().await;

            Ok(None)
        } else {
            let mutex = Arc::new(Mutex::new(()));
            self.client
                .inner
                .encryption_state_request_locks
                .insert(self.inner.room_id().to_owned(), mutex.clone());

            let _guard = mutex.lock().await;

            let request = get_state_events_for_key::v3::Request::new(
                self.inner.room_id().to_owned(),
                StateEventType::RoomEncryption,
                "".to_owned(),
            );
            let response = match self.client.send(request, None).await {
                Ok(response) => {
                    Some(response.content.deserialize_as::<RoomEncryptionEventContent>()?)
                }
                Err(err) if err.client_api_error_kind() == Some(&ErrorKind::NotFound) => None,
                Err(err) => return Err(err.into()),
            };

            let sync_lock = self.client.base_client().sync_lock().read().await;
            let mut room_info = self.inner.clone_info();
            room_info.mark_encryption_state_synced();
            room_info.set_encryption_event(response.clone());
            let mut changes = StateChanges::default();
            changes.add_room(room_info.clone());
            self.client.store().save_changes(&changes).await?;
            self.update_summary(room_info);
            drop(sync_lock);

            self.client.inner.encryption_state_request_locks.remove(self.inner.room_id());

            Ok(response)
        }
    }

    /// Check whether this room is encrypted. If the room encryption state is
    /// not synced yet, it will send a request to fetch it.
    ///
    /// Returns true if the room is encrypted, otherwise false.
    pub async fn is_encrypted(&self) -> Result<bool> {
        if !self.is_encryption_state_synced() {
            let encryption = self.request_encryption_state().await?;
            Ok(encryption.is_some())
        } else {
            Ok(self.inner.is_encrypted())
        }
    }

    fn are_events_visible(&self) -> bool {
        if let RoomState::Invited = self.inner.state() {
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
    /// quick succession, in that case the return value will be `None`. This
    /// method does nothing if the members are already synced.
    pub async fn sync_members(&self) -> Result<Option<MembersResponse>> {
        if !self.are_events_visible() {
            return Ok(None);
        }

        if !self.are_members_synced() {
            self.request_members().await
        } else {
            Ok(None)
        }
    }

    /// Get active members for this room, includes invited, joined members.
    ///
    /// *Note*: This method will fetch the members from the homeserver if the
    /// member list isn't synchronized due to member lazy loading. Because of
    /// that, it might panic if it isn't run on a tokio thread.
    ///
    /// Use [active_members_no_sync()](#method.active_members_no_sync) if you
    /// want a method that doesn't do any requests.
    #[deprecated = "Use members with RoomMemberships::ACTIVE instead"]
    pub async fn active_members(&self) -> Result<Vec<RoomMember>> {
        self.sync_members().await?;
        self.members_no_sync(RoomMemberships::ACTIVE).await
    }

    /// Get active members for this room, includes invited, joined members.
    ///
    /// *Note*: This method will not fetch the members from the homeserver if
    /// the member list isn't synchronized due to member lazy loading. Thus,
    /// members could be missing from the list.
    ///
    /// Use [active_members()](#method.active_members) if you want to ensure to
    /// always get the full member list.
    #[deprecated = "Use members_no_sync with RoomMemberships::ACTIVE instead"]
    pub async fn active_members_no_sync(&self) -> Result<Vec<RoomMember>> {
        self.members_no_sync(RoomMemberships::ACTIVE).await
    }

    /// Get all the joined members of this room.
    ///
    /// *Note*: This method will fetch the members from the homeserver if the
    /// member list isn't synchronized due to member lazy loading. Because of
    /// that it might panic if it isn't run on a tokio thread.
    ///
    /// Use [joined_members_no_sync()](#method.joined_members_no_sync) if you
    /// want a method that doesn't do any requests.
    #[deprecated = "Use members with RoomMemberships::JOIN instead"]
    pub async fn joined_members(&self) -> Result<Vec<RoomMember>> {
        self.sync_members().await?;
        self.members_no_sync(RoomMemberships::JOIN).await
    }

    /// Get all the joined members of this room.
    ///
    /// *Note*: This method will not fetch the members from the homeserver if
    /// the member list isn't synchronized due to member lazy loading. Thus,
    /// members could be missing from the list.
    ///
    /// Use [joined_members()](#method.joined_members) if you want to ensure to
    /// always get the full member list.
    #[deprecated = "Use members_no_sync with RoomMemberships::JOIN instead"]
    pub async fn joined_members_no_sync(&self) -> Result<Vec<RoomMember>> {
        self.members_no_sync(RoomMemberships::JOIN).await
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
        self.sync_members().await?;
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

    /// Get members for this room, with the given memberships.
    ///
    /// *Note*: This method will fetch the members from the homeserver if the
    /// member list isn't synchronized due to member lazy loading. Because of
    /// that it might panic if it isn't run on a tokio thread.
    ///
    /// Use [members_no_sync()](#method.members_no_sync) if you want a
    /// method that doesn't do any requests.
    pub async fn members(&self, memberships: RoomMemberships) -> Result<Vec<RoomMember>> {
        self.sync_members().await?;
        self.members_no_sync(memberships).await
    }

    /// Get members for this room, with the given memberships.
    ///
    /// *Note*: This method will not fetch the members from the homeserver if
    /// the member list isn't synchronized due to member lazy loading. Thus,
    /// members could be missing.
    ///
    /// Use [members()](#method.members) if you want to ensure to always get
    /// the full member list.
    pub async fn members_no_sync(&self, memberships: RoomMemberships) -> Result<Vec<RoomMember>> {
        Ok(self
            .inner
            .members(memberships)
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
        C: StaticEventContent + StaticStateEventContent + RedactContent,
        C::Redacted: RedactedStateEventContent,
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
        C: StaticEventContent + StaticStateEventContent<StateKey = EmptyStateKey> + RedactContent,
        C::Redacted: RedactedStateEventContent,
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
        C: StaticEventContent + StaticStateEventContent + RedactContent,
        C::StateKey: Borrow<K>,
        C::Redacted: RedactedStateEventContent,
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
        let user_ids =
            self.client.store().get_user_ids(self.room_id(), RoomMemberships::empty()).await?;

        for user_id in user_ids {
            let devices = self.client.encryption().get_user_devices(&user_id).await?;
            let any_unverified = devices.devices().any(|d| !d.is_verified());

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
        let request = create_tag::v3::Request::new(
            user_id.to_owned(),
            self.inner.room_id().to_owned(),
            tag.to_string(),
            tag_info,
        );
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
        let request = delete_tag::v3::Request::new(
            user_id.to_owned(),
            self.inner.room_id().to_owned(),
            tag.to_string(),
        );
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
            .account()
            .account_data::<DirectEventContent>()
            .await?
            .map(|c| c.deserialize())
            .transpose()?
            .unwrap_or_default();

        let this_room_id = self.inner.room_id();

        if is_direct {
            let mut room_members = self.members(RoomMemberships::ACTIVE).await?;
            room_members.retain(|member| member.user_id() != self.own_user_id());

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

        let request = set_global_account_data::v3::Request::new(user_id.to_owned(), &content)?;

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
    ) -> Result<TimelineEvent> {
        if let Some(machine) = self.client.olm_machine() {
            let mut event =
                machine.decrypt_room_event(event.cast_ref(), self.inner.room_id()).await?;

            event.push_actions = self.event_push_actions(&event.event).await?;

            Ok(event)
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
            .members_no_sync(RoomMemberships::JOIN)
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

    /// Get the latest receipt of a user in this room.
    ///
    /// # Arguments
    ///
    /// * `receipt_type` - The type of receipt to get.
    ///
    /// * `thread` - The thread containing the event of the receipt, if any.
    ///
    /// * `user_id` - The ID of the user.
    ///
    /// Returns the ID of the event on which the receipt applies and the
    /// receipt.
    pub async fn user_receipt(
        &self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        user_id: &UserId,
    ) -> Result<Option<(OwnedEventId, Receipt)>> {
        self.inner.user_receipt(receipt_type, thread, user_id).await.map_err(Into::into)
    }

    /// Get the receipts for an event in this room.
    ///
    /// # Arguments
    ///
    /// * `receipt_type` - The type of receipt to get.
    ///
    /// * `thread` - The thread containing the event of the receipt, if any.
    ///
    /// * `event_id` - The ID of the event.
    ///
    /// Returns a list of IDs of users who have sent a receipt for the event and
    /// the corresponding receipts.
    pub async fn event_receipts(
        &self,
        receipt_type: ReceiptType,
        thread: ReceiptThread,
        event_id: &EventId,
    ) -> Result<Vec<(OwnedUserId, Receipt)>> {
        self.inner.event_receipts(receipt_type, thread, event_id).await.map_err(Into::into)
    }

    /// Get the push context for this room.
    ///
    /// Returns `None` if some data couldn't be found. This should only happen
    /// in brand new rooms, while we process its state.
    pub async fn push_context(&self) -> Result<Option<PushConditionRoomCtx>> {
        let room_id = self.room_id();
        let user_id = self.own_user_id();
        let room_info = self.clone_info();
        let member_count = room_info.active_members_count();

        let user_display_name = if let Some(member) = self.get_member_no_sync(user_id).await? {
            member.name().to_owned()
        } else {
            return Ok(None);
        };

        let room_power_levels = if let Some(event) = self
            .get_state_event_static::<RoomPowerLevelsEventContent>()
            .await?
            .and_then(|e| e.deserialize().ok())
        {
            event.power_levels()
        } else {
            return Ok(None);
        };

        Ok(Some(PushConditionRoomCtx {
            user_id: user_id.to_owned(),
            room_id: room_id.to_owned(),
            member_count: UInt::new(member_count).unwrap_or(UInt::MAX),
            user_display_name,
            users_power_levels: room_power_levels.users,
            default_power_level: room_power_levels.users_default,
            notification_power_levels: room_power_levels.notifications,
        }))
    }

    /// Get the push actions for the given event with the current room state.
    ///
    /// Note that it is possible that no push action is returned because the
    /// current room state does not have all the required state events.
    pub async fn event_push_actions<T>(&self, event: &Raw<T>) -> Result<Vec<Action>> {
        let Some(push_context) = self.push_context().await? else {
            debug!("Could not aggregate push context");
            return Ok(Vec::default());
        };

        let push_rules = self.client().account().push_rules().await?;

        Ok(push_rules.get_actions(event, &push_context).to_owned())
    }
}

/// Options for [`messages`][Common::messages].
///
/// See that method and
/// <https://spec.matrix.org/v1.3/client-server-api/#get_matrixclientv3roomsroomidmessages>
/// for details.
#[non_exhaustive]
pub struct MessagesOptions {
    /// The token to start returning events from.
    ///
    /// This token can be obtained from a `prev_batch` token returned for each
    /// room from the sync API, or from a start or end token returned by a
    /// previous `messages` call.
    ///
    /// If `from` isn't provided the homeserver shall return a list of messages
    /// from the first or last (per the value of the dir parameter) visible
    /// event in the room history for the requesting user.
    pub from: Option<String>,

    /// The token to stop returning events at.
    ///
    /// This token can be obtained from a `prev_batch` token returned for each
    /// room by the sync API, or from a start or end token returned by a
    /// previous `messages` call.
    pub to: Option<String>,

    /// The direction to return events in.
    pub dir: Direction,

    /// The maximum number of events to return.
    ///
    /// Default: 10.
    pub limit: UInt,

    /// A [`RoomEventFilter`] to filter returned events with.
    pub filter: RoomEventFilter,
}

impl MessagesOptions {
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
    pub fn from<'a>(self, from: impl Into<Option<&'a str>>) -> Self {
        Self { from: from.into().map(ToOwned::to_owned), ..self }
    }

    fn into_request(self, room_id: &RoomId) -> get_message_events::v3::Request {
        assign!(get_message_events::v3::Request::new(room_id.to_owned(), self.dir), {
            from: self.from,
            to: self.to,
            limit: self.limit,
            filter: self.filter,
        })
    }
}

impl fmt::Debug for MessagesOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self { from, to, dir, limit, filter } = self;

        let mut s = f.debug_struct("MessagesOptions");
        s.maybe_field("from", from).maybe_field("to", to).field("dir", dir).field("limit", limit);
        if !filter.is_empty() {
            s.field("filter", filter);
        }
        s.finish()
    }
}
