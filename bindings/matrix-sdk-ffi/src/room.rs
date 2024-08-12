use std::sync::Arc;

use anyhow::{Context, Result};
use matrix_sdk::{
    event_cache::paginator::PaginatorError,
    room::{
        edit::EditedContent, power_levels::RoomPowerLevelChanges, Room as SdkRoom, RoomMemberRole,
    },
    ComposerDraft as SdkComposerDraft, ComposerDraftType as SdkComposerDraftType,
    RoomHero as SdkRoomHero, RoomMemberships, RoomState,
};
use matrix_sdk_ui::timeline::{PaginationError, RoomExt, TimelineFocus};
use mime::Mime;
use ruma::{
    api::client::room::report_content,
    assign,
    events::{
        call::notify,
        room::{
            avatar::ImageInfo as RumaAvatarImageInfo,
            message::RoomMessageEventContentWithoutRelation,
            power_levels::RoomPowerLevels as RumaPowerLevels, MediaSource,
        },
        TimelineEventType,
    },
    EventId, Int, RoomAliasId, UserId,
};
use tokio::sync::RwLock;
use tracing::error;

use super::RUNTIME;
use crate::{
    chunk_iterator::ChunkIterator,
    error::{ClientError, MediaInfoError, RoomError},
    event::{MessageLikeEventType, StateEventType},
    room_info::RoomInfo,
    room_member::RoomMember,
    ruma::{ImageInfo, Mentions, NotifyType},
    timeline::{FocusEventError, ReceiptType, Timeline},
    utils::u64_to_uint,
    TaskHandle,
};

#[derive(uniffi::Enum)]
pub enum Membership {
    Invited,
    Joined,
    Left,
}

impl From<RoomState> for Membership {
    fn from(value: RoomState) -> Self {
        match value {
            RoomState::Invited => Membership::Invited,
            RoomState::Joined => Membership::Joined,
            RoomState::Left => Membership::Left,
        }
    }
}

pub(crate) type TimelineLock = Arc<RwLock<Option<Arc<Timeline>>>>;

#[derive(uniffi::Object)]
pub struct Room {
    pub(super) inner: SdkRoom,
    timeline: TimelineLock,
}

impl Room {
    pub(crate) fn new(inner: SdkRoom) -> Self {
        Room { inner, timeline: Default::default() }
    }

