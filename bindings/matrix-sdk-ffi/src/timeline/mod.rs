// Copyright 2023 The Matrix.org Foundation C.I.C.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{collections::HashMap, fmt::Write as _, fs, sync::Arc};

use anyhow::{Context, Result};
use as_variant::as_variant;
use eyeball_im::VectorDiff;
use futures_util::{pin_mut, StreamExt as _};
use matrix_sdk::attachment::{
    AttachmentConfig, AttachmentInfo, BaseAudioInfo, BaseFileInfo, BaseImageInfo,
    BaseThumbnailInfo, BaseVideoInfo, Thumbnail,
};
use matrix_sdk_ui::timeline::{
    EventItemOrigin, LiveBackPaginationStatus, Profile, TimelineDetails,
};
use mime::Mime;
use ruma::{
    events::{
        location::{AssetType as RumaAssetType, LocationContent, ZoomLevel},
        poll::{
            unstable_end::UnstablePollEndEventContent,
            unstable_response::UnstablePollResponseEventContent,
            unstable_start::{
                NewUnstablePollStartEventContent, UnstablePollAnswer, UnstablePollAnswers,
                UnstablePollStartContentBlock,
            },
        },
        receipt::ReceiptThread,
        relation::Annotation,
        room::message::{
            ForwardThread, LocationMessageEventContent, MessageType,
            RoomMessageEventContentWithoutRelation,
        },
        AnyMessageLikeEventContent,
    },
    EventId, OwnedTransactionId,
};
use tokio::{
    sync::Mutex,
    task::{AbortHandle, JoinHandle},
};
use tracing::{error, warn};
use uuid::Uuid;

use self::content::{Reaction, ReactionSenderData, TimelineItemContent};
use crate::{
    client::ProgressWatcher,
    error::{ClientError, RoomError},
    helpers::unwrap_or_clone_arc,
    ruma::{
        AssetType, AudioInfo, FileInfo, FormattedBody, ImageInfo, PollKind, ThumbnailInfo,
        VideoInfo,
    },
    task_handle::TaskHandle,
    RUNTIME,
};

mod content;

#[derive(uniffi::Object)]
#[repr(transparent)]
pub struct Timeline {
    pub(crate) inner: matrix_sdk_ui::timeline::Timeline,
}

impl Timeline {
    pub(crate) fn new(inner: matrix_sdk_ui::timeline::Timeline) -> Arc<Self> {
        Arc::new(Self { inner })
    }

