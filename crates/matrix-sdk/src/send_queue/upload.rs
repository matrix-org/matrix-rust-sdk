// Copyright 2024 The Matrix.org Foundation C.I.C.
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

//! Private implementations of the media upload mechanism.

#[cfg(feature = "unstable-msc4274")]
use std::{collections::HashMap, iter::zip};

use matrix_sdk_base::{
    RoomState,
    media::{
        MediaFormat, MediaRequestParameters, MediaThumbnailSettings,
        store::IgnoreMediaRetentionPolicy,
    },
    store::{
        ChildTransactionId, DependentQueuedRequestKind, FinishUploadThumbnailInfo,
        QueuedRequestKind, SentMediaInfo, SentRequestKey, SerializableEventContent,
    },
};
#[cfg(feature = "unstable-msc4274")]
use matrix_sdk_base::{
    media::UniqueKey,
    store::{AccumulatedSentMediaInfo, FinishGalleryItemInfo},
};
use mime::Mime;
#[cfg(feature = "unstable-msc4274")]
use ruma::events::room::message::{GalleryItemType, GalleryMessageEventContent};
use ruma::{
    MilliSecondsSinceUnixEpoch, OwnedTransactionId, TransactionId,
    events::{
        AnyMessageLikeEventContent, Mentions,
        room::{
            MediaSource, ThumbnailInfo,
            message::{FormattedBody, MessageType, RoomMessageEventContent},
        },
    },
};
use tracing::{Span, debug, error, instrument, trace, warn};

use super::{QueueStorage, QueueThumbnailInfo, RoomSendQueue, RoomSendQueueError};
use crate::{
    Client, Media, Room,
    attachment::{AttachmentConfig, Thumbnail},
    room::edit::update_media_caption,
    send_queue::{
        LocalEcho, LocalEchoContent, MediaHandles, RoomSendQueueStorageError, RoomSendQueueUpdate,
        SendHandle,
    },
};
#[cfg(feature = "unstable-msc4274")]
use crate::{
    attachment::{GalleryConfig, GalleryItemInfo},
    send_queue::GalleryItemQueueInfo,
};

/// Replace the source by the final ones in all the media types handled by
/// [`Room::make_attachment_type()`].
fn update_media_event_after_upload(echo: &mut RoomMessageEventContent, sent: SentMediaInfo) {
    // Some variants look really similar below, but the `event` and `info` are all
    // different types…
    match &mut echo.msgtype {
        MessageType::Audio(event) => {
            event.source = sent.file;
        }
        MessageType::File(event) => {
            event.source = sent.file;
            if let Some(info) = event.info.as_mut() {
                info.thumbnail_source = sent.thumbnail;
            }
        }
        MessageType::Image(event) => {
            event.source = sent.file;
            if let Some(info) = event.info.as_mut() {
                info.thumbnail_source = sent.thumbnail;
            }
        }
        MessageType::Video(event) => {
            event.source = sent.file;
            if let Some(info) = event.info.as_mut() {
                info.thumbnail_source = sent.thumbnail;
            }
        }

        _ => {
            // All `MessageType` created by `Room::make_attachment_type` should be
            // handled here. The only way to end up here is that a message type has
            // been tampered with in the database.
            error!("Invalid message type in database: {}", echo.msgtype());
            // Only crash debug builds.
            debug_assert!(false, "invalid message type in database");
        }
    }
}

