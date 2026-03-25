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

use std::sync::Arc;

use eyeball::{ObservableWriteGuard, SharedObservable, Subscriber};
use eyeball_im::{ObservableVector, VectorDiff, VectorSubscriberBatchedStream};
use futures_util::future::join_all;
use imbl::Vector;
use matrix_sdk::{
    Result, Room,
    deserialized_responses::TimelineEvent,
    event_cache::{RoomEventCacheSubscriber, RoomEventCacheUpdate},
    locks::Mutex,
    paginators::PaginationToken,
    room::ListThreadsOptions,
    task_monitor::BackgroundTaskHandle,
};
use matrix_sdk_common::serde_helpers::extract_thread_root;
use ruma::{
    MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedUserId,
    events::{
        AnyMessageLikeEventContent, relation::Replacement, room::message::RoomMessageEventContent,
    },
};
use tokio::sync::Mutex as AsyncMutex;
use tracing::{error, trace, warn};

use crate::timeline::{
    Profile, TimelineDetails, TimelineItemContent,
    event_handler::{HandleAggregationKind, TimelineAction},
    traits::RoomDataProvider,
};

/// Each `ThreadListItem` represents one thread root event in the room. The
/// fields are pre-resolved from the raw homeserver response: the sender's
/// profile is fetched eagerly and the event content is parsed into a
/// [`TimelineItemContent`] so that consumers can render the item without any
/// additional work.
///
/// `ThreadListItem`s are accumulated inside
/// [`super::thread_list_service::ThreadListService`] as pages are fetched via
/// [`super::thread_list_service::ThreadListService::paginate`].
#[derive(Clone, Debug)]
pub struct ThreadListItem {
    /// The thread root event.
    pub root_event: ThreadListItemEvent,

    /// The latest event in the thread (i.e. the most recent reply), if
    /// available.
    ///
    /// This is initially populated from the server's bundled thread summary
    /// and is updated in real time as new events arrive via sync.
    pub latest_event: Option<ThreadListItemEvent>,

    /// The number of replies in this thread (excluding the root event).
    ///
    /// This is initially populated from the server's bundled thread summary
    /// and is updated in real time as new events arrive via sync.
    pub num_replies: u32,
}

/// Information about an event in a thread (either the root or the latest
/// reply).
#[derive(Clone, Debug)]
pub struct ThreadListItemEvent {
    /// The event ID.
    pub event_id: OwnedEventId,

    /// The timestamp of the event.
    pub timestamp: MilliSecondsSinceUnixEpoch,

    /// The sender of the event.
    pub sender: OwnedUserId,

    /// Whether the event was sent by the current user.
    pub is_own: bool,

    /// The sender's profile (display name and avatar URL).
    pub sender_profile: TimelineDetails<Profile>,

    /// The parsed content of the event, if available.
    ///
    /// `None` when the event could not be deserialized into a known
    /// [`TimelineItemContent`] variant (e.g. an unsupported or redacted event
    /// type).
    pub content: Option<TimelineItemContent>,
}

/// The pagination state of a [`ThreadListService`].
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ThreadListPaginationState {
    /// The list is idle (not currently loading).
    Idle {
        /// Whether the end of the thread list has been reached (no more pages
        /// to load).
        end_reached: bool,
    },
    /// The list is currently loading the next page.
    Loading,
}

/// An error that occurred while using a [`ThreadListService`].
#[derive(Debug, thiserror::Error)]
pub enum ThreadListServiceError {
    /// An error from the underlying Matrix SDK.
    #[error(transparent)]
    Sdk(#[from] matrix_sdk::Error),
}

/// A paginated list of threads for a given room.
///
/// `ThreadListService` provides an observable, paginated list of
/// [`ThreadListItem`]s. It exposes methods to paginate forward through the
/// thread list as well as subscribe to state changes.
///
/// When created, the service automatically starts a background task that
/// listens to room event cache updates (from `/sync` and other sources).
/// Whenever a new event belonging to a known thread arrives, the service
/// updates that thread's `latest_event` and `num_replies` fields in real time,
/// emitting observable diffs to all subscribers.
///
/// # Example
///
/// ```no_run
/// use matrix_sdk::Room;
/// use matrix_sdk_ui::timeline::thread_list_service::{
///     ThreadListPaginationState, ThreadListService,
/// };
///
/// # async {
/// # let room: Room = todo!();
/// let service = ThreadListService::new(room);
///
/// assert_eq!(
///     service.pagination_state(),
///     ThreadListPaginationState::Idle { end_reached: false }
/// );
///
/// service.paginate().await.unwrap();
///
/// let items = service.items();
/// # anyhow::Ok(()) };
/// ```
pub struct ThreadListService {
    /// The room whose threads are being listed.
    room: Room,

