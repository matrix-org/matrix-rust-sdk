use std::{convert::TryFrom, fs, sync::Arc};

use anyhow::{anyhow, bail, Context, Result};
use futures_util::{pin_mut, StreamExt};
use matrix_sdk::{
    attachment::{
        AttachmentConfig, AttachmentInfo, BaseAudioInfo, BaseFileInfo, BaseImageInfo,
        BaseThumbnailInfo, BaseVideoInfo, Thumbnail,
    },
    room::{Receipts, Room as SdkRoom},
    ruma::{
        api::client::{receipt::create_receipt::v3::ReceiptType, room::report_content},
        events::{
            location::{AssetType as RumaAssetType, LocationContent, ZoomLevel},
            receipt::ReceiptThread,
            relation::{Annotation, Replacement},
            room::message::{
                ForwardThread, LocationMessageEventContent, MessageType, Relation,
                RoomMessageEvent, RoomMessageEventContent,
            },
        },
        EventId, UserId,
    },
    RoomMemberships, RoomState,
};
use matrix_sdk_ui::timeline::{BackPaginationStatus, RoomExt, Timeline};
use mime::Mime;
use tokio::{
    sync::{Mutex, RwLock},
    task::{AbortHandle, JoinHandle},
};
use tracing::{error, info};

use super::RUNTIME;
use crate::{
    client::ProgressWatcher,
    error::{ClientError, RoomError},
    room_member::{MessageLikeEventType, RoomMember, StateEventType},
    timeline::{
        AudioInfo, FileInfo, ImageInfo, ThumbnailInfo, TimelineDiff, TimelineItem,
        TimelineListener, VideoInfo,
    },
    TaskHandle,
};

#[derive(uniffi::Enum)]
pub enum Membership {
    Invited,
    Joined,
    Left,
}

pub(crate) type TimelineLock = Arc<RwLock<Option<Arc<Timeline>>>>;

#[derive(uniffi::Object)]
pub struct Room {
    inner: SdkRoom,
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

    pub fn name(&self) -> Option<String> {
        self.inner.name()
    }

    pub fn topic(&self) -> Option<String> {
        self.inner.topic()
    }

    pub fn avatar_url(&self) -> Option<String> {
        self.inner.avatar_url().map(|m| m.to_string())
    }

    pub fn is_direct(&self) -> bool {
        RUNTIME.block_on(async move { self.inner.is_direct().await.unwrap_or(false) })
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
        match self.inner.state() {
            RoomState::Invited => Membership::Invited,
            RoomState::Joined => Membership::Joined,
            RoomState::Left => Membership::Left,
        }
    }

    pub fn inviter(&self) -> Option<Arc<RoomMember>> {
        if self.inner.state() == RoomState::Invited {
            RUNTIME.block_on(async move {
                self.inner
                    .invite_details()
                    .await
                    .ok()
                    .and_then(|a| a.inviter)
                    .map(|m| Arc::new(RoomMember::new(m)))
            })
        } else {
            None
        }
    }

    /// Removes the timeline.
    ///
    /// Timeline items cached in memory as well as timeline listeners are
    /// dropped.
    pub fn remove_timeline(&self) {
        RUNTIME.block_on(async {
            *self.timeline.write().await = None;
        });
    }

    pub fn retry_decryption(&self, session_ids: Vec<String>) {
        let timeline = match &*RUNTIME.block_on(self.timeline.read()) {
            Some(t) => Arc::clone(t),
            None => {
                error!("Timeline not set up, can't retry decryption");
                return;
            }
        };

        RUNTIME.spawn(async move {
            timeline.retry_decryption(&session_ids).await;
        });
    }