    pub(crate) fn with_timeline(inner: SdkRoom, timeline: TimelineLock) -> Self {
        Room { inner, timeline }
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl Room {
    pub fn id(&self) -> String {
        self.inner.room_id().to_string()
    }

    /// Returns the room's name from the state event if available, otherwise
    /// compute a room name based on the room's nature (DM or not) and number of
    /// members.
    pub fn display_name(&self) -> Option<String> {
        Some(self.inner.cached_display_name()?.to_string())
    }

    /// The raw name as present in the room state event.
    pub fn raw_name(&self) -> Option<String> {
        self.inner.name()
    }

    pub fn topic(&self) -> Option<String> {
        self.inner.topic()
    }

    pub fn avatar_url(&self) -> Option<String> {
        self.inner.avatar_url().map(|m| m.to_string())
    }

    pub fn is_direct(&self) -> bool {
        RUNTIME.block_on(self.inner.is_direct()).unwrap_or(false)
    }

    pub fn is_public(&self) -> bool {
        self.inner.is_public()
    }

    pub fn is_space(&self) -> bool {
        self.inner.is_space()
    }

    pub fn is_tombstoned(&self) -> bool {
        self.inner.is_tombstoned()
    }

    pub fn canonical_alias(&self) -> Option<String> {
        self.inner.canonical_alias().map(|a| a.to_string())
    }

    pub fn alternative_aliases(&self) -> Vec<String> {
        self.inner.alt_aliases().iter().map(|a| a.to_string()).collect()
    }

    pub fn membership(&self) -> Membership {
        self.inner.state().into()
    }

    /// Returns the room heroes for this room.
    pub fn heroes(&self) -> Vec<RoomHero> {
        self.inner.heroes().into_iter().map(Into::into).collect()
    }

    /// Is there a non expired membership with application "m.call" and scope
    /// "m.room" in this room.
    pub fn has_active_room_call(&self) -> bool {
        self.inner.has_active_room_call()
    }

    /// Returns a Vec of userId's that participate in the room call.
    ///
    /// MatrixRTC memberships with application "m.call" and scope "m.room" are
    /// considered. A user can occur twice if they join with two devices.
    /// convert to a set depending if the different users are required or the
    /// amount of sessions.
    ///
    /// The vector is ordered by oldest membership user to newest.
    pub fn active_room_call_participants(&self) -> Vec<String> {
        self.inner.active_room_call_participants().iter().map(|u| u.to_string()).collect()
    }

    /// For rooms one is invited to, retrieves the room member information for
    /// the user who invited the logged-in user to a room.
    pub async fn inviter(&self) -> Option<RoomMember> {
        if self.inner.state() == RoomState::Invited {
            self.inner.invite_details().await.ok().and_then(|a| a.inviter).map(|m| m.into())
        } else {
            None
        }
    }

    /// Forces the currently active room key, which is used to encrypt messages,
    /// to be rotated.
    ///
    /// A new room key will be crated and shared with all the room members the
    /// next time a message will be sent. You don't have to call this method,
    /// room keys will be rotated automatically when necessary. This method is
    /// still useful for debugging purposes.
    pub async fn discard_room_key(&self) -> Result<(), ClientError> {
        self.inner.discard_room_key().await?;
        Ok(())
    }

    pub async fn timeline(&self) -> Result<Arc<Timeline>, ClientError> {
        let mut write_guard = self.timeline.write().await;
        if let Some(timeline) = &*write_guard {
            Ok(timeline.clone())
        } else {
            let timeline = Timeline::new(self.inner.timeline().await?);
            *write_guard = Some(timeline.clone());
            Ok(timeline)
        }
    }

    /// Returns a timeline focused on the given event.
    ///
    /// Note: this timeline is independent from that returned with
    /// [`Self::timeline`], and as such it is not cached.
    pub async fn timeline_focused_on_event(
        &self,
        event_id: String,
        num_context_events: u16,
        internal_id_prefix: Option<String>,
    ) -> Result<Arc<Timeline>, FocusEventError> {
        let parsed_event_id = EventId::parse(&event_id).map_err(|err| {
            FocusEventError::InvalidEventId { event_id: event_id.clone(), err: err.to_string() }
        })?;

        let room = &self.inner;

        let mut builder = matrix_sdk_ui::timeline::Timeline::builder(room);

        if let Some(internal_id_prefix) = internal_id_prefix {
            builder = builder.with_internal_id_prefix(internal_id_prefix);
        }

        let timeline = match builder
            .with_focus(TimelineFocus::Event { target: parsed_event_id, num_context_events })
            .build()
            .await
        {
            Ok(t) => t,
            Err(err) => {
                if let matrix_sdk_ui::timeline::Error::PaginationError(
                    PaginationError::Paginator(PaginatorError::EventNotFound(..)),
                ) = err
                {
                    return Err(FocusEventError::EventNotFound { event_id: event_id.to_string() });
                }
                return Err(FocusEventError::Other { msg: err.to_string() });
            }
        };

        Ok(Timeline::new(timeline))
    }

    pub async fn pinned_events_timeline(
        &self,
        internal_id_prefix: Option<String>,
        max_events_to_load: u16,
    ) -> Result<Arc<Timeline>, ClientError> {
        let room = &self.inner;

        let mut builder = matrix_sdk_ui::timeline::Timeline::builder(room);

        if let Some(internal_id_prefix) = internal_id_prefix {
            builder = builder.with_internal_id_prefix(internal_id_prefix);
        }

        let timeline =
            builder.with_focus(TimelineFocus::PinnedEvents { max_events_to_load }).build().await?;

        Ok(Timeline::new(timeline))
    }

    pub fn is_encrypted(&self) -> Result<bool, ClientError> {
        Ok(RUNTIME.block_on(self.inner.is_encrypted())?)
    }

    pub async fn members(&self) -> Result<Arc<RoomMembersIterator>, ClientError> {
        Ok(Arc::new(RoomMembersIterator::new(self.inner.members(RoomMemberships::empty()).await?)))
    }

    pub async fn members_no_sync(&self) -> Result<Arc<RoomMembersIterator>, ClientError> {
        Ok(Arc::new(RoomMembersIterator::new(
            self.inner.members_no_sync(RoomMemberships::empty()).await?,
        )))
    }

    pub async fn member(&self, user_id: String) -> Result<RoomMember, ClientError> {
        let user_id = UserId::parse(&*user_id).context("Invalid user id.")?;
        let member = self.inner.get_member(&user_id).await?.context("User not found")?;
        Ok(member.into())
    }

    pub async fn member_avatar_url(&self, user_id: String) -> Result<Option<String>, ClientError> {
        let user_id = UserId::parse(&*user_id).context("Invalid user id.")?;
        let member = self.inner.get_member(&user_id).await?.context("User not found")?;
        let avatar_url_string = member.avatar_url().map(|m| m.to_string());
        Ok(avatar_url_string)
    }

    pub async fn member_display_name(
        &self,
        user_id: String,
    ) -> Result<Option<String>, ClientError> {
        let user_id = UserId::parse(&*user_id).context("Invalid user id.")?;
        let member = self.inner.get_member(&user_id).await?.context("User not found")?;
        let avatar_url_string = member.display_name().map(|m| m.to_owned());
        Ok(avatar_url_string)
    }

    pub async fn room_info(&self) -> Result<RoomInfo, ClientError> {
        Ok(RoomInfo::new(&self.inner).await?)
    }

    pub fn subscribe_to_room_info_updates(
        self: Arc<Self>,
        listener: Box<dyn RoomInfoListener>,
    ) -> Arc<TaskHandle> {
        let mut subscriber = self.inner.subscribe_info();
        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            while subscriber.next().await.is_some() {
                match self.room_info().await {
                    Ok(room_info) => listener.call(room_info),
                    Err(e) => {
                        error!("Failed to compute new RoomInfo: {e}");
                    }
                }
            }
        })))
    }

    pub async fn set_is_favourite(
        &self,
        is_favourite: bool,
        tag_order: Option<f64>,
    ) -> Result<(), ClientError> {
        self.inner.set_is_favourite(is_favourite, tag_order).await?;
        Ok(())
    }

    pub async fn set_is_low_priority(
        &self,
        is_low_priority: bool,
        tag_order: Option<f64>,
    ) -> Result<(), ClientError> {
        self.inner.set_is_low_priority(is_low_priority, tag_order).await?;
        Ok(())
    }

    /// Redacts an event from the room.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The ID of the event to redact
    ///
    /// * `reason` - The reason for the event being redacted (optional). its
    ///   transaction ID (optional). If not given one is created.
    pub async fn redact(
        &self,
        event_id: String,
        reason: Option<String>,
    ) -> Result<(), ClientError> {
        let event_id = EventId::parse(event_id)?;
        self.inner.redact(&event_id, reason.as_deref(), None).await?;
        Ok(())
    }

    pub fn active_members_count(&self) -> u64 {
        self.inner.active_members_count()
    }

    pub fn invited_members_count(&self) -> u64 {
        self.inner.invited_members_count()
    }

    pub fn joined_members_count(&self) -> u64 {
        self.inner.joined_members_count()
    }

    /// Reports an event from the room.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The ID of the event to report
    ///
    /// * `reason` - The reason for the event being reported (optional).
    ///
    /// * `score` - The score to rate this content as where -100 is most
    ///   offensive and 0 is inoffensive (optional).
    pub async fn report_content(
        &self,
        event_id: String,
        score: Option<i32>,
        reason: Option<String>,
    ) -> Result<(), ClientError> {
        let event_id = EventId::parse(event_id)?;
        let int_score = score.map(|value| value.into());
        self.inner
            .client()
            .send(
                report_content::v3::Request::new(
                    self.inner.room_id().into(),
                    event_id,
                    int_score,
                    reason,
                ),
                None,
            )
            .await?;
        Ok(())
    }

    /// Ignores a user.
    ///
    /// # Arguments
    ///
    /// * `user_id` - The ID of the user to ignore.
    pub async fn ignore_user(&self, user_id: String) -> Result<(), ClientError> {
        let user_id = UserId::parse(user_id)?;
        self.inner.client().account().ignore_user(&user_id).await?;
        Ok(())
    }

    /// Leave this room.
    ///
    /// Only invited and joined rooms can be left.
    pub async fn leave(&self) -> Result<(), ClientError> {
        self.inner.leave().await?;
        Ok(())
    }

    /// Join this room.
    ///
    /// Only invited and left rooms can be joined via this method.
    pub async fn join(&self) -> Result<(), ClientError> {
        self.inner.join().await?;
        Ok(())
    }

    /// Sets a new name to the room.
    pub async fn set_name(&self, name: String) -> Result<(), ClientError> {
        self.inner.set_name(name).await?;
        Ok(())
    }

    /// Sets a new topic in the room.
    pub async fn set_topic(&self, topic: String) -> Result<(), ClientError> {
        self.inner.set_room_topic(&topic).await?;
        Ok(())
    }

    /// Upload and set the room's avatar.
    ///
    /// This will upload the data produced by the reader to the homeserver's
    /// content repository, and set the room's avatar to the MXC URI for the
    /// uploaded file.
    ///
    /// # Arguments
    ///
    /// * `mime_type` - The mime description of the avatar, for example
    ///   image/jpeg
    /// * `data` - The raw data that will be uploaded to the homeserver's
    ///   content repository
    /// * `media_info` - The media info used as avatar image info.
    pub async fn upload_avatar(
        &self,
        mime_type: String,
        data: Vec<u8>,
        media_info: Option<ImageInfo>,
    ) -> Result<(), ClientError> {
        let mime: Mime = mime_type.parse()?;
        self.inner
            .upload_avatar(
                &mime,
                data,
                media_info
                    .map(TryInto::try_into)
                    .transpose()
                    .map_err(|_| RoomError::InvalidMediaInfo)?,
            )
            .await?;
        Ok(())
    }

    /// Removes the current room avatar
    pub async fn remove_avatar(&self) -> Result<(), ClientError> {
        self.inner.remove_avatar().await?;
        Ok(())
    }

    pub async fn invite_user_by_id(&self, user_id: String) -> Result<(), ClientError> {
        let user =
            <&UserId>::try_from(user_id.as_str()).context("Could not create user from string")?;
        self.inner.invite_user_by_id(user).await?;
        Ok(())
    }

    pub async fn can_user_redact_own(&self, user_id: String) -> Result<bool, ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.can_user_redact_own(&user_id).await?)
    }

    pub async fn can_user_redact_other(&self, user_id: String) -> Result<bool, ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.can_user_redact_other(&user_id).await?)
    }

    pub async fn can_user_ban(&self, user_id: String) -> Result<bool, ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.can_user_ban(&user_id).await?)
    }

    pub async fn ban_user(
        &self,
        user_id: String,
        reason: Option<String>,
    ) -> Result<(), ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.ban_user(&user_id, reason.as_deref()).await?)
    }

    pub async fn unban_user(
        &self,
        user_id: String,
        reason: Option<String>,
    ) -> Result<(), ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.unban_user(&user_id, reason.as_deref()).await?)
    }

    pub async fn can_user_invite(&self, user_id: String) -> Result<bool, ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.can_user_invite(&user_id).await?)
    }

    pub async fn can_user_kick(&self, user_id: String) -> Result<bool, ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.can_user_kick(&user_id).await?)
    }

    pub async fn kick_user(
        &self,
        user_id: String,
        reason: Option<String>,
    ) -> Result<(), ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.kick_user(&user_id, reason.as_deref()).await?)
    }

    pub async fn can_user_send_state(
        &self,
        user_id: String,
        state_event: StateEventType,
    ) -> Result<bool, ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.can_user_send_state(&user_id, state_event.into()).await?)
    }

    pub async fn can_user_send_message(
        &self,
        user_id: String,
        message: MessageLikeEventType,
    ) -> Result<bool, ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.can_user_send_message(&user_id, message.into()).await?)
    }

    pub async fn can_user_pin_unpin(&self, user_id: String) -> Result<bool, ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.can_user_pin_unpin(&user_id).await?)
    }

    pub async fn can_user_trigger_room_notification(
        &self,
        user_id: String,
    ) -> Result<bool, ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.can_user_trigger_room_notification(&user_id).await?)
    }

    pub fn own_user_id(&self) -> String {
        self.inner.own_user_id().to_string()
    }

    pub async fn typing_notice(&self, is_typing: bool) -> Result<(), ClientError> {
        Ok(self.inner.typing_notice(is_typing).await?)
    }

    pub fn subscribe_to_typing_notifications(
        self: Arc<Self>,
        listener: Box<dyn TypingNotificationsListener>,
    ) -> Arc<TaskHandle> {
        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            let (_event_handler_drop_guard, mut subscriber) =
                self.inner.subscribe_to_typing_notifications();
            while let Ok(typing_user_ids) = subscriber.recv().await {
                let typing_user_ids =
                    typing_user_ids.into_iter().map(|user_id| user_id.to_string()).collect();
                listener.call(typing_user_ids);
            }
        })))
    }

    /// Set (or unset) a flag on the room to indicate that the user has
    /// explicitly marked it as unread.
    pub async fn set_unread_flag(&self, new_value: bool) -> Result<(), ClientError> {
        Ok(self.inner.set_unread_flag(new_value).await?)
    }

    /// Mark a room as read, by attaching a read receipt on the latest event.
    ///
    /// Note: this does NOT unset the unread flag; it's the caller's
    /// responsibility to do so, if needs be.
    pub async fn mark_as_read(&self, receipt_type: ReceiptType) -> Result<(), ClientError> {
        let timeline = self.timeline().await?;

        timeline.mark_as_read(receipt_type).await?;
        Ok(())
    }

    pub async fn get_power_levels(&self) -> Result<RoomPowerLevels, ClientError> {
        let power_levels = self.inner.room_power_levels().await?;
        Ok(RoomPowerLevels::from(power_levels))
    }

    pub async fn apply_power_level_changes(
        &self,
        changes: RoomPowerLevelChanges,
    ) -> Result<(), ClientError> {
        self.inner.apply_power_level_changes(changes).await?;
        Ok(())
    }

    pub async fn update_power_levels_for_users(
        &self,
        updates: Vec<UserPowerLevelUpdate>,
    ) -> Result<(), ClientError> {
        let updates = updates
            .iter()
            .map(|update| {
                let user_id: &UserId = update.user_id.as_str().try_into()?;
                let power_level = Int::new(update.power_level).context("Invalid power level")?;
                Ok((user_id, power_level))
            })
            .collect::<Result<Vec<_>>>()?;

        self.inner
            .update_power_levels(updates)
            .await
            .map_err(|e| ClientError::Generic { msg: e.to_string() })?;
        Ok(())
    }

    pub async fn suggested_role_for_user(
        &self,
        user_id: String,
    ) -> Result<RoomMemberRole, ClientError> {
        let user_id = UserId::parse(&user_id)?;
        Ok(self.inner.get_suggested_user_role(&user_id).await?)
    }

    pub async fn reset_power_levels(&self) -> Result<RoomPowerLevels, ClientError> {
        Ok(RoomPowerLevels::from(self.inner.reset_power_levels().await?))
    }

    pub async fn matrix_to_permalink(&self) -> Result<String, ClientError> {
        Ok(self.inner.matrix_to_permalink().await?.to_string())
    }

    pub async fn matrix_to_event_permalink(&self, event_id: String) -> Result<String, ClientError> {
        let event_id = EventId::parse(event_id)?;
        Ok(self.inner.matrix_to_event_permalink(event_id).await?.to_string())
    }

    /// This will only send a call notification event if appropriate.
    ///
    /// This function is supposed to be called whenever the user creates a room
    /// call. It will send a `m.call.notify` event if:
    ///  - there is not yet a running call.
    ///
    /// It will configure the notify type: ring or notify based on:
    ///  - is this a DM room -> ring
    ///  - is this a group with more than one other member -> notify
    pub async fn send_call_notification_if_needed(&self) -> Result<(), ClientError> {
        self.inner.send_call_notification_if_needed().await?;
        Ok(())
    }

    /// Send a call notification event in the current room.
    ///
    /// This is only supposed to be used in **custom** situations where the user
    /// explicitly chooses to send a `m.call.notify` event to invite/notify
    /// someone explicitly in unusual conditions. The default should be to
    /// use `send_call_notification_if_necessary` just before a new room call is
    /// created/joined.
    ///
    /// One example could be that the UI allows to start a call with a subset of
    /// users of the room members first. And then later on the user can
    /// invite more users to the call.
    pub async fn send_call_notification(
        &self,
        call_id: String,
        application: RtcApplicationType,
        notify_type: NotifyType,
        mentions: Mentions,
    ) -> Result<(), ClientError> {
        self.inner
            .send_call_notification(
                call_id,
                application.into(),
                notify_type.into(),
                mentions.into(),
            )
            .await?;
        Ok(())
    }

    /// Returns whether the send queue for that particular room is enabled or
    /// not.
    pub fn is_send_queue_enabled(&self) -> bool {
        self.inner.send_queue().is_enabled()
    }

    /// Enable or disable the send queue for that particular room.
    pub fn enable_send_queue(&self, enable: bool) {
        self.inner.send_queue().set_enabled(enable);
    }

    /// Store the given `ComposerDraft` in the state store using the current
    /// room id, as identifier.
    pub async fn save_composer_draft(&self, draft: ComposerDraft) -> Result<(), ClientError> {
        Ok(self.inner.save_composer_draft(draft.try_into()?).await?)
    }

    /// Retrieve the `ComposerDraft` stored in the state store for this room.
    pub async fn load_composer_draft(&self) -> Result<Option<ComposerDraft>, ClientError> {
        Ok(self.inner.load_composer_draft().await?.map(Into::into))
    }

    /// Remove the `ComposerDraft` stored in the state store for this room.
    pub async fn clear_composer_draft(&self) -> Result<(), ClientError> {
        Ok(self.inner.clear_composer_draft().await?)
    }

    /// Edit an event given its event id.
    ///
    /// Useful outside the context of a timeline, or when a timeline doesn't
    /// have the full content of an event.
    pub async fn edit(
        &self,
        event_id: String,
        new_content: Arc<RoomMessageEventContentWithoutRelation>,
    ) -> Result<(), ClientError> {
        let event_id = EventId::parse(event_id)?;

        let replacement_event = self
            .inner
            .make_edit_event(&event_id, EditedContent::RoomMessage((*new_content).clone()))
            .await?;

        self.inner.send(replacement_event).await?;
        Ok(())
    }
}