    /// The pagination token used to fetch subsequent pages.
    token: AsyncMutex<PaginationToken>,

    /// The current pagination state.
    pagination_state: SharedObservable<ThreadListPaginationState>,

    /// The current list of thread items.
    items: Arc<Mutex<ObservableVector<ThreadListItem>>>,

    /// Handle to the background task listening for event cache updates.
    /// Dropping this aborts the task.
    _event_cache_task: BackgroundTaskHandle,
}

impl ThreadListService {
    /// Creates a new [`ThreadListService`] for the given room.
    ///
    /// This immediately spawns a background task that listens to the room's
    /// event cache for live updates. The task self-bootstraps by performing
    /// the async event cache subscription internally.
    pub fn new(room: Room) -> Self {
        let items: Arc<Mutex<ObservableVector<ThreadListItem>>> =
            Arc::new(Mutex::new(ObservableVector::new()));

        // Eagerly subscribe the event cache to sync responses (this is a cheap,
        // synchronous, idempotent call).
        if let Err(e) = room.client().event_cache().subscribe() {
            warn!("ThreadListService: failed to subscribe event cache to sync: {e}");
        }

        let event_cache_task = room
            .client()
            .task_monitor()
            .spawn_background_task("thread_list_service::event_cache_listener", {
                let room = room.clone();
                let items = items.clone();
                async move {
                    // Obtain the room event cache and a subscriber.
                    let (_event_cache_drop, mut subscriber) = match async {
                        let (room_event_cache, drop_handles) = room.event_cache().await?;
                        let (_, subscriber) = room_event_cache.subscribe().await?;
                        matrix_sdk::event_cache::Result::Ok((drop_handles, subscriber))
                    }
                    .await
                    {
                        Ok(pair) => pair,
                        Err(e) => {
                            error!(
                                "ThreadListService: failed to subscribe to room event cache, \
                                 live updates will not work: {e}"
                            );
                            return;
                        }
                    };

                    trace!("ThreadListService: event cache listener started");

                    Self::event_cache_listener_loop(&room, &mut subscriber, items).await;
                }
            })
            .abort_on_drop();

        Self {
            room,
            token: AsyncMutex::new(PaginationToken::None),
            pagination_state: SharedObservable::new(ThreadListPaginationState::Idle {
                end_reached: false,
            }),
            items,
            _event_cache_task: event_cache_task,
        }
    }

    /// Returns the current pagination state.
    pub fn pagination_state(&self) -> ThreadListPaginationState {
        self.pagination_state.get()
    }

    /// Subscribes to pagination state updates.
    ///
    /// The returned [`Subscriber`] will emit a new value every time the
    /// pagination state changes.
    pub fn subscribe_to_pagination_state_updates(&self) -> Subscriber<ThreadListPaginationState> {
        self.pagination_state.subscribe()
    }

    /// Returns the current list of thread items as a snapshot.
    pub fn items(&self) -> Vec<ThreadListItem> {
        self.items.lock().iter().cloned().collect()
    }

    /// Subscribes to updates of the thread item list.
    ///
    /// Returns a snapshot of the current items alongside a batched stream of
    /// [`eyeball_im::VectorDiff`]s that describe subsequent changes.
    pub fn subscribe_to_items_updates(
        &self,
    ) -> (Vector<ThreadListItem>, VectorSubscriberBatchedStream<ThreadListItem>) {
        self.items.lock().subscribe().into_values_and_batched_stream()
    }

