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

use std::{collections::BTreeSet, fmt, sync::Arc};

use as_variant::as_variant;
use eyeball_im::{VectorDiff, VectorSubscriberStream};
use eyeball_im_util::vector::{FilterMap, VectorObserverExt};
use futures_core::Stream;
use imbl::Vector;
#[cfg(test)]
use matrix_sdk::Result;
use matrix_sdk::{
    config::RequestConfig,
    deserialized_responses::TimelineEvent,
    event_cache::{DecryptionRetryRequest, RoomEventCache, RoomPaginationStatus},
    paginators::{PaginationResult, PaginationToken, Paginator},
    send_queue::{
        LocalEcho, LocalEchoContent, RoomSendQueueUpdate, SendHandle, SendReactionHandle,
    },
};
#[cfg(test)]
use ruma::events::receipt::ReceiptEventContent;
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedTransactionId, TransactionId, UserId,
    api::client::receipt::create_receipt::v3::ReceiptType as SendReceiptType,
    events::{
        AnyMessageLikeEventContent, AnySyncEphemeralRoomEvent, AnySyncMessageLikeEvent,
        AnySyncTimelineEvent, MessageLikeEventType,
        poll::unstable_start::UnstablePollStartEventContent,
        reaction::ReactionEventContent,
        receipt::{Receipt, ReceiptThread, ReceiptType},
        relation::{Annotation, RelationType},
        room::message::{MessageType, Relation},
    },
    room_version_rules::RoomVersionRules,
    serde::Raw,
};
use tokio::sync::{OnceCell, RwLock, RwLockWriteGuard};
use tracing::{debug, error, field::debug, info, instrument, trace, warn};

pub(super) use self::{
    metadata::{RelativePosition, TimelineMetadata},
    observable_items::{
        AllRemoteEvents, ObservableItems, ObservableItemsEntry, ObservableItemsTransaction,
        ObservableItemsTransactionEntry,
    },
    state::TimelineState,
    state_transaction::TimelineStateTransaction,
};
use super::{
    DateDividerMode, EmbeddedEvent, Error, EventSendState, EventTimelineItem, InReplyToDetails,
    MediaUploadProgress, PaginationError, Profile, TimelineDetails, TimelineEventItemId,
    TimelineFocus, TimelineItem, TimelineItemContent, TimelineItemKind,
    TimelineReadReceiptTracking, VirtualTimelineItem,
    algorithms::{rfind_event_by_id, rfind_event_item},
    event_item::{ReactionStatus, RemoteEventOrigin},
    item::TimelineUniqueId,
    subscriber::TimelineSubscriber,
    traits::RoomDataProvider,
};
use crate::{
    timeline::{
        MsgLikeContent, MsgLikeKind, Room, TimelineEventFilterFn, TimelineEventFocusThreadMode,
        algorithms::rfind_event_by_item_id,
        controller::decryption_retry_task::compute_redecryption_candidates,
        date_dividers::DateDividerAdjuster,
        event_item::TimelineItemHandle,
        pinned_events_loader::{PinnedEventsLoader, PinnedEventsLoaderError},
    },
    unable_to_decrypt_hook::UtdHookManager,
};

pub(in crate::timeline) mod aggregations;
mod decryption_retry_task;
mod metadata;
mod observable_items;
mod read_receipts;
mod state;
mod state_transaction;

pub(super) use aggregations::*;
pub(super) use decryption_retry_task::{CryptoDropHandles, spawn_crypto_tasks};
use matrix_sdk::paginators::{PaginatorError, thread::ThreadedEventsLoader};
use matrix_sdk_common::serde_helpers::extract_thread_root;

/// Data associated to the current timeline focus.
///
/// This is the private counterpart of [`TimelineFocus`], and it is an augmented
/// version of it, including extra state that makes it useful over the lifetime
/// of a timeline.
#[derive(Debug)]
pub(in crate::timeline) enum TimelineFocusKind<P: RoomDataProvider> {
    /// The timeline receives live events from the sync.
    Live {
        /// Whether to hide in-thread events from the timeline.
        hide_threaded_events: bool,
    },

    /// The timeline is focused on a single event, and it can expand in one
    /// direction or another.
    Event {
        /// The paginator instance.
        paginator: OnceCell<AnyPaginator<P>>,
    },

    /// A live timeline for a thread.
    Thread {
        /// The root event for the current thread.
        root_event_id: OwnedEventId,
    },

    PinnedEvents {
        loader: PinnedEventsLoader,
    },
}

#[derive(Debug)]
pub(in crate::timeline) enum AnyPaginator<P: RoomDataProvider> {
    Unthreaded {
        /// The actual event paginator.
        paginator: Paginator<P>,
        /// Whether to hide in-thread events from the timeline.
        hide_threaded_events: bool,
    },
    Threaded(ThreadedEventsLoader<P>),
}

impl<P: RoomDataProvider> AnyPaginator<P> {
    /// Runs a backward pagination (requesting `num_events` to the server), from
    /// the current state of the object.
    ///
    /// Will return immediately if we have already hit the start of the
    /// timeline.
    ///
    /// May return an error if it's already paginating, or if the call to
    /// the homeserver endpoints failed.
    pub async fn paginate_backwards(
        &self,
        num_events: u16,
    ) -> Result<PaginationResult, PaginatorError> {
        match self {
            Self::Unthreaded { paginator, .. } => {
                paginator.paginate_backward(num_events.into()).await
            }
            Self::Threaded(threaded_paginator) => {
                threaded_paginator.paginate_backwards(num_events.into()).await
            }
        }
    }

    /// Runs a forward pagination (requesting `num_events` to the server), from
    /// the current state of the object.
    ///
    /// Will return immediately if we have already hit the end of the timeline.
    ///
    /// May return an error if it's already paginating, or if the call to
    /// the homeserver endpoints failed.
    pub async fn paginate_forwards(
        &self,
        num_events: u16,
    ) -> Result<PaginationResult, PaginatorError> {
        match self {
            Self::Unthreaded { paginator, .. } => {
                paginator.paginate_forward(num_events.into()).await
            }
            Self::Threaded(threaded_paginator) => {
                threaded_paginator.paginate_forwards(num_events.into()).await
            }
        }
    }

    /// Whether to hide in-thread events from the timeline.
    pub fn hide_threaded_events(&self) -> bool {
        match self {
            Self::Unthreaded { hide_threaded_events, .. } => *hide_threaded_events,
            Self::Threaded(_) => false,
        }
    }

    /// Returns the root event id of the thread, if the paginator is
    /// [`AnyPaginator::Threaded`].
    pub fn thread_root(&self) -> Option<&EventId> {
        match self {
            Self::Unthreaded { .. } => None,
            Self::Threaded(thread_events_loader) => {
                Some(thread_events_loader.thread_root_event_id())
            }
        }
    }
}

impl<P: RoomDataProvider> TimelineFocusKind<P> {
    /// Returns the [`ReceiptThread`] that should be used for the current
    /// timeline focus.
    ///
    /// Live and event timelines will use the unthreaded read receipt type in
    /// general, unless they hide in-thread events, in which case they will
    /// use the main thread.
    pub(super) fn receipt_thread(&self) -> ReceiptThread {
        if let Some(thread_root) = self.thread_root() {
            ReceiptThread::Thread(thread_root.to_owned())
        } else if self.hide_threaded_events() {
            ReceiptThread::Main
        } else {
            ReceiptThread::Unthreaded
        }
    }

    /// Whether to hide in-thread events from the timeline.
    fn hide_threaded_events(&self) -> bool {
        match self {
            TimelineFocusKind::Live { hide_threaded_events } => *hide_threaded_events,
            TimelineFocusKind::Event { paginator } => {
                paginator.get().is_some_and(|paginator| paginator.hide_threaded_events())
            }
            TimelineFocusKind::Thread { .. } | TimelineFocusKind::PinnedEvents { .. } => false,
        }
    }