/// Generates a `matrix.to` permalink to the given room alias.
#[uniffi::export]
pub fn matrix_to_room_alias_permalink(
    room_alias: String,
) -> std::result::Result<String, ClientError> {
    let room_alias = RoomAliasId::parse(room_alias)?;
    Ok(room_alias.matrix_to_uri().to_string())
}

#[derive(uniffi::Record)]
pub struct RoomPowerLevels {
    /// The level required to ban a user.
    pub ban: i64,
    /// The level required to invite a user.
    pub invite: i64,
    /// The level required to kick a user.
    pub kick: i64,
    /// The level required to redact an event.
    pub redact: i64,
    /// The default level required to send message events.
    pub events_default: i64,
    /// The default level required to send state events.
    pub state_default: i64,
    /// The default power level for every user in the room.
    pub users_default: i64,
    /// The level required to change the room's name.
    pub room_name: i64,
    /// The level required to change the room's avatar.
    pub room_avatar: i64,
    /// The level required to change the room's topic.
    pub room_topic: i64,
}

impl From<RumaPowerLevels> for RoomPowerLevels {
    fn from(value: RumaPowerLevels) -> Self {
        fn state_event_level_for(
            power_levels: &RumaPowerLevels,
            event_type: &TimelineEventType,
        ) -> i64 {
            let default_state: i64 = power_levels.state_default.into();
            power_levels.events.get(event_type).map_or(default_state, |&level| level.into())
        }
        Self {
            ban: value.ban.into(),
            invite: value.invite.into(),
            kick: value.kick.into(),
            redact: value.redact.into(),
            events_default: value.events_default.into(),
            state_default: value.state_default.into(),
            users_default: value.users_default.into(),
            room_name: state_event_level_for(&value, &TimelineEventType::RoomName),
            room_avatar: state_event_level_for(&value, &TimelineEventType::RoomAvatar),
            room_topic: state_event_level_for(&value, &TimelineEventType::RoomTopic),
        }
    }
}

