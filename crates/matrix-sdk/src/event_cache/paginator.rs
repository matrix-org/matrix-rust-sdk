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

//! The paginator is a stateful helper object that handles reaching an event,
//! either from a cache or network, and surrounding events ("context"). Then, it
//! makes it possible to paginate forward or backward, from that event, until
//! one end of the timeline (front or back) is reached.

use eyeball::{SharedObservable, Subscriber};
use matrix_sdk_base::{deserialized_responses::TimelineEvent, SendOutsideWasm, SyncOutsideWasm};
use ruma::{api::Direction, uint, EventId, OwnedEventId};
use tokio::sync::Mutex;

use crate::{
    room::{EventWithContextResponse, Messages, MessagesOptions},
    Room,
};

/// Current state of a [`Paginator`].
#[derive(Debug, PartialEq, Copy, Clone)]
pub enum PaginatorState {
    /// The initial state of the paginator.
    Initial,

    /// The paginator is fetching the target initial event.
    FetchingTargetEvent,

    /// The target initial event could be found, zero or more paginations have
    /// happened since then, and the paginator is at rest now.
    Idle,

    /// The paginator is… paginating one direction or another.
    Paginating,
}

/// An error that happened when using a [`Paginator`].
#[derive(Debug, thiserror::Error)]
pub enum PaginatorError {
    /// The target event could not be found.
    #[error("target event with id {0} could not be found")]
    EventNotFound(OwnedEventId),

    /// We're trying to manipulate the paginator in the wrong state.
    #[error("expected paginator state {expected:?}, observed {actual:?}")]
    InvalidPreviousState {
        /// The state we were expecting to see.
        expected: PaginatorState,
        /// The actual state when doing the check.
        actual: PaginatorState,
    },

    /// There was another SDK error while paginating.
    #[error("an error happened while paginating")]
    SdkError(#[source] crate::Error),
}

/// A stateful object to reach to an event, and then paginate backward and
/// forward from it.
///
/// See also the module-level documentation.
pub struct Paginator {
    /// The room in which we're going to run the pagination.
    room: Box<dyn PaginableRoom>,

    /// Current state of the paginator.
    state: SharedObservable<PaginatorState>,

    /// The token to run the next backward pagination.
    prev_batch_token: Mutex<Option<String>>,

    /// The token to run the next forward pagination.
    next_batch_token: Mutex<Option<String>>,
}

#[cfg(not(tarpaulin_include))]
impl std::fmt::Debug for Paginator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Don't include the room in the debug output.
        f.debug_struct("Paginator")
            .field("state", &self.state.get())
            .field("prev_batch_token", &self.prev_batch_token)
            .field("next_batch_token", &self.next_batch_token)
            .finish_non_exhaustive()
    }
}

/// The result of a single pagination, be it from
/// [`Paginator::paginate_backward`] or [`Paginator::paginate_forward`].
#[derive(Debug)]
pub struct PaginationResult {
    /// Events returned during this pagination.
    ///
    /// If this is the result of a backward pagination, then the events are in
    /// reverse topological order.
    ///
    /// If this is the result of a forward pagination, then the events are in
    /// topological order.
    pub events: Vec<TimelineEvent>,

    /// Did we hit an end of the timeline?
    ///
    /// If this is the result of a backward pagination, this means we hit the
    /// *start* of the timeline.
    ///
    /// If this is the result of a forward pagination, this means we hit the
    /// *end* of the timeline.
    pub hit_end_of_timeline: bool,
}

/// The result of an initial [`Paginator::start_from`] query.
#[derive(Debug)]
pub struct StartFromResult {
    /// All the events returned during this pagination, in topological ordering.
    pub events: Vec<TimelineEvent>,

    /// Whether the /context query returned a previous batch token.
    pub has_prev: bool,

    /// Whether the /context query returned a next batch token.
    pub has_next: bool,
}

impl Paginator {
    /// Create a new [`Paginator`], given a room implementation.
    pub fn new(room: Box<dyn PaginableRoom>) -> Self {
        Self {
            room,
            state: SharedObservable::new(PaginatorState::Initial),
            prev_batch_token: Mutex::new(None),
            next_batch_token: Mutex::new(None),
        }
    }

    /// Check if the current state of the paginator matches the expected one.
    fn check_state(&self, expected: PaginatorState) -> Result<(), PaginatorError> {
        let actual = self.state.get();
        if actual != expected {
            Err(PaginatorError::InvalidPreviousState { expected, actual })
        } else {
            Ok(())
        }
    }

