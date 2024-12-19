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

//! A sub-object for running pagination tasks on a given room.

use std::{future::Future, ops::ControlFlow, sync::Arc, time::Duration};

use eyeball::Subscriber;
use matrix_sdk_base::deserialized_responses::SyncTimelineEvent;
use matrix_sdk_common::linked_chunk::ChunkContent;
use tokio::time::timeout;
use tracing::{debug, instrument, trace};

use super::{
    paginator::{PaginationResult, PaginatorState},
    room::{
        events::{Gap, RoomEvents},
        RoomEventCacheInner,
    },
    BackPaginationOutcome, EventsOrigin, Result, RoomEventCacheUpdate,
};

/// An API object to run pagination queries on a [`super::RoomEventCache`].
///
/// Can be created with [`super::RoomEventCache::pagination()`].
#[allow(missing_debug_implementations)]
pub struct RoomPagination {
    pub(super) inner: Arc<RoomEventCacheInner>,
}

impl RoomPagination {
    /// Starts a back-pagination for the requested number of events.
    ///
    /// This automatically takes care of waiting for a pagination token from
    /// sync, if we haven't done that before.
    ///
    /// The `until` argument is an async closure that returns a [`ControlFlow`]
    /// to decide whether a new pagination must be run or not. It's helpful when
    /// the server replies with e.g. a certain set of events, but we would like
    /// more, or the event we are looking for isn't part of this set: in this
    /// case, `until` returns [`ControlFlow::Continue`], otherwise it returns
    /// [`ControlFlow::Break`]. `until` receives [`BackPaginationOutcome`] as
    /// its sole argument.
    ///
    /// # Errors
    ///
    /// It may return an error if the pagination token used during
    /// back-pagination has disappeared while we started the pagination. In
    /// that case, it's desirable to call the method again.
    ///
    /// # Example
    ///
    /// To do a single run:
    ///
    /// ```rust
    /// use std::ops::ControlFlow;
    ///
    /// use matrix_sdk::event_cache::{
    ///     BackPaginationOutcome,
    ///     RoomPagination,
    ///     TimelineHasBeenResetWhilePaginating
    /// };
    ///
    /// # async fn foo(room_pagination: RoomPagination) {
    /// let result = room_pagination.run_backwards(
    ///     42,
    ///     |BackPaginationOutcome { events, reached_start },
    ///      _timeline_has_been_reset: TimelineHasBeenResetWhilePaginating| async move {
    ///         // Do something with `events` and `reached_start` maybe?
    ///         let _ = events;
    ///         let _ = reached_start;
    ///
    ///         ControlFlow::Break(())
    ///     }
    /// ).await;
    /// # }
    #[instrument(skip(self, until))]
    pub async fn run_backwards<Until, Break, UntilFuture>(
        &self,
        batch_size: u16,
        mut until: Until,
    ) -> Result<Break>
    where
        Until: FnMut(BackPaginationOutcome, TimelineHasBeenResetWhilePaginating) -> UntilFuture,
        UntilFuture: Future<Output = ControlFlow<Break, ()>>,
    {
        let mut timeline_has_been_reset = TimelineHasBeenResetWhilePaginating::No;

        loop {
            if let Some(outcome) = self.run_backwards_impl(batch_size).await? {
                match until(outcome, timeline_has_been_reset).await {
                    ControlFlow::Continue(()) => {
                        trace!("back-pagination continues");

                        timeline_has_been_reset = TimelineHasBeenResetWhilePaginating::No;

                        continue;
                    }

                    ControlFlow::Break(value) => return Ok(value),
                }
            }

            timeline_has_been_reset = TimelineHasBeenResetWhilePaginating::Yes;

            debug!("back-pagination has been internally restarted because of a timeline reset.");
        }
    }