    /// Whether the focus is on a thread (from a live thread or a thread
    /// permalink).
    fn is_thread(&self) -> bool {
        self.thread_root().is_some()
    }

    /// If the focus is a thread, returns its root event ID.
    fn thread_root(&self) -> Option<&EventId> {
        match self {
            TimelineFocusKind::Event { paginator, .. } => {
                paginator.get().and_then(|paginator| paginator.thread_root())
            }
            TimelineFocusKind::Live { .. } | TimelineFocusKind::PinnedEvents { .. } => None,
            TimelineFocusKind::Thread { root_event_id } => Some(root_event_id),
        }
    }
}

#[derive(Clone, Debug)]
pub(super) struct TimelineController<P: RoomDataProvider = Room> {
    /// Inner mutable state.
    state: Arc<RwLock<TimelineState<P>>>,

    /// Focus data.
    focus: Arc<TimelineFocusKind<P>>,

    /// A [`RoomDataProvider`] implementation, providing data.
    ///
    /// The type is a `RoomDataProvider` to allow testing. In the real world,
    /// this would normally be a [`Room`].
    pub(crate) room_data_provider: P,

    /// Settings applied to this timeline.
    pub(super) settings: TimelineSettings,
}

#[derive(Clone)]
pub(super) struct TimelineSettings {
    /// Should the read receipts and read markers be handled and on which event
    /// types?
    pub(super) track_read_receipts: TimelineReadReceiptTracking,

    /// Event filter that controls what's rendered as a timeline item (and thus
    /// what can carry read receipts).
    pub(super) event_filter: Arc<TimelineEventFilterFn>,

    /// Are unparsable events added as timeline items of their own kind?
    pub(super) add_failed_to_parse: bool,

    /// Should the timeline items be grouped by day or month?
    pub(super) date_divider_mode: DateDividerMode,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for TimelineSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TimelineSettings")
            .field("track_read_receipts", &self.track_read_receipts)
            .field("add_failed_to_parse", &self.add_failed_to_parse)
            .finish_non_exhaustive()
    }
}

impl Default for TimelineSettings {
    fn default() -> Self {
        Self {
            track_read_receipts: TimelineReadReceiptTracking::Disabled,
            event_filter: Arc::new(default_event_filter),
            add_failed_to_parse: true,
            date_divider_mode: DateDividerMode::Daily,
        }
    }
}

/// The default event filter for
/// [`crate::timeline::TimelineBuilder::event_filter`].
///
/// It filters out events that are not rendered by the timeline, including but
/// not limited to: reactions, edits, redactions on existing messages.
///
/// If you have a custom filter, it may be best to chain yours with this one if
/// you do not want to run into situations where a read receipt is not visible
/// because it's living on an event that doesn't have a matching timeline item.
pub fn default_event_filter(event: &AnySyncTimelineEvent, rules: &RoomVersionRules) -> bool {
    match event {
        AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomRedaction(ev)) => {
            if ev.redacts(&rules.redaction).is_some() {
                // This is a redaction of an existing message, we'll only update the previous
                // message and not render a new entry.
                false
            } else {
                // This is a redacted entry, that we'll show only if the redacted entity wasn't
                // a reaction.
                ev.event_type() != MessageLikeEventType::Reaction
            }
        }

        AnySyncTimelineEvent::MessageLike(msg) => {
            match msg.original_content() {
                None => {
                    // This is a redacted entry, that we'll show only if the redacted entity wasn't
                    // a reaction.
                    msg.event_type() != MessageLikeEventType::Reaction
                }

                Some(original_content) => {
                    match original_content {
                        AnyMessageLikeEventContent::RoomMessage(content) => {
                            if content
                                .relates_to
                                .as_ref()
                                .is_some_and(|rel| matches!(rel, Relation::Replacement(_)))
                            {
                                // Edits aren't visible by default.
                                return false;
                            }

                            match content.msgtype {
                                MessageType::Audio(_)
                                | MessageType::Emote(_)
                                | MessageType::File(_)
                                | MessageType::Image(_)
                                | MessageType::Location(_)
                                | MessageType::Notice(_)
                                | MessageType::ServerNotice(_)
                                | MessageType::Text(_)
                                | MessageType::Video(_)
                                | MessageType::VerificationRequest(_) => true,
                                #[cfg(feature = "unstable-msc4274")]
                                MessageType::Gallery(_) => true,
                                _ => false,
                            }
                        }

                        AnyMessageLikeEventContent::Sticker(_)
                        | AnyMessageLikeEventContent::UnstablePollStart(
                            UnstablePollStartEventContent::New(_),
                        )
                        | AnyMessageLikeEventContent::CallInvite(_)
                        | AnyMessageLikeEventContent::RtcNotification(_)
                        | AnyMessageLikeEventContent::RoomEncrypted(_) => true,

                        _ => false,
                    }
                }
            }
        }

        AnySyncTimelineEvent::State(_) => {
            // All the state events may get displayed by default.
            true
        }
    }
}

impl<P: RoomDataProvider> TimelineController<P> {
    pub(super) fn new(
        room_data_provider: P,
        focus: TimelineFocus,
        internal_id_prefix: Option<String>,
        unable_to_decrypt_hook: Option<Arc<UtdHookManager>>,
        is_room_encrypted: bool,
        settings: TimelineSettings,
    ) -> Self {
        let focus = match focus {
            TimelineFocus::Live { hide_threaded_events } => {
                TimelineFocusKind::Live { hide_threaded_events }
            }

            TimelineFocus::Event { .. } => TimelineFocusKind::Event { paginator: OnceCell::new() },

            TimelineFocus::Thread { root_event_id, .. } => {
                TimelineFocusKind::Thread { root_event_id }
            }

            TimelineFocus::PinnedEvents { max_events_to_load, max_concurrent_requests } => {
                TimelineFocusKind::PinnedEvents {
                    loader: PinnedEventsLoader::new(
                        Arc::new(room_data_provider.clone()),
                        max_events_to_load as usize,
                        max_concurrent_requests as usize,
                    ),
                }
            }
        };

        let focus = Arc::new(focus);
        let state = Arc::new(RwLock::new(TimelineState::new(
            focus.clone(),
            room_data_provider.own_user_id().to_owned(),
            room_data_provider.room_version_rules(),
            internal_id_prefix,
            unable_to_decrypt_hook,
            is_room_encrypted,
        )));

        Self { state, focus, room_data_provider, settings }
    }