    /// Returns a subscriber to the internal [`PaginatorState`] machine.
    pub fn state(&self) -> Subscriber<PaginatorState> {
        self.state.subscribe()
    }

    /// Starts the pagination from the initial event.
    ///
    /// Only works for fresh [`Paginator`] objects, which are in the
    /// [`PaginatorState::Initial`] state.
    pub async fn start_from(&self, event_id: &EventId) -> Result<StartFromResult, PaginatorError> {
        self.check_state(PaginatorState::Initial)?;

        // Note: it's possible two callers have checked the state and both figured it's
        // initial. This check below makes sure there's at most one which can set the
        // state to FetchingTargetEvent, preventing a race condition.
        if self.state.set_if_not_eq(PaginatorState::FetchingTargetEvent).is_none() {
            return Err(PaginatorError::InvalidPreviousState {
                expected: PaginatorState::Initial,
                actual: PaginatorState::FetchingTargetEvent,
            });
        }

        // TODO: do we want to lazy load members?
        let lazy_load_members = true;

        let response = self.room.event_with_context(event_id, lazy_load_members).await?;

        let has_prev = response.prev_batch_token.is_some();
        let has_next = response.next_batch_token.is_some();
        *self.prev_batch_token.lock().await = response.prev_batch_token;
        *self.next_batch_token.lock().await = response.next_batch_token;

        self.state.set(PaginatorState::Idle);

        // Consolidate the events into a linear timeline, topologically ordered.
        // - the events before are returned in the reverse topological order: invert
        //   them.
        // - insert the target event, if set.
        // - the events after are returned in the correct topological order.

        let events = response
            .events_before
            .into_iter()
            .rev()
            .chain(response.event)
            .chain(response.events_after)
            .collect();

        Ok(StartFromResult { events, has_prev, has_next })
    }

    /// Runs a backward pagination, from the current state of the object.
    ///
    /// Will return immediately if we have already hit the start of the
    /// timeline.
    ///
    /// May return an error if it's already paginating, or if the call to
    /// /messages failed.
    pub async fn paginate_backward(&self) -> Result<PaginationResult, PaginatorError> {
        self.paginate(Direction::Backward, &self.prev_batch_token).await
    }

    /// Runs a forward pagination, from the current state of the object.
    ///
    /// Will return immediately if we have already hit the end of the timeline.
    ///
    /// May return an error if it's already paginating, or if the call to
    /// /messages failed.
    pub async fn paginate_forward(&self) -> Result<PaginationResult, PaginatorError> {
        self.paginate(Direction::Forward, &self.next_batch_token).await
    }

    async fn paginate(
        &self,
        dir: Direction,
        token_lock: &Mutex<Option<String>>,
    ) -> Result<PaginationResult, PaginatorError> {
        self.check_state(PaginatorState::Idle)?;

        let token = {
            let token = token_lock.lock().await;
            if token.is_none() {
                return Ok(PaginationResult { events: Vec::new(), hit_end_of_timeline: true });
            };
            token.clone()
        };

        // Note: it's possible two callers have checked the state and both figured it's
        // idle. This check below makes sure there's at most one which can set the
        // state to paginating, preventing a race condition.
        if self.state.set_if_not_eq(PaginatorState::Paginating).is_none() {
            return Err(PaginatorError::InvalidPreviousState {
                expected: PaginatorState::Idle,
                actual: PaginatorState::Paginating,
            });
        }

        let response_result =
            self.room.messages(MessagesOptions::new(dir).from(token.as_deref())).await;

        // In case of error, reset the state to idle.
        let response = match response_result {
            Ok(res) => res,
            Err(err) => {
                self.state.set(PaginatorState::Idle);
                return Err(err);
            }
        };

        let hit_end_of_timeline = response.end.is_none();
        *token_lock.lock().await = response.end;

        // TODO: what to do with state events?

        self.state.set(PaginatorState::Idle);

        Ok(PaginationResult { events: response.chunk, hit_end_of_timeline })
    }
}

/// A room that can be paginated.
///
/// Not [`crate::Room`] because we may want to paginate rooms we don't belong
/// to.
#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
pub trait PaginableRoom: SendOutsideWasm + SyncOutsideWasm {
    /// Runs a /context query for the given room.
    ///
    /// Must return [`PaginatorError::EventNotFound`] whenever the target event
    /// could not be found, instead of causing an http `Err` result.
    async fn event_with_context(
        &self,
        event_id: &EventId,
        lazy_load_members: bool,
    ) -> Result<EventWithContextResponse, PaginatorError>;

