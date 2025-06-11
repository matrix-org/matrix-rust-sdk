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

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use futures_core::Stream;
use futures_util::{pin_mut, StreamExt};
use matrix_sdk::{
    crypto::store::types::RoomKeyInfo,
    encryption::backups::BackupState,
    event_cache::{EventsOrigin, RoomEventCache, RoomEventCacheListener, RoomEventCacheUpdate},
    executor::spawn,
    send_queue::RoomSendQueueUpdate,
    Room,
};
use matrix_sdk_base::{SendOutsideWasm, SyncOutsideWasm};
use ruma::{events::AnySyncTimelineEvent, OwnedEventId, RoomVersionId};
use tokio::sync::broadcast::{error::RecvError, Receiver};
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use tracing::{info_span, instrument, trace, warn, Instrument, Span};

use super::{
    controller::{TimelineController, TimelineSettings},
    to_device::{handle_forwarded_room_key_event, handle_room_key_event},
    DateDividerMode, Error, Timeline, TimelineDropHandle, TimelineFocus,
};
use crate::{timeline::event_item::RemoteEventOrigin, unable_to_decrypt_hook::UtdHookManager};

/// Builder that allows creating and configuring various parts of a
/// [`Timeline`].
#[must_use]
#[derive(Debug)]
pub struct TimelineBuilder {
    room: Room,
    settings: TimelineSettings,
    focus: TimelineFocus,

    /// An optional hook to call whenever we run into an unable-to-decrypt or a
    /// late-decryption event.
    unable_to_decrypt_hook: Option<Arc<UtdHookManager>>,

    /// An optional prefix for internal IDs.
    internal_id_prefix: Option<String>,
}

impl TimelineBuilder {
    pub fn new(room: &Room) -> Self {
        Self {
            room: room.clone(),
            settings: TimelineSettings::default(),
            unable_to_decrypt_hook: None,
            focus: TimelineFocus::Live { hide_threaded_events: false },
            internal_id_prefix: None,
        }
    }

    /// Sets up the initial focus for this timeline.
    ///
    /// This can be changed later on while the timeline is alive.
    pub fn with_focus(mut self, focus: TimelineFocus) -> Self {
        self.focus = focus;
        self
    }

    /// Sets up a hook to catch unable-to-decrypt (UTD) events for the timeline
    /// we're building.
    ///
    /// If it was previously set before, will overwrite the previous one.
    pub fn with_unable_to_decrypt_hook(mut self, hook: Arc<UtdHookManager>) -> Self {
        self.unable_to_decrypt_hook = Some(hook);
        self
    }

    /// Sets the internal id prefix for this timeline.
    ///
    /// The prefix will be prepended to any internal ID using when generating
    /// timeline IDs for this timeline.
    pub fn with_internal_id_prefix(mut self, prefix: String) -> Self {
        self.internal_id_prefix = Some(prefix);
        self
    }

    /// Chose when to insert the date separators, either in between each day
    /// or each month.
    pub fn with_date_divider_mode(mut self, mode: DateDividerMode) -> Self {
        self.settings.date_divider_mode = mode;
        self
    }

    /// Enable tracking of the fully-read marker and the read receipts on the
    /// timeline.
    pub fn track_read_marker_and_receipts(mut self) -> Self {
        self.settings.track_read_receipts = true;
        self
    }

    /// Use the given filter to choose whether to add events to the timeline.
    ///
    /// # Arguments
    ///
    /// * `filter` - A function that takes a deserialized event, and should
    ///   return `true` if the event should be added to the `Timeline`.
    ///
    /// If this is not overridden, the timeline uses the default filter that
    /// only allows events that are materialized into a `Timeline` item. For
    /// instance, reactions and edits don't get their own timeline item (as
    /// they affect another existing one), so they're "filtered out" to
    /// reflect that.
    ///
    /// You can use the default event filter with
    /// [`crate::timeline::default_event_filter`] so as to chain it with
    /// your own event filter, if you want to avoid situations where a read
    /// receipt would be attached to an event that doesn't get its own
    /// timeline item.
    ///
    /// Note that currently:
    ///
    /// - Not all event types have a representation as a `TimelineItem` so these
    ///   are not added no matter what the filter returns.
    /// - It is not possible to filter out `m.room.encrypted` events (otherwise
    ///   they couldn't be decrypted when the appropriate room key arrives).
    pub fn event_filter<F>(mut self, filter: F) -> Self
    where
        F: Fn(&AnySyncTimelineEvent, &RoomVersionId) -> bool
            + SendOutsideWasm
            + SyncOutsideWasm
            + 'static,
    {
        self.settings.event_filter = Arc::new(filter);
        self
    }

    /// Whether to add events that failed to deserialize to the timeline.
    ///
    /// Defaults to `true`.
    pub fn add_failed_to_parse(mut self, add: bool) -> Self {
        self.settings.add_failed_to_parse = add;
        self
    }

