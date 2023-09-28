//! High-level room API

use std::{borrow::Borrow, collections::BTreeMap, ops::Deref, time::Duration};

use eyeball::SharedObservable;
use matrix_sdk_base::{
    deserialized_responses::{
        RawAnySyncOrStrippedState, RawSyncOrStrippedState, SyncOrStrippedState, TimelineEvent,
    },
    instant::Instant,
    store::StateStoreExt,
    RoomMemberships, StateChanges,
};
use matrix_sdk_common::timeout::timeout;
use mime::Mime;
#[cfg(feature = "e2e-encryption")]
use ruma::events::{
    room::encrypted::OriginalSyncRoomEncryptedEvent, AnySyncMessageLikeEvent, AnySyncTimelineEvent,
    SyncMessageLikeEvent,
};
use ruma::{
    api::client::{
        config::set_global_account_data,
        context,
        error::ErrorKind,
        filter::LazyLoadOptions,
        membership::{
            ban_user, forget_room, get_member_events,
            invite_user::{self, v3::InvitationRecipient},
            join_room_by_id, kick_user, leave_room, Invite3pid,
        },
        message::send_message_event,
        read_marker::set_read_marker,
        receipt::create_receipt,
        redact::redact_event,
        room::get_room_event,
        state::{get_state_events_for_key, send_state_event},
        tag::{create_tag, delete_tag},
        typing::create_typing_event::{self, v3::Typing},
    },
    assign,
    events::{
        direct::DirectEventContent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        room::{
            avatar::{self, RoomAvatarEventContent},
            encryption::RoomEncryptionEventContent,
            history_visibility::HistoryVisibility,
            message::RoomMessageEventContent,
            name::RoomNameEventContent,
            power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent},
            server_acl::RoomServerAclEventContent,
            topic::RoomTopicEventContent,
            MediaSource,
        },
        tag::{TagInfo, TagName},
        AnyRoomAccountDataEvent, AnyStateEvent, EmptyStateKey, MessageLikeEventContent,
        MessageLikeEventType, RedactContent, RedactedStateEventContent, RoomAccountDataEvent,
        RoomAccountDataEventContent, RoomAccountDataEventType, StateEventContent, StateEventType,
        StaticEventContent, StaticStateEventContent,
    },
    push::{Action, PushConditionRoomCtx},
    serde::Raw,
    uint, EventId, Int, MatrixToUri, MatrixUri, MxcUri, OwnedEventId, OwnedServerName,
    OwnedTransactionId, OwnedUserId, TransactionId, UInt, UserId,
};
use serde::de::DeserializeOwned;
use thiserror::Error;
use tokio::sync::broadcast;
use tracing::{debug, instrument, warn};

use crate::{
    attachment::AttachmentConfig,
    error::WrongRoomState,
    event_handler::{EventHandler, EventHandlerHandle, SyncEvent},
    media::{MediaFormat, MediaRequest},
    notification_settings::{IsEncrypted, IsOneToOne, RoomNotificationMode},
    sync::RoomUpdate,
    BaseRoom, Client, Error, HttpError, HttpResult, Result, RoomState, TransmissionProgress,
};

mod futures;
mod member;
mod messages;

pub use self::{
    futures::SendAttachment,
    member::RoomMember,
    messages::{Messages, MessagesOptions},
};

/// A struct containing methods that are common for Joined, Invited and Left
/// Rooms
#[derive(Debug, Clone)]
pub struct Room {
    inner: BaseRoom,
    pub(crate) client: Client,
}

impl Deref for Room {
    type Target = BaseRoom;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

const TYPING_NOTICE_TIMEOUT: Duration = Duration::from_secs(4);
const TYPING_NOTICE_RESEND_TIMEOUT: Duration = Duration::from_secs(3);

impl Room {
    /// Create a new `Room`
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
    #[doc(alias = "reject_invitation")]
    pub async fn leave(&self) -> Result<()> {
        let state = self.state();
        if state == RoomState::Left {
            return Err(Error::WrongRoomState(WrongRoomState::new("Joined or Invited", state)));
        }

        let request = leave_room::v3::Request::new(self.inner.room_id().to_owned());
        self.client.send(request, None).await?;
        self.client.base_client().room_left(self.room_id()).await?;
        Ok(())
    }