    /// Runs a /messages query for the given room.
    async fn messages(&self, opts: MessagesOptions) -> Result<Messages, PaginatorError>;
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl PaginableRoom for Room {
    async fn event_with_context(
        &self,
        event_id: &EventId,
        lazy_load_members: bool,
    ) -> Result<EventWithContextResponse, PaginatorError> {
        let response = match self.event_with_context(event_id, lazy_load_members, uint!(20)).await {
            Ok(result) => result,

            Err(err) => {
                // If the error was a 404, then the event wasn't found on the server; special
                // case this to make it easy to react to such an error.
                if let Some(error) = err.as_client_api_error() {
                    if error.status_code == 404 {
                        // Event not found
                        return Err(PaginatorError::EventNotFound(event_id.to_owned()));
                    }
                }

                // Otherwise, just return a wrapped error.
                return Err(PaginatorError::SdkError(err));
            }
        };

        Ok(response)
    }

    async fn messages(&self, opts: MessagesOptions) -> Result<Messages, PaginatorError> {
        self.messages(opts).await.map_err(PaginatorError::SdkError)
    }
}

#[cfg(all(not(target_arch = "wasm32"), test))]
mod tests {
    use std::sync::Arc;

    use assert_matches2::assert_let;
    use async_trait::async_trait;
    use futures_core::Future;
    use futures_util::FutureExt as _;
    use matrix_sdk_test::async_test;
    use once_cell::sync::Lazy;
    use ruma::{event_id, room_id, user_id, RoomId, UserId};
    use tokio::{spawn, sync::Notify};

    use super::*;
    use crate::test_utils::{assert_event_matches_msg, events::EventFactory};

    #[derive(Clone)]
    struct TestRoom {
        event_factory: Arc<EventFactory>,
        wait_for_ready: bool,

        target_event_text: Arc<Mutex<String>>,
        next_events: Arc<Mutex<Vec<TimelineEvent>>>,
        prev_events: Arc<Mutex<Vec<TimelineEvent>>>,
        prev_batch_token: Arc<Mutex<Option<String>>>,
        next_batch_token: Arc<Mutex<Option<String>>>,

        room_ready: Arc<Notify>,
    }

    impl TestRoom {
        fn new(wait_for_ready: bool, room_id: &RoomId, sender: &UserId) -> Self {
            let event_factory = Arc::new(EventFactory::default().sender(sender).room(room_id));

            Self {
                event_factory,
                wait_for_ready,

                room_ready: Default::default(),
                target_event_text: Default::default(),
                next_events: Default::default(),
                prev_events: Default::default(),
                prev_batch_token: Default::default(),
                next_batch_token: Default::default(),
            }
        }

        /// Unblocks the next request.
        fn mark_ready(&self) {
            self.room_ready.notify_one();
        }
    }

    static ROOM_ID: Lazy<&RoomId> = Lazy::new(|| room_id!("!dune:herbert.org"));
    static USER_ID: Lazy<&UserId> = Lazy::new(|| user_id!("@paul:atreid.es"));

    #[async_trait]
    impl PaginableRoom for TestRoom {
        async fn event_with_context(
            &self,
            event_id: &EventId,
            _lazy_load_members: bool,
        ) -> Result<EventWithContextResponse, PaginatorError> {
            // Wait for the room to be marked as ready first.
            if self.wait_for_ready {
                self.room_ready.notified().await;
            }

            let event = self
                .event_factory
                .text_msg(self.target_event_text.lock().await.clone())
                .event_id(event_id)
                .into_timeline();

            return Ok(EventWithContextResponse {
                event: Some(event),
                events_before: self.prev_events.lock().await.clone(),
                events_after: self.next_events.lock().await.clone(),
                prev_batch_token: self.prev_batch_token.lock().await.clone(),
                next_batch_token: self.next_batch_token.lock().await.clone(),
                state: Vec::new(),
            });
        }

        async fn messages(&self, opts: MessagesOptions) -> Result<Messages, PaginatorError> {
            if self.wait_for_ready {
                self.room_ready.notified().await;
            }

            let (end, events) = match opts.dir {
                Direction::Backward => (
                    self.prev_batch_token.lock().await.clone(),
                    self.prev_events.lock().await.clone(),
                ),
                Direction::Forward => (
                    self.next_batch_token.lock().await.clone(),
                    self.next_events.lock().await.clone(),
                ),
            };

            return Ok(Messages {
                start: opts.from.unwrap().to_owned(),
                end,
                chunk: events,
                state: Vec::new(),
            });
        }
    }