    /// Initializes the configured focus with appropriate data.
    ///
    /// Should be called only once after creation of the [`TimelineInner`], with
    /// all its fields set.
    ///
    /// Returns whether there were any events added to the timeline.
    pub(super) async fn init_focus(
        &self,
        focus: &TimelineFocus,
        room_event_cache: &RoomEventCache,
    ) -> Result<bool, Error> {
        match focus {
            TimelineFocus::Live { .. } => {
                // Retrieve the cached events, and add them to the timeline.
                let events = room_event_cache.events().await?;

                let has_events = !events.is_empty();

                self.replace_with_initial_remote_events(events, RemoteEventOrigin::Cache).await;

                match room_event_cache.pagination().status().get() {
                    RoomPaginationStatus::Idle { hit_timeline_start } => {
                        if hit_timeline_start {
                            // Eagerly insert the timeline start item, since pagination claims
                            // we've already hit the timeline start.
                            self.insert_timeline_start_if_missing().await;
                        }
                    }
                    RoomPaginationStatus::Paginating => {}
                }

                Ok(has_events)
            }

            TimelineFocus::Event { target: event_id, num_context_events, thread_mode } => {
                let TimelineFocusKind::Event { paginator, .. } = &*self.focus else {
                    // NOTE: this is sync'd with code in the ctor.
                    unreachable!();
                };

                let event_paginator = Paginator::new(self.room_data_provider.clone());

                let load_events_with_context = || async {
                    // Start a /context request to load the focused event and surrounding events.
                    event_paginator
                        .start_from(event_id, (*num_context_events).into())
                        .await
                        .map(|r| r.events)
                        .map_err(PaginationError::Paginator)
                };

                let events = if *num_context_events == 0 {
                    // If no context is requested, try to load the event from the cache first and
                    // include common relations such as reactions and edits.
                    let request_config = Some(RequestConfig::default().retry_limit(3));
                    let relations_filter =
                        Some(vec![RelationType::Annotation, RelationType::Replacement]);

                    // Load the event from the cache or, failing that, the server.
                    match self
                        .room_data_provider
                        .load_event_with_relations(event_id, request_config, relations_filter)
                        .await
                    {
                        Ok((event, related_events)) => {
                            let mut events = vec![event];
                            events.extend(related_events);
                            events
                        }
                        Err(err) => {
                            error!("error when loading focussed event: {err}");
                            // Fall back to load the focused event using /context.
                            load_events_with_context().await?
                        }
                    }
                } else {
                    // Start a /context request to load the focussed event and surrounding events.
                    load_events_with_context().await?
                };

                // Find the target event, and see if it's part of a thread.
                let extracted_thread_root = events
                    .iter()
                    .find(
                        |event| {
                            if let Some(id) = event.event_id() { id == *event_id } else { false }
                        },
                    )
                    .and_then(|event| extract_thread_root(event.raw()));

                // Determine the timeline's threading behavior.
                let (thread_root_event_id, hide_threaded_events) = match thread_mode {
                    TimelineEventFocusThreadMode::ForceThread => {
                        // If the event is part of a thread, use its thread root. Otherwise,
                        // assume the event itself is the thread root.
                        (extracted_thread_root.or_else(|| Some(event_id.clone())), false)
                    }
                    TimelineEventFocusThreadMode::Automatic { hide_threaded_events } => {
                        (extracted_thread_root, *hide_threaded_events)
                    }
                };

                let _ = paginator.set(match thread_root_event_id {
                    Some(root_id) => {
                        let mut tokens = event_paginator.tokens();

                        // Look if the thread root event is part of the /context response. This
                        // allows us to spare some backwards pagination with
                        // /relations.
                        let includes_root_event = events.iter().any(|event| {
                            if let Some(id) = event.event_id() { id == root_id } else { false }
                        });

                        if includes_root_event {
                            // If we have the root event, there's no need to do back-paginations
                            // with /relations, since we are at the start of the thread.
                            tokens.previous = PaginationToken::HitEnd;
                        }

                        AnyPaginator::Threaded(ThreadedEventsLoader::new(
                            self.room_data_provider.clone(),
                            root_id,
                            tokens,
                        ))
                    }

                    None => AnyPaginator::Unthreaded {
                        paginator: event_paginator,
                        hide_threaded_events,
                    },
                });

                let has_events = !events.is_empty();

                match paginator.get().expect("Paginator was not instantiated") {
                    AnyPaginator::Unthreaded { .. } => {
                        self.replace_with_initial_remote_events(
                            events,
                            RemoteEventOrigin::Pagination,
                        )
                        .await;
                    }

                    AnyPaginator::Threaded(threaded_events_loader) => {
                        // We filter only events that are part of the thread (including the root),
                        // since /context will return adjacent events without filters.
                        let thread_root = threaded_events_loader.thread_root_event_id();
                        let events_in_thread = events.into_iter().filter(|event| {
                            extract_thread_root(event.raw())
                                .is_some_and(|event_thread_root| event_thread_root == thread_root)
                                || event.event_id().as_deref() == Some(thread_root)
                        });

                        self.replace_with_initial_remote_events(
                            events_in_thread,
                            RemoteEventOrigin::Pagination,
                        )
                        .await;
                    }
                }

                Ok(has_events)
            }

            TimelineFocus::Thread { root_event_id, .. } => {
                let (events, _) =
                    room_event_cache.subscribe_to_thread(root_event_id.clone()).await?;
                let has_events = !events.is_empty();

                // For each event, we also need to find the related events, as they don't
                // include the thread relationship, they won't be included in
                // the initial list of events.
                let mut related_events = Vector::new();
                for event_id in events.iter().filter_map(|event| event.event_id()) {
                    if let Some((_original, related)) =
                        room_event_cache.find_event_with_relations(&event_id, None).await?
                    {
                        related_events.extend(related);
                    }
                }

                self.replace_with_initial_remote_events(events, RemoteEventOrigin::Cache).await;

                // Now that we've inserted the thread events, add the aggregations too.
                if !related_events.is_empty() {
                    self.handle_remote_aggregations(
                        vec![VectorDiff::Append { values: related_events }],
                        RemoteEventOrigin::Cache,
                    )
                    .await;
                }

                Ok(has_events)
            }

            TimelineFocus::PinnedEvents { .. } => {
                let TimelineFocusKind::PinnedEvents { loader } = &*self.focus else {
                    // NOTE: this is sync'd with code in the ctor.
                    unreachable!();
                };

                let Some(loaded_events) =
                    loader.load_events().await.map_err(Error::PinnedEventsError)?
                else {
                    // There wasn't any events.
                    return Ok(false);
                };

                let has_events = !loaded_events.is_empty();

                self.replace_with_initial_remote_events(
                    loaded_events,
                    RemoteEventOrigin::Pagination,
                )
                .await;

                Ok(has_events)
            }
        }
    }

    /// Listens to encryption state changes for the room in
    /// [`matrix_sdk_base::RoomInfo`] and applies the new value to the
    /// existing timeline items. This will then cause a refresh of those
    /// timeline items.
    pub async fn handle_encryption_state_changes(&self) {
        let mut room_info = self.room_data_provider.room_info();

        // Small function helper to help mark as encrypted.
        let mark_encrypted = || async {
            let mut state = self.state.write().await;
            state.meta.is_room_encrypted = true;
            state.mark_all_events_as_encrypted();
        };

        if room_info.get().encryption_state().is_encrypted() {
            // If the room was already encrypted, it won't toggle to unencrypted, so we can
            // shut down this task early.
            mark_encrypted().await;
            return;
        }

        while let Some(info) = room_info.next().await {
            if info.encryption_state().is_encrypted() {
                mark_encrypted().await;
                // Once the room is encrypted, it cannot switch back to unencrypted, so our work
                // here is done.
                break;
            }
        }
    }

    pub(crate) async fn reload_pinned_events(
        &self,
    ) -> Result<Option<Vec<TimelineEvent>>, PinnedEventsLoaderError> {
        if let TimelineFocusKind::PinnedEvents { loader } = &*self.focus {
            loader.load_events().await
        } else {
            Err(PinnedEventsLoaderError::TimelineFocusNotPinnedEvents)
        }
    }

    /// Run a lazy backwards pagination (in live mode).
    ///
    /// It adjusts the `count` value of the `Skip` higher-order stream so that
    /// more items are pushed front in the timeline.
    ///
    /// If no more items are available (i.e. if the `count` is zero), this
    /// method returns `Some(needs)` where `needs` is the number of events that
    /// must be unlazily backwards paginated.
    pub(super) async fn live_lazy_paginate_backwards(&self, num_events: u16) -> Option<usize> {
        let state = self.state.read().await;

        let (count, needs) = state
            .meta
            .subscriber_skip_count
            .compute_next_when_paginating_backwards(num_events.into());

        // This always happens on a live timeline.
        let is_live_timeline = true;
        state.meta.subscriber_skip_count.update(count, is_live_timeline);

        needs
    }

    /// Run a backwards pagination (in focused mode) and append the results to
    /// the timeline.
    ///
    /// Returns whether we hit the start of the timeline.
    pub(super) async fn focused_paginate_backwards(
        &self,
        num_events: u16,
    ) -> Result<bool, PaginationError> {
        let PaginationResult { events, hit_end_of_timeline } = match &*self.focus {
            TimelineFocusKind::Live { .. }
            | TimelineFocusKind::PinnedEvents { .. }
            | TimelineFocusKind::Thread { .. } => {
                return Err(PaginationError::NotSupported);
            }
            TimelineFocusKind::Event { paginator, .. } => paginator
                .get()
                .expect("Paginator was not instantiated")
                .paginate_backwards(num_events)
                .await
                .map_err(PaginationError::Paginator)?,
        };

        // Events are in reverse topological order.
        // We can push front each event individually.
        self.handle_remote_events_with_diffs(
            events.into_iter().map(|event| VectorDiff::PushFront { value: event }).collect(),
            RemoteEventOrigin::Pagination,
        )
        .await;

        Ok(hit_end_of_timeline)
    }