    /// Join this room.
    ///
    /// Only invited and left rooms can be joined via this method.
    #[doc(alias = "accept_invitation")]
    pub async fn join(&self) -> Result<()> {
        let state = self.state();
        if state == RoomState::Joined {
            return Err(Error::WrongRoomState(WrongRoomState::new("Invited or Left", state)));
        }

        let prev_room_state = self.inner.state();

        let mark_as_direct = prev_room_state == RoomState::Invited
            && self.inner.is_direct().await.unwrap_or_else(|e| {
                warn!(room_id = ?self.room_id(), "is_direct() failed: {e}");
                false
            });

        let request = join_room_by_id::v3::Request::new(self.inner.room_id().to_owned());
        let response = self.client.send(request, None).await?;
        self.client.base_client().room_joined(&response.room_id).await?;

        if mark_as_direct {
            self.set_is_direct(true).await?;
        }

        Ok(())
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
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::Client;
    /// # use matrix_sdk::ruma::room_id;
    /// # use matrix_sdk::media::MediaFormat;
    /// # use url::Url;
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # async {
    /// # let user = "example";
    /// let client = Client::new(homeserver).await.unwrap();
    /// client.matrix_auth().login_username(user, "password").send().await.unwrap();
    /// let room_id = room_id!("!roomid:example.com");
    /// let room = client.get_room(&room_id).unwrap();
    /// if let Some(avatar) = room.avatar(MediaFormat::File).await.unwrap() {
    ///     std::fs::write("avatar.png", avatar);
    /// }
    /// # };
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
    ///
    /// ```no_run
    /// use matrix_sdk::{room::MessagesOptions, Client};
    /// # use matrix_sdk::ruma::{
    /// #     api::client::filter::RoomEventFilter,
    /// #     room_id,
    /// # };
    /// # use url::Url;
    ///
    /// # let homeserver = Url::parse("http://example.com").unwrap();
    /// # async {
    /// let options =
    ///     MessagesOptions::backward().from("t47429-4392820_219380_26003_2265");
    ///
    /// let mut client = Client::new(homeserver).await.unwrap();
    /// let room = client.get_room(room_id!("!roomid:example.com")).unwrap();
    /// assert!(room.messages(options).await.is_ok());
    /// # };
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
        {
            let machine = self.client.olm_machine().await;
            if let Some(machine) = machine.as_ref() {
                for event in http_response.chunk {
                    let decrypted_event = if let Ok(AnySyncTimelineEvent::MessageLike(
                        AnySyncMessageLikeEvent::RoomEncrypted(SyncMessageLikeEvent::Original(_)),
                    )) = event.deserialize_as::<AnySyncTimelineEvent>()
                    {
                        if let Ok(event) =
                            machine.decrypt_room_event(event.cast_ref(), room_id).await
                        {
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
        }

        if let Some(push_context) = self.push_context().await? {
            let push_rules = self.client().account().push_rules().await?;

            for event in &mut response.chunk {
                event.push_actions =
                    Some(push_rules.get_actions(&event.event, &push_context).to_owned());
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

    /// Subscribe to all updates for this room.
    ///
    /// The returned receiver will receive a new message for each sync response
    /// that contains updates for this room.
    pub fn subscribe_to_updates(&self) -> broadcast::Receiver<RoomUpdate> {
        self.client.subscribe_to_room_updates(self.room_id())
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

    /// Fetch the event with the given `EventId` in this room, using the
    /// `/context` endpoint to get more information.
    pub async fn event_with_context(
        &self,
        event_id: &EventId,
        lazy_load_members: bool,
    ) -> Result<Option<(TimelineEvent, Vec<Raw<AnyStateEvent>>)>> {
        let mut request =
            context::get_context::v3::Request::new(self.room_id().to_owned(), event_id.to_owned());

        request.limit = uint!(0);

        if lazy_load_members {
            request.filter.lazy_load_options =
                LazyLoadOptions::Enabled { include_redundant_members: false };
        }

        let response = self.client.send(request, None).await?;

        let Some(event) = response.event else {
            return Ok(None);
        };

        #[cfg(feature = "e2e-encryption")]
        if let Ok(AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomEncrypted(
            SyncMessageLikeEvent::Original(_),
        ))) = event.deserialize_as::<AnySyncTimelineEvent>()
        {
            if let Ok(event) = self.decrypt_event(event.cast_ref()).await {
                return Ok(Some((event, response.state)));
            }
        }

        let push_actions = self.event_push_actions(&event).await?;

        Ok(Some((TimelineEvent { event, encryption_info: None, push_actions }, response.state)))
    }

    pub(crate) async fn request_members(&self) -> Result<()> {
        self.client
            .locks()
            .members_request_deduplicated_handler
            .run(self.room_id().to_owned(), async move {
                let request = get_member_events::v3::Request::new(self.inner.room_id().to_owned());
                let response = self.client.send(request, None).await?;

                // That's a large `Future`. Let's `Box::pin` to reduce its size on the stack.
                Box::pin(self.client.base_client().receive_members(self.room_id(), &response))
                    .await?;

                Ok(())
            })
            .await
    }

    async fn request_encryption_state(&self) -> Result<()> {
        self.client
            .locks()
            .encryption_state_deduplicated_handler
            .run(self.room_id().to_owned(), async move {
                // Request the event from the server.
                let request = get_state_events_for_key::v3::Request::new(
                    self.room_id().to_owned(),
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

                let _sync_lock = self.client.base_client().sync_lock().read().await;

                // Persist the event and the fact that we requested it from the server in
                // `RoomInfo`.
                let mut room_info = self.clone_info();
                room_info.mark_encryption_state_synced();
                room_info.set_encryption_event(response.clone());
                let mut changes = StateChanges::default();
                changes.add_room(room_info.clone());

                self.client.store().save_changes(&changes).await?;
                self.update_summary(room_info);

                Ok(())
            })
            .await
    }

    /// Check whether this room is encrypted. If the room encryption state is
    /// not synced yet, it will send a request to fetch it.
    ///
    /// Returns true if the room is encrypted, otherwise false.
    pub async fn is_encrypted(&self) -> Result<bool> {
        if !self.is_encryption_state_synced() {
            self.request_encryption_state().await?;
        }

        Ok(self.inner.is_encrypted())
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
    pub async fn sync_members(&self) -> Result<()> {
        if !self.are_events_visible() {
            return Ok(());
        }

        if !self.are_members_synced() {
            self.request_members().await
        } else {
            Ok(())
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
    ) -> Result<Vec<RawAnySyncOrStrippedState>> {
        self.client.store().get_state_events(self.room_id(), event_type).await.map_err(Into::into)
    }

    /// Get all state events of a given statically-known type in this room.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async {
    /// # let room: matrix_sdk::Room = todo!();
    /// use matrix_sdk::ruma::{
    ///     events::room::member::RoomMemberEventContent, serde::Raw,
    /// };
    ///
    /// let room_members =
    ///     room.get_state_events_static::<RoomMemberEventContent>().await?;
    /// # anyhow::Ok(())
    /// # };
    /// ```
    pub async fn get_state_events_static<C>(&self) -> Result<Vec<RawSyncOrStrippedState<C>>>
    where
        C: StaticEventContent + StaticStateEventContent + RedactContent,
        C::Redacted: RedactedStateEventContent,
    {
        Ok(self.client.store().get_state_events_static(self.room_id()).await?)
    }

    /// Get the state events of a given type with the given state keys in this
    /// room.
    pub async fn get_state_events_for_keys(
        &self,
        event_type: StateEventType,
        state_keys: &[&str],
    ) -> Result<Vec<RawAnySyncOrStrippedState>> {
        self.client
            .store()
            .get_state_events_for_keys(self.room_id(), event_type, state_keys)
            .await
            .map_err(Into::into)
    }

    /// Get the state events of a given statically-known type with the given
    /// state keys in this room.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async {
    /// # let room: matrix_sdk::Room = todo!();
    /// # let user_ids: &[matrix_sdk::ruma::OwnedUserId] = &[];
    /// use matrix_sdk::ruma::events::room::member::RoomMemberEventContent;
    ///
    /// let room_members = room
    ///     .get_state_events_for_keys_static::<RoomMemberEventContent, _, _>(
    ///         user_ids,
    ///     )
    ///     .await?;
    /// # anyhow::Ok(())
    /// # };
    /// ```
    pub async fn get_state_events_for_keys_static<'a, C, K, I>(
        &self,
        state_keys: I,
    ) -> Result<Vec<RawSyncOrStrippedState<C>>>
    where
        C: StaticEventContent + StaticStateEventContent + RedactContent,
        C::StateKey: Borrow<K>,
        C::Redacted: RedactedStateEventContent,
        K: AsRef<str> + Sized + Sync + 'a,
        I: IntoIterator<Item = &'a K> + Send,
        I::IntoIter: Send,
    {
        Ok(self.client.store().get_state_events_for_keys_static(self.room_id(), state_keys).await?)
    }

    /// Get a specific state event in this room.
    pub async fn get_state_event(
        &self,
        event_type: StateEventType,
        state_key: &str,
    ) -> Result<Option<RawAnySyncOrStrippedState>> {
        self.client
            .store()
            .get_state_event(self.room_id(), event_type, state_key)
            .await
            .map_err(Into::into)
    }

    /// Get a specific state event of statically-known type with an empty state
    /// key in this room.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async {
    /// # let room: matrix_sdk::Room = todo!();
    /// use matrix_sdk::ruma::events::room::power_levels::RoomPowerLevelsEventContent;
    ///
    /// let power_levels = room
    ///     .get_state_event_static::<RoomPowerLevelsEventContent>()
    ///     .await?
    ///     .expect("every room has a power_levels event")
    ///     .deserialize()?;
    /// # anyhow::Ok(())
    /// # };
    /// ```
    pub async fn get_state_event_static<C>(&self) -> Result<Option<RawSyncOrStrippedState<C>>>
    where
        C: StaticEventContent + StaticStateEventContent<StateKey = EmptyStateKey> + RedactContent,
        C::Redacted: RedactedStateEventContent,
    {
        self.get_state_event_static_for_key(&EmptyStateKey).await
    }

    /// Get a specific state event of statically-known type in this room.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # async {
    /// # let room: matrix_sdk::Room = todo!();
    /// use matrix_sdk::ruma::{
    ///     events::room::member::RoomMemberEventContent, serde::Raw, user_id,
    /// };
    ///
    /// let member_event = room
    ///     .get_state_event_static_for_key::<RoomMemberEventContent, _>(user_id!(
    ///         "@alice:example.org"
    ///     ))
    ///     .await?;
    /// # anyhow::Ok(())
    /// # };
    /// ```
    pub async fn get_state_event_static_for_key<C, K>(
        &self,
        state_key: &K,
    ) -> Result<Option<RawSyncOrStrippedState<C>>>
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
    /// # Examples
    ///
    /// ```no_run
    /// # async {
    /// # let room: matrix_sdk::Room = todo!();
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
    /// # Examples
    ///
    /// ```no_run
    /// # use std::str::FromStr;
    /// # use ruma::events::tag::{TagInfo, TagName, UserTagName};
    /// # async {
    /// # let homeserver = url::Url::parse("http://localhost:8080")?;
    /// # let mut client = matrix_sdk::Client::new(homeserver).await?;
    /// # let room_id = matrix_sdk::ruma::room_id!("!test:localhost");
    /// use matrix_sdk::ruma::events::tag::TagInfo;
    ///
    /// if let Some(room) = client.get_room(&room_id) {
    ///     let mut tag_info = TagInfo::new();
    ///     tag_info.order = Some(0.9);
    ///     let user_tag = UserTagName::from_str("u.work")?;
    ///
    ///     room.set_tag(TagName::User(user_tag), tag_info).await?;
    /// }
    /// # anyhow::Ok(()) };
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
        let machine = self.client.olm_machine().await;
        if let Some(machine) = machine.as_ref() {
            let mut event =
                machine.decrypt_room_event(event.cast_ref(), self.inner.room_id()).await?;

            event.push_actions = self.event_push_actions(&event.event).await?;

            Ok(event)
        } else {
            Err(Error::NoOlmMachine)
        }
    }

    /// Ban the user with `UserId` from this room.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The user to ban with `UserId`.
    ///
    /// * `reason` - The reason for banning this user.
    #[instrument(skip_all)]
    pub async fn ban_user(&self, user_id: &UserId, reason: Option<&str>) -> Result<()> {
        let request = assign!(
            ban_user::v3::Request::new(self.room_id().to_owned(), user_id.to_owned()),
            { reason: reason.map(ToOwned::to_owned) }
        );
        self.client.send(request, None).await?;
        Ok(())
    }

    /// Kick a user out of this room.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The `UserId` of the user that should be kicked out of the
    ///   room.
    ///
    /// * `reason` - Optional reason why the room member is being kicked out.
    #[instrument(skip_all)]
    pub async fn kick_user(&self, user_id: &UserId, reason: Option<&str>) -> Result<()> {
        let request = assign!(
            kick_user::v3::Request::new(self.room_id().to_owned(), user_id.to_owned()),
            { reason: reason.map(ToOwned::to_owned) }
        );
        self.client.send(request, None).await?;
        Ok(())
    }

    /// Invite the specified user by `UserId` to this room.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The `UserId` of the user to invite to the room.
    #[instrument(skip_all)]
    pub async fn invite_user_by_id(&self, user_id: &UserId) -> Result<()> {
        let recipient = InvitationRecipient::UserId { user_id: user_id.to_owned() };
        let request = invite_user::v3::Request::new(self.room_id().to_owned(), recipient);
        self.client.send(request, None).await?;

        Ok(())
    }

    /// Invite the specified user by third party id to this room.
    ///
    /// # Arguments
    ///
    /// * `invite_id` - A third party id of a user to invite to the room.
    #[instrument(skip_all)]
    pub async fn invite_user_by_3pid(&self, invite_id: Invite3pid) -> Result<()> {
        let recipient = InvitationRecipient::ThirdPartyId(invite_id);
        let request = invite_user::v3::Request::new(self.room_id().to_owned(), recipient);
        self.client.send(request, None).await?;

        Ok(())
    }

    /// Activate typing notice for this room.
    ///
    /// The typing notice remains active for 4s. It can be deactivate at any
    /// point by setting typing to `false`. If this method is called while
    /// the typing notice is active nothing will happen. This method can be
    /// called on every key stroke, since it will do nothing while typing is
    /// active.
    ///
    /// # Arguments
    ///
    /// * `typing` - Whether the user is typing or has stopped typing.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use std::time::Duration;
    ///
    /// use matrix_sdk::ruma::api::client::typing::create_typing_event::v3::Typing;
    /// # use matrix_sdk::{
    /// #     Client, config::SyncSettings,
    /// #     ruma::room_id,
    /// # };
    /// # use url::Url;
    ///
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");
    ///
    /// if let Some(room) = client.get_room(&room_id) {
    ///     room.typing_notice(true).await?
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn typing_notice(&self, typing: bool) -> Result<()> {
        self.ensure_room_joined()?;

        // Only send a request to the homeserver if the old timeout has elapsed
        // or the typing notice changed state within the
        // TYPING_NOTICE_TIMEOUT
        let send =
            if let Some(typing_time) = self.client.inner.typing_notice_times.get(self.room_id()) {
                if typing_time.elapsed() > TYPING_NOTICE_RESEND_TIMEOUT {
                    // We always reactivate the typing notice if typing is true or
                    // we may need to deactivate it if it's
                    // currently active if typing is false
                    typing || typing_time.elapsed() <= TYPING_NOTICE_TIMEOUT
                } else {
                    // Only send a request when we need to deactivate typing
                    !typing
                }
            } else {
                // Typing notice is currently deactivated, therefore, send a request
                // only when it's about to be activated
                typing
            };

        if send {
            self.send_typing_notice(typing).await?;
        }

        Ok(())
    }

    #[instrument(name = "typing_notice", skip(self))]
    async fn send_typing_notice(&self, typing: bool) -> Result<()> {
        let typing = if typing {
            self.client.inner.typing_notice_times.insert(self.room_id().to_owned(), Instant::now());
            Typing::Yes(TYPING_NOTICE_TIMEOUT)
        } else {
            self.client.inner.typing_notice_times.remove(self.room_id());
            Typing::No
        };

        let request = create_typing_event::v3::Request::new(
            self.own_user_id().to_owned(),
            self.room_id().to_owned(),
            typing,
        );

        self.client.send(request, None).await?;

        Ok(())
    }

    /// Send a request to set a single receipt.
    ///
    /// # Arguments
    ///
    /// * `receipt_type` - The type of the receipt to set. Note that it is
    ///   possible to set the fully-read marker although it is technically not a
    ///   receipt.
    ///
    /// * `thread` - The thread where this receipt should apply, if any. Note
    ///   that this must be [`ReceiptThread::Unthreaded`] when sending a
    ///   [`ReceiptType::FullyRead`][create_receipt::v3::ReceiptType::FullyRead].
    ///
    /// * `event_id` - The `EventId` of the event to set the receipt on.
    #[instrument(skip_all)]
    pub async fn send_single_receipt(
        &self,
        receipt_type: create_receipt::v3::ReceiptType,
        thread: ReceiptThread,
        event_id: OwnedEventId,
    ) -> Result<()> {
        let mut request =
            create_receipt::v3::Request::new(self.room_id().to_owned(), receipt_type, event_id);
        request.thread = thread;

        self.client.send(request, None).await?;
        Ok(())
    }

    /// Send a request to set multiple receipts at once.
    ///
    /// # Arguments
    ///
    /// * `receipts` - The `Receipts` to send.
    ///
    /// If `receipts` is empty, this is a no-op.
    #[instrument(skip_all)]
    pub async fn send_multiple_receipts(&self, receipts: Receipts) -> Result<()> {
        if receipts.is_empty() {
            return Ok(());
        }

        let Receipts { fully_read, public_read_receipt, private_read_receipt } = receipts;
        let request = assign!(set_read_marker::v3::Request::new(self.room_id().to_owned()), {
            fully_read,
            read_receipt: public_read_receipt,
            private_read_receipt,
        });

        self.client.send(request, None).await?;
        Ok(())
    }

    /// Enable End-to-end encryption in this room.
    ///
    /// This method will be a noop if encryption is already enabled, otherwise
    /// sends a `m.room.encryption` state event to the room. This might fail if
    /// you don't have the appropriate power level to enable end-to-end
    /// encryption.
    ///
    /// A sync needs to be received to update the local room state. This method
    /// will wait for a sync to be received, this might time out if no
    /// sync loop is running or if the server is slow.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use matrix_sdk::{
    /// #     Client, config::SyncSettings,
    /// #     ruma::room_id,
    /// # };
    /// # use url::Url;
    /// #
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let client = Client::new(homeserver).await?;
    /// # let room_id = room_id!("!test:localhost");
    /// let room_id = room_id!("!SVkFJHzfwvuaIEawgC:localhost");
    ///
    /// if let Some(room) = client.get_room(&room_id) {
    ///     room.enable_encryption().await?
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    #[instrument(skip_all)]
    pub async fn enable_encryption(&self) -> Result<()> {
        use ruma::{
            events::room::encryption::RoomEncryptionEventContent, EventEncryptionAlgorithm,
        };
        const SYNC_WAIT_TIME: Duration = Duration::from_secs(3);

        if !self.is_encrypted().await? {
            let content =
                RoomEncryptionEventContent::new(EventEncryptionAlgorithm::MegolmV1AesSha2);
            self.send_state_event(content).await?;

            // TODO do we want to return an error here if we time out? This
            // could be quite useful if someone wants to enable encryption and
            // send a message right after it's enabled.
            _ = timeout(self.client.inner.sync_beat.listen(), SYNC_WAIT_TIME).await;
        }

        Ok(())
    }

    /// Share a room key with users in the given room.
    ///
    /// This will create Olm sessions with all the users/device pairs in the
    /// room if necessary and share a room key that can be shared with them.
    ///
    /// Does nothing if no room key needs to be shared.
    // TODO: expose this publicly so people can pre-share a group session if
    // e.g. a user starts to type a message for a room.
    #[cfg(feature = "e2e-encryption")]
    #[instrument(skip_all, fields(room_id = ?self.room_id()))]
    async fn preshare_room_key(&self) -> Result<()> {
        self.ensure_room_joined()?;

        // Take and release the lock on the store, if needs be.
        let _guard = self.client.encryption().spin_lock_store(Some(60000)).await?;

        self.client
            .locks()
            .group_session_deduplicated_handler
            .run(self.room_id().to_owned(), async move {
                {
                    let members = self
                        .client
                        .store()
                        .get_user_ids(self.room_id(), RoomMemberships::ACTIVE)
                        .await?;
                    self.client.claim_one_time_keys(members.iter().map(Deref::deref)).await?;
                };

                let response = self.share_room_key().await;

                // If one of the responses failed invalidate the group
                // session as using it would end up in undecryptable
                // messages.
                if let Err(r) = response {
                    let machine = self.client.olm_machine().await;
                    if let Some(machine) = machine.as_ref() {
                        machine.invalidate_group_session(self.room_id()).await?;
                    }
                    return Err(r);
                }

                Ok(())
            })
            .await
    }

    /// Share a group session for a room.
    ///
    /// # Panics
    ///
    /// Panics if the client isn't logged in.
    #[cfg(feature = "e2e-encryption")]
    #[instrument(skip_all)]
    async fn share_room_key(&self) -> Result<()> {
        self.ensure_room_joined()?;

        let requests = self.client.base_client().share_room_key(self.room_id()).await?;

        for request in requests {
            let response = self.client.send_to_device(&request).await?;
            self.client.mark_request_as_sent(&request.txn_id, &response).await?;
        }

        Ok(())
    }

    /// Wait for the room to be fully synced.
    ///
    /// This method makes sure the room that was returned when joining a room
    /// has been echoed back in the sync.
    ///
    /// Warning: This waits until a sync happens and does not return if no sync
    /// is happening. It can also return early when the room is not a joined
    /// room anymore.
    #[instrument(skip_all)]
    pub async fn sync_up(&self) {
        while !self.is_synced() && self.state() == RoomState::Joined {
            let wait_for_beat = self.client.inner.sync_beat.listen();
            // We don't care whether it's a timeout or a sync beat.
            let _ = timeout(wait_for_beat, Duration::from_millis(1000)).await;
        }
    }

    /// Send a room message to this room.
    ///
    /// Returns the parsed response from the server.
    ///
    /// If the encryption feature is enabled this method will transparently
    /// encrypt the room message if this room is encrypted.
    ///
    /// **Note**: If you just want to send a custom JSON payload to a room, you
    /// can use the [`send_raw()`][Self::send_raw] method for that.
    ///
    /// # Arguments
    ///
    /// * `content` - The content of the message event.
    ///
    /// * `txn_id` - A locally-unique ID describing a message transaction with
    ///   the homeserver. Unless you're doing something special, you can pass in
    ///   `None` which will create a suitable one for you automatically.
    ///     * On the sending side, this field is used for re-trying earlier
    ///       failed transactions. Subsequent messages *must never* re-use an
    ///       earlier transaction ID.
    ///     * On the receiving side, the field is used for recognizing our own
    ///       messages when they arrive down the sync: the server includes the
    ///       ID in the [`MessageLikeUnsigned`] field [`transaction_id`] of the
    ///       corresponding [`SyncMessageLikeEvent`], but only for the *sending*
    ///       device. Other devices will not see it. This is then used to ignore
    ///       events sent by our own device and/or to implement local echo.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::sync::{Arc, RwLock};
    /// # use matrix_sdk::{Client, config::SyncSettings};
    /// # use url::Url;
    /// # use matrix_sdk::ruma::room_id;
    /// # use serde::{Deserialize, Serialize};
    /// use matrix_sdk::ruma::{
    ///     events::{
    ///         macros::EventContent,
    ///         room::message::{RoomMessageEventContent, TextMessageEventContent},
    ///     },
    ///     uint, MilliSecondsSinceUnixEpoch, TransactionId,
    /// };
    ///
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// # let room_id = room_id!("!test:localhost");
    /// let content = RoomMessageEventContent::text_plain("Hello world");
    /// let txn_id = TransactionId::new();
    ///
    /// if let Some(room) = client.get_room(&room_id) {
    ///     room.send(content, Some(&txn_id)).await?;
    /// }
    ///
    /// // Custom events work too:
    /// #[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
    /// #[ruma_event(type = "org.shiny_new_2fa.token", kind = MessageLike)]
    /// struct TokenEventContent {
    ///     token: String,
    ///     #[serde(rename = "exp")]
    ///     expires_at: MilliSecondsSinceUnixEpoch,
    /// }
    ///
    /// # fn generate_token() -> String { todo!() }
    /// let content = TokenEventContent {
    ///     token: generate_token(),
    ///     expires_at: {
    ///         let now = MilliSecondsSinceUnixEpoch::now();
    ///         MilliSecondsSinceUnixEpoch(now.0 + uint!(30_000))
    ///     },
    /// };
    /// let txn_id = TransactionId::new();
    ///
    /// if let Some(room) = client.get_room(&room_id) {
    ///     room.send(content, Some(&txn_id)).await?;
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    ///
    /// [`SyncMessageLikeEvent`]: ruma::events::SyncMessageLikeEvent
    /// [`MessageLikeUnsigned`]: ruma::events::MessageLikeUnsigned
    /// [`transaction_id`]: ruma::events::MessageLikeUnsigned#structfield.transaction_id
    pub async fn send(
        &self,
        content: impl MessageLikeEventContent,
        txn_id: Option<&TransactionId>,
    ) -> Result<send_message_event::v3::Response> {
        let event_type = content.event_type().to_string();
        let content = serde_json::to_value(&content)?;

        self.send_raw(content, &event_type, txn_id).await
    }

    /// Run /keys/query requests for all the non-tracked users.
    #[cfg(feature = "e2e-encryption")]
    async fn query_keys_for_untracked_users(&self) -> Result<()> {
        use std::collections::HashMap;

        let olm = self.client.olm_machine().await;
        let olm = olm.as_ref().expect("Olm machine wasn't started");

        let members =
            self.client.store().get_user_ids(self.room_id(), RoomMemberships::ACTIVE).await?;

        let tracked: HashMap<_, _> = olm
            .store()
            .load_tracked_users()
            .await?
            .into_iter()
            .map(|tracked| (tracked.user_id, tracked.dirty))
            .collect();

        // A member has no unknown devices iff it was tracked *and* the tracking is
        // not considered dirty.
        let members_with_unknown_devices =
            members.iter().filter(|member| tracked.get(*member).map_or(true, |dirty| *dirty));

        let (req_id, request) =
            olm.query_keys_for_users(members_with_unknown_devices.map(|owned| owned.borrow()));

        if !request.device_keys.is_empty() {
            self.client.keys_query(&req_id, request.device_keys).await?;
        }

        Ok(())
    }

    /// Send a room message to this room from a json `Value`.
    ///
    /// Returns the parsed response from the server.
    ///
    /// If the encryption feature is enabled this method will transparently
    /// encrypt the room message if this room is encrypted.
    ///
    /// This method is equivalent to the [`send()`][Self::send] method but
    /// allows sending custom JSON payloads, e.g. constructed using the
    /// [`serde_json::json!()`] macro.
    ///
    /// # Arguments
    ///
    /// * `content` - The content of the event as a json `Value`.
    ///
    /// * `event_type` - The type of the event.
    ///
    /// * `txn_id` - A locally-unique ID describing a message transaction with
    ///   the homeserver. Unless you're doing something special, you can pass in
    ///   `None` which will create a suitable one for you automatically.
    ///     * On the sending side, this field is used for re-trying earlier
    ///       failed transactions. Subsequent messages *must never* re-use an
    ///       earlier transaction ID.
    ///     * On the receiving side, the field is used for recognizing our own
    ///       messages when they arrive down the sync: the server includes the
    ///       ID in the [`StateUnsigned`] field [`transaction_id`] of the
    ///       corresponding [`SyncMessageLikeEvent`], but only for the *sending*
    ///       device. Other devices will not see it. This is then used to ignore
    ///       events sent by our own device and/or to implement local echo.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::sync::{Arc, RwLock};
    /// # use matrix_sdk::{Client, config::SyncSettings};
    /// # use url::Url;
    /// # use matrix_sdk::ruma::room_id;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// # let room_id = room_id!("!test:localhost");
    /// use serde_json::json;
    ///
    /// let content = json!({
    ///     "body": "Hello world",
    /// });
    ///
    /// if let Some(room) = client.get_room(&room_id) {
    ///     room.send_raw(content, "m.room.message", None).await?;
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    ///
    /// [`SyncMessageLikeEvent`]: ruma::events::SyncMessageLikeEvent
    /// [`StateUnsigned`]: ruma::events::StateUnsigned
    /// [`transaction_id`]: ruma::events::StateUnsigned#structfield.transaction_id
    #[instrument(skip_all, fields(event_type, room_id = %self.room_id(), transaction_id, encrypted))]
    pub async fn send_raw(
        &self,
        content: serde_json::Value,
        event_type: &str,
        txn_id: Option<&TransactionId>,
    ) -> Result<send_message_event::v3::Response> {
        self.ensure_room_joined()?;

        let txn_id: OwnedTransactionId = txn_id.map_or_else(TransactionId::new, ToOwned::to_owned);
        tracing::Span::current().record("transaction_id", tracing::field::debug(&txn_id));

        #[cfg(not(feature = "e2e-encryption"))]
        let content = {
            debug!(
                room_id = ?self.room_id(),
                "Sending plaintext event to room because we don't have encryption support.",
            );
            Raw::new(&content)?.cast()
        };

        #[cfg(feature = "e2e-encryption")]
        let (content, event_type) = if self.is_encrypted().await? {
            tracing::Span::current().record("encrypted", tracing::field::debug(&txn_id));
            // Reactions are currently famously not encrypted, skip encrypting
            // them until they are.
            if event_type == "m.reaction" {
                debug!(
                    room_id = ?self.room_id(),
                    "Sending plaintext event because the event type is {event_type}",
                );
                (Raw::new(&content)?.cast(), event_type)
            } else {
                debug!(
                    room_id = self.room_id().as_str(),
                    "Sending encrypted event because the room is encrypted.",
                );

                if !self.are_members_synced() {
                    self.sync_members().await?;
                }

                // Query keys in case we don't have them for newly synced members.
                //
                // Note we do it all the time, because we might have sync'd members before
                // sending a message (so didn't enter the above branch), but
                // could have not query their keys ever.
                self.query_keys_for_untracked_users().await?;

                self.preshare_room_key().await?;

                let olm = self.client.olm_machine().await;
                let olm = olm.as_ref().expect("Olm machine wasn't started");

                let encrypted_content =
                    olm.encrypt_room_event_raw(self.room_id(), content, event_type).await?;

                (encrypted_content.cast(), "m.room.encrypted")
            }
        } else {
            debug!(
                room_id = ?self.room_id(),
                "Sending plaintext event because the room is NOT encrypted.",
            );

            (Raw::new(&content)?.cast(), event_type)
        };

        let request = send_message_event::v3::Request::new_raw(
            self.room_id().to_owned(),
            txn_id,
            event_type.into(),
            content,
        );

        let response = self.client.send(request, None).await?;
        Ok(response)
    }

    /// Send an attachment to this room.
    ///
    /// This will upload the given data that the reader produces using the
    /// [`upload()`] method and post an event to the given room.
    /// If the room is encrypted and the encryption feature is enabled the
    /// upload will be encrypted.
    ///
    /// This is a convenience method that calls the
    /// [`upload()`] and afterwards the [`send()`].
    ///
    /// # Arguments
    /// * `body` - A textual representation of the media that is going to be
    /// uploaded. Usually the file name.
    ///
    /// * `content_type` - The type of the media, this will be used as the
    /// content-type header.
    ///
    /// * `reader` - A `Reader` that will be used to fetch the raw bytes of the
    /// media.
    ///
    /// * `config` - Metadata and configuration for the attachment.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use std::fs;
    /// # use matrix_sdk::{Client, ruma::room_id, attachment::AttachmentConfig};
    /// # use url::Url;
    /// # use mime;
    /// # async {
    /// # let homeserver = Url::parse("http://localhost:8080")?;
    /// # let mut client = Client::new(homeserver).await?;
    /// # let room_id = room_id!("!test:localhost");
    /// let mut image = fs::read("/home/example/my-cat.jpg")?;
    ///
    /// if let Some(room) = client.get_room(&room_id) {
    ///     room.send_attachment(
    ///         "My favorite cat",
    ///         &mime::IMAGE_JPEG,
    ///         image,
    ///         AttachmentConfig::new(),
    ///     ).await?;
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    ///
    /// [`upload()`]: crate::Media::upload
    /// [`send()`]: Self::send
    #[instrument(skip_all)]
    pub fn send_attachment<'a>(
        &'a self,
        body: &'a str,
        content_type: &'a Mime,
        data: Vec<u8>,
        config: AttachmentConfig,
    ) -> SendAttachment<'a> {
        SendAttachment::new(self, body, content_type, data, config)
    }

    /// Prepare and send an attachment to this room.
    ///
    /// This will upload the given data that the reader produces using the
    /// [`upload()`](#method.upload) method and post an event to the given room.
    /// If the room is encrypted and the encryption feature is enabled the
    /// upload will be encrypted.
    ///
    /// This is a convenience method that calls the
    /// [`Client::upload()`](#Client::method.upload) and afterwards the
    /// [`send()`](#method.send).
    ///
    /// # Arguments
    /// * `body` - A textual representation of the media that is going to be
    /// uploaded. Usually the file name.
    ///
    /// * `content_type` - The type of the media, this will be used as the
    /// content-type header.
    ///
    /// * `reader` - A `Reader` that will be used to fetch the raw bytes of the
    /// media.
    ///
    /// * `config` - Metadata and configuration for the attachment.
    pub(super) async fn prepare_and_send_attachment<'a>(
        &'a self,
        body: &'a str,
        content_type: &'a Mime,
        data: Vec<u8>,
        config: AttachmentConfig,
        send_progress: SharedObservable<TransmissionProgress>,
    ) -> Result<send_message_event::v3::Response> {
        self.ensure_room_joined()?;

        #[cfg(feature = "e2e-encryption")]
        let content = if self.is_encrypted().await? {
            self.client
                .prepare_encrypted_attachment_message(
                    body,
                    content_type,
                    data,
                    config.info,
                    config.thumbnail,
                    send_progress,
                )
                .await?
        } else {
            self.client
                .media()
                .prepare_attachment_message(
                    body,
                    content_type,
                    data,
                    config.info,
                    config.thumbnail,
                    send_progress,
                )
                .await?
        };

        #[cfg(not(feature = "e2e-encryption"))]
        let content = self
            .client
            .media()
            .prepare_attachment_message(
                body,
                content_type,
                data,
                config.info,
                config.thumbnail,
                send_progress,
            )
            .await?;

        self.send(RoomMessageEventContent::new(content), config.txn_id.as_deref()).await
    }

    /// Update the power levels of a select set of users of this room.
    ///
    /// Issue a `power_levels` state event request to the server, changing the
    /// given UserId -> Int levels. May fail if the `power_levels` aren't
    /// locally known yet or the server rejects the state event update, e.g.
    /// because of insufficient permissions. Neither permissions to update
    /// nor whether the data might be stale is checked prior to issuing the
    /// request.
    pub async fn update_power_levels(
        &self,
        updates: Vec<(&UserId, Int)>,
    ) -> Result<send_state_event::v3::Response> {
        let mut power_levels = self.get_room_power_levels().await?;

        for (user_id, new_level) in updates {
            if new_level == power_levels.users_default {
                power_levels.users.remove(user_id);
            } else {
                power_levels.users.insert(user_id.to_owned(), new_level);
            }
        }

        self.send_state_event(RoomPowerLevelsEventContent::from(power_levels)).await
    }

    async fn get_room_power_levels(&self) -> Result<RoomPowerLevels> {
        Ok(self
            .get_state_event_static::<RoomPowerLevelsEventContent>()
            .await?
            .ok_or(Error::InsufficientData)?
            .deserialize()?
            .power_levels())
    }

    /// Sets the name of this room.
    pub async fn set_name(&self, name: String) -> Result<send_state_event::v3::Response> {
        self.send_state_event(RoomNameEventContent::new(name)).await
    }

    /// Sets a new topic for this room.
    pub async fn set_room_topic(&self, topic: &str) -> Result<send_state_event::v3::Response> {
        self.send_state_event(RoomTopicEventContent::new(topic.into())).await
    }

    /// Sets the new avatar url for this room.
    ///
    /// # Arguments
    /// * `avatar_url` - The owned matrix uri that represents the avatar
    /// * `info` - The optional image info that can be provided for the avatar
    pub async fn set_avatar_url(
        &self,
        url: &MxcUri,
        info: Option<avatar::ImageInfo>,
    ) -> Result<send_state_event::v3::Response> {
        self.ensure_room_joined()?;

        let mut room_avatar_event = RoomAvatarEventContent::new();
        room_avatar_event.url = Some(url.to_owned());
        room_avatar_event.info = info.map(Box::new);

        self.send_state_event(room_avatar_event).await
    }

    /// Removes the avatar from the room
    pub async fn remove_avatar(&self) -> Result<send_state_event::v3::Response> {
        self.send_state_event(RoomAvatarEventContent::new()).await
    }

    /// Uploads a new avatar for this room.
    ///
    /// # Arguments
    /// * `mime` - The mime type describing the data
    /// * `data` - The data representation of the avatar
    /// * `info` - The optional image info provided for the avatar,
    /// the blurhash and the mimetype will always be updated
    pub async fn upload_avatar(
        &self,
        mime: &Mime,
        data: Vec<u8>,
        info: Option<avatar::ImageInfo>,
    ) -> Result<send_state_event::v3::Response> {
        self.ensure_room_joined()?;

        let upload_response = self.client.media().upload(mime, data).await?;
        let mut info = info.unwrap_or_default();
        info.blurhash = upload_response.blurhash;
        info.mimetype = Some(mime.to_string());

        self.set_avatar_url(&upload_response.content_uri, Some(info)).await
    }

    /// Send a state event with an empty state key to the homeserver.
    ///
    /// For state events with a non-empty state key, see
    /// [`send_state_event_for_key`][Self::send_state_event_for_key].
    ///
    /// Returns the parsed response from the server.
    ///
    /// # Arguments
    ///
    /// * `content` - The content of the state event.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use serde::{Deserialize, Serialize};
    /// # async {
    /// # let joined_room: matrix_sdk::Room = todo!();
    /// use matrix_sdk::ruma::{
    ///     events::{
    ///         macros::EventContent, room::encryption::RoomEncryptionEventContent,
    ///         EmptyStateKey,
    ///     },
    ///     EventEncryptionAlgorithm,
    /// };
    ///
    /// let encryption_event_content = RoomEncryptionEventContent::new(
    ///     EventEncryptionAlgorithm::MegolmV1AesSha2,
    /// );
    /// joined_room.send_state_event(encryption_event_content).await?;
    ///
    /// // Custom event:
    /// #[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
    /// #[ruma_event(
    ///     type = "org.matrix.msc_9000.xxx",
    ///     kind = State,
    ///     state_key_type = EmptyStateKey,
    /// )]
    /// struct XxxStateEventContent {/* fields... */}
    ///
    /// let content: XxxStateEventContent = todo!();
    /// joined_room.send_state_event(content).await?;
    /// # anyhow::Ok(()) };
    /// ```
    #[instrument(skip_all)]
    pub async fn send_state_event(
        &self,
        content: impl StateEventContent<StateKey = EmptyStateKey>,
    ) -> Result<send_state_event::v3::Response> {
        self.send_state_event_for_key(&EmptyStateKey, content).await
    }

    /// Send a state event to the homeserver.
    ///
    /// Returns the parsed response from the server.
    ///
    /// # Arguments
    ///
    /// * `content` - The content of the state event.
    ///
    /// * `state_key` - A unique key which defines the overwriting semantics for
    ///   this piece of room state.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use serde::{Deserialize, Serialize};
    /// # async {
    /// # let joined_room: matrix_sdk::Room = todo!();
    /// use matrix_sdk::ruma::{
    ///     events::{
    ///         macros::EventContent,
    ///         room::member::{RoomMemberEventContent, MembershipState},
    ///     },
    ///     mxc_uri,
    /// };
    ///
    /// let avatar_url = mxc_uri!("mxc://example.org/avatar").to_owned();
    /// let mut content = RoomMemberEventContent::new(MembershipState::Join);
    /// content.avatar_url = Some(avatar_url);
    ///
    /// joined_room.send_state_event_for_key(ruma::user_id!("@foo:bar.com"), content).await?;
    ///
    /// // Custom event:
    /// #[derive(Clone, Debug, Deserialize, Serialize, EventContent)]
    /// #[ruma_event(type = "org.matrix.msc_9000.xxx", kind = State, state_key_type = String)]
    /// struct XxxStateEventContent { /* fields... */ }
    ///
    /// let content: XxxStateEventContent = todo!();
    /// joined_room.send_state_event_for_key("foo", content).await?;
    /// # anyhow::Ok(()) };
    /// ```
    pub async fn send_state_event_for_key<C, K>(
        &self,
        state_key: &K,
        content: C,
    ) -> Result<send_state_event::v3::Response>
    where
        C: StateEventContent,
        C::StateKey: Borrow<K>,
        K: AsRef<str> + ?Sized,
    {
        self.ensure_room_joined()?;
        let request =
            send_state_event::v3::Request::new(self.room_id().to_owned(), state_key, &content)?;
        let response = self.client.send(request, None).await?;
        Ok(response)
    }

    /// Send a raw room state event to the homeserver.
    ///
    /// Returns the parsed response from the server.
    ///
    /// # Arguments
    ///
    /// * `content` - The raw content of the state event.
    ///
    /// * `event_type` - The type of the event that we're sending out.
    ///
    /// * `state_key` - A unique key which defines the overwriting semantics for
    /// this piece of room state. This value is often a zero-length string.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use serde_json::json;
    ///
    /// # async {
    /// # let homeserver = url::Url::parse("http://localhost:8080")?;
    /// # let mut client = matrix_sdk::Client::new(homeserver).await?;
    /// # let room_id = matrix_sdk::ruma::room_id!("!test:localhost");
    /// let content = json!({
    ///     "avatar_url": "mxc://example.org/SEsfnsuifSDFSSEF",
    ///     "displayname": "Alice Margatroid",
    ///     "membership": "join"
    /// });
    ///
    /// if let Some(room) = client.get_room(&room_id) {
    ///     room.send_state_event_raw(content, "m.room.member", "").await?;
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    #[instrument(skip_all)]
    pub async fn send_state_event_raw(
        &self,
        content: serde_json::Value,
        event_type: &str,
        state_key: &str,
    ) -> Result<send_state_event::v3::Response> {
        self.ensure_room_joined()?;

        let content = Raw::new(&content)?.cast();
        let request = send_state_event::v3::Request::new_raw(
            self.room_id().to_owned(),
            event_type.into(),
            state_key.to_owned(),
            content,
        );

        Ok(self.client.send(request, None).await?)
    }

    /// Strips all information out of an event of the room.
    ///
    /// Returns the [`redact_event::v3::Response`] from the server.
    ///
    /// This cannot be undone. Users may redact their own events, and any user
    /// with a power level greater than or equal to the redact power level of
    /// the room may redact events there.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The ID of the event to redact
    ///
    /// * `reason` - The reason for the event being redacted.
    ///
    /// * `txn_id` - A unique ID that can be attached to this event as
    /// its transaction ID. If not given one is created for the message.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use matrix_sdk::ruma::event_id;
    ///
    /// # async {
    /// # let homeserver = url::Url::parse("http://localhost:8080")?;
    /// # let mut client = matrix_sdk::Client::new(homeserver).await?;
    /// # let room_id = matrix_sdk::ruma::room_id!("!test:localhost");
    /// #
    /// if let Some(room) = client.get_room(&room_id) {
    ///     let event_id = event_id!("$xxxxxx:example.org");
    ///     let reason = Some("Indecent material");
    ///     room.redact(&event_id, reason, None).await?;
    /// }
    /// # anyhow::Ok(()) };
    /// ```
    #[instrument(skip_all)]
    pub async fn redact(
        &self,
        event_id: &EventId,
        reason: Option<&str>,
        txn_id: Option<OwnedTransactionId>,
    ) -> HttpResult<redact_event::v3::Response> {
        let txn_id = txn_id.unwrap_or_else(TransactionId::new);
        let request = assign!(
            redact_event::v3::Request::new(self.room_id().to_owned(), event_id.to_owned(), txn_id),
            { reason: reason.map(ToOwned::to_owned) }
        );

        self.client.send(request, None).await
    }

    /// Returns true if the user with the given user_id is able to redact
    /// messages in the room.
    ///
    /// The call may fail if there is an error in getting the power levels.
    pub async fn can_user_redact(&self, user_id: &UserId) -> Result<bool> {
        Ok(self.get_room_power_levels().await?.user_can_redact(user_id))
    }

    /// Returns true if the user with the given user_id is able to ban in the
    /// room.
    ///
    /// The call may fail if there is an error in getting the power levels.
    pub async fn can_user_ban(&self, user_id: &UserId) -> Result<bool> {
        Ok(self.get_room_power_levels().await?.user_can_ban(user_id))
    }

    /// Returns true if the user with the given user_id is able to kick in the
    /// room.
    ///
    /// The call may fail if there is an error in getting the power levels.
    pub async fn can_user_invite(&self, user_id: &UserId) -> Result<bool> {
        Ok(self.get_room_power_levels().await?.user_can_invite(user_id))
    }

    /// Returns true if the user with the given user_id is able to kick in the
    /// room.
    ///
    /// The call may fail if there is an error in getting the power levels.
    pub async fn can_user_kick(&self, user_id: &UserId) -> Result<bool> {
        Ok(self.get_room_power_levels().await?.user_can_kick(user_id))
    }

    /// Returns true if the user with the given user_id is able to send a
    /// specific state event type in the room.
    ///
    /// The call may fail if there is an error in getting the power levels.
    pub async fn can_user_send_state(
        &self,
        user_id: &UserId,
        state_event: StateEventType,
    ) -> Result<bool> {
        Ok(self.get_room_power_levels().await?.user_can_send_state(user_id, state_event))
    }

    /// Returns true if the user with the given user_id is able to send a
    /// specific message type in the room.
    ///
    /// The call may fail if there is an error in getting the power levels.
    pub async fn can_user_send_message(
        &self,
        user_id: &UserId,
        message: MessageLikeEventType,
    ) -> Result<bool> {
        Ok(self.get_room_power_levels().await?.user_can_send_message(user_id, message))
    }

    /// Returns true if the user with the given user_id is able to trigger a
    /// notification in the room.
    ///
    /// The call may fail if there is an error in getting the power levels.
    pub async fn can_user_trigger_room_notification(&self, user_id: &UserId) -> Result<bool> {
        Ok(self.get_room_power_levels().await?.user_can_trigger_room_notification(user_id))
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
        let acl = acl_ev.as_ref().and_then(|ev| match ev {
            SyncOrStrippedState::Sync(ev) => ev.as_original().map(|ev| &ev.content),
            SyncOrStrippedState::Stripped(ev) => Some(&ev.content),
        });

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
    pub async fn event_push_actions<T>(&self, event: &Raw<T>) -> Result<Option<Vec<Action>>> {
        let Some(push_context) = self.push_context().await? else {
            debug!("Could not aggregate push context");
            return Ok(None);
        };

        let push_rules = self.client().account().push_rules().await?;

        Ok(Some(push_rules.get_actions(event, &push_context).to_owned()))
    }

    /// The membership details of the (latest) invite for the logged-in user in
    /// this room.
    pub async fn invite_details(&self) -> Result<Invite> {
        let state = self.state();
        if state != RoomState::Invited {
            return Err(Error::WrongRoomState(WrongRoomState::new("Invited", state)));
        }

        let invitee = self
            .get_member_no_sync(self.own_user_id())
            .await?
            .ok_or_else(|| Error::UnknownError(Box::new(InvitationError::EventMissing)))?;
        let event = invitee.event();
        let inviter_id = event.sender();
        let inviter = self.get_member_no_sync(inviter_id).await?;
        Ok(Invite { invitee, inviter })
    }

    /// Forget this room.
    ///
    /// This communicates to the homeserver that it should forget the room.
    ///
    /// Only left rooms can be forgotten.
    pub async fn forget(&self) -> Result<()> {
        let state = self.state();
        if state != RoomState::Left {
            return Err(Error::WrongRoomState(WrongRoomState::new("Left", state)));
        }

        let request = forget_room::v3::Request::new(self.inner.room_id().to_owned());
        let _response = self.client.send(request, None).await?;
        self.client.store().remove_room(self.inner.room_id()).await?;

        Ok(())
    }

    fn ensure_room_joined(&self) -> Result<()> {
        let state = self.state();
        if state == RoomState::Joined {
            Ok(())
        } else {
            Err(Error::WrongRoomState(WrongRoomState::new("Joined", state)))
        }
    }

    /// Get the notification mode
    pub async fn notification_mode(&self) -> Option<RoomNotificationMode> {
        if !matches!(self.state(), RoomState::Joined) {
            return None;
        }
        let notification_settings = self.client().notification_settings().await;

        // Get the user-defined mode if available
        let notification_mode =
            notification_settings.get_user_defined_room_notification_mode(self.room_id()).await;
        if notification_mode.is_some() {
            notification_mode
        } else if let Ok(is_encrypted) = self.is_encrypted().await {
            // Otherwise, if encrypted status is available, get the default mode for this
            // type of room.
            // From the point of view of notification settings, a `one-to-one` room is one
            // that involves exactly two people.
            let is_one_to_one = IsOneToOne::from(self.active_members_count() == 2);
            let default_mode = notification_settings
                .get_default_room_notification_mode(IsEncrypted::from(is_encrypted), is_one_to_one)
                .await;
            Some(default_mode)
        } else {
            None
        }
    }

    /// Get the user-defined notification mode
    pub async fn user_defined_notification_mode(&self) -> Option<RoomNotificationMode> {
        if !matches!(self.state(), RoomState::Joined) {
            return None;
        }
        let notification_settings = self.client().notification_settings().await;

        // Get the user-defined mode if available
        notification_settings.get_user_defined_room_notification_mode(self.room_id()).await
    }
}

/// Details of the (latest) invite.
#[derive(Debug, Clone)]
pub struct Invite {
    /// Who has been invited.
    pub invitee: RoomMember,
    /// Who sent the invite.
    pub inviter: Option<RoomMember>,
}

#[derive(Error, Debug)]
enum InvitationError {
    #[error("No membership event found")]
    EventMissing,
}

/// Receipts to send all at once.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Receipts {
    /// Fully-read marker (room account data).
    pub fully_read: Option<OwnedEventId>,
    /// Read receipt (public ephemeral room event).
    pub public_read_receipt: Option<OwnedEventId>,
    /// Read receipt (private ephemeral room event).
    pub private_read_receipt: Option<OwnedEventId>,
}

impl Receipts {
    /// Create an empty `Receipts`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the last event the user has read.
    ///
    /// It means that the user has read all the events before this event.
    ///
    /// This is a private marker only visible by the user.
    ///
    /// Note that this is technically not a receipt as it is persisted in the
    /// room account data.
    pub fn fully_read_marker(mut self, event_id: impl Into<Option<OwnedEventId>>) -> Self {
        self.fully_read = event_id.into();
        self
    }

    /// Set the last event presented to the user and forward it to the other
    /// users in the room.
    ///
    /// This is used to reset the unread messages/notification count and
    /// advertise to other users the last event that the user has likely seen.
    pub fn public_read_receipt(mut self, event_id: impl Into<Option<OwnedEventId>>) -> Self {
        self.public_read_receipt = event_id.into();
        self
    }

    /// Set the last event presented to the user and don't forward it.
    ///
    /// This is used to reset the unread messages/notification count.
    pub fn private_read_receipt(mut self, event_id: impl Into<Option<OwnedEventId>>) -> Self {
        self.private_read_receipt = event_id.into();
        self
    }

    /// Whether this `Receipts` is empty.
    pub fn is_empty(&self) -> bool {
        self.fully_read.is_none()
            && self.public_read_receipt.is_none()
            && self.private_read_receipt.is_none()
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use matrix_sdk_base::SessionMeta;
    use matrix_sdk_test::{
        async_test, test_json, JoinedRoomBuilder, StateTestEvent, SyncResponseBuilder,
    };
    use ruma::{device_id, user_id};
    use wiremock::{
        matchers::{header, method, path_regex},
        Mock, MockServer, ResponseTemplate,
    };

    use crate::{
        config::RequestConfig,
        matrix_auth::{MatrixSession, MatrixSessionTokens},
        Client,
    };

    #[cfg(all(feature = "sqlite", feature = "e2e-encryption"))]
    #[async_test]
    async fn test_cache_invalidation_while_encrypt() {
        let sqlite_path = std::env::temp_dir().join("cache_invalidation_while_encrypt.db");
        let session = MatrixSession {
            meta: SessionMeta {
                user_id: user_id!("@example:localhost").to_owned(),
                device_id: device_id!("DEVICEID").to_owned(),
            },
            tokens: MatrixSessionTokens { access_token: "1234".to_owned(), refresh_token: None },
        };

        let client = Client::builder()
            .homeserver_url("http://localhost:1234")
            .request_config(RequestConfig::new().disable_retry())
            .sqlite_store(&sqlite_path, None)
            .build()
            .await
            .unwrap();
        client.matrix_auth().restore_session(session.clone()).await.unwrap();

        client.encryption().enable_cross_process_store_lock("client1".to_owned()).await.unwrap();

        // Mock receiving an event to create an internal room.
        let server = MockServer::start().await;
        let room_id = &test_json::DEFAULT_SYNC_ROOM_ID;
        {
            Mock::given(method("GET"))
                .and(path_regex(r"^/_matrix/client/r0/rooms/.*/state/m.*room.*encryption.?"))
                .and(header("authorization", "Bearer 1234"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(&*test_json::sync_events::ENCRYPTION_CONTENT),
                )
                .mount(&server)
                .await;
            let response = SyncResponseBuilder::default()
                .add_joined_room(
                    JoinedRoomBuilder::default()
                        .add_state_event(StateTestEvent::Member)
                        .add_state_event(StateTestEvent::PowerLevels)
                        .add_state_event(StateTestEvent::Encryption),
                )
                .build_sync_response();
            client.base_client().receive_sync_response(response).await.unwrap();
        }

        let room = client.get_room(room_id).expect("Room should exist");

        // Step 1, preshare the room keys.
        room.preshare_room_key().await.unwrap();

        // Step 2, force lock invalidation by pretending another client obtained the
        // lock.
        {
            let client = Client::builder()
                .homeserver_url("http://localhost:1234")
                .request_config(RequestConfig::new().disable_retry())
                .sqlite_store(&sqlite_path, None)
                .build()
                .await
                .unwrap();
            client.matrix_auth().restore_session(session.clone()).await.unwrap();
            client
                .encryption()
                .enable_cross_process_store_lock("client2".to_owned())
                .await
                .unwrap();

            let guard = client.encryption().spin_lock_store(None).await.unwrap();
            assert!(guard.is_some());
        }

        // Step 3, take the crypto-store lock.
        let guard = client.encryption().spin_lock_store(None).await.unwrap();
        assert!(guard.is_some());

        // Step 4, try to encrypt a message.
        let olm = client.olm_machine().await;
        let olm = olm.as_ref().expect("Olm machine wasn't started");

        // Now pretend we're encrypting an event; the olm machine shouldn't rely on
        // caching the outgoing session before.
        let _encrypted_content = olm
            .encrypt_room_event_raw(room.room_id(), serde_json::json!({}), "test-event")
            .await
            .unwrap();
    }
}