    async fn assert_invalid_state<T: std::fmt::Debug>(
        task: impl Future<Output = Result<T, PaginatorError>>,
        expected: PaginatorState,
        actual: PaginatorState,
    ) {
        assert_let!(
            Err(PaginatorError::InvalidPreviousState {
                expected: real_expected,
                actual: real_actual
            }) = task.await
        );
        assert_eq!(real_expected, expected);
        assert_eq!(real_actual, actual);
    }

    #[async_test]
    async fn test_start_from() {
        // Prepare test data.
        let room = Box::new(TestRoom::new(false, *ROOM_ID, *USER_ID));

        let event_id = event_id!("$yoyoyo");
        let event_factory = &room.event_factory;

        *room.target_event_text.lock().await = "fetch_from".to_owned();
        *room.prev_events.lock().await = (0..10)
            .rev()
            .map(|i| {
                TimelineEvent::new(
                    event_factory.text_msg(format!("before-{i}")).into_raw_timeline(),
                )
            })
            .collect();
        *room.next_events.lock().await = (0..10)
            .map(|i| {
                TimelineEvent::new(event_factory.text_msg(format!("after-{i}")).into_raw_timeline())
            })
            .collect();

        // When I call `Paginator::start_from`, it works,
        let paginator = Arc::new(Paginator::new(room.clone()));
        let context = paginator.start_from(event_id).await.expect("start_from should work");

        assert!(!context.has_prev);
        assert!(!context.has_next);

        // And I get the events I expected.

        // 10 events before, the target event, 10 events after.
        assert_eq!(context.events.len(), 21);

        for i in 0..10 {
            assert_event_matches_msg(&context.events[i], &format!("before-{i}"));
        }

        assert_event_matches_msg(&context.events[10], "fetch_from");
        assert_eq!(context.events[10].event.deserialize().unwrap().event_id(), event_id);

        for i in 0..10 {
            assert_event_matches_msg(&context.events[i + 11], &format!("after-{i}"));
        }
    }

    #[async_test]
    async fn test_paginate_backward() {
        // Prepare test data.
        let room = Box::new(TestRoom::new(false, *ROOM_ID, *USER_ID));

        let event_id = event_id!("$yoyoyo");
        let event_factory = &room.event_factory;

        *room.target_event_text.lock().await = "initial".to_owned();
        *room.prev_batch_token.lock().await = Some("prev".to_owned());

        // When I call `Paginator::start_from`, it works,
        let paginator = Arc::new(Paginator::new(room.clone()));
        let context = paginator.start_from(event_id).await.expect("start_from should work");

        // And I get the events I expected.
        assert_eq!(context.events.len(), 1);
        assert_event_matches_msg(&context.events[0], "initial");
        assert_eq!(context.events[0].event.deserialize().unwrap().event_id(), event_id);

        // There's a previous batch.
        assert!(context.has_prev);
        assert!(!context.has_next);

        // Preparing data for the next back-pagination.
        *room.prev_events.lock().await = vec![event_factory.text_msg("previous").into_timeline()];
        *room.prev_batch_token.lock().await = Some("prev2".to_owned());

        // When I backpaginate, I get the events I expect.
        let prev = paginator.paginate_backward().await.expect("paginate backward should work");
        assert!(!prev.hit_end_of_timeline);
        assert_eq!(prev.events.len(), 1);
        assert_event_matches_msg(&prev.events[0], "previous");

        // And I can backpaginate again, because there's a prev batch token
        // still.
        *room.prev_events.lock().await = vec![event_factory.text_msg("oldest").into_timeline()];
        *room.prev_batch_token.lock().await = None;

        let prev = paginator
            .paginate_backward()
            .await
            .expect("paginate backward the second time should work");
        assert!(prev.hit_end_of_timeline);
        assert_eq!(prev.events.len(), 1);
        assert_event_matches_msg(&prev.events[0], "oldest");

        // I've hit the start of the timeline, but back-paginating again will
        // return immediately.
        let prev = paginator
            .paginate_backward()
            .await
            .expect("paginate backward the third time should work");
        assert!(prev.hit_end_of_timeline);
        assert!(prev.events.is_empty());
    }