    /// Fetches the next page of threads, appending the results to the item
    /// list.
    ///
    /// - If the list is already loading or the end has been reached, this
    ///   method returns immediately with `Ok(())`.
    /// - On a network/SDK error the pagination state is reset to `Idle {
    ///   end_reached: false }` and the error is propagated.
    pub async fn paginate(&self) -> Result<(), ThreadListServiceError> {
        // Guard: do nothing if we are already loading or have reached the end.
        {
            let mut pagination_state = self.pagination_state.write();

            match *pagination_state {
                ThreadListPaginationState::Idle { end_reached: true }
                | ThreadListPaginationState::Loading => return Ok(()),
                _ => {}
            }

            ObservableWriteGuard::set(&mut pagination_state, ThreadListPaginationState::Loading);
        }

        let mut pagination_token = self.token.lock().await;

        // Build the options for this page, using the current token if we have one.
        let from = match &*pagination_token {
            PaginationToken::HasMore(token) => Some(token.clone()),
            _ => None,
        };

        let opts = ListThreadsOptions { from, ..Default::default() };

        match self.load_thread_list(opts).await {
            Ok(thread_list) => {
                // Update the pagination token based on whether there are more pages.
                *pagination_token = match &thread_list.prev_batch_token {
                    Some(token) => PaginationToken::HasMore(token.clone()),
                    None => PaginationToken::HitEnd,
                };

                let end_reached = thread_list.prev_batch_token.is_none();

                // Append new items to the observable vector.
                self.items.lock().append(thread_list.items.into());

                self.pagination_state.set(ThreadListPaginationState::Idle { end_reached });

                Ok(())
            }
            Err(err) => {
                self.pagination_state.set(ThreadListPaginationState::Idle { end_reached: false });
                Err(ThreadListServiceError::Sdk(err))
            }
        }
    }

    /// Resets the service back to its initial state.
    ///
    /// Clears all loaded items, discards the current pagination token, and
    /// sets the pagination state to `Idle { end_reached: false }`.  The next
    /// call to [`Self::paginate`] will therefore start from the beginning of
    /// the thread list.
    pub async fn reset(&self) {
        let mut pagination_token = self.token.lock().await;
        *pagination_token = PaginationToken::None;

        self.items.lock().clear();

        self.pagination_state.set(ThreadListPaginationState::Idle { end_reached: false });
    }

    async fn load_thread_list(&self, opts: ListThreadsOptions) -> Result<ThreadList> {
        let thread_roots = self.room.list_threads(opts).await?;

        let list_items = join_all(
            thread_roots
                .chunk
                .into_iter()
                .map(|timeline_event| Self::build_thread_list_item(&self.room, timeline_event))
                .collect::<Vec<_>>(),
        )
        .await
        .into_iter()
        .flatten()
        .collect();

        Ok(ThreadList { items: list_items, prev_batch_token: thread_roots.prev_batch_token })
    }

    async fn build_thread_list_item(
        room: &Room,
        timeline_event: TimelineEvent,
    ) -> Option<ThreadListItem> {
        // Extract thread summary info before consuming the event.
        let thread_summary = timeline_event.thread_summary.summary().cloned();
        let bundled_latest_thread_event = timeline_event.bundled_latest_thread_event.clone();

        // Build the root event using the same logic as latest events.
        let root_event = Self::build_event(room, timeline_event).await?;

        // Build the latest event from the bundled thread summary, if available.
        let num_replies = thread_summary.as_ref().map(|s| s.num_replies).unwrap_or(0);

        let latest_event = if let Some(ev) = bundled_latest_thread_event.map(|b| *b) {
            Self::build_event(room, ev).await
        } else {
            None
        };

        Some(ThreadListItem { root_event, latest_event, num_replies })
    }

    /// Build a [`ThreadListItemEvent`] from a [`TimelineEvent`].
    async fn build_event(
        room: &Room,
        timeline_event: TimelineEvent,
    ) -> Option<ThreadListItemEvent> {
        let event_id = timeline_event.event_id()?;
        let timestamp = timeline_event.timestamp()?;
        let sender = timeline_event.sender()?;
        let is_own = room.own_user_id() == sender;

        let raw = timeline_event.into_raw();
        let deserialized = match raw.deserialize() {
            Ok(ev) => ev,
            Err(e) => {
                error!("Failed deserializing thread event for latest_event: {e}");
                return None;
            }
        };

        let sender_profile = room
            .profile_from_user_id(&sender)
            .await
            .map(TimelineDetails::Ready)
            .unwrap_or(TimelineDetails::Unavailable);

        let content: Option<TimelineItemContent> = match TimelineAction::from_event(
            deserialized,
            &raw,
            room,
            None,
            None,
            None,
            None,
        )
        .await
        {
            Some(TimelineAction::AddItem { content }) => Some(content),
            Some(TimelineAction::HandleAggregation {
                kind: HandleAggregationKind::Edit { replacement: Replacement { new_content, .. } },
                ..
            }) => {
                match TimelineAction::from_content(
                    AnyMessageLikeEventContent::RoomMessage(RoomMessageEventContent::new(
                        new_content.msgtype,
                    )),
                    None,
                    None,
                    None,
                ) {
                    TimelineAction::AddItem { content } => Some(content),
                    _ => None,
                }
            }
            _ => None,
        };

        Some(ThreadListItemEvent { event_id, timestamp, sender, is_own, sender_profile, content })
    }