/// Replace the sources by the final ones in all the media types handled by
/// [`Room::make_gallery_item_type()`].
#[cfg(feature = "unstable-msc4274")]
fn update_gallery_event_after_upload(
    echo: &mut RoomMessageEventContent,
    sent: HashMap<String, AccumulatedSentMediaInfo>,
) {
    let MessageType::Gallery(gallery) = &mut echo.msgtype else {
        // All `GalleryItemType` created by `Room::make_gallery_item_type` should be
        // handled here. The only way to end up here is that a item type has
        // been tampered with in the database.
        error!("Invalid gallery item types in database");
        // Only crash debug builds.
        debug_assert!(false, "invalid item type in database {:?}", echo.msgtype());
        return;
    };

    // Some variants look really similar below, but the `event` and `info` are all
    // different types…
    for itemtype in gallery.itemtypes.iter_mut() {
        match itemtype {
            GalleryItemType::Audio(event) => match sent.get(&event.source.unique_key()) {
                Some(sent) => event.source = sent.file.clone(),
                None => error!("key for item {:?} does not exist on gallery event", event.source),
            },
            GalleryItemType::File(event) => match sent.get(&event.source.unique_key()) {
                Some(sent) => {
                    event.source = sent.file.clone();
                    if let Some(info) = event.info.as_mut() {
                        info.thumbnail_source = sent.thumbnail.clone();
                    }
                }
                None => error!("key for item {:?} does not exist on gallery event", event.source),
            },
            GalleryItemType::Image(event) => match sent.get(&event.source.unique_key()) {
                Some(sent) => {
                    event.source = sent.file.clone();
                    if let Some(info) = event.info.as_mut() {
                        info.thumbnail_source = sent.thumbnail.clone();
                    }
                }
                None => error!("key for item {:?} does not exist on gallery event", event.source),
            },
            GalleryItemType::Video(event) => match sent.get(&event.source.unique_key()) {
                Some(sent) => {
                    event.source = sent.file.clone();
                    if let Some(info) = event.info.as_mut() {
                        info.thumbnail_source = sent.thumbnail.clone();
                    }
                }
                None => error!("key for item {:?} does not exist on gallery event", event.source),
            },

            _ => {
                // All `GalleryItemType` created by `Room::make_gallery_item_type` should be
                // handled here. The only way to end up here is that a item type has
                // been tampered with in the database.
                error!("Invalid gallery item types in database");
                // Only crash debug builds.
                debug_assert!(false, "invalid gallery item type in database {itemtype:?}");
            }
        }
    }
}

#[derive(Default)]
struct MediaCacheResult {
    upload_thumbnail_txn: Option<OwnedTransactionId>,
    event_thumbnail_info: Option<(MediaSource, Box<ThumbnailInfo>)>,
    queue_thumbnail_info: Option<QueueThumbnailInfo>,
}