    #[async_test]
    async fn test_paginate_forward() {
        // Prepare test data.
        let room = Box::new(TestRoom::new(false, *ROOM_ID, *USER_ID));

        let event_id = event_id!("$yoyoyo");
        let event_factory = &room.event_factory;

        *room.target_event_text.lock().await = "initial".to_owned();
        *room.next_batch_token.lock().await = Some("next".to_owned());

        // When I call `Paginator::start_from`, it works,
        let paginator = Arc::new(Paginator::new(room.clone()));
        let context = paginator.start_from(event_id).await.expect("start_from should work");

        // And I get the events I expected.
        assert_eq!(context.events.len(), 1);
        assert_event_matches_msg(&context.events[0], "initial");
        assert_eq!(context.events[0].event.deserialize().unwrap().event_id(), event_id);

        // There's a next batch.
        assert!(!context.has_prev);
        assert!(context.has_next);

        // Preparing data for the next forward-pagination.
        *room.next_events.lock().await = vec![event_factory.text_msg("next").into_timeline()];
        *room.next_batch_token.lock().await = Some("next2".to_owned());

        // When I forward-paginate, I get the events I expect.
        let next = paginator.paginate_forward().await.expect("paginate forward should work");
        assert!(!next.hit_end_of_timeline);
        assert_eq!(next.events.len(), 1);
        assert_event_matches_msg(&next.events[0], "next");

        // And I can forward-paginate again, because there's a prev batch token
        // still.
        *room.next_events.lock().await = vec![event_factory.text_msg("latest").into_timeline()];
        *room.next_batch_token.lock().await = None;

        let next = paginator
            .paginate_forward()
            .await
            .expect("paginate forward the second time should work");
        assert!(next.hit_end_of_timeline);
        assert_eq!(next.events.len(), 1);
        assert_event_matches_msg(&next.events[0], "latest");

        // I've hit the start of the timeline, but back-paginating again will
        // return immediately.
        let next = paginator
            .paginate_forward()
            .await
            .expect("paginate forward the third time should work");
        assert!(next.hit_end_of_timeline);
        assert!(next.events.is_empty());
    }

    #[async_test]
    async fn test_state() {
        let room = Box::new(TestRoom::new(true, *ROOM_ID, *USER_ID));

        *room.prev_batch_token.lock().await = Some("prev".to_owned());
        *room.next_batch_token.lock().await = Some("next".to_owned());

        let paginator = Arc::new(Paginator::new(room.clone()));

        let event_id = event_id!("$yoyoyo");

        let mut state = paginator.state();

        assert_eq!(state.get(), PaginatorState::Initial);
        assert!(state.next().now_or_never().is_none());

        // Attempting to run pagination must fail and not change the state.
        assert_invalid_state(
            paginator.paginate_backward(),
            PaginatorState::Idle,
            PaginatorState::Initial,
        )
        .await;

        assert!(state.next().now_or_never().is_none());

        // Running the initial query must work.
        let p = paginator.clone();
        let join_handle = spawn(async move { p.start_from(event_id).await });

        assert_eq!(state.next().await, Some(PaginatorState::FetchingTargetEvent));
        assert!(state.next().now_or_never().is_none());

        // The query is pending. Running other operations must fail.
        assert_invalid_state(
            paginator.start_from(event_id),
            PaginatorState::Initial,
            PaginatorState::FetchingTargetEvent,
        )
        .await;

        assert_invalid_state(
            paginator.paginate_backward(),
            PaginatorState::Idle,
            PaginatorState::FetchingTargetEvent,
        )
        .await;

        assert!(state.next().now_or_never().is_none());

        // Mark the dummy room as ready. The query may now terminate.
        room.mark_ready();

        // After fetching the initial event data, the paginator switches to `Idle`.
        assert_eq!(state.next().await, Some(PaginatorState::Idle));

        join_handle.await.expect("joined failed").expect("/context failed");

        assert!(state.next().now_or_never().is_none());

        let p = paginator.clone();
        let join_handle = spawn(async move { p.paginate_backward().await });

        assert_eq!(state.next().await, Some(PaginatorState::Paginating));

        // The query is pending. Running other operations must fail.
        assert_invalid_state(
            paginator.start_from(event_id),
            PaginatorState::Initial,
            PaginatorState::Paginating,
        )
        .await;

        assert_invalid_state(
            paginator.paginate_backward(),
            PaginatorState::Idle,
            PaginatorState::Paginating,
        )
        .await;

        assert_invalid_state(
            paginator.paginate_forward(),
            PaginatorState::Idle,
            PaginatorState::Paginating,
        )
        .await;

        assert!(state.next().now_or_never().is_none());

        room.mark_ready();

        assert_eq!(state.next().await, Some(PaginatorState::Idle));

        join_handle.await.expect("joined failed").expect("/messages failed");

        assert!(state.next().now_or_never().is_none());
    }
}
