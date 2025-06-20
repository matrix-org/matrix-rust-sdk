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

use std::{collections::HashMap, fmt::Write as _, fs, panic, sync::Arc};

use anyhow::{Context, Result};
use as_variant::as_variant;
use eyeball_im::VectorDiff;
use futures_util::pin_mut;
use matrix_sdk::{
    attachment::{
        AttachmentConfig, AttachmentInfo, BaseAudioInfo, BaseFileInfo, BaseImageInfo,
        BaseVideoInfo, Thumbnail,
    },
    deserialized_responses::{ShieldState as SdkShieldState, ShieldStateCode},
    event_cache::RoomPaginationStatus,
    room::{
        edit::EditedContent as SdkEditedContent,
        reply::{EnforceThread, Reply},
    },
};
use matrix_sdk_common::{
    executor::{AbortHandle, JoinHandle},
    stream::{BoxStream, StreamExt},
};
use matrix_sdk_ui::timeline::{
    self, AttachmentSource, EventItemOrigin, Profile, TimelineDetails,
    TimelineUniqueId as SdkTimelineUniqueId,
};
use mime::Mime;
use reply::{EmbeddedEventDetails, InReplyToDetails};
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
        room::message::{
            LocationMessageEventContent, MessageType, ReplyWithinThread,
            RoomMessageEventContentWithoutRelation,
        },
        AnyMessageLikeEventContent,
    },
    EventId, UInt,
};
use tokio::sync::Mutex;
use tracing::{error, warn};
use uuid::Uuid;

use self::content::TimelineItemContent;
pub use self::msg_like::MessageContent;
use crate::{
    client::ProgressWatcher,
    error::{ClientError, RoomError},
    event::EventOrTransactionId,
    helpers::unwrap_or_clone_arc,
    ruma::{
        AssetType, AudioInfo, FileInfo, FormattedBody, ImageInfo, Mentions, PollKind,
        ThumbnailInfo, VideoInfo,
    },
    runtime::get_runtime_handle,
    task_handle::TaskHandle,
    utils::Timestamp,
};

pub mod configuration;
mod content;
mod msg_like;
mod reply;

use matrix_sdk::utils::formatted_body_from;
use matrix_sdk_common::{SendOutsideWasm, SyncOutsideWasm};

use crate::error::QueueWedgeError;

#[derive(uniffi::Object)]
#[repr(transparent)]
pub struct Timeline {
    pub(crate) inner: matrix_sdk_ui::timeline::Timeline,
}

impl Timeline {
    pub(crate) fn new(inner: matrix_sdk_ui::timeline::Timeline) -> Arc<Self> {
        Arc::new(Self { inner })
    }

    fn send_attachment(
        self: Arc<Self>,
        params: UploadParameters,
        attachment_info: AttachmentInfo,
        mime_type: Option<String>,
        progress_watcher: Option<Box<dyn ProgressWatcher>>,
        thumbnail: Option<Thumbnail>,
    ) -> Result<Arc<SendAttachmentJoinHandle>, RoomError> {
        let mime_str = mime_type.as_ref().ok_or(RoomError::InvalidAttachmentMimeType)?;
        let mime_type =
            mime_str.parse::<Mime>().map_err(|_| RoomError::InvalidAttachmentMimeType)?;

        let formatted_caption = formatted_body_from(
            params.caption.as_deref(),
            params.formatted_caption.map(Into::into),
        );

        let attachment_config = AttachmentConfig::new()
            .thumbnail(thumbnail)
            .info(attachment_info)
            .caption(params.caption)
            .formatted_caption(formatted_caption)
            .mentions(params.mentions.map(Into::into))
            .reply(params.reply_params.map(|p| p.try_into()).transpose()?);

        let handle = SendAttachmentJoinHandle::new(get_runtime_handle().spawn(async move {
            let mut request =
                self.inner.send_attachment(params.source, mime_type, attachment_config);

            if params.use_send_queue {
                request = request.use_send_queue();
            }

            if let Some(progress_watcher) = progress_watcher {
                let mut subscriber = request.subscribe_to_send_progress();
                get_runtime_handle().spawn(async move {
                    while let Some(progress) = subscriber.next().await {
                        progress_watcher.transmission_progress(progress.into());
                    }
                });
            }

            request.await.map_err(|_| RoomError::FailedSendingAttachment)?;
            Ok(())
        }));

        Ok(handle)
    }
}

fn build_thumbnail_info(
    thumbnail_path: Option<String>,
    thumbnail_info: Option<ThumbnailInfo>,
) -> Result<Option<Thumbnail>, RoomError> {
    match (thumbnail_path, thumbnail_info) {
        (None, None) => Ok(None),

        (Some(thumbnail_path), Some(thumbnail_info)) => {
            let thumbnail_data =
                fs::read(thumbnail_path).map_err(|_| RoomError::InvalidThumbnailData)?;

            let height = thumbnail_info
                .height
                .and_then(|u| UInt::try_from(u).ok())
                .ok_or(RoomError::InvalidAttachmentData)?;
            let width = thumbnail_info
                .width
                .and_then(|u| UInt::try_from(u).ok())
                .ok_or(RoomError::InvalidAttachmentData)?;
            let size = thumbnail_info
                .size
                .and_then(|u| UInt::try_from(u).ok())
                .ok_or(RoomError::InvalidAttachmentData)?;

            let mime_str =
                thumbnail_info.mimetype.as_ref().ok_or(RoomError::InvalidAttachmentMimeType)?;
            let mime_type =
                mime_str.parse::<Mime>().map_err(|_| RoomError::InvalidAttachmentMimeType)?;

            Ok(Some(Thumbnail {
                data: thumbnail_data,
                content_type: mime_type,
                height,
                width,
                size,
            }))
        }

        _ => {
            warn!("Ignoring thumbnail because either the thumbnail path or info isn't defined");
            Ok(None)
        }
    }
}