    async fn run_backwards_impl(&self, batch_size: u16) -> Result<Option<BackPaginationOutcome>> {
        const DEFAULT_WAIT_FOR_TOKEN_DURATION: Duration = Duration::from_secs(3);

        let prev_token = self.get_or_wait_for_token(Some(DEFAULT_WAIT_FOR_TOKEN_DURATION)).await;

        let paginator = &self.inner.paginator;

        paginator.set_idle_state(PaginatorState::Idle, prev_token.clone(), None)?;

        // Run the actual pagination.
        let PaginationResult { events, hit_end_of_timeline: reached_start } =
            paginator.paginate_backward(batch_size.into()).await?;

        // Make sure the `RoomEvents` isn't updated while we are saving events from
        // backpagination.
        let mut state = self.inner.state.write().await;

        // Check that the previous token still exists; otherwise it's a sign that the
        // room's timeline has been cleared.
        let prev_gap_id = if let Some(token) = prev_token {
            let gap_id = state.events().chunk_identifier(|chunk| {
                matches!(chunk.content(), ChunkContent::Gap(Gap { ref prev_token }) if *prev_token == token)
            });

            // We got a previous-batch token from the linked chunk *before* running the
            // request, which is missing from the linked chunk *after*
            // completing the request. It may be a sign the linked chunk has
            // been reset, and it's an error in any case.
            if gap_id.is_none() {
                return Ok(None);
            }

            gap_id
        } else {
            None
        };

        let new_gap = paginator.prev_batch_token().map(|prev_token| Gap { prev_token });

        let result = state
            .with_events_mut(move |room_events| {
                // Note: The chunk could be empty.
                //
                // If there's any event, they are presented in reverse order (i.e. the first one
                // should be prepended first).

                let sync_events = events
                    .iter()
                    // Reverse the order of the events as `/messages` has been called with `dir=b`
                    // (backward). The `RoomEvents` API expects the first event to be the oldest.
                    .rev()
                    .cloned()
                    .map(SyncTimelineEvent::from);

                let first_event_pos = room_events.events().next().map(|(item_pos, _)| item_pos);

                // First, insert events.
                let insert_new_gap_pos = if let Some(gap_id) = prev_gap_id {
                    // There is a prior gap, let's replace it by new events!
                    trace!("replaced gap with new events from backpagination");
                    room_events
                        .replace_gap_at(sync_events, gap_id)
                        .expect("gap_identifier is a valid chunk id we read previously")
                } else if let Some(pos) = first_event_pos {
                    // No prior gap, but we had some events: assume we need to prepend events
                    // before those.
                    trace!("inserted events before the first known event");
                    room_events
                        .insert_events_at(sync_events, pos)
                        .expect("pos is a valid position we just read above");
                    Some(pos)
                } else {
                    // No prior gap, and no prior events: push the events.
                    trace!("pushing events received from back-pagination");
                    room_events.push_events(sync_events);
                    // A new gap may be inserted before the new events, if there are any.
                    room_events.events().next().map(|(item_pos, _)| item_pos)
                };

                // And insert the new gap if needs be.
                if let Some(new_gap) = new_gap {
                    if let Some(new_pos) = insert_new_gap_pos {
                        room_events
                            .insert_gap_at(new_gap, new_pos)
                            .expect("events_chunk_pos represents a valid chunk position");
                    } else {
                        room_events.push_gap(new_gap);
                    }
                }

                let sync_timeline_events_diffs = room_events.updates_as_vector_diffs();

                if !sync_timeline_events_diffs.is_empty() {
                    let _ = self.inner.sender.send(RoomEventCacheUpdate::UpdateTimelineEvents {
                        diffs: sync_timeline_events_diffs,
                        origin: EventsOrigin::Sync,
                    });
                }

                BackPaginationOutcome { events, reached_start }
            })
            .await?;

        Ok(Some(result))
    }

    /// Get the latest pagination token, as stored in the room events linked
    /// list, or wait for it for the given amount of time.
    ///
    /// It will only wait if we *never* saw an initial previous-batch token.
    /// Otherwise, it will immediately skip.
    #[doc(hidden)]
    pub async fn get_or_wait_for_token(&self, wait_time: Option<Duration>) -> Option<String> {
        fn get_latest(events: &RoomEvents) -> Option<String> {
            events.rchunks().find_map(|chunk| match chunk.content() {
                ChunkContent::Gap(gap) => Some(gap.prev_token.clone()),
                ChunkContent::Items(..) => None,
            })
        }

        {
            // Scope for the lock guard.
            let state = self.inner.state.read().await;
            // Fast-path: we do have a previous-batch token already.
            if let Some(found) = get_latest(state.events()) {
                return Some(found);
            }
            // If we've already waited for an initial previous-batch token before,
            // immediately abort.
            if state.waited_for_initial_prev_token {
                return None;
            }
        }

        // If the caller didn't set a wait time, return none early.
        let wait_time = wait_time?;

        // Otherwise, wait for a notification that we received a previous-batch token.
        // Note the state lock is released while doing so, allowing other tasks to write
        // into the linked chunk.
        let _ = timeout(wait_time, self.inner.pagination_batch_token_notifier.notified()).await;

        let mut state = self.inner.state.write().await;
        let token = get_latest(state.events());
        state.waited_for_initial_prev_token = true;
        token
    }