#[uniffi::export(callback_interface)]
pub trait RoomInfoListener: Sync + Send {
    fn call(&self, room_info: RoomInfo);
}

#[uniffi::export(callback_interface)]
pub trait TypingNotificationsListener: Sync + Send {
    fn call(&self, typing_user_ids: Vec<String>);
}

#[derive(uniffi::Object)]
pub struct RoomMembersIterator {
    chunk_iterator: ChunkIterator<matrix_sdk::room::RoomMember>,
}

impl RoomMembersIterator {
    fn new(members: Vec<matrix_sdk::room::RoomMember>) -> Self {
        Self { chunk_iterator: ChunkIterator::new(members) }
    }
}

#[uniffi::export]
impl RoomMembersIterator {
    fn len(&self) -> u32 {
        self.chunk_iterator.len()
    }

    fn next_chunk(&self, chunk_size: u32) -> Option<Vec<RoomMember>> {
        self.chunk_iterator
            .next(chunk_size)
            .map(|members| members.into_iter().map(|m| m.into()).collect())
    }
}

/// Information about a member considered to be a room hero.
#[derive(uniffi::Record)]
pub struct RoomHero {
    /// The user ID of the hero.
    user_id: String,
    /// The display name of the hero.
    display_name: Option<String>,
    /// The avatar URL of the hero.
    avatar_url: Option<String>,
}

