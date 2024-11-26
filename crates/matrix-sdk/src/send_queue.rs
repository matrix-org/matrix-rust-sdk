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

//! A send queue facility to serializing queuing and sending of messages.
//!
//! # [`Room`] send queue
//!
//! Each room gets its own [`RoomSendQueue`], that's available by calling
//! [`Room::send_queue()`]. The first time this method is called, it will spawn
//! a background task that's used to actually send events, in the order they
//! were passed from calls to [`RoomSendQueue::send()`].
//!
//! This queue tries to simplify error management around sending events, using
//! [`RoomSendQueue::send`] or [`RoomSendQueue::send_raw`]: by default, it will retry to send the
//! same event a few times, before automatically disabling itself, and emitting
//! a notification that can be listened to with the global send queue (see
//! paragraph below) or using [`RoomSendQueue::subscribe()`].
//!
//! It is possible to control whether a single room is enabled using
//! [`RoomSendQueue::set_enabled()`].
//!
//! # Global [`SendQueue`] object
//!
//! The [`Client::send_queue()`] method returns an API object allowing to
//! control all the room send queues:
//!
//! - enable/disable them all at once with [`SendQueue::set_enabled()`].
//! - get notifications about send errors with [`SendQueue::subscribe_errors`].
//! - reload all unsent events that had been persisted in storage using
//!   [`SendQueue::respawn_tasks_for_rooms_with_unsent_requests()`]. It is
//!   recommended to call this method during initialization of a client,
//!   otherwise persisted unsent events will only be re-sent after the send
//!   queue for the given room has been reopened for the first time.
//!
//! # Send handle
//!
//! Just after queuing a request to send something, a [`SendHandle`] is
//! returned, allowing manipulating the inflight request.
//!
//! For a send handle for an event, it's possible to edit the event / abort
//! sending it. If it was still in the queue (i.e. not sent yet, or not being
//! sent), then such an action would happen locally (i.e. in the database).
//! Otherwise, it is "too late": the background task may be sending
//! the event already, or has sent it; in that case, the edit/aborting must
//! happen as an actual event materializing this, on the server. To accomplish
//! this, the send queue may send such an event, using the dependency system
//! described below.
//!
//! # Dependency system
//!
//! The send queue includes a simple dependency system, where a
//! [`QueuedRequest`] can have zero or more dependents in the form of
//! [`DependentQueuedRequest`]. A dependent queued request can have at most one
//! depended-upon (parent) queued request.
//!
//! This allows implementing deferred edits/redacts, as hinted to in the
//! previous section.
//!
//! ## Media upload
//!
//! This dependency system also allows uploading medias, since the media's
//! *content* must be uploaded before we send the media *event* that describes
//! it.
//!
//! In the simplest case, that is, a media file and its event must be sent (i.e.
//! no thumbnails):
//!
//! - The file's content is immediately cached in the
//!   [`matrix_sdk_base::event_cache::store::EventCacheStore`], using an MXC ID
//!   that is temporary and designates a local URI without any possible doubt.
//! - An initial media event is created and uses this temporary MXC ID, and
//!   propagated as a local echo for an event.
//! - A [`QueuedRequest`] is pushed to upload the file's media
//!   ([`QueuedRequestKind::MediaUpload`]).
//! - A [`DependentQueuedRequest`] is pushed to finish the upload
//!   ([`DependentQueuedRequestKind::FinishUpload`]).
//!
//! What is expected to happen, if all goes well, is the following:
//!
//! - the media is uploaded to the media homeserver, which returns the final MXC
//!   ID.
//! - when marking the upload request as sent, the MXC ID is injected (as a
//!   [`matrix_sdk_base::store::SentRequestKey`]) into the dependent request
//!   [`DependentQueuedRequestKind::FinishUpload`] created in the last step
//!   above.
//! - next time the send queue handles dependent queries, it'll see this one is
//!   ready to be sent, and it will transform it into an event queued request
//!   ([`QueuedRequestKind::Event`]), with the event created in the local echo
//!   before, updated with the MXC ID returned from the server.
//! - this updated local echo is also propagated as an edit of the local echo to
//!   observers, who get the final version with the final MXC IDs at this point
//!   too.
//! - then the event is sent normally, as any event sent with the send queue.
//!
//! When there is a thumbnail, things behave similarly, with some tweaks:
//!
//! - the thumbnail's content is also stored into the cache store immediately,
//! - the thumbnail is sent first as an [`QueuedRequestKind::MediaUpload`]
//!   request,
//! - the file upload is pushed as a dependent request of kind
//!   [`DependentQueuedRequestKind::UploadFileWithThumbnail`] (this variant
//!   keeps the file's key used to look it up in the cache store).
//! - the media event is then sent as a dependent request as described in the
//!   previous section.
//!
//! What's expected to happen is thus the following:
//!
//! - After the thumbnail has been uploaded, the dependent query will retrieve
//!   the final MXC ID returned by the homeserver for the thumbnail, and store
//!   it into the [`QueuedRequestKind::MediaUpload`]'s `thumbnail_source` field,
//!   allowing to remember the thumbnail MXC ID when it's time to finish the
//!   upload later.
//! - The dependent request is morphed into another
//!   [`QueuedRequestKind::MediaUpload`], for the file itself.
//!
//! The rest of the process is then similar to that of uploading a file without
//! a thumbnail. The only difference is that there's a thumbnail source (MXC ID)
//! remembered and fixed up into the media event, just before sending it.

use std::{
    collections::{BTreeMap, HashMap},
    str::FromStr as _,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock,
    },
};

use as_variant::as_variant;
use matrix_sdk_base::{
    event_cache::store::EventCacheStoreError,
    media::MediaRequestParameters,
    store::{
        ChildTransactionId, DependentQueuedRequest, DependentQueuedRequestKind,
        FinishUploadThumbnailInfo, QueueWedgeError, QueuedRequest, QueuedRequestKind,
        SentMediaInfo, SentRequestKey, SerializableEventContent,
    },
    store_locks::LockStoreError,
    RoomState, StoreError,
};
use matrix_sdk_common::executor::{spawn, JoinHandle};
use mime::Mime;
use ruma::{
    events::{
        reaction::ReactionEventContent,
        relation::Annotation,
        room::{
            message::{FormattedBody, RoomMessageEventContent},
            MediaSource,
        },
        AnyMessageLikeEventContent, EventContent as _,
    },
    serde::Raw,
    OwnedEventId, OwnedRoomId, OwnedTransactionId, TransactionId,
};
use tokio::sync::{broadcast, oneshot, Mutex, Notify, OwnedMutexGuard};
use tracing::{debug, error, info, instrument, trace, warn};

#[cfg(feature = "e2e-encryption")]
use crate::crypto::{OlmError, SessionRecipientCollectionError};
use crate::{
    client::WeakClient,
    config::RequestConfig,
    error::RetryKind,
    room::{edit::EditedContent, WeakRoom},
    Client, Media, Room,
};

mod upload;

/// A client-wide send queue, for all the rooms known by a client.
pub struct SendQueue {
    client: Client,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for SendQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendQueue").finish_non_exhaustive()
    }
}

impl SendQueue {
    pub(super) fn new(client: Client) -> Self {
        Self { client }
    }

    /// Reload all the rooms which had unsent requests, and respawn tasks for
    /// those rooms.
    pub async fn respawn_tasks_for_rooms_with_unsent_requests(&self) {
        if !self.is_enabled() {
            return;
        }

        let room_ids =
            self.client.store().load_rooms_with_unsent_requests().await.unwrap_or_else(|err| {
                warn!("error when loading rooms with unsent requests: {err}");
                Vec::new()
            });

        // Getting the [`RoomSendQueue`] is sufficient to spawn the task if needs be.
        for room_id in room_ids {
            if let Some(room) = self.client.get_room(&room_id) {
                let _ = self.for_room(room);
            }
        }
    }

    /// Tiny helper to get the send queue's global context from the [`Client`].
    #[inline(always)]
    fn data(&self) -> &SendQueueData {
        &self.client.inner.send_queue_data
    }

    /// Get or create a new send queue for a given room, and insert it into our
    /// memoized rooms mapping.
    fn for_room(&self, room: Room) -> RoomSendQueue {
        let data = self.data();

        let mut map = data.rooms.write().unwrap();

        let room_id = room.room_id();
        if let Some(room_q) = map.get(room_id).cloned() {
            return room_q;
        }

        let owned_room_id = room_id.to_owned();
        let room_q = RoomSendQueue::new(
            self.is_enabled(),
            data.error_reporter.clone(),
            data.is_dropping.clone(),
            &self.client,
            owned_room_id.clone(),
        );

        map.insert(owned_room_id, room_q.clone());

        room_q
    }

    /// Enable or disable the send queue for the entire client, i.e. all rooms.
    ///
    /// If we're disabling the queue, and requests were being sent, they're not
    /// aborted, and will continue until a status resolves (error responses
    /// will keep the events in the buffer of events to send later). The
    /// disablement will happen before the next request is sent.
    ///
    /// This may wake up background tasks and resume sending of requests in the
    /// background.
    pub async fn set_enabled(&self, enabled: bool) {
        debug!(?enabled, "setting global send queue enablement");

        self.data().globally_enabled.store(enabled, Ordering::SeqCst);

        // Wake up individual rooms we already know about.
        for room in self.data().rooms.read().unwrap().values() {
            room.set_enabled(enabled);
        }

        // Reload some extra rooms that might not have been awaken yet, but could have
        // requests from previous sessions.
        self.respawn_tasks_for_rooms_with_unsent_requests().await;
    }

