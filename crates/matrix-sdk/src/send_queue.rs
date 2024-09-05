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
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock as SyncRwLock,
    },
};

use matrix_sdk_base::{
    store::{
        ChildTransactionId, DependentQueuedEvent, DependentQueuedEventKind, QueuedEvent,
        SerializableEventContent,
    },
    RoomState, StoreError,
};
use matrix_sdk_common::executor::{spawn, JoinHandle};
use ruma::{
    events::{
        reaction::ReactionEventContent, relation::Annotation, AnyMessageLikeEventContent,
        EventContent as _,
    },
    serde::Raw,
    EventId, OwnedEventId, OwnedRoomId, OwnedTransactionId, TransactionId,
};
use tokio::sync::{broadcast, Notify, RwLock};
use tracing::{debug, error, info, instrument, trace, warn};

use crate::{
    client::WeakClient,
    config::RequestConfig,
    error::RetryKind,
    room::{edit::EditedContent, WeakRoom},
    Client, Room,
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
            content: LocalEchoContent::Event {
                serialized_event: content,
                send_handle: SendHandle {
                    room: self.clone(),
                    transaction_id: transaction_id.clone(),
                },
                is_wedged: false,
            },
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
        let local_echoes = self.inner.queue.local_echoes(self).await?;

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

            // Try to apply dependent events now; those applying to previously failed
            // attempts (local echoes) would succeed now.
            if let Err(err) = queue.apply_dependent_events().await {
                warn!("errors when applying dependent events: {err}");
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

                    match queue.mark_as_sent(&queued_event.transaction_id, &res.event_id).await {
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

    /// Unwedge a local echo identified by its transaction identifier and try to
    /// resend it.
    pub async fn unwedge(&self, transaction_id: &TransactionId) -> Result<(), RoomSendQueueError> {
        self.inner
            .queue
            .mark_as_unwedged(transaction_id)
            .await
            .map_err(RoomSendQueueError::StorageError)?;

        // Wake up the queue, in case the room was asleep before unwedging the event.
        self.inner.notifier.notify_one();

        let _ = self
            .inner
            .updates
            .send(RoomSendQueueUpdate::RetryEvent { transaction_id: transaction_id.to_owned() });

        Ok(())
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

    /// Small helper to get a strong Client from the weak one.
    fn client(&self) -> Result<Client, RoomSendQueueStorageError> {
        self.client.get().ok_or(RoomSendQueueStorageError::ClientShuttingDown)
    }

    /// Push a new event to be sent in the queue.
    ///
    /// Returns the transaction id chosen to identify the request.
    async fn push(
        &self,
        serializable: SerializableEventContent,
    ) -> Result<OwnedTransactionId, RoomSendQueueStorageError> {
        let transaction_id = TransactionId::new();

        self.client()?
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

        let queued_events = self.client()?.store().load_send_queue_events(&self.room_id).await?;

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
            .client()?
            .store()
            .update_send_queue_event_status(&self.room_id, transaction_id, true)
            .await?)
    }

    /// Marks an event identified with the given transaction id as being now
    /// unwedged and adds it back to the queue.
    async fn mark_as_unwedged(
        &self,
        transaction_id: &TransactionId,
    ) -> Result<(), RoomSendQueueStorageError> {
        Ok(self
            .client()?
            .store()
            .update_send_queue_event_status(&self.room_id, transaction_id, false)
            .await?)
    }

    /// Marks an event pushed with [`Self::push`] and identified with the given
    /// transaction id as sent by removing it from the local queue.
    async fn mark_as_sent(
        &self,
        transaction_id: &TransactionId,
        event_id: &EventId,
    ) -> Result<(), RoomSendQueueStorageError> {
        // Keep the lock until we're done touching the storage.
        let mut being_sent = self.being_sent.write().await;
        being_sent.remove(transaction_id);

        let client = self.client()?;
        let store = client.store();

        // Update all dependent events.
        store
            .update_dependent_send_queue_event(&self.room_id, transaction_id, event_id.to_owned())
            .await?;

        let removed = store.remove_send_queue_event(&self.room_id, transaction_id).await?;

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
            // Save the intent to redact the event.
            self.client()?
                .store()
                .save_dependent_send_queue_event(
                    &self.room_id,
                    transaction_id,
                    ChildTransactionId::new(),
                    DependentQueuedEventKind::Redact,
                )
                .await?;

            return Ok(true);
        }

        let removed =
            self.client()?.store().remove_send_queue_event(&self.room_id, transaction_id).await?;

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
            // Save the intent to redact the event.
            self.client()?
                .store()
                .save_dependent_send_queue_event(
                    &self.room_id,
                    transaction_id,
                    ChildTransactionId::new(),
                    DependentQueuedEventKind::Edit { new_content: serializable },
                )
                .await?;

            return Ok(true);
        }

        let edited = self
            .client()?
            .store()
            .update_send_queue_event(&self.room_id, transaction_id, serializable)
            .await?;

        Ok(edited)
    }

    #[instrument(skip(self))]
    async fn react(
        &self,
        transaction_id: &TransactionId,
        key: String,
    ) -> Result<Option<ChildTransactionId>, RoomSendQueueStorageError> {
        let client = self.client()?;
        let store = client.store();

        let queued_events = store.load_send_queue_events(&self.room_id).await?;

        // If the event has been already sent, abort immediately.
        if !queued_events.iter().any(|item| item.transaction_id == transaction_id) {
            return Ok(None);
        }

        // Record the dependent event.
        let reaction_txn_id = ChildTransactionId::new();
        store
            .save_dependent_send_queue_event(
                &self.room_id,
                transaction_id,
                reaction_txn_id.clone(),
                DependentQueuedEventKind::React { key },
            )
            .await?;

        Ok(Some(reaction_txn_id))
    }

    /// Returns a list of the local echoes, that is, all the events that we're
    /// about to send but that haven't been sent yet (or are being sent).
    async fn local_echoes(
        &self,
        room: &RoomSendQueue,
    ) -> Result<Vec<LocalEcho>, RoomSendQueueStorageError> {
        let client = self.client()?;
        let store = client.store();

        let local_events =
            store.load_send_queue_events(&self.room_id).await?.into_iter().map(|queued| {
                LocalEcho {
                    transaction_id: queued.transaction_id.clone(),
                    content: LocalEchoContent::Event {
                        serialized_event: queued.event,
                        send_handle: SendHandle {
                            room: room.clone(),
                            transaction_id: queued.transaction_id,
                        },
                        is_wedged: queued.is_wedged,
                    },
                }
            });

        let local_reactions = store
            .list_dependent_send_queue_events(&self.room_id)
            .await?
            .into_iter()
            .filter_map(|dep| match dep.kind {
                DependentQueuedEventKind::Edit { .. } | DependentQueuedEventKind::Redact => None,
                DependentQueuedEventKind::React { key } => Some(LocalEcho {
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
            });

        Ok(local_events.chain(local_reactions).collect())
    }

    /// Try to apply a single dependent event, whether it's local or remote.
    ///
    /// This swallows errors that would retrigger every time if we retried
    /// applying the dependent event: invalid edit content, etc.
    ///
    /// Returns true if the dependent event has been sent (or should not be
    /// retried later).
    #[instrument(skip_all)]
    async fn try_apply_single_dependent_event(
        &self,
        client: &Client,
        de: DependentQueuedEvent,
    ) -> Result<bool, RoomSendQueueError> {
        let store = client.store();

        match de.kind {
            DependentQueuedEventKind::Edit { new_content } => {
                if let Some(event_id) = de.event_id {
                    // The parent event has been sent, so send an edit event.
                    let room = client
                        .get_room(&self.room_id)
                        .ok_or(RoomSendQueueError::RoomDisappeared)?;

                    // Check the event is one we know how to edit with an edit event.

                    // It must be deserializable
                    let edited_content = match new_content.deserialize() {
                        Ok(AnyMessageLikeEventContent::RoomMessage(c)) => {
                            EditedContent::RoomMessage(c.into())
                        }
                        Ok(AnyMessageLikeEventContent::UnstablePollStart(c)) => {
                            let poll_start = c.poll_start().clone();
                            EditedContent::PollStart(poll_start.question.text.clone(), poll_start)
                        }
                        Ok(c) => {
                            warn!("Unsupported edit content type: {:?}", c.event_type());
                            return Ok(true);
                        }
                        Err(err) => {
                            warn!("unable to deserialize: {err}");
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
                        .save_send_queue_event(
                            &self.room_id,
                            de.own_transaction_id.into(),
                            serializable,
                        )
                        .await
                        .map_err(RoomSendQueueStorageError::StorageError)?;
                } else {
                    // The parent event is still local (sending must have failed); update the local
                    // echo.
                    let edited = store
                        .update_send_queue_event(
                            &self.room_id,
                            &de.parent_transaction_id,
                            new_content,
                        )
                        .await
                        .map_err(RoomSendQueueStorageError::StorageError)?;

                    if !edited {
                        warn!("missing local echo upon dependent edit");
                    }
                }
            }

            DependentQueuedEventKind::Redact => {
                if let Some(event_id) = de.event_id {
                    // The parent event has been sent; send a redaction.
                    let room = client
                        .get_room(&self.room_id)
                        .ok_or(RoomSendQueueError::RoomDisappeared)?;

                    // Ideally we'd use the send queue to send the redaction, but the protocol has
                    // changed the shape of a room.redaction after v11, so keep it simple and try
                    // once here.

                    // Note: no reason is provided because we materialize the intent of "cancel
                    // sending the parent event".

                    if let Err(err) =
                        room.redact(&event_id, None, Some(de.own_transaction_id.into())).await
                    {
                        warn!("error when sending a redact for {event_id}: {err}");
                        return Ok(false);
                    }
                } else {
                    // The parent event is still local (sending must have failed); redact the local
                    // echo.
                    let removed = store
                        .remove_send_queue_event(&self.room_id, &de.parent_transaction_id)
                        .await
                        .map_err(RoomSendQueueStorageError::StorageError)?;

                    if !removed {
                        warn!("missing local echo upon dependent redact");
                    }
                }
            }

            DependentQueuedEventKind::React { key } => {
                if let Some(event_id) = de.event_id {
                    // Queue the reaction event in the send queue 🧠.
                    let react_event =
                        ReactionEventContent::new(Annotation::new(event_id, key)).into();
                    let serializable = SerializableEventContent::from_raw(
                        Raw::new(&react_event)
                            .map_err(RoomSendQueueStorageError::JsonSerialization)?,
                        react_event.event_type().to_string(),
                    );

                    store
                        .save_send_queue_event(
                            &self.room_id,
                            de.own_transaction_id.into(),
                            serializable,
                        )
                        .await
                        .map_err(RoomSendQueueStorageError::StorageError)?;
                } else {
                    // Not applied yet, we should retry later => false.
                    return Ok(false);
                }
            }
        }

        Ok(true)
    }

    #[instrument(skip(self))]
    async fn apply_dependent_events(&self) -> Result<(), RoomSendQueueError> {
        // Keep the lock until we're done touching the storage.
        let _being_sent = self.being_sent.read().await;

        let client = self.client()?;
        let store = client.store();

        let dependent_events = store
            .list_dependent_send_queue_events(&self.room_id)
            .await
            .map_err(RoomSendQueueStorageError::StorageError)?;

        let num_initial_dependent_events = dependent_events.len();
        if num_initial_dependent_events == 0 {
            // Returning early here avoids a bit of useless logging.
            return Ok(());
        }

        let canonicalized_dependent_events = canonicalize_dependent_events(&dependent_events);

        // Get rid of the all non-canonical dependent events.
        for original in &dependent_events {
            if !canonicalized_dependent_events
                .iter()
                .any(|canonical| canonical.own_transaction_id == original.own_transaction_id)
            {
                store
                    .remove_dependent_send_queue_event(&self.room_id, &original.own_transaction_id)
                    .await
                    .map_err(RoomSendQueueStorageError::StorageError)?;
            }
        }

        let mut num_dependent_events = canonicalized_dependent_events.len();

        debug!(
            num_dependent_events,
            num_initial_dependent_events, "starting handling of dependent events"
        );

        for dependent in canonicalized_dependent_events {
            let dependent_id = dependent.own_transaction_id.clone();

            match self.try_apply_single_dependent_event(&client, dependent).await {
                Ok(should_remove) => {
                    if should_remove {
                        // The dependent event has been successfully applied, forget about it.
                        store
                            .remove_dependent_send_queue_event(&self.room_id, &dependent_id)
                            .await
                            .map_err(RoomSendQueueStorageError::StorageError)?;

                        num_dependent_events -= 1;
                    }
                }

                Err(err) => {
                    warn!("error when applying single dependent event: {err}");
                }
            }
        }

        debug!(
            leftover_dependent_events = num_dependent_events,
            "stopped handling dependent events"
        );

        Ok(())
    }

    /// Remove a single dependent event from storage.
    async fn remove_dependent_send_queue_event(
        &self,
        dependent_event_id: &ChildTransactionId,
    ) -> Result<bool, RoomSendQueueStorageError> {
        // Keep the lock until we're done touching the storage.
        let _being_sent = self.being_sent.read().await;

        Ok(self
            .client()?
            .store()
            .remove_dependent_send_queue_event(&self.room_id, dependent_event_id)
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
        is_wedged: bool,
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

/// An event that has been locally queued for sending, but hasn't been sent yet.
#[derive(Clone, Debug)]
pub struct LocalEcho {
    /// Transaction id used to identify this event.
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
        if self.room.inner.queue.remove_dependent_send_queue_event(&self.transaction_id).await? {
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
        };

        handle.abort().await
    }

    /// The transaction id that will be used to send this reaction later.
    pub fn transaction_id(&self) -> &TransactionId {
        &self.transaction_id
    }
}

/// From a given source of [`DependentQueuedEvent`], return only the most
/// meaningful, i.e. the ones that wouldn't be overridden after applying the
/// others.
fn canonicalize_dependent_events(dependent: &[DependentQueuedEvent]) -> Vec<DependentQueuedEvent> {
    let mut by_event_id = HashMap::<OwnedTransactionId, Vec<&DependentQueuedEvent>>::new();

    for d in dependent {
        let prevs = by_event_id.entry(d.parent_transaction_id.clone()).or_default();

        if prevs.iter().any(|prev| matches!(prev.kind, DependentQueuedEventKind::Redact)) {
            // The event has already been flagged for redaction, don't consider the other
            // dependent events.
            continue;
        }

        match &d.kind {
            DependentQueuedEventKind::Edit { .. } => {
                // Replace any previous edit with this one.
                if let Some(prev_edit) = prevs
                    .iter_mut()
                    .find(|prev| matches!(prev.kind, DependentQueuedEventKind::Edit { .. }))
                {
                    *prev_edit = d;
                } else {
                    prevs.insert(0, d);
                }
            }

            DependentQueuedEventKind::React { .. } => {
                prevs.push(d);
            }

            DependentQueuedEventKind::Redact => {
                // Remove every other dependent action.
                prevs.clear();
                prevs.push(d);
            }
        }
    }

    by_event_id
        .into_iter()
        .flat_map(|(_parent_txn_id, entries)| entries.into_iter().cloned())
        .collect()
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::{sync::Arc, time::Duration};

    use assert_matches2::{assert_let, assert_matches};
    use matrix_sdk_base::store::{
        ChildTransactionId, DependentQueuedEvent, DependentQueuedEventKind,
        SerializableEventContent,
    };
    use matrix_sdk_test::{async_test, JoinedRoomBuilder, SyncResponseBuilder};
    use ruma::{
        events::{room::message::RoomMessageEventContent, AnyMessageLikeEventContent},
        room_id, TransactionId,
    };

    use super::canonicalize_dependent_events;
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

        let edit = DependentQueuedEvent {
            own_transaction_id: ChildTransactionId::new(),
            parent_transaction_id: txn.clone(),
            kind: DependentQueuedEventKind::Edit {
                new_content: SerializableEventContent::new(
                    &RoomMessageEventContent::text_plain("edit").into(),
                )
                .unwrap(),
            },
            event_id: None,
        };
        let res = canonicalize_dependent_events(&[edit]);

        assert_eq!(res.len(), 1);
        assert_matches!(&res[0].kind, DependentQueuedEventKind::Edit { .. });
        assert_eq!(res[0].parent_transaction_id, txn);
        assert!(res[0].event_id.is_none());
    }

    #[test]
    fn test_canonicalize_dependent_events_redaction_preferred() {
        // A redaction is preferred over any other kind of dependent event.
        let txn = TransactionId::new();

        let mut inputs = Vec::with_capacity(100);
        let redact = DependentQueuedEvent {
            own_transaction_id: ChildTransactionId::new(),
            parent_transaction_id: txn.clone(),
            kind: DependentQueuedEventKind::Redact,
            event_id: None,
        };

        let edit = DependentQueuedEvent {
            own_transaction_id: ChildTransactionId::new(),
            parent_transaction_id: txn.clone(),
            kind: DependentQueuedEventKind::Edit {
                new_content: SerializableEventContent::new(
                    &RoomMessageEventContent::text_plain("edit").into(),
                )
                .unwrap(),
            },
            event_id: None,
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

        let res = canonicalize_dependent_events(&inputs);

        assert_eq!(res.len(), 1);
        assert_matches!(&res[0].kind, DependentQueuedEventKind::Redact);
        assert_eq!(res[0].parent_transaction_id, txn);
    }

    #[test]
    fn test_canonicalize_dependent_events_last_edit_preferred() {
        let parent_txn = TransactionId::new();

        // The latest edit of a list is always preferred.
        let inputs = (0..10)
            .map(|i| DependentQueuedEvent {
                own_transaction_id: ChildTransactionId::new(),
                parent_transaction_id: parent_txn.clone(),
                kind: DependentQueuedEventKind::Edit {
                    new_content: SerializableEventContent::new(
                        &RoomMessageEventContent::text_plain(format!("edit{i}")).into(),
                    )
                    .unwrap(),
                },
                event_id: None,
            })
            .collect::<Vec<_>>();

        let txn = inputs[9].parent_transaction_id.clone();

        let res = canonicalize_dependent_events(&inputs);

        assert_eq!(res.len(), 1);
        assert_let!(DependentQueuedEventKind::Edit { new_content } = &res[0].kind);
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
            DependentQueuedEvent {
                own_transaction_id: child1.clone(),
                kind: DependentQueuedEventKind::Redact,
                parent_transaction_id: txn1.clone(),
                event_id: None,
            },
            // This one pertains to txn2.
            DependentQueuedEvent {
                own_transaction_id: child2,
                kind: DependentQueuedEventKind::Edit {
                    new_content: SerializableEventContent::new(
                        &RoomMessageEventContent::text_plain("edit").into(),
                    )
                    .unwrap(),
                },
                parent_transaction_id: txn2.clone(),
                event_id: None,
            },
        ];

        let res = canonicalize_dependent_events(&inputs);

        // The canonicalization shouldn't depend per event id.
        assert_eq!(res.len(), 2);

        for dependent in res {
            if dependent.own_transaction_id == child1 {
                assert_eq!(dependent.parent_transaction_id, txn1);
                assert_matches!(dependent.kind, DependentQueuedEventKind::Redact);
            } else {
                assert_eq!(dependent.parent_transaction_id, txn2);
                assert_matches!(dependent.kind, DependentQueuedEventKind::Edit { .. });
            }
        }
    }

    #[test]
    fn test_canonicalize_reactions_after_edits() {
        // Sending reactions should happen after edits to a given event.
        let txn = TransactionId::new();

        let react_id = ChildTransactionId::new();
        let react = DependentQueuedEvent {
            own_transaction_id: react_id.clone(),
            kind: DependentQueuedEventKind::React { key: "🧠".to_owned() },
            parent_transaction_id: txn.clone(),
            event_id: None,
        };

        let edit_id = ChildTransactionId::new();
        let edit = DependentQueuedEvent {
            own_transaction_id: edit_id.clone(),
            kind: DependentQueuedEventKind::Edit {
                new_content: SerializableEventContent::new(
                    &RoomMessageEventContent::text_plain("edit").into(),
                )
                .unwrap(),
            },
            parent_transaction_id: txn,
            event_id: None,
        };

        let res = canonicalize_dependent_events(&[react, edit]);

        assert_eq!(res.len(), 2);
        assert_eq!(res[0].own_transaction_id, edit_id);
        assert_eq!(res[1].own_transaction_id, react_id);
    }
}