    /// Returns a subscriber to the pagination status used for the
    /// back-pagination integrated to the event cache.
    pub fn status(&self) -> Subscriber<PaginatorState> {
        self.inner.paginator.state()
    }

    /// Returns whether we've hit the start of the timeline.
    ///
    /// This is true if, and only if, we didn't have a previous-batch token and
    /// running backwards pagination would be useless.
    pub fn hit_timeline_start(&self) -> bool {
        self.inner.paginator.hit_timeline_start()
    }

    /// Returns whether we've hit the end of the timeline.
    ///
    /// This is true if, and only if, we didn't have a next-batch token and
    /// running forwards pagination would be useless.
    pub fn hit_timeline_end(&self) -> bool {
        self.inner.paginator.hit_timeline_end()
    }
}

/// A type representing whether the timeline has been reset.
#[derive(Debug)]
pub enum TimelineHasBeenResetWhilePaginating {
    /// The timeline has been reset.
    Yes,

    /// The timeline has not been reset.
    No,
}

#[cfg(test)]
mod tests {
    // Those tests require time to work, and it does not on wasm32.
    #[cfg(not(target_arch = "wasm32"))]
    mod time_tests {
        use std::time::{Duration, Instant};

        use matrix_sdk_base::RoomState;
        use matrix_sdk_test::{
            async_test, event_factory::EventFactory, sync_timeline_event, ALICE,
        };
        use ruma::room_id;
        use tokio::{spawn, time::sleep};

        use crate::{
            deserialized_responses::SyncTimelineEvent, event_cache::room::events::Gap,
            test_utils::logged_in_client,
        };

        #[async_test]
        async fn test_wait_no_pagination_token() {
            let client = logged_in_client(None).await;
            let room_id = room_id!("!galette:saucisse.bzh");
            client.base_client().get_or_create_room(room_id, RoomState::Joined);

            let event_cache = client.event_cache();

            event_cache.subscribe().unwrap();

            let (room_event_cache, _drop_handlers) = event_cache.for_room(room_id).await.unwrap();

            // When I only have events in a room,
            room_event_cache
                .inner
                .state
                .write()
                .await
                .with_events_mut(|events| {
                    events.push_events([SyncTimelineEvent::new(sync_timeline_event!({
                        "sender": "b@z.h",
                        "type": "m.room.message",
                        "event_id": "$ida",
                        "origin_server_ts": 12344446,
                        "content": { "body":"yolo", "msgtype": "m.text" },
                    }))])
                })
                .await
                .unwrap();

            let pagination = room_event_cache.pagination();

            // If I don't wait for the backpagination token,
            let found = pagination.get_or_wait_for_token(None).await;
            // Then I don't find it.
            assert!(found.is_none());

            // Reset waited_for_initial_prev_token state.
            pagination.inner.state.write().await.reset().await.unwrap();

            // If I wait for a back-pagination token for 0 seconds,
            let before = Instant::now();
            let found = pagination.get_or_wait_for_token(Some(Duration::default())).await;
            let waited = before.elapsed();
            // then I don't get any,
            assert!(found.is_none());
            // and I haven't waited long.
            assert!(waited.as_secs() < 1);

            // Reset waited_for_initial_prev_token state.
            pagination.inner.state.write().await.reset().await.unwrap();

            // If I wait for a back-pagination token for 1 second,
            let before = Instant::now();
            let found = pagination.get_or_wait_for_token(Some(Duration::from_secs(1))).await;
            let waited = before.elapsed();
            // then I still don't get any.
            assert!(found.is_none());
            // and I've waited a bit.
            assert!(waited.as_secs() < 2);
            assert!(waited.as_secs() >= 1);
        }