#[derive(uniffi::Record)]
pub struct UploadParameters {
    /// Source from which to upload data
    source: UploadSource,
    /// Optional non-formatted caption, for clients that support it.
    caption: Option<String>,
    /// Optional HTML-formatted caption, for clients that support it.
    formatted_caption: Option<FormattedBody>,
    /// Optional intentional mentions to be sent with the media.
    mentions: Option<Mentions>,
    /// Optional parameters for sending the media as (threaded) reply.
    reply_params: Option<ReplyParameters>,
    /// Should the media be sent with the send queue, or synchronously?
    ///
    /// Watching progress only works with the synchronous method, at the moment.
    use_send_queue: bool,
}

/// A source for uploading a file
#[derive(uniffi::Enum)]
pub enum UploadSource {
    /// Upload source is a file on disk
    File {
        /// Path to file
        filename: String,
    },
    /// Upload source is data in memory
    Data {
        /// Bytes being uploaded
        bytes: Vec<u8>,
        /// Filename to associate with bytes
        filename: String,
    },
}

impl From<UploadSource> for AttachmentSource {
    fn from(value: UploadSource) -> Self {
        match value {
            UploadSource::File { filename } => Self::File(filename.into()),
            UploadSource::Data { bytes, filename } => Self::Data { bytes, filename },
        }
    }
}

#[derive(uniffi::Record)]
pub struct ReplyParameters {
    /// The ID of the event to reply to.
    event_id: String,
    /// Whether to enforce a thread relation.
    enforce_thread: bool,
    /// If enforcing a threaded relation, whether the message is a reply on a
    /// thread.
    reply_within_thread: bool,
}

impl TryInto<Reply> for ReplyParameters {
    type Error = RoomError;

    fn try_into(self) -> Result<Reply, Self::Error> {
        let event_id =
            EventId::parse(&self.event_id).map_err(|_| RoomError::InvalidRepliedToEventId)?;
        let enforce_thread = if self.enforce_thread {
            EnforceThread::Threaded(if self.reply_within_thread {
                ReplyWithinThread::Yes
            } else {
                ReplyWithinThread::No
            })
        } else {
            EnforceThread::MaybeThreaded
        };

        Ok(Reply { event_id, enforce_thread })
    }
}

