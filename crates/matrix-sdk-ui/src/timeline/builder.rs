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

use std::sync::Arc;

use matrix_sdk::{Room, executor::spawn};
use matrix_sdk_base::{SendOutsideWasm, SyncOutsideWasm};
use ruma::{events::AnySyncTimelineEvent, room_version_rules::RoomVersionRules};
use tracing::{Instrument, Span, info_span};

use super::{
    DateDividerMode, Error, Timeline, TimelineDropHandle, TimelineFocus,
    controller::{TimelineController, TimelineSettings},
};
use crate::{
    timeline::{
        TimelineReadReceiptTracking,
        controller::spawn_crypto_tasks,
        tasks::{
            pinned_events_task, room_event_cache_updates_task, room_send_queue_update_task,
            thread_updates_task,
        },
    },
    unable_to_decrypt_hook::UtdHookManager,
};

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
    /// By default, the focus for a timeline is to be "live" (i.e. it will
    /// listen to sync and append this room's events in real-time, and it'll be
    /// able to back-paginate older events), and show all events (including
    /// events in threads). Look at [`TimelineFocus`] for other options.
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

    /// Choose when to insert the date separators, either in between each day
    /// or each month.
    pub fn with_date_divider_mode(mut self, mode: DateDividerMode) -> Self {
        self.settings.date_divider_mode = mode;
        self
    }

    /// Choose whether to enable tracking of the fully-read marker and the read
    /// receipts and on which event types.
    pub fn track_read_marker_and_receipts(mut self, tracking: TimelineReadReceiptTracking) -> Self {
        self.settings.track_read_receipts = tracking;
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
        F: Fn(&AnySyncTimelineEvent, &RoomVersionRules) -> bool
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
            track_read_receipts = ?self.settings.track_read_receipts,
        )
    )]
    pub async fn build(self) -> Result<Timeline, Error> {
        let Self { room, settings, unable_to_decrypt_hook, focus, internal_id_prefix } = self;

        // Subscribe the event cache to sync responses, in case we hadn't done it yet.
        room.client().event_cache().subscribe()?;

        let (room_event_cache, event_cache_drop) = room.event_cache().await?;
        let (_, event_subscriber) = room_event_cache.subscribe().await?;

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
            settings,
        );

        let has_events = controller.init_focus(&focus, &room_event_cache).await?;

        let pinned_events_join_handle = if matches!(focus, TimelineFocus::PinnedEvents { .. }) {
            Some(spawn(pinned_events_task(room.pinned_event_ids_stream(), controller.clone())))
        } else {
            None
        };

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

        let thread_update_join_handle =
            if let TimelineFocus::Thread { root_event_id: root } = &focus {
                Some({
                    let span = info_span!(
                        parent: Span::none(),
                        "thread_live_update_handler",
                        room_id = ?room.room_id(),
                        focus = focus.debug_string(),
                        prefix = internal_id_prefix
                    );
                    span.follows_from(Span::current());

                    // Note: must be done here *before* spawning the task, to avoid race conditions
                    // with event cache updates happening in the background.
                    let (_events, receiver) =
                        room_event_cache.subscribe_to_thread(root.clone()).await?;

                    spawn(
                        thread_updates_task(
                            receiver,
                            room_event_cache.clone(),
                            controller.clone(),
                            root.clone(),
                        )
                        .instrument(span),
                    )
                })
            } else {
                None
            };

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

        let crypto_drop_handles = spawn_crypto_tasks(controller.clone()).await;

        let timeline = Timeline {
            controller,
            event_cache: room_event_cache,
            drop_handle: Arc::new(TimelineDropHandle {
                _crypto_drop_handles: crypto_drop_handles,
                room_update_join_handle,
                thread_update_join_handle,
                pinned_events_join_handle,
                local_echo_listener_handle,
                _event_cache_drop_handle: event_cache_drop,
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