    pub fn fetch_members(&self) -> Result<Arc<TaskHandle>, ClientError> {
        let timeline = RUNTIME
            .block_on(self.timeline.read())
            .clone()
            .context("Timeline not set up, can't fetch members")?;

        Ok(Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            timeline.fetch_members().await;
        }))))
    }

    pub fn display_name(&self) -> Result<String, ClientError> {
        let r = self.inner.clone();
        RUNTIME.block_on(async move { Ok(r.display_name().await?.to_string()) })
    }

    pub fn is_encrypted(&self) -> Result<bool, ClientError> {
        let room = self.inner.clone();
        RUNTIME.block_on(async move {
            let is_encrypted = room.is_encrypted().await?;
            Ok(is_encrypted)
        })
    }

    pub fn members(&self) -> Result<Vec<Arc<RoomMember>>, ClientError> {
        let room = self.inner.clone();
        RUNTIME.block_on(async move {
            let members = room
                .members(RoomMemberships::empty())
                .await?
                .iter()
                .map(|m| Arc::new(RoomMember::new(m.clone())))
                .collect();
            Ok(members)
        })
    }

    pub fn member(&self, user_id: String) -> Result<Arc<RoomMember>, ClientError> {
        let room = self.inner.clone();
        let user_id = user_id;
        RUNTIME.block_on(async move {
            let user_id = UserId::parse(&*user_id).context("Invalid user id.")?;
            let member = room.get_member(&user_id).await?.context("No user found")?;
            Ok(Arc::new(RoomMember::new(member)))
        })
    }

    pub fn member_avatar_url(&self, user_id: String) -> Result<Option<String>, ClientError> {
        let room = self.inner.clone();
        let user_id = user_id;
        RUNTIME.block_on(async move {
            let user_id = UserId::parse(&*user_id).context("Invalid user id.")?;
            let member = room.get_member(&user_id).await?.context("No user found")?;
            let avatar_url_string = member.avatar_url().map(|m| m.to_string());
            Ok(avatar_url_string)
        })
    }

    pub fn member_display_name(&self, user_id: String) -> Result<Option<String>, ClientError> {
        let room = self.inner.clone();
        let user_id = user_id;
        RUNTIME.block_on(async move {
            let user_id = UserId::parse(&*user_id).context("Invalid user id.")?;
            let member = room.get_member(&user_id).await?.context("No user found")?;
            let avatar_url_string = member.display_name().map(|m| m.to_owned());
            Ok(avatar_url_string)
        })
    }

    pub fn add_timeline_listener(
        self: Arc<Self>,
        listener: Box<dyn TimelineListener>,
    ) -> RoomTimelineListenerResult {
        RUNTIME.block_on(async move {
            let timeline = self
                .timeline
                .write()
                .await
                .get_or_insert_with(|| {
                    let timeline = RUNTIME.block_on(self.inner.timeline());
                    Arc::new(timeline)
                })
                .clone();

            let (timeline_items, timeline_stream) = timeline.subscribe_batched().await;

            let timeline_stream = TaskHandle::new(RUNTIME.spawn(async move {
                pin_mut!(timeline_stream);

                while let Some(diffs) = timeline_stream.next().await {
                    listener.on_update(
                        diffs.into_iter().map(|d| Arc::new(TimelineDiff::new(d))).collect(),
                    );
                }
            }));

            RoomTimelineListenerResult {
                items: timeline_items.into_iter().map(TimelineItem::from_arc).collect(),
                items_stream: Arc::new(timeline_stream),
            }
        })
    }

    pub fn subscribe_to_back_pagination_status(
        &self,
        listener: Box<dyn BackPaginationStatusListener>,
    ) -> Result<Arc<TaskHandle>, ClientError> {
        let mut subscriber = match &*RUNTIME.block_on(self.timeline.read()) {
            Some(t) => t.back_pagination_status(),
            None => {
                return Err(anyhow!(
                    "Timeline not set up, can't subscribe to back-pagination status"
                )
                .into());
            }
        };

        Ok(Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            // Send the current state even if it hasn't changed right away.
            listener.on_update(subscriber.next_now());

            while let Some(status) = subscriber.next().await {
                listener.on_update(status);
            }
        }))))
    }

    /// Loads older messages into the timeline.
    ///
    /// Raises an exception if there are no timeline listeners.
    pub fn paginate_backwards(&self, opts: PaginationOptions) -> Result<(), ClientError> {
        RUNTIME.block_on(async move {
            let timeline: Arc<_> = self
                .timeline
                .read()
                .await
                .clone()
                .context("No timeline listeners registered, can't paginate")?;
            Ok(timeline.paginate_backwards(opts.into()).await?)
        })
    }

    pub fn send_read_receipt(&self, event_id: String) -> Result<(), ClientError> {
        let event_id = EventId::parse(event_id)?;

        RUNTIME.block_on(async move {
            self.inner
                .send_single_receipt(ReceiptType::Read, ReceiptThread::Unthreaded, event_id)
                .await?;
            Ok(())
        })
    }

    pub fn send_read_marker(
        &self,
        fully_read_event_id: String,
        read_receipt_event_id: Option<String>,
    ) -> Result<(), ClientError> {
        let fully_read =
            EventId::parse(fully_read_event_id).context("parsing fully read event ID")?;
        let read_receipt = read_receipt_event_id
            .map(EventId::parse)
            .transpose()
            .context("parsing read receipt event ID")?;
        let receipts =
            Receipts::new().fully_read_marker(fully_read).public_read_receipt(read_receipt);

        RUNTIME.block_on(async move {
            self.inner.send_multiple_receipts(receipts).await?;
            Ok(())
        })
    }

    pub fn send(&self, msg: Arc<RoomMessageEventContent>, txn_id: Option<String>) {
        let timeline = match &*RUNTIME.block_on(self.timeline.read()) {
            Some(t) => Arc::clone(t),
            None => {
                error!("Timeline not set up, can't send message");
                return;
            }
        };

        RUNTIME.spawn(async move {
            timeline.send((*msg).to_owned().into(), txn_id.as_deref().map(Into::into)).await;
        });
    }

    pub fn send_reply(
        &self,
        msg: String,
        in_reply_to_event_id: String,
        txn_id: Option<String>,
    ) -> Result<(), ClientError> {
        let timeline = match &*RUNTIME.block_on(self.timeline.read()) {
            Some(t) => Arc::clone(t),
            None => return Err(anyhow!("Timeline not set up, can't send message").into()),
        };

        let event_id: &EventId =
            in_reply_to_event_id.as_str().try_into().context("Failed to create EventId.")?;

        let reply_content = RUNTIME.block_on(async move {
            let timeline_event =
                self.inner.event(event_id).await.context("Couldn't find event.")?;

            let event_content = timeline_event
                .event
                .deserialize_as::<RoomMessageEvent>()
                .context("Couldn't deserialize event")?;

            let original_message =
                event_content.as_original().context("Couldn't retrieve original message.")?;

            anyhow::Ok(
                RoomMessageEventContent::text_markdown(msg)
                    .make_reply_to(original_message, ForwardThread::Yes),
            )
        })?;

        RUNTIME.spawn(async move {
            timeline.send(reply_content.into(), txn_id.as_deref().map(Into::into)).await;
        });
        Ok(())
    }

    pub fn edit(
        &self,
        new_msg: String,
        original_event_id: String,
        txn_id: Option<String>,
    ) -> Result<(), ClientError> {
        let timeline = match &*RUNTIME.block_on(self.timeline.read()) {
            Some(t) => Arc::clone(t),
            None => return Err(anyhow!("Timeline not set up, can't send message").into()),
        };

        let event_id: &EventId =
            original_event_id.as_str().try_into().context("Failed to create EventId.")?;

        let edited_content = RUNTIME.block_on(async move {
            let timeline_event =
                self.inner.event(event_id).await.context("Couldn't find event.")?;

            let event_content = timeline_event
                .event
                .deserialize_as::<RoomMessageEvent>()
                .context("Couldn't deserialise event")?;

            if self.inner.own_user_id() != event_content.sender() {
                bail!("Can't edit an event not sent by own user");
            }

            let replacement = Replacement::new(
                event_id.to_owned(),
                MessageType::text_markdown(new_msg.to_owned()).into(),
            );

            let mut edited_content = RoomMessageEventContent::text_markdown(new_msg);
            edited_content.relates_to = Some(Relation::Replacement(replacement));
            Ok(edited_content)
        })?;

        RUNTIME.spawn(async move {
            timeline.send(edited_content.into(), txn_id.as_deref().map(Into::into)).await;
        });
        Ok(())
    }

    /// Redacts an event from the room.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The ID of the event to redact
    ///
    /// * `reason` - The reason for the event being redacted (optional).
    ///
    /// * `txn_id` - A unique ID that can be attached to this event as
    /// its transaction ID (optional). If not given one is created.
    pub fn redact(
        &self,
        event_id: String,
        reason: Option<String>,
        txn_id: Option<String>,
    ) -> Result<(), ClientError> {
        RUNTIME.block_on(async move {
            let event_id = EventId::parse(event_id)?;
            self.inner.redact(&event_id, reason.as_deref(), txn_id.map(Into::into)).await?;
            Ok(())
        })
    }

    pub fn toggle_reaction(&self, event_id: String, key: String) -> Result<(), ClientError> {
        let timeline = match &*RUNTIME.block_on(self.timeline.read()) {
            Some(t) => Arc::clone(t),
            None => return Err(anyhow!("Timeline not set up, can't send message").into()),
        };

        RUNTIME.block_on(async move {
            let event_id = EventId::parse(event_id)?;
            timeline.toggle_reaction(&Annotation::new(event_id, key)).await?;
            Ok(())
        })
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
    pub fn report_content(
        &self,
        event_id: String,
        score: Option<i32>,
        reason: Option<String>,
    ) -> Result<(), ClientError> {
        let int_score = score.map(|value| value.into());
        RUNTIME.block_on(async move {
            let event_id = EventId::parse(event_id)?;
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
        })
    }

    /// Ignores a user.
    ///
    /// # Arguments
    ///
    /// * `event_id` - The ID of the user to ignore.
    pub fn ignore_user(&self, user_id: String) -> Result<(), ClientError> {
        RUNTIME.block_on(async move {
            let user_id = UserId::parse(user_id)?;
            self.inner.client().account().ignore_user(&user_id).await?;
            Ok(())
        })
    }

    /// Leave this room.
    ///
    /// Only invited and joined rooms can be left.
    pub fn leave(&self) -> Result<(), ClientError> {
        RUNTIME.block_on(async {
            self.inner.leave().await?;
            Ok(())
        })
    }

    /// Join this room.
    ///
    /// Only invited and left rooms can be joined via this method.
    pub fn join(&self) -> Result<(), ClientError> {
        RUNTIME.block_on(async {
            self.inner.join().await?;
            Ok(())
        })
    }

    /// Sets a new name to the room.
    pub fn set_name(&self, name: Option<String>) -> Result<(), ClientError> {
        RUNTIME.block_on(async move {
            self.inner.set_name(name).await?;
            Ok(())
        })
    }

    /// Sets a new topic in the room.
    pub fn set_topic(&self, topic: String) -> Result<(), ClientError> {
        RUNTIME.block_on(async move {
            self.inner.set_room_topic(&topic).await?;
            Ok(())
        })
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
    pub fn upload_avatar(&self, mime_type: String, data: Vec<u8>) -> Result<(), ClientError> {
        RUNTIME.block_on(async move {
            let mime: Mime = mime_type.parse()?;
            // TODO: We could add an FFI ImageInfo struct in the future
            self.inner.upload_avatar(&mime, data, None).await?;
            Ok(())
        })
    }

    /// Removes the current room avatar
    pub fn remove_avatar(&self) -> Result<(), ClientError> {
        RUNTIME.block_on(async move {
            self.inner.remove_avatar().await?;
            Ok(())
        })
    }

    pub fn invite_user_by_id(&self, user_id: String) -> Result<(), ClientError> {
        RUNTIME.block_on(async move {
            let user = <&UserId>::try_from(user_id.as_str())
                .context("Could not create user from string")?;
            self.inner.invite_user_by_id(user).await?;
            Ok(())
        })
    }

    pub fn fetch_details_for_event(&self, event_id: String) -> Result<(), ClientError> {
        let timeline = RUNTIME
            .block_on(self.timeline.read())
            .as_ref()
            .context("Timeline not set up, can't fetch event details")?
            .clone();

        RUNTIME.block_on(async move {
            let event_id = <&EventId>::try_from(event_id.as_str())?;
            timeline.fetch_details_for_event(event_id).await.context("Fetching event details")?;
            Ok(())
        })
    }

    pub fn send_image(
        self: Arc<Self>,
        url: String,
        thumbnail_url: String,
        image_info: ImageInfo,
        progress_watcher: Option<Box<dyn ProgressWatcher>>,
    ) -> Arc<SendAttachmentJoinHandle> {
        SendAttachmentJoinHandle::new(RUNTIME.spawn(async move {
            let mime_str =
                image_info.mimetype.as_ref().ok_or(RoomError::InvalidAttachmentMimeType)?;
            let mime_type =
                mime_str.parse::<Mime>().map_err(|_| RoomError::InvalidAttachmentMimeType)?;

            let base_image_info = BaseImageInfo::try_from(&image_info)
                .map_err(|_| RoomError::InvalidAttachmentData)?;

            let attachment_info = AttachmentInfo::Image(base_image_info);

            let attachment_config = match image_info.thumbnail_info {
                Some(thumbnail_image_info) => {
                    let thumbnail =
                        self.build_thumbnail_info(thumbnail_url, thumbnail_image_info)?;
                    AttachmentConfig::with_thumbnail(thumbnail).info(attachment_info)
                }
                None => AttachmentConfig::new().info(attachment_info),
            };

            self.send_attachment(url, mime_type, attachment_config, progress_watcher).await
        }))
    }

    pub fn send_video(
        self: Arc<Self>,
        url: String,
        thumbnail_url: String,
        video_info: VideoInfo,
        progress_watcher: Option<Box<dyn ProgressWatcher>>,
    ) -> Arc<SendAttachmentJoinHandle> {
        SendAttachmentJoinHandle::new(RUNTIME.spawn(async move {
            let mime_str =
                video_info.mimetype.as_ref().ok_or(RoomError::InvalidAttachmentMimeType)?;
            let mime_type =
                mime_str.parse::<Mime>().map_err(|_| RoomError::InvalidAttachmentMimeType)?;

            let base_video_info: BaseVideoInfo = BaseVideoInfo::try_from(&video_info)
                .map_err(|_| RoomError::InvalidAttachmentData)?;

            let attachment_info = AttachmentInfo::Video(base_video_info);

            let attachment_config = match video_info.thumbnail_info {
                Some(thumbnail_image_info) => {
                    let thumbnail =
                        self.build_thumbnail_info(thumbnail_url, thumbnail_image_info)?;
                    AttachmentConfig::with_thumbnail(thumbnail).info(attachment_info)
                }
                None => AttachmentConfig::new().info(attachment_info),
            };

            self.send_attachment(url, mime_type, attachment_config, progress_watcher).await
        }))
    }

    pub fn send_audio(
        self: Arc<Self>,
        url: String,
        audio_info: AudioInfo,
        progress_watcher: Option<Box<dyn ProgressWatcher>>,
    ) -> Arc<SendAttachmentJoinHandle> {
        SendAttachmentJoinHandle::new(RUNTIME.spawn(async move {
            let mime_str =
                audio_info.mimetype.as_ref().ok_or(RoomError::InvalidAttachmentMimeType)?;
            let mime_type =
                mime_str.parse::<Mime>().map_err(|_| RoomError::InvalidAttachmentMimeType)?;

            let base_audio_info: BaseAudioInfo = BaseAudioInfo::try_from(&audio_info)
                .map_err(|_| RoomError::InvalidAttachmentData)?;

            let attachment_info = AttachmentInfo::Audio(base_audio_info);
            let attachment_config = AttachmentConfig::new().info(attachment_info);

            self.send_attachment(url, mime_type, attachment_config, progress_watcher).await
        }))
    }

    pub fn send_file(
        self: Arc<Self>,
        url: String,
        file_info: FileInfo,
        progress_watcher: Option<Box<dyn ProgressWatcher>>,
    ) -> Arc<SendAttachmentJoinHandle> {
        SendAttachmentJoinHandle::new(RUNTIME.spawn(async move {
            let mime_str =
                file_info.mimetype.as_ref().ok_or(RoomError::InvalidAttachmentMimeType)?;
            let mime_type =
                mime_str.parse::<Mime>().map_err(|_| RoomError::InvalidAttachmentMimeType)?;

            let base_file_info: BaseFileInfo =
                BaseFileInfo::try_from(&file_info).map_err(|_| RoomError::InvalidAttachmentData)?;

            let attachment_info = AttachmentInfo::File(base_file_info);
            let attachment_config = AttachmentConfig::new().info(attachment_info);

            self.send_attachment(url, mime_type, attachment_config, progress_watcher).await
        }))
    }

    pub fn retry_send(&self, txn_id: String) {
        let timeline = match &*RUNTIME.block_on(self.timeline.read()) {
            Some(t) => Arc::clone(t),
            None => {
                error!("Timeline not set up, can't retry sending message");
                return;
            }
        };

        RUNTIME.spawn(async move {
            if let Err(e) = timeline.retry_send(txn_id.as_str().into()).await {
                error!(txn_id, "Failed to retry sending: {e}");
            }
        });
    }

    pub fn send_location(
        &self,
        body: String,
        geo_uri: String,
        description: Option<String>,
        zoom_level: Option<u8>,
        asset_type: Option<AssetType>,
        txn_id: Option<String>,
    ) {
        let mut location_event_message_content =
            LocationMessageEventContent::new(body, geo_uri.clone());

        if let Some(asset_type) = asset_type {
            location_event_message_content =
                location_event_message_content.with_asset_type(RumaAssetType::from(asset_type));
        }

        let mut location_content = LocationContent::new(geo_uri);
        location_content.description = description;
        location_content.zoom_level = zoom_level.and_then(ZoomLevel::new);
        location_event_message_content.location = Some(location_content);

        let room_message_event_content =
            RoomMessageEventContent::new(MessageType::Location(location_event_message_content));
        self.send(Arc::new(room_message_event_content), txn_id)
    }

    pub fn cancel_send(&self, txn_id: String) {
        let timeline = match &*RUNTIME.block_on(self.timeline.read()) {
            Some(t) => Arc::clone(t),
            None => {
                error!("Timeline not set up, can't retry sending message");
                return;
            }
        };

        RUNTIME.spawn(async move {
            if !timeline.cancel_send(txn_id.as_str().into()).await {
                info!(txn_id, "Failed to discard local echo: Not found");
            }
        });
    }

    pub fn get_timeline_event_content_by_event_id(
        &self,
        event_id: String,
    ) -> Result<Arc<RoomMessageEventContent>, ClientError> {
        RUNTIME.block_on(async move {
            let timeline = self
                .timeline
                .read()
                .await
                .clone()
                .context("Timeline not set up, can't get event content")?;

            let event_id = EventId::parse(event_id)?;

            let item = timeline
                .item_by_event_id(&event_id)
                .await
                .context("Item with given event ID not found")?;

            let msgtype = item
                .content()
                .as_message()
                .context("Item with given event ID is not a message")?
                .msgtype()
                .to_owned();

            Ok(Arc::new(RoomMessageEventContent::new(msgtype)))
        })
    }

    pub async fn can_user_redact(self: Arc<Self>, user_id: String) -> Result<bool, ClientError> {
        RUNTIME
            .spawn(async move {
                let user_id = UserId::parse(&user_id)?;
                Ok(self.inner.can_user_redact(&user_id).await?)
            })
            .await
            .unwrap()
    }

    pub async fn can_user_ban(self: Arc<Self>, user_id: String) -> Result<bool, ClientError> {
        RUNTIME
            .spawn(async move {
                let user_id = UserId::parse(&user_id)?;
                Ok(self.inner.can_user_ban(&user_id).await?)
            })
            .await
            .unwrap()
    }

    pub async fn can_user_invite(self: Arc<Self>, user_id: String) -> Result<bool, ClientError> {
        RUNTIME
            .spawn(async move {
                let user_id = UserId::parse(&user_id)?;
                Ok(self.inner.can_user_invite(&user_id).await?)
            })
            .await
            .unwrap()
    }

    pub async fn can_user_kick(self: Arc<Self>, user_id: String) -> Result<bool, ClientError> {
        RUNTIME
            .spawn(async move {
                let user_id = UserId::parse(&user_id)?;
                Ok(self.inner.can_user_kick(&user_id).await?)
            })
            .await
            .unwrap()
    }

    pub async fn can_user_send_state(
        self: Arc<Self>,
        user_id: String,
        state_event: StateEventType,
    ) -> Result<bool, ClientError> {
        RUNTIME
            .spawn(async move {
                let user_id = UserId::parse(&user_id)?;
                Ok(self.inner.can_user_send_state(&user_id, state_event.into()).await?)
            })
            .await
            .unwrap()
    }

    pub async fn can_user_send_message(
        self: Arc<Self>,
        user_id: String,
        message: MessageLikeEventType,
    ) -> Result<bool, ClientError> {
        RUNTIME
            .spawn(async move {
                let user_id = UserId::parse(&user_id)?;
                Ok(self.inner.can_user_send_message(&user_id, message.into()).await?)
            })
            .await
            .unwrap()
    }

    pub async fn can_user_trigger_room_notification(
        self: Arc<Self>,
        user_id: String,
    ) -> Result<bool, ClientError> {
        RUNTIME
            .spawn(async move {
                let user_id = UserId::parse(&user_id)?;
                Ok(self.inner.can_user_trigger_room_notification(&user_id).await?)
            })
            .await
            .unwrap()
    }

    pub fn own_user_id(&self) -> String {
        self.inner.own_user_id().to_string()
    }
}

impl Room {
    fn build_thumbnail_info(
        &self,
        thumbnail_url: String,
        thumbnail_info: ThumbnailInfo,
    ) -> Result<Thumbnail, RoomError> {
        let thumbnail_data =
            fs::read(thumbnail_url).map_err(|_| RoomError::InvalidThumbnailData)?;

        let base_thumbnail_info = BaseThumbnailInfo::try_from(&thumbnail_info)
            .map_err(|_| RoomError::InvalidAttachmentData)?;

        let mime_str =
            thumbnail_info.mimetype.as_ref().ok_or(RoomError::InvalidAttachmentMimeType)?;
        let mime_type =
            mime_str.parse::<Mime>().map_err(|_| RoomError::InvalidAttachmentMimeType)?;

        Ok(Thumbnail {
            data: thumbnail_data,
            content_type: mime_type,
            info: Some(base_thumbnail_info),
        })
    }

    async fn send_attachment(
        &self,
        url: String,
        mime_type: Mime,
        attachment_config: AttachmentConfig,
        progress_watcher: Option<Box<dyn ProgressWatcher>>,
    ) -> Result<(), RoomError> {
        let timeline = self.timeline.clone();
        RUNTIME
            .spawn(async move {
                let timeline_guard = timeline.read().await;
                let timeline = timeline_guard.as_ref().ok_or(RoomError::TimelineUnavailable)?;

                let request = timeline.send_attachment(url, mime_type, attachment_config);
                if let Some(progress_watcher) = progress_watcher {
                    let mut subscriber = request.subscribe_to_send_progress();
                    RUNTIME.spawn(async move {
                        while let Some(progress) = subscriber.next().await {
                            progress_watcher.transmission_progress(progress.into());
                        }
                    });
                }

                request.await.map_err(|_| RoomError::FailedSendingAttachment)?;
                Ok(())
            })
            .await
            .unwrap()
    }
}

#[derive(uniffi::Object)]
pub struct SendAttachmentJoinHandle {
    join_hdl: Mutex<JoinHandle<Result<(), RoomError>>>,
    abort_hdl: AbortHandle,
}

impl SendAttachmentJoinHandle {
    fn new(join_hdl: JoinHandle<Result<(), RoomError>>) -> Arc<Self> {
        let abort_hdl = join_hdl.abort_handle();
        let join_hdl = Mutex::new(join_hdl);
        Arc::new(Self { join_hdl, abort_hdl })
    }
}

#[uniffi::export]
impl SendAttachmentJoinHandle {
    pub fn join(self: Arc<Self>) -> Result<(), RoomError> {
        RUNTIME.block_on(async move { (&mut *self.join_hdl.lock().await).await.unwrap() })
    }

    pub fn cancel(&self) {
        self.abort_hdl.abort();
    }
}

#[derive(uniffi::Record)]
pub struct RoomTimelineListenerResult {
    pub items: Vec<Arc<TimelineItem>>,
    pub items_stream: Arc<TaskHandle>,
}

#[uniffi::export(callback_interface)]
pub trait BackPaginationStatusListener: Sync + Send {
    fn on_update(&self, status: BackPaginationStatus);
}

#[derive(uniffi::Enum)]
pub enum PaginationOptions {
    SingleRequest { event_limit: u16, wait_for_token: bool },
    UntilNumItems { event_limit: u16, items: u16, wait_for_token: bool },
}

impl From<PaginationOptions> for matrix_sdk_ui::timeline::PaginationOptions<'static> {
    fn from(value: PaginationOptions) -> Self {
        use matrix_sdk_ui::timeline::PaginationOptions as Opts;
        let (wait_for_token, mut opts) = match value {
            PaginationOptions::SingleRequest { event_limit, wait_for_token } => {
                (wait_for_token, Opts::single_request(event_limit))
            }
            PaginationOptions::UntilNumItems { event_limit, items, wait_for_token } => {
                (wait_for_token, Opts::until_num_items(event_limit, items))
            }
        };

        if wait_for_token {
            opts = opts.wait_for_token();
        }

        opts
    }
}

#[derive(Clone, uniffi::Enum)]
pub enum AssetType {
    Sender,
    Pin,
}

impl From<AssetType> for RumaAssetType {
    fn from(value: AssetType) -> Self {
        match value {
            AssetType::Sender => Self::Self_,
            AssetType::Pin => Self::Pin,
        }
    }
}