#[matrix_sdk_ffi_macros::export]
impl Timeline {
    pub async fn add_listener(&self, listener: Box<dyn TimelineListener>) -> Arc<TaskHandle> {
        let (timeline_items, timeline_stream) = self.inner.subscribe().await;

        // It's important that the initial items are passed *before* we forward the
        // stream updates, with a guaranteed ordering. Otherwise, it could
        // be that the listener be called before the initial items have been
        // handled by the caller. See #3535 for details.

        // First, pass all the items as a reset update.
        listener.on_update(vec![Arc::new(TimelineDiff::new(VectorDiff::Reset {
            values: timeline_items,
        }))]);

        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            pin_mut!(timeline_stream);

            // Then forward new items.
            while let Some(diffs) = timeline_stream.next().await {
                listener
                    .on_update(diffs.into_iter().map(|d| Arc::new(TimelineDiff::new(d))).collect());
            }
        })))
    }

    pub fn retry_decryption(self: Arc<Self>, session_ids: Vec<String>) {
        get_runtime_handle().spawn(async move {
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

        // Send the current state even if it hasn't changed right away.
        //
        // Note: don't do it in the spawned function, so that the caller is immediately
        // aware of the current state, and this doesn't depend on the async runtime
        // having an available worker
        listener.on_update(initial);

        Ok(Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            while let Some(status) = subscriber.next().await {
                listener.on_update(status);
            }
        }))))
    }

    /// Paginate backwards, whether we are in focused mode or in live mode.
    ///
    /// Returns whether we hit the start of the timeline or not.
    pub async fn paginate_backwards(&self, num_events: u16) -> Result<bool, ClientError> {
        Ok(self.inner.paginate_backwards(num_events).await?)
    }

    /// Paginate forwards, whether we are in focused mode or in live mode.
    ///
    /// Returns whether we hit the end of the timeline or not.
    pub async fn paginate_forwards(&self, num_events: u16) -> Result<bool, ClientError> {
        Ok(self.inner.paginate_forwards(num_events).await?)
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
    ) -> Result<Arc<SendHandle>, ClientError> {
        match self.inner.send((*msg).to_owned().with_relation(None).into()).await {
            Ok(handle) => Ok(Arc::new(SendHandle::new(handle))),
            Err(err) => {
                error!("error when sending a message: {err}");
                Err(err.into())
            }
        }
    }

    pub fn send_image(
        self: Arc<Self>,
        params: UploadParameters,
        thumbnail_path: Option<String>,
        image_info: ImageInfo,
        progress_watcher: Option<Box<dyn ProgressWatcher>>,
    ) -> Result<Arc<SendAttachmentJoinHandle>, RoomError> {
        let attachment_info = AttachmentInfo::Image(
            BaseImageInfo::try_from(&image_info).map_err(|_| RoomError::InvalidAttachmentData)?,
        );
        let thumbnail = build_thumbnail_info(thumbnail_path, image_info.thumbnail_info)?;
        self.send_attachment(
            params,
            attachment_info,
            image_info.mimetype,
            progress_watcher,
            thumbnail,
        )
    }

    pub fn send_video(
        self: Arc<Self>,
        params: UploadParameters,
        thumbnail_path: Option<String>,
        video_info: VideoInfo,
        progress_watcher: Option<Box<dyn ProgressWatcher>>,
    ) -> Result<Arc<SendAttachmentJoinHandle>, RoomError> {
        let attachment_info = AttachmentInfo::Video(
            BaseVideoInfo::try_from(&video_info).map_err(|_| RoomError::InvalidAttachmentData)?,
        );
        let thumbnail = build_thumbnail_info(thumbnail_path, video_info.thumbnail_info)?;
        self.send_attachment(
            params,
            attachment_info,
            video_info.mimetype,
            progress_watcher,
            thumbnail,
        )
    }

    pub fn send_audio(
        self: Arc<Self>,
        params: UploadParameters,
        audio_info: AudioInfo,
        progress_watcher: Option<Box<dyn ProgressWatcher>>,
    ) -> Result<Arc<SendAttachmentJoinHandle>, RoomError> {
        let attachment_info = AttachmentInfo::Audio(
            BaseAudioInfo::try_from(&audio_info).map_err(|_| RoomError::InvalidAttachmentData)?,
        );
        self.send_attachment(params, attachment_info, audio_info.mimetype, progress_watcher, None)
    }

    pub fn send_voice_message(
        self: Arc<Self>,
        params: UploadParameters,
        audio_info: AudioInfo,
        waveform: Vec<u16>,
        progress_watcher: Option<Box<dyn ProgressWatcher>>,
    ) -> Result<Arc<SendAttachmentJoinHandle>, RoomError> {
        let attachment_info = AttachmentInfo::Voice {
            audio_info: BaseAudioInfo::try_from(&audio_info)
                .map_err(|_| RoomError::InvalidAttachmentData)?,
            waveform: Some(waveform),
        };
        self.send_attachment(params, attachment_info, audio_info.mimetype, progress_watcher, None)
    }

    pub fn send_file(
        self: Arc<Self>,
        params: UploadParameters,
        file_info: FileInfo,
        progress_watcher: Option<Box<dyn ProgressWatcher>>,
    ) -> Result<Arc<SendAttachmentJoinHandle>, RoomError> {
        let attachment_info = AttachmentInfo::File(
            BaseFileInfo::try_from(&file_info).map_err(|_| RoomError::InvalidAttachmentData)?,
        );
        self.send_attachment(params, attachment_info, file_info.mimetype, progress_watcher, None)
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
        poll_start_event_id: String,
        answers: Vec<String>,
    ) -> Result<(), ClientError> {
        let poll_start_event_id =
            EventId::parse(poll_start_event_id).context("Failed to parse EventId")?;
        let poll_response_event_content =
            UnstablePollResponseEventContent::new(answers, poll_start_event_id);
        let event_content =
            AnyMessageLikeEventContent::UnstablePollResponse(poll_response_event_content);

        if let Err(err) = self.inner.send(event_content).await {
            error!("unable to send poll response: {err}");
        }

        Ok(())
    }

    pub async fn end_poll(
        self: Arc<Self>,
        poll_start_event_id: String,
        text: String,
    ) -> Result<(), ClientError> {
        let poll_start_event_id =
            EventId::parse(poll_start_event_id).context("Failed to parse EventId")?;
        let poll_end_event_content = UnstablePollEndEventContent::new(text, poll_start_event_id);
        let event_content = AnyMessageLikeEventContent::UnstablePollEnd(poll_end_event_content);

        if let Err(err) = self.inner.send(event_content).await {
            error!("unable to end poll: {err}");
        }

        Ok(())
    }

    /// Send a reply.
    ///
    /// If the replied to event has a thread relation, it is forwarded on the
    /// reply so that clients that support threads can render the reply
    /// inside the thread.
    pub async fn send_reply(
        &self,
        msg: Arc<RoomMessageEventContentWithoutRelation>,
        reply_params: ReplyParameters,
    ) -> Result<(), ClientError> {
        self.inner.send_reply((*msg).clone(), reply_params.try_into()?).await?;
        Ok(())
    }

    /// Edits an event from the timeline.
    ///
    /// If it was a local event, this will *try* to edit it, if it was not
    /// being sent already. If the event was a remote event, then it will be
    /// redacted by sending an edit request to the server.
    ///
    /// Returns whether the edit did happen. It can only return false for
    /// local events that are being processed.
    pub async fn edit(
        &self,
        event_or_transaction_id: EventOrTransactionId,
        new_content: EditedContent,
    ) -> Result<(), ClientError> {
        match self
            .inner
            .edit(&event_or_transaction_id.clone().try_into()?, new_content.clone().try_into()?)
            .await
        {
            Ok(()) => Ok(()),

            Err(timeline::Error::EventNotInTimeline(_)) => {
                // If we couldn't edit, assume it was an (remote) event that wasn't in the
                // timeline, and try to edit it via the room itself.
                let event_id = match event_or_transaction_id {
                    EventOrTransactionId::EventId { event_id } => EventId::parse(event_id)?,
                    EventOrTransactionId::TransactionId { .. } => {
                        warn!("trying to apply an edit to a local echo that doesn't exist in this timeline, aborting");
                        return Ok(());
                    }
                };
                let room = self.inner.room();
                let edit_event = room.make_edit_event(&event_id, new_content.try_into()?).await?;
                room.send_queue().send(edit_event).await?;
                Ok(())
            }

            Err(err) => Err(err.into()),
        }
    }

    pub async fn send_location(
        self: Arc<Self>,
        body: String,
        geo_uri: String,
        description: Option<String>,
        zoom_level: Option<u8>,
        asset_type: Option<AssetType>,
        reply_params: Option<ReplyParameters>,
    ) -> Result<(), ClientError> {
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

        if let Some(reply_params) = reply_params {
            self.send_reply(Arc::new(room_message_event_content), reply_params).await
        } else {
            self.send(Arc::new(room_message_event_content)).await?;
            Ok(())
        }
    }

    /// Toggle a reaction on an event.
    ///
    /// Adds or redacts a reaction based on the state of the reaction at the
    /// time it is called.
    ///
    /// This method works both on local echoes and remote items.
    ///
    /// When redacting a previous reaction, the redaction reason is not set.
    ///
    /// Ensures that only one reaction is sent at a time to avoid race
    /// conditions and spamming the homeserver with requests.
    pub async fn toggle_reaction(
        &self,
        item_id: EventOrTransactionId,
        key: String,
    ) -> Result<(), ClientError> {
        self.inner.toggle_reaction(&item_id.try_into()?, &key).await?;
        Ok(())
    }

    pub async fn fetch_details_for_event(&self, event_id: String) -> Result<(), ClientError> {
        let event_id = <&EventId>::try_from(event_id.as_str())?;
        self.inner
            .fetch_details_for_event(event_id)
            .await
            .map_err(|e| ClientError::from_str(e, Some("Fetching event details".to_owned())))?;
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
    ) -> Result<EventTimelineItem, ClientError> {
        let event_id = EventId::parse(event_id)?;
        let item = self
            .inner
            .item_by_event_id(&event_id)
            .await
            .context("Item with given event ID not found")?;
        Ok(item.into())
    }

    /// Redacts an event from the timeline.
    ///
    /// Only works for events that exist as timeline items.
    ///
    /// If it was a local event, this will *try* to cancel it, if it was not
    /// being sent already. If the event was a remote event, then it will be
    /// redacted by sending a redaction request to the server.
    ///
    /// Will return an error if the event couldn't be redacted.
    pub async fn redact_event(
        &self,
        event_or_transaction_id: EventOrTransactionId,
        reason: Option<String>,
    ) -> Result<(), ClientError> {
        Ok(self.inner.redact(&(event_or_transaction_id.try_into()?), reason.as_deref()).await?)
    }

    /// Load the reply details for the given event id.
    ///
    /// This will return an `InReplyToDetails` object that contains the details
    /// which will either be ready or an error.
    pub async fn load_reply_details(
        &self,
        event_id_str: String,
    ) -> Result<Arc<InReplyToDetails>, ClientError> {
        let event_id = EventId::parse(&event_id_str)?;

        let replied_to = match self.inner.room().load_or_fetch_event(&event_id, None).await {
            Ok(event) => self.inner.make_replied_to(event).await.map_err(ClientError::from),
            Err(e) => Err(ClientError::from(e)),
        };

        match replied_to {
            Ok(Some(replied_to)) => Ok(Arc::new(InReplyToDetails::new(
                event_id_str,
                EmbeddedEventDetails::Ready {
                    content: replied_to.content.clone().into(),
                    sender: replied_to.sender.to_string(),
                    sender_profile: replied_to.sender_profile.into(),
                },
            ))),

            Ok(None) => Ok(Arc::new(InReplyToDetails::new(
                event_id_str,
                EmbeddedEventDetails::Error { message: "unsupported event".to_owned() },
            ))),

            Err(e) => Ok(Arc::new(InReplyToDetails::new(
                event_id_str,
                EmbeddedEventDetails::Error { message: e.to_string() },
            ))),
        }
    }

    /// Adds a new pinned event by sending an updated `m.room.pinned_events`
    /// event containing the new event id.
    ///
    /// Returns `true` if we sent the request, `false` if the event was already
    /// pinned.
    async fn pin_event(&self, event_id: String) -> Result<bool, ClientError> {
        let event_id = EventId::parse(event_id).map_err(ClientError::from)?;
        self.inner.pin_event(&event_id).await.map_err(ClientError::from)
    }

    /// Adds a new pinned event by sending an updated `m.room.pinned_events`
    /// event without the event id we want to remove.
    ///
    /// Returns `true` if we sent the request, `false` if the event wasn't
    /// pinned
    async fn unpin_event(&self, event_id: String) -> Result<bool, ClientError> {
        let event_id = EventId::parse(event_id).map_err(ClientError::from)?;
        self.inner.unpin_event(&event_id).await.map_err(ClientError::from)
    }

    pub fn create_message_content(
        &self,
        msg_type: crate::ruma::MessageType,
    ) -> Option<Arc<RoomMessageEventContentWithoutRelation>> {
        let msg_type: Option<MessageType> = msg_type.try_into().ok();
        msg_type.map(|m| Arc::new(RoomMessageEventContentWithoutRelation::new(m)))
    }
}

