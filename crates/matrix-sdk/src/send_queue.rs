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
//!   [`SendQueue::respawn_tasks_for_rooms_with_unsent_events()`]. It is
//!   recommended to call this method during initialization of a client,
//!   otherwise persisted unsent events will only be re-sent after the send
//!   queue for the given room has been reopened for the first time.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock as SyncRwLock,
    },
};

use matrix_sdk_base::{
    store::{QueuedEvent, SerializableEventContent},
    RoomState, StoreError,
};
use matrix_sdk_common::executor::{spawn, JoinHandle};
use ruma::{
    events::{AnyMessageLikeEventContent, EventContent as _},
    serde::Raw,
    OwnedEventId, OwnedRoomId, OwnedTransactionId, TransactionId,
};
use tokio::sync::{broadcast, Notify, RwLock};
use tracing::{debug, error, info, instrument, trace, warn};

use crate::{
    client::WeakClient, config::RequestConfig, error::RetryKind, room::WeakRoom, Client, Room,
};

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

    /// Reload all the rooms which had unsent events, and respawn tasks for
    /// those rooms.
    pub async fn respawn_tasks_for_rooms_with_unsent_events(&self) {
        if !self.is_enabled() {
            return;
        }

        let room_ids =
            self.client.store().load_rooms_with_unsent_events().await.unwrap_or_else(|err| {
                warn!("error when loading rooms with unsent events: {err}");
                Vec::new()
            });

        // Getting the [`RoomSendQueue`] is sufficient to spawn the task if needs be.
        for room_id in room_ids {
            if let Some(room) = self.client.get_room(&room_id) {
                let _ = self.for_room(room);
            }
        }
    }

    #[inline(always)]
    fn data(&self) -> &SendQueueData {
        &self.client.inner.send_queue_data
    }

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
    /// disablement will happen before the next event is sent.
    ///
    /// This may wake up background tasks and resume sending of events in the
    /// background.
    pub async fn set_enabled(&self, enabled: bool) {
        debug!(?enabled, "setting global send queue enablement");

        self.data().globally_enabled.store(enabled, Ordering::SeqCst);

        // Wake up individual rooms we already know about.
        for room in self.data().rooms.read().unwrap().values() {
            room.set_enabled(enabled);
        }

        // Reload some extra rooms that might not have been awaken yet, but could have
        // events from previous sessions.
        self.respawn_tasks_for_rooms_with_unsent_events().await;
    }

    /// Returns whether the send queue is enabled, at a client-wide
    /// granularity.
    pub fn is_enabled(&self) -> bool {
        self.data().globally_enabled.load(Ordering::SeqCst)
    }

    /// A subscriber to the enablement status (enabled or disabled) of the
    /// send queue.
    pub fn subscribe_errors(&self) -> broadcast::Receiver<SendQueueRoomError> {
        self.data().error_reporter.subscribe()
    }
}

/// A specific room ran into an error, and has disabled itself.
#[derive(Clone, Debug)]
pub struct SendQueueRoomError {
    /// Which room is failing?
    pub room_id: OwnedRoomId,

    /// The error the room has ran into, when trying to send an event.
    pub error: Arc<crate::Error>,

    /// Whether the error is considered recoverable or not.
    ///
    /// An error that's recoverable will disable the room's send queue, while an
    /// unrecoverable error will be parked, until the user decides to cancel
    /// sending it.
    pub is_recoverable: bool,
}

impl Client {
    /// Returns a [`SendQueue`] that handles sending, retrying and not
    /// forgetting about messages that are to be sent.
    pub fn send_queue(&self) -> SendQueue {
        SendQueue::new(self.clone())
    }
}

pub(super) struct SendQueueData {
    /// Mapping of room to their unique send queue.
    rooms: SyncRwLock<BTreeMap<OwnedRoomId, RoomSendQueue>>,

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
    /// By default, if sending the event fails on the first attempt, it will be
    /// retried a few times. If sending failed, the entire client's sending
    /// queue will be disabled, and it will need to be manually re-enabled
    /// by the caller.
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

        let transaction_id = self.inner.queue.push(content.clone()).await?;
        trace!(%transaction_id, "manager sends a raw event to the background task");

        self.inner.notifier.notify_one();