    /// The main loop of the event-cache listener task.
    ///
    /// Listens for [`RoomEventCacheUpdate`]s and, for each new timeline event
    /// that belongs to a thread we are tracking, updates the corresponding
    /// [`ThreadListItem`]'s `latest_event` and `num_replies`.
    async fn event_cache_listener_loop(
        room: &Room,
        subscriber: &mut RoomEventCacheSubscriber,
        items: Arc<Mutex<ObservableVector<ThreadListItem>>>,
    ) {
        use tokio::sync::broadcast::error::RecvError;

        loop {
            let update = match subscriber.recv().await {
                Ok(update) => update,
                Err(RecvError::Closed) => {
                    error!("ThreadListService: event cache channel closed, stopping listener");
                    break;
                }
                Err(RecvError::Lagged(n)) => {
                    warn!("ThreadListService: lagged behind {n} event cache updates");
                    continue;
                }
            };

            if let RoomEventCacheUpdate::UpdateTimelineEvents(timeline_diffs) = update {
                let new_events = Self::collect_events_from_diffs(timeline_diffs.diffs);

                for event in new_events {
                    // Check if this event has a thread relation pointing to a known root.
                    let Some(thread_root) = extract_thread_root(event.raw()) else { continue };

                    // Find the position of this thread root in our list.
                    let position = {
                        let guard = items.lock();
                        guard.iter().position(|item| item.root_event.event_id == thread_root)
                    };

                    if let Some(index) = position {
                        // Build the latest event representation from the raw event.
                        if let Some(latest_event) = Self::build_event(room, event).await {
                            let mut guard = items.lock();

                            // Re-check the position — the vector may have changed while
                            // we were awaiting the profile lookup above.
                            if index < guard.len()
                                && guard[index].root_event.event_id == thread_root
                            {
                                let mut updated = guard[index].clone();
                                updated.latest_event = Some(latest_event);
                                updated.num_replies = updated.num_replies.saturating_add(1);
                                guard.set(index, updated);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Extracts all events from a list of [`VectorDiff`]s.
    fn collect_events_from_diffs(
        diffs: Vec<VectorDiff<matrix_sdk_base::event_cache::Event>>,
    ) -> Vec<matrix_sdk_base::event_cache::Event> {
        let mut events = Vec::new();

        for diff in diffs {
            match diff {
                VectorDiff::Append { values } => events.extend(values),
                VectorDiff::PushBack { value }
                | VectorDiff::PushFront { value }
                | VectorDiff::Insert { value, .. }
                | VectorDiff::Set { value, .. } => events.push(value),
                VectorDiff::Reset { values } => events.extend(values),
                // These diffs don't carry new events.
                VectorDiff::Clear
                | VectorDiff::PopBack
                | VectorDiff::PopFront
                | VectorDiff::Remove { .. }
                | VectorDiff::Truncate { .. } => {}
            }
        }

        events
    }
}

/// A structure wrapping a Thread List endpoint response i.e.
/// [`ThreadListItem`]s and the current pagination token.
#[derive(Clone, Debug)]
struct ThreadList {
    /// The thread-root events that belong to this page of results.
    pub items: Vec<ThreadListItem>,

    /// Opaque pagination token returned by the homeserver.
    pub prev_batch_token: Option<String>,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures_util::pin_mut;
    use matrix_sdk::test_utils::mocks::MatrixMockServer;
    use matrix_sdk_test::{async_test, event_factory::EventFactory};
    use ruma::{event_id, events::AnyTimelineEvent, room_id, serde::Raw, user_id};
    use serde_json::json;
    use stream_assert::{assert_next_matches, assert_pending};
    use wiremock::ResponseTemplate;

    use super::{ThreadListPaginationState, ThreadListService};

    #[async_test]
    async fn test_initial_state() {
        let server = MatrixMockServer::new().await;
        let service = make_service(&server).await;

        assert_eq!(
            service.pagination_state(),
            ThreadListPaginationState::Idle { end_reached: false }
        );
        assert!(service.items().is_empty());
    }

    #[async_test]
    async fn test_pagination() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");
        let sender_id = user_id!("@alice:b.c");

        let f = EventFactory::new().room(room_id).sender(sender_id);

        let eid1 = event_id!("$1");
        let eid2 = event_id!("$2");

        server
            .mock_room_threads()
            .ok(
                vec![f.text_msg("Thread root 1").event_id(eid1).into_raw()],
                Some("next_page_token".to_owned()),
            )
            .mock_once()
            .mount()
            .await;

        server
            .mock_room_threads()
            .match_from("next_page_token")
            .ok(vec![f.text_msg("Thread root 2").event_id(eid2).into_raw()], None)
            .mock_once()
            .mount()
            .await;

        let room = server.sync_joined_room(&client, room_id).await;
        let service = ThreadListService::new(room);

        service.paginate().await.expect("first paginate failed");

        assert_eq!(
            service.pagination_state(),
            ThreadListPaginationState::Idle { end_reached: false }
        );
        assert_eq!(service.items().len(), 1);
        assert_eq!(service.items()[0].root_event.event_id, eid1);

        service.paginate().await.expect("second paginate failed");

        assert_eq!(
            service.pagination_state(),
            ThreadListPaginationState::Idle { end_reached: true }
        );
        assert_eq!(service.items().len(), 2);
        assert_eq!(service.items()[1].root_event.event_id, eid2);
    }

    #[async_test]
    async fn test_pagination_end_reached() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");
        let sender_id = user_id!("@alice:b.c");
        let f = EventFactory::new().room(room_id).sender(sender_id);
        let eid1 = event_id!("$1");

        server
            .mock_room_threads()
            .ok(vec![f.text_msg("Thread root").event_id(eid1).into_raw()], None)
            .mock_once()
            .mount()
            .await;

        let room = server.sync_joined_room(&client, room_id).await;
        let service = ThreadListService::new(room);

        service.paginate().await.expect("paginate failed");
        assert_eq!(
            service.pagination_state(),
            ThreadListPaginationState::Idle { end_reached: true }
        );
        assert_eq!(service.items().len(), 1);

        service.paginate().await.expect("second paginate should be a no-op");
        assert_eq!(service.items().len(), 1);
        assert_eq!(
            service.pagination_state(),
            ThreadListPaginationState::Idle { end_reached: true }
        );
    }

    /// Two concurrent calls to [`ThreadListService::paginate`] must not result
    /// in two concurrent HTTP requests. The second call should detect that a
    /// pagination is already in progress (state is `Loading`) and return
    /// immediately without making another network request.
    #[async_test]
    async fn test_concurrent_pagination_is_not_possible() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");
        let sender_id = user_id!("@alice:b.c");
        let f = EventFactory::new().room(room_id).sender(sender_id);
        let eid1 = event_id!("$1");

        // Set up a slow mock response so both `paginate()` calls overlap in
        // flight. Using `expect(1)` means the test will panic during server
        // teardown if the endpoint is hit more than once.
        let chunk: Vec<Raw<AnyTimelineEvent>> =
            vec![f.text_msg("Thread root").event_id(eid1).into_raw()];
        server
            .mock_room_threads()
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "chunk": chunk, "next_batch": null }))
                    .set_delay(Duration::from_millis(100)),
            )
            .expect(1)
            .mount()
            .await;

        let room = server.sync_joined_room(&client, room_id).await;
        let service = ThreadListService::new(room);

        // Run two paginations concurrently.
        let (first, second) = tokio::join!(service.paginate(), service.paginate());

        first.expect("first paginate should succeed");
        second.expect("second (concurrent) paginate should succeed as a no-op");

        // Only one HTTP request was made, so we have exactly one item.
        assert_eq!(service.items().len(), 1);
        assert_eq!(service.items()[0].root_event.event_id, eid1);
        assert_eq!(
            service.pagination_state(),
            ThreadListPaginationState::Idle { end_reached: true }
        );
    }