    /// Returns whether the send queue is enabled, at a client-wide
    /// granularity.
    pub fn is_enabled(&self) -> bool {
        self.data().globally_enabled.load(Ordering::SeqCst)
    }

    /// A subscriber to the enablement status (enabled or disabled) of the
    /// send queue, along with useful errors.
    pub fn subscribe_errors(&self) -> broadcast::Receiver<SendQueueRoomError> {
        self.data().error_reporter.subscribe()
    }
}

/// A specific room's send queue ran into an error, and it has disabled itself.
#[derive(Clone, Debug)]
pub struct SendQueueRoomError {
    /// For which room is the send queue failing?
    pub room_id: OwnedRoomId,

    /// The error the room has ran into, when trying to send a request.
    pub error: Arc<crate::Error>,

    /// Whether the error is considered recoverable or not.
    ///
    /// An error that's recoverable will disable the room's send queue, while an
    /// unrecoverable error will be parked, until the user decides to do
    /// something about it.
    pub is_recoverable: bool,
}

impl Client {
    /// Returns a [`SendQueue`] that handles sending, retrying and not
    /// forgetting about requests that are to be sent.
    pub fn send_queue(&self) -> SendQueue {
        SendQueue::new(self.clone())
    }
}

pub(super) struct SendQueueData {
    /// Mapping of room to their unique send queue.
    rooms: RwLock<BTreeMap<OwnedRoomId, RoomSendQueue>>,

    /// Is the whole mechanism enabled or disabled?
    ///
    /// This is only kept in memory to initialize new room queues with an
    /// initial enablement state.
    globally_enabled: AtomicBool,

    /// Global error updates for the send queue.
    error_reporter: broadcast::Sender<SendQueueRoomError>,

    /// Are we currently dropping the Client?
    is_dropping: Arc<AtomicBool>,
}

impl SendQueueData {
    /// Create the data for a send queue, in the given enabled state.
    pub fn new(globally_enabled: bool) -> Self {
        let (sender, _) = broadcast::channel(32);

        Self {
            rooms: Default::default(),
            globally_enabled: AtomicBool::new(globally_enabled),
            error_reporter: sender,
            is_dropping: Arc::new(false.into()),
        }
    }
}

impl Drop for SendQueueData {
    fn drop(&mut self) {
        // Mark the whole send queue as shutting down, then wake up all the room
        // queues so they're stopped too.
        debug!("globally dropping the send queue");
        self.is_dropping.store(true, Ordering::SeqCst);

        let rooms = self.rooms.read().unwrap();
        for room in rooms.values() {
            room.inner.notifier.notify_one();
        }
    }
}

impl Room {
    /// Returns the [`RoomSendQueue`] for this specific room.
    pub fn send_queue(&self) -> RoomSendQueue {
        self.client.send_queue().for_room(self.clone())
    }
}

/// A per-room send queue.
///
/// This is cheap to clone.
#[derive(Clone)]
pub struct RoomSendQueue {
    inner: Arc<RoomSendQueueInner>,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for RoomSendQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoomSendQueue").finish_non_exhaustive()
    }
}

impl RoomSendQueue {
    fn new(
        globally_enabled: bool,
        global_error_reporter: broadcast::Sender<SendQueueRoomError>,
        is_dropping: Arc<AtomicBool>,
        client: &Client,
        room_id: OwnedRoomId,
    ) -> Self {
        let (updates_sender, _) = broadcast::channel(32);

        let queue = QueueStorage::new(WeakClient::from_client(client), room_id.clone());
        let notifier = Arc::new(Notify::new());

        let weak_room = WeakRoom::new(WeakClient::from_client(client), room_id);
        let locally_enabled = Arc::new(AtomicBool::new(globally_enabled));

        let task = spawn(Self::sending_task(
            weak_room.clone(),
            queue.clone(),
            notifier.clone(),
            updates_sender.clone(),
            locally_enabled.clone(),
            global_error_reporter,
            is_dropping,
        ));

        Self {
            inner: Arc::new(RoomSendQueueInner {
                room: weak_room,
                updates: updates_sender,
                _task: task,
                queue,
                notifier,
                locally_enabled,
            }),
        }
    }

