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

use std::collections::{HashMap, HashSet};

use eyeball_im::VectorDiff;
use itertools::Itertools as _;
use matrix_sdk::{
    deserialized_responses::{
        ThreadSummary as SdkThreadSummary, ThreadSummaryStatus, TimelineEvent, TimelineEventKind,
        UnsignedEventLocation,
    },
    event_cache::TimelineGap,
};
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, OwnedUserId, UserId,
    events::{
        AnySyncTimelineEvent,
        receipt::{ReceiptThread, ReceiptType},
    },
    push::Action,
    serde::Raw,
};
use tracing::{debug, instrument, trace, warn};

use super::{
    super::{
        controller::ObservableItemsTransactionEntry,
        date_dividers::DateDividerAdjuster,
        event_handler::{Flow, TimelineEventContext, TimelineEventHandler, TimelineItemPosition},
        event_item::RemoteEventOrigin,
        traits::RoomDataProvider,
    },
    ObservableItems, ObservableItemsTransaction, TimelineMetadata, TimelineReadReceiptTracking,
    TimelineSettings,
    metadata::EventMeta,
};
use crate::timeline::{
    EmbeddedEvent, Profile, ThreadSummary, TimelineDetails, TimelineUniqueId, VirtualTimelineItem,
    controller::TimelineFocusKind,
    event_handler::{FailedToParseEvent, RemovedItem, TimelineAction},
};

pub(in crate::timeline) struct TimelineStateTransaction<'a, P: RoomDataProvider> {
    /// A vector transaction over the items themselves. Holds temporary state
    /// until committed.
    pub items: ObservableItemsTransaction<'a>,

    /// Number of items when the transaction has been created/has started.
    number_of_items_when_transaction_started: usize,

    /// A clone of the previous meta, that we're operating on during the
    /// transaction, and that will be committed to the previous meta location in
    /// [`Self::commit`].
    pub meta: TimelineMetadata,

    /// Pointer to the previous meta, only used during [`Self::commit`].
    previous_meta: &'a mut TimelineMetadata,

    /// The kind of focus of this timeline.
    pub focus: &'a TimelineFocusKind,

    /// Phantom data for type parameter.
    _phantom: std::marker::PhantomData<P>,
}

impl<'a, P: RoomDataProvider> TimelineStateTransaction<'a, P> {
    /// Create a new [`TimelineStateTransaction`].
    pub(super) fn new(
        items: &'a mut ObservableItems,
        meta: &'a mut TimelineMetadata,
        focus: &'a TimelineFocusKind,
    ) -> Self {
        let previous_meta = meta;
        let meta = previous_meta.clone();
        let items = items.transaction();

        Self {
            number_of_items_when_transaction_started: items.len(),
            items,
            previous_meta,
            meta,
            focus,
            _phantom: std::marker::PhantomData,
        }
    }

    /// If `event_id` is already tracked in `all_remote_events` (and possibly
    /// rendered as a timeline item), remove the stale copy and return its
    /// recycled timeline id so the re-delivered event replaces it in place
    /// (stable internal id) instead of duplicating it.
    ///
    /// The event-cache diff stream is *meant* to be self-consistent, but a
    /// limited-sync collapse/rebuild can interleave with a re-delivery (e.g.
    /// an own-message arriving via both the send queue and the sync echo)
    /// such that an event we still hold is pushed again without a `Remove`
    /// for the copy we have. Mirroring that faithfully would render the same
    /// event twice - and duplicate items break per-event-id keying in
    /// downstream UIs. Enforce the "one item per event id" invariant here.
    fn remove_stale_duplicate(
        &mut self,
        event_id: Option<&ruma::EventId>,
        date_divider_adjuster: &mut DateDividerAdjuster,
    ) -> Option<(usize, Option<TimelineUniqueId>)> {
        let event_id = event_id?;
        let old_event_index = self.items.all_remote_events().position_by_event_id(event_id)?;
        warn!(
            ?event_id,
            old_event_index,
            "remote event re-delivered while already present in the timeline; \
             removing the stale copy before re-adding"
        );
        let recycled = self.remove_timeline_item(old_event_index, date_divider_adjuster);
        Some((old_event_index, recycled.map(|(id, _)| id)))
    }

    /// Handle updates on events as [`VectorDiff`]s.
    pub(super) async fn handle_remote_events_with_diffs(
        &mut self,
        diffs: Vec<VectorDiff<TimelineEvent>>,
        origin: RemoteEventOrigin,
        room_data_provider: &P,
        settings: &TimelineSettings,
    ) {
        let mut date_divider_adjuster =
            DateDividerAdjuster::new(settings.date_divider_mode.clone());

        let mut cached_profiles: HashMap<OwnedUserId, Option<Profile>> = HashMap::new();

        let mut recycled_timeline_ids = HashMap::new();

        for diff in diffs {
            match diff {
                VectorDiff::Append { values: events } => {
                    for event in events {
                        let recycled_timeline_id = event
                            .event_id()
                            .and_then(|event_id| recycled_timeline_ids.remove(event_id));
                        let recycled_timeline_id = self
                            .remove_stale_duplicate(event.event_id(), &mut date_divider_adjuster)
                            .and_then(|(_, id)| id)
                            .or(recycled_timeline_id);
                        self.handle_remote_event(
                            event,
                            TimelineItemPosition::End { origin },
                            room_data_provider,
                            settings,
                            &mut date_divider_adjuster,
                            &mut cached_profiles,
                            recycled_timeline_id,
                        )
                        .await;
                    }
                }

                VectorDiff::PushFront { value: event } => {
                    let recycled_timeline_id = event
                        .event_id()
                        .and_then(|event_id| recycled_timeline_ids.remove(event_id));
                    let recycled_timeline_id = self
                        .remove_stale_duplicate(event.event_id(), &mut date_divider_adjuster)
                        .and_then(|(_, id)| id)
                        .or(recycled_timeline_id);
                    self.handle_remote_event(
                        event,
                        TimelineItemPosition::Start { origin },
                        room_data_provider,
                        settings,
                        &mut date_divider_adjuster,
                        &mut cached_profiles,
                        recycled_timeline_id,
                    )
                    .await;
                }

                VectorDiff::PushBack { value: event } => {
                    let recycled_timeline_id = event
                        .event_id()
                        .and_then(|event_id| recycled_timeline_ids.remove(event_id));
                    let recycled_timeline_id = self
                        .remove_stale_duplicate(event.event_id(), &mut date_divider_adjuster)
                        .and_then(|(_, id)| id)
                        .or(recycled_timeline_id);
                    self.handle_remote_event(
                        event,
                        TimelineItemPosition::End { origin },
                        room_data_provider,
                        settings,
                        &mut date_divider_adjuster,
                        &mut cached_profiles,
                        recycled_timeline_id,
                    )
                    .await;
                }

                VectorDiff::Insert { index: event_index, value: event } => {
                    let recycled_timeline_id = event
                        .event_id()
                        .and_then(|event_id| recycled_timeline_ids.remove(event_id));
                    // Removing a stale duplicate shifts `all_remote_events`;
                    // adjust the insertion index if the removed entry was
                    // before it.
                    let mut event_index = event_index;
                    let recycled_timeline_id = match self
                        .remove_stale_duplicate(event.event_id(), &mut date_divider_adjuster)
                    {
                        Some((old_event_index, id)) => {
                            if old_event_index < event_index {
                                event_index -= 1;
                            }
                            id.or(recycled_timeline_id)
                        }
                        None => recycled_timeline_id,
                    };
                    self.handle_remote_event(
                        event,
                        TimelineItemPosition::At { event_index, origin },
                        room_data_provider,
                        settings,
                        &mut date_divider_adjuster,
                        &mut cached_profiles,
                        recycled_timeline_id,
                    )
                    .await;
                }

                VectorDiff::Set { index: event_index, value: event } => {
                    if let Some(timeline_item_index) = self
                        .items
                        .all_remote_events()
                        .get(event_index)
                        .and_then(|meta| meta.timeline_item_index)
                    {
                        self.handle_remote_event(
                            event,
                            TimelineItemPosition::UpdateAt { timeline_item_index },
                            room_data_provider,
                            settings,
                            &mut date_divider_adjuster,
                            &mut cached_profiles,
                            None,
                        )
                        .await;
                    } else {
                        warn!(
                            event_index,
                            "Set update dropped because there wasn't any attached timeline item index."
                        );
                    }
                }

                VectorDiff::Remove { index: event_index } => {
                    if let Some((timeline_id, event_id)) =
                        self.remove_timeline_item(event_index, &mut date_divider_adjuster)
                    {
                        recycled_timeline_ids.insert(event_id, timeline_id);
                    }
                }

                VectorDiff::Clear => {
                    self.clear();
                }

                v => unimplemented!("{v:?}"),
            }
        }

        self.adjust_date_dividers(date_divider_adjuster);
        self.check_invariants();
    }