impl From<SdkRoomHero> for RoomHero {
    fn from(value: SdkRoomHero) -> Self {
        Self {
            user_id: value.user_id.to_string(),
            display_name: value.display_name.clone(),
            avatar_url: value.avatar_url.as_ref().map(ToString::to_string),
        }
    }
}

/// An update for a particular user's power level within the room.
#[derive(uniffi::Record)]
pub struct UserPowerLevelUpdate {
    /// The user ID of the user to update.
    user_id: String,
    /// The power level to assign to the user.
    power_level: i64,
}

impl TryFrom<ImageInfo> for RumaAvatarImageInfo {
    type Error = MediaInfoError;

    fn try_from(value: ImageInfo) -> Result<Self, MediaInfoError> {
        let thumbnail_url = if let Some(media_source) = value.thumbnail_source {
            match media_source.as_ref() {
                MediaSource::Plain(mxc_uri) => Some(mxc_uri.clone()),
                MediaSource::Encrypted(_) => return Err(MediaInfoError::InvalidField),
            }
        } else {
            None
        };

        Ok(assign!(RumaAvatarImageInfo::new(), {
            height: value.height.map(u64_to_uint),
            width: value.width.map(u64_to_uint),
            mimetype: value.mimetype,
            size: value.size.map(u64_to_uint),
            thumbnail_info: value.thumbnail_info.map(Into::into).map(Box::new),
            thumbnail_url: thumbnail_url,
            blurhash: value.blurhash,
        }))
    }
}

