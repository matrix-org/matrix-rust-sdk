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

use std::{collections::HashMap, sync::Arc};

use eyeball::Subscriber;
use matrix_sdk_base::{
    linked_chunk::OwnedLinkedChunkId, serde_helpers::extract_thread_root_from_content,
    sync::RoomUpdates,
};
use ruma::{OwnedEventId, OwnedTransactionId};
use tokio::{
    select,
    sync::broadcast::{Receiver, Sender, error::RecvError},
};
use tracing::{Instrument as _, Span, debug, error, info, info_span, instrument, trace, warn};

use super::{EventCacheError, EventCacheInner, RoomEventCacheLinkedChunkUpdate};
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

/// Handle [`SendQueueUpdate`] and [`RoomEventCacheLinkedChunkUpdate`] to update
/// the threads, for a thread the user was not subscribed to.
#[instrument(skip_all)]
pub(super) async fn thread_subscriber_task(
    client: WeakClient,
    linked_chunk_update_sender: Sender<RoomEventCacheLinkedChunkUpdate>,
    thread_subscriber_sender: Sender<()>,
) {
    let mut send_q_rx = if let Some(client) = client.get() {
        if !client.enabled_thread_subscriptions() {
            trace!("Thread subscriptions are not enabled, not spawning thread subscriber task");
            return;
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