impl RoomSendQueue {
    /// Queues an attachment to be sent to the room, using the send queue.
    ///
    /// This returns quickly (without sending or uploading anything), and will
    /// push the event to be sent into a queue, handled in the background.
    ///
    /// Callers are expected to consume [`RoomSendQueueUpdate`] via calling
    /// the [`Self::subscribe()`] method to get updates about the sending of
    /// that event.
    ///
    /// By default, if sending failed on the first attempt, it will be retried a
    /// few times. If sending failed after those retries, the entire
    /// client's sending queue will be disabled, and it will need to be
    /// manually re-enabled by the caller (e.g. after network is back, or when
    /// something has been done about the faulty requests).
    ///
    /// The attachment and its optional thumbnail are stored in the media cache
    /// and can be retrieved at any time, by calling
    /// [`Media::get_media_content()`] with the `MediaSource` that can be found
    /// in the local or remote echo, and using a `MediaFormat::File`.
    #[instrument(skip_all, fields(event_txn))]
    pub async fn send_attachment(
        &self,
        filename: impl Into<String>,
        content_type: Mime,
        data: Vec<u8>,
        mut config: AttachmentConfig,
    ) -> Result<SendHandle, RoomSendQueueError> {
        let Some(room) = self.inner.room.get() else {
            return Err(RoomSendQueueError::RoomDisappeared);
        };

        if room.state() != RoomState::Joined {
            return Err(RoomSendQueueError::RoomNotJoined);
        }

        let filename = filename.into();
        let upload_file_txn = TransactionId::new();
        let send_event_txn = config.txn_id.map_or_else(ChildTransactionId::new, Into::into);

        Span::current().record("event_txn", tracing::field::display(&*send_event_txn));
        debug!(filename, %content_type, %upload_file_txn, "sending an attachment");

        let file_media_request = Media::make_local_file_media_request(&upload_file_txn);

        let MediaCacheResult { upload_thumbnail_txn, event_thumbnail_info, queue_thumbnail_info } =
            RoomSendQueue::cache_media(&room, data, config.thumbnail.take(), &file_media_request)
                .await?;

        // Create the content for the media event.
        let event_content = room
            .make_media_event(
                Room::make_attachment_type(
                    &content_type,
                    filename,
                    file_media_request.source.clone(),
                    config.caption,
                    config.info,
                    event_thumbnail_info,
                ),
                config.mentions,
                config.reply,
            )
            .await
            .map_err(|_| RoomSendQueueError::FailedToCreateAttachment)?;

        let created_at = MilliSecondsSinceUnixEpoch::now();

        // Save requests in the queue storage.
        self.inner
            .queue
            .push_media(
                event_content.clone(),
                content_type,
                send_event_txn.clone().into(),
                created_at,
                upload_file_txn.clone(),
                file_media_request,
                queue_thumbnail_info,
            )
            .await?;

        trace!("manager sends a media to the background task");

        self.inner.notifier.notify_one();

        let send_handle = SendHandle {
            room: self.clone(),
            transaction_id: send_event_txn.clone().into(),
            media_handles: vec![MediaHandles { upload_thumbnail_txn, upload_file_txn }],
            created_at,
        };

        self.send_update(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
            transaction_id: send_event_txn.clone().into(),
            content: LocalEchoContent::Event {
                serialized_event: SerializableEventContent::new(&event_content.into())
                    .map_err(RoomSendQueueStorageError::JsonSerialization)?,
                send_handle: send_handle.clone(),
                send_error: None,
            },
        }));

        Ok(send_handle)
    }

    /// Queues a gallery to be sent to the room, using the send queue.
    ///
    /// This returns quickly (without sending or uploading anything), and will
    /// push the event to be sent into a queue, handled in the background.
    ///
    /// Callers are expected to consume [`RoomSendQueueUpdate`] via calling
    /// the [`Self::subscribe()`] method to get updates about the sending of
    /// that event.
    ///
    /// By default, if sending failed on the first attempt, it will be retried a
    /// few times. If sending failed after those retries, the entire
    /// client's sending queue will be disabled, and it will need to be
    /// manually re-enabled by the caller (e.g. after network is back, or when
    /// something has been done about the faulty requests).
    ///
    /// The attachments and their optional thumbnails are stored in the media
    /// cache and can be retrieved at any time, by calling
    /// [`Media::get_media_content()`] with the `MediaSource` that can be found
    /// in the local or remote echo, and using a `MediaFormat::File`.
    #[cfg(feature = "unstable-msc4274")]
    #[instrument(skip_all, fields(event_txn))]
    pub async fn send_gallery(
        &self,
        gallery: GalleryConfig,
    ) -> Result<SendHandle, RoomSendQueueError> {
        let Some(room) = self.inner.room.get() else {
            return Err(RoomSendQueueError::RoomDisappeared);
        };

        if room.state() != RoomState::Joined {
            return Err(RoomSendQueueError::RoomNotJoined);
        }

        if gallery.is_empty() {
            return Err(RoomSendQueueError::EmptyGallery);
        }

        let send_event_txn =
            gallery.txn_id.clone().map_or_else(ChildTransactionId::new, Into::into);

        Span::current().record("event_txn", tracing::field::display(&*send_event_txn));

        let mut item_types = Vec::with_capacity(gallery.len());
        let mut item_queue_infos = Vec::with_capacity(gallery.len());
        let mut media_handles = Vec::with_capacity(gallery.len());

        for item_info in gallery.items {
            let GalleryItemInfo { filename, content_type, data, .. } = item_info;

            let upload_file_txn = TransactionId::new();

            debug!(filename, %content_type, %upload_file_txn, "uploading a gallery attachment");

            let file_media_request = Media::make_local_file_media_request(&upload_file_txn);

            let MediaCacheResult {
                upload_thumbnail_txn,
                event_thumbnail_info,
                queue_thumbnail_info,
            } = RoomSendQueue::cache_media(&room, data, item_info.thumbnail, &file_media_request)
                .await?;

            item_types.push(Room::make_gallery_item_type(
                &content_type,
                filename,
                file_media_request.source.clone(),
                item_info.caption,
                Some(item_info.attachment_info),
                event_thumbnail_info,
            ));

            item_queue_infos.push(GalleryItemQueueInfo {
                content_type,
                upload_file_txn: upload_file_txn.clone(),
                file_media_request,
                thumbnail: queue_thumbnail_info,
            });

            media_handles.push(MediaHandles { upload_file_txn, upload_thumbnail_txn });
        }

        // Create the content for the gallery event.
        let (body, formatted) =
            gallery.caption.map(|caption| (caption.body, caption.formatted)).unwrap_or_default();
        let event_content = room
            .make_media_event(
                MessageType::Gallery(GalleryMessageEventContent::new(body, formatted, item_types)),
                gallery.mentions,
                gallery.reply,
            )
            .await
            .map_err(|_| RoomSendQueueError::FailedToCreateGallery)?;

        let created_at = MilliSecondsSinceUnixEpoch::now();

        // Save requests in the queue storage.
        self.inner
            .queue
            .push_gallery(
                event_content.clone(),
                send_event_txn.clone().into(),
                created_at,
                item_queue_infos,
            )
            .await?;

        trace!("manager sends a gallery to the background task");

        self.inner.notifier.notify_one();

        let send_handle = SendHandle {
            room: self.clone(),
            transaction_id: send_event_txn.clone().into(),
            media_handles,
            created_at,
        };

        self.send_update(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
            transaction_id: send_event_txn.clone().into(),
            content: LocalEchoContent::Event {
                serialized_event: SerializableEventContent::new(&event_content.into())
                    .map_err(RoomSendQueueStorageError::JsonSerialization)?,
                send_handle: send_handle.clone(),
                send_error: None,
            },
        }));

        Ok(send_handle)
    }

    async fn cache_media(
        room: &Room,
        data: Vec<u8>,
        thumbnail: Option<Thumbnail>,
        file_media_request: &MediaRequestParameters,
    ) -> Result<MediaCacheResult, RoomSendQueueError> {
        let client = room.client();
        let media_store =
            client.media_store().lock().await.map_err(RoomSendQueueStorageError::LockError)?;

        // Cache the file itself in the cache store.
        media_store
            .add_media_content(
                file_media_request,
                data,
                // Make sure that the file is stored until it has been uploaded.
                IgnoreMediaRetentionPolicy::Yes,
            )
            .await
            .map_err(RoomSendQueueStorageError::MediaStoreError)?;

        // Process the thumbnail, if it's been provided.
        if let Some(thumbnail) = thumbnail {
            let txn = TransactionId::new();
            trace!(upload_thumbnail_txn = %txn, "media has a thumbnail");

            // Create the information required for filling the thumbnail section of the
            // event.
            let (data, content_type, thumbnail_info) = thumbnail.into_parts();
            let file_size = data.len();

            let thumbnail_height = thumbnail_info.height;
            let thumbnail_width = thumbnail_info.width;

            // Cache thumbnail in the cache store.
            let thumbnail_media_request = Media::make_local_file_media_request(&txn);
            media_store
                .add_media_content(
                    &thumbnail_media_request,
                    data,
                    // Make sure that the thumbnail is stored until it has been uploaded.
                    IgnoreMediaRetentionPolicy::Yes,
                )
                .await
                .map_err(RoomSendQueueStorageError::MediaStoreError)?;

            Ok(MediaCacheResult {
                upload_thumbnail_txn: Some(txn.clone()),
                event_thumbnail_info: Some((
                    thumbnail_media_request.source.clone(),
                    thumbnail_info,
                )),
                queue_thumbnail_info: Some(QueueThumbnailInfo {
                    finish_upload_thumbnail_info: FinishUploadThumbnailInfo {
                        txn,
                        width: thumbnail_width,
                        height: thumbnail_height,
                    },
                    media_request_parameters: thumbnail_media_request,
                    content_type,
                    file_size,
                }),
            })
        } else {
            Ok(Default::default())
        }
    }
}