    async fn handle_remote_aggregation(
        &mut self,
        event: TimelineEvent,
        position: TimelineItemPosition,
        room_data_provider: &P,
        date_divider_adjuster: &mut DateDividerAdjuster,
    ) {
        let raw_event = event.raw();

        let deserialized = match raw_event.deserialize() {
            Ok(deserialized) => deserialized,
            Err(err) => {
                warn!("Failed to deserialize timeline event: {err}");
                return;
            }
        };

        let sender = deserialized.sender().to_owned();
        let timestamp = deserialized.origin_server_ts();
        let event_id = deserialized.event_id().to_owned();
        let txn_id = deserialized.transaction_id().map(ToOwned::to_owned);

        let timeline_actions = TimelineAction::from_event(
            deserialized,
            raw_event,
            room_data_provider,
            None,
            None,
            None,
            None,
        )
        .await;

        if timeline_actions.is_empty() {
            return;
        }

        let encryption_info = event.kind.encryption_info().cloned();
        let sender_profile = room_data_provider.profile_from_user_id(&sender).await;

        let (forwarder, forwarder_profile) = get_forwarder_info(&event, room_data_provider).await;

        let mut ctx = TimelineEventContext {
            sender,
            sender_profile,
            forwarder,
            forwarder_profile,
            timestamp,
            // These are not used when handling an aggregation.
            read_receipts: Default::default(),
            is_highlighted: false,
            flow: Flow::Remote {
                event_id: event_id.clone(),
                raw_event: event.raw().clone(),
                encryption_info,
                txn_id,
                position,
            },
            // This field is not used when handling an aggregation.
            should_add_new_items: false,
        };

        // FIXME: Continuation of the hackjob to get UTDs for focused timelines
        // working from `handle_remote_aggregations()`.
        if let [TimelineAction::AddItem { .. }] = timeline_actions.as_slice()
            && let TimelineItemPosition::UpdateAt { timeline_item_index } = position
            && let Some(item) = self.items.get(timeline_item_index)
            && item
                .as_event()
                .map(|e| e.content().is_unable_to_decrypt() && e.event_id() == Some(&event_id))
                .unwrap_or_default()
        {
            // Except when this is an UTD transitioning into a decrypted event.
            ctx.should_add_new_items = true;
        }

        for action in timeline_actions {
            TimelineEventHandler::new(self, &ctx)
                .handle_event(date_divider_adjuster, action, None)
                .await;
        }
    }