    pub(crate) fn from_arc(inner: Arc<matrix_sdk_ui::timeline::Timeline>) -> Arc<Self> {
        // SAFETY: repr(transparent) means transmuting the arc this way is allowed
        unsafe { Arc::from_raw(Arc::into_raw(inner) as _) }
    }

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
        filename: String,
        mime_type: Mime,
        attachment_config: AttachmentConfig,
        progress_watcher: Option<Box<dyn ProgressWatcher>>,
    ) -> Result<(), RoomError> {
        let request = self.inner.send_attachment(filename, mime_type, attachment_config);
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
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl Timeline {
    pub async fn add_listener(
        &self,
        listener: Box<dyn TimelineListener>,
    ) -> RoomTimelineListenerResult {
        let (timeline_items, timeline_stream) = self.inner.subscribe_batched().await;
        let timeline_stream = TaskHandle::new(RUNTIME.spawn(async move {
            pin_mut!(timeline_stream);

            while let Some(diffs) = timeline_stream.next().await {
                listener
                    .on_update(diffs.into_iter().map(|d| Arc::new(TimelineDiff::new(d))).collect());
            }
        }));

        RoomTimelineListenerResult {
            items: timeline_items.into_iter().map(TimelineItem::from_arc).collect(),
            items_stream: Arc::new(timeline_stream),
        }
    }

    pub fn retry_decryption(self: Arc<Self>, session_ids: Vec<String>) {
        RUNTIME.spawn(async move {
            self.inner.retry_decryption(&session_ids).await;
        });
    }

    pub async fn fetch_members(&self) {
        self.inner.fetch_members().await
    }

    pub async fn subscribe_to_back_pagination_status(
        &self,
        listener: Box<dyn PaginationStatusListener>,
    ) -> Result<Arc<TaskHandle>, ClientError> {
        let (initial, mut subscriber) = self
            .inner
            .live_back_pagination_status()
            .await
            .context("can't subscribe to the back-pagination status on a focused timeline")?;

        Ok(Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            // Send the current state even if it hasn't changed right away.
            listener.on_update(initial);

            while let Some(status) = subscriber.next().await {
                listener.on_update(status);
            }
        }))))
    }

    /// Paginate backwards, whether we are in focused mode or in live mode.
    ///
    /// Returns whether we hit the end of the timeline or not.
    pub async fn paginate_backwards(&self, num_events: u16) -> Result<bool, ClientError> {
        Ok(self.inner.paginate_backwards(num_events).await?)
    }

    /// Paginate forwards, when in focused mode.
    ///
    /// Returns whether we hit the end of the timeline or not.
    pub async fn focused_paginate_forwards(&self, num_events: u16) -> Result<bool, ClientError> {
        Ok(self.inner.focused_paginate_forwards(num_events).await?)
    }

    pub async fn send_read_receipt(
        &self,
        receipt_type: ReceiptType,
        event_id: String,
    ) -> Result<(), ClientError> {
        let event_id = EventId::parse(event_id)?;
        self.inner
            .send_single_receipt(receipt_type.into(), ReceiptThread::Unthreaded, event_id)
            .await?;
        Ok(())
    }

    /// Mark the room as read by trying to attach an *unthreaded* read receipt
    /// to the latest room event.
    ///
    /// This works even if the latest event belongs to a thread, as a threaded
    /// reply also belongs to the unthreaded timeline. No threaded receipt
    /// will be sent here (see also #3123).
    pub async fn mark_as_read(&self, receipt_type: ReceiptType) -> Result<(), ClientError> {
        self.inner.mark_as_read(receipt_type.into()).await?;
        Ok(())
    }

    /// Queues an event in the room's send queue so it's processed for
    /// sending later.
    ///
    /// Returns an abort handle that allows to abort sending, if it hasn't
    /// happened yet.
    pub async fn send(
        self: Arc<Self>,
        msg: Arc<RoomMessageEventContentWithoutRelation>,
    ) -> Result<Arc<AbortSendHandle>, ClientError> {
        match self.inner.send((*msg).to_owned().with_relation(None).into()).await {
            Ok(handle) => Ok(Arc::new(AbortSendHandle { inner: Mutex::new(Some(handle)) })),
            Err(err) => {
                error!("error when sending a message: {err}");
                Err(anyhow::anyhow!(err).into())
            }
        }
    }

    pub fn send_image(
        self: Arc<Self>,
        url: String,
        thumbnail_url: Option<String>,
        image_info: ImageInfo,
        caption: Option<String>,
        formatted_caption: Option<FormattedBody>,
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

            let attachment_config = match (thumbnail_url, image_info.thumbnail_info) {
                (Some(thumbnail_url), Some(thumbnail_image_info)) => {
                    let thumbnail =
                        self.build_thumbnail_info(thumbnail_url, thumbnail_image_info)?;
                    AttachmentConfig::with_thumbnail(thumbnail).info(attachment_info)
                }
                _ => AttachmentConfig::new().info(attachment_info),
            }
            .caption(caption)
            .formatted_caption(formatted_caption.map(Into::into));

            self.send_attachment(url, mime_type, attachment_config, progress_watcher).await
        }))
    }

    pub fn send_video(
        self: Arc<Self>,
        url: String,
        thumbnail_url: Option<String>,
        video_info: VideoInfo,
        caption: Option<String>,
        formatted_caption: Option<FormattedBody>,
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

            let attachment_config = match (thumbnail_url, video_info.thumbnail_info) {
                (Some(thumbnail_url), Some(thumbnail_image_info)) => {
                    let thumbnail =
                        self.build_thumbnail_info(thumbnail_url, thumbnail_image_info)?;
                    AttachmentConfig::with_thumbnail(thumbnail).info(attachment_info)
                }
                _ => AttachmentConfig::new().info(attachment_info),
            }
            .caption(caption)
            .formatted_caption(formatted_caption.map(Into::into));

            self.send_attachment(url, mime_type, attachment_config, progress_watcher).await
        }))
    }

    pub fn send_audio(
        self: Arc<Self>,
        url: String,
        audio_info: AudioInfo,
        caption: Option<String>,
        formatted_caption: Option<FormattedBody>,
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
            let attachment_config = AttachmentConfig::new()
                .info(attachment_info)
                .caption(caption)
                .formatted_caption(formatted_caption.map(Into::into));

            self.send_attachment(url, mime_type, attachment_config, progress_watcher).await
        }))
    }

    pub fn send_voice_message(
        self: Arc<Self>,
        url: String,
        audio_info: AudioInfo,
        waveform: Vec<u16>,
        caption: Option<String>,
        formatted_caption: Option<FormattedBody>,
        progress_watcher: Option<Box<dyn ProgressWatcher>>,
    ) -> Arc<SendAttachmentJoinHandle> {
        SendAttachmentJoinHandle::new(RUNTIME.spawn(async move {
            let mime_str =
                audio_info.mimetype.as_ref().ok_or(RoomError::InvalidAttachmentMimeType)?;
            let mime_type =
                mime_str.parse::<Mime>().map_err(|_| RoomError::InvalidAttachmentMimeType)?;

            let base_audio_info: BaseAudioInfo = BaseAudioInfo::try_from(&audio_info)
                .map_err(|_| RoomError::InvalidAttachmentData)?;

            let attachment_info =
                AttachmentInfo::Voice { audio_info: base_audio_info, waveform: Some(waveform) };
            let attachment_config = AttachmentConfig::new()
                .info(attachment_info)
                .caption(caption)
                .formatted_caption(formatted_caption.map(Into::into));

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

    pub async fn create_poll(
        self: Arc<Self>,
        question: String,
        answers: Vec<String>,
        max_selections: u8,
        poll_kind: PollKind,
    ) -> Result<(), ClientError> {
        let poll_data = PollData { question, answers, max_selections, poll_kind };

        let poll_start_event_content = NewUnstablePollStartEventContent::plain_text(
            poll_data.fallback_text(),
            poll_data.try_into()?,
        );
        let event_content =
            AnyMessageLikeEventContent::UnstablePollStart(poll_start_event_content.into());

        if let Err(err) = self.inner.send(event_content).await {
            error!("unable to start poll: {err}");
        }

        Ok(())
    }

    pub async fn send_poll_response(
        self: Arc<Self>,
        poll_start_id: String,
        answers: Vec<String>,
    ) -> Result<(), ClientError> {
        let poll_start_event_id =
            EventId::parse(poll_start_id).context("Failed to parse EventId")?;
        let poll_response_event_content =
            UnstablePollResponseEventContent::new(answers, poll_start_event_id);
        let event_content =
            AnyMessageLikeEventContent::UnstablePollResponse(poll_response_event_content);

        if let Err(err) = self.inner.send(event_content).await {
            error!("unable to send poll response: {err}");
        }

        Ok(())
    }

    pub fn end_poll(
        self: Arc<Self>,
        poll_start_id: String,
        text: String,
    ) -> Result<(), ClientError> {
        let poll_start_event_id =
            EventId::parse(poll_start_id).context("Failed to parse EventId")?;
        let poll_end_event_content = UnstablePollEndEventContent::new(text, poll_start_event_id);
        let event_content = AnyMessageLikeEventContent::UnstablePollEnd(poll_end_event_content);

        RUNTIME.spawn(async move {
            if let Err(err) = self.inner.send(event_content).await {
                error!("unable to end poll: {err}");
            }
        });

        Ok(())
    }

    pub async fn send_reply(
        &self,
        msg: Arc<RoomMessageEventContentWithoutRelation>,
        reply_item: Arc<EventTimelineItem>,
    ) -> Result<(), ClientError> {
        self.inner
            .send_reply((*msg).clone(), &reply_item.0, ForwardThread::Yes)
            .await
            .map_err(|err| anyhow::anyhow!(err))?;
        Ok(())
    }

    pub async fn edit(
        &self,
        new_content: Arc<RoomMessageEventContentWithoutRelation>,
        edit_item: Arc<EventTimelineItem>,
    ) -> Result<(), ClientError> {
        self.inner
            .edit((*new_content).clone(), &edit_item.0)
            .await
            .map_err(|err| anyhow::anyhow!(err))?;
        Ok(())
    }

    pub async fn edit_poll(
        &self,
        question: String,
        answers: Vec<String>,
        max_selections: u8,
        poll_kind: PollKind,
        edit_item: Arc<EventTimelineItem>,
    ) -> Result<(), ClientError> {
        let poll_data = PollData { question, answers, max_selections, poll_kind };
        self.inner
            .edit_poll(poll_data.fallback_text(), poll_data.try_into()?, &edit_item.0)
            .await
            .map_err(|err| anyhow::anyhow!(err))?;
        Ok(())
    }

    pub async fn send_location(
        self: Arc<Self>,
        body: String,
        geo_uri: String,
        description: Option<String>,
        zoom_level: Option<u8>,
        asset_type: Option<AssetType>,
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

        let room_message_event_content = RoomMessageEventContentWithoutRelation::new(
            MessageType::Location(location_event_message_content),
        );
        // Errors are logged in `Self::send` already.
        let _ = self.send(Arc::new(room_message_event_content)).await;
    }

    pub async fn toggle_reaction(&self, event_id: String, key: String) -> Result<(), ClientError> {
        let event_id = EventId::parse(event_id)?;
        self.inner.toggle_reaction(&Annotation::new(event_id, key)).await?;
        Ok(())
    }

    pub async fn fetch_details_for_event(&self, event_id: String) -> Result<(), ClientError> {
        let event_id = <&EventId>::try_from(event_id.as_str())?;
        self.inner.fetch_details_for_event(event_id).await.context("Fetching event details")?;
        Ok(())
    }

    /// Get the current timeline item for the given event ID, if any.
    ///
    /// Will return a remote event, *or* a local echo that has been sent but not
    /// yet replaced by a remote echo.
    ///
    /// It's preferable to store the timeline items in the model for your UI, if
    /// possible, instead of just storing IDs and coming back to the timeline
    /// object to look up items.
    pub async fn get_event_timeline_item_by_event_id(
        &self,
        event_id: String,
    ) -> Result<Arc<EventTimelineItem>, ClientError> {
        let event_id = EventId::parse(event_id)?;
        let item = self
            .inner
            .item_by_event_id(&event_id)
            .await
            .context("Item with given event ID not found")?;
        Ok(Arc::new(EventTimelineItem(item)))
    }

    /// Get the current timeline item for the given transaction ID, if any.
    ///
    /// This will always return a local echo, if found.
    ///
    /// It's preferable to store the timeline items in the model for your UI, if
    /// possible, instead of just storing IDs and coming back to the timeline
    /// object to look up items.
    pub async fn get_event_timeline_item_by_transaction_id(
        &self,
        transaction_id: String,
    ) -> Result<Arc<EventTimelineItem>, ClientError> {
        let transaction_id: OwnedTransactionId = transaction_id.into();
        let item = self
            .inner
            .item_by_transaction_id(&transaction_id)
            .await
            .context("Item with given transaction ID not found")?;
        Ok(Arc::new(EventTimelineItem(item)))
    }

    pub async fn latest_event(&self) -> Option<Arc<EventTimelineItem>> {
        let latest_event = self.inner.latest_event().await;

        latest_event.map(|item| Arc::new(EventTimelineItem(item)))
    }

    /// Redacts an event from the timeline.
    ///
    /// Only works for events that exist as timeline items.
    ///
    /// If it was a local event, this will *try* to cancel it, if it was not
    /// being sent already. If the event was a remote event, then it will be
    /// redacted by sending a redaction request to the server.
    ///
    /// Returns whether the redaction did happen. It can only return false for
    /// local events that are being processed.
    pub async fn redact_event(
        &self,
        item: Arc<EventTimelineItem>,
        reason: Option<String>,
    ) -> Result<bool, ClientError> {
        let removed = self
            .inner
            .redact(&item.0, reason.as_deref())
            .await
            .map_err(|err| anyhow::anyhow!(err))?;

        Ok(removed)
    }
}

#[derive(uniffi::Object)]
pub struct AbortSendHandle {
    inner: Mutex<Option<matrix_sdk::send_queue::AbortSendHandle>>,
}

#[uniffi::export(async_runtime = "tokio")]
impl AbortSendHandle {
    /// Try to abort the sending of the current event.
    ///
    /// If this returns `true`, then the sending could be aborted, because the
    /// event hasn't been sent yet. Otherwise, if this returns `false`, the
    /// event had already been sent and could not be aborted.
    ///
    /// This has an effect only on the first call; subsequent calls will always
    /// return `false`.
    async fn abort(self: Arc<Self>) -> bool {
        if let Some(inner) = self.inner.lock().await.take() {
            inner.abort().await
        } else {
            warn!("trying to abort an send handle that's already been actioned");
            false
        }
    }
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum FocusEventError {
    #[error("the event id parameter {event_id} is incorrect: {err}")]
    InvalidEventId { event_id: String, err: String },

    #[error("the event {event_id} could not be found")]
    EventNotFound { event_id: String },

    #[error("error when trying to focus on an event: {msg}")]
    Other { msg: String },
}

#[derive(uniffi::Record)]
pub struct RoomTimelineListenerResult {
    pub items: Vec<Arc<TimelineItem>>,
    pub items_stream: Arc<TaskHandle>,
}

#[uniffi::export(callback_interface)]
pub trait TimelineListener: Sync + Send {
    fn on_update(&self, diff: Vec<Arc<TimelineDiff>>);
}

#[uniffi::export(callback_interface)]
pub trait PaginationStatusListener: Sync + Send {
    fn on_update(&self, status: LiveBackPaginationStatus);
}

#[derive(Clone, uniffi::Object)]
pub enum TimelineDiff {
    Append { values: Vec<Arc<TimelineItem>> },
    Clear,
    PushFront { value: Arc<TimelineItem> },
    PushBack { value: Arc<TimelineItem> },
    PopFront,
    PopBack,
    Insert { index: usize, value: Arc<TimelineItem> },
    Set { index: usize, value: Arc<TimelineItem> },
    Remove { index: usize },
    Truncate { length: usize },
    Reset { values: Vec<Arc<TimelineItem>> },
}

impl TimelineDiff {
    pub(crate) fn new(inner: VectorDiff<Arc<matrix_sdk_ui::timeline::TimelineItem>>) -> Self {
        match inner {
            VectorDiff::Append { values } => {
                Self::Append { values: values.into_iter().map(TimelineItem::from_arc).collect() }
            }
            VectorDiff::Clear => Self::Clear,
            VectorDiff::Insert { index, value } => {
                Self::Insert { index, value: TimelineItem::from_arc(value) }
            }
            VectorDiff::Set { index, value } => {
                Self::Set { index, value: TimelineItem::from_arc(value) }
            }
            VectorDiff::Truncate { length } => Self::Truncate { length },
            VectorDiff::Remove { index } => Self::Remove { index },
            VectorDiff::PushBack { value } => {
                Self::PushBack { value: TimelineItem::from_arc(value) }
            }
            VectorDiff::PushFront { value } => {
                Self::PushFront { value: TimelineItem::from_arc(value) }
            }
            VectorDiff::PopBack => Self::PopBack,
            VectorDiff::PopFront => Self::PopFront,
            VectorDiff::Reset { values } => {
                warn!("Timeline subscriber lagged behind and was reset");
                Self::Reset { values: values.into_iter().map(TimelineItem::from_arc).collect() }
            }
        }
    }
}

#[uniffi::export]
impl TimelineDiff {
    pub fn change(&self) -> TimelineChange {
        match self {
            Self::Append { .. } => TimelineChange::Append,
            Self::Insert { .. } => TimelineChange::Insert,
            Self::Set { .. } => TimelineChange::Set,
            Self::Remove { .. } => TimelineChange::Remove,
            Self::PushBack { .. } => TimelineChange::PushBack,
            Self::PushFront { .. } => TimelineChange::PushFront,
            Self::PopBack => TimelineChange::PopBack,
            Self::PopFront => TimelineChange::PopFront,
            Self::Clear => TimelineChange::Clear,
            Self::Truncate { .. } => TimelineChange::Truncate,
            Self::Reset { .. } => TimelineChange::Reset,
        }
    }

    pub fn append(self: Arc<Self>) -> Option<Vec<Arc<TimelineItem>>> {
        let this = unwrap_or_clone_arc(self);
        as_variant!(this, Self::Append { values } => values)
    }

    pub fn insert(self: Arc<Self>) -> Option<InsertData> {
        let this = unwrap_or_clone_arc(self);
        as_variant!(this, Self::Insert { index, value } => {
            InsertData { index: index.try_into().unwrap(), item: value }
        })
    }

    pub fn set(self: Arc<Self>) -> Option<SetData> {
        let this = unwrap_or_clone_arc(self);
        as_variant!(this, Self::Set { index, value } => {
            SetData { index: index.try_into().unwrap(), item: value }
        })
    }

    pub fn remove(&self) -> Option<u32> {
        as_variant!(self, Self::Remove { index } => (*index).try_into().unwrap())
    }

    pub fn push_back(self: Arc<Self>) -> Option<Arc<TimelineItem>> {
        let this = unwrap_or_clone_arc(self);
        as_variant!(this, Self::PushBack { value } => value)
    }

    pub fn push_front(self: Arc<Self>) -> Option<Arc<TimelineItem>> {
        let this = unwrap_or_clone_arc(self);
        as_variant!(this, Self::PushFront { value } => value)
    }

    pub fn reset(self: Arc<Self>) -> Option<Vec<Arc<TimelineItem>>> {
        let this = unwrap_or_clone_arc(self);
        as_variant!(this, Self::Reset { values } => values)
    }
}

#[derive(uniffi::Record)]
pub struct InsertData {
    pub index: u32,
    pub item: Arc<TimelineItem>,
}

#[derive(uniffi::Record)]
pub struct SetData {
    pub index: u32,
    pub item: Arc<TimelineItem>,
}

#[derive(Clone, Copy, uniffi::Enum)]
pub enum TimelineChange {
    Append,
    Clear,
    Insert,
    Set,
    Remove,
    PushBack,
    PushFront,
    PopBack,
    PopFront,
    Truncate,
    Reset,
}

#[repr(transparent)]
#[derive(Clone, uniffi::Object)]
pub struct TimelineItem(pub(crate) matrix_sdk_ui::timeline::TimelineItem);

impl TimelineItem {
    pub(crate) fn from_arc(arc: Arc<matrix_sdk_ui::timeline::TimelineItem>) -> Arc<Self> {
        // SAFETY: This is valid because Self is a repr(transparent) wrapper
        //         around the other Timeline type.
        unsafe { Arc::from_raw(Arc::into_raw(arc) as _) }
    }
}

#[uniffi::export]
impl TimelineItem {
    pub fn as_event(self: Arc<Self>) -> Option<Arc<EventTimelineItem>> {
        let event_item = self.0.as_event()?;
        Some(Arc::new(EventTimelineItem(event_item.clone())))
    }

    pub fn as_virtual(self: Arc<Self>) -> Option<VirtualTimelineItem> {
        use matrix_sdk_ui::timeline::VirtualTimelineItem as VItem;
        match self.0.as_virtual()? {
            VItem::DayDivider(ts) => Some(VirtualTimelineItem::DayDivider { ts: ts.0.into() }),
            VItem::ReadMarker => Some(VirtualTimelineItem::ReadMarker),
        }
    }

    pub fn unique_id(&self) -> String {
        self.0.unique_id().to_owned()
    }

    pub fn fmt_debug(&self) -> String {
        format!("{:#?}", self.0)
    }
}

/// This type represents the “send state” of a local event timeline item.
#[derive(Clone, uniffi::Enum)]
pub enum EventSendState {
    /// The local event has not been sent yet.
    NotSentYet,
    /// The local event has been sent to the server, but unsuccessfully: The
    /// sending has failed.
    SendingFailed { error: String },
    /// The local event has been sent successfully to the server.
    Sent { event_id: String },
}

impl From<&matrix_sdk_ui::timeline::EventSendState> for EventSendState {
    fn from(value: &matrix_sdk_ui::timeline::EventSendState) -> Self {
        use matrix_sdk_ui::timeline::EventSendState::*;

        match value {
            NotSentYet => Self::NotSentYet,
            SendingFailed { error } => Self::SendingFailed { error: error.to_string() },
            Sent { event_id } => Self::Sent { event_id: event_id.to_string() },
        }
    }
}

#[derive(uniffi::Object)]
pub struct EventTimelineItem(pub(crate) matrix_sdk_ui::timeline::EventTimelineItem);

#[uniffi::export]
impl EventTimelineItem {
    pub fn is_local(&self) -> bool {
        self.0.is_local_echo()
    }

    pub fn is_remote(&self) -> bool {
        !self.0.is_local_echo()
    }

    pub fn transaction_id(&self) -> Option<String> {
        self.0.transaction_id().map(ToString::to_string)
    }

    pub fn event_id(&self) -> Option<String> {
        self.0.event_id().map(ToString::to_string)
    }

    pub fn sender(&self) -> String {
        self.0.sender().to_string()
    }

    pub fn sender_profile(&self) -> ProfileDetails {
        self.0.sender_profile().into()
    }

    pub fn is_own(&self) -> bool {
        self.0.is_own()
    }

    pub fn is_editable(&self) -> bool {
        self.0.is_editable()
    }

    pub fn content(&self) -> Arc<TimelineItemContent> {
        Arc::new(TimelineItemContent(self.0.content().clone()))
    }

    pub fn timestamp(&self) -> u64 {
        self.0.timestamp().0.into()
    }

    pub fn reactions(&self) -> Vec<Reaction> {
        self.0
            .reactions()
            .iter()
            .map(|(k, v)| Reaction {
                key: k.to_owned(),
                count: v.len().try_into().unwrap(),
                senders: v
                    .senders()
                    .map(|v| ReactionSenderData {
                        sender_id: v.sender_id.to_string(),
                        timestamp: v.timestamp.0.into(),
                    })
                    .collect(),
            })
            .collect()
    }

    pub fn debug_info(&self) -> EventTimelineItemDebugInfo {
        EventTimelineItemDebugInfo {
            model: format!("{:#?}", self.0),
            original_json: self.0.original_json().map(|raw| raw.json().get().to_owned()),
            latest_edit_json: self.0.latest_edit_json().map(|raw| raw.json().get().to_owned()),
        }
    }

    pub fn local_send_state(&self) -> Option<EventSendState> {
        self.0.send_state().map(Into::into)
    }

    pub fn read_receipts(&self) -> HashMap<String, Receipt> {
        self.0.read_receipts().iter().map(|(k, v)| (k.to_string(), v.clone().into())).collect()
    }

    pub fn origin(&self) -> Option<EventItemOrigin> {
        self.0.origin()
    }

    pub fn can_be_replied_to(&self) -> bool {
        self.0.can_be_replied_to()
    }
}

#[derive(uniffi::Record)]
pub struct Receipt {
    pub timestamp: Option<u64>,
}

impl From<ruma::events::receipt::Receipt> for Receipt {
    fn from(value: ruma::events::receipt::Receipt) -> Self {
        Receipt { timestamp: value.ts.map(|ts| ts.0.into()) }
    }
}

#[derive(uniffi::Record)]
pub struct EventTimelineItemDebugInfo {
    model: String,
    original_json: Option<String>,
    latest_edit_json: Option<String>,
}

#[derive(uniffi::Enum)]
pub enum ProfileDetails {
    Unavailable,
    Pending,
    Ready { display_name: Option<String>, display_name_ambiguous: bool, avatar_url: Option<String> },
    Error { message: String },
}

impl From<&TimelineDetails<Profile>> for ProfileDetails {
    fn from(details: &TimelineDetails<Profile>) -> Self {
        match details {
            TimelineDetails::Unavailable => Self::Unavailable,
            TimelineDetails::Pending => Self::Pending,
            TimelineDetails::Ready(profile) => Self::Ready {
                display_name: profile.display_name.clone(),
                display_name_ambiguous: profile.display_name_ambiguous,
                avatar_url: profile.avatar_url.as_ref().map(ToString::to_string),
            },
            TimelineDetails::Error(e) => Self::Error { message: e.to_string() },
        }
    }
}

struct PollData {
    question: String,
    answers: Vec<String>,
    max_selections: u8,
    poll_kind: PollKind,
}

impl PollData {
    fn fallback_text(&self) -> String {
        self.answers.iter().enumerate().fold(self.question.clone(), |mut acc, (index, answer)| {
            write!(&mut acc, "\n{}. {answer}", index + 1).unwrap();
            acc
        })
    }
}

impl TryFrom<PollData> for UnstablePollStartContentBlock {
    type Error = ClientError;

    fn try_from(value: PollData) -> Result<Self, Self::Error> {
        let poll_answers_vec: Vec<UnstablePollAnswer> = value
            .answers
            .iter()
            .map(|answer| UnstablePollAnswer::new(Uuid::new_v4().to_string(), answer))
            .collect();

        let poll_answers = UnstablePollAnswers::try_from(poll_answers_vec)
            .context("Failed to create poll answers")?;

        let mut poll_content_block =
            UnstablePollStartContentBlock::new(value.question.clone(), poll_answers);
        poll_content_block.kind = value.poll_kind.into();
        poll_content_block.max_selections = value.max_selections.into();

        Ok(poll_content_block)
    }
}

#[derive(uniffi::Object)]
pub struct SendAttachmentJoinHandle {
    join_hdl: Arc<Mutex<JoinHandle<Result<(), RoomError>>>>,
    abort_hdl: AbortHandle,
}

impl SendAttachmentJoinHandle {
    fn new(join_hdl: JoinHandle<Result<(), RoomError>>) -> Arc<Self> {
        let abort_hdl = join_hdl.abort_handle();
        let join_hdl = Arc::new(Mutex::new(join_hdl));
        Arc::new(Self { join_hdl, abort_hdl })
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl SendAttachmentJoinHandle {
    pub async fn join(&self) -> Result<(), RoomError> {
        let join_hdl = self.join_hdl.clone();
        RUNTIME.spawn(async move { (&mut *join_hdl.lock().await).await.unwrap() }).await.unwrap()
    }

    pub fn cancel(&self) {
        self.abort_hdl.abort();
    }
}

/// A [`TimelineItem`](super::TimelineItem) that doesn't correspond to an event.
#[derive(uniffi::Enum)]
pub enum VirtualTimelineItem {
    /// A divider between messages of two days.
    DayDivider {
        /// A timestamp in milliseconds since Unix Epoch on that day in local
        /// time.
        ts: u64,
    },

    /// The user's own read marker.
    ReadMarker,
}

/// A [`TimelineItem`](super::TimelineItem) that doesn't correspond to an event.
#[derive(uniffi::Enum)]
pub enum ReceiptType {
    Read,
    ReadPrivate,
    FullyRead,
}

impl From<ReceiptType> for ruma::api::client::receipt::create_receipt::v3::ReceiptType {
    fn from(value: ReceiptType) -> Self {
        match value {
            ReceiptType::Read => Self::Read,
            ReceiptType::ReadPrivate => Self::ReadPrivate,
            ReceiptType::FullyRead => Self::FullyRead,
        }
    }
}