impl QueueStorage {
    /// Consumes a finished upload and queues sending of the final media event.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn handle_dependent_finish_upload(
        &self,
        client: &Client,
        event_txn: OwnedTransactionId,
        parent_key: SentRequestKey,
        mut local_echo: RoomMessageEventContent,
        file_upload_txn: OwnedTransactionId,
        thumbnail_info: Option<FinishUploadThumbnailInfo>,
        new_updates: &mut Vec<RoomSendQueueUpdate>,
    ) -> Result<(), RoomSendQueueError> {
        // Both uploads are ready: enqueue the event with its final data.
        let sent_media = parent_key
            .into_media()
            .ok_or(RoomSendQueueError::StorageError(RoomSendQueueStorageError::InvalidParentKey))?;

        update_media_cache_keys_after_upload(client, &file_upload_txn, thumbnail_info, &sent_media)
            .await?;
        update_media_event_after_upload(&mut local_echo, sent_media);

        let new_content = SerializableEventContent::new(&local_echo.into())
            .map_err(RoomSendQueueStorageError::JsonSerialization)?;

        // Indicates observers that the upload finished, by editing the local echo for
        // the event into its final form before sending.
        new_updates.push(RoomSendQueueUpdate::ReplacedLocalEvent {
            transaction_id: event_txn.clone(),
            new_content: new_content.clone(),
        });

        trace!(%event_txn, "queueing media event after successfully uploading media(s)");

        client
            .state_store()
            .save_send_queue_request(
                &self.room_id,
                event_txn,
                MilliSecondsSinceUnixEpoch::now(),
                new_content.into(),
                Self::HIGH_PRIORITY,
            )
            .await
            .map_err(RoomSendQueueStorageError::StateStoreError)?;

        Ok(())
    }

    /// Consumes a finished gallery upload and queues sending of the final
    /// gallery event.
    #[cfg(feature = "unstable-msc4274")]
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn handle_dependent_finish_gallery_upload(
        &self,
        client: &Client,
        event_txn: OwnedTransactionId,
        parent_key: SentRequestKey,
        mut local_echo: RoomMessageEventContent,
        item_infos: Vec<FinishGalleryItemInfo>,
        new_updates: &mut Vec<RoomSendQueueUpdate>,
    ) -> Result<(), RoomSendQueueError> {
        // All uploads are ready: enqueue the event with its final data.
        let sent_gallery = parent_key
            .into_media()
            .ok_or(RoomSendQueueError::StorageError(RoomSendQueueStorageError::InvalidParentKey))?;

        let mut sent_media_vec = sent_gallery.accumulated;
        sent_media_vec.push(AccumulatedSentMediaInfo {
            file: sent_gallery.file,
            thumbnail: sent_gallery.thumbnail,
        });

        let mut sent_infos = HashMap::new();

        for (item_info, sent_media) in zip(item_infos, sent_media_vec) {
            let FinishGalleryItemInfo { file_upload: file_upload_txn, thumbnail_info } = item_info;

            // Store the sent media under the original cache key for later insertion into
            // the local echo.
            let from_req = Media::make_local_file_media_request(&file_upload_txn);
            sent_infos.insert(from_req.source.unique_key(), sent_media.clone());

            update_media_cache_keys_after_upload(
                client,
                &file_upload_txn,
                thumbnail_info,
                &sent_media.into(),
            )
            .await?;
        }

        update_gallery_event_after_upload(&mut local_echo, sent_infos);

        let new_content = SerializableEventContent::new(&local_echo.into())
            .map_err(RoomSendQueueStorageError::JsonSerialization)?;

        // Indicates observers that the upload finished, by editing the local echo for
        // the event into its final form before sending.
        new_updates.push(RoomSendQueueUpdate::ReplacedLocalEvent {
            transaction_id: event_txn.clone(),
            new_content: new_content.clone(),
        });

        trace!(%event_txn, "queueing media event after successfully uploading media(s)");

        client
            .state_store()
            .save_send_queue_request(
                &self.room_id,
                event_txn,
                MilliSecondsSinceUnixEpoch::now(),
                new_content.into(),
                Self::HIGH_PRIORITY,
            )
            .await
            .map_err(RoomSendQueueStorageError::StateStoreError)?;

        Ok(())
    }

    /// Consumes a finished file or thumbnail upload and queues the dependent
    /// file or thumbnail upload.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn handle_dependent_file_or_thumbnail_upload(
        &self,
        client: &Client,
        next_upload_txn: OwnedTransactionId,
        parent_key: SentRequestKey,
        content_type: String,
        cache_key: MediaRequestParameters,
        event_txn: OwnedTransactionId,
        parent_is_thumbnail_upload: bool,
    ) -> Result<(), RoomSendQueueError> {
        // The previous file or thumbnail has been sent, now transform the dependent
        // file or thumbnail upload request into a ready one.
        let sent_media = parent_key
            .into_media()
            .ok_or(RoomSendQueueError::StorageError(RoomSendQueueStorageError::InvalidParentKey))?;

        // If the previous upload was a thumbnail, it shouldn't have
        // a thumbnail itself.
        if parent_is_thumbnail_upload {
            debug_assert!(sent_media.thumbnail.is_none());
            if sent_media.thumbnail.is_some() {
                warn!("unexpected thumbnail for a thumbnail!");
            }
        }

        trace!(
            related_to = %event_txn,
            "done uploading file or thumbnail, now queuing the dependent file \
             or thumbnail upload request",
        );

        // If the parent request was a thumbnail upload, don't add it to the list of
        // accumulated medias yet because its dependent file upload is still
        // pending. If the parent request was a file upload, we know that both
        // the file and its thumbnail (if any) have finished uploading and we
        // can add them to the accumulated sent media.
        #[cfg(feature = "unstable-msc4274")]
        let accumulated = if parent_is_thumbnail_upload {
            sent_media.accumulated
        } else {
            let mut accumulated = sent_media.accumulated;
            accumulated.push(AccumulatedSentMediaInfo {
                file: sent_media.file.clone(),
                thumbnail: sent_media.thumbnail,
            });
            accumulated
        };

        let request = QueuedRequestKind::MediaUpload {
            content_type,
            cache_key,
            // If the previous upload was a thumbnail, it becomes the thumbnail source for the next
            // upload.
            thumbnail_source: parent_is_thumbnail_upload.then_some(sent_media.file),
            related_to: event_txn,
            #[cfg(feature = "unstable-msc4274")]
            accumulated,
        };

        client
            .state_store()
            .save_send_queue_request(
                &self.room_id,
                next_upload_txn,
                MilliSecondsSinceUnixEpoch::now(),
                request,
                Self::HIGH_PRIORITY,
            )
            .await
            .map_err(RoomSendQueueStorageError::StateStoreError)?;

        Ok(())
    }

    /// Try to abort an upload that would be ongoing.
    ///
    /// Return true if any media (media itself or its thumbnail) was being
    /// uploaded. In this case, the media event has also been removed from
    /// the send queue. If it returns false, then the uploads already
    /// happened, and the event sending *may* have started.
    #[instrument(skip(self, handles))]
    pub(super) async fn abort_upload(
        &self,
        event_txn: &TransactionId,
        handles: &MediaHandles,
    ) -> Result<bool, RoomSendQueueStorageError> {
        let mut guard = self.store.lock().await;
        let client = guard.client()?;

        // Keep the lock until we're done touching the storage.
        debug!("trying to abort an upload");

        let store = client.state_store();

        let upload_file_as_dependent = ChildTransactionId::from(handles.upload_file_txn.clone());
        let event_as_dependent = ChildTransactionId::from(event_txn.to_owned());

        let mut removed_dependent_upload = false;
        let mut removed_dependent_event = false;

        if let Some(thumbnail_txn) = &handles.upload_thumbnail_txn
            && store.remove_send_queue_request(&self.room_id, thumbnail_txn).await?
        {
            // The thumbnail upload existed as a request: either it was pending (something
            // else was being sent), or it was actively being sent.
            trace!("could remove thumbnail request, removing 2 dependent requests now");

            // 1. Try to abort sending using the being_sent info, in case it was active.
            if let Some(info) = guard.being_sent.as_ref()
                && info.transaction_id == *thumbnail_txn
            {
                // SAFETY: we knew it was Some(), two lines above.
                let info = guard.being_sent.take().unwrap();
                if info.cancel_upload() {
                    trace!("aborted ongoing thumbnail upload");
                }
            }

            // 2. Remove the dependent requests.
            removed_dependent_upload = store
                .remove_dependent_queued_request(&self.room_id, &upload_file_as_dependent)
                .await?;

            if !removed_dependent_upload {
                warn!("unable to find the dependent file upload request");
            }

            removed_dependent_event =
                store.remove_dependent_queued_request(&self.room_id, &event_as_dependent).await?;

            if !removed_dependent_event {
                warn!("unable to find the dependent media event upload request");
            }
        }

        // If we're here:
        // - either there was no thumbnail to upload,
        // - or the thumbnail request has terminated already.
        //
        // So the next target is the upload request itself, in both cases.

        if !removed_dependent_upload {
            if store.remove_send_queue_request(&self.room_id, &handles.upload_file_txn).await? {
                // The upload existed as a request: either it was pending (something else was
                // being sent), or it was actively being sent.
                trace!("could remove file upload request, removing 1 dependent request");

                // 1. Try to abort sending using the being_sent info, in case it was active.
                if let Some(info) = guard.being_sent.as_ref()
                    && info.transaction_id == handles.upload_file_txn
                {
                    // SAFETY: we knew it was Some(), two lines above.
                    let info = guard.being_sent.take().unwrap();
                    if info.cancel_upload() {
                        trace!("aborted ongoing file upload");
                    }
                }

                // 2. Remove the dependent request.
                if !store
                    .remove_dependent_queued_request(&self.room_id, &event_as_dependent)
                    .await?
                {
                    warn!("unable to find the dependent media event upload request");
                }
            } else {
                // The upload was not in the send queue, so it's completed.
                //
                // It means the event sending is either still queued as a dependent request, or
                // it's graduated into a request.
                if !removed_dependent_event
                    && !store
                        .remove_dependent_queued_request(&self.room_id, &event_as_dependent)
                        .await?
                {
                    // The media event has been promoted into a request, or the promoted request
                    // has been sent already: we couldn't abort, let the caller decide what to do.
                    debug!("uploads already happened => deferring to aborting an event sending");
                    return Ok(false);
                }
            }
        }

        // At this point, all the requests and dependent requests have been cleaned up.
        // Perform the final step: empty the cache from the local items.
        {
            let media_store = client.media_store().lock().await?;
            media_store
                .remove_media_content_for_uri(&Media::make_local_uri(&handles.upload_file_txn))
                .await?;
            if let Some(txn) = &handles.upload_thumbnail_txn {
                media_store.remove_media_content_for_uri(&Media::make_local_uri(txn)).await?;
            }
        }

        debug!("successfully aborted!");
        Ok(true)
    }

    #[instrument(skip(self, caption, formatted_caption))]
    pub(super) async fn edit_media_caption(
        &self,
        txn: &TransactionId,
        caption: Option<String>,
        formatted_caption: Option<FormattedBody>,
        mentions: Option<Mentions>,
    ) -> Result<Option<AnyMessageLikeEventContent>, RoomSendQueueStorageError> {
        // This error will be popular here.
        use RoomSendQueueStorageError::InvalidMediaCaptionEdit;

        let guard = self.store.lock().await;
        let client = guard.client()?;
        let store = client.state_store();

        // The media event can be in one of three states:
        // - still stored as a dependent request,
        // - stored as a queued request, active (aka it's being sent).
        // - stored as a queued request, not active yet (aka it's not being sent yet),
        //
        // We'll handle each of these cases one by one.

        {
            // If the event can be found as a dependent event, update the captions, save it
            // back into the database, and return early.
            let dependent_requests = store.load_dependent_queued_requests(&self.room_id).await?;

            if let Some(found) =
                dependent_requests.into_iter().find(|req| *req.own_transaction_id == *txn)
            {
                trace!("found the caption to edit in a dependent request");

                let DependentQueuedRequestKind::FinishUpload {
                    mut local_echo,
                    file_upload,
                    thumbnail_info,
                } = found.kind
                else {
                    return Err(InvalidMediaCaptionEdit);
                };

                if !update_media_caption(&mut local_echo, caption, formatted_caption, mentions) {
                    return Err(InvalidMediaCaptionEdit);
                }

                let new_dependent_request = DependentQueuedRequestKind::FinishUpload {
                    local_echo: local_echo.clone(),
                    file_upload,
                    thumbnail_info,
                };
                store
                    .update_dependent_queued_request(
                        &self.room_id,
                        &found.own_transaction_id,
                        new_dependent_request,
                    )
                    .await?;

                trace!("caption successfully updated");
                return Ok(Some((*local_echo).into()));
            }
        }

        let requests = store.load_send_queue_requests(&self.room_id).await?;
        let Some(found) = requests.into_iter().find(|req| req.transaction_id == *txn) else {
            // Couldn't be found anymore, it's not possible to update captions.
            return Ok(None);
        };

        trace!("found the caption to edit as a request");

        let QueuedRequestKind::Event { content: serialized_content } = found.kind else {
            return Err(InvalidMediaCaptionEdit);
        };

        let deserialized = serialized_content.deserialize()?;
        let AnyMessageLikeEventContent::RoomMessage(mut content) = deserialized else {
            return Err(InvalidMediaCaptionEdit);
        };

        if !update_media_caption(&mut content, caption, formatted_caption, mentions) {
            return Err(InvalidMediaCaptionEdit);
        }

        let any_content: AnyMessageLikeEventContent = content.into();
        let new_serialized = SerializableEventContent::new(&any_content.clone())?;

        // If the request is active (being sent), send a dependent request.
        if let Some(being_sent) = guard.being_sent.as_ref()
            && being_sent.transaction_id == *txn
        {
            // Record a dependent request to edit, and exit.
            store
                .save_dependent_queued_request(
                    &self.room_id,
                    txn,
                    ChildTransactionId::new(),
                    MilliSecondsSinceUnixEpoch::now(),
                    DependentQueuedRequestKind::EditEvent { new_content: new_serialized },
                )
                .await?;

            trace!("media event was being sent, pushed a dependent edit");
            return Ok(Some(any_content));
        }

        // The request is not active: edit the local echo.
        store
            .update_send_queue_request(
                &self.room_id,
                txn,
                QueuedRequestKind::Event { content: new_serialized },
            )
            .await?;

        trace!("media event was not being sent, updated local echo");
        Ok(Some(any_content))
    }
}