    /// Run a forwards pagination (in focused mode) and append the results to
    /// the timeline.
    ///
    /// Returns whether we hit the end of the timeline.
    pub(super) async fn focused_paginate_forwards(
        &self,
        num_events: u16,
    ) -> Result<bool, PaginationError> {
        let PaginationResult { events, hit_end_of_timeline } = match &*self.focus {
            TimelineFocusKind::Live { .. }
            | TimelineFocusKind::PinnedEvents { .. }
            | TimelineFocusKind::Thread { .. } => return Err(PaginationError::NotSupported),

            TimelineFocusKind::Event { paginator, .. } => paginator
                .get()
                .expect("Paginator was not instantiated")
                .paginate_forwards(num_events)
                .await
                .map_err(PaginationError::Paginator)?,
        };

        // Events are in topological order.
        // We can append all events with no transformation.
        self.handle_remote_events_with_diffs(
            vec![VectorDiff::Append { values: events.into() }],
            RemoteEventOrigin::Pagination,
        )
        .await;

        Ok(hit_end_of_timeline)
    }

    /// Is this timeline receiving events from sync (aka has a live focus)?
    pub(super) fn is_live(&self) -> bool {
        matches!(&*self.focus, TimelineFocusKind::Live { .. })
    }

    /// Is this timeline focused on a thread?
    pub(super) fn is_threaded(&self) -> bool {
        self.focus.is_thread()
    }

    /// The root of the current thread, for a live thread timeline or a
    /// permalink to a thread message.
    pub(super) fn thread_root(&self) -> Option<OwnedEventId> {
        self.focus.thread_root().map(ToOwned::to_owned)
    }

    /// Get a copy of the current items in the list.
    ///
    /// Cheap because `im::Vector` is cheap to clone.
    pub(super) async fn items(&self) -> Vector<Arc<TimelineItem>> {
        self.state.read().await.items.clone_items()
    }

    #[cfg(test)]
    pub(super) async fn subscribe_raw(
        &self,
    ) -> (Vector<Arc<TimelineItem>>, VectorSubscriberStream<Arc<TimelineItem>>) {
        self.state.read().await.items.subscribe().into_values_and_stream()
    }

    pub(super) async fn subscribe(&self) -> (Vector<Arc<TimelineItem>>, TimelineSubscriber) {
        let state = self.state.read().await;

        TimelineSubscriber::new(&state.items, &state.meta.subscriber_skip_count)
    }

    pub(super) async fn subscribe_filter_map<U, F>(
        &self,
        f: F,
    ) -> (Vector<U>, FilterMap<VectorSubscriberStream<Arc<TimelineItem>>, F>)
    where
        U: Clone,
        F: Fn(Arc<TimelineItem>) -> Option<U>,
    {
        self.state.read().await.items.subscribe().filter_map(f)
    }

    /// Toggle a reaction locally.
    ///
    /// Returns true if the reaction was added, false if it was removed.
    #[instrument(skip_all)]
    pub(super) async fn toggle_reaction_local(
        &self,
        item_id: &TimelineEventItemId,
        key: &str,
    ) -> Result<bool, Error> {
        let mut state = self.state.write().await;

        let Some((item_pos, item)) = rfind_event_by_item_id(&state.items, item_id) else {
            warn!("Timeline item not found, can't add reaction");
            return Err(Error::FailedToToggleReaction);
        };

        let user_id = self.room_data_provider.own_user_id();
        let prev_status = item
            .content()
            .reactions()
            .and_then(|map| Some(map.get(key)?.get(user_id)?.status.clone()));

        let Some(prev_status) = prev_status else {
            // Adding the new reaction.
            match item.handle() {
                TimelineItemHandle::Local(send_handle) => {
                    if send_handle
                        .react(key.to_owned())
                        .await
                        .map_err(|err| Error::SendQueueError(err.into()))?
                        .is_some()
                    {
                        trace!("adding a reaction to a local echo");
                        return Ok(true);
                    }

                    warn!("couldn't toggle reaction for local echo");
                    return Ok(false);
                }

                TimelineItemHandle::Remote(event_id) => {
                    // Add a reaction through the room data provider.
                    // No need to reflect the effect locally, since the local echo handling will
                    // take care of it.
                    trace!("adding a reaction to a remote echo");
                    let annotation = Annotation::new(event_id.to_owned(), key.to_owned());
                    self.room_data_provider
                        .send(ReactionEventContent::from(annotation).into())
                        .await?;
                    return Ok(true);
                }
            }
        };

        trace!("removing a previous reaction");
        match prev_status {
            ReactionStatus::LocalToLocal(send_reaction_handle) => {
                if let Some(handle) = send_reaction_handle {
                    if !handle.abort().await.map_err(|err| Error::SendQueueError(err.into()))? {
                        // Impossible state: the reaction has moved from local to echo under our
                        // feet, but the timeline was supposed to be locked!
                        warn!("unexpectedly unable to abort sending of local reaction");
                    }
                } else {
                    warn!("no send reaction handle (this should only happen in testing contexts)");
                }
            }

            ReactionStatus::LocalToRemote(send_handle) => {
                // No need to reflect the change ourselves, since handling the discard of the
                // local echo will take care of it.
                trace!("aborting send of the previous reaction that was a local echo");
                if let Some(handle) = send_handle {
                    if !handle.abort().await.map_err(|err| Error::SendQueueError(err.into()))? {
                        // Impossible state: the reaction has moved from local to echo under our
                        // feet, but the timeline was supposed to be locked!
                        warn!("unexpectedly unable to abort sending of local reaction");
                    }
                } else {
                    warn!("no send handle (this should only happen in testing contexts)");
                }
            }

            ReactionStatus::RemoteToRemote(event_id) => {
                // Assume the redaction will work; we'll re-add the reaction if it didn't.
                let Some(annotated_event_id) =
                    item.as_remote().map(|event_item| event_item.event_id.clone())
                else {
                    warn!("remote reaction to remote event, but the associated item isn't remote");
                    return Ok(false);
                };

                let mut reactions = item.content().reactions().cloned().unwrap_or_default();
                let reaction_info = reactions.remove_reaction(user_id, key);

                if reaction_info.is_some() {
                    let new_item = item.with_reactions(reactions);
                    state.items.replace(item_pos, new_item);
                } else {
                    warn!(
                        "reaction is missing on the item, not removing it locally, \
                         but sending redaction."
                    );
                }

                // Release the lock before running the request.
                drop(state);

                trace!("sending redact for a previous reaction");
                if let Err(err) = self.room_data_provider.redact(&event_id, None, None).await {
                    if let Some(reaction_info) = reaction_info {
                        debug!("sending redact failed, adding the reaction back to the list");

                        let mut state = self.state.write().await;
                        if let Some((item_pos, item)) =
                            rfind_event_by_id(&state.items, &annotated_event_id)
                        {
                            // Re-add the reaction to the mapping.
                            let mut reactions =
                                item.content().reactions().cloned().unwrap_or_default();
                            reactions
                                .entry(key.to_owned())
                                .or_default()
                                .insert(user_id.to_owned(), reaction_info);
                            let new_item = item.with_reactions(reactions);
                            state.items.replace(item_pos, new_item);
                        } else {
                            warn!(
                                "couldn't find item to re-add reaction anymore; \
                                 maybe it's been redacted?"
                            );
                        }
                    }

                    return Err(err);
                }
            }
        }

        Ok(false)
    }

