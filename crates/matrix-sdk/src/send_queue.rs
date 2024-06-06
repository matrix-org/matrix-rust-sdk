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

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, RwLock as SyncRwLock,
    },
};

use eyeball::{SharedObservable, Subscriber};
use matrix_sdk_base::RoomState;
use matrix_sdk_common::executor::{spawn, JoinHandle};
use ruma::{
    events::AnyMessageLikeEventContent, OwnedEventId, OwnedRoomId, OwnedTransactionId,
    TransactionId,
};
use tokio::sync::{broadcast, Notify, RwLock};
use tracing::{debug, error, info, instrument, trace, warn};

use crate::{client::WeakClient, config::RequestConfig, room::WeakRoom, Client, Room};

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

    fn for_room(&self, room: Room) -> RoomSendQueue {
        let data = &self.client.inner.sending_queue_data;

        let mut map = data.rooms.write().unwrap();

        let room_id = room.room_id();
        if let Some(room_q) = map.get(room_id).cloned() {
            return room_q;
        }

        let owned_room_id = room_id.to_owned();
        let room_q = RoomSendQueue::new(
            data.globally_enabled.clone(),
            data.is_dropping.clone(),
            &self.client,
            owned_room_id.clone(),
        );
        map.insert(owned_room_id, room_q.clone());
        room_q
    }

    /// Enable the send queue for the entire client, i.e. all rooms.
    ///
    /// This may wake up background tasks and resume sending of events in the
    /// background.
    pub fn enable(&self) {
        if self.client.inner.sending_queue_data.globally_enabled.set_if_not_eq(true).is_some() {
            debug!("globally enabling send queue");
            let rooms = self.client.inner.sending_queue_data.rooms.read().unwrap();
            // Wake up the rooms, in case events have been queued in the meanwhile.
            for room in rooms.values() {
                room.inner.notifier.notify_one();
            }
        }
    }

    /// Disable the send queue for the entire client, i.e. all rooms.
    ///
    /// If requests were being sent, they're not aborted, and will continue
    /// until a status resolves (error responses will keep the events in the
    /// buffer of events to send later). The disablement will happen before
    /// the next event is sent.
    pub fn disable(&self) {
        // Note: it's not required to wake the tasks just to let them know they're
        // disabled:
        // - either they were busy, will continue to the next iteration and realize
        // the queue is now disabled,
        // - or they were not, and it's not worth it waking them to let them they're
        // disabled, which causes them to go to sleep again.
        debug!("globally disabling send queue");
        self.client.inner.sending_queue_data.globally_enabled.set(false);
    }

    /// Returns whether the send queue is enabled, at a client-wide
    /// granularity.
    pub fn is_enabled(&self) -> bool {
        self.client.inner.sending_queue_data.globally_enabled.get()
    }

    /// A subscriber to the enablement status (enabled or disabled) of the
    /// send queue.
    pub fn subscribe_status(&self) -> Subscriber<bool> {
        self.client.inner.sending_queue_data.globally_enabled.subscribe()
    }
}

impl Client {
    /// Returns a [`SendQueue`] that handles sending, retrying and not
    /// forgetting about messages that are to be sent.
    pub fn sending_queue(&self) -> SendQueue {
        SendQueue::new(self.clone())
    }
}

pub(super) struct SendQueueData {
    /// Mapping of room to their unique send queue.
    rooms: SyncRwLock<BTreeMap<OwnedRoomId, RoomSendQueue>>,

    /// Is the whole mechanism enabled or disabled?
    globally_enabled: SharedObservable<bool>,

    /// Are we currently dropping the Client?
    is_dropping: Arc<AtomicBool>,
}