    /// Handle a set of live remote aggregations on events as [`VectorDiff`]s.
    ///
    /// This is like `handle_remote_events`, with two key differences:
    /// - it only applies to aggregated events, not all the sync events.
    /// - it will also not add the events to the `all_remote_events` array
    ///   itself.
    pub(super) async fn handle_remote_aggregations(
        &mut self,
        diffs: Vec<VectorDiff<TimelineEvent>>,
        origin: RemoteEventOrigin,
        room_data_provider: &P,
        settings: &TimelineSettings,
    ) {
        let mut date_divider_adjuster =
            DateDividerAdjuster::new(settings.date_divider_mode.clone());

        for diff in diffs {
            match diff {
                VectorDiff::Append { values: events } => {
                    for event in events {
                        self.handle_remote_aggregation(
                            event,
                            TimelineItemPosition::End { origin },
                            room_data_provider,
                            &mut date_divider_adjuster,
                        )
                        .await;
                    }
                }

                VectorDiff::PushFront { value: event } => {
                    self.handle_remote_aggregation(
                        event,
                        TimelineItemPosition::Start { origin },
                        room_data_provider,
                        &mut date_divider_adjuster,
                    )
                    .await;
                }

                VectorDiff::PushBack { value: event } => {
                    self.handle_remote_aggregation(
                        event,
                        TimelineItemPosition::End { origin },
                        room_data_provider,
                        &mut date_divider_adjuster,
                    )
                    .await;
                }

                VectorDiff::Insert { index: event_index, value: event } => {
                    self.handle_remote_aggregation(
                        event,
                        TimelineItemPosition::At { event_index, origin },
                        room_data_provider,
                        &mut date_divider_adjuster,
                    )
                    .await;
                }

                VectorDiff::Set { index: event_index, value: event } => {
                    if let Some(timeline_item_index) = self
                        .items
                        .all_remote_events()
                        .get(event_index)
                        .and_then(|meta| meta.timeline_item_index)
                    {
                        self.handle_remote_aggregation(
                            event,
                            TimelineItemPosition::UpdateAt { timeline_item_index },
                            room_data_provider,
                            &mut date_divider_adjuster,
                        )
                        .await;
                    } else if let Some(event_id) = event.event_id()
                        && let Some(meta) = self.items.all_remote_events().get_by_event_id(event_id)
                        && let Some(timeline_item_index) = meta.timeline_item_index
                    {
                        // FIXME: This branch is a complete hackjob.
                        //
                        // The reason being is that this branch is here to handle UTD -> Decrypted
                        // event remplacements for focused timelines. But this transition should
                        // naturally happen the same way it happens for unfocused timelines.
                        //
                        // Why it doesn't work here? Because the event cache fires out a
                        // VectorDiff::Set with an index that matches to the cache's view of the
                        // timeline, which is unfiltered, while the focused timeline will only show
                        // i.e. pinned events.
                        //
                        // The `test_pinned_events_are_decrypted_after_recovering` integration test
                        // showcases this. The event cache fires out the `Set` with an index of 7,
                        // but the timeline with the PinnedEvents focus has only 4 items.
                        //
                        // This hackjob continues in the `handle_remote_aggregation()` method as we
                        // can't just handle any `TimelineAction::AddItem` due to:
                        //  https://github.com/matrix-org/matrix-rust-sdk/pull/4645
                        //
                        // Doing so breaks the `test_new_pinned_events_are_not_added_on_sync` test.
                        //
                        // Relevant issue: https://github.com/matrix-org/matrix-rust-sdk/issues/5954.
                        self.handle_remote_aggregation(
                            event,
                            TimelineItemPosition::UpdateAt { timeline_item_index },
                            room_data_provider,
                            &mut date_divider_adjuster,
                        )
                        .await;
                    } else {
                        warn!(
                            event_index,
                            "Set update dropped because there wasn't any attached timeline item index."
                        );
                    }
                }

                VectorDiff::Remove { .. } | VectorDiff::Clear => {
                    // Do nothing. An aggregated redaction comes with a
                    // redaction event, or as a redacted event in the first
                    // place.
                }

                v => unimplemented!("{v:?}"),
            }
        }

        self.adjust_date_dividers(date_divider_adjuster);
        self.check_invariants();
    }

    fn check_invariants(&self) {
        self.check_no_duplicate_read_receipts();
        self.check_no_unused_unique_ids();
    }

    /// Check whether any read receipts are duplicated. If so, log an error
    /// message.
    ///
    /// Returns true if some receipts are duplicated, and false if all is fine.
    /// This return value is used for testing, and ignored in production (the
    /// error log is sufficient).
    fn check_no_duplicate_read_receipts(&self) -> bool {
        let mut by_user_and_thread = HashMap::new();
        let mut duplicates = HashSet::new();

        for item in self.items.iter_remotes_region().filter_map(|(_, item)| item.as_event()) {
            if let Some(event_id) = item.event_id() {
                for (user_id, read_receipt) in item.read_receipts() {
                    let key = (user_id, read_receipt.thread.as_str());
                    if let Some(prev_event_id) = by_user_and_thread.insert(key, event_id) {
                        duplicates.insert((key.0, key.1, prev_event_id, event_id));
                    }
                }
            }
        }

        if duplicates.is_empty() {
            // No duplicates - all good
            false
        } else {
            // Some duplicates - log an error and return true (used in tests)
            tracing::error!(
                ?duplicates,
                items = ?self.items,
                "duplicate read receipts in this timeline",
            );

            true
        }
    }

    fn check_no_unused_unique_ids(&self) {
        let duplicates = self
            .items
            .iter_all_regions()
            .duplicates_by(|(_nth, item)| item.unique_id())
            .map(|(_nth, item)| item.unique_id())
            .collect::<Vec<_>>();

        if !duplicates.is_empty() {
            #[cfg(any(debug_assertions, test))]
            panic!("duplicate unique ids in this timeline: {duplicates:?}\n{:?}", self.items);

            #[cfg(not(any(debug_assertions, test)))]
            tracing::error!(
                ?duplicates,
                items = ?self.items,
                "duplicate unique ids in this timeline",
            );
        }
    }

    /// Whether the event should be added to the timeline as a new item.
    fn should_add_event_item(
        &self,
        room_data_provider: &P,
        settings: &TimelineSettings,
        event: &AnySyncTimelineEvent,
        thread_root: Option<&EventId>,
        position: TimelineItemPosition,
    ) -> bool {
        let rules = room_data_provider.room_version_rules();

        if !(settings.event_filter)(event, &rules) {
            // The user filtered out the event.
            return false;
        }

        match &self.focus {
            TimelineFocusKind::PinnedEvents { .. } => {
                // The pinned events timeline only receives updates for, well, pinned events.
                true
            }

            TimelineFocusKind::Event { .. } => {
                // For event-focused timelines, thread filtering is now handled in the
                // event cache layer. We accept all events from pagination.

                // Retrieve the origin of the event.
                let origin = match position {
                    TimelineItemPosition::End { origin }
                    | TimelineItemPosition::Start { origin }
                    | TimelineItemPosition::At { origin, .. } => origin,

                    TimelineItemPosition::UpdateAt { timeline_item_index: idx } => self
                        .items
                        .get(idx)
                        .and_then(|item| item.as_event()?.as_remote())
                        .map_or(RemoteEventOrigin::Unknown, |item| item.origin),
                };

                match origin {
                    // Never add any item to a focused timeline when the item comes from sync.
                    RemoteEventOrigin::Sync | RemoteEventOrigin::Unknown => false,
                    RemoteEventOrigin::Cache | RemoteEventOrigin::Pagination => true,
                }
            }

            TimelineFocusKind::Live { hide_threaded_events, .. } => {
                // If the timeline's filtering out in-thread events, don't add items for
                // threaded events.
                thread_root.is_none() || !hide_threaded_events
            }

            TimelineFocusKind::Thread { root_event_id, .. } => {
                // Add new items only for the thread root and the thread replies.
                event.event_id() == root_event_id
                    || thread_root.as_ref().is_some_and(|r| r == root_event_id)
            }
        }
    }

    /// Whether this event can show read receipts, or if they should be moved
    /// to the previous event.
    fn can_show_read_receipts(
        &self,
        settings: &TimelineSettings,
        event: &AnySyncTimelineEvent,
    ) -> bool {
        match event {
            AnySyncTimelineEvent::State(_) => {
                matches!(settings.track_read_receipts, TimelineReadReceiptTracking::AllEvents)
            }
            AnySyncTimelineEvent::MessageLike(_) => {
                !matches!(settings.track_read_receipts, TimelineReadReceiptTracking::Disabled)
            }
        }
    }