        #[async_test]
        async fn test_wait_for_pagination_token_already_present() {
            let client = logged_in_client(None).await;
            let room_id = room_id!("!galette:saucisse.bzh");
            client.base_client().get_or_create_room(room_id, RoomState::Joined);

            let event_cache = client.event_cache();

            event_cache.subscribe().unwrap();

            let (room_event_cache, _drop_handlers) = event_cache.for_room(room_id).await.unwrap();

            let expected_token = "old".to_owned();

            // When I have events and multiple gaps, in a room,
            {
                room_event_cache
                    .inner
                    .state
                    .write()
                    .await
                    .with_events_mut(|room_events| {
                        room_events.push_gap(Gap { prev_token: expected_token.clone() });
                        room_events.push_events([SyncTimelineEvent::new(sync_timeline_event!({
                            "sender": "b@z.h",
                            "type": "m.room.message",
                            "event_id": "$ida",
                            "origin_server_ts": 12344446,
                            "content": { "body":"yolo", "msgtype": "m.text" },
                        }))]);
                    })
                    .await
                    .unwrap();
            }

            let pagination = room_event_cache.pagination();

            // If I don't wait for a back-pagination token,
            let found = pagination.get_or_wait_for_token(None).await;
            // Then I get it.
            assert_eq!(found.as_ref(), Some(&expected_token));

            // If I wait for a back-pagination token for 0 seconds,
            let before = Instant::now();
            let found = pagination.get_or_wait_for_token(Some(Duration::default())).await;
            let waited = before.elapsed();
            // then I do get one.
            assert_eq!(found.as_ref(), Some(&expected_token));
            // and I haven't waited long.
            assert!(waited.as_millis() < 100);

            // If I wait for a back-pagination token for 1 second,
            let before = Instant::now();
            let found = pagination.get_or_wait_for_token(Some(Duration::from_secs(1))).await;
            let waited = before.elapsed();
            // then I do get one.
            assert_eq!(found, Some(expected_token));
            // and I haven't waited long.
            assert!(waited.as_millis() < 100);
        }

        #[async_test]
        async fn test_wait_for_late_pagination_token() {
            let client = logged_in_client(None).await;
            let room_id = room_id!("!galette:saucisse.bzh");
            client.base_client().get_or_create_room(room_id, RoomState::Joined);

            let event_cache = client.event_cache();

            event_cache.subscribe().unwrap();

            let (room_event_cache, _drop_handles) = event_cache.for_room(room_id).await.unwrap();

            let expected_token = "old".to_owned();

            let before = Instant::now();
            let cloned_expected_token = expected_token.clone();
            let cloned_room_event_cache = room_event_cache.clone();
            let insert_token_task = spawn(async move {
                // If a backpagination token is inserted after 400 milliseconds,
                sleep(Duration::from_millis(400)).await;

                cloned_room_event_cache
                    .inner
                    .state
                    .write()
                    .await
                    .with_events_mut(|events| {
                        events.push_gap(Gap { prev_token: cloned_expected_token })
                    })
                    .await
                    .unwrap();
            });

            let pagination = room_event_cache.pagination();

            // Then first I don't get it (if I'm not waiting,)
            let found = pagination.get_or_wait_for_token(None).await;
            assert!(found.is_none());

            // And if I wait for the back-pagination token for 600ms,
            let found = pagination.get_or_wait_for_token(Some(Duration::from_millis(600))).await;
            let waited = before.elapsed();

            // then I do get one eventually.
            assert_eq!(found, Some(expected_token));
            // and I have waited between ~400 and ~1000 milliseconds.
            assert!(waited.as_secs() < 1);
            assert!(waited.as_millis() >= 400);

            // The task succeeded.
            insert_token_task.await.unwrap();
        }

        #[async_test]
        async fn test_get_latest_token() {
            let client = logged_in_client(None).await;
            let room_id = room_id!("!galette:saucisse.bzh");
            client.base_client().get_or_create_room(room_id, RoomState::Joined);

            let event_cache = client.event_cache();

            event_cache.subscribe().unwrap();

            let (room_event_cache, _drop_handles) = event_cache.for_room(room_id).await.unwrap();

            let old_token = "old".to_owned();
            let new_token = "new".to_owned();

            // Assuming a room event cache that contains both an old and a new pagination
            // token, and events in between,
            room_event_cache
                .inner
                .state
                .write()
                .await
                .with_events_mut(|events| {
                    let f = EventFactory::new().room(room_id).sender(*ALICE);

                    // This simulates a valid representation of a room: first group of gap+events
                    // were e.g. restored from the cache; second group of gap+events was received
                    // from a subsequent sync.
                    events.push_gap(Gap { prev_token: old_token });
                    events.push_events([f.text_msg("oldest from cache").into()]);

                    events.push_gap(Gap { prev_token: new_token.clone() });
                    events.push_events([f.text_msg("sync'd gappy timeline").into()]);
                })
                .await
                .unwrap();

            let pagination = room_event_cache.pagination();

            // Retrieving the pagination token will return the most recent one, not the old
            // one.
            let found = pagination.get_or_wait_for_token(None).await;
            assert_eq!(found, Some(new_token));
        }
    }
}