impl SendQueueData {
    /// Create the data for a send queue, in the given enabled state.
    pub fn new(globally_enabled: bool) -> Self {
        Self {
            rooms: Default::default(),
            globally_enabled: SharedObservable::new(globally_enabled),
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
    pub fn sending_queue(&self) -> RoomSendQueue {
        self.client.sending_queue().for_room(self.clone())
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
        globally_enabled: SharedObservable<bool>,
        is_dropping: Arc<AtomicBool>,
        client: &Client,
        room_id: OwnedRoomId,
    ) -> Self {
        let (updates_sender, _) = broadcast::channel(32);

        let queue = QueueStorage::new();
        let notifier = Arc::new(Notify::new());

        let weak_room = WeakRoom::new(WeakClient::from_client(client), room_id);

        let task = spawn(Self::sending_task(
            weak_room.clone(),
            queue.clone(),
            notifier.clone(),
            updates_sender.clone(),
            globally_enabled,
            is_dropping,
        ));

        Self {
            inner: Arc::new(RoomSendQueueInner {
                room: weak_room,
                updates: updates_sender,
                _task: task,
                queue,
                notifier,
            }),
        }
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
    ) -> Result<AbortSendHandle, RoomSendQueueError> {
        let Some(room) = self.inner.room.get() else {
            return Err(RoomSendQueueError::RoomDisappeared);
        };
        if room.state() != RoomState::Joined {
            return Err(RoomSendQueueError::RoomNotJoined);
        }

        let transaction_id = self.inner.queue.push(content.clone()).await;
        trace!(%transaction_id, "manager sends an event to the background task");

        self.inner.notifier.notify_one();

        let _ = self.inner.updates.send(RoomSendQueueUpdate::NewLocalEvent(LocalEcho {
            transaction_id: transaction_id.clone(),
            content,
            abort_handle: AbortSendHandle {
                room: self.clone(),
                transaction_id: transaction_id.clone(),
            },
        }));

        Ok(AbortSendHandle { transaction_id, room: self.clone() })
    }

    /// Returns the current local events as well as a receiver to listen to the
    /// send queue updates, as defined in [`RoomSendQueueUpdate`].
    pub async fn subscribe(&self) -> (Vec<LocalEcho>, broadcast::Receiver<RoomSendQueueUpdate>) {
        let local_echoes = self
            .inner
            .queue
            .local_echoes()
            .await
            .into_iter()
            .map(|(transaction_id, content)| LocalEcho {
                transaction_id: transaction_id.clone(),
                content,
                abort_handle: AbortSendHandle { room: self.clone(), transaction_id },
            })
            .collect();

        (local_echoes, self.inner.updates.subscribe())
    }

    #[instrument(skip_all, fields(room_id = %room.room_id()))]
    async fn sending_task(
        room: WeakRoom,
        queue: QueueStorage,
        notifier: Arc<Notify>,
        updates: broadcast::Sender<RoomSendQueueUpdate>,
        globally_enabled: SharedObservable<bool>,
        is_dropping: Arc<AtomicBool>,
    ) {
        info!("spawned the sending task");

        loop {
            // A request to shut down should be preferred above everything else.
            if is_dropping.load(Ordering::SeqCst) {
                trace!("shutting down!");
                break;
            }

            if !globally_enabled.get() {
                trace!("not enabled, sleeping");
                // Wait for an explicit wakeup.
                notifier.notified().await;
                continue;
            }

            let Some(queued_event) = queue.peek_next_to_send().await else {
                trace!("queue is empty, sleeping");
                // Wait for an explicit wakeup.
                notifier.notified().await;
                continue;
            };

            trace!("received an event to send!");

            let Some(room) = room.get() else {
                if is_dropping.load(Ordering::SeqCst) {
                    break;
                }
                error!("the weak room couldn't be upgraded but we're not shutting down?");
                continue;
            };

            match room
                .send(queued_event.event)
                .with_transaction_id(&queued_event.transaction_id)
                .with_request_config(RequestConfig::short_retry())
                .await
            {
                Ok(res) => {
                    trace!(txn_id = %queued_event.transaction_id, event_id = %res.event_id, "successfully sent");

                    queue.mark_as_sent(&queued_event.transaction_id).await;

                    let _ = updates.send(RoomSendQueueUpdate::SentEvent {
                        transaction_id: queued_event.transaction_id,
                        event_id: res.event_id,
                    });
                }

                Err(err) => {
                    warn!(txn_id = %queued_event.transaction_id, "error when sending event: {err}");

                    // In this case, we intentionally keep the event in the queue, but mark it as
                    // not being sent anymore.
                    queue.mark_as_not_being_sent(&queued_event.transaction_id).await;

                    // Let observers know about a failure *after* we've marked the item as not
                    // being sent anymore. Otherwise, there's a possible race where a caller might
                    // try to remove an item, while it's still marked as being sent, resulting in a
                    // cancellation failure.

                    // Disable the queue after an error.
                    // See comment in [`SendQueue::disable()`].
                    globally_enabled.set(false);

                    let _ = updates.send(RoomSendQueueUpdate::SendError {
                        transaction_id: queued_event.transaction_id,
                        error: Arc::new(err),
                    });
                }
            }
        }

        info!("exited sending task");
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

    /// Handle to the actual sending task. Unused, but kept alive along this
    /// data structure.
    _task: JoinHandle<()>,
}

#[derive(Clone)]
struct QueuedEvent {
    event: AnyMessageLikeEventContent,
    transaction_id: OwnedTransactionId,

    /// Flag to indicate if an event has been scheduled for sending.
    ///
    /// Useful to indicate if cancelling could happen or if it was too late and
    /// the event had already been sent.
    is_being_sent: bool,
}

#[derive(Clone)]
struct QueueStorage(Arc<RwLock<VecDeque<QueuedEvent>>>);

impl QueueStorage {
    /// Create a new synchronized queue for queuing events to be sent later.
    fn new() -> Self {
        Self(Arc::new(RwLock::new(VecDeque::with_capacity(16))))
    }

    /// Push a new event to be sent in the queue.
    ///
    /// Returns the transaction id chosen to identify the request.
    async fn push(&self, content: AnyMessageLikeEventContent) -> OwnedTransactionId {
        let transaction_id = TransactionId::new();

        self.0.write().await.push_back(QueuedEvent {
            event: content,
            transaction_id: transaction_id.clone(),
            is_being_sent: false,
        });

        transaction_id
    }

    /// Peeks the next event to be sent, marking it as being sent.
    ///
    /// It is required to call [`Self::mark_as_sent`] after it's been
    /// effectively sent.
    async fn peek_next_to_send(&self) -> Option<QueuedEvent> {
        let mut q = self.0.write().await;
        if let Some(event) = q.front_mut() {
            // TODO: This flag should probably live in memory when we have an actual
            // storage.
            event.is_being_sent = true;
            Some(event.clone())
        } else {
            None
        }
    }

    /// Marks an event popped with [`Self::peek_next_to_send`] and identified
    /// with the given transaction id as not being sent anymore, so it can
    /// be removed from the queue later.
    async fn mark_as_not_being_sent(&self, transaction_id: &TransactionId) {
        for item in self.0.write().await.iter_mut() {
            if item.transaction_id == transaction_id {
                item.is_being_sent = false;
                break;
            }
        }
    }

    /// Marks an event pushed with [`Self::push`] and identified with the given
    /// transaction id as sent by removing it from the local queue.
    async fn mark_as_sent(&self, transaction_id: &TransactionId) {
        let mut q = self.0.write().await;
        if let Some(index) = q.iter().position(|item| item.transaction_id == transaction_id) {
            q.remove(index);
        } else {
            warn!("couldn't find item to mark as sent with transaction id {transaction_id}");
        }
    }

    /// Cancel a sending command for an event that has been sent with
    /// [`Self::push`] with the given transaction id.
    ///
    /// Returns whether the given transaction has been effectively removed. If
    /// false, this either means that the transaction id was unrelated to
    /// this queue, or that the event was sent before we cancelled it.
    async fn cancel(&self, transaction_id: &TransactionId) -> bool {
        let mut found = false;
        self.0.write().await.retain(|queued| {
            if queued.transaction_id == transaction_id && !queued.is_being_sent {
                found = true;
                false
            } else {
                true
            }
        });
        found
    }

    /// Returns a list of the local echoes, that is, all the events that we're
    /// about to send but that haven't been sent yet (or are being sent).
    async fn local_echoes(&self) -> Vec<(OwnedTransactionId, AnyMessageLikeEventContent)> {
        self.0
            .write()
            .await
            .iter()
            .map(|queued| (queued.transaction_id.clone(), queued.event.clone()))
            .collect()
    }
}

/// An event that has been locally queued for sending, but hasn't been sent yet.
#[derive(Clone, Debug)]
pub struct LocalEcho {
    /// Transaction id used to identify this event.
    pub transaction_id: OwnedTransactionId,
    /// Content of the event itself, that we are about to send.
    pub content: AnyMessageLikeEventContent,
    /// A handle to abort sending the associated event.
    pub abort_handle: AbortSendHandle,
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

    /// An error happened when an event was being sent.
    ///
    /// The event has not been removed from the queue. All the send queues
    /// will be disabled after this happens, and must be manually re-enabled.
    SendError {
        /// Transaction id used to identify this event.
        transaction_id: OwnedTransactionId,
        /// Error received while sending the event.
        error: Arc<crate::Error>,
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
}

/// A way to tentatively abort sending an event that was scheduled to be sent to
/// a room.
#[derive(Clone, Debug)]
pub struct AbortSendHandle {
    room: RoomSendQueue,
    transaction_id: OwnedTransactionId,
}

impl AbortSendHandle {
    /// Aborts the sending of the event, if it wasn't sent yet.
    ///
    /// Returns true if the sending could be aborted, false if not (i.e. the
    /// event had already been sent).
    pub async fn abort(self) -> bool {
        if self.room.inner.queue.cancel(&self.transaction_id).await {
            // Propagate a cancelled update too.
            let _ = self.room.inner.updates.send(RoomSendQueueUpdate::CancelledLocalEvent {
                transaction_id: self.transaction_id.clone(),
            });
            true
        } else {
            false
        }
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
                let q = room.sending_queue();

                let _watcher = q.subscribe().await;

                if enabled {
                    client.sending_queue().enable();
                } else {
                    client.sending_queue().disable();
                }
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