/// Update cache keys in the cache store after uploading a media file /
/// thumbnail.
async fn update_media_cache_keys_after_upload(
    client: &Client,
    file_upload_txn: &OwnedTransactionId,
    thumbnail_info: Option<FinishUploadThumbnailInfo>,
    sent_media: &SentMediaInfo,
) -> Result<(), RoomSendQueueError> {
    // Do it for the file itself.
    let from_req = Media::make_local_file_media_request(file_upload_txn);

    trace!(from = ?from_req.source, to = ?sent_media.file, "renaming media file key in cache store");
    let media_store =
        client.media_store().lock().await.map_err(RoomSendQueueStorageError::LockError)?;

    // The media file can now be removed during cleanups.
    media_store
        .set_ignore_media_retention_policy(&from_req, IgnoreMediaRetentionPolicy::No)
        .await
        .map_err(RoomSendQueueStorageError::MediaStoreError)?;

    media_store
        .replace_media_key(
            &from_req,
            &MediaRequestParameters { source: sent_media.file.clone(), format: MediaFormat::File },
        )
        .await
        .map_err(RoomSendQueueStorageError::MediaStoreError)?;

    // Rename the thumbnail too, if needs be.
    if let Some((info, remote_thumbnail_source)) =
        thumbnail_info.as_ref().zip(sent_media.thumbnail.clone())
    {
        let from_request_params = Media::make_local_file_media_request(&info.txn);

        if let Some((height, width)) = info.height.zip(info.width) {
            trace!(
                from = ?from_req.source,
                to = ?remote_thumbnail_source,
                height = u64::from(height),
                width = u64::from(width),
                "storing thumbnail as a thumbnail for the uploaded media, and a file in itself, in cache store"
            );

            // Try to reload the content of the thumbnail from the media store. As it's not
            // a strong requirement to store the thumbnail a second time, we
            // silently log errors instead of propagating them to the caller.
            match media_store
                .get_media_content(&MediaRequestParameters {
                    source: from_request_params.source.clone(),
                    format: MediaFormat::File,
                })
                .await
            {
                Ok(Some(thumbnail_content)) => {
                    // Also cache this as a thumbnail for the thumbnail, in the media store; the
                    // ElementX apps expect that specific format for thumbnails.
                    media_store
                        .add_media_content(
                            &MediaRequestParameters {
                                source: remote_thumbnail_source.clone(),
                                format: MediaFormat::Thumbnail(MediaThumbnailSettings::new(
                                    width, height,
                                )),
                            },
                            thumbnail_content,
                            IgnoreMediaRetentionPolicy::No,
                        )
                        .await
                        .map_err(RoomSendQueueStorageError::MediaStoreError)?;
                }

                Ok(None) => {
                    warn!(
                        from = ?from_request_params.source,
                        "unable to reload thumbnail content from media store: no content found",
                    );
                }

                Err(err) => {
                    // Silently log the error, but proceed, as storing the thumbnail as such isn't
                    // a strong requirement.
                    error!(
                        from = ?from_request_params.source,
                        "unable to reload thumbnail content from media store: {err}"
                    );
                }
            }
        } else {
            trace!(from = ?from_req.source, to = ?remote_thumbnail_source, "only renaming thumbnail key to file in cache store");
        }

        // The thumbnail file can now be removed during cleanups.
        media_store
            .set_ignore_media_retention_policy(&from_request_params, IgnoreMediaRetentionPolicy::No)
            .await
            .map_err(RoomSendQueueStorageError::MediaStoreError)?;

        // Save the thumbnail as a file as well.
        media_store
            .replace_media_key(
                &from_request_params,
                &MediaRequestParameters {
                    source: remote_thumbnail_source,
                    format: MediaFormat::File,
                },
            )
            .await
            .map_err(RoomSendQueueStorageError::MediaStoreError)?;
    }

    Ok(())
}