    /// After a deserialization error, adds a failed-to-parse item to the
    /// timeline if configured to do so, or logs the error (and optionally
    /// save metadata) if not.
    async fn maybe_add_error_item(
        &mut self,
        position: TimelineItemPosition,
        room_data_provider: &P,
        raw: &Raw<AnySyncTimelineEvent>,
        deserialization_error: serde_json::Error,
        settings: &TimelineSettings,
    ) -> Option<(
        OwnedEventId,
        OwnedUserId,
        MilliSecondsSinceUnixEpoch,
        Option<OwnedTransactionId>,
        Vec<TimelineAction>,
        Option<OwnedEventId>,
        bool,
        bool,
    )> {
        let state_key: Option<String> = raw.get_field("state_key").ok().flatten();

        // A state event is an event that has a state key. Note that the two branches
        // differ because the inferred return type for `get_field` is different
        // in each case.
        //
        // If this was a state event but it didn't include a state_key, we'll assume it
        // was a msg-like, because we can't do much more.
        let event_type = if let Some(state_key) = state_key {
            raw.get_field("type")
                .ok()
                .flatten()
                .map(|event_type| FailedToParseEvent::State { event_type, state_key })
        } else {
            raw.get_field("type").ok().flatten().map(FailedToParseEvent::MsgLike)
        };

        let event_id: Option<OwnedEventId> = raw.get_field("event_id").ok().flatten();
        let Some(event_id) = event_id else {
            // If the event doesn't even have an event ID, we can't do anything with it.
            warn!(
                ?event_type,
                "Failed to deserialize timeline event (with no ID): {deserialization_error}"
            );
            return None;
        };

        let sender: Option<OwnedUserId> = raw.get_field("sender").ok().flatten();
        let origin_server_ts: Option<MilliSecondsSinceUnixEpoch> =
            raw.get_field("origin_server_ts").ok().flatten();

        match (sender, origin_server_ts, event_type) {
            (Some(sender), Some(origin_server_ts), Some(event_type))
                if settings.add_failed_to_parse =>
            {
                // We have sufficient information to show an item in the timeline, and we've
                // been requested to show it, let's do it.
                #[derive(serde::Deserialize)]
                struct Unsigned {
                    transaction_id: Option<OwnedTransactionId>,
                }

                let transaction_id: Option<OwnedTransactionId> = raw
                    .get_field::<Unsigned>("unsigned")
                    .ok()
                    .flatten()
                    .and_then(|unsigned| unsigned.transaction_id);

                // The event can be partially deserialized, and it is allowed to be added to
                // the timeline.
                Some((
                    event_id,
                    sender,
                    origin_server_ts,
                    transaction_id,
                    vec![TimelineAction::failed_to_parse(event_type, deserialization_error)],
                    None,
                    true,
                    true,
                ))
            }

            (sender, origin_server_ts, event_type) => {
                // We either lack information for rendering an item, or we've been requested not
                // to show it. Save it into the metadata and return.
                warn!(
                    ?event_type,
                    ?event_id,
                    "Failed to deserialize timeline event: {deserialization_error}"
                );

                // Remember the event before returning prematurely.
                // See [`ObservableItems::all_remote_events`].
                self.add_or_update_remote_event(
                    EventMeta::new(event_id, sender.as_deref(), false, false, None),
                    sender.as_deref(),
                    origin_server_ts,
                    position,
                    room_data_provider,
                    settings,
                )
                .await;
                None
            }
        }
    }

    // Attempt to load a thread's latest reply as an embedded timeline item, either
    // using the event cache or the storage.
    #[instrument(skip(self, room_data_provider))]
    async fn fetch_latest_thread_reply(
        &mut self,
        event_id: &EventId,
        room_data_provider: &P,
    ) -> Option<Box<EmbeddedEvent>> {
        let event = RoomDataProvider::load_event(room_data_provider, event_id)
            .await
            .inspect_err(|err| {
                warn!("Failed to load thread latest event: {err}");
            })
            .ok()?;

        EmbeddedEvent::try_from_timeline_event(event, room_data_provider, &self.meta)
            .await
            .inspect_err(|err| {
                warn!("Failed to extract thread latest event into a timeline item content: {err}");
            })
            .ok()
            .flatten()
            .map(Box::new)
    }

    /// Compute the thread public and private receipts, for the sake of the
    /// [`ThreadSummary`] of an event.
    async fn compute_summary_thread_receipts(
        &self,
        event: &TimelineEvent,
        summary: &SdkThreadSummary,
        room_data_provider: &P,
        settings: &TimelineSettings,
    ) -> (Option<OwnedEventId>, Option<OwnedEventId>) {
        if !settings.track_read_receipts.is_enabled() {
            return (None, None);
        }

        // Load the public and private read receipts for the user, in the thread. In the
        // future, we might move this code in the event cache, so that read
        // receipt handling happens there instead.

        // As an exception to handle the latest "implicit" read receipt (which is the
        // latest event sent by the user): if the latest event has been sent by
        // the current user, then we consider that as a read receipt.
        #[allow(clippy::collapsible_if)] // clippy has poor taste
        if let Some(ref latest_reply) = summary.latest_reply {
            if let Ok(event) = RoomDataProvider::load_event(room_data_provider, latest_reply)
                .await
                .inspect_err(|err| {
                    warn!("Failed to load thread latest event: {err}");
                })
            {
                // Parse the sender.
                if let Some(sender) = event.sender()
                    && sender == self.meta.own_user_id
                {
                    let latest = Some(latest_reply.clone());
                    return (latest.clone(), latest);
                }
            }
        }

        // Otherwise, resort to trying to load receipts from the database.
        let own_thread_public_receipt = if let Some(event_id) = event.event_id() {
            room_data_provider
                .load_user_receipt(
                    ReceiptType::Read,
                    ReceiptThread::Thread(event_id.to_owned()),
                    &self.meta.own_user_id,
                )
                .await
                .map(|(event_id, _receipt)| event_id)
        } else {
            None
        };

        let own_thread_private_receipt = if let Some(event_id) = event.event_id() {
            room_data_provider
                .load_user_receipt(
                    ReceiptType::ReadPrivate,
                    ReceiptThread::Thread(event_id.to_owned()),
                    &self.meta.own_user_id,
                )
                .await
                .map(|(event_id, _receipt)| event_id)
        } else {
            None
        };

        (own_thread_public_receipt, own_thread_private_receipt)
    }