/// A handle to perform actions onto a local echo.
#[derive(uniffi::Object)]
pub struct SendHandle {
    inner: Mutex<Option<matrix_sdk::send_queue::SendHandle>>,
}

impl SendHandle {
    fn new(handle: matrix_sdk::send_queue::SendHandle) -> Self {
        Self { inner: Mutex::new(Some(handle)) }
    }
}

#[matrix_sdk_ffi_macros::export]
impl SendHandle {
    /// Try to abort the sending of the current event.
    ///
    /// If this returns `true`, then the sending could be aborted, because the
    /// event hasn't been sent yet. Otherwise, if this returns `false`, the
    /// event had already been sent and could not be aborted.
    ///
    /// This has an effect only on the first call; subsequent calls will always
    /// return `false`.
    async fn abort(self: Arc<Self>) -> Result<bool, ClientError> {
        if let Some(inner) = self.inner.lock().await.take() {
            Ok(inner
                .abort()
                .await
                .map_err(|err| anyhow::anyhow!("error when saving in store: {err}"))?)
        } else {
            warn!("trying to abort a send handle that's already been actioned");
            Ok(false)
        }
    }

    /// Attempt to manually resend messages that failed to send due to issues
    /// that should now have been fixed.
    ///
    /// This is useful for example, when there's a
    /// `SessionRecipientCollectionError::VerifiedUserChangedIdentity` error;
    /// the user may have re-verified on a different device and would now
    /// like to send the failed message that's waiting on this device.
    ///
    /// # Arguments
    ///
    /// * `transaction_id` - The send queue transaction identifier of the local
    ///   echo that should be unwedged.
    pub async fn try_resend(self: Arc<Self>) -> Result<(), ClientError> {
        let locked = self.inner.lock().await;
        if let Some(handle) = locked.as_ref() {
            handle.unwedge().await?;
        } else {
            warn!("trying to unwedge a send handle that's been aborted");
        }
        Ok(())
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

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait TimelineListener: SyncOutsideWasm + SendOutsideWasm {
    fn on_update(&self, diff: Vec<Arc<TimelineDiff>>);
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait PaginationStatusListener: SyncOutsideWasm + SendOutsideWasm {
    fn on_update(&self, status: RoomPaginationStatus);
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
                Self::Reset { values: values.into_iter().map(TimelineItem::from_arc).collect() }
            }
        }
    }
}