    /// Queues a raw event for sending it to this room.
    ///
    /// This immediately returns, and will push the event to be sent into a
    /// queue, handled in the background.
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
    pub async fn send_raw(
        &self,
        content: Raw<AnyMessageLikeEventContent>,
        event_type: String,
    ) -> Result<SendHandle, RoomSendQueueError> {
        let Some(room) = self.inner.room.get() else {
            return Err(RoomSendQueueError::RoomDisappeared);
        };
        if room.state() != RoomState::Joined {
            return Err(RoomSendQueueError::RoomNotJoined);
        }

        let content = SerializableEventContent::from_raw(content, event_type);

        let transaction_id = self.inner.queue.push(content.clone().into()).await?;
        trace!(%transaction_id, "manager sends a raw event to the background task");

        self.inner.notifier.notify_one();

        let send_handle = SendHandle {
            room: self.clone(),
            transaction_id: transaction_id.clone(),
            media_handles: None,
        };

        let _ = self.inner.updates.send(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
            transaction_id,
            content: LocalEchoContent::Event {
                serialized_event: content,
                send_handle: send_handle.clone(),
                send_error: None,
            },
        }));

        Ok(send_handle)
    }

    /// Queues an event for sending it to this room.
    ///
    /// This immediately returns, and will push the event to be sent into a
    /// queue, handled in the background.
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
    pub async fn send(
        &self,
        content: AnyMessageLikeEventContent,
    ) -> Result<SendHandle, RoomSendQueueError> {
        self.send_raw(
            Raw::new(&content).map_err(RoomSendQueueStorageError::JsonSerialization)?,
            content.event_type().to_string(),
        )
        .await
    }

    /// Returns the current local requests as well as a receiver to listen to
    /// the send queue updates, as defined in [`RoomSendQueueUpdate`].
    pub async fn subscribe(
        &self,
    ) -> Result<(Vec<LocalEcho>, broadcast::Receiver<RoomSendQueueUpdate>), RoomSendQueueError>
    {
        let local_echoes = self.inner.queue.local_echoes(self).await?;

        Ok((local_echoes, self.inner.updates.subscribe()))
    }

    /// A task that must be spawned in the async runtime, running in the
    /// background for each room that has a send queue.
    ///
    /// It only progresses forward: nothing can be cancelled at any point, which
    /// makes the implementation not overly complicated to follow.
    #[instrument(skip_all, fields(room_id = %room.room_id()))]
    async fn sending_task(
        room: WeakRoom,
        queue: QueueStorage,
        notifier: Arc<Notify>,
        updates: broadcast::Sender<RoomSendQueueUpdate>,
        locally_enabled: Arc<AtomicBool>,
        global_error_reporter: broadcast::Sender<SendQueueRoomError>,
        is_dropping: Arc<AtomicBool>,
    ) {
        info!("spawned the sending task");

        loop {
            // A request to shut down should be preferred above everything else.
            if is_dropping.load(Ordering::SeqCst) {
                trace!("shutting down!");
                break;
            }

            // Try to apply dependent requests now; those applying to previously failed
            // attempts (local echoes) would succeed now.
            let mut new_updates = Vec::new();
            if let Err(err) = queue.apply_dependent_requests(&mut new_updates).await {
                warn!("errors when applying dependent requests: {err}");
            }

            for up in new_updates {
                let _ = updates.send(up);
            }

            if !locally_enabled.load(Ordering::SeqCst) {
                trace!("not enabled, sleeping");
                // Wait for an explicit wakeup.
                notifier.notified().await;
                continue;
            }

            let (queued_request, cancel_upload_rx) = match queue.peek_next_to_send().await {
                Ok(Some(request)) => request,

                Ok(None) => {
                    trace!("queue is empty, sleeping");
                    // Wait for an explicit wakeup.
                    notifier.notified().await;
                    continue;
                }

                Err(err) => {
                    warn!("error when loading next request to send: {err}");
                    continue;
                }
            };

            let txn_id = queued_request.transaction_id.clone();
            trace!(txn_id = %txn_id, "received a request to send!");

            let related_txn_id = as_variant!(&queued_request.kind, QueuedRequestKind::MediaUpload { related_to, .. } => related_to.clone());

            let Some(room) = room.get() else {
                if is_dropping.load(Ordering::SeqCst) {
                    break;
                }
                error!("the weak room couldn't be upgraded but we're not shutting down?");
                continue;
            };

            match Self::handle_request(&room, queued_request, cancel_upload_rx).await {
                Ok(Some(parent_key)) => match queue.mark_as_sent(&txn_id, parent_key.clone()).await
                {
                    Ok(()) => match parent_key {
                        SentRequestKey::Event(event_id) => {
                            let _ = updates.send(RoomSendQueueUpdate::SentEvent {
                                transaction_id: txn_id,
                                event_id,
                            });
                        }

                        SentRequestKey::Media(media_info) => {
                            let _ = updates.send(RoomSendQueueUpdate::UploadedMedia {
                                related_to: related_txn_id.as_ref().unwrap_or(&txn_id).clone(),
                                file: media_info.file,
                            });
                        }
                    },

                    Err(err) => {
                        warn!("unable to mark queued request as sent: {err}");
                    }
                },

                Ok(None) => {
                    debug!("Request has been aborted while running, continuing.");
                }

                Err(err) => {
                    let is_recoverable = match err {
                        crate::Error::Http(ref http_err) => {
                            // All transient errors are recoverable.
                            matches!(
                                http_err.retry_kind(),
                                RetryKind::Transient { .. } | RetryKind::NetworkFailure
                            )
                        }

                        // `ConcurrentRequestFailed` typically happens because of an HTTP failure;
                        // since we don't get the underlying error, be lax and consider it
                        // recoverable, and let observers decide to retry it or not. At some point
                        // we'll get the actual underlying error.
                        crate::Error::ConcurrentRequestFailed => true,

                        // As of 2024-06-27, all other error types are considered unrecoverable.
                        _ => false,
                    };

                    if is_recoverable {
                        warn!(txn_id = %txn_id, error = ?err, "Recoverable error when sending request: {err}, disabling send queue");

                        // In this case, we intentionally keep the request in the queue, but mark it
                        // as not being sent anymore.
                        queue.mark_as_not_being_sent(&txn_id).await;

                        // Let observers know about a failure *after* we've marked the item as not
                        // being sent anymore. Otherwise, there's a possible race where a caller
                        // might try to remove an item, while it's still marked as being sent,
                        // resulting in a cancellation failure.

                        // Disable the queue for this room after a recoverable error happened. This
                        // should be the sign that this error is temporary (maybe network
                        // disconnected, maybe the server had a hiccup).
                        locally_enabled.store(false, Ordering::SeqCst);
                    } else {
                        warn!(txn_id = %txn_id, error = ?err, "Unrecoverable error when sending request: {err}");

                        // Mark the request as wedged, so it's not picked at any future point.
                        if let Err(storage_error) =
                            queue.mark_as_wedged(&txn_id, QueueWedgeError::from(&err)).await
                        {
                            warn!("unable to mark request as wedged: {storage_error}");
                        }
                    }

                    let error = Arc::new(err);

                    let _ = global_error_reporter.send(SendQueueRoomError {
                        room_id: room.room_id().to_owned(),
                        error: error.clone(),
                        is_recoverable,
                    });

                    let _ = updates.send(RoomSendQueueUpdate::SendError {
                        transaction_id: related_txn_id.unwrap_or(txn_id),
                        error,
                        is_recoverable,
                    });
                }
            }
        }

        info!("exited sending task");
    }

    /// Handles a single request and returns the [`SentRequestKey`] on success
    /// (unless the request was cancelled, in which case it'll return
    /// `None`).
    async fn handle_request(
        room: &Room,
        request: QueuedRequest,
        cancel_upload_rx: Option<oneshot::Receiver<()>>,
    ) -> Result<Option<SentRequestKey>, crate::Error> {
        match request.kind {
            QueuedRequestKind::Event { content } => {
                let (event, event_type) = content.raw();

                let res = room
                    .send_raw(event_type, event)
                    .with_transaction_id(&request.transaction_id)
                    .with_request_config(RequestConfig::short_retry())
                    .await?;

                trace!(txn_id = %request.transaction_id, event_id = %res.event_id, "event successfully sent");
                Ok(Some(SentRequestKey::Event(res.event_id)))
            }

            QueuedRequestKind::MediaUpload {
                content_type,
                cache_key,
                thumbnail_source,
                related_to: relates_to,
            } => {
                trace!(%relates_to, "uploading media related to event");

                let fut = async move {
                    let mime = Mime::from_str(&content_type).map_err(|_| {
                        crate::Error::SendQueueWedgeError(QueueWedgeError::InvalidMimeType {
                            mime_type: content_type.clone(),
                        })
                    })?;

                    let data = room
                        .client()
                        .event_cache_store()
                        .lock()
                        .await?
                        .get_media_content(&cache_key)
                        .await?
                        .ok_or(crate::Error::SendQueueWedgeError(
                            QueueWedgeError::MissingMediaContent,
                        ))?;

                    #[cfg(feature = "e2e-encryption")]
                    let media_source = if room.is_encrypted().await? {
                        trace!("upload will be encrypted (encrypted room)");
                        let mut cursor = std::io::Cursor::new(data);
                        let encrypted_file = room
                            .client()
                            .upload_encrypted_file(&mime, &mut cursor)
                            .with_request_config(RequestConfig::short_retry())
                            .await?;
                        MediaSource::Encrypted(Box::new(encrypted_file))
                    } else {
                        trace!("upload will be in clear text (room without encryption)");
                        let request_config = RequestConfig::short_retry()
                            .timeout(Media::reasonable_upload_timeout(&data));
                        let res =
                            room.client().media().upload(&mime, data, Some(request_config)).await?;
                        MediaSource::Plain(res.content_uri)
                    };

                    #[cfg(not(feature = "e2e-encryption"))]
                    let media_source = {
                        let request_config = RequestConfig::short_retry()
                            .timeout(Media::reasonable_upload_timeout(&data));
                        let res =
                            room.client().media().upload(&mime, data, Some(request_config)).await?;
                        MediaSource::Plain(res.content_uri)
                    };

                    let uri = match &media_source {
                        MediaSource::Plain(uri) => uri,
                        MediaSource::Encrypted(encrypted_file) => &encrypted_file.url,
                    };
                    trace!(%relates_to, mxc_uri = %uri, "media successfully uploaded");

                    Ok(SentRequestKey::Media(SentMediaInfo {
                        file: media_source,
                        thumbnail: thumbnail_source,
                    }))
                };

                let wait_for_cancel = async move {
                    if let Some(rx) = cancel_upload_rx {
                        rx.await
                    } else {
                        std::future::pending().await
                    }
                };

                tokio::select! {
                    biased;

                    _ = wait_for_cancel => {
                        Ok(None)
                    }

                    res = fut => {
                        res.map(Some)
                    }
                }
            }
        }
    }

    /// Returns whether the room is enabled, at the room level.
    pub fn is_enabled(&self) -> bool {
        self.inner.locally_enabled.load(Ordering::SeqCst)
    }

    /// Set the locally enabled flag for this room queue.
    pub fn set_enabled(&self, enabled: bool) {
        self.inner.locally_enabled.store(enabled, Ordering::SeqCst);

        // No need to wake a task to tell it it's been disabled, so only notify if we're
        // re-enabling the queue.
        if enabled {
            self.inner.notifier.notify_one();
        }
    }
}

impl From<&crate::Error> for QueueWedgeError {
    fn from(value: &crate::Error) -> Self {
        match value {
            #[cfg(feature = "e2e-encryption")]
            crate::Error::OlmError(OlmError::SessionRecipientCollectionError(error)) => match error
            {
                SessionRecipientCollectionError::VerifiedUserHasUnsignedDevice(user_map) => {
                    QueueWedgeError::InsecureDevices { user_device_map: user_map.clone() }
                }

                SessionRecipientCollectionError::VerifiedUserChangedIdentity(users) => {
                    QueueWedgeError::IdentityViolations { users: users.clone() }
                }

                SessionRecipientCollectionError::CrossSigningNotSetup
                | SessionRecipientCollectionError::SendingFromUnverifiedDevice => {
                    QueueWedgeError::CrossVerificationRequired
                }
            },

            // Flatten errors of `Self` type.
            crate::Error::SendQueueWedgeError(error) => error.clone(),

            _ => QueueWedgeError::GenericApiError { msg: value.to_string() },
        }
    }
}

struct RoomSendQueueInner {
    /// The room which this send queue relates to.
    room: WeakRoom,

    /// Broadcaster for notifications about the statuses of requests to be sent.
    ///
    /// Can be subscribed to from the outside.
    updates: broadcast::Sender<RoomSendQueueUpdate>,

    /// Queue of requests that are either to be sent, or being sent.
    ///
    /// When a request has been sent to the server, it is removed from that
    /// queue *after* being sent. That way, we will retry sending upon
    /// failure, in the same order requests have been inserted in the first
    /// place.
    queue: QueueStorage,

    /// A notifier that's updated any time common data is touched (stopped or
    /// enabled statuses), or the associated room [`QueueStorage`].
    notifier: Arc<Notify>,

    /// Should the room process new requests or not (because e.g. it might be
    /// running off the network)?
    locally_enabled: Arc<AtomicBool>,

    /// Handle to the actual sending task. Unused, but kept alive along this
    /// data structure.
    _task: JoinHandle<()>,
}

/// Information about a request being sent right this moment.
struct BeingSentInfo {
    /// Transaction id of the thing being sent.
    transaction_id: OwnedTransactionId,

    /// For an upload request, a trigger to cancel the upload before it
    /// completes.
    cancel_upload: Option<oneshot::Sender<()>>,
}

impl BeingSentInfo {
    /// Aborts the upload, if a trigger is available.
    ///
    /// Consumes the object because the sender is a oneshot and will be consumed
    /// upon sending.
    fn cancel_upload(self) -> bool {
        if let Some(cancel_upload) = self.cancel_upload {
            let _ = cancel_upload.send(());
            true
        } else {
            false
        }
    }
}

/// A specialized lock that guards both against the state store and the
/// [`Self::being_sent`] data.
#[derive(Clone)]
struct StoreLock {
    /// Reference to the client, to get access to the underlying store.
    client: WeakClient,

    /// The one queued request that is being sent at the moment, along with
    /// associated data that can be useful to act upon it.
    ///
    /// Also used as the lock to access the state store.
    being_sent: Arc<Mutex<Option<BeingSentInfo>>>,
}

impl StoreLock {
    /// Gets a hold of the locked store and [`Self::being_sent`] pair.
    async fn lock(&self) -> StoreLockGuard {
        StoreLockGuard {
            client: self.client.clone(),
            being_sent: self.being_sent.clone().lock_owned().await,
        }
    }
}