    /// Create a [`Timeline`] with the options set on this builder.
    #[tracing::instrument(
        skip(self),
        fields(
            room_id = ?self.room.room_id(),
            track_read_receipts = self.settings.track_read_receipts,
        )
    )]
    pub async fn build(self) -> Result<Timeline, Error> {
        let Self { room, settings, unable_to_decrypt_hook, focus, internal_id_prefix } = self;

        let client = room.client();
        let event_cache = client.event_cache();

        // Subscribe the event cache to sync responses, in case we hadn't done it yet.
        event_cache.subscribe()?;

        let (room_event_cache, event_cache_drop) = room.event_cache().await?;
        let (_, event_subscriber) = room_event_cache.subscribe().await;

        let is_room_encrypted = room
            .latest_encryption_state()
            .await
            .map(|state| state.is_encrypted())
            .ok()
            .unwrap_or_default();

        let controller = TimelineController::new(
            room.clone(),
            focus.clone(),
            internal_id_prefix.clone(),
            unable_to_decrypt_hook,
            is_room_encrypted,
        )
        .with_settings(settings);

        let has_events = controller.init_focus(&room_event_cache).await?;

        let pinned_events_join_handle = if matches!(focus, TimelineFocus::PinnedEvents { .. }) {
            Some(spawn(pinned_events_task(room.pinned_event_ids_stream(), controller.clone())))
        } else {
            None
        };

        let encryption_changes_handle = spawn({
            let inner = controller.clone();
            async move {
                inner.handle_encryption_state_changes().await;
            }
        });

        let room_update_join_handle = spawn({
            let span = info_span!(
                parent: Span::none(),
                "live_update_handler",
                room_id = ?room.room_id(),
                focus = focus.debug_string(),
                prefix = internal_id_prefix
            );
            span.follows_from(Span::current());

            room_event_cache_updates_task(
                room_event_cache.clone(),
                controller.clone(),
                event_subscriber,
                focus.clone(),
            )
            .instrument(span)
        });

        let local_echo_listener_handle = {
            let timeline_controller = controller.clone();
            let (local_echoes, send_queue_stream) = room.send_queue().subscribe().await?;

            spawn({
                // Handles existing local echoes first.
                for echo in local_echoes {
                    timeline_controller.handle_local_echo(echo).await;
                }

                let span = info_span!(
                    parent: Span::none(),
                    "local_echo_handler",
                    room_id = ?room.room_id(),
                    focus = focus.debug_string(),
                    prefix = internal_id_prefix
                );
                span.follows_from(Span::current());

                room_send_queue_update_task(send_queue_stream, timeline_controller).instrument(span)
            })
        };

        let room_key_handle = client.add_event_handler(handle_room_key_event(
            controller.clone(),
            room.room_id().to_owned(),
        ));

        let forwarded_room_key_handle = client.add_event_handler(handle_forwarded_room_key_event(
            controller.clone(),
            room.room_id().to_owned(),
        ));

        let event_handlers = vec![room_key_handle, forwarded_room_key_handle];

        // Not using room.add_event_handler here because RoomKey events are
        // to-device events that are not received in the context of a room.

        let room_key_from_backups_join_handle = spawn(room_keys_from_backups_task(
            client.encryption().backups().room_keys_for_room_stream(controller.room().room_id()),
            controller.clone(),
        ));

        let room_key_backup_enabled_join_handle = spawn(backup_states_task(
            client.encryption().backups().state_stream(),
            controller.clone(),
        ));

        // TODO: Technically, this should be the only stream we need to listen to get
        // notified when we should retry to decrypt an event. We sadly can't do that,
        // since the cross-process support kills the `OlmMachine` which then in
        // turn kills this stream. Once this is solved remove all the other ways we
        // listen for room keys.
        let room_keys_received_join_handle = {
            spawn(room_key_received_task(
                client.encryption().room_keys_received_stream().await.expect(
                    "We should be logged in by now, so we should have access to an `OlmMachine` \
                     to be able to listen to this stream",
                ),
                controller.clone(),
            ))
        };

        let timeline = Timeline {
            controller,
            event_cache: room_event_cache,
            drop_handle: Arc::new(TimelineDropHandle {
                client,
                event_handler_handles: event_handlers,
                room_update_join_handle,
                pinned_events_join_handle,
                room_key_from_backups_join_handle,
                room_key_backup_enabled_join_handle,
                room_keys_received_join_handle,
                local_echo_listener_handle,
                _event_cache_drop_handle: event_cache_drop,
                encryption_changes_handle,
            }),
        };

        if has_events {
            // The events we're injecting might be encrypted events, but we might
            // have received the room key to decrypt them while nobody was listening to the
            // `m.room_key` event, let's retry now.
            timeline.retry_decryption_for_all_events().await;
        }

        Ok(timeline)
    }
}