    /// Handle updates on events as [`VectorDiff`]s.
    pub(super) async fn handle_remote_events_with_diffs(
        &self,
        diffs: Vec<VectorDiff<TimelineEvent>>,
        origin: RemoteEventOrigin,
    ) {
        if diffs.is_empty() {
            return;
        }

        let mut state = self.state.write().await;
        state
            .handle_remote_events_with_diffs(
                diffs,
                origin,
                &self.room_data_provider,
                &self.settings,
            )
            .await
    }

    /// Only handle aggregations received as [`VectorDiff`]s.
    pub(super) async fn handle_remote_aggregations(
        &self,
        diffs: Vec<VectorDiff<TimelineEvent>>,
        origin: RemoteEventOrigin,
    ) {
        if diffs.is_empty() {
            return;
        }

        let mut state = self.state.write().await;
        state
            .handle_remote_aggregations(diffs, origin, &self.room_data_provider, &self.settings)
            .await
    }

    pub(super) async fn clear(&self) {
        self.state.write().await.clear();
    }

    /// Replaces the content of the current timeline with initial events.
    ///
    /// Also sets up read receipts and the read marker for a live timeline of a
    /// room.
    ///
    /// This is all done with a single lock guard, since we don't want the state
    /// to be modified between the clear and re-insertion of new events.
    pub(super) async fn replace_with_initial_remote_events<Events>(
        &self,
        events: Events,
        origin: RemoteEventOrigin,
    ) where
        Events: IntoIterator,
        <Events as IntoIterator>::Item: Into<TimelineEvent>,
    {
        let mut state = self.state.write().await;

        let track_read_markers = &self.settings.track_read_receipts;
        if track_read_markers.is_enabled() {
            state.populate_initial_user_receipt(&self.room_data_provider, ReceiptType::Read).await;
            state
                .populate_initial_user_receipt(&self.room_data_provider, ReceiptType::ReadPrivate)
                .await;
        }

        // Replace the events if either the current event list or the new one aren't
        // empty.
        // Previously we just had to check the new one wasn't empty because
        // we did a clear operation before so the current one would always be empty, but
        // now we may want to replace a populated timeline with an empty one.
        let mut events = events.into_iter().peekable();
        if !state.items.is_empty() || events.peek().is_some() {
            state
                .replace_with_remote_events(
                    events,
                    origin,
                    &self.room_data_provider,
                    &self.settings,
                )
                .await;
        }

        if track_read_markers.is_enabled() {
            if let Some(fully_read_event_id) =
                self.room_data_provider.load_fully_read_marker().await
            {
                state.handle_fully_read_marker(fully_read_event_id);
            } else if let Some(latest_receipt_event_id) = state
                .latest_user_read_receipt_timeline_event_id(self.room_data_provider.own_user_id())
            {
                // Fall back to read receipt if no fully read marker exists.
                debug!("no `m.fully_read` marker found, falling back to read receipt");
                state.handle_fully_read_marker(latest_receipt_event_id);
            }
        }
    }

    pub(super) async fn handle_fully_read_marker(&self, fully_read_event_id: OwnedEventId) {
        self.state.write().await.handle_fully_read_marker(fully_read_event_id);
    }

    pub(super) async fn handle_ephemeral_events(
        &self,
        events: Vec<Raw<AnySyncEphemeralRoomEvent>>,
    ) {
        // Don't even take the lock if there are no events to process.
        if events.is_empty() {
            return;
        }
        let mut state = self.state.write().await;
        state.handle_ephemeral_events(events, &self.room_data_provider).await;
    }

    /// Creates the local echo for an event we're sending.
    #[instrument(skip_all)]
    pub(super) async fn handle_local_event(
        &self,
        txn_id: OwnedTransactionId,
        content: AnyMessageLikeEventContent,
        send_handle: Option<SendHandle>,
    ) {
        let sender = self.room_data_provider.own_user_id().to_owned();
        let profile = self.room_data_provider.profile_from_user_id(&sender).await;

        let date_divider_mode = self.settings.date_divider_mode.clone();

        let mut state = self.state.write().await;
        state
            .handle_local_event(sender, profile, date_divider_mode, txn_id, send_handle, content)
            .await;
    }

    /// Update the send state of a local event represented by a transaction ID.
    ///
    /// If the corresponding local timeline item is missing, a warning is
    /// raised.
    #[instrument(skip(self))]
    pub(super) async fn update_event_send_state(
        &self,
        txn_id: &TransactionId,
        send_state: EventSendState,
    ) {
        let mut state = self.state.write().await;
        let mut txn = state.transaction();

        let new_event_id: Option<&EventId> =
            as_variant!(&send_state, EventSendState::Sent { event_id } => event_id);

        // The local echoes are always at the end of the timeline, we must first make
        // sure the remote echo hasn't showed up yet.
        if rfind_event_item(&txn.items, |it| {
            new_event_id.is_some() && it.event_id() == new_event_id && it.as_remote().is_some()
        })
        .is_some()
        {
            // Remote echo already received. This is very unlikely.
            trace!("Remote echo received before send-event response");

            let local_echo = rfind_event_item(&txn.items, |it| it.transaction_id() == Some(txn_id));

            // If there's both the remote echo and a local echo, that means the
            // remote echo was received before the response *and* contained no
            // transaction ID (and thus duplicated the local echo).
            if let Some((idx, _)) = local_echo {
                warn!("Message echo got duplicated, removing the local one");
                txn.items.remove(idx);

                // Adjust the date dividers, if needs be.
                let mut adjuster =
                    DateDividerAdjuster::new(self.settings.date_divider_mode.clone());
                adjuster.run(&mut txn.items, &mut txn.meta);
            }

            txn.commit();
            return;
        }

        // Look for the local event by the transaction ID or event ID.
        let result = rfind_event_item(&txn.items, |it| {
            it.transaction_id() == Some(txn_id)
                || new_event_id.is_some()
                    && it.event_id() == new_event_id
                    && it.as_local().is_some()
        });

        let Some((idx, item)) = result else {
            // Event wasn't found as a standalone item.
            //
            // If it was just sent, try to find if it matches a corresponding aggregation,
            // and mark it as sent in that case.
            if let Some(new_event_id) = new_event_id {
                if txn.meta.aggregations.mark_aggregation_as_sent(
                    txn_id.to_owned(),
                    new_event_id.to_owned(),
                    &mut txn.items,
                    &txn.meta.room_version_rules,
                ) {
                    trace!("Aggregation marked as sent");
                    txn.commit();
                    return;
                }

                trace!("Sent aggregation was not found");
            }

            warn!("Timeline item not found, can't update send state");
            return;
        };

        let Some(local_item) = item.as_local() else {
            warn!("We looked for a local item, but it transitioned to remote.");
            return;
        };

        // The event was already marked as sent, that's a broken state, let's
        // emit an error but also override to the given sent state.
        if let EventSendState::Sent { event_id: existing_event_id } = &local_item.send_state {
            error!(?existing_event_id, ?new_event_id, "Local echo already marked as sent");
        }

        // If the event has just been marked as sent, update the aggregations mapping to
        // take that into account.
        if let Some(new_event_id) = new_event_id {
            txn.meta.aggregations.mark_target_as_sent(txn_id.to_owned(), new_event_id.to_owned());
        }

        let new_item = item.with_inner_kind(local_item.with_send_state(send_state));
        txn.items.replace(idx, new_item);

        txn.commit();
    }