/// A lock guard obtained through locking with [`StoreLock`].
/// `being_sent` data.
struct StoreLockGuard {
    /// Reference to the client, to get access to the underlying store.
    client: WeakClient,

    /// The one queued request that is being sent at the moment, along with
    /// associated data that can be useful to act upon it.
    being_sent: OwnedMutexGuard<Option<BeingSentInfo>>,
}

impl StoreLockGuard {
    /// Get a client from the locked state, useful to get a handle on a store.
    fn client(&self) -> Result<Client, RoomSendQueueStorageError> {
        self.client.get().ok_or(RoomSendQueueStorageError::ClientShuttingDown)
    }
}

#[derive(Clone)]
struct QueueStorage {
    /// A lock to make sure the state store is only accessed once at a time, to
    /// make some store operations atomic.
    store: StoreLock,

    /// To which room is this storage related.
    room_id: OwnedRoomId,
}

impl QueueStorage {
    /// Default priority for a queued request.
    const LOW_PRIORITY: usize = 0;

    /// High priority for a queued request that must be handled before others.
    const HIGH_PRIORITY: usize = 10;

    /// Create a new queue for queuing requests to be sent later.
    fn new(client: WeakClient, room: OwnedRoomId) -> Self {
        Self { room_id: room, store: StoreLock { client, being_sent: Default::default() } }
    }

    /// Push a new event to be sent in the queue, with a default priority of 0.
    ///
    /// Returns the transaction id chosen to identify the request.
    async fn push(
        &self,
        request: QueuedRequestKind,
    ) -> Result<OwnedTransactionId, RoomSendQueueStorageError> {
        let transaction_id = TransactionId::new();

        self.store
            .lock()
            .await
            .client()?
            .store()
            .save_send_queue_request(
                &self.room_id,
                transaction_id.clone(),
                request,
                Self::LOW_PRIORITY,
            )
            .await?;

        Ok(transaction_id)
    }

    /// Peeks the next request to be sent, marking it as being sent.
    ///
    /// It is required to call [`Self::mark_as_sent`] after it's been
    /// effectively sent.
    async fn peek_next_to_send(
        &self,
    ) -> Result<Option<(QueuedRequest, Option<oneshot::Receiver<()>>)>, RoomSendQueueStorageError>
    {
        let mut guard = self.store.lock().await;
        let queued_requests =
            guard.client()?.store().load_send_queue_requests(&self.room_id).await?;

        if let Some(request) = queued_requests.iter().find(|queued| !queued.is_wedged()) {
            let (cancel_upload_tx, cancel_upload_rx) =
                if matches!(request.kind, QueuedRequestKind::MediaUpload { .. }) {
                    let (tx, rx) = oneshot::channel();
                    (Some(tx), Some(rx))
                } else {
                    Default::default()
                };

            let prev = guard.being_sent.replace(BeingSentInfo {
                transaction_id: request.transaction_id.clone(),
                cancel_upload: cancel_upload_tx,
            });

            if let Some(prev) = prev {
                error!(
                    prev_txn = ?prev.transaction_id,
                    "a previous request was still active while picking a new one"
                );
            }

            Ok(Some((request.clone(), cancel_upload_rx)))
        } else {
            Ok(None)
        }
    }

    /// Marks a request popped with [`Self::peek_next_to_send`] and identified
    /// with the given transaction id as not being sent anymore, so it can
    /// be removed from the queue later.
    async fn mark_as_not_being_sent(&self, transaction_id: &TransactionId) {
        let was_being_sent = self.store.lock().await.being_sent.take();

        let prev_txn = was_being_sent.as_ref().map(|info| info.transaction_id.as_ref());
        if prev_txn != Some(transaction_id) {
            error!(prev_txn = ?prev_txn, "previous active request didn't match that we expect (after transient error)");
        }
    }

    /// Marks a request popped with [`Self::peek_next_to_send`] and identified
    /// with the given transaction id as being wedged (and not being sent
    /// anymore), so it can be removed from the queue later.
    async fn mark_as_wedged(
        &self,
        transaction_id: &TransactionId,
        reason: QueueWedgeError,
    ) -> Result<(), RoomSendQueueStorageError> {
        // Keep the lock until we're done touching the storage.
        let mut guard = self.store.lock().await;
        let was_being_sent = guard.being_sent.take();

        let prev_txn = was_being_sent.as_ref().map(|info| info.transaction_id.as_ref());
        if prev_txn != Some(transaction_id) {
            error!(prev_txn = ?prev_txn, "previous active request didn't match that we expect (after permanent error)");
        }

        Ok(guard
            .client()?
            .store()
            .update_send_queue_request_status(&self.room_id, transaction_id, Some(reason))
            .await?)
    }

    /// Marks a request identified with the given transaction id as being now
    /// unwedged and adds it back to the queue.
    async fn mark_as_unwedged(
        &self,
        transaction_id: &TransactionId,
    ) -> Result<(), RoomSendQueueStorageError> {
        Ok(self
            .store
            .lock()
            .await
            .client()?
            .store()
            .update_send_queue_request_status(&self.room_id, transaction_id, None)
            .await?)
    }

    /// Marks a request pushed with [`Self::push`] and identified with the given
    /// transaction id as sent, by removing it from the local queue.
    async fn mark_as_sent(
        &self,
        transaction_id: &TransactionId,
        parent_key: SentRequestKey,
    ) -> Result<(), RoomSendQueueStorageError> {
        // Keep the lock until we're done touching the storage.
        let mut guard = self.store.lock().await;
        let was_being_sent = guard.being_sent.take();

        let prev_txn = was_being_sent.as_ref().map(|info| info.transaction_id.as_ref());
        if prev_txn != Some(transaction_id) {
            error!(prev_txn = ?prev_txn, "previous active request didn't match that we expect (after successful send");
        }

        let client = guard.client()?;
        let store = client.store();

        // Update all dependent requests.
        store
            .mark_dependent_queued_requests_as_ready(&self.room_id, transaction_id, parent_key)
            .await?;

        let removed = store.remove_send_queue_request(&self.room_id, transaction_id).await?;

        if !removed {
            warn!(txn_id = %transaction_id, "request marked as sent was missing from storage");
        }

        Ok(())
    }

    /// Cancel a sending command for an event that has been sent with
    /// [`Self::push`] with the given transaction id.
    ///
    /// Returns whether the given transaction has been effectively removed. If
    /// false, this either means that the transaction id was unrelated to
    /// this queue, or that the request was sent before we cancelled it.
    async fn cancel_event(
        &self,
        transaction_id: &TransactionId,
    ) -> Result<bool, RoomSendQueueStorageError> {
        let guard = self.store.lock().await;

        if guard.being_sent.as_ref().map(|info| info.transaction_id.as_ref())
            == Some(transaction_id)
        {
            // Save the intent to redact the event.
            guard
                .client()?
                .store()
                .save_dependent_queued_request(
                    &self.room_id,
                    transaction_id,
                    ChildTransactionId::new(),
                    DependentQueuedRequestKind::RedactEvent,
                )
                .await?;

            return Ok(true);
        }

        let removed = guard
            .client()?
            .store()
            .remove_send_queue_request(&self.room_id, transaction_id)
            .await?;

        Ok(removed)
    }

    /// Replace an event that has been sent with [`Self::push`] with the given
    /// transaction id, before it's been actually sent.
    ///
    /// Returns whether the given transaction has been effectively edited. If
    /// false, this either means that the transaction id was unrelated to
    /// this queue, or that the request was sent before we edited it.
    async fn replace_event(
        &self,
        transaction_id: &TransactionId,
        serializable: SerializableEventContent,
    ) -> Result<bool, RoomSendQueueStorageError> {
        let guard = self.store.lock().await;

        if guard.being_sent.as_ref().map(|info| info.transaction_id.as_ref())
            == Some(transaction_id)
        {
            // Save the intent to edit the associated event.
            guard
                .client()?
                .store()
                .save_dependent_queued_request(
                    &self.room_id,
                    transaction_id,
                    ChildTransactionId::new(),
                    DependentQueuedRequestKind::EditEvent { new_content: serializable },
                )
                .await?;

            return Ok(true);
        }

        let edited = guard
            .client()?
            .store()
            .update_send_queue_request(&self.room_id, transaction_id, serializable.into())
            .await?;

        Ok(edited)
    }