        let _ = self.inner.updates.send(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
            transaction_id: transaction_id.clone(),
            serialized_event: content,
            send_handle: SendHandle { room: self.clone(), transaction_id: transaction_id.clone() },
            is_wedged: false,
        }));

        Ok(SendHandle { transaction_id, room: self.clone() })
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
    /// By default, if sending the event fails on the first attempt, it will be
    /// retried a few times. If sending failed, the entire client's sending
    /// queue will be disabled, and it will need to be manually re-enabled
    /// by the caller.
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

    /// Returns the current local events as well as a receiver to listen to the
    /// send queue updates, as defined in [`RoomSendQueueUpdate`].
    pub async fn subscribe(
        &self,
    ) -> Result<(Vec<LocalEcho>, broadcast::Receiver<RoomSendQueueUpdate>), RoomSendQueueError>
    {
        let local_echoes = self
            .inner
            .queue
            .local_echoes()
            .await?
            .into_iter()
            .map(|queued| LocalEcho {
                transaction_id: queued.transaction_id.clone(),
                serialized_event: queued.event,
                send_handle: SendHandle {
                    room: self.clone(),
                    transaction_id: queued.transaction_id,
                },
                is_wedged: queued.is_wedged,
            })
            .collect();

        Ok((local_echoes, self.inner.updates.subscribe()))
    }

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

            if !locally_enabled.load(Ordering::SeqCst) {
                trace!("not enabled, sleeping");
                // Wait for an explicit wakeup.
                notifier.notified().await;
                continue;
            }

            let queued_event = match queue.peek_next_to_send().await {
                Ok(Some(event)) => event,

                Ok(None) => {
                    trace!("queue is empty, sleeping");
                    // Wait for an explicit wakeup.
                    notifier.notified().await;
                    continue;
                }

                Err(err) => {
                    warn!("error when loading next event to send: {err}");
                    continue;
                }
            };

            trace!(txn_id = %queued_event.transaction_id, "received an event to send!");

            let Some(room) = room.get() else {
                if is_dropping.load(Ordering::SeqCst) {
                    break;
                }
                error!("the weak room couldn't be upgraded but we're not shutting down?");
                continue;
            };

            let (event, event_type) = queued_event.event.raw();
            match room
                .send_raw(&event_type.to_string(), event)
                .with_transaction_id(&queued_event.transaction_id)
                .with_request_config(RequestConfig::short_retry())
                .await
            {
                Ok(res) => {
                    trace!(txn_id = %queued_event.transaction_id, event_id = %res.event_id, "successfully sent");

                    match queue.mark_as_sent(&queued_event.transaction_id).await {
                        Ok(()) => {
                            let _ = updates.send(RoomSendQueueUpdate::SentEvent {
                                transaction_id: queued_event.transaction_id,
                                event_id: res.event_id,
                            });
                        }

                        Err(err) => {
                            warn!("unable to mark queued event as sent: {err}");
                        }
                    }
                }

                Err(err) => {
                    let is_recoverable = match err {
                        crate::Error::Http(ref http_err) => {
                            // All transient errors are recoverable.
                            matches!(http_err.retry_kind(), RetryKind::Transient { .. })
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
                        warn!(txn_id = %queued_event.transaction_id, error = ?err, "Recoverable error when sending event: {err}, disabling send queue");

                        // In this case, we intentionally keep the event in the queue, but mark it
                        // as not being sent anymore.
                        queue.mark_as_not_being_sent(&queued_event.transaction_id).await;

                        // Let observers know about a failure *after* we've marked the item as not
                        // being sent anymore. Otherwise, there's a possible race where a caller
                        // might try to remove an item, while it's still
                        // marked as being sent, resulting in a cancellation
                        // failure.

                        // Disable the queue for this room after a recoverable error happened. This
                        // should be the sign that this error is temporary (maybe network
                        // disconnected, maybe the server had a hiccup).
                        locally_enabled.store(false, Ordering::SeqCst);
                    } else {
                        warn!(txn_id = %queued_event.transaction_id, error = ?err, "Unrecoverable error when sending event: {err}");

                        // Mark the event as wedged, so it's not picked at any future point.
                        if let Err(err) = queue.mark_as_wedged(&queued_event.transaction_id).await {
                            warn!("unable to mark event as wedged: {err}");
                        }
                    }

                    let error = Arc::new(err);

                    let _ = global_error_reporter.send(SendQueueRoomError {
                        room_id: room.room_id().to_owned(),
                        error: error.clone(),
                        is_recoverable,
                    });

                    let _ = updates.send(RoomSendQueueUpdate::SendError {
                        transaction_id: queued_event.transaction_id,
                        error,
                        is_recoverable,
                    });
                }
            }
        }

        info!("exited sending task");
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

struct RoomSendQueueInner {
    /// The room which this send queue relates to.
    room: WeakRoom,

    /// Broadcaster for notifications about the statuses of events to be sent.
    ///
    /// Can be subscribed to from the outside.
    updates: broadcast::Sender<RoomSendQueueUpdate>,

    /// Queue of events that are either to be sent, or being sent.
    ///
    /// When an event has been sent to the server, it is removed from that queue
    /// *after* being sent. That way, we will retry sending upon failure, in
    /// the same order events have been inserted in the first place.
    ///
    /// In the future, this will be replaced by a database, and this field may
    /// be removed. Instead of appending to that queue / updating its
    /// content / deleting entries, all that will be required will be to
    /// manipulate the on-disk storage. In other words, the storage will become
    /// the one source of truth.
    queue: QueueStorage,