    /// Handle a remote event.
    ///
    /// Returns whether an item has been removed from the timeline.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn handle_remote_event(
        &mut self,
        event: TimelineEvent,
        position: TimelineItemPosition,
        room_data_provider: &P,
        settings: &TimelineSettings,
        date_divider_adjuster: &mut DateDividerAdjuster,
        profiles: &mut HashMap<OwnedUserId, Option<Profile>>,
        recycled_timeline_id: Option<TimelineUniqueId>,
    ) -> RemovedItem {
        let is_highlighted =
            event.push_actions().is_some_and(|actions| actions.iter().any(Action::is_highlight));

        let thread_summary = if let ThreadSummaryStatus::Some(ref summary) = event.thread_summary {
            let latest_reply_item = if let Some(ref latest_reply) = summary.latest_reply {
                self.fetch_latest_thread_reply(latest_reply, room_data_provider).await
            } else {
                None
            };

            let (own_thread_public_receipt, own_thread_private_receipt) = self
                .compute_summary_thread_receipts(&event, summary, room_data_provider, settings)
                .await;

            Some(ThreadSummary {
                latest_event: TimelineDetails::from_initial_value(latest_reply_item),
                num_replies: summary.num_replies,
                public_read_receipt_event_id: own_thread_public_receipt,
                private_read_receipt_event_id: own_thread_private_receipt,
            })
        } else {
            None
        };

        let encryption_info = event.kind.encryption_info().cloned();

        let bundled_edit_encryption_info = event.kind.unsigned_encryption_map().and_then(|map| {
            map.get(&UnsignedEventLocation::RelationsReplace)?.encryption_info().cloned()
        });

        let (forwarder, forwarder_profile) = get_forwarder_info(&event, room_data_provider).await;

        let (raw, utd_info) = match event.kind {
            TimelineEventKind::UnableToDecrypt { utd_info, event } => (event, Some(utd_info)),
            _ => (event.kind.into_raw(), None),
        };

        let (
            event_id,
            sender,
            timestamp,
            txn_id,
            timeline_actions,
            thread_root,
            should_add,
            can_show_read_receipts,
        ) = match raw.deserialize() {
            // Classical path: the event is valid, can be deserialized, everything is alright.
            Ok(event) => {
                let (in_reply_to, thread_root) = self.meta.process_event_relations(
                    &event,
                    &raw,
                    bundled_edit_encryption_info,
                    &self.items,
                    self.focus.is_thread(),
                );

                let should_add = self.should_add_event_item(
                    room_data_provider,
                    settings,
                    &event,
                    thread_root.as_deref(),
                    position,
                );

                let can_show_read_receipts = self.can_show_read_receipts(settings, &event);

                (
                    event.event_id().to_owned(),
                    event.sender().to_owned(),
                    event.origin_server_ts(),
                    event.transaction_id().map(ToOwned::to_owned),
                    TimelineAction::from_event(
                        event,
                        &raw,
                        room_data_provider,
                        utd_info
                            .map(|utd_info| (utd_info, self.meta.unable_to_decrypt_hook.as_ref())),
                        in_reply_to,
                        thread_root.clone(),
                        thread_summary,
                    )
                    .await,
                    thread_root,
                    should_add,
                    can_show_read_receipts,
                )
            }

            // The event seems invalid…
            Err(e) => {
                if let Some(tuple) =
                    self.maybe_add_error_item(position, room_data_provider, &raw, e, settings).await
                {
                    tuple
                } else {
                    return false;
                }
            }
        };

        // Remember the event.
        // See [`ObservableItems::all_remote_events`].
        self.add_or_update_remote_event(
            EventMeta::new(
                event_id.clone(),
                Some(&sender),
                should_add,
                can_show_read_receipts,
                thread_root,
            ),
            Some(&sender),
            Some(timestamp),
            position,
            room_data_provider,
            settings,
        )
        .await;

        // Handle the event to create or update a timeline item.
        let item_added = if !timeline_actions.is_empty() {
            let sender_profile = if let Some(profile) = profiles.get(&sender) {
                profile.clone()
            } else {
                let profile = room_data_provider.profile_from_user_id(&sender).await;
                profiles.insert(sender.clone(), profile.clone());
                profile
            };

            let mut item_added = false;
            let ctx = TimelineEventContext {
                sender,
                sender_profile,
                forwarder,
                forwarder_profile,
                timestamp,
                read_receipts: if settings.track_read_receipts.is_enabled()
                    && should_add
                    && can_show_read_receipts
                {
                    self.meta.read_receipts.compute_event_receipts(
                        &event_id,
                        &mut self.items,
                        matches!(position, TimelineItemPosition::End { .. }),
                    )
                } else {
                    Default::default()
                },

                is_highlighted,
                flow: Flow::Remote {
                    event_id: event_id.clone(),
                    raw_event: raw,
                    encryption_info,
                    txn_id,
                    position,
                },
                should_add_new_items: should_add,
            };
            // A recycled timeline ID carries the TimelineUniqueId from a previously
            // removed item (see VectorDiff::Remove), so that when the same event is
            // re-added in the same diff batch the UI sees a stable identifier.
            // It is only applicable when the event produces a single AddItem action;
            // with multiple actions (e.g. beacon replace) there
            // is no single item to associate it with, so it's safe to ignore.
            let recycled_timeline_id = recycled_timeline_id.filter(|_| timeline_actions.len() == 1);

            for action in timeline_actions {
                item_added |= TimelineEventHandler::new(self, &ctx)
                    .handle_event(date_divider_adjuster, action, recycled_timeline_id.clone())
                    .await;
            }
            item_added
        } else {
            // No item has been added to the timeline.
            false
        };

        let mut item_removed = false;

        if !item_added {
            trace!("No new item added");

            if let TimelineItemPosition::UpdateAt { timeline_item_index } = position {
                // If add was not called, that means the UTD event is one that
                // wouldn't normally be visible. Remove it.
                trace!("Removing UTD that was successfully retried");
                self.items.remove(timeline_item_index);
                item_removed = true;
            }
        }

        item_removed
    }

    /// Remove one timeline item by its `event_index`.
    fn remove_timeline_item(
        &mut self,
        event_index: usize,
        day_divider_adjuster: &mut DateDividerAdjuster,
    ) -> Option<(TimelineUniqueId, OwnedEventId)> {
        day_divider_adjuster.mark_used();

        // We need to be careful here.
        //
        // We must first remove the timeline item, which will update the mapping between
        // remote events and timeline items. Removing the timeline item will “unlink”
        // this mapping as the remote event will be updated to map to nothing. Only
        // after that, we can remove the remote event. Doing this in the other order
        // will update the mapping twice, and will result in a corrupted state.

        let mut recycled_timeline_id = None;

        // Remove the timeline item first.
        if let Some(event_meta) = self.items.all_remote_events().get(event_index) {
            // Fetch the `timeline_item_index` associated to the remote event.
            if let Some(timeline_item_index) = event_meta.timeline_item_index {
                let event_id = event_meta.event_id.clone();
                let timeline_item = self.items.remove(timeline_item_index);
                recycled_timeline_id = Some((timeline_item.unique_id().clone(), event_id));
            }

            // Now we can remove the remote event.
            self.items.remove_remote_event(event_index);
        }

        recycled_timeline_id
    }

    pub(super) fn clear(&mut self) {
        // By first checking if there are any local echoes first, we do a bit
        // more work in case some are found, but it should be worth it because
        // there will often not be any, and only emitting a single
        // `VectorDiff::Clear` should be much more efficient to process for
        // subscribers.
        if self.items.has_local() {
            // Remove all remote events and virtual items that aren't date dividers.
            self.items.for_each(|entry| {
                if entry.is_remote_event()
                    || entry.as_virtual().is_some_and(|vitem| match vitem {
                        VirtualTimelineItem::DateDivider(_) => false,
                        VirtualTimelineItem::ReadMarker
                        | VirtualTimelineItem::TimelineStart
                        | VirtualTimelineItem::Gap { .. } => true,
                    })
                {
                    ObservableItemsTransactionEntry::remove(entry);
                }
            });

            // The per-entry removal above only unlinks the item<->event
            // mappings - the `EventMeta` entries survive in
            // `all_remote_events`. They must go too: `all_remote_events`
            // positionally mirrors the event cache's events vector (incoming
            // `Insert/Set/Remove @k` diffs index it), and the cache just
            // cleared ITS side. Keeping stale entries offsets every
            // subsequent positional diff - observed live as a fresh message
            // being destroyed by a `Remove @k` that addressed a stale
            // neighbour (the cache meant its re-delivered own-message copy;
            // the meta list had +1 entries and resolved k to the new
            // message). Only this branch is affected: the `else` branch's
            // `items.clear()` already clears both. Local echoes aren't
            // tracked in `all_remote_events`, so clearing it wholesale is
            // safe here.
            self.items.clear_all_remote_events();

            // Remove stray date dividers
            let mut idx = 0;
            while idx < self.items.len() {
                if self.items[idx].is_date_divider()
                    && self.items.get(idx + 1).is_none_or(|item| item.is_date_divider())
                {
                    self.items.remove(idx);
                    // don't increment idx because all elements have shifted
                } else {
                    idx += 1;
                }
            }
        } else {
            self.items.clear();
        }

        self.meta.clear();

        debug!(remaining_items = self.items.len(), "Timeline cleared");
    }

    #[instrument(skip_all)]
    pub(super) fn set_fully_read_event(&mut self, fully_read_event_id: OwnedEventId) {
        // A similar event has been handled already. We can ignore it.
        if self.meta.fully_read_event.as_ref().is_some_and(|id| *id == fully_read_event_id) {
            return;
        }

        self.meta.fully_read_event = Some(fully_read_event_id);
        self.meta.update_read_marker(&mut self.items);
    }

    /// Whether the oldest known gap leads the timeline, i.e. no known remote
    /// event is older than it.
    ///
    /// Judged from the event cache's gap snapshot
    /// ([`TimelineMetadata::timeline_gaps`]) rather than from the rendered
    /// gap items: a gap whose anchor event hasn't reached this timeline yet
    /// has no item, but is leading all the same.
    pub(super) fn has_leading_gap(&self) -> bool {
        self.meta.timeline_gaps.first().is_some_and(|gap| {
            self.items
                .position_by_event_id(&gap.following_event_id)
                .is_none_or(|event_index| event_index == 0)
        })
    }

    /// Insert the timeline start item, unless it's there already, or a
    /// leading gap means the room's start hasn't actually been seen yet.
    pub(super) fn insert_timeline_start_if_missing(&mut self) {
        if self.has_leading_gap() {
            return;
        }

        self.items.push_timeline_start_if_missing(
            self.meta.new_timeline_item(VirtualTimelineItem::TimelineStart),
        );
    }

    /// Reconcile the gap virtual items against the latest set of gaps
    /// reported by the event cache ([`TimelineMetadata::timeline_gaps`]).
    ///
    /// Each gap is (re-)inserted immediately before its anchor: the timeline
    /// item of the first event following the gap that has one. Gaps that
    /// can't be anchored (trailing gaps, or gaps whose followers are all
    /// filtered out of this timeline) aren't rendered, and gaps sharing an
    /// anchor are collapsed down to their newest member so that adjacent
    /// gaps show as a single item.
    ///
    /// This runs at the end of every state transaction. Since a transaction's
    /// diffs are applied atomically by subscribers, removing and re-inserting
    /// an unchanged gap item (which keeps its identity) is invisible to them.
    fn reconcile_gap_items(&mut self) {
        // Fast path: no gaps wanted, and none rendered.
        if self.meta.timeline_gaps.is_empty() && !self.meta.has_gap_items {
            return;
        }

        // Compute the desired placements: for each anchorable gap, the index
        // of its anchor item, i.e. the timeline item of the first event
        // at-or-after the gap's following event that has one (the following
        // event itself may be filtered out of this timeline). Gaps whose
        // followers are all unknown or filtered out aren't rendered.
        //
        // Note: these indices are in the *current* coordinates, i.e. with the
        // currently rendered gap items still in place.
        let desired = self
            .meta
            .timeline_gaps
            .clone()
            .into_iter()
            .filter_map(|gap| {
                let event_index = self.items.position_by_event_id(&gap.following_event_id)?;
                let anchor_item_index = self
                    .items
                    .all_remote_events()
                    .range(event_index..)
                    .find_map(|event_meta| event_meta.timeline_item_index)?;
                Some((gap, anchor_item_index))
            })
            .collect::<Vec<_>>();

        // Gaps sharing an anchor would render as adjacent items with nothing
        // between them; collapse each such run down to its newest member.
        // Resolving that one either closes it, or lands events between the
        // gaps, at which point the survivors anchor to those and get rendered
        // in turn. Runs are consecutive in `desired`, since gaps arrive in
        // timeline order and anchors are monotonic.
        let desired = {
            let mut collapsed: Vec<(TimelineGap, usize)> = Vec::with_capacity(desired.len());
            for entry in desired {
                if collapsed.last().is_some_and(|(_, anchor)| *anchor == entry.1) {
                    *collapsed.last_mut().unwrap() = entry;
                } else {
                    collapsed.push(entry);
                }
            }
            collapsed
        };

        // Fast path: everything is already in place, i.e. for each anchor,
        // the items immediately preceding it are exactly its gaps, in order,
        // and no other gap item exists. In that case, don't touch the items
        // at all: this runs on every transaction, and no-op remove/re-insert
        // pairs would spam the subscribers.
        let rendered_gap_count = (0..self.items.len())
            .filter(|i| self.items.get(*i).is_some_and(|it| it.is_gap()))
            .count();

        let all_in_place = rendered_gap_count == desired.len()
            && desired
                .iter()
                .rev()
                .scan(HashMap::<usize, usize>::new(), |offsets, (gap, anchor)| {
                    // The n-th gap (from the end) anchored to `anchor` must
                    // live at `anchor - 1 - n`.
                    let nth = offsets.entry(*anchor).or_insert(0);
                    let expected_index = anchor.checked_sub(1 + *nth);
                    *nth += 1;

                    Some(expected_index.is_some_and(|index| {
                        self.items
                            .get(index)
                            .and_then(|item| item.as_gap())
                            .is_some_and(|rendered| rendered == gap.prev_token)
                    }))
                })
                .all(|in_place| in_place);

        if all_in_place {
            self.meta.has_gap_items = rendered_gap_count > 0;
            return;
        }

        // Remove all existing gap items, remembering them by token so they
        // keep their identity when re-inserted below.
        let mut existing_items = HashMap::new();

        for timeline_item_index in (0..self.items.len()).rev() {
            let Some(token) = self
                .items
                .get(timeline_item_index)
                .and_then(|item| item.as_gap())
                .map(ToOwned::to_owned)
            else {
                continue;
            };

            existing_items.insert(token, self.items.remove(timeline_item_index));
        }

        // (Re-)insert the desired gaps, in timeline order.
        let mut has_gap_items = false;

        for (gap, _) in desired {
            // The anchor is the timeline item of the first event at-or-after
            // the gap's following event that has one (the following event
            // itself may be filtered out of this timeline).
            let Some(event_index) = self.items.position_by_event_id(&gap.following_event_id) else {
                // The timeline doesn't know about the event (yet); a later
                // transaction will reconcile again.
                continue;
            };

            let Some(anchor_item_index) = self
                .items
                .all_remote_events()
                .range(event_index..)
                .find_map(|event_meta| event_meta.timeline_item_index)
            else {
                // Nothing after the gap is rendered in this timeline; there's
                // no sensible place to show it.
                continue;
            };

            let item = existing_items.remove(&gap.prev_token).unwrap_or_else(|| {
                self.meta.new_timeline_item(VirtualTimelineItem::Gap { prev_token: gap.prev_token })
            });

            self.items.insert(anchor_item_index, item, None);
            has_gap_items = true;
        }

        // Any leftover in `existing_items` is a gap that's no longer wanted;
        // it's already been removed from the items above.

        self.meta.has_gap_items = has_gap_items;
    }

    pub(super) fn commit(mut self) {
        // Bring the rendered gap items in line with the event cache's latest
        // gap report, now that all the item mutations of this transaction are
        // done.
        self.reconcile_gap_items();

        // Update the `subscriber_skip_count` value.
        let previous_number_of_items = self.number_of_items_when_transaction_started;
        let next_number_of_items = self.items.len();

        if previous_number_of_items != next_number_of_items {
            let count = self
                .meta
                .subscriber_skip_count
                .compute_next(previous_number_of_items, next_number_of_items);
            self.meta
                .subscriber_skip_count
                .update(count, matches!(self.focus, TimelineFocusKind::Live { .. }));
        }

        // Replace the pointer to the previous meta with the new one.
        *self.previous_meta = self.meta;

        self.items.commit();
    }

    /// Add or update a remote event in the
    /// [`ObservableItems::all_remote_events`] collection.
    ///
    /// This method also adjusts read receipt if needed.
    async fn add_or_update_remote_event(
        &mut self,
        event_meta: EventMeta,
        sender: Option<&UserId>,
        timestamp: Option<MilliSecondsSinceUnixEpoch>,
        position: TimelineItemPosition,
        room_data_provider: &P,
        settings: &TimelineSettings,
    ) {
        let event_id = event_meta.event_id.clone();

        match position {
            TimelineItemPosition::Start { .. } => self.items.push_front_remote_event(event_meta),

            TimelineItemPosition::End { .. } => {
                self.items.push_back_remote_event(event_meta);
            }

            TimelineItemPosition::At { event_index, .. } => {
                self.items.insert_remote_event(event_index, event_meta);
            }

            TimelineItemPosition::UpdateAt { .. } => {
                if let Some(event) =
                    self.items.get_remote_event_by_event_id_mut(&event_meta.event_id)
                    && (event.visible != event_meta.visible
                        || event.can_show_read_receipts != event_meta.can_show_read_receipts)
                {
                    event.visible = event_meta.visible;
                    event.can_show_read_receipts = event_meta.can_show_read_receipts;

                    if settings.track_read_receipts.is_enabled() {
                        // Since the event's visibility changed, we need to update the read
                        // receipts of the previous visible event.
                        self.maybe_update_read_receipts_of_prev_event(&event_meta.event_id);
                    }
                }
            }
        }

        if settings.track_read_receipts.is_enabled()
            && matches!(
                position,
                TimelineItemPosition::Start { .. }
                    | TimelineItemPosition::End { .. }
                    | TimelineItemPosition::At { .. }
            )
        {
            self.load_read_receipts_for_event(&event_id, room_data_provider).await;

            self.maybe_add_implicit_read_receipt(&event_id, sender, timestamp);
        }
    }

    pub(super) fn adjust_date_dividers(&mut self, mut adjuster: DateDividerAdjuster) {
        adjuster.run(&mut self.items, &mut self.meta);
    }

    /// This method replaces the `is_room_encrypted` value for all timeline
    /// items to its updated version and creates a `VectorDiff::Set` operation
    /// for each item which will be added to this transaction.
    pub(super) fn mark_all_events_as_encrypted(&mut self) {
        for idx in 0..self.items.len() {
            let item = &self.items[idx];

            if let Some(event) = item.as_event() {
                if event.is_room_encrypted {
                    continue;
                }

                let mut cloned_event = event.clone();
                cloned_event.is_room_encrypted = true;

                // Replace the existing item with a new version with the right encryption flag
                let item = item.with_kind(cloned_event);
                self.items.replace(idx, item);
            }
        }
    }
}

