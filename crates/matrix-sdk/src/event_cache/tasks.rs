// Copyright 2026 The Matrix.org Foundation C.I.C.
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

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Weak},
};

use eyeball::Subscriber;
use matrix_sdk_base::{
    linked_chunk::OwnedLinkedChunkId, locks::Mutex,
    serde_helpers::extract_thread_root_from_content, sync::RoomUpdates,
};
use ruma::{OwnedEventId, OwnedRoomId, OwnedTransactionId};
use tokio::{
    select,
    sync::{
        broadcast::{Receiver, Sender, error::RecvError},
        mpsc,
    },
    task::JoinSet,
};
use tracing::{Instrument as _, Span, debug, error, info, info_span, instrument, trace, warn};

use super::{
    AutoShrinkChannelPayload, EventCacheError, EventCacheInner, EventsOrigin,
    RoomEventCacheLinkedChunkUpdate, RoomEventCacheUpdate, TimelineVectorDiffs,
};
use crate::{
    client::WeakClient,
    send_queue::{LocalEchoContent, RoomSendQueueUpdate, SendQueueUpdate},
};

/// Listen to [`RoomUpdates`] to update the Event Cache.
#[instrument(skip_all)]
pub(super) async fn room_updates_task(
    inner: Arc<EventCacheInner>,
    mut room_updates_feed: Receiver<RoomUpdates>,
) {
    trace!("Spawning the listen task");
    loop {
        match room_updates_feed.recv().await {
            Ok(updates) => {
                trace!("Receiving `RoomUpdates`");

                if let Err(err) = inner.handle_room_updates(updates).await {
                    match err {
                        EventCacheError::ClientDropped => {
                            // The client has dropped, exit the listen task.
                            info!(
                                "Closing the event cache global listen task because client dropped"
                            );
                            break;
                        }
                        err => {
                            error!("Error when handling room updates: {err}");
                        }
                    }
                }
            }

            Err(RecvError::Lagged(num_skipped)) => {
                // Forget everything we know; we could have missed events, and we have
                // no way to reconcile at the moment!
                // TODO: implement Smart Matching™,
                warn!(num_skipped, "Lagged behind room updates, clearing all rooms");
                if let Err(err) = inner.clear_all_rooms().await {
                    error!("when clearing storage after lag in listen_task: {err}");
                }
            }

            Err(RecvError::Closed) => {
                // The sender has shut down, exit.
                info!("Closing the event cache global listen task because receiver closed");
                break;
            }
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) enum BackgroundRequest {
    PaginateRoomBackwards { room_id: OwnedRoomId },
}

/// Listen to background requests, and dispatch them to worker tasks.
///
/// Instead of processing pagination requests one at a time, this task acts as a
/// dispatcher: it receives requests from an mpsc channel and spawns them onto a
/// bounded pool of worker tasks. This allows paginations for different rooms to
/// run concurrently, while the dispatcher itself remains mostly idle.
///
/// Concurrency is bounded by
/// [`EventCacheConfig::max_concurrent_background_paginations`]. Paginations for
/// the *same* room are serialized by the dispatcher: when a request arrives for
/// a room that already has an in-flight worker, it is queued and re-dispatched
/// once the current worker completes. This avoids coalescing with a potentially
/// failing in-flight pagination, ensuring retries get their own network
/// request.
///
/// [`EventCacheConfig`]: super::EventCacheConfig
#[instrument(skip_all)]
pub(super) async fn background_requests_task(
    inner: Arc<EventCacheInner>,
    mut receiver: mpsc::Receiver<BackgroundRequest>,
) {
    trace!("Spawning the background request task");

    let max_concurrent_workers = inner.config.read().unwrap().max_concurrent_background_paginations;

    let mut room_pagination_credits = HashMap::new();
    let mut join_set = JoinSet::new();

    let in_flight_rooms = Arc::new(Mutex::new(HashSet::new()));

    loop {
        // Back-pressure: if at capacity, wait for a worker to finish.
        while join_set.len() >= max_concurrent_workers {
            if let Some(join_result) = join_set.join_next().await {
                process_worker_result(
                    inner.clone(),
                    join_result,
                    &mut room_pagination_credits,
                    &mut join_set,
                    in_flight_rooms.clone(),
                );
            }
        }

        select! {
            // Prefer cleaning up completed workers when available.
            biased;

            Some(join_result) = join_set.join_next(), if !join_set.is_empty() => {
                process_worker_result(inner.clone(), join_result, &mut room_pagination_credits, &mut join_set, in_flight_rooms.clone());
            }

            request = receiver.recv() => {
                let Some(request) = request else {
                    break;
                };

                match request {
                    BackgroundRequest::PaginateRoomBackwards { room_id } => {
                        start_background_pagination(
                            inner.clone(),
                            room_id,
                            &mut room_pagination_credits,
                            &mut join_set,
                            in_flight_rooms.clone(),
                        );
                    }
                }
            }
        }
    }

    // Shut down all the worker tasks, if any are still running.
    join_set.abort_all();

    info!("Closing the background request task because receiver closed");
}

fn start_background_pagination(
    inner: Arc<EventCacheInner>,
    room_id: OwnedRoomId,
    room_pagination_credits: &mut HashMap<OwnedRoomId, usize>,
    join_set: &mut JoinSet<BackgroundPaginationResult>,
    in_flight_rooms: Arc<Mutex<HashSet<OwnedRoomId>>>,
) {
    // Check credits before spawning.
    let credits = room_pagination_credits
        .entry(room_id.clone())
        .or_insert_with(|| inner.config.read().unwrap().room_pagination_per_room_credit);

    if *credits == 0 {
        trace!(for_room = %room_id, "No more credits to paginate this room, skipping");
        return;
    }

    // Check that we're not already paginating.
    if !in_flight_rooms.lock().insert(room_id.clone()) {
        trace!(for_room = %room_id, "Pagination already in-flight, skipping");
        return;
    }

    // Spawn the pagination work onto a worker task.
    join_set.spawn(run_background_pagination(inner.clone(), room_id, in_flight_rooms.clone()));
}

/// Process the result of a completed worker task.
///
/// This handles credit bookkeeping, removes the room from the in-flight set,
/// and re-dispatches any pending request for the same room.
fn process_worker_result(
    inner: Arc<EventCacheInner>,
    result: Result<BackgroundPaginationResult, tokio::task::JoinError>,
    room_pagination_credits: &mut HashMap<OwnedRoomId, usize>,
    join_set: &mut JoinSet<BackgroundPaginationResult>,
    in_flight_rooms: Arc<Mutex<HashSet<OwnedRoomId>>>,
) {
    let worker_result = match result {
        Ok(r) => r,
        Err(err) => {
            warn!("Background pagination worker panicked: {err}");
            return;
        }
    };

    // Update credits based on the pagination outcome.
    if worker_result.should_decrement_credit
        && let Some(credit) = room_pagination_credits.get_mut(&worker_result.room_id)
    {
        *credit = credit.saturating_sub(1);
    }

    if worker_result.retry {
        start_background_pagination(
            inner,
            worker_result.room_id,
            room_pagination_credits,
            join_set,
            in_flight_rooms,
        );
    }
}

/// The result returned by a background pagination worker.
struct BackgroundPaginationResult {
    /// The room that was paginated.
    room_id: OwnedRoomId,
    /// Whether the dispatcher should decrement a credit for this room.
    should_decrement_credit: bool,
    /// Whether the pagination ended in an error, and should be retried later.
    retry: bool,
}

/// Run a single background pagination for a room.
///
/// This is spawned as an independent task by [`background_requests_task`].
/// It returns a [`BackgroundPaginationResult`] so the dispatcher can handle
/// credit bookkeeping and per-room re-dispatch.
///
/// It is expected that the room id is already in the in_flight_rooms set at the
/// beginning of this function call.
async fn run_background_pagination(
    inner: Arc<EventCacheInner>,
    room_id: OwnedRoomId,
    in_flight_rooms: Arc<Mutex<HashSet<OwnedRoomId>>>,
) -> BackgroundPaginationResult {
    debug_assert!(
        { in_flight_rooms.lock().contains(&room_id) },
        "API contract not respected by the caller: room_id should be in in_flight_rooms at the beginning of run_background_pagination"
    );

    /// RAII to remove the room from the in-flight set when the worker finishes,
    /// even if it panics.
    struct RemoveInFlightGuard {
        room_id: OwnedRoomId,
        in_flight_rooms: Arc<Mutex<HashSet<OwnedRoomId>>>,
    }

    impl Drop for RemoveInFlightGuard {
        fn drop(&mut self) {
            self.in_flight_rooms.lock().remove(&self.room_id);
        }
    }

    let _guard = RemoveInFlightGuard { room_id: room_id.clone(), in_flight_rooms };

    let pagination = match inner.all_caches_for_room(&room_id).await {
        Ok(caches) => caches.room.pagination(),
        Err(err) => {
            warn!(for_room = %room_id, "Failed to get the `Caches`: {err}");
            return BackgroundPaginationResult {
                room_id,
                should_decrement_credit: false,
                retry: false,
            };
        }
    };

    trace!(for_room = %room_id, "automatic backpagination triggered");

    let room_pagination_batch_size = inner.config.read().unwrap().room_pagination_batch_size;

    match pagination.run_backwards_once(room_pagination_batch_size).await {
        Ok(outcome) => {
            // Only decrement the credit if we did meaningful progress (i.e. reached the
            // start or there was new events).
            let should_decrement_credit = !outcome.reached_start || !outcome.events.is_empty();
            BackgroundPaginationResult { room_id, should_decrement_credit, retry: false }
        }
        Err(err) => {
            warn!(for_room = %room_id, "Failed to run background pagination: {err}");
            BackgroundPaginationResult { room_id, should_decrement_credit: false, retry: true }
        }
    }
}

/// Listen to _ignore user list update changes_ to clear the rooms when a user
/// is ignored or unignored.
#[instrument(skip_all)]
pub(super) async fn ignore_user_list_update_task(
    inner: Arc<EventCacheInner>,
    mut ignore_user_list_stream: Subscriber<Vec<String>>,
) {
    let span = info_span!(parent: Span::none(), "ignore_user_list_update_task");
    span.follows_from(Span::current());

    async move {
        while ignore_user_list_stream.next().await.is_some() {
            info!("Received an ignore user list change");

            if let Err(err) = inner.clear_all_rooms().await {
                error!("when clearing room storage after ignore user list change: {err}");
            }
        }

        info!("Ignore user list stream has closed");
    }
    .instrument(span)
    .await;
}

/// Spawns the task that will listen to auto-shrink notifications.
///
/// The auto-shrink mechanism works this way:
///
/// - Each time there's a new subscriber to a [`RoomEventCache`], it will
///   increment the active number of subscribers to that room, aka
///   `RoomEventCacheState::subscriber_count`.
/// - When that subscriber is dropped, it will decrement that count; and notify
///   the task below if it reached 0.
/// - The task spawned here, owned by the [`EventCacheInner`], will listen to
///   such notifications that a room may be shrunk. It will attempt an
///   auto-shrink, by letting the inner state decide whether this is a good time
///   to do so (new subscribers might have spawned in the meanwhile).
///
/// [`RoomEventCache`]: super::RoomEventCache
/// [`EventCacheInner`]: super::EventCacheInner
#[instrument(skip_all)]
pub(super) async fn auto_shrink_linked_chunk_task(
    inner: Weak<EventCacheInner>,
    mut rx: mpsc::Receiver<AutoShrinkChannelPayload>,
) {
    while let Some(room_id) = rx.recv().await {
        trace!(for_room = %room_id, "received notification to shrink");

        let Some(inner) = inner.upgrade() else {
            return;
        };

        let room = {
            let caches = match inner.all_caches_for_room(&room_id).await {
                Ok(caches) => caches,
                Err(err) => {
                    warn!(for_room = %room_id, "Failed to get the `Caches`: {err}");
                    continue;
                }
            };

            caches.room.clone()
        };

        trace!("Waiting for state lock…");

        let mut state = match room.state().write().await {
            Ok(state) => state,
            Err(err) => {
                warn!(for_room = %room_id, "Failed to get the `RoomEventCacheStateLock`: {err}");
                continue;
            }
        };

        match state.auto_shrink_if_no_subscribers().await {
            Ok(diffs) => {
                if let Some(diffs) = diffs {
                    // Hey, fun stuff: we shrunk the linked chunk, so there shouldn't be any
                    // subscribers, right? RIGHT? Especially because the state is guarded behind
                    // a lock.
                    //
                    // However, better safe than sorry, and it's cheap to send an update here,
                    // so let's do it!
                    if !diffs.is_empty() {
                        room.update_sender().send(
                            RoomEventCacheUpdate::UpdateTimelineEvents(TimelineVectorDiffs {
                                diffs,
                                origin: EventsOrigin::Cache,
                            }),
                            None,
                        );
                    }
                } else {
                    debug!("auto-shrinking didn't happen");
                }
            }

            Err(err) => {
                // There's not much we can do here, unfortunately.
                warn!(for_room = %room_id, "error when attempting to shrink linked chunk: {err}");
            }
        }
    }

    info!("Auto-shrink linked chunk task has been closed, exiting");
}

/// Handle [`SendQueueUpdate`] and [`RoomEventCacheLinkedChunkUpdate`] to update
/// the threads, for a thread the user was not subscribed to.
#[instrument(skip_all)]
pub(super) async fn thread_subscriber_task(
    client: WeakClient,
    linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,
    thread_subscriber_sender: Sender<()>,
) {
    let mut send_q_rx = if let Some(client) = client.get() {
        match client.enabled_thread_subscriptions().await {
            Ok(enabled) => {
                if !enabled {
                    trace!(
                        "Thread subscriptions are not enabled, not spawning thread subscriber task"
                    );
                    return;
                }
            }

            Err(err) => {
                warn!(%err, "Failed to get whether thread subscriptions are enabled, not spawning thread subscriber task");
                return;
            }
        }

        client.send_queue().subscribe()
    } else {
        trace!("Client is shutting down, not spawning thread subscriber task");
        return;
    };

    let mut linked_chunk_rx = linked_chunk_update_sender.subscribe();

    // A mapping of local echoes (events being sent), to their thread root, if
    // they're in an in-thread reply.
    //
    // Entirely managed by `handle_thread_subscriber_send_queue_update`.
    let mut events_being_sent = HashMap::new();

    loop {
        select! {
            res = send_q_rx.recv() => {
                match res {
                    Ok(up) => {
                        if !handle_thread_subscriber_send_queue_update(&client, &thread_subscriber_sender, &mut events_being_sent, up).await {
                            break;
                        }
                    }
                    Err(RecvError::Closed) => {
                        debug!("Linked chunk update channel has been closed, exiting thread subscriber task");
                        break;
                    }
                    Err(RecvError::Lagged(num_skipped)) => {
                        warn!(num_skipped, "Lagged behind linked chunk updates");
                    }
                }
            }

            res = linked_chunk_rx.recv() => {
                match res {
                    Ok(up) => {
                        if !handle_thread_subscriber_linked_chunk_update(&client, &thread_subscriber_sender, up).await {
                            break;
                        }
                    }
                    Err(RecvError::Closed) => {
                        debug!("Linked chunk update channel has been closed, exiting thread subscriber task");
                        break;
                    }
                    Err(RecvError::Lagged(num_skipped)) => {
                        warn!(num_skipped, "Lagged behind linked chunk updates");
                    }
                }
            }
        }
    }
}

/// React to a given send queue update by subscribing the user to a
/// thread, if needs be (when the user sent an event in a thread they were
/// not subscribed to).
///
/// Returns a boolean indicating whether the task should keep on running or
/// not.
#[instrument(skip(client, thread_subscriber_sender))]
async fn handle_thread_subscriber_send_queue_update(
    client: &WeakClient,
    thread_subscriber_sender: &Sender<()>,
    events_being_sent: &mut HashMap<OwnedTransactionId, OwnedEventId>,
    up: SendQueueUpdate,
) -> bool {
    let Some(client) = client.get() else {
        // Client shutting down.
        debug!("Client is shutting down, exiting thread subscriber task");
        return false;
    };

    let room_id = up.room_id;
    let Some(room) = client.get_room(&room_id) else {
        warn!(%room_id, "unknown room");
        return true;
    };

    let (thread_root, subscribe_up_to) = match up.update {
        RoomSendQueueUpdate::NewLocalEvent(local_echo) => {
            match local_echo.content {
                LocalEchoContent::Event { serialized_event, .. } => {
                    if let Some(thread_root) =
                        extract_thread_root_from_content(serialized_event.into_raw().0)
                    {
                        events_being_sent.insert(local_echo.transaction_id, thread_root);
                    }
                }
                LocalEchoContent::React { .. } => {
                    // Nothing to do, reactions don't count as a thread
                    // subscription.
                }
            }
            return true;
        }

        RoomSendQueueUpdate::CancelledLocalEvent { transaction_id } => {
            events_being_sent.remove(&transaction_id);
            return true;
        }

        RoomSendQueueUpdate::ReplacedLocalEvent { transaction_id, new_content } => {
            if let Some(thread_root) = extract_thread_root_from_content(new_content.into_raw().0) {
                events_being_sent.insert(transaction_id, thread_root);
            } else {
                // It could be that the event isn't part of a thread anymore; handle that by
                // removing the pending transaction id.
                events_being_sent.remove(&transaction_id);
            }
            return true;
        }

        RoomSendQueueUpdate::SentEvent { transaction_id, event_id } => {
            if let Some(thread_root) = events_being_sent.remove(&transaction_id) {
                (thread_root, event_id)
            } else {
                // We don't know about the event that has been sent, so ignore it.
                trace!(%transaction_id, "received a sent event that we didn't know about, ignoring");
                return true;
            }
        }

        RoomSendQueueUpdate::SendError { .. }
        | RoomSendQueueUpdate::RetryEvent { .. }
        | RoomSendQueueUpdate::MediaUpload { .. } => {
            // Nothing to do for these bad boys.
            return true;
        }
    };

    // And if we've found such a mention, subscribe to the thread up to this event.
    trace!(thread = %thread_root, up_to = %subscribe_up_to, "found a new thread to subscribe to");

    if let Err(err) = room.subscribe_thread_if_needed(&thread_root, Some(subscribe_up_to)).await {
        warn!(%err, "Failed to subscribe to thread");
    } else {
        let _ = thread_subscriber_sender.send(());
    }

    true
}

/// React to a given linked chunk update by subscribing the user to a
/// thread, if needs be (when the user got mentioned in a thread reply, for
/// a thread they were not subscribed to).
///
/// Returns a boolean indicating whether the task should keep on running or
/// not.
#[instrument(skip(client, thread_subscriber_sender))]
async fn handle_thread_subscriber_linked_chunk_update(
    client: &WeakClient,
    thread_subscriber_sender: &Sender<()>,
    up: RoomEventCacheLinkedChunkUpdate,
) -> bool {
    let Some(client) = client.get() else {
        // Client shutting down.
        debug!("Client is shutting down, exiting thread subscriber task");
        return false;
    };

    let OwnedLinkedChunkId::Thread(room_id, thread_root) = &up.linked_chunk_id else {
        trace!("received an update for a non-thread linked chunk, ignoring");
        return true;
    };

    let Some(room) = client.get_room(room_id) else {
        warn!(%room_id, "unknown room");
        return true;
    };

    let thread_root = thread_root.clone();

    let mut new_events = up.events().peekable();

    if new_events.peek().is_none() {
        // No new events, nothing to do.
        return true;
    }

    // This `PushContext` is going to be used to compute whether an in-thread event
    // would trigger a mention.
    //
    // Of course, we're not interested in an in-thread event causing a mention,
    // because it's part of a thread we've subscribed to. So the
    // `PushContext` must not include the check for thread subscriptions (otherwise
    // it would be impossible to subscribe to new threads).

    let with_thread_subscriptions = false;

    let Some(push_context) = room
        .push_context_internal(with_thread_subscriptions)
        .await
        .inspect_err(|err| {
            warn!("Failed to get push context for threads: {err}");
        })
        .ok()
        .flatten()
    else {
        warn!("Missing push context for thread subscriptions.");
        return true;
    };

    let mut subscribe_up_to = None;

    // Find if there's an event that would trigger a mention for the current
    // user, iterating from the end of the new events towards the oldest, so we can
    // find the most recent event to subscribe to.
    for ev in new_events.rev() {
        if push_context.for_event(ev.raw()).await.into_iter().any(|action| action.should_notify()) {
            let Some(event_id) = ev.event_id() else {
                // Shouldn't happen.
                continue;
            };
            subscribe_up_to = Some(event_id);
            break;
        }
    }

    // And if we've found such a mention, subscribe to the thread up to this
    // event.
    if let Some(event_id) = subscribe_up_to {
        trace!(thread = %thread_root, up_to = %event_id, "found a new thread to subscribe to");
        if let Err(err) = room.subscribe_thread_if_needed(&thread_root, Some(event_id)).await {
            warn!(%err, "Failed to subscribe to thread");
        } else {
            let _ = thread_subscriber_sender.send(());
        }
    }

    true
}

/// Takes an [`Event`] and passes it to the [`RoomIndex`] of the
/// given room which will add/remove/edit an event in the index based on
/// the event type.
///
/// [`Event`]: matrix_sdk_base::event_cache::Event
/// [`RoomIndex`]: matrix_sdk_search::index::RoomIndex
#[cfg(feature = "experimental-search")]
#[instrument(skip_all)]
pub(super) async fn search_indexing_task(
    client: WeakClient,
    linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,
) {
    let mut linked_chunk_update_receiver = linked_chunk_update_sender.subscribe();

    loop {
        match linked_chunk_update_receiver.recv().await {
            Ok(room_ec_lc_update) => {
                let OwnedLinkedChunkId::Room(room_id) = room_ec_lc_update.linked_chunk_id.clone()
                else {
                    trace!("Received non-room updates, ignoring.");
                    continue;
                };

                let mut timeline_events = room_ec_lc_update.events().peekable();

                if timeline_events.peek().is_none() {
                    continue;
                }

                let Some(client) = client.get() else {
                    trace!("Client is shutting down, not spawning thread subscriber task");
                    return;
                };

                let maybe_room_cache = client.event_cache().for_room(&room_id).await;
                let Ok((room_cache, _drop_handles)) = maybe_room_cache else {
                    warn!(for_room = %room_id, "Failed to get RoomEventCache: {maybe_room_cache:?}");
                    continue;
                };

                let maybe_room = client.get_room(&room_id);
                let Some(room) = maybe_room else {
                    warn!(get_room = %room_id, "Failed to get room while indexing: {maybe_room:?}");
                    continue;
                };
                let redaction_rules = room.clone_info().room_version_rules_or_default().redaction;

                let mut search_index_guard = client.search_index().lock().await;

                if let Err(err) = search_index_guard
                    .bulk_handle_timeline_event(
                        timeline_events,
                        &room_cache,
                        &room_id,
                        &redaction_rules,
                    )
                    .await
                {
                    error!("Failed to handle events for indexing: {err}")
                }
            }
            Err(RecvError::Closed) => {
                debug!(
                    "Linked chunk update channel has been closed, exiting thread subscriber task"
                );
                break;
            }
            Err(RecvError::Lagged(num_skipped)) => {
                warn!(num_skipped, "Lagged behind linked chunk updates");
            }
        }
    }
}

// MatrixMockServer et al. aren't available on wasm.
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use std::time::Duration;

    use matrix_sdk_base::sleep::sleep;
    use matrix_sdk_test::{BOB, JoinedRoomBuilder, async_test, event_factory::EventFactory};
    use ruma::{event_id, room_id};
    use tokio::sync::mpsc;

    use crate::{
        assert_let_timeout,
        event_cache::{
            EventsOrigin, RoomEventCacheUpdate, tasks::BackgroundRequest::PaginateRoomBackwards,
        },
        test_utils::mocks::{MatrixMockServer, RoomMessagesResponseTemplate},
    };

    impl super::super::EventCache {
        fn background_requests_sender(&self) -> Option<mpsc::Sender<super::BackgroundRequest>> {
            self.inner.background_requests_sender.get().cloned()
        }
    }

    /// Test that we can send background requests and trigger room paginations.
    #[async_test]
    async fn test_background_room_paginations() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let event_cache = client.event_cache();
        event_cache.config_mut().experimental_auto_backpagination = true;
        event_cache.subscribe().unwrap();

        let room_id = room_id!("!omelette:fromage.fr");
        let f = EventFactory::new().room(room_id).sender(*BOB);

        let room = server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id)
                    .set_timeline_limited()
                    .set_timeline_prev_batch("prev_batch"),
            )
            .await;

        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Starting with an empty, inactive room,
        let (room_events, mut room_cache_updates) = room_event_cache.subscribe().await.unwrap();
        assert!(room_events.is_empty());
        assert!(room_cache_updates.is_empty());

        // Set up the mock for /messages,
        server
            .mock_room_messages()
            .ok(RoomMessagesResponseTemplate::default().events(vec![
                f.text_msg("comté").event_id(event_id!("$2")),
                f.text_msg("beaufort").event_id(event_id!("$1")),
            ]))
            .mock_once()
            .mount()
            .await;

        // Send a request for a background pagination,
        let sender = event_cache.background_requests_sender().unwrap();
        sender.send(PaginateRoomBackwards { room_id: room_id.to_owned() }).await.unwrap();

        // The room pagination happens in the background.
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents(update)) = room_cache_updates.recv()
        );
        assert_eq!(update.diffs.len(), 1);

        assert_eq!(update.origin, EventsOrigin::Pagination);

        let mut room_events = room_events.into();
        for diff in update.diffs {
            diff.apply(&mut room_events);
        }

        assert_eq!(room_events.len(), 2);
        assert_eq!(room_events[0].event_id().unwrap(), event_id!("$1"));
        assert_eq!(room_events[1].event_id().unwrap(), event_id!("$2"));

        // And there's no more updates.
        assert!(room_cache_updates.is_empty());
    }

    /// Test that the credit system works.
    #[async_test]
    async fn test_room_pagination_respects_credits_system() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;

        let event_cache = client.event_cache();
        event_cache.config_mut().experimental_auto_backpagination = true;

        // Only allow 1 background pagination per room, to test that the credit system
        // is properly taken into account.
        event_cache.config_mut().room_pagination_per_room_credit = 1;
        event_cache.subscribe().unwrap();

        let room_id = room_id!("!omelette:fromage.fr");
        let f = EventFactory::new().room(room_id).sender(*BOB);

        let room = server.sync_joined_room(&client, room_id).await;
        let (room_event_cache, _drop_handles) = room.event_cache().await.unwrap();

        // Starting with an empty, inactive room,
        let (room_events, mut room_cache_updates) = room_event_cache.subscribe().await.unwrap();
        assert!(room_events.is_empty());
        assert!(room_cache_updates.is_empty());

        server
            .sync_room(
                &client,
                JoinedRoomBuilder::new(room_id)
                    .set_timeline_limited()
                    .set_timeline_prev_batch("prev_batch"),
            )
            .await;

        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents(_)) = room_cache_updates.recv()
        );

        // Set up the mock for /messages, so that it returns another prev-batch token,
        server
            .mock_room_messages()
            .match_from("prev_batch")
            .ok(RoomMessagesResponseTemplate::default()
                .events(vec![
                    f.text_msg("comté").event_id(event_id!("$2")),
                    f.text_msg("beaufort").event_id(event_id!("$1")),
                ])
                .end_token("prev_batch_2"))
            .mock_once()
            .mount()
            .await;

        // Send a request for a background pagination,
        let sender = event_cache.background_requests_sender().unwrap();
        sender.send(PaginateRoomBackwards { room_id: room_id.to_owned() }).await.unwrap();

        // The room pagination happens in the background.
        assert_let_timeout!(
            Ok(RoomEventCacheUpdate::UpdateTimelineEvents(update)) = room_cache_updates.recv()
        );
        assert_eq!(update.diffs.len(), 1);

        assert_eq!(update.origin, EventsOrigin::Pagination);

        let mut room_events = room_events.into();
        for diff in update.diffs {
            diff.apply(&mut room_events);
        }

        assert_eq!(room_events.len(), 2);
        assert_eq!(room_events[0].event_id().unwrap(), event_id!("$1"));
        assert_eq!(room_events[1].event_id().unwrap(), event_id!("$2"));

        // And there's no more updates yet.
        assert!(room_cache_updates.is_empty());

        // One can send another request to back-paginate…
        sender.send(PaginateRoomBackwards { room_id: room_id.to_owned() }).await.unwrap();

        sleep(Duration::from_millis(300)).await;
        // But it doesn't happen, because we don't have enough credits for automatic
        // backpagination.
        assert!(room_cache_updates.is_empty());

        // We can still manually backpaginate with success, though.
        server
            .mock_room_messages()
            .match_from("prev_batch_2")
            .ok(RoomMessagesResponseTemplate::default())
            .mock_once()
            .mount()
            .await;

        let outcome = room_event_cache.pagination().run_backwards_once(30).await.unwrap();
        assert!(outcome.reached_start);
    }
}