    /// A notifier that's updated any time common data is touched (stopped or
    /// enabled statuses), or the associated room [`QueueStorage`].
    notifier: Arc<Notify>,

    /// Should the room process new events or not (because e.g. it might be
    /// running off the network)?
    locally_enabled: Arc<AtomicBool>,

    /// Handle to the actual sending task. Unused, but kept alive along this
    /// data structure.
    _task: JoinHandle<()>,
}

#[derive(Clone)]
struct QueueStorage {
    /// Reference to the client, to get access to the underlying store.
    client: WeakClient,

    /// To which room is this storage related.
    room_id: OwnedRoomId,

    /// All the queued events that are being sent at the moment.
    ///
    /// It also serves as an internal lock on the storage backend.
    being_sent: Arc<RwLock<BTreeSet<OwnedTransactionId>>>,
}

impl QueueStorage {
    /// Create a new synchronized queue for queuing events to be sent later.
    fn new(client: WeakClient, room: OwnedRoomId) -> Self {
        Self { room_id: room, being_sent: Default::default(), client }
    }

    /// Push a new event to be sent in the queue.
    ///
    /// Returns the transaction id chosen to identify the request.
    async fn push(
        &self,
        serializable: SerializableEventContent,
    ) -> Result<OwnedTransactionId, RoomSendQueueStorageError> {
        let transaction_id = TransactionId::new();

        self.client
            .get()
            .ok_or(RoomSendQueueStorageError::ClientShuttingDown)?
            .store()
            .save_send_queue_event(&self.room_id, transaction_id.clone(), serializable)
            .await?;

        Ok(transaction_id)
    }

    /// Peeks the next event to be sent, marking it as being sent.
    ///
    /// It is required to call [`Self::mark_as_sent`] after it's been
    /// effectively sent.
    async fn peek_next_to_send(&self) -> Result<Option<QueuedEvent>, RoomSendQueueStorageError> {
        // Keep the lock until we're done touching the storage.
        let mut being_sent = self.being_sent.write().await;

        let queued_events = self
            .client
            .get()
            .ok_or(RoomSendQueueStorageError::ClientShuttingDown)?
            .store()
            .load_send_queue_events(&self.room_id)
            .await?;

        if let Some(event) = queued_events.iter().find(|queued| !queued.is_wedged) {
            being_sent.insert(event.transaction_id.clone());

            Ok(Some(event.clone()))
        } else {
            Ok(None)
        }
    }

    /// Marks an event popped with [`Self::peek_next_to_send`] and identified
    /// with the given transaction id as not being sent anymore, so it can
    /// be removed from the queue later.
    async fn mark_as_not_being_sent(&self, transaction_id: &TransactionId) {
        self.being_sent.write().await.remove(transaction_id);
    }

    /// Marks an event popped with [`Self::peek_next_to_send`] and identified
    /// with the given transaction id as being wedged (and not being sent
    /// anymore), so it can be removed from the queue later.
    async fn mark_as_wedged(
        &self,
        transaction_id: &TransactionId,
    ) -> Result<(), RoomSendQueueStorageError> {
        // Keep the lock until we're done touching the storage.
        let mut being_sent = self.being_sent.write().await;
        being_sent.remove(transaction_id);

        Ok(self
            .client
            .get()
            .ok_or(RoomSendQueueStorageError::ClientShuttingDown)?
            .store()
            .update_send_queue_event_status(&self.room_id, transaction_id, true)
            .await?)
    }

    /// Marks an event pushed with [`Self::push`] and identified with the given
    /// transaction id as sent by removing it from the local queue.
    async fn mark_as_sent(
        &self,
        transaction_id: &TransactionId,
    ) -> Result<(), RoomSendQueueStorageError> {
        // Keep the lock until we're done touching the storage.
        let mut being_sent = self.being_sent.write().await;
        being_sent.remove(transaction_id);

        let removed = self
            .client
            .get()
            .ok_or(RoomSendQueueStorageError::ClientShuttingDown)?
            .store()
            .remove_send_queue_event(&self.room_id, transaction_id)
            .await?;

        if !removed {
            warn!(txn_id = %transaction_id, "event marked as sent was missing from storage");
        }

        Ok(())
    }

