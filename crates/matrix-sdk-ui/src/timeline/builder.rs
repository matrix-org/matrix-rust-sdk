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

use std::{collections::BTreeSet, sync::Arc};

use futures_util::{pin_mut, StreamExt};
use matrix_sdk::{
    event_cache::{EventsOrigin, RoomEventCacheUpdate},
    executor::spawn,
    Room,
};
use ruma::{events::AnySyncTimelineEvent, RoomVersionId};
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, info_span, trace, warn, Instrument, Span};

#[cfg(feature = "e2e-encryption")]
use super::to_device::{handle_forwarded_room_key_event, handle_room_key_event};
use super::{
    inner::{TimelineInner, TimelineInnerSettings},
    Error, Timeline, TimelineDropHandle, TimelineFocus,
};
use crate::{
    timeline::{event_item::RemoteEventOrigin, inner::TimelineEnd},
    unable_to_decrypt_hook::UtdHookManager,
};

/// Builder that allows creating and configuring various parts of a
/// [`Timeline`].
#[must_use]
#[derive(Debug)]
pub struct TimelineBuilder {
    room: Room,
    settings: TimelineInnerSettings,
    focus: TimelineFocus,

    /// An optional hook to call whenever we run into an unable-to-decrypt or a
    /// late-decryption event.
    unable_to_decrypt_hook: Option<Arc<UtdHookManager>>,

    /// An optional prefix for internal IDs.
    internal_id_prefix: Option<String>,
}