#[matrix_sdk_ffi_macros::export]
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

    pub fn truncate(&self) -> Option<u32> {
        as_variant!(self, Self::Truncate { length } => (*length).try_into().unwrap())
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

#[derive(Clone, uniffi::Record)]
pub struct TimelineUniqueId {
    id: String,
}

impl From<&SdkTimelineUniqueId> for TimelineUniqueId {
    fn from(value: &SdkTimelineUniqueId) -> Self {
        Self { id: value.0.clone() }
    }
}

impl From<&TimelineUniqueId> for SdkTimelineUniqueId {
    fn from(value: &TimelineUniqueId) -> Self {
        Self(value.id.clone())
    }
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

#[matrix_sdk_ffi_macros::export]
impl TimelineItem {
    pub fn as_event(self: Arc<Self>) -> Option<EventTimelineItem> {
        let event_item = self.0.as_event()?;
        Some(event_item.clone().into())
    }

    pub fn as_virtual(self: Arc<Self>) -> Option<VirtualTimelineItem> {
        use matrix_sdk_ui::timeline::VirtualTimelineItem as VItem;
        match self.0.as_virtual()? {
            VItem::DateDivider(ts) => Some(VirtualTimelineItem::DateDivider { ts: (*ts).into() }),
            VItem::ReadMarker => Some(VirtualTimelineItem::ReadMarker),
            VItem::TimelineStart => Some(VirtualTimelineItem::TimelineStart),
        }
    }

    /// An opaque unique identifier for this timeline item.
    pub fn unique_id(&self) -> TimelineUniqueId {
        self.0.unique_id().into()
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
    SendingFailed {
        /// The error reason, with information for the user.
        error: QueueWedgeError,

        /// Whether the error is considered recoverable or not.
        ///
        /// An error that's recoverable will disable the room's send queue,
        /// while an unrecoverable error will be parked, until the user
        /// decides to cancel sending it.
        is_recoverable: bool,
    },

    /// The local event has been sent successfully to the server.
    Sent { event_id: String },
}

impl From<&matrix_sdk_ui::timeline::EventSendState> for EventSendState {
    fn from(value: &matrix_sdk_ui::timeline::EventSendState) -> Self {
        use matrix_sdk_ui::timeline::EventSendState::*;

        match value {
            NotSentYet => Self::NotSentYet,
            SendingFailed { error, is_recoverable } => {
                let as_queue_wedge_error: matrix_sdk::QueueWedgeError = (&**error).into();
                Self::SendingFailed {
                    is_recoverable: *is_recoverable,
                    error: as_queue_wedge_error.into(),
                }
            }
            Sent { event_id } => Self::Sent { event_id: event_id.to_string() },
        }
    }
}

/// Recommended decorations for decrypted messages, representing the message's
/// authenticity properties.
#[derive(uniffi::Enum, Clone)]
pub enum ShieldState {
    /// A red shield with a tooltip containing the associated message should be
    /// presented.
    Red { code: ShieldStateCode, message: String },
    /// A grey shield with a tooltip containing the associated message should be
    /// presented.
    Grey { code: ShieldStateCode, message: String },
    /// No shield should be presented.
    None,
}

impl From<SdkShieldState> for ShieldState {
    fn from(value: SdkShieldState) -> Self {
        match value {
            SdkShieldState::Red { code, message } => {
                Self::Red { code, message: message.to_owned() }
            }
            SdkShieldState::Grey { code, message } => {
                Self::Grey { code, message: message.to_owned() }
            }
            SdkShieldState::None => Self::None,
        }
    }
}

#[derive(Clone, uniffi::Record)]
pub struct EventTimelineItem {
    /// Indicates that an event is remote.
    is_remote: bool,
    event_or_transaction_id: EventOrTransactionId,
    sender: String,
    sender_profile: ProfileDetails,
    is_own: bool,
    is_editable: bool,
    content: TimelineItemContent,
    timestamp: Timestamp,
    local_send_state: Option<EventSendState>,
    local_created_at: Option<u64>,
    read_receipts: HashMap<String, Receipt>,
    origin: Option<EventItemOrigin>,
    can_be_replied_to: bool,
    lazy_provider: Arc<LazyTimelineItemProvider>,
}

impl From<matrix_sdk_ui::timeline::EventTimelineItem> for EventTimelineItem {
    fn from(item: matrix_sdk_ui::timeline::EventTimelineItem) -> Self {
        let item = Arc::new(item);
        let lazy_provider = Arc::new(LazyTimelineItemProvider(item.clone()));
        let read_receipts =
            item.read_receipts().iter().map(|(k, v)| (k.to_string(), v.clone().into())).collect();
        Self {
            is_remote: !item.is_local_echo(),
            event_or_transaction_id: item.identifier().into(),
            sender: item.sender().to_string(),
            sender_profile: item.sender_profile().clone().into(),
            is_own: item.is_own(),
            is_editable: item.is_editable(),
            content: item.content().clone().into(),
            timestamp: item.timestamp().into(),
            local_send_state: item.send_state().map(|s| s.into()),
            local_created_at: item.local_created_at().map(|t| t.0.into()),
            read_receipts,
            origin: item.origin(),
            can_be_replied_to: item.can_be_replied_to(),
            lazy_provider,
        }
    }
}

#[derive(Clone, uniffi::Record)]
pub struct Receipt {
    pub timestamp: Option<Timestamp>,
}

impl From<ruma::events::receipt::Receipt> for Receipt {
    fn from(value: ruma::events::receipt::Receipt) -> Self {
        Receipt { timestamp: value.ts.map(|ts| ts.into()) }
    }
}

#[derive(Clone, uniffi::Record)]
pub struct EventTimelineItemDebugInfo {
    model: String,
    original_json: Option<String>,
    latest_edit_json: Option<String>,
}

#[derive(Clone, uniffi::Enum)]
pub enum ProfileDetails {
    Unavailable,
    Pending,
    Ready { display_name: Option<String>, display_name_ambiguous: bool, avatar_url: Option<String> },
    Error { message: String },
}

impl From<TimelineDetails<Profile>> for ProfileDetails {
    fn from(details: TimelineDetails<Profile>) -> Self {
        match details {
            TimelineDetails::Unavailable => Self::Unavailable,
            TimelineDetails::Pending => Self::Pending,
            TimelineDetails::Ready(profile) => Self::Ready {
                display_name: profile.display_name,
                display_name_ambiguous: profile.display_name_ambiguous,
                avatar_url: profile.avatar_url.as_ref().map(ToString::to_string),
            },
            TimelineDetails::Error(e) => Self::Error { message: e.to_string() },
        }
    }
}

#[derive(Clone, uniffi::Record)]
pub struct PollData {
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
    join_handle: Arc<Mutex<JoinHandle<Result<(), RoomError>>>>,
    abort_handle: AbortHandle,
}

impl SendAttachmentJoinHandle {
    fn new(join_handle: JoinHandle<Result<(), RoomError>>) -> Arc<Self> {
        let abort_handle = join_handle.abort_handle();
        let join_handle = Arc::new(Mutex::new(join_handle));
        Arc::new(Self { join_handle, abort_handle })
    }
}

#[matrix_sdk_ffi_macros::export]
impl SendAttachmentJoinHandle {
    /// Wait until the attachment has been sent.
    ///
    /// If the sending had been cancelled, will return immediately.
    pub async fn join(&self) -> Result<(), RoomError> {
        let handle = self.join_handle.clone();
        let mut locked_handle = handle.lock().await;
        let join_result = (&mut *locked_handle).await;
        match join_result {
            Ok(res) => res,
            Err(err) => {
                if err.is_cancelled() {
                    return Ok(());
                }
                error!("task panicked! resuming panic from here.");
                #[cfg(not(target_family = "wasm"))]
                panic::resume_unwind(err.into_panic());
                #[cfg(target_family = "wasm")]
                panic!("task panicked! {err}");
            }
        }
    }

    /// Cancel the current sending task.
    ///
    /// A subsequent call to [`Self::join`] will return immediately.
    pub fn cancel(&self) {
        self.abort_handle.abort();
    }
}

/// A [`TimelineItem`](super::TimelineItem) that doesn't correspond to an event.
#[derive(uniffi::Enum)]
pub enum VirtualTimelineItem {
    /// A divider between messages of different day or month depending on
    /// timeline settings.
    DateDivider {
        /// A timestamp in milliseconds since Unix Epoch on that day in local
        /// time.
        ts: Timestamp,
    },

    /// The user's own read marker.
    ReadMarker,

    /// The timeline start, that is, the *oldest* event in time for that room.
    TimelineStart,
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

#[derive(Clone, uniffi::Enum)]
pub enum EditedContent {
    RoomMessage {
        content: Arc<RoomMessageEventContentWithoutRelation>,
    },
    MediaCaption {
        caption: Option<String>,
        formatted_caption: Option<FormattedBody>,
        mentions: Option<Mentions>,
    },
    PollStart {
        poll_data: PollData,
    },
}

impl TryFrom<EditedContent> for SdkEditedContent {
    type Error = ClientError;

    fn try_from(value: EditedContent) -> Result<Self, Self::Error> {
        match value {
            EditedContent::RoomMessage { content } => {
                Ok(SdkEditedContent::RoomMessage((*content).clone()))
            }
            EditedContent::MediaCaption { caption, formatted_caption, mentions } => {
                Ok(SdkEditedContent::MediaCaption {
                    caption,
                    formatted_caption: formatted_caption.map(Into::into),
                    mentions: mentions.map(Into::into),
                })
            }
            EditedContent::PollStart { poll_data } => {
                let block: UnstablePollStartContentBlock = poll_data.clone().try_into()?;
                Ok(SdkEditedContent::PollStart {
                    fallback_text: poll_data.fallback_text(),
                    new_content: block,
                })
            }
        }
    }
}

/// Create a caption edit.
///
/// If no `formatted_caption` is provided, then it's assumed the `caption`
/// represents valid Markdown that can be used as the formatted caption.
#[matrix_sdk_ffi_macros::export]
fn create_caption_edit(
    caption: Option<String>,
    formatted_caption: Option<FormattedBody>,
    mentions: Option<Mentions>,
) -> EditedContent {
    let formatted_caption =
        formatted_body_from(caption.as_deref(), formatted_caption.map(Into::into));
    EditedContent::MediaCaption {
        caption,
        formatted_caption: formatted_caption.as_ref().map(Into::into),
        mentions,
    }
}

/// Wrapper to retrieve some timeline item info lazily.
#[derive(Clone, uniffi::Object)]
pub struct LazyTimelineItemProvider(Arc<matrix_sdk_ui::timeline::EventTimelineItem>);

#[matrix_sdk_ffi_macros::export]
impl LazyTimelineItemProvider {
    /// Returns the shields for this event timeline item.
    fn get_shields(&self, strict: bool) -> Option<ShieldState> {
        self.0.get_shield(strict).map(Into::into)
    }

    /// Returns some debug information for this event timeline item.
    fn debug_info(&self) -> EventTimelineItemDebugInfo {
        EventTimelineItemDebugInfo {
            model: format!("{:#?}", self.0),
            original_json: self.0.original_json().map(|raw| raw.json().get().to_owned()),
            latest_edit_json: self.0.latest_edit_json().map(|raw| raw.json().get().to_owned()),
        }
    }

    /// For local echoes, return the associated send handle; returns `None` for
    /// remote echoes.
    fn get_send_handle(&self) -> Option<Arc<SendHandle>> {
        self.0.local_echo_send_handle().map(|handle| Arc::new(SendHandle::new(handle)))
    }

    fn contains_only_emojis(&self) -> bool {
        self.0.contains_only_emojis()
    }
}

#[cfg(feature = "unstable-msc4274")]
mod galleries {
    use std::{panic, sync::Arc};

    use matrix_sdk::{
        attachment::{
            AttachmentInfo, BaseAudioInfo, BaseFileInfo, BaseImageInfo, BaseVideoInfo, Thumbnail,
        },
        utils::formatted_body_from,
    };
    use matrix_sdk_common::executor::{AbortHandle, JoinHandle};
    use matrix_sdk_ui::timeline::GalleryConfig;
    use mime::Mime;
    use tokio::sync::Mutex;
    use tracing::error;

    use crate::{
        error::RoomError,
        ruma::{AudioInfo, FileInfo, FormattedBody, ImageInfo, Mentions, VideoInfo},
        runtime::get_runtime_handle,
        timeline::{build_thumbnail_info, ReplyParameters, Timeline},
    };

    #[derive(uniffi::Record)]
    pub struct GalleryUploadParameters {
        /// Optional non-formatted caption, for clients that support it.
        caption: Option<String>,
        /// Optional HTML-formatted caption, for clients that support it.
        formatted_caption: Option<FormattedBody>,
        /// Optional intentional mentions to be sent with the gallery.
        mentions: Option<Mentions>,
        /// Optional parameters for sending the media as (threaded) reply.
        reply_params: Option<ReplyParameters>,
    }

    #[derive(uniffi::Enum)]
    pub enum GalleryItemInfo {
        Audio {
            audio_info: AudioInfo,
            filename: String,
            caption: Option<String>,
            formatted_caption: Option<FormattedBody>,
        },
        File {
            file_info: FileInfo,
            filename: String,
            caption: Option<String>,
            formatted_caption: Option<FormattedBody>,
        },
        Image {
            image_info: ImageInfo,
            filename: String,
            caption: Option<String>,
            formatted_caption: Option<FormattedBody>,
            thumbnail_path: Option<String>,
        },
        Video {
            video_info: VideoInfo,
            filename: String,
            caption: Option<String>,
            formatted_caption: Option<FormattedBody>,
            thumbnail_path: Option<String>,
        },
    }

    impl GalleryItemInfo {
        fn mimetype(&self) -> &Option<String> {
            match self {
                GalleryItemInfo::Audio { audio_info, .. } => &audio_info.mimetype,
                GalleryItemInfo::File { file_info, .. } => &file_info.mimetype,
                GalleryItemInfo::Image { image_info, .. } => &image_info.mimetype,
                GalleryItemInfo::Video { video_info, .. } => &video_info.mimetype,
            }
        }

        fn filename(&self) -> &String {
            match self {
                GalleryItemInfo::Audio { filename, .. } => filename,
                GalleryItemInfo::File { filename, .. } => filename,
                GalleryItemInfo::Image { filename, .. } => filename,
                GalleryItemInfo::Video { filename, .. } => filename,
            }
        }

        fn caption(&self) -> &Option<String> {
            match self {
                GalleryItemInfo::Audio { caption, .. } => caption,
                GalleryItemInfo::File { caption, .. } => caption,
                GalleryItemInfo::Image { caption, .. } => caption,
                GalleryItemInfo::Video { caption, .. } => caption,
            }
        }

        fn formatted_caption(&self) -> &Option<FormattedBody> {
            match self {
                GalleryItemInfo::Audio { formatted_caption, .. } => formatted_caption,
                GalleryItemInfo::File { formatted_caption, .. } => formatted_caption,
                GalleryItemInfo::Image { formatted_caption, .. } => formatted_caption,
                GalleryItemInfo::Video { formatted_caption, .. } => formatted_caption,
            }
        }

        fn attachment_info(&self) -> Result<AttachmentInfo, RoomError> {
            match self {
                GalleryItemInfo::Audio { audio_info, .. } => Ok(AttachmentInfo::Audio(
                    BaseAudioInfo::try_from(audio_info)
                        .map_err(|_| RoomError::InvalidAttachmentData)?,
                )),
                GalleryItemInfo::File { file_info, .. } => Ok(AttachmentInfo::File(
                    BaseFileInfo::try_from(file_info)
                        .map_err(|_| RoomError::InvalidAttachmentData)?,
                )),
                GalleryItemInfo::Image { image_info, .. } => Ok(AttachmentInfo::Image(
                    BaseImageInfo::try_from(image_info)
                        .map_err(|_| RoomError::InvalidAttachmentData)?,
                )),
                GalleryItemInfo::Video { video_info, .. } => Ok(AttachmentInfo::Video(
                    BaseVideoInfo::try_from(video_info)
                        .map_err(|_| RoomError::InvalidAttachmentData)?,
                )),
            }
        }

        fn thumbnail(&self) -> Result<Option<Thumbnail>, RoomError> {
            match self {
                GalleryItemInfo::Audio { .. } | GalleryItemInfo::File { .. } => Ok(None),
                GalleryItemInfo::Image { image_info, thumbnail_path, .. } => {
                    build_thumbnail_info(thumbnail_path.clone(), image_info.thumbnail_info.clone())
                }
                GalleryItemInfo::Video { video_info, thumbnail_path, .. } => {
                    build_thumbnail_info(thumbnail_path.clone(), video_info.thumbnail_info.clone())
                }
            }
        }
    }

    impl TryInto<matrix_sdk_ui::timeline::GalleryItemInfo> for GalleryItemInfo {
        type Error = RoomError;

        fn try_into(
            self,
        ) -> std::result::Result<matrix_sdk_ui::timeline::GalleryItemInfo, Self::Error> {
            let mime_str = self.mimetype().as_ref().ok_or(RoomError::InvalidAttachmentMimeType)?;
            let mime_type =
                mime_str.parse::<Mime>().map_err(|_| RoomError::InvalidAttachmentMimeType)?;
            Ok(matrix_sdk_ui::timeline::GalleryItemInfo {
                source: self.filename().into(),
                content_type: mime_type,
                attachment_info: self.attachment_info()?,
                caption: self.caption().clone(),
                formatted_caption: self
                    .formatted_caption()
                    .clone()
                    .map(ruma::events::room::message::FormattedBody::from),
                thumbnail: self.thumbnail()?,
            })
        }
    }

    #[derive(uniffi::Object)]
    pub struct SendGalleryJoinHandle {
        join_handle: Arc<Mutex<JoinHandle<Result<(), RoomError>>>>,
        abort_handle: AbortHandle,
    }

    impl SendGalleryJoinHandle {
        fn new(join_handle: JoinHandle<Result<(), RoomError>>) -> Arc<Self> {
            let abort_handle = join_handle.abort_handle();
            let join_handle = Arc::new(Mutex::new(join_handle));
            Arc::new(Self { join_handle, abort_handle })
        }
    }

    #[matrix_sdk_ffi_macros::export]
    impl SendGalleryJoinHandle {
        /// Wait until the gallery has been sent.
        ///
        /// If the sending had been cancelled, will return immediately.
        pub async fn join(&self) -> Result<(), RoomError> {
            let handle = self.join_handle.clone();
            let mut locked_handle = handle.lock().await;
            let join_result = (&mut *locked_handle).await;
            match join_result {
                Ok(res) => res,
                Err(err) => {
                    if err.is_cancelled() {
                        return Ok(());
                    }
                    error!("task panicked! resuming panic from here.");
                    panic::resume_unwind(err.into_panic());
                }
            }
        }

        /// Cancel the current sending task.
        ///
        /// A subsequent call to [`Self::join`] will return immediately.
        pub fn cancel(&self) {
            self.abort_handle.abort();
        }
    }

    #[matrix_sdk_ffi_macros::export]
    impl Timeline {
        pub fn send_gallery(
            self: Arc<Self>,
            params: GalleryUploadParameters,
            item_infos: Vec<GalleryItemInfo>,
        ) -> Result<Arc<SendGalleryJoinHandle>, RoomError> {
            let formatted_caption = formatted_body_from(
                params.caption.as_deref(),
                params.formatted_caption.map(Into::into),
            );

            let mut gallery_config = GalleryConfig::new()
                .caption(params.caption)
                .formatted_caption(formatted_caption)
                .mentions(params.mentions.map(Into::into))
                .reply(params.reply_params.map(|p| p.try_into()).transpose()?);

            for item_info in item_infos {
                gallery_config = gallery_config.add_item(item_info.try_into()?);
            }

            let handle = SendGalleryJoinHandle::new(get_runtime_handle().spawn(async move {
                let request = self.inner.send_gallery(gallery_config);
                request.await.map_err(|_| RoomError::FailedSendingAttachment)?;
                Ok(())
            }));

            Ok(handle)
        }
    }
}