    pub(super) async fn discard_local_echo(&self, txn_id: &TransactionId) -> bool {
        let mut state = self.state.write().await;

        if let Some((idx, _)) =
            rfind_event_item(&state.items, |it| it.transaction_id() == Some(txn_id))
        {
            let mut txn = state.transaction();

            txn.items.remove(idx);

            // A read marker or a date divider may have been inserted before the local echo.
            // Ensure both are up to date.
            let mut adjuster = DateDividerAdjuster::new(self.settings.date_divider_mode.clone());
            adjuster.run(&mut txn.items, &mut txn.meta);

            txn.meta.update_read_marker(&mut txn.items);

            txn.commit();

            debug!("discarded local echo");
            return true;
        }

        // Avoid multiple mutable and immutable borrows of the lock guard by explicitly
        // dereferencing it once.
        let mut txn = state.transaction();

        // Look if this was a local aggregation.
        let found_aggregation = match txn.meta.aggregations.try_remove_aggregation(
            &TimelineEventItemId::TransactionId(txn_id.to_owned()),
            &mut txn.items,
        ) {
            Ok(val) => val,
            Err(err) => {
                warn!("error when discarding local echo for an aggregation: {err}");
                // The aggregation has been found, it's just that we couldn't discard it.
                true
            }
        };

        if found_aggregation {
            txn.commit();
        }

        found_aggregation
    }

    pub(super) async fn replace_local_echo(
        &self,
        txn_id: &TransactionId,
        content: AnyMessageLikeEventContent,
    ) -> bool {
        let AnyMessageLikeEventContent::RoomMessage(content) = content else {
            // Ideally, we'd support replacing local echoes for a reaction, etc., but
            // handling RoomMessage should be sufficient in most cases. Worst
            // case, the local echo will be sent Soon™ and we'll get another chance at
            // editing the event then.
            warn!("Replacing a local echo for a non-RoomMessage-like event NYI");
            return false;
        };

        let mut state = self.state.write().await;
        let mut txn = state.transaction();

        let Some((idx, prev_item)) =
            rfind_event_item(&txn.items, |it| it.transaction_id() == Some(txn_id))
        else {
            debug!("Can't find local echo to replace");
            return false;
        };

        // Reuse the previous local echo's state, but reset the send state to not sent
        // (per API contract).
        let ti_kind = {
            let Some(prev_local_item) = prev_item.as_local() else {
                warn!("We looked for a local item, but it transitioned as remote??");
                return false;
            };
            // If the local echo had an upload progress, retain it.
            let progress = as_variant!(&prev_local_item.send_state,
                EventSendState::NotSentYet { progress } => progress.clone())
            .flatten();
            prev_local_item.with_send_state(EventSendState::NotSentYet { progress })
        };

        // Replace the local-related state (kind) and the content state.
        let new_item = TimelineItem::new(
            prev_item.with_kind(ti_kind).with_content(TimelineItemContent::message(
                content.msgtype,
                content.mentions,
                prev_item.content().reactions().cloned().unwrap_or_default(),
                prev_item.content().thread_root(),
                prev_item.content().in_reply_to(),
                prev_item.content().thread_summary(),
            )),
            prev_item.internal_id.to_owned(),
        );

        txn.items.replace(idx, new_item);

        // This doesn't change the original sending time, so there's no need to adjust
        // date dividers.

        txn.commit();

        debug!("Replaced local echo");
        true
    }

    pub(super) async fn compute_redecryption_candidates(
        &self,
    ) -> (BTreeSet<String>, BTreeSet<String>) {
        let state = self.state.read().await;
        compute_redecryption_candidates(&state.items)
    }

    pub(super) async fn set_sender_profiles_pending(&self) {
        self.set_non_ready_sender_profiles(TimelineDetails::Pending).await;
    }

    pub(super) async fn set_sender_profiles_error(&self, error: Arc<matrix_sdk::Error>) {
        self.set_non_ready_sender_profiles(TimelineDetails::Error(error)).await;
    }

    async fn set_non_ready_sender_profiles(&self, profile_state: TimelineDetails<Profile>) {
        self.state.write().await.items.for_each(|mut entry| {
            let Some(event_item) = entry.as_event() else { return };
            if !matches!(event_item.sender_profile(), TimelineDetails::Ready(_)) {
                let new_item = entry.with_kind(TimelineItemKind::Event(
                    event_item.with_sender_profile(profile_state.clone()),
                ));
                ObservableItemsEntry::replace(&mut entry, new_item);
            }
        });
    }

    pub(super) async fn update_missing_sender_profiles(&self) {
        trace!("Updating missing sender profiles");

        let mut state = self.state.write().await;
        let mut entries = state.items.entries();
        while let Some(mut entry) = entries.next() {
            let Some(event_item) = entry.as_event() else { continue };
            let event_id = event_item.event_id().map(debug);
            let transaction_id = event_item.transaction_id().map(debug);

            if event_item.sender_profile().is_ready() {
                trace!(event_id, transaction_id, "Profile already set");
                continue;
            }

            match self.room_data_provider.profile_from_user_id(event_item.sender()).await {
                Some(profile) => {
                    trace!(event_id, transaction_id, "Adding profile");
                    let updated_item =
                        event_item.with_sender_profile(TimelineDetails::Ready(profile));
                    let new_item = entry.with_kind(updated_item);
                    ObservableItemsEntry::replace(&mut entry, new_item);
                }
                None => {
                    if !event_item.sender_profile().is_unavailable() {
                        trace!(event_id, transaction_id, "Marking profile unavailable");
                        let updated_item =
                            event_item.with_sender_profile(TimelineDetails::Unavailable);
                        let new_item = entry.with_kind(updated_item);
                        ObservableItemsEntry::replace(&mut entry, new_item);
                    } else {
                        debug!(event_id, transaction_id, "Profile already marked unavailable");
                    }
                }
            }
        }

        trace!("Done updating missing sender profiles");
    }

    /// Update the profiles of the given senders, even if they are ready.
    pub(super) async fn force_update_sender_profiles(&self, sender_ids: &BTreeSet<&UserId>) {
        trace!("Forcing update of sender profiles: {sender_ids:?}");

        let mut state = self.state.write().await;
        let mut entries = state.items.entries();
        while let Some(mut entry) = entries.next() {
            let Some(event_item) = entry.as_event() else { continue };
            if !sender_ids.contains(event_item.sender()) {
                continue;
            }

            let event_id = event_item.event_id().map(debug);
            let transaction_id = event_item.transaction_id().map(debug);

            match self.room_data_provider.profile_from_user_id(event_item.sender()).await {
                Some(profile) => {
                    if matches!(event_item.sender_profile(), TimelineDetails::Ready(old_profile) if *old_profile == profile)
                    {
                        debug!(event_id, transaction_id, "Profile already up-to-date");
                    } else {
                        trace!(event_id, transaction_id, "Updating profile");
                        let updated_item =
                            event_item.with_sender_profile(TimelineDetails::Ready(profile));
                        let new_item = entry.with_kind(updated_item);
                        ObservableItemsEntry::replace(&mut entry, new_item);
                    }
                }
                None => {
                    if !event_item.sender_profile().is_unavailable() {
                        trace!(event_id, transaction_id, "Marking profile unavailable");
                        let updated_item =
                            event_item.with_sender_profile(TimelineDetails::Unavailable);
                        let new_item = entry.with_kind(updated_item);
                        ObservableItemsEntry::replace(&mut entry, new_item);
                    } else {
                        debug!(event_id, transaction_id, "Profile already marked unavailable");
                    }
                }
            }
        }

        trace!("Done forcing update of sender profiles");
    }

    #[cfg(test)]
    pub(super) async fn handle_read_receipts(&self, receipt_event_content: ReceiptEventContent) {
        let own_user_id = self.room_data_provider.own_user_id();
        self.state.write().await.handle_read_receipts(receipt_event_content, own_user_id);
    }

    /// Get the latest read receipt for the given user.
    ///
    /// Useful to get the latest read receipt, whether it's private or public.
    pub(super) async fn latest_user_read_receipt(
        &self,
        user_id: &UserId,
    ) -> Option<(OwnedEventId, Receipt)> {
        let receipt_thread = self.focus.receipt_thread();

        self.state
            .read()
            .await
            .latest_user_read_receipt(user_id, receipt_thread, &self.room_data_provider)
            .await
    }

    /// Get the ID of the timeline event with the latest read receipt for the
    /// given user.
    pub(super) async fn latest_user_read_receipt_timeline_event_id(
        &self,
        user_id: &UserId,
    ) -> Option<OwnedEventId> {
        self.state.read().await.latest_user_read_receipt_timeline_event_id(user_id)
    }

