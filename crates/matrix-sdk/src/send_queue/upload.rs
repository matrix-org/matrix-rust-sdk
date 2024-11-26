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

use matrix_sdk_base::{
    media::{MediaFormat, MediaRequestParameters, MediaThumbnailSettings},
    store::{
        ChildTransactionId, DependentQueuedRequestKind, FinishUploadThumbnailInfo,
        QueuedRequestKind, SentMediaInfo, SentRequestKey, SerializableEventContent,
    },
    RoomState,
};
use mime::Mime;
use ruma::{
    events::{
        room::{
            message::{FormattedBody, MessageType, RoomMessageEventContent},
            MediaSource,
        },
        AnyMessageLikeEventContent,
    },
    OwnedMxcUri, OwnedTransactionId, TransactionId, UInt,
};
use tracing::{debug, error, instrument, trace, warn, Span};

use super::{QueueStorage, RoomSendQueue, RoomSendQueueError};
use crate::{
    attachment::AttachmentConfig,
    room::edit::update_media_caption,
    send_queue::{
        LocalEcho, LocalEchoContent, MediaHandles, RoomSendQueueStorageError, RoomSendQueueUpdate,
        SendHandle,
    },
    Client, Room,
};

/// Create an [`OwnedMxcUri`] for a file or thumbnail we want to store locally
/// before sending it.
///
/// This uses a MXC ID that is only locally valid.
fn make_local_uri(txn_id: &TransactionId) -> OwnedMxcUri {
    // This mustn't represent a potentially valid media server, otherwise it'd be
    // possible for an attacker to return malicious content under some
    // preconditions (e.g. the cache store has been cleared before the upload
    // took place). To mitigate against this, we use the .localhost TLD,
    // which is guaranteed to be on the local machine. As a result, the only attack
    // possible would be coming from the user themselves, which we consider a
    // non-threat.
    OwnedMxcUri::from(format!("mxc://send-queue.localhost/{txn_id}"))
}

/// Create a [`MediaRequest`] for a file we want to store locally before
/// sending it.
///
/// This uses a MXC ID that is only locally valid.
fn make_local_file_media_request(txn_id: &TransactionId) -> MediaRequestParameters {
    MediaRequestParameters {
        source: MediaSource::Plain(make_local_uri(txn_id)),
        format: MediaFormat::File,
    }
}