    /// Push requests (and dependents) to upload a media.
    ///
    /// See the module-level description for details of the whole processus.
    async fn push_media(
        &self,
        event: RoomMessageEventContent,
        content_type: Mime,
        send_event_txn: OwnedTransactionId,
        upload_file_txn: OwnedTransactionId,
        file_media_request: MediaRequestParameters,
        thumbnail: Option<(FinishUploadThumbnailInfo, MediaRequestParameters, Mime)>,
    ) -> Result<(), RoomSendQueueStorageError> {
        let guard = self.store.lock().await;
        let client = guard.client()?;
        let store = client.store();

        let thumbnail_info =
            if let Some((thumbnail_info, thumbnail_media_request, thumbnail_content_type)) =
                thumbnail
            {
                let upload_thumbnail_txn = thumbnail_info.txn.clone();

                // Save the thumbnail upload request.
                store
                    .save_send_queue_request(
                        &self.room_id,
                        upload_thumbnail_txn.clone(),
                        QueuedRequestKind::MediaUpload {
                            content_type: thumbnail_content_type.to_string(),
                            cache_key: thumbnail_media_request,
                            thumbnail_source: None, // the thumbnail has no thumbnails :)
                            related_to: send_event_txn.clone(),
                        },
                        Self::LOW_PRIORITY,
                    )
                    .await?;

                // Save the file upload request as a dependent request of the thumbnail upload.
                store
                    .save_dependent_queued_request(
                        &self.room_id,
                        &upload_thumbnail_txn,
                        upload_file_txn.clone().into(),
                        DependentQueuedRequestKind::UploadFileWithThumbnail {
                            content_type: content_type.to_string(),
                            cache_key: file_media_request,
                            related_to: send_event_txn.clone(),
                        },
                    )
                    .await?;

                Some(thumbnail_info)
            } else {
                // Save the file upload as its own request, not a dependent one.
                store
                    .save_send_queue_request(
                        &self.room_id,
                        upload_file_txn.clone(),
                        QueuedRequestKind::MediaUpload {
                            content_type: content_type.to_string(),
                            cache_key: file_media_request,
                            thumbnail_source: None,
                            related_to: send_event_txn.clone(),
                        },
                        Self::LOW_PRIORITY,
                    )
                    .await?;

                None
            };

        // Push the dependent request for the event itself.
        store
            .save_dependent_queued_request(
                &self.room_id,
                &upload_file_txn,
                send_event_txn.into(),
                DependentQueuedRequestKind::FinishUpload {
                    local_echo: event,
                    file_upload: upload_file_txn.clone(),
                    thumbnail_info,
                },
            )
            .await?;

        Ok(())
    }

    /// Reacts to the given local echo of an event.
    #[instrument(skip(self))]
    async fn react(
        &self,
        transaction_id: &TransactionId,
        key: String,
    ) -> Result<Option<ChildTransactionId>, RoomSendQueueStorageError> {
        let guard = self.store.lock().await;
        let client = guard.client()?;
        let store = client.store();

        let requests = store.load_send_queue_requests(&self.room_id).await?;

        // If the target event has been already sent, abort immediately.
        if !requests.iter().any(|item| item.transaction_id == transaction_id) {
            // We didn't find it as a queued request; try to find it as a dependent queued
            // request.
            let dependent_requests = store.load_dependent_queued_requests(&self.room_id).await?;
            if !dependent_requests
                .into_iter()
                .filter_map(|item| item.is_own_event().then_some(item.own_transaction_id))
                .any(|child_txn| *child_txn == *transaction_id)
            {
                // We didn't find it as either a request or a dependent request, abort.
                return Ok(None);
            }
        }

        // Record the dependent request.
        let reaction_txn_id = ChildTransactionId::new();
        store
            .save_dependent_queued_request(
                &self.room_id,
                transaction_id,
                reaction_txn_id.clone(),
                DependentQueuedRequestKind::ReactEvent { key },
            )
            .await?;

        Ok(Some(reaction_txn_id))
    }

    /// Returns a list of the local echoes, that is, all the requests that we're
    /// about to send but that haven't been sent yet (or are being sent).
    async fn local_echoes(
        &self,
        room: &RoomSendQueue,
    ) -> Result<Vec<LocalEcho>, RoomSendQueueStorageError> {
        let guard = self.store.lock().await;
        let client = guard.client()?;
        let store = client.store();

        let local_requests =
            store.load_send_queue_requests(&self.room_id).await?.into_iter().filter_map(|queued| {
                Some(LocalEcho {
                    transaction_id: queued.transaction_id.clone(),
                    content: match queued.kind {
                        QueuedRequestKind::Event { content } => LocalEchoContent::Event {
                            serialized_event: content,
                            send_handle: SendHandle {
                                room: room.clone(),
                                transaction_id: queued.transaction_id,
                                media_handles: None,
                            },
                            send_error: queued.error,
                        },

                        QueuedRequestKind::MediaUpload { .. } => {
                            // Don't return uploaded medias as their own things; the accompanying
                            // event represented as a dependent request should be sufficient.
                            return None;
                        }
                    },
                })
            });

        let reactions_and_medias = store
            .load_dependent_queued_requests(&self.room_id)
            .await?
            .into_iter()
            .filter_map(|dep| match dep.kind {
                DependentQueuedRequestKind::EditEvent { .. }
                | DependentQueuedRequestKind::RedactEvent => {
                    // TODO: reflect local edits/redacts too?
                    None
                }

                DependentQueuedRequestKind::ReactEvent { key } => Some(LocalEcho {
                    transaction_id: dep.own_transaction_id.clone().into(),
                    content: LocalEchoContent::React {
                        key,
                        send_handle: SendReactionHandle {
                            room: room.clone(),
                            transaction_id: dep.own_transaction_id,
                        },
                        applies_to: dep.parent_transaction_id,
                    },
                }),

                DependentQueuedRequestKind::UploadFileWithThumbnail { .. } => {
                    // Don't reflect these: only the associated event is interesting to observers.
                    None
                }

                DependentQueuedRequestKind::FinishUpload {
                    local_echo,
                    file_upload,
                    thumbnail_info,
                } => {
                    // Materialize as an event local echo.
                    Some(LocalEcho {
                        transaction_id: dep.own_transaction_id.clone().into(),
                        content: LocalEchoContent::Event {
                            serialized_event: SerializableEventContent::new(&local_echo.into())
                                .ok()?,
                            send_handle: SendHandle {
                                room: room.clone(),
                                transaction_id: dep.own_transaction_id.into(),
                                media_handles: Some(MediaHandles {
                                    upload_thumbnail_txn: thumbnail_info.map(|info| info.txn),
                                    upload_file_txn: file_upload,
                                }),
                            },
                            send_error: None,
                        },
                    })
                }
            });

        Ok(local_requests.chain(reactions_and_medias).collect())
    }

    /// Try to apply a single dependent request, whether it's local or remote.
    ///
    /// This swallows errors that would retrigger every time if we retried
    /// applying the dependent request: invalid edit content, etc.
    ///
    /// Returns true if the dependent request has been sent (or should not be
    /// retried later).
    #[instrument(skip_all)]
    async fn try_apply_single_dependent_request(
        &self,
        client: &Client,
        dependent_request: DependentQueuedRequest,
        new_updates: &mut Vec<RoomSendQueueUpdate>,
    ) -> Result<bool, RoomSendQueueError> {
        let store = client.store();

        let parent_key = dependent_request.parent_key;

        match dependent_request.kind {
            DependentQueuedRequestKind::EditEvent { new_content } => {
                if let Some(parent_key) = parent_key {
                    let Some(event_id) = parent_key.into_event_id() else {
                        return Err(RoomSendQueueError::StorageError(
                            RoomSendQueueStorageError::InvalidParentKey,
                        ));
                    };

                    // The parent event has been sent, so send an edit event.
                    let room = client
                        .get_room(&self.room_id)
                        .ok_or(RoomSendQueueError::RoomDisappeared)?;

                    // Check the event is one we know how to edit with an edit event.

                    // It must be deserializable…
                    let edited_content = match new_content.deserialize() {
                        Ok(AnyMessageLikeEventContent::RoomMessage(c)) => {
                            // Assume no relationships.
                            EditedContent::RoomMessage(c.into())
                        }

                        Ok(AnyMessageLikeEventContent::UnstablePollStart(c)) => {
                            let poll_start = c.poll_start().clone();
                            EditedContent::PollStart {
                                fallback_text: poll_start.question.text.clone(),
                                new_content: poll_start,
                            }
                        }

                        Ok(c) => {
                            warn!("Unsupported edit content type: {:?}", c.event_type());
                            return Ok(true);
                        }

                        Err(err) => {
                            warn!("Unable to deserialize: {err}");
                            return Ok(true);
                        }
                    };

                    let edit_event = match room.make_edit_event(&event_id, edited_content).await {
                        Ok(e) => e,
                        Err(err) => {
                            warn!("couldn't create edited event: {err}");
                            return Ok(true);
                        }
                    };

                    // Queue the edit event in the send queue 🧠.
                    let serializable = SerializableEventContent::from_raw(
                        Raw::new(&edit_event)
                            .map_err(RoomSendQueueStorageError::JsonSerialization)?,
                        edit_event.event_type().to_string(),
                    );

                    store
                        .save_send_queue_request(
                            &self.room_id,
                            dependent_request.own_transaction_id.into(),
                            serializable.into(),
                            Self::HIGH_PRIORITY,
                        )
                        .await
                        .map_err(RoomSendQueueStorageError::StateStoreError)?;
                } else {
                    // The parent event is still local; update the local echo.
                    let edited = store
                        .update_send_queue_request(
                            &self.room_id,
                            &dependent_request.parent_transaction_id,
                            new_content.into(),
                        )
                        .await
                        .map_err(RoomSendQueueStorageError::StateStoreError)?;

                    if !edited {
                        warn!("missing local echo upon dependent edit");
                    }
                }
            }

            DependentQueuedRequestKind::RedactEvent => {
                if let Some(parent_key) = parent_key {
                    let Some(event_id) = parent_key.into_event_id() else {
                        return Err(RoomSendQueueError::StorageError(
                            RoomSendQueueStorageError::InvalidParentKey,
                        ));
                    };

                    // The parent event has been sent; send a redaction.
                    let room = client
                        .get_room(&self.room_id)
                        .ok_or(RoomSendQueueError::RoomDisappeared)?;

                    // Ideally we'd use the send queue to send the redaction, but the protocol has
                    // changed the shape of a room.redaction after v11, so keep it simple and try
                    // once here.

                    // Note: no reason is provided because we materialize the intent of "cancel
                    // sending the parent event".

                    if let Err(err) = room
                        .redact(&event_id, None, Some(dependent_request.own_transaction_id.into()))
                        .await
                    {
                        warn!("error when sending a redact for {event_id}: {err}");
                        return Ok(false);
                    }
                } else {
                    // The parent event is still local (sending must have failed); redact the local
                    // echo.
                    let removed = store
                        .remove_send_queue_request(
                            &self.room_id,
                            &dependent_request.parent_transaction_id,
                        )
                        .await
                        .map_err(RoomSendQueueStorageError::StateStoreError)?;

                    if !removed {
                        warn!("missing local echo upon dependent redact");
                    }
                }
            }

            DependentQueuedRequestKind::ReactEvent { key } => {
                if let Some(parent_key) = parent_key {
                    let Some(parent_event_id) = parent_key.into_event_id() else {
                        return Err(RoomSendQueueError::StorageError(
                            RoomSendQueueStorageError::InvalidParentKey,
                        ));
                    };

                    // Queue the reaction event in the send queue 🧠.
                    let react_event =
                        ReactionEventContent::new(Annotation::new(parent_event_id, key)).into();
                    let serializable = SerializableEventContent::from_raw(
                        Raw::new(&react_event)
                            .map_err(RoomSendQueueStorageError::JsonSerialization)?,
                        react_event.event_type().to_string(),
                    );

                    store
                        .save_send_queue_request(
                            &self.room_id,
                            dependent_request.own_transaction_id.into(),
                            serializable.into(),
                            Self::HIGH_PRIORITY,
                        )
                        .await
                        .map_err(RoomSendQueueStorageError::StateStoreError)?;
                } else {
                    // Not applied yet, we should retry later => false.
                    return Ok(false);
                }
            }

            DependentQueuedRequestKind::UploadFileWithThumbnail {
                content_type,
                cache_key,
                related_to,
            } => {
                let Some(parent_key) = parent_key else {
                    // Not finished yet, we should retry later => false.
                    return Ok(false);
                };
                self.handle_dependent_file_upload_with_thumbnail(
                    client,
                    dependent_request.own_transaction_id.into(),
                    parent_key,
                    content_type,
                    cache_key,
                    related_to,
                )
                .await?;
            }

            DependentQueuedRequestKind::FinishUpload {
                local_echo,
                file_upload,
                thumbnail_info,
            } => {
                let Some(parent_key) = parent_key else {
                    // Not finished yet, we should retry later => false.
                    return Ok(false);
                };
                self.handle_dependent_finish_upload(
                    client,
                    dependent_request.own_transaction_id.into(),
                    parent_key,
                    local_echo,
                    file_upload,
                    thumbnail_info,
                    new_updates,
                )
                .await?;
            }
        }

        Ok(true)
    }