impl TimelineBuilder {
    pub(super) fn new(room: &Room) -> Self {
        Self {
            room: room.clone(),
            settings: TimelineInnerSettings::default(),
            unable_to_decrypt_hook: None,
            focus: TimelineFocus::Live,
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
        F: Fn(&AnySyncTimelineEvent, &RoomVersionId) -> bool + Send + Sync + 'static,
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
        let (_, mut event_subscriber) = room_event_cache.subscribe().await?;

        let is_live = matches!(focus, TimelineFocus::Live);
        let is_pinned_events = matches!(focus, TimelineFocus::PinnedEvents { .. });
        let is_room_encrypted =
            room.is_encrypted().await.map_err(|_| Error::UnknownEncryptionState)?;

        let inner = TimelineInner::new(
            room,
            focus.clone(),
            internal_id_prefix,
            unable_to_decrypt_hook,
            is_room_encrypted,
        )
        .with_settings(settings);

        let has_events = inner.init_focus(&room_event_cache).await?;

        let room = inner.room();
        let client = room.client();

        let pinned_events_join_handle = if is_pinned_events {
            let mut pinned_event_ids_stream = room.pinned_event_ids_stream();
            Some(spawn({
                let inner = inner.clone();
                async move {
                    while pinned_event_ids_stream.next().await.is_some() {
                        if let Ok(events) = inner.reload_pinned_events().await {
                            inner
                                .replace_with_initial_remote_events(
                                    events,
                                    RemoteEventOrigin::Pagination,
                                )
                                .await;
                        }
                    }
                }
            }))
        } else {
            None
        };

        let room_update_join_handle = spawn({
            let room_event_cache = room_event_cache.clone();
            let inner = inner.clone();

            let span =
                info_span!(parent: Span::none(), "room_update_handler", room_id = ?room.room_id());
            span.follows_from(Span::current());

            async move {
                trace!("Spawned the event subscriber task.");

                loop {
                    trace!("Waiting for an event.");

                    let update = match event_subscriber.recv().await {
                        Ok(up) => up,
                        Err(RecvError::Closed) => break,
                        Err(RecvError::Lagged(num_skipped)) => {
                            warn!(
                                num_skipped,
                                "Lagged behind event cache updates, resetting timeline"
                            );

                            // The updates might have lagged, but the room event cache might have
                            // events, so retrieve them and add them back again to the timeline,
                            // after clearing it.
                            //
                            // If we can't get a handle on the room cache's events, just clear the
                            // current timeline.
                            match room_event_cache.subscribe().await {
                                Ok((events, _)) => {
                                    inner.replace_with_initial_remote_events(events, RemoteEventOrigin::Sync).await;
                                }
                                Err(err) => {
                                    warn!("Error when re-inserting initial events into the timeline: {err}");
                                    inner.clear().await;
                                }
                            }

                            continue;
                        }
                    };

                    match update {
                        RoomEventCacheUpdate::MoveReadMarkerTo { event_id } => {
                            trace!(target = %event_id, "Handling fully read marker.");
                            inner.handle_fully_read_marker(event_id).await;
                        }

                        RoomEventCacheUpdate::Clear => {
                            if !inner.is_live().await {
                                // Ignore a clear for a timeline not in the live mode; the
                                // focused-on-event mode doesn't add any new items to the timeline
                                // anyways.
                                continue;
                            }

                            trace!("Clearing the timeline.");
                            inner.clear().await;
                        }

                        RoomEventCacheUpdate::AddTimelineEvents { events, origin } => {
                            trace!("Received new timeline events.");

                            // Note: we deliberately choose to not handle
                            // updates/reactions/redactions for pinned events.
                            if !is_pinned_events {
                                inner.add_events_at(
                                    events,
                                    TimelineEnd::Back,
                                    match origin {
                                        EventsOrigin::Sync => RemoteEventOrigin::Sync,
                                    }
                                ).await;
                            }
                        }

                        RoomEventCacheUpdate::AddEphemeralEvents { events } => {
                            trace!("Received new ephemeral events from sync.");

                            // TODO: (bnjbvr) ephemeral should be handled by the event cache.
                            inner.handle_ephemeral_events(events).await;
                        }

                        RoomEventCacheUpdate::UpdateMembers { ambiguity_changes } => {
                            if !ambiguity_changes.is_empty() {
                                let member_ambiguity_changes = ambiguity_changes
                                    .values()
                                    .flat_map(|change| change.user_ids())
                                    .collect::<BTreeSet<_>>();
                                inner.force_update_sender_profiles(&member_ambiguity_changes).await;
                            }
                        }
                    }
                }
            }
            .instrument(span)
        });

        let local_echo_listener_handle = if is_live {
            let timeline = inner.clone();
            let (local_echoes, mut listener) = room.send_queue().subscribe().await?;

            Some(spawn({
                // Handles existing local echoes first.
                for echo in local_echoes {
                    timeline.handle_local_echo(echo).await;
                }

                let span = info_span!(parent: Span::none(), "local_echo_handler", room_id = ?room.room_id());
                span.follows_from(Span::current());

                // React to future local echoes too.
                async move {
                    info!("spawned the local echo handler!");

                    loop {
                        match listener.recv().await {
                            Ok(update) => timeline.handle_room_send_queue_update(update).await,

                            Err(RecvError::Lagged(num_missed)) => {
                                warn!("missed {num_missed} local echoes, ignoring those missed");
                            }

                            Err(RecvError::Closed) => {
                                info!("channel closed, exiting the local echo handler");
                                break;
                            }
                        }
                    }
                }
                .instrument(span)
            }))
        } else {
            None
        };

        // Not using room.add_event_handler here because RoomKey events are
        // to-device events that are not received in the context of a room.

        #[cfg(feature = "e2e-encryption")]
        let room_key_handle = client
            .add_event_handler(handle_room_key_event(inner.clone(), room.room_id().to_owned()));
        #[cfg(feature = "e2e-encryption")]
        let forwarded_room_key_handle = client.add_event_handler(handle_forwarded_room_key_event(
            inner.clone(),
            room.room_id().to_owned(),
        ));

        let handles = vec![
            #[cfg(feature = "e2e-encryption")]
            room_key_handle,
            #[cfg(feature = "e2e-encryption")]
            forwarded_room_key_handle,
        ];

        let room_key_from_backups_join_handle = {
            let inner = inner.clone();
            let room_id = inner.room().room_id();

            let stream = client.encryption().backups().room_keys_for_room_stream(room_id);

            spawn(async move {
                pin_mut!(stream);

                while let Some(update) = stream.next().await {
                    let room = inner.room();

                    match update {
                        Ok(info) => {
                            let mut session_ids = BTreeSet::new();

                            for set in info.into_values() {
                                session_ids.extend(set);
                            }

                            inner.retry_event_decryption(room, Some(session_ids)).await;
                        }
                        // We lagged, so retry every event.
                        Err(_) => inner.retry_event_decryption(room, None).await,
                    }
                }
            })
        };

        let timeline = Timeline {
            inner,
            event_cache: room_event_cache,
            drop_handle: Arc::new(TimelineDropHandle {
                client,
                event_handler_handles: handles,
                room_update_join_handle,
                pinned_events_join_handle,
                room_key_from_backups_join_handle,
                local_echo_listener_handle,
                _event_cache_drop_handle: event_cache_drop,
            }),
        };

        #[cfg(feature = "e2e-encryption")]
        if has_events {
            // The events we're injecting might be encrypted events, but we might
            // have received the room key to decrypt them while nobody was listening to the
            // `m.room_key` event, let's retry now.
            timeline.retry_decryption_for_all_events().await;
        }

        Ok(timeline)
    }
}