#[derive(uniffi::Enum)]
pub enum RtcApplicationType {
    Call,
}
impl From<RtcApplicationType> for notify::ApplicationType {
    fn from(value: RtcApplicationType) -> Self {
        match value {
            RtcApplicationType::Call => notify::ApplicationType::Call,
        }
    }
}

/// Current draft of the composer for the room.
#[derive(uniffi::Record)]
pub struct ComposerDraft {
    /// The draft content in plain text.
    pub plain_text: String,
    /// If the message is formatted in HTML, the HTML representation of the
    /// message.
    pub html_text: Option<String>,
    /// The type of draft.
    pub draft_type: ComposerDraftType,
}

impl From<SdkComposerDraft> for ComposerDraft {
    fn from(value: SdkComposerDraft) -> Self {
        let SdkComposerDraft { plain_text, html_text, draft_type } = value;
        Self { plain_text, html_text, draft_type: draft_type.into() }
    }
}

impl TryFrom<ComposerDraft> for SdkComposerDraft {
    type Error = ruma::IdParseError;

    fn try_from(value: ComposerDraft) -> std::result::Result<Self, Self::Error> {
        let ComposerDraft { plain_text, html_text, draft_type } = value;
        Ok(Self { plain_text, html_text, draft_type: draft_type.try_into()? })
    }
}

