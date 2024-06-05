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
use tokio::{
    sync::{Mutex, Notify, RwLockReadGuard},
    time::timeout,
};
use tracing::{debug, instrument, trace};

use super::{
    paginator::{PaginationResult, Paginator, PaginatorState},
    store::Gap,
    BackPaginationOutcome, Result, RoomEventCacheInner,
};
use crate::event_cache::{linked_chunk::ChunkContent, store::RoomEvents};

#[derive(Debug)]
pub(super) struct RoomPaginationData {
    /// A notifier that we received a new pagination token.
    pub token_notifier: Notify,

    /// The stateful paginator instance used for the integrated pagination.
    pub paginator: Paginator,

    /// Have we ever waited for a previous-batch-token to come from sync? We do
    /// this at most once per room, the first time we try to run backward
    /// pagination. We reset that upon clearing the timeline events.
    pub waited_for_initial_prev_token: Mutex<bool>,
}

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
        // Make sure there's at most one back-pagination request.
        let prev_token = self.get_or_wait_for_token().await;

        let paginator = &self.inner.pagination.paginator;

        paginator.set_idle_state(prev_token.clone(), None)?;

        // Run the actual pagination.
        let PaginationResult { events, hit_end_of_timeline: reached_start } =
            paginator.paginate_backward(batch_size.into()).await?;

        // Make sure the `RoomEvents` isn't updated while we are saving events from
        // backpagination.
        let mut room_events = self.inner.events.write().await;

        // Check that the previous token still exists; otherwise it's a sign that the
        // room's timeline has been cleared.
        let gap_identifier = if let Some(token) = prev_token {
            let gap_identifier = room_events.chunk_identifier(|chunk| {
                matches!(chunk.content(), ChunkContent::Gap(Gap { ref prev_token }) if *prev_token == token)
            });

            // The method has been called with `token` but it doesn't exist in `RoomEvents`,
            // it's an error.
            if gap_identifier.is_none() {
                return Ok(None);
            }

            gap_identifier
        } else {
            None
        };

        let prev_token = paginator.prev_batch_token().map(|prev_token| Gap { prev_token });

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

        // There is a `token`/gap, let's replace it by new events!
        if let Some(gap_identifier) = gap_identifier {
            let new_position = {
                // Replace the gap by new events.
                let new_chunk = room_events
                    .replace_gap_at(sync_events, gap_identifier)
                    // SAFETY: we are sure that `gap_identifier` represents a valid
                    // `ChunkIdentifier` for a `Gap` chunk.
                    .expect("The `gap_identifier` must represent a `Gap`");

                new_chunk.first_position()
            };

            // And insert a new gap if there is any `prev_token`.
            if let Some(prev_token_gap) = prev_token {
                room_events
                    .insert_gap_at(prev_token_gap, new_position)
                    // SAFETY: we are sure that `new_position` represents a valid
                    // `ChunkIdentifier` for an `Item` chunk.
                    .expect("The `new_position` must represent an `Item`");
            }

            trace!("replaced gap with new events from backpagination");

            // TODO: implement smarter reconciliation later
            //let _ = self.sender.send(RoomEventCacheUpdate::Prepend { events });

            return Ok(Some(BackPaginationOutcome { events, reached_start }));
        }

        // There is no `token`/gap identifier. Let's assume we must prepend the new
        // events.
        let first_event_pos = room_events.events().next().map(|(item_pos, _)| item_pos);

        match first_event_pos {
            // Is there a first item? Insert at this position.
            Some(first_event_pos) => {
                if let Some(prev_token_gap) = prev_token {
                    room_events
                            .insert_gap_at(prev_token_gap, first_event_pos)
                            // SAFETY: The `first_event_pos` can only be an `Item` chunk, it's
                            // an invariant of `LinkedChunk`. Also, it can only represent a valid
                            // `ChunkIdentifier` as the data structure isn't modified yet.
                            .expect("`first_event_pos` must point to a valid `Item` chunk when inserting a gap");
                }

                room_events
                        .insert_events_at(sync_events, first_event_pos)
                        // SAFETY: The `first_event_pos` can only be an `Item` chunk, it's
                        // an invariant of `LinkedChunk`. The chunk it points to has not been
                        // removed.
                        .expect("The `first_event_pos` must point to a valid `Item` chunk when inserting events");
            }

            // There is no first item. Let's simply push.
            None => {
                if let Some(prev_token_gap) = prev_token {
                    room_events.push_gap(prev_token_gap);
                }

                room_events.push_events(sync_events);
            }
        }

        Ok(Some(BackPaginationOutcome { events, reached_start }))
    }

    /// Get the latest pagination token, as stored in the room events linked
    /// list.
    #[doc(hidden)]
    pub async fn get_or_wait_for_token(&self) -> Option<String> {
        const DEFAULT_INITIAL_WAIT_DURATION: Duration = Duration::from_secs(3);

        let waited = *self.inner.pagination.waited_for_initial_prev_token.lock().await;
        if waited {
            self.oldest_token(None).await
        } else {
            let token = self.oldest_token(Some(DEFAULT_INITIAL_WAIT_DURATION)).await;
            *self.inner.pagination.waited_for_initial_prev_token.lock().await = true;
            token
        }
    }

    /// Returns the oldest back-pagination token, that is, the one closest to
    /// the start of the timeline as we know it.
    ///
    /// Optionally, wait at most for the given duration for a back-pagination
    /// token to be returned by a sync.
    async fn oldest_token(&self, max_wait: Option<Duration>) -> Option<String> {
        // Optimistically try to return the backpagination token immediately.
        fn get_oldest(room_events: RwLockReadGuard<'_, RoomEvents>) -> Option<String> {
            room_events.chunks().find_map(|chunk| match chunk.content() {
                ChunkContent::Gap(gap) => Some(gap.prev_token.clone()),
                ChunkContent::Items(..) => None,
            })
        }

        if let Some(token) = get_oldest(self.inner.events.read().await) {
            return Some(token);
        }

        let Some(max_wait) = max_wait else {
            // We had no token and no time to wait, so… no tokens.
            return None;
        };

        // Otherwise wait for a notification that we received a token.
        // Timeouts are fine, per this function's contract.
        let _ = timeout(max_wait, self.inner.pagination.token_notifier.notified()).await;

        get_oldest(self.inner.events.read().await)
    }

    /// Returns a subscriber to the pagination status used for the
    /// back-pagination integrated to the event cache.
    pub fn status(&self) -> Subscriber<PaginatorState> {
        self.inner.pagination.paginator.state()
    }

    /// Returns whether we've hit the start of the timeline.
    ///
    /// This is true if, and only if, we didn't have a previous-batch token and
    /// running backwards pagination would be useless.
    pub fn hit_timeline_start(&self) -> bool {
        self.inner.pagination.paginator.hit_timeline_start()
    }

    /// Returns whether we've hit the end of the timeline.
    ///
    /// This is true if, and only if, we didn't have a next-batch token and
    /// running forwards pagination would be useless.
    pub fn hit_timeline_end(&self) -> bool {
        self.inner.pagination.paginator.hit_timeline_end()
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
        use matrix_sdk_test::{async_test, sync_timeline_event};
        use ruma::room_id;
        use tokio::{spawn, time::sleep};

        use crate::{event_cache::store::Gap, test_utils::logged_in_client};

        #[async_test]
        async fn test_wait_no_pagination_token() {
            let client = logged_in_client(None).await;
            let room_id = room_id!("!galette:saucisse.bzh");
            client.base_client().get_or_create_room(room_id, RoomState::Joined);

            let event_cache = client.event_cache();

            event_cache.subscribe().unwrap();

            let (room_event_cache, _drop_handlers) = event_cache.for_room(room_id).await.unwrap();
            let room_event_cache = room_event_cache.unwrap();

            // When I only have events in a room,
            {
                let mut room_events = room_event_cache.inner.events.write().await;
                room_events.push_events([sync_timeline_event!({
                    "sender": "b@z.h",
                    "type": "m.room.message",
                    "event_id": "$ida",
                    "origin_server_ts": 12344446,
                    "content": { "body":"yolo", "msgtype": "m.text" },
                })
                .into()]);
            }

            let pagination = room_event_cache.pagination();

            // If I don't wait for the backpagination token,
            let found = pagination.oldest_token(None).await;
            // Then I don't find it.
            assert!(found.is_none());

            // If I wait for a back-pagination token for 0 seconds,
            let before = Instant::now();
            let found = pagination.oldest_token(Some(Duration::default())).await;
            let waited = before.elapsed();
            // then I don't get any,
            assert!(found.is_none());
            // and I haven't waited long.
            assert!(waited.as_secs() < 1);

            // If I wait for a back-pagination token for 1 second,
            let before = Instant::now();
            let found = pagination.oldest_token(Some(Duration::from_secs(1))).await;
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
            let room_event_cache = room_event_cache.unwrap();

            let expected_token = "old".to_owned();

            // When I have events and multiple gaps, in a room,
            {
                let mut room_events = room_event_cache.inner.events.write().await;
                room_events.push_gap(Gap { prev_token: expected_token.clone() });
                room_events.push_events([sync_timeline_event!({
                    "sender": "b@z.h",
                    "type": "m.room.message",
                    "event_id": "$ida",
                    "origin_server_ts": 12344446,
                    "content": { "body":"yolo", "msgtype": "m.text" },
                })
                .into()]);
            }

            let paginator = room_event_cache.pagination();

            // If I don't wait for a back-pagination token,
            let found = paginator.oldest_token(None).await;
            // Then I get it.
            assert_eq!(found.as_ref(), Some(&expected_token));

            // If I wait for a back-pagination token for 0 seconds,
            let before = Instant::now();
            let found = paginator.oldest_token(Some(Duration::default())).await;
            let waited = before.elapsed();
            // then I do get one.
            assert_eq!(found.as_ref(), Some(&expected_token));
            // and I haven't waited long.
            assert!(waited.as_millis() < 100);

            // If I wait for a back-pagination token for 1 second,
            let before = Instant::now();
            let found = paginator.oldest_token(Some(Duration::from_secs(1))).await;
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
            let room_event_cache = room_event_cache.unwrap();

            let expected_token = "old".to_owned();

            let before = Instant::now();
            let cloned_expected_token = expected_token.clone();
            let cloned_room_event_cache = room_event_cache.clone();
            let insert_token_task = spawn(async move {
                // If a backpagination token is inserted after 400 milliseconds,
                sleep(Duration::from_millis(400)).await;

                {
                    let mut room_events = cloned_room_event_cache.inner.events.write().await;
                    room_events.push_gap(Gap { prev_token: cloned_expected_token });
                }
            });

            let pagination = room_event_cache.pagination();

            // Then first I don't get it (if I'm not waiting,)
            let found = pagination.oldest_token(None).await;
            assert!(found.is_none());

            // And if I wait for the back-pagination token for 600ms,
            let found = pagination.oldest_token(Some(Duration::from_millis(600))).await;
            let waited = before.elapsed();

            // then I do get one eventually.
            assert_eq!(found, Some(expected_token));
            // and I have waited between ~400 and ~1000 milliseconds.
            assert!(waited.as_secs() < 1);
            assert!(waited.as_millis() >= 400);

            // The task succeeded.
            insert_token_task.await.unwrap();
        }
    }
}