    /// Subscribe to changes in the read receipts of our own user.
    pub async fn subscribe_own_user_read_receipts_changed(
        &self,
    ) -> impl Stream<Item = ()> + use<P> {
        self.state.read().await.meta.read_receipts.subscribe_own_user_read_receipts_changed()
    }

    /// Handle a room send update that's a new local echo.
    pub(crate) async fn handle_local_echo(&self, echo: LocalEcho) {
        match echo.content {
            LocalEchoContent::Event { serialized_event, send_handle, send_error } => {
                let content = match serialized_event.deserialize() {
                    Ok(d) => d,
                    Err(err) => {
                        warn!("error deserializing local echo: {err}");
                        return;
                    }
                };

                self.handle_local_event(echo.transaction_id.clone(), content, Some(send_handle))
                    .await;

                if let Some(send_error) = send_error {
                    self.update_event_send_state(
                        &echo.transaction_id,
                        EventSendState::SendingFailed {
                            error: Arc::new(matrix_sdk::Error::SendQueueWedgeError(Box::new(
                                send_error,
                            ))),
                            is_recoverable: false,
                        },
                    )
                    .await;
                }
            }

            LocalEchoContent::React { key, send_handle, applies_to } => {
                self.handle_local_reaction(key, send_handle, applies_to).await;
            }
        }
    }

    /// Adds a reaction (local echo) to a local echo.
    #[instrument(skip(self, send_handle))]
    async fn handle_local_reaction(
        &self,
        reaction_key: String,
        send_handle: SendReactionHandle,
        applies_to: OwnedTransactionId,
    ) {
        let mut state = self.state.write().await;
        let mut tr = state.transaction();

        let target = TimelineEventItemId::TransactionId(applies_to);

        let reaction_txn_id = send_handle.transaction_id().to_owned();
        let reaction_status = ReactionStatus::LocalToLocal(Some(send_handle));
        let aggregation = Aggregation::new(
            TimelineEventItemId::TransactionId(reaction_txn_id),
            AggregationKind::Reaction {
                key: reaction_key.clone(),
                sender: self.room_data_provider.own_user_id().to_owned(),
                timestamp: MilliSecondsSinceUnixEpoch::now(),
                reaction_status,
            },
        );

        tr.meta.aggregations.add(target.clone(), aggregation.clone());
        find_item_and_apply_aggregation(
            &tr.meta.aggregations,
            &mut tr.items,
            &target,
            aggregation,
            &tr.meta.room_version_rules,
        );

        tr.commit();
    }

    /// Handle a single room send queue update.
    pub(crate) async fn handle_room_send_queue_update(&self, update: RoomSendQueueUpdate) {
        match update {
            RoomSendQueueUpdate::NewLocalEvent(echo) => {
                self.handle_local_echo(echo).await;
            }

            RoomSendQueueUpdate::CancelledLocalEvent { transaction_id } => {
                if !self.discard_local_echo(&transaction_id).await {
                    warn!("couldn't find the local echo to discard");
                }
            }

            RoomSendQueueUpdate::ReplacedLocalEvent { transaction_id, new_content } => {
                let content = match new_content.deserialize() {
                    Ok(d) => d,
                    Err(err) => {
                        warn!("error deserializing local echo (upon edit): {err}");
                        return;
                    }
                };

                if !self.replace_local_echo(&transaction_id, content).await {
                    warn!("couldn't find the local echo to replace");
                }
            }

            RoomSendQueueUpdate::SendError { transaction_id, error, is_recoverable } => {
                self.update_event_send_state(
                    &transaction_id,
                    EventSendState::SendingFailed { error, is_recoverable },
                )
                .await;
            }

            RoomSendQueueUpdate::RetryEvent { transaction_id } => {
                self.update_event_send_state(
                    &transaction_id,
                    EventSendState::NotSentYet { progress: None },
                )
                .await;
            }

            RoomSendQueueUpdate::SentEvent { transaction_id, event_id } => {
                self.update_event_send_state(&transaction_id, EventSendState::Sent { event_id })
                    .await;
            }

            RoomSendQueueUpdate::MediaUpload { related_to, index, progress, .. } => {
                self.update_event_send_state(
                    &related_to,
                    EventSendState::NotSentYet {
                        progress: Some(MediaUploadProgress { index, progress }),
                    },
                )
                .await;
            }
        }
    }

    /// Insert a timeline start item at the beginning of the room, if it's
    /// missing.
    pub async fn insert_timeline_start_if_missing(&self) {
        let mut state = self.state.write().await;
        let mut txn = state.transaction();
        txn.items.push_timeline_start_if_missing(
            txn.meta.new_timeline_item(VirtualTimelineItem::TimelineStart),
        );
        txn.commit();
    }

    /// Create a [`EmbeddedEvent`] from an arbitrary event, be it in the
    /// timeline or not.
    ///
    /// Can be `None` if the event cannot be represented as a standalone item,
    /// because it's an aggregation.
    pub(super) async fn make_replied_to(
        &self,
        event: TimelineEvent,
    ) -> Result<Option<EmbeddedEvent>, Error> {
        let state = self.state.read().await;
        EmbeddedEvent::try_from_timeline_event(event, &self.room_data_provider, &state.meta).await
    }
}

impl TimelineController {
    pub(super) fn room(&self) -> &Room {
        &self.room_data_provider
    }

    /// Given an event identifier, will fetch the details for the event it's
    /// replying to, if applicable.
    #[instrument(skip(self))]
    pub(super) async fn fetch_in_reply_to_details(&self, event_id: &EventId) -> Result<(), Error> {
        let state_guard = self.state.write().await;
        let (index, item) = rfind_event_by_id(&state_guard.items, event_id)
            .ok_or(Error::EventNotInTimeline(TimelineEventItemId::EventId(event_id.to_owned())))?;
        let remote_item = item
            .as_remote()
            .ok_or(Error::EventNotInTimeline(TimelineEventItemId::EventId(event_id.to_owned())))?
            .clone();

        let TimelineItemContent::MsgLike(msglike) = item.content().clone() else {
            debug!("Event is not a message");
            return Ok(());
        };
        let Some(in_reply_to) = msglike.in_reply_to.clone() else {
            debug!("Event is not a reply");
            return Ok(());
        };
        if let TimelineDetails::Pending = &in_reply_to.event {
            debug!("Replied-to event is already being fetched");
            return Ok(());
        }
        if let TimelineDetails::Ready(_) = &in_reply_to.event {
            debug!("Replied-to event has already been fetched");
            return Ok(());
        }

        let internal_id = item.internal_id.to_owned();
        let item = item.clone();
        let event = fetch_replied_to_event(
            state_guard,
            &self.state,
            index,
            &item,
            internal_id,
            &msglike,
            &in_reply_to.event_id,
            self.room(),
        )
        .await?;

        // We need to be sure to have the latest position of the event as it might have
        // changed while waiting for the request.
        let mut state = self.state.write().await;
        let (index, item) = rfind_event_by_id(&state.items, &remote_item.event_id)
            .ok_or(Error::EventNotInTimeline(TimelineEventItemId::EventId(event_id.to_owned())))?;

        // Check the state of the event again, it might have been redacted while
        // the request was in-flight.
        let TimelineItemContent::MsgLike(MsgLikeContent {
            kind: MsgLikeKind::Message(message),
            reactions,
            thread_root,
            in_reply_to,
            thread_summary,
        }) = item.content().clone()
        else {
            info!("Event is no longer a message (redacted?)");
            return Ok(());
        };
        let Some(in_reply_to) = in_reply_to else {
            warn!("Event no longer has a reply (bug?)");
            return Ok(());
        };

        // Now that we've received the content of the replied-to event, replace the
        // replied-to content in the item with it.
        trace!("Updating in-reply-to details");
        let internal_id = item.internal_id.to_owned();
        let mut item = item.clone();
        item.set_content(TimelineItemContent::MsgLike(MsgLikeContent {
            kind: MsgLikeKind::Message(message),
            reactions,
            thread_root,
            in_reply_to: Some(InReplyToDetails { event_id: in_reply_to.event_id, event }),
            thread_summary,
        }));
        state.items.replace(index, TimelineItem::new(item, internal_id));

        Ok(())
    }