/// The type of draft of the composer.
#[derive(uniffi::Enum)]
pub enum ComposerDraftType {
    /// The draft is a new message.
    NewMessage,
    /// The draft is a reply to an event.
    Reply {
        /// The ID of the event being replied to.
        event_id: String,
    },
    /// The draft is an edit of an event.
    Edit {
        /// The ID of the event being edited.
        event_id: String,
    },
}

impl From<SdkComposerDraftType> for ComposerDraftType {
    fn from(value: SdkComposerDraftType) -> Self {
        match value {
            SdkComposerDraftType::NewMessage => Self::NewMessage,
            SdkComposerDraftType::Reply { event_id } => Self::Reply { event_id: event_id.into() },
            SdkComposerDraftType::Edit { event_id } => Self::Edit { event_id: event_id.into() },
        }
    }
}

impl TryFrom<ComposerDraftType> for SdkComposerDraftType {
    type Error = ruma::IdParseError;

    fn try_from(value: ComposerDraftType) -> std::result::Result<Self, Self::Error> {
        let draft_type = match value {
            ComposerDraftType::NewMessage => Self::NewMessage,
            ComposerDraftType::Reply { event_id } => Self::Reply { event_id: event_id.try_into()? },
            ComposerDraftType::Edit { event_id } => Self::Edit { event_id: event_id.try_into()? },
        };

        Ok(draft_type)
    }
}