/// Retrieves the forwarder information for a given timeline event.
///
/// # Parameters
///
/// - `event`: The timeline event to extract forwarder information from.
/// - `room_data_provider`: A reference to the room data provider.
///
/// # Returns
///
/// A tuple containing:
/// - `Option<OwnedUserId>`: The user ID of the forwarder, if available.
/// - `Option<Profile>`: The profile of the forwarder, if available.
async fn get_forwarder_info<P: RoomDataProvider>(
    event: &TimelineEvent,
    room_data_provider: &P,
) -> (Option<OwnedUserId>, Option<Profile>) {
    let forwarder = event
        .kind
        .encryption_info()
        .and_then(|info| info.forwarder.as_ref())
        .map(|info| info.user_id.clone());

    let forwarder_profile = if let Some(ref forwarder_id) = forwarder {
        Some(room_data_provider.profile_from_user_id(forwarder_id).await)
    } else {
        None
    };

    (forwarder, forwarder_profile.flatten())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use matrix_sdk::{Room, test_utils::mocks::MatrixMockServer};
    use matrix_sdk_test::async_test;
    use ruma::{
        MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId,
        events::receipt::{Receipt, ReceiptThread},
        owned_event_id, owned_user_id, room_id,
        room_version_rules::RoomVersionRules,
    };

    use crate::timeline::{
        EventTimelineItem, MsgLikeContent, TimelineDetails, TimelineItem, TimelineItemContent,
        TimelineItemKind, TimelineUniqueId,
        controller::{
            ObservableItems, TimelineFocusKind, TimelineMetadata, TimelineStateTransaction,
        },
        event_item::{EventTimelineItemKind, RemoteEventOrigin, RemoteEventTimelineItem},
    };

    #[async_test]
    async fn test_detects_duplicate_read_receipts_in_same_receipt_thread() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!r0");

        let _ = server.sync_joined_room(&client, room_id).await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        // Given a timeline with clashing receipts
        let mut items = create_items_with_receipts(vec![
            (owned_user_id!("@user1:s.co"), create_receipt(ReceiptThread::Unthreaded)),
            (owned_user_id!("@user1:s.co"), create_receipt(ReceiptThread::Unthreaded)),
        ]);

        // When we check for duplicates
        let user_id = owned_user_id!("@foo:s.co");
        let mut meta = TimelineMetadata::new(user_id, RoomVersionRules::V12, None, None, true);
        let focus = TimelineFocusKind::Live {
            hide_threaded_events: false,
            event_cache: event_cache.room(room_id).await.unwrap().0,
        };
        let transaction: TimelineStateTransaction<'_, Room> =
            TimelineStateTransaction::new(&mut items, &mut meta, &focus);

        let dups = transaction.check_no_duplicate_read_receipts();

        // Then some duplicates were found
        assert!(dups);
    }

    #[async_test]
    async fn test_if_there_are_no_duplicate_receipts_we_report_no_duplicates() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!r0");

        let _ = server.sync_joined_room(&client, room_id).await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        // Given a timeline with receipts, but no clashes (users are different)
        let mut items = create_items_with_receipts(vec![
            (owned_user_id!("@user1:s.co"), create_receipt(ReceiptThread::Unthreaded)),
            (owned_user_id!("@user2:s.co"), create_receipt(ReceiptThread::Unthreaded)),
        ]);

        // When we check for duplicates
        let user_id = owned_user_id!("@foo:s.co");
        let mut meta = TimelineMetadata::new(user_id, RoomVersionRules::V12, None, None, true);
        let focus = TimelineFocusKind::Live {
            hide_threaded_events: false,
            event_cache: event_cache.room(room_id).await.unwrap().0,
        };
        let transaction: TimelineStateTransaction<'_, Room> =
            TimelineStateTransaction::new(&mut items, &mut meta, &focus);

        let dups = transaction.check_no_duplicate_read_receipts();

        // Then no duplicates were found
        assert!(!dups);
    }

    #[async_test]
    async fn test_if_there_are_receipts_for_different_receipt_threads_we_report_no_duplicates() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!r0");

        let _ = server.sync_joined_room(&client, room_id).await;

        let event_cache = client.event_cache();
        event_cache.subscribe().unwrap();

        // Given a timeline with receipts, but no clashes (users are different)
        let mut items = create_items_with_receipts(vec![
            (owned_user_id!("@user1:s.co"), create_receipt(ReceiptThread::Unthreaded)),
            (
                owned_user_id!("@user1:s.co"),
                create_receipt(ReceiptThread::Thread(owned_event_id!("$thread_root"))),
            ),
        ]);

        // When we check for duplicates
        let user_id = owned_user_id!("@foo:s.co");
        let mut meta = TimelineMetadata::new(user_id, RoomVersionRules::V12, None, None, true);
        let focus = TimelineFocusKind::Live {
            hide_threaded_events: false,
            event_cache: event_cache.room(room_id).await.unwrap().0,
        };
        let transaction: TimelineStateTransaction<'_, Room> =
            TimelineStateTransaction::new(&mut items, &mut meta, &focus);

        let dups = transaction.check_no_duplicate_read_receipts();

        // Then no duplicates were found
        assert!(!dups);
    }

    fn create_receipt(receipt_thread: ReceiptThread) -> Receipt {
        let mut receipt = Receipt::new(MilliSecondsSinceUnixEpoch::now());
        receipt.thread = receipt_thread;
        receipt
    }

    fn create_items_with_receipts(receipts: Vec<(OwnedUserId, Receipt)>) -> ObservableItems {
        let mut items = ObservableItems::new();
        {
            let mut t = items.transaction();

            for (num, receipt) in receipts.into_iter().enumerate() {
                let event_id = OwnedEventId::try_from(format!("$event-{num}")).unwrap();
                let timeline_id = format!("timeline-{num}");

                t.push_front(event_with_receipt_item(event_id, timeline_id, receipt), None);
            }
            t.commit();
        }
        items
    }

    fn event_with_receipt_item(
        event_id: OwnedEventId,
        timeline_id: String,
        receipt: (OwnedUserId, Receipt),
    ) -> Arc<TimelineItem> {
        let event_kind = EventTimelineItemKind::Remote(RemoteEventTimelineItem {
            event_id,
            transaction_id: None,
            read_receipts: [receipt].into(),
            is_own: false,
            is_highlighted: false,
            encryption_info: None,
            original_json: None,
            latest_edit_json: None,
            origin: RemoteEventOrigin::Sync,
        });

        let event_timeline_item = EventTimelineItem::new(
            owned_user_id!("@u:s.co"),
            TimelineDetails::Pending,
            None,
            None,
            MilliSecondsSinceUnixEpoch::now(),
            TimelineItemContent::MsgLike(MsgLikeContent::redacted()),
            event_kind,
            false,
        );

        TimelineItem::new(
            TimelineItemKind::Event(event_timeline_item),
            TimelineUniqueId(timeline_id),
        )
    }
}