    /// When the server returns an error, [`ThreadListService::paginate`] must
    /// propagate the error *and* reset the pagination state back to
    /// `Idle { end_reached: false }` so that the caller can retry.
    #[async_test]
    async fn test_pagination_error() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");

        server.mock_room_threads().error500().mock_once().mount().await;

        let room = server.sync_joined_room(&client, room_id).await;
        let service = ThreadListService::new(room);

        // Pagination must surface the server error.
        service.paginate().await.expect_err("paginate should fail on a 500 response");

        // The state must be reset so the caller can retry; it must *not* be
        // stuck in `Loading`.
        assert_eq!(
            service.pagination_state(),
            ThreadListPaginationState::Idle { end_reached: false }
        );

        // No items should have been added.
        assert!(service.items().is_empty());
    }

    #[async_test]
    async fn test_reset() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");
        let sender_id = user_id!("@alice:b.c");
        let f = EventFactory::new().room(room_id).sender(sender_id);
        let eid1 = event_id!("$1");

        server
            .mock_room_threads()
            .ok(vec![f.text_msg("Thread root").event_id(eid1).into_raw()], None)
            .expect(2)
            .mount()
            .await;

        let room = server.sync_joined_room(&client, room_id).await;
        let service = ThreadListService::new(room);

        service.paginate().await.expect("first paginate failed");
        assert_eq!(service.items().len(), 1);
        assert_eq!(
            service.pagination_state(),
            ThreadListPaginationState::Idle { end_reached: true }
        );