    #[instrument(skip(self))]
    async fn apply_dependent_requests(
        &self,
        new_updates: &mut Vec<RoomSendQueueUpdate>,
    ) -> Result<(), RoomSendQueueError> {
        let guard = self.store.lock().await;

        let client = guard.client()?;
        let store = client.store();

        let dependent_requests = store
            .load_dependent_queued_requests(&self.room_id)
            .await
            .map_err(RoomSendQueueStorageError::StateStoreError)?;

        let num_initial_dependent_requests = dependent_requests.len();
        if num_initial_dependent_requests == 0 {
            // Returning early here avoids a bit of useless logging.
            return Ok(());
        }

        let canonicalized_dependent_requests = canonicalize_dependent_requests(&dependent_requests);

        // Get rid of the all non-canonical dependent events.
        for original in &dependent_requests {
            if !canonicalized_dependent_requests
                .iter()
                .any(|canonical| canonical.own_transaction_id == original.own_transaction_id)
            {
                store
                    .remove_dependent_queued_request(&self.room_id, &original.own_transaction_id)
                    .await
                    .map_err(RoomSendQueueStorageError::StateStoreError)?;
            }
        }

        let mut num_dependent_requests = canonicalized_dependent_requests.len();

        debug!(
            num_dependent_requests,
            num_initial_dependent_requests, "starting handling of dependent requests"
        );

        for dependent in canonicalized_dependent_requests {
            let dependent_id = dependent.own_transaction_id.clone();

            match self.try_apply_single_dependent_request(&client, dependent, new_updates).await {
                Ok(should_remove) => {
                    if should_remove {
                        // The dependent request has been successfully applied, forget about it.
                        store
                            .remove_dependent_queued_request(&self.room_id, &dependent_id)
                            .await
                            .map_err(RoomSendQueueStorageError::StateStoreError)?;

                        num_dependent_requests -= 1;
                    }
                }

                Err(err) => {
                    warn!("error when applying single dependent request: {err}");
                }
            }
        }

        debug!(
            leftover_dependent_requests = num_dependent_requests,
            "stopped handling dependent request"
        );

        Ok(())
    }

    /// Remove a single dependent request from storage.
    async fn remove_dependent_send_queue_request(
        &self,
        dependent_event_id: &ChildTransactionId,
    ) -> Result<bool, RoomSendQueueStorageError> {
        Ok(self
            .store
            .lock()
            .await
            .client()?
            .store()
            .remove_dependent_queued_request(&self.room_id, dependent_event_id)
            .await?)
    }
}

/// The content of a local echo.
#[derive(Clone, Debug)]
pub enum LocalEchoContent {
    /// The local echo contains an actual event ready to display.
    Event {
        /// Content of the event itself (along with its type) that we are about
        /// to send.
        serialized_event: SerializableEventContent,
        /// A handle to manipulate the sending of the associated event.
        send_handle: SendHandle,
        /// Whether trying to send this local echo failed in the past with an
        /// unrecoverable error (see [`SendQueueRoomError::is_recoverable`]).
        send_error: Option<QueueWedgeError>,
    },

    /// A local echo has been reacted to.
    React {
        /// The key with which the local echo has been reacted to.
        key: String,
        /// A handle to manipulate the sending of the reaction.
        send_handle: SendReactionHandle,
        /// The local echo which has been reacted to.
        applies_to: OwnedTransactionId,
    },
}

/// A local representation for a request that hasn't been sent yet to the user's
/// homeserver.
#[derive(Clone, Debug)]
pub struct LocalEcho {
    /// Transaction id used to identify the associated request.
    pub transaction_id: OwnedTransactionId,
    /// The content for the local echo.
    pub content: LocalEchoContent,
}

/// An update to a room send queue, observable with
/// [`RoomSendQueue::subscribe`].
#[derive(Clone, Debug)]
pub enum RoomSendQueueUpdate {
    /// A new local event is being sent.
    ///
    /// There's been a user query to create this event. It is being sent to the
    /// server.
    NewLocalEvent(LocalEcho),

    /// A local event that hadn't been sent to the server yet has been cancelled
    /// before sending.
    CancelledLocalEvent {
        /// Transaction id used to identify this event.
        transaction_id: OwnedTransactionId,
    },

    /// A local event's content has been replaced with something else.
    ReplacedLocalEvent {
        /// Transaction id used to identify this event.
        transaction_id: OwnedTransactionId,

        /// The new content replacing the previous one.
        new_content: SerializableEventContent,
    },

    /// An error happened when an event was being sent.
    ///
    /// The event has not been removed from the queue. All the send queues
    /// will be disabled after this happens, and must be manually re-enabled.
    SendError {
        /// Transaction id used to identify this event.
        transaction_id: OwnedTransactionId,
        /// Error received while sending the event.
        error: Arc<crate::Error>,
        /// Whether the error is considered recoverable or not.
        ///
        /// An error that's recoverable will disable the room's send queue,
        /// while an unrecoverable error will be parked, until the user
        /// decides to cancel sending it.
        is_recoverable: bool,
    },

    /// The event has been unwedged and sending is now being retried.
    RetryEvent {
        /// Transaction id used to identify this event.
        transaction_id: OwnedTransactionId,
    },

    /// The event has been sent to the server, and the query returned
    /// successfully.
    SentEvent {
        /// Transaction id used to identify this event.
        transaction_id: OwnedTransactionId,
        /// Received event id from the send response.
        event_id: OwnedEventId,
    },

    /// A media has been successfully uploaded.
    UploadedMedia {
        /// The media event this uploaded media relates to.
        related_to: OwnedTransactionId,

        /// The final media source for the file that was just uploaded.
        file: MediaSource,
    },
}

/// An error triggered by the send queue module.
#[derive(Debug, thiserror::Error)]
pub enum RoomSendQueueError {
    /// The room isn't in the joined state.
    #[error("the room isn't in the joined state")]
    RoomNotJoined,

    /// The room is missing from the client.
    ///
    /// This happens only whenever the client is shutting down.
    #[error("the room is now missing from the client")]
    RoomDisappeared,