    /// Returns the thread that should be used for a read receipt based on the
    /// current focus of the timeline and the receipt type.
    ///
    /// A `SendReceiptType::FullyRead` will always use
    /// `ReceiptThread::Unthreaded`
    pub(super) fn infer_thread_for_read_receipt(
        &self,
        receipt_type: &SendReceiptType,
    ) -> ReceiptThread {
        if matches!(receipt_type, SendReceiptType::FullyRead) {
            ReceiptThread::Unthreaded
        } else {
            self.focus.receipt_thread()
        }
    }

    /// Check whether the given receipt should be sent.
    ///
    /// Returns `false` if the given receipt is older than the current one.
    pub(super) async fn should_send_receipt(
        &self,
        receipt_type: &SendReceiptType,
        receipt_thread: &ReceiptThread,
        event_id: &EventId,
    ) -> bool {
        let own_user_id = self.room().own_user_id();
        let state = self.state.read().await;
        let room = self.room();

        match receipt_type {
            SendReceiptType::Read => {
                if let Some((old_pub_read, _)) = state
                    .meta
                    .user_receipt(
                        own_user_id,
                        ReceiptType::Read,
                        receipt_thread.clone(),
                        room,
                        state.items.all_remote_events(),
                    )
                    .await
                {
                    trace!(%old_pub_read, "found a previous public receipt");
                    if let Some(relative_pos) = TimelineMetadata::compare_events_positions(
                        &old_pub_read,
                        event_id,
                        state.items.all_remote_events(),
                    ) {
                        trace!(
                            "event referred to new receipt is {relative_pos:?} the previous receipt"
                        );
                        return relative_pos == RelativePosition::After;
                    }
                }
            }

            // Implicit read receipts are saved as public read receipts, so get the latest. It also
            // doesn't make sense to have a private read receipt behind a public one.
            SendReceiptType::ReadPrivate => {
                if let Some((old_priv_read, _)) =
                    state.latest_user_read_receipt(own_user_id, receipt_thread.clone(), room).await
                {
                    trace!(%old_priv_read, "found a previous private receipt");
                    if let Some(relative_pos) = TimelineMetadata::compare_events_positions(
                        &old_priv_read,
                        event_id,
                        state.items.all_remote_events(),
                    ) {
                        trace!(
                            "event referred to new receipt is {relative_pos:?} the previous receipt"
                        );
                        return relative_pos == RelativePosition::After;
                    }
                }
            }

            SendReceiptType::FullyRead => {
                if let Some(prev_event_id) = self.room_data_provider.load_fully_read_marker().await
                    && let Some(relative_pos) = TimelineMetadata::compare_events_positions(
                        &prev_event_id,
                        event_id,
                        state.items.all_remote_events(),
                    )
                {
                    return relative_pos == RelativePosition::After;
                }
            }

            _ => {}
        }

        // Let the server handle unknown receipts.
        true
    }

    /// Returns the latest event identifier, even if it's not visible, or if
    /// it's folded into another timeline item.
    pub(crate) async fn latest_event_id(&self) -> Option<OwnedEventId> {
        let state = self.state.read().await;
        let filter_out_thread_events = match self.focus() {
            TimelineFocusKind::Thread { .. } => false,
            TimelineFocusKind::Live { hide_threaded_events } => hide_threaded_events.to_owned(),
            TimelineFocusKind::Event { paginator } => {
                paginator.get().is_some_and(|paginator| paginator.hide_threaded_events())
            }
            _ => true,
        };

        // In some timelines, threaded events are added to the `AllRemoteEvents`
        // collection since they need to be taken into account to calculate read
        // receipts, but we don't want to actually take them into account for returning
        // the latest event id since they're not visibly in the timeline
        state
            .items
            .all_remote_events()
            .iter()
            .rev()
            .filter_map(|item| {
                if !filter_out_thread_events || item.thread_root_id.is_none() {
                    Some(item.event_id.clone())
                } else {
                    None
                }
            })
            .next()
    }

    #[instrument(skip(self), fields(room_id = ?self.room().room_id()))]
    pub(super) async fn retry_event_decryption(&self, session_ids: Option<BTreeSet<String>>) {
        let (utds, decrypted) = self.compute_redecryption_candidates().await;

        let request = DecryptionRetryRequest {
            room_id: self.room().room_id().to_owned(),
            utd_session_ids: utds,
            refresh_info_session_ids: decrypted,
        };

        self.room().client().event_cache().request_decryption(request);
    }

    /// Combine the global (event cache) pagination status with the local state
    /// of the timeline.
    ///
    /// This only changes the global pagination status of this room, in one
    /// case: if the timeline has a skip count greater than 0, it will
    /// ensure that the pagination status says that we haven't reached the
    /// timeline start yet.
    pub(super) async fn map_pagination_status(
        &self,
        status: RoomPaginationStatus,
    ) -> RoomPaginationStatus {
        match status {
            RoomPaginationStatus::Idle { hit_timeline_start } => {
                if hit_timeline_start {
                    let state = self.state.read().await;
                    // If the skip count is greater than 0, it means that a subsequent pagination
                    // could return more items, so pretend we didn't get the information that the
                    // timeline start was hit.
                    if state.meta.subscriber_skip_count.get() > 0 {
                        return RoomPaginationStatus::Idle { hit_timeline_start: false };
                    }
                }
            }
            RoomPaginationStatus::Paginating => {}
        }

        // You're perfect, just the way you are.
        status
    }
}

impl<P: RoomDataProvider> TimelineController<P> {
    /// Returns the timeline focus of the [`TimelineController`].
    pub(super) fn focus(&self) -> &TimelineFocusKind<P> {
        &self.focus
    }
}

#[allow(clippy::too_many_arguments)]
async fn fetch_replied_to_event<P: RoomDataProvider>(
    mut state_guard: RwLockWriteGuard<'_, TimelineState<P>>,
    state_lock: &RwLock<TimelineState<P>>,
    index: usize,
    item: &EventTimelineItem,
    internal_id: TimelineUniqueId,
    msglike: &MsgLikeContent,
    in_reply_to: &EventId,
    room: &Room,
) -> Result<TimelineDetails<Box<EmbeddedEvent>>, Error> {
    if let Some((_, item)) = rfind_event_by_id(&state_guard.items, in_reply_to) {
        let details = TimelineDetails::Ready(Box::new(EmbeddedEvent::from_timeline_item(&item)));
        trace!("Found replied-to event locally");
        return Ok(details);
    }

    // Replace the item with a new timeline item that has the fetching status of the
    // replied-to event to pending.
    trace!("Setting in-reply-to details to pending");
    let in_reply_to_details =
        InReplyToDetails { event_id: in_reply_to.to_owned(), event: TimelineDetails::Pending };

    let event_item = item
        .with_content(TimelineItemContent::MsgLike(msglike.with_in_reply_to(in_reply_to_details)));

    let new_timeline_item = TimelineItem::new(event_item, internal_id);
    state_guard.items.replace(index, new_timeline_item);

    // Don't hold the state lock while the network request is made.
    drop(state_guard);

    trace!("Fetching replied-to event");
    let res = match room.load_or_fetch_event(in_reply_to, None).await {
        Ok(timeline_event) => {
            let state = state_lock.read().await;

            let replied_to_item =
                EmbeddedEvent::try_from_timeline_event(timeline_event, room, &state.meta).await?;

            if let Some(item) = replied_to_item {
                TimelineDetails::Ready(Box::new(item))
            } else {
                // The replied-to item is an aggregation, not a standalone item.
                return Err(Error::UnsupportedEvent);
            }
        }

        Err(e) => TimelineDetails::Error(Arc::new(e)),
    };

    Ok(res)
}