/// Create a [`MediaRequest`] for a file we want to store locally before
/// sending it.
///
/// This uses a MXC ID that is only locally valid.
fn make_local_thumbnail_media_request(
    txn_id: &TransactionId,
    height: UInt,
    width: UInt,
) -> MediaRequestParameters {
    // See comment in [`make_local_file_media_request`].
    MediaRequestParameters {
        source: MediaSource::Plain(make_local_uri(txn_id)),
        format: MediaFormat::Thumbnail(MediaThumbnailSettings::new(width, height)),
    }
}

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
    #[instrument(skip_all, fields(event_txn))]
    pub async fn send_attachment(
        &self,
        filename: &str,
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

        let upload_file_txn = TransactionId::new();
        let send_event_txn = config.txn_id.map_or_else(ChildTransactionId::new, Into::into);

        Span::current().record("event_txn", tracing::field::display(&*send_event_txn));
        debug!(filename, %content_type, %upload_file_txn, "sending an attachment");

        let file_media_request = make_local_file_media_request(&upload_file_txn);

        let (upload_thumbnail_txn, event_thumbnail_info, queue_thumbnail_info) = {
            let client = room.client();
            let cache_store = client
                .event_cache_store()
                .lock()
                .await
                .map_err(RoomSendQueueStorageError::LockError)?;

            // Cache the file itself in the cache store.
            cache_store
                .add_media_content(&file_media_request, data.clone())
                .await
                .map_err(RoomSendQueueStorageError::EventCacheStoreError)?;

            // Process the thumbnail, if it's been provided.
            if let Some(thumbnail) = config.thumbnail.take() {
                // Normalize information to retrieve the thumbnail in the cache store.
                let height = thumbnail.height;
                let width = thumbnail.width;

                let txn = TransactionId::new();
                trace!(upload_thumbnail_txn = %txn, thumbnail_size = ?(height, width), "attachment has a thumbnail");

                // Create the information required for filling the thumbnail section of the
                // media event.
                let (data, content_type, thumbnail_info) = thumbnail.into_parts();

                // Cache thumbnail in the cache store.
                let thumbnail_media_request =
                    make_local_thumbnail_media_request(&txn, height, width);
                cache_store
                    .add_media_content(&thumbnail_media_request, data)
                    .await
                    .map_err(RoomSendQueueStorageError::EventCacheStoreError)?;

                (
                    Some(txn.clone()),
                    Some((thumbnail_media_request.source.clone(), thumbnail_info)),
                    Some((
                        FinishUploadThumbnailInfo { txn, width, height },
                        thumbnail_media_request,
                        content_type,
                    )),
                )
            } else {
                Default::default()
            }
        };

        // Create the content for the media event.
        let event_content = Room::make_attachment_event(
            room.make_attachment_type(
                &content_type,
                filename,
                file_media_request.source.clone(),
                config.caption,
                config.formatted_caption,
                config.info,
                event_thumbnail_info,
            ),
            config.mentions,
        );

        // Save requests in the queue storage.
        self.inner
            .queue
            .push_media(
                event_content.clone(),
                content_type,
                send_event_txn.clone().into(),
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
            media_handles: Some(MediaHandles { upload_thumbnail_txn, upload_file_txn }),
        };

        let _ = self.inner.updates.send(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
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

        // Update cache keys in the cache store.
        {
            // Do it for the file itself.
            let from_req = make_local_file_media_request(&file_upload_txn);

            trace!(from = ?from_req.source, to = ?sent_media.file, "renaming media file key in cache store");
            let cache_store = client
                .event_cache_store()
                .lock()
                .await
                .map_err(RoomSendQueueStorageError::LockError)?;

            cache_store
                .replace_media_key(
                    &from_req,
                    &MediaRequestParameters {
                        source: sent_media.file.clone(),
                        format: MediaFormat::File,
                    },
                )
                .await
                .map_err(RoomSendQueueStorageError::EventCacheStoreError)?;

            // Rename the thumbnail too, if needs be.
            if let Some((info, new_source)) =
                thumbnail_info.as_ref().zip(sent_media.thumbnail.clone())
            {
                let from_req =
                    make_local_thumbnail_media_request(&info.txn, info.height, info.width);

                trace!(from = ?from_req.source, to = ?new_source, "renaming thumbnail file key in cache store");

                // Reuse the same format for the cached thumbnail with the final MXC ID.
                let new_format = from_req.format.clone();

                cache_store
                    .replace_media_key(
                        &from_req,
                        &MediaRequestParameters { source: new_source, format: new_format },
                    )
                    .await
                    .map_err(RoomSendQueueStorageError::EventCacheStoreError)?;
            }
        }

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
            .store()
            .save_send_queue_request(
                &self.room_id,
                event_txn,
                new_content.into(),
                Self::HIGH_PRIORITY,
            )
            .await
            .map_err(RoomSendQueueStorageError::StateStoreError)?;

        Ok(())
    }

    /// Consumes a finished upload of a thumbnail and queues the file upload.
    pub(super) async fn handle_dependent_file_upload_with_thumbnail(
        &self,
        client: &Client,
        next_upload_txn: OwnedTransactionId,
        parent_key: SentRequestKey,
        content_type: String,
        cache_key: MediaRequestParameters,
        event_txn: OwnedTransactionId,
    ) -> Result<(), RoomSendQueueError> {
        // The thumbnail has been sent, now transform the dependent file upload request
        // into a ready one.
        let sent_media = parent_key
            .into_media()
            .ok_or(RoomSendQueueError::StorageError(RoomSendQueueStorageError::InvalidParentKey))?;

        // The media we just uploaded was a thumbnail, so the thumbnail shouldn't have
        // a thumbnail itself.
        debug_assert!(sent_media.thumbnail.is_none());
        if sent_media.thumbnail.is_some() {
            warn!("unexpected thumbnail for a thumbnail!");
        }

        trace!(related_to = %event_txn, "done uploading thumbnail, now queuing a request to send the media file itself");

        let request = QueuedRequestKind::MediaUpload {
            content_type,
            cache_key,
            // The thumbnail for the next upload is the file we just uploaded here.
            thumbnail_source: Some(sent_media.file),
            related_to: event_txn,
        };

        client
            .store()
            .save_send_queue_request(&self.room_id, next_upload_txn, request, Self::HIGH_PRIORITY)
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

        let store = client.store();

        let upload_file_as_dependent = ChildTransactionId::from(handles.upload_file_txn.clone());
        let event_as_dependent = ChildTransactionId::from(event_txn.to_owned());

        let mut removed_dependent_upload = false;
        let mut removed_dependent_event = false;

        if let Some(thumbnail_txn) = &handles.upload_thumbnail_txn {
            if store.remove_send_queue_request(&self.room_id, thumbnail_txn).await? {
                // The thumbnail upload existed as a request: either it was pending (something
                // else was being sent), or it was actively being sent.
                trace!("could remove thumbnail request, removing 2 dependent requests now");

                // 1. Try to abort sending using the being_sent info, in case it was active.
                if let Some(info) = guard.being_sent.as_ref() {
                    if info.transaction_id == *thumbnail_txn {
                        // SAFETY: we knew it was Some(), two lines above.
                        let info = guard.being_sent.take().unwrap();
                        if info.cancel_upload() {
                            trace!("aborted ongoing thumbnail upload");
                        }
                    }
                }

                // 2. Remove the dependent requests.
                removed_dependent_upload = store
                    .remove_dependent_queued_request(&self.room_id, &upload_file_as_dependent)
                    .await?;

                if !removed_dependent_upload {
                    warn!("unable to find the dependent file upload request");
                }

                removed_dependent_event = store
                    .remove_dependent_queued_request(&self.room_id, &event_as_dependent)
                    .await?;

                if !removed_dependent_event {
                    warn!("unable to find the dependent media event upload request");
                }
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
                if let Some(info) = guard.being_sent.as_ref() {
                    if info.transaction_id == handles.upload_file_txn {
                        // SAFETY: we knew it was Some(), two lines above.
                        let info = guard.being_sent.take().unwrap();
                        if info.cancel_upload() {
                            trace!("aborted ongoing file upload");
                        }
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
            let event_cache = client.event_cache_store().lock().await?;
            event_cache
                .remove_media_content_for_uri(&make_local_uri(&handles.upload_file_txn))
                .await?;
            if let Some(txn) = &handles.upload_thumbnail_txn {
                event_cache.remove_media_content_for_uri(&make_local_uri(txn)).await?;
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
    ) -> Result<Option<AnyMessageLikeEventContent>, RoomSendQueueStorageError> {
        // This error will be popular here.
        use RoomSendQueueStorageError::InvalidMediaCaptionEdit;

        let guard = self.store.lock().await;
        let client = guard.client()?;
        let store = client.store();

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

                if !update_media_caption(&mut local_echo, caption, formatted_caption) {
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
                return Ok(Some(local_echo.into()));
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

        if !update_media_caption(&mut content, caption, formatted_caption) {
            return Err(InvalidMediaCaptionEdit);
        }

        let any_content: AnyMessageLikeEventContent = content.into();
        let new_serialized = SerializableEventContent::new(&any_content.clone())?;

        // If the request is active (being sent), send a dependent request.
        if let Some(being_sent) = guard.being_sent.as_ref() {
            if being_sent.transaction_id == *txn {
                // Record a dependent request to edit, and exit.
                store
                    .save_dependent_queued_request(
                        &self.room_id,
                        txn,
                        ChildTransactionId::new(),
                        DependentQueuedRequestKind::EditEvent { new_content: new_serialized },
                    )
                    .await?;

                trace!("media event was being sent, pushed a dependent edit");
                return Ok(Some(any_content));
            }
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