    /// Error coming from storage.
    #[error(transparent)]
    StorageError(#[from] RoomSendQueueStorageError),
}

/// An error triggered by the send queue storage.
#[derive(Debug, thiserror::Error)]
pub enum RoomSendQueueStorageError {
    /// Error caused by the state store.
    #[error(transparent)]
    StateStoreError(#[from] StoreError),

    /// Error caused by the event cache store.
    #[error(transparent)]
    EventCacheStoreError(#[from] EventCacheStoreError),

    /// Error caused when attempting to get a handle on the event cache store.
    #[error(transparent)]
    LockError(#[from] LockStoreError),

    /// Error caused when (de)serializing into/from json.
    #[error(transparent)]
    JsonSerialization(#[from] serde_json::Error),

    /// A parent key was expected to be of a certain type, and it was another
    /// type instead.
    #[error("a dependent event had an invalid parent key type")]
    InvalidParentKey,

    /// The client is shutting down.
    #[error("The client is shutting down.")]
    ClientShuttingDown,

    /// An operation not implemented on a send handle.
    #[error("This operation is not implemented for media uploads")]
    OperationNotImplementedYet,

    /// Trying to edit a media caption for something that's not a media.
    #[error("Can't edit a media caption when the underlying event isn't a media")]
    InvalidMediaCaptionEdit,
}

/// Extra transaction IDs useful during an upload.
#[derive(Clone, Debug)]
struct MediaHandles {
    /// Transaction id used when uploading the thumbnail.
    ///
    /// Optional because a media can be uploaded without a thumbnail.
    upload_thumbnail_txn: Option<OwnedTransactionId>,

    /// Transaction id used when uploading the media itself.
    upload_file_txn: OwnedTransactionId,
}

/// A handle to manipulate an event that was scheduled to be sent to a room.
#[derive(Clone, Debug)]
pub struct SendHandle {
    /// Link to the send queue used to send this request.
    room: RoomSendQueue,

    /// Transaction id used for the sent request.
    ///
    /// If this is a media upload, this is the "main" transaction id, i.e. the
    /// one used to send the event, and that will be seen by observers.
    transaction_id: OwnedTransactionId,

    /// Additional handles for a media upload.
    media_handles: Option<MediaHandles>,
}

impl SendHandle {
    fn nyi_for_uploads(&self) -> Result<(), RoomSendQueueStorageError> {
        if self.media_handles.is_some() {
            Err(RoomSendQueueStorageError::OperationNotImplementedYet)
        } else {
            Ok(())
        }
    }

    /// Aborts the sending of the event, if it wasn't sent yet.
    ///
    /// Returns true if the sending could be aborted, false if not (i.e. the
    /// event had already been sent).
    #[instrument(skip(self), fields(room_id = %self.room.inner.room.room_id(), txn_id = %self.transaction_id))]
    pub async fn abort(&self) -> Result<bool, RoomSendQueueStorageError> {
        trace!("received an abort request");

        let queue = &self.room.inner.queue;

        if let Some(handles) = &self.media_handles {
            if queue.abort_upload(&self.transaction_id, handles).await? {
                // Propagate a cancelled update.
                let _ = self.room.inner.updates.send(RoomSendQueueUpdate::CancelledLocalEvent {
                    transaction_id: self.transaction_id.clone(),
                });

                return Ok(true);
            }

            // If it failed, it means the sending of the event is not a
            // dependent request anymore. Fall back to the regular
            // code path below, that handles aborting sending of an event.
        }

        if queue.cancel_event(&self.transaction_id).await? {
            trace!("successful abort");

            // Propagate a cancelled update too.
            let _ = self.room.inner.updates.send(RoomSendQueueUpdate::CancelledLocalEvent {
                transaction_id: self.transaction_id.clone(),
            });

            Ok(true)
        } else {
            debug!("local echo didn't exist anymore, can't abort");
            Ok(false)
        }
    }

    /// Edits the content of a local echo with a raw event content.
    ///
    /// Returns true if the event to be sent was replaced, false if not (i.e.
    /// the event had already been sent).
    #[instrument(skip(self, new_content), fields(room_id = %self.room.inner.room.room_id(), txn_id = %self.transaction_id))]
    pub async fn edit_raw(
        &self,
        new_content: Raw<AnyMessageLikeEventContent>,
        event_type: String,
    ) -> Result<bool, RoomSendQueueStorageError> {
        trace!("received an edit request");
        self.nyi_for_uploads()?;

        let serializable = SerializableEventContent::from_raw(new_content, event_type);

        if self.room.inner.queue.replace_event(&self.transaction_id, serializable.clone()).await? {
            trace!("successful edit");

            // Wake up the queue, in case the room was asleep before the edit.
            self.room.inner.notifier.notify_one();

            // Propagate a replaced update too.
            let _ = self.room.inner.updates.send(RoomSendQueueUpdate::ReplacedLocalEvent {
                transaction_id: self.transaction_id.clone(),
                new_content: serializable,
            });

            Ok(true)
        } else {
            debug!("local echo doesn't exist anymore, can't edit");
            Ok(false)
        }
    }

    /// Edits the content of a local echo with an event content.
    ///
    /// Returns true if the event to be sent was replaced, false if not (i.e.
    /// the event had already been sent).
    pub async fn edit(
        &self,
        new_content: AnyMessageLikeEventContent,
    ) -> Result<bool, RoomSendQueueStorageError> {
        self.edit_raw(
            Raw::new(&new_content).map_err(RoomSendQueueStorageError::JsonSerialization)?,
            new_content.event_type().to_string(),
        )
        .await
    }

    /// Edits the content of a local echo with a media caption.
    ///
    /// Will fail if the event to be sent, represented by this send handle,
    /// wasn't a media.
    pub async fn edit_media_caption(
        &self,
        caption: Option<String>,
        formatted_caption: Option<FormattedBody>,
    ) -> Result<bool, RoomSendQueueStorageError> {
        if let Some(new_content) = self
            .room
            .inner
            .queue
            .edit_media_caption(&self.transaction_id, caption, formatted_caption)
            .await?
        {
            trace!("successful edit of media caption");

            // Wake up the queue, in case the room was asleep before the edit.
            self.room.inner.notifier.notify_one();

            let new_content = SerializableEventContent::new(&new_content)
                .map_err(RoomSendQueueStorageError::JsonSerialization)?;

            // Propagate a replaced update too.
            let _ = self.room.inner.updates.send(RoomSendQueueUpdate::ReplacedLocalEvent {
                transaction_id: self.transaction_id.clone(),
                new_content,
            });

            Ok(true)
        } else {
            debug!("local echo doesn't exist anymore, can't edit media caption");
            Ok(false)
        }
    }

    /// Unwedge a local echo identified by its transaction identifier and try to
    /// resend it.
    pub async fn unwedge(&self) -> Result<(), RoomSendQueueError> {
        let room = &self.room.inner;
        room.queue
            .mark_as_unwedged(&self.transaction_id)
            .await
            .map_err(RoomSendQueueError::StorageError)?;

        // If we have media handles, also try to unwedge them.
        //
        // It's fine to always do it to *all* the transaction IDs at once, because only
        // one of the three requests will be active at the same time, i.e. only
        // one entry will be updated in the store. The other two are either
        // done, or dependent requests.

        if let Some(handles) = &self.media_handles {
            room.queue
                .mark_as_unwedged(&handles.upload_file_txn)
                .await
                .map_err(RoomSendQueueError::StorageError)?;

            if let Some(txn) = &handles.upload_thumbnail_txn {
                room.queue.mark_as_unwedged(txn).await.map_err(RoomSendQueueError::StorageError)?;
            }
        }

        // Wake up the queue, in case the room was asleep before unwedging the request.
        room.notifier.notify_one();

        let _ = room
            .updates
            .send(RoomSendQueueUpdate::RetryEvent { transaction_id: self.transaction_id.clone() });

        Ok(())
    }

    /// Send a reaction to the event as soon as it's sent.
    ///
    /// If returning `Ok(None)`; this means the reaction couldn't be sent
    /// because the event is already a remote one.
    #[instrument(skip(self), fields(room_id = %self.room.inner.room.room_id(), txn_id = %self.transaction_id))]
    pub async fn react(
        &self,
        key: String,
    ) -> Result<Option<SendReactionHandle>, RoomSendQueueStorageError> {
        trace!("received an intent to react");

        if let Some(reaction_txn_id) =
            self.room.inner.queue.react(&self.transaction_id, key.clone()).await?
        {
            trace!("successfully queued react");

            // Wake up the queue, in case the room was asleep before the sending.
            self.room.inner.notifier.notify_one();

            // Propagate a new local event.
            let send_handle = SendReactionHandle {
                room: self.room.clone(),
                transaction_id: reaction_txn_id.clone(),
            };

            let _ = self.room.inner.updates.send(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
                // Note: we do want to use the txn_id we're going to use for the reaction, not the
                // one for the event we're reacting to.
                transaction_id: reaction_txn_id.into(),
                content: LocalEchoContent::React {
                    key,
                    send_handle: send_handle.clone(),
                    applies_to: self.transaction_id.clone(),
                },
            }));

            Ok(Some(send_handle))
        } else {
            debug!("local echo doesn't exist anymore, can't react");
            Ok(None)
        }
    }
}

/// A handle to execute actions on the sending of a reaction.
#[derive(Clone, Debug)]
pub struct SendReactionHandle {
    /// Reference to the send queue for the room where this reaction was sent.
    room: RoomSendQueue,
    /// The own transaction id for the reaction.
    transaction_id: ChildTransactionId,
}

impl SendReactionHandle {
    /// Abort the sending of the reaction.
    ///
    /// Will return true if the reaction could be aborted, false if it's been
    /// sent (and there's no matching local echo anymore).
    pub async fn abort(&self) -> Result<bool, RoomSendQueueStorageError> {
        if self.room.inner.queue.remove_dependent_send_queue_request(&self.transaction_id).await? {
            // Simple case: the reaction was found in the dependent event list.

            // Propagate a cancelled update too.
            let _ = self.room.inner.updates.send(RoomSendQueueUpdate::CancelledLocalEvent {
                transaction_id: self.transaction_id.clone().into(),
            });

            return Ok(true);
        }

        // The reaction has already been queued for sending, try to abort it using a
        // regular abort.
        let handle = SendHandle {
            room: self.room.clone(),
            transaction_id: self.transaction_id.clone().into(),
            media_handles: None,
        };

        handle.abort().await
    }

    /// The transaction id that will be used to send this reaction later.
    pub fn transaction_id(&self) -> &TransactionId {
        &self.transaction_id
    }
}

/// From a given source of [`DependentQueuedRequest`], return only the most
/// meaningful, i.e. the ones that wouldn't be overridden after applying the
/// others.
fn canonicalize_dependent_requests(
    dependent: &[DependentQueuedRequest],
) -> Vec<DependentQueuedRequest> {
    let mut by_txn = HashMap::<OwnedTransactionId, Vec<&DependentQueuedRequest>>::new();

    for d in dependent {
        let prevs = by_txn.entry(d.parent_transaction_id.clone()).or_default();

        if prevs.iter().any(|prev| matches!(prev.kind, DependentQueuedRequestKind::RedactEvent)) {
            // The parent event has already been flagged for redaction, don't consider the
            // other dependent events.
            continue;
        }

        match &d.kind {
            DependentQueuedRequestKind::EditEvent { .. } => {
                // Replace any previous edit with this one.
                if let Some(prev_edit) = prevs
                    .iter_mut()
                    .find(|prev| matches!(prev.kind, DependentQueuedRequestKind::EditEvent { .. }))
                {
                    *prev_edit = d;
                } else {
                    prevs.insert(0, d);
                }
            }

            DependentQueuedRequestKind::UploadFileWithThumbnail { .. }
            | DependentQueuedRequestKind::FinishUpload { .. }
            | DependentQueuedRequestKind::ReactEvent { .. } => {
                // These requests can't be canonicalized, push them as is.
                prevs.push(d);
            }

            DependentQueuedRequestKind::RedactEvent => {
                // Remove every other dependent action.
                prevs.clear();
                prevs.push(d);
            }
        }
    }

    by_txn.into_iter().flat_map(|(_parent_txn_id, entries)| entries.into_iter().cloned()).collect()
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::{sync::Arc, time::Duration};

    use assert_matches2::{assert_let, assert_matches};
    use matrix_sdk_base::store::{
        ChildTransactionId, DependentQueuedRequest, DependentQueuedRequestKind,
        SerializableEventContent,
    };
    use matrix_sdk_test::{async_test, JoinedRoomBuilder, SyncResponseBuilder};
    use ruma::{
        events::{room::message::RoomMessageEventContent, AnyMessageLikeEventContent},
        room_id, TransactionId,
    };

    use super::canonicalize_dependent_requests;
    use crate::{client::WeakClient, test_utils::logged_in_client};

    #[async_test]
    async fn test_client_no_cycle_with_send_queue() {
        for enabled in [true, false] {
            let client = logged_in_client(None).await;
            let weak_client = WeakClient::from_client(&client);

            {
                let mut sync_response_builder = SyncResponseBuilder::new();

                let room_id = room_id!("!a:b.c");

                // Make sure the client knows about the room.
                client
                    .base_client()
                    .receive_sync_response(
                        sync_response_builder
                            .add_joined_room(JoinedRoomBuilder::new(room_id))
                            .build_sync_response(),
                    )
                    .await
                    .unwrap();

                let room = client.get_room(room_id).unwrap();
                let q = room.send_queue();

                let _watcher = q.subscribe().await;

                client.send_queue().set_enabled(enabled).await;
            }

            drop(client);

            // Give a bit of time for background tasks to die.
            tokio::time::sleep(Duration::from_millis(500)).await;

            // The weak client must be the last reference to the client now.
            let client = weak_client.get();
            assert!(
                client.is_none(),
                "too many strong references to the client: {}",
                Arc::strong_count(&client.unwrap().inner)
            );
        }
    }

    #[test]
    fn test_canonicalize_dependent_events_smoke_test() {
        // Smoke test: canonicalizing a single dependent event returns it.
        let txn = TransactionId::new();

        let edit = DependentQueuedRequest {
            own_transaction_id: ChildTransactionId::new(),
            parent_transaction_id: txn.clone(),
            kind: DependentQueuedRequestKind::EditEvent {
                new_content: SerializableEventContent::new(
                    &RoomMessageEventContent::text_plain("edit").into(),
                )
                .unwrap(),
            },
            parent_key: None,
        };
        let res = canonicalize_dependent_requests(&[edit]);

        assert_eq!(res.len(), 1);
        assert_matches!(&res[0].kind, DependentQueuedRequestKind::EditEvent { .. });
        assert_eq!(res[0].parent_transaction_id, txn);
        assert!(res[0].parent_key.is_none());
    }

    #[test]
    fn test_canonicalize_dependent_events_redaction_preferred() {
        // A redaction is preferred over any other kind of dependent event.
        let txn = TransactionId::new();

        let mut inputs = Vec::with_capacity(100);
        let redact = DependentQueuedRequest {
            own_transaction_id: ChildTransactionId::new(),
            parent_transaction_id: txn.clone(),
            kind: DependentQueuedRequestKind::RedactEvent,
            parent_key: None,
        };

        let edit = DependentQueuedRequest {
            own_transaction_id: ChildTransactionId::new(),
            parent_transaction_id: txn.clone(),
            kind: DependentQueuedRequestKind::EditEvent {
                new_content: SerializableEventContent::new(
                    &RoomMessageEventContent::text_plain("edit").into(),
                )
                .unwrap(),
            },
            parent_key: None,
        };

        inputs.push({
            let mut edit = edit.clone();
            edit.own_transaction_id = ChildTransactionId::new();
            edit
        });

        inputs.push(redact);

        for _ in 0..98 {
            let mut edit = edit.clone();
            edit.own_transaction_id = ChildTransactionId::new();
            inputs.push(edit);
        }

        let res = canonicalize_dependent_requests(&inputs);

        assert_eq!(res.len(), 1);
        assert_matches!(&res[0].kind, DependentQueuedRequestKind::RedactEvent);
        assert_eq!(res[0].parent_transaction_id, txn);
    }

    #[test]
    fn test_canonicalize_dependent_events_last_edit_preferred() {
        let parent_txn = TransactionId::new();

        // The latest edit of a list is always preferred.
        let inputs = (0..10)
            .map(|i| DependentQueuedRequest {
                own_transaction_id: ChildTransactionId::new(),
                parent_transaction_id: parent_txn.clone(),
                kind: DependentQueuedRequestKind::EditEvent {
                    new_content: SerializableEventContent::new(
                        &RoomMessageEventContent::text_plain(format!("edit{i}")).into(),
                    )
                    .unwrap(),
                },
                parent_key: None,
            })
            .collect::<Vec<_>>();

        let txn = inputs[9].parent_transaction_id.clone();

        let res = canonicalize_dependent_requests(&inputs);

        assert_eq!(res.len(), 1);
        assert_let!(DependentQueuedRequestKind::EditEvent { new_content } = &res[0].kind);
        assert_let!(
            AnyMessageLikeEventContent::RoomMessage(msg) = new_content.deserialize().unwrap()
        );
        assert_eq!(msg.body(), "edit9");
        assert_eq!(res[0].parent_transaction_id, txn);
    }

    #[test]
    fn test_canonicalize_multiple_local_echoes() {
        let txn1 = TransactionId::new();
        let txn2 = TransactionId::new();

        let child1 = ChildTransactionId::new();
        let child2 = ChildTransactionId::new();

        let inputs = vec![
            // This one pertains to txn1.
            DependentQueuedRequest {
                own_transaction_id: child1.clone(),
                kind: DependentQueuedRequestKind::RedactEvent,
                parent_transaction_id: txn1.clone(),
                parent_key: None,
            },
            // This one pertains to txn2.
            DependentQueuedRequest {
                own_transaction_id: child2,
                kind: DependentQueuedRequestKind::EditEvent {
                    new_content: SerializableEventContent::new(
                        &RoomMessageEventContent::text_plain("edit").into(),
                    )
                    .unwrap(),
                },
                parent_transaction_id: txn2.clone(),
                parent_key: None,
            },
        ];

        let res = canonicalize_dependent_requests(&inputs);

        // The canonicalization shouldn't depend per event id.
        assert_eq!(res.len(), 2);

        for dependent in res {
            if dependent.own_transaction_id == child1 {
                assert_eq!(dependent.parent_transaction_id, txn1);
                assert_matches!(dependent.kind, DependentQueuedRequestKind::RedactEvent);
            } else {
                assert_eq!(dependent.parent_transaction_id, txn2);
                assert_matches!(dependent.kind, DependentQueuedRequestKind::EditEvent { .. });
            }
        }
    }

    #[test]
    fn test_canonicalize_reactions_after_edits() {
        // Sending reactions should happen after edits to a given event.
        let txn = TransactionId::new();

        let react_id = ChildTransactionId::new();
        let react = DependentQueuedRequest {
            own_transaction_id: react_id.clone(),
            kind: DependentQueuedRequestKind::ReactEvent { key: "🧠".to_owned() },
            parent_transaction_id: txn.clone(),
            parent_key: None,
        };

        let edit_id = ChildTransactionId::new();
        let edit = DependentQueuedRequest {
            own_transaction_id: edit_id.clone(),
            kind: DependentQueuedRequestKind::EditEvent {
                new_content: SerializableEventContent::new(
                    &RoomMessageEventContent::text_plain("edit").into(),
                )
                .unwrap(),
            },
            parent_transaction_id: txn,
            parent_key: None,
        };

        let res = canonicalize_dependent_requests(&[react, edit]);

        assert_eq!(res.len(), 2);
        assert_eq!(res[0].own_transaction_id, edit_id);
        assert_eq!(res[1].own_transaction_id, react_id);
    }
}