    /// Cancel a sending command for an event that has been sent with
    /// [`Self::push`] with the given transaction id.
    ///
    /// Returns whether the given transaction has been effectively removed. If
    /// false, this either means that the transaction id was unrelated to
    /// this queue, or that the event was sent before we cancelled it.
    async fn cancel(
        &self,
        transaction_id: &TransactionId,
    ) -> Result<bool, RoomSendQueueStorageError> {
        // Keep the lock until we're done touching the storage.
        let being_sent = self.being_sent.read().await;

        if being_sent.contains(transaction_id) {
            return Ok(false);
        }

        let removed = self
            .client
            .get()
            .ok_or(RoomSendQueueStorageError::ClientShuttingDown)?
            .store()
            .remove_send_queue_event(&self.room_id, transaction_id)
            .await?;

        Ok(removed)
    }

    /// Replace an event that has been sent with
    /// [`Self::push`] with the given transaction id, before it's been actually
    /// sent.
    ///
    /// Returns whether the given transaction has been effectively edited. If
    /// false, this either means that the transaction id was unrelated to
    /// this queue, or that the event was sent before we edited it.
    async fn replace(
        &self,
        transaction_id: &TransactionId,
        serializable: SerializableEventContent,
    ) -> Result<bool, RoomSendQueueStorageError> {
        // Keep the lock until we're done touching the storage.
        let being_sent = self.being_sent.read().await;

        if being_sent.contains(transaction_id) {
            return Ok(false);
        }

        let edited = self
            .client
            .get()
            .ok_or(RoomSendQueueStorageError::ClientShuttingDown)?
            .store()
            .update_send_queue_event(&self.room_id, transaction_id, serializable)
            .await?;

        Ok(edited)
    }

    /// Returns a list of the local echoes, that is, all the events that we're
    /// about to send but that haven't been sent yet (or are being sent).
    async fn local_echoes(&self) -> Result<Vec<QueuedEvent>, RoomSendQueueStorageError> {
        Ok(self
            .client
            .get()
            .ok_or(RoomSendQueueStorageError::ClientShuttingDown)?
            .store()
            .load_send_queue_events(&self.room_id)
            .await?)
    }
}

/// An event that has been locally queued for sending, but hasn't been sent yet.
#[derive(Clone, Debug)]
pub struct LocalEcho {
    /// Transaction id used to identify this event.
    pub transaction_id: OwnedTransactionId,
    /// Content of the event itself (along with its type) that we are about to
    /// send.
    pub serialized_event: SerializableEventContent,
    /// A handle to manipulate the sending of the associated event.
    pub send_handle: SendHandle,
    /// Whether trying to send this local echo failed in the past with an
    /// unrecoverable error (see [`SendQueueRoomError::is_recoverable`]).
    pub is_wedged: bool,
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

    /// The event has been sent to the server, and the query returned
    /// successfully.
    SentEvent {
        /// Transaction id used to identify this event.
        transaction_id: OwnedTransactionId,
        /// Received event id from the send response.
        event_id: OwnedEventId,
    },
}

/// An error triggered by the send queue module.
#[derive(Debug, thiserror::Error)]
pub enum RoomSendQueueError {
    /// The room isn't in the joined state.
    #[error("the room isn't in the joined state")]
    RoomNotJoined,

    /// The room is missing from the client. This could happen if the client is
    /// shutting down.
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
    StorageError(#[from] StoreError),

    /// Error caused when (de)serializing into/from json.
    #[error(transparent)]
    JsonSerialization(#[from] serde_json::Error),

    /// The client is shutting down.
    #[error("The client is shutting down.")]
    ClientShuttingDown,
}

/// A handle to manipulate an event that was scheduled to be sent to a room.
#[derive(Clone, Debug)]
pub struct SendHandle {
    room: RoomSendQueue,
    transaction_id: OwnedTransactionId,
}

impl SendHandle {
    /// Aborts the sending of the event, if it wasn't sent yet.
    ///
    /// Returns true if the sending could be aborted, false if not (i.e. the
    /// event had already been sent).
    #[instrument(skip(self), fields(room_id = %self.room.inner.room.room_id(), txn_id = %self.transaction_id))]
    pub async fn abort(&self) -> Result<bool, RoomSendQueueStorageError> {
        trace!("received an abort request");

        if self.room.inner.queue.cancel(&self.transaction_id).await? {
            trace!("successful abort");

            // Propagate a cancelled update too.
            let _ = self.room.inner.updates.send(RoomSendQueueUpdate::CancelledLocalEvent {
                transaction_id: self.transaction_id.clone(),
            });

            Ok(true)
        } else {
            debug!("event was being sent, can't abort");
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

        let serializable = SerializableEventContent::from_raw(new_content, event_type);

        if self.room.inner.queue.replace(&self.transaction_id, serializable.clone()).await? {
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
            debug!("event was being sent, can't edit");
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
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::{sync::Arc, time::Duration};

    use matrix_sdk_test::{async_test, JoinedRoomBuilder, SyncResponseBuilder};
    use ruma::room_id;

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
}
