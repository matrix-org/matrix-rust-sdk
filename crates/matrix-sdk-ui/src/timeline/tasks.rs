// Copyright 2025 The Matrix.org Foundation C.I.C.
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

//! Long-lived tasks for the timeline.

use std::collections::BTreeSet;

use futures_core::Stream;
use futures_util::pin_mut;
use matrix_sdk::{
    event_cache::{
        EventsOrigin, RoomEventCache, RoomEventCacheSubscriber, RoomEventCacheUpdate,
        ThreadEventCacheUpdate,
    },
    send_queue::RoomSendQueueUpdate,
};
use ruma::OwnedEventId;
use tokio::sync::broadcast::{Receiver, error::RecvError};
use tokio_stream::StreamExt as _;
use tracing::{error, instrument, trace, warn};

use crate::timeline::{TimelineController, TimelineFocus, event_item::RemoteEventOrigin};

/// Long-lived task, in the pinned events focus mode, that updates the timeline
/// after any changes in the pinned events.
#[instrument(
    skip_all,
    fields(
        room_id = %timeline_controller.room().room_id(),
    )
)]
pub(in crate::timeline) async fn pinned_events_task<S>(
    pinned_event_ids_stream: S,
    timeline_controller: TimelineController,
) where
    S: Stream<Item = Vec<OwnedEventId>>,
{
    pin_mut!(pinned_event_ids_stream);

    while pinned_event_ids_stream.next().await.is_some() {
        trace!("received a pinned events update");

        match timeline_controller.reload_pinned_events().await {
            Ok(Some(events)) => {
                trace!("successfully reloaded pinned events");
                timeline_controller
                    .replace_with_initial_remote_events(events, RemoteEventOrigin::Pagination)
                    .await;
            }

            Ok(None) => {
                // The list of pinned events hasn't changed since the previous
                // time.
            }

            Err(err) => {
                warn!("Failed to reload pinned events: {err}");
            }
        }
    }
}

/// For a thread-focused timeline, a long-lived task that will listen to the
/// underlying thread updates.
pub(in crate::timeline) async fn thread_updates_task(
    mut receiver: Receiver<ThreadEventCacheUpdate>,
    room_event_cache: RoomEventCache,
    timeline_controller: TimelineController,
    root: OwnedEventId,
) {
    trace!("Spawned the thread event subscriber task.");

    loop {
        trace!("Waiting for an event.");

        let update = match receiver.recv().await {
            Ok(up) => up,
            Err(RecvError::Closed) => break,
            Err(RecvError::Lagged(num_skipped)) => {
                warn!(num_skipped, "Lagged behind event cache updates, resetting timeline");

                // The updates might have lagged, but the room event cache might
                // have events, so retrieve them and add them back again to the
                // timeline, after clearing it.
                let (initial_events, _) =
                    match room_event_cache.subscribe_to_thread(root.clone()).await {
                        Ok(values) => values,
                        Err(err) => {
                            error!(?err, "Subscribing to thread failed");
                            break;
                        }
                    };

                timeline_controller
                    .replace_with_initial_remote_events(initial_events, RemoteEventOrigin::Cache)
                    .await;

                continue;
            }
        };

        trace!("Received new timeline events diffs");

        let origin = match update.origin {
            EventsOrigin::Sync => RemoteEventOrigin::Sync,
            EventsOrigin::Pagination => RemoteEventOrigin::Pagination,
            EventsOrigin::Cache => RemoteEventOrigin::Cache,
        };

        let has_diffs = !update.diffs.is_empty();

        timeline_controller.handle_remote_events_with_diffs(update.diffs, origin).await;

        if has_diffs && matches!(origin, RemoteEventOrigin::Cache) {
            timeline_controller.retry_event_decryption(None).await;
        }
    }

    trace!("Thread event subscriber task finished.");
}

/// Long-lived task that forwards the [`RoomEventCacheUpdate`]s (remote echoes)
/// to the timeline.
pub(in crate::timeline) async fn room_event_cache_updates_task(
    room_event_cache: RoomEventCache,
    timeline_controller: TimelineController,
    mut room_event_cache_subscriber: RoomEventCacheSubscriber,
    timeline_focus: TimelineFocus,
) {
    trace!("Spawned the event subscriber task.");

    loop {
        trace!("Waiting for an event.");

        let update = match room_event_cache_subscriber.recv().await {
            Ok(up) => up,
            Err(RecvError::Closed) => break,
            Err(RecvError::Lagged(num_skipped)) => {
                warn!(num_skipped, "Lagged behind event cache updates, resetting timeline");

                // The updates might have lagged, but the room event cache might have
                // events, so retrieve them and add them back again to the timeline,
                // after clearing it.
                let initial_events = match room_event_cache.events().await {
                    Ok(initial_events) => initial_events,
                    Err(err) => {
                        error!(
                            ?err,
                            "Failed to replace the initial remote events in the event cache"
                        );
                        break;
                    }
                };

                timeline_controller
                    .replace_with_initial_remote_events(initial_events, RemoteEventOrigin::Cache)
                    .await;

                continue;
            }
        };

        match update {
            RoomEventCacheUpdate::MoveReadMarkerTo { event_id } => {
                trace!(target = %event_id, "Handling fully read marker.");
                timeline_controller.handle_fully_read_marker(event_id).await;
            }

            RoomEventCacheUpdate::UpdateTimelineEvents { diffs, origin } => {
                trace!("Received new timeline events diffs");
                let origin = match origin {
                    EventsOrigin::Sync => RemoteEventOrigin::Sync,
                    EventsOrigin::Pagination => RemoteEventOrigin::Pagination,
                    EventsOrigin::Cache => RemoteEventOrigin::Cache,
                };

                let has_diffs = !diffs.is_empty();

                if matches!(timeline_focus, TimelineFocus::Live { .. }) {
                    timeline_controller.handle_remote_events_with_diffs(diffs, origin).await;
                } else {
                    // Only handle the remote aggregation for a non-live timeline.
                    timeline_controller.handle_remote_aggregations(diffs, origin).await;
                }

                if has_diffs && matches!(origin, RemoteEventOrigin::Cache) {
                    timeline_controller.retry_event_decryption(None).await;
                }
            }

            RoomEventCacheUpdate::AddEphemeralEvents { events } => {
                trace!("Received new ephemeral events from sync.");

                // TODO: (bnjbvr) ephemeral should be handled by the event cache.
                timeline_controller.handle_ephemeral_events(events).await;
            }

            RoomEventCacheUpdate::UpdateMembers { ambiguity_changes } => {
                if !ambiguity_changes.is_empty() {
                    let member_ambiguity_changes = ambiguity_changes
                        .values()
                        .flat_map(|change| change.user_ids())
                        .collect::<BTreeSet<_>>();
                    timeline_controller
                        .force_update_sender_profiles(&member_ambiguity_changes)
                        .await;
                }
            }
        }
    }
}

/// Long-lived task that forwards [`RoomSendQueueUpdate`]s (local echoes) to the
/// timeline.
pub(in crate::timeline) async fn room_send_queue_update_task(
    mut send_queue_stream: Receiver<RoomSendQueueUpdate>,
    timeline_controller: TimelineController,
) {
    trace!("spawned the local echo task!");

    loop {
        match send_queue_stream.recv().await {
            Ok(update) => timeline_controller.handle_room_send_queue_update(update).await,

            Err(RecvError::Lagged(num_missed)) => {
                warn!("missed {num_missed} local echoes, ignoring those missed");
            }

            Err(RecvError::Closed) => {
                trace!("channel closed, exiting the local echo handler");
                break;
            }
        }
    }
}