/// The task that handles the pinned event IDs updates.
#[instrument(
    skip_all,
    fields(
        room_id = %timeline_controller.room().room_id(),
    )
)]
async fn pinned_events_task<S>(pinned_event_ids_stream: S, timeline_controller: TimelineController)
where
    S: Stream<Item = Vec<OwnedEventId>>,
{
    pin_mut!(pinned_event_ids_stream);

    while pinned_event_ids_stream.next().await.is_some() {
        trace!("received a pinned events update");

        match timeline_controller.reload_pinned_events().await {
            Ok(Some(events)) => {
                trace!("successfully reloaded pinned events");
                timeline_controller
                    .replace_with_initial_remote_events(
                        events.into_iter(),
                        RemoteEventOrigin::Pagination,
                    )
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

/// The task that handles the [`RoomEventCacheUpdate`]s.
async fn room_event_cache_updates_task(
    room_event_cache: RoomEventCache,
    timeline_controller: TimelineController,
    mut event_subscriber: RoomEventCacheListener,
    timeline_focus: TimelineFocus,
) {
    trace!("Spawned the event subscriber task.");

    loop {
        trace!("Waiting for an event.");

        let update = match event_subscriber.recv().await {
            Ok(up) => up,
            Err(RecvError::Closed) => break,
            Err(RecvError::Lagged(num_skipped)) => {
                warn!(num_skipped, "Lagged behind event cache updates, resetting timeline");

                // The updates might have lagged, but the room event cache might have
                // events, so retrieve them and add them back again to the timeline,
                // after clearing it.
                let initial_events = room_event_cache.events().await;

                timeline_controller
                    .replace_with_initial_remote_events(
                        initial_events.into_iter(),
                        RemoteEventOrigin::Cache,
                    )
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

                if matches!(
                    timeline_focus,
                    TimelineFocus::Live { .. } | TimelineFocus::Thread { .. }
                ) {
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

/// The task that handles the [`RoomSendQueueUpdate`]s.
async fn room_send_queue_update_task(
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

/// The task that handles the room keys from backups.
async fn room_keys_from_backups_task<S>(stream: S, timeline_controller: TimelineController)
where
    S: Stream<Item = Result<BTreeMap<String, BTreeSet<String>>, BroadcastStreamRecvError>>,
{
    pin_mut!(stream);

    while let Some(update) = stream.next().await {
        match update {
            Ok(info) => {
                let mut session_ids = BTreeSet::new();

                for set in info.into_values() {
                    session_ids.extend(set);
                }

                timeline_controller.retry_event_decryption(Some(session_ids)).await;
            }
            // We lagged, so retry every event.
            Err(_) => timeline_controller.retry_event_decryption(None).await,
        }
    }
}

/// The task that handles the [`BackupState`] updates.
async fn backup_states_task<S>(backup_states_stream: S, timeline_controller: TimelineController)
where
    S: Stream<Item = Result<BackupState, BroadcastStreamRecvError>>,
{
    pin_mut!(backup_states_stream);

    while let Some(update) = backup_states_stream.next().await {
        match update {
            // If the backup got enabled, or we lagged and thus missed that the backup
            // might be enabled, retry to decrypt all the events. Please note, depending
            // on the backup download strategy, this might do two things under the
            // assumption that the backup contains the relevant room keys:
            //
            // 1. It will decrypt the events, if `BackupDownloadStrategy` has been set to `OneShot`.
            // 2. It will fail to decrypt the event, but try to download the room key to decrypt it
            //    if the `BackupDownloadStrategy` has been set to `AfterDecryptionFailure`.
            Ok(BackupState::Enabled) | Err(_) => {
                timeline_controller.retry_event_decryption(None).await;
            }
            // The other states aren't interesting since they are either still enabling
            // the backup or have the backup in the disabled state.
            Ok(
                BackupState::Unknown
                | BackupState::Creating
                | BackupState::Resuming
                | BackupState::Disabling
                | BackupState::Downloading
                | BackupState::Enabling,
            ) => (),
        }
    }
}

/// The task that handles the [`RoomKeyInfo`] updates.
async fn room_key_received_task<S>(
    room_keys_received_stream: S,
    timeline_controller: TimelineController,
) where
    S: Stream<Item = Result<Vec<RoomKeyInfo>, BroadcastStreamRecvError>>,
{
    pin_mut!(room_keys_received_stream);

    let room_id = timeline_controller.room().room_id();

    while let Some(room_keys) = room_keys_received_stream.next().await {
        let session_ids = match room_keys {
            Ok(room_keys) => {
                let session_ids: BTreeSet<String> = room_keys
                    .into_iter()
                    .filter(|info| info.room_id == room_id)
                    .map(|info| info.session_id)
                    .collect();

                Some(session_ids)
            }
            Err(BroadcastStreamRecvError::Lagged(missed_updates)) => {
                // We lagged, let's retry to decrypt anything we have, maybe something
                // was received.
                warn!(
                    missed_updates,
                    "The room keys stream has lagged, retrying to decrypt the whole timeline"
                );

                None
            }
        };

        timeline_controller.retry_event_decryption(session_ids).await;
    }
}