        service.reset().await;
        assert!(service.items().is_empty());
        assert_eq!(
            service.pagination_state(),
            ThreadListPaginationState::Idle { end_reached: false }
        );

        service.paginate().await.expect("paginate after reset failed");
        assert_eq!(service.items().len(), 1);
    }

    #[async_test]
    async fn test_pagination_state_subscriber() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");
        let sender_id = user_id!("@alice:b.c");
        let f = EventFactory::new().room(room_id).sender(sender_id);
        let eid1 = event_id!("$1");

        server
            .mock_room_threads()
            .ok(
                vec![f.text_msg("Thread root").event_id(eid1).into_raw()],
                Some("next_token".to_owned()),
            )
            .mock_once()
            .mount()
            .await;

        let room = server.sync_joined_room(&client, room_id).await;
        let service = ThreadListService::new(room);

        let subscriber = service.subscribe_to_pagination_state_updates();
        pin_mut!(subscriber);

        assert_pending!(subscriber);

        service.paginate().await.expect("paginate failed");

        assert_next_matches!(subscriber, ThreadListPaginationState::Idle { end_reached: false });
    }

    #[async_test]
    async fn test_paginated_items_have_num_replies_zero_without_summary() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");
        let sender_id = user_id!("@alice:b.c");
        let f = EventFactory::new().room(room_id).sender(sender_id);
        let eid1 = event_id!("$1");

        // A thread root without bundled thread summary.
        server
            .mock_room_threads()
            .ok(vec![f.text_msg("Thread root").event_id(eid1).into_raw()], None)
            .mock_once()
            .mount()
            .await;

        let room = server.sync_joined_room(&client, room_id).await;
        let service = ThreadListService::new(room);

        service.paginate().await.expect("paginate failed");

        let items = service.items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].num_replies, 0);
        assert!(items[0].latest_event.is_none());
    }

    #[async_test]
    async fn test_paginated_items_have_num_replies_from_bundled_summary() {
        let server = MatrixMockServer::new().await;
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");
        let sender_id = user_id!("@alice:b.c");
        let f = EventFactory::new().room(room_id).sender(sender_id);
        let root_id = event_id!("$root");
        let reply_id = event_id!("$reply");

        // Build a reply event to use as the bundled latest event.
        // `with_bundled_thread_summary` expects `Raw<AnySyncMessageLikeEvent>`,
        // so we cast from the more general `Raw<AnySyncTimelineEvent>`.
        let reply_event =
            f.text_msg("Reply in thread").event_id(reply_id).into_raw_sync().cast_unchecked();

        // Build a thread root with a bundled thread summary (3 replies).
        let thread_root = f
            .text_msg("Thread root")
            .event_id(root_id)
            .with_bundled_thread_summary(reply_event, 3, false)
            .into_raw();

        server.mock_room_threads().ok(vec![thread_root], None).mock_once().mount().await;

        let room = server.sync_joined_room(&client, room_id).await;
        let service = ThreadListService::new(room);

        service.paginate().await.expect("paginate failed");

        let items = service.items();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].root_event.event_id, root_id);
        assert_eq!(items[0].num_replies, 3);

        // The latest event should be populated from the bundled summary.
        let latest = items[0].latest_event.as_ref().expect("should have latest_event");
        assert_eq!(latest.event_id, reply_id);
        assert_eq!(latest.sender.as_str(), sender_id.as_str());
    }

    /// Builds a [`ThreadListService`] and makes the room known to the client
    /// by performing a sync.
    async fn make_service(server: &MatrixMockServer) -> ThreadListService {
        let client = server.client_builder().build().await;
        let room_id = room_id!("!a:b.c");
        let room = server.sync_joined_room(&client, room_id).await;
        ThreadListService::new(room)
    }
}
