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

use std::sync::Mutex;

use eyeball::{SharedObservable, Subscriber};
use matrix_sdk_base::{deserialized_responses::TimelineEvent, SendOutsideWasm, SyncOutsideWasm};
use ruma::{api::Direction, EventId, OwnedEventId, UInt};

use crate::{
    room::{EventWithContextResponse, Messages, MessagesOptions, WeakRoom},
    Room,
};

/// Current state of a [`Paginator`].
#[derive(Debug, PartialEq, Copy, Clone)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Enum))]
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

/// Pagination token data, indicating in which state is the current pagination.
#[derive(Clone, Debug)]
enum PaginationToken {
    /// We never had a pagination token, so we'll start back-paginating from the
    /// end, or forward-paginating from the start.
    None,
    /// We paginated once before, and we received a prev/next batch token that
    /// we may reuse for the next query.
    HasMore(String),
    /// We've hit one end of the timeline (either the start or the actual end),
    /// so there's no need to continue paginating.
    HitEnd,
}

impl From<Option<String>> for PaginationToken {
    fn from(token: Option<String>) -> Self {
        match token {
            Some(val) => Self::HasMore(val),
            None => Self::None,
        }
    }
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
    ///
    /// This mutex is only taken for short periods of time, so it's sync.
    prev_batch_token: Mutex<PaginationToken>,

    /// The token to run the next forward pagination.
    ///
    /// This mutex is only taken for short periods of time, so it's sync.
    next_batch_token: Mutex<PaginationToken>,
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

    /// Did we hit *an* end of the timeline?
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

/// Reset the state to a given target on drop.
struct ResetStateGuard {
    target: Option<PaginatorState>,
    state: SharedObservable<PaginatorState>,
}

impl ResetStateGuard {
    /// Create a new reset state guard.
    fn new(state: SharedObservable<PaginatorState>, target: PaginatorState) -> Self {
        Self { target: Some(target), state }
    }

    /// Render the guard effectless, and consume it.
    fn disarm(mut self) {
        self.target = None;
    }
}

impl Drop for ResetStateGuard {
    fn drop(&mut self) {
        if let Some(target) = self.target.take() {
            self.state.set_if_not_eq(target);
        }
    }
}

impl Paginator {
    /// Create a new [`Paginator`], given a room implementation.
    pub fn new(room: Box<dyn PaginableRoom>) -> Self {
        Self {
            room,
            state: SharedObservable::new(PaginatorState::Initial),
            prev_batch_token: Mutex::new(None.into()),
            next_batch_token: Mutex::new(None.into()),
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

    /// Prepares the paginator to be in the idle state, ready for backwards- and
    /// forwards- pagination.
    ///
    /// Will return an `InvalidPreviousState` error if the paginator is busy
    /// (running /context or /messages).
    pub(super) fn set_idle_state(
        &self,
        prev_batch_token: Option<String>,
        next_batch_token: Option<String>,
    ) -> Result<(), PaginatorError> {
        let prev_state = self.state.get();

        match prev_state {
            PaginatorState::Initial | PaginatorState::Idle => {}
            PaginatorState::FetchingTargetEvent | PaginatorState::Paginating => {
                // The paginator was busy. Don't interrupt it.
                return Err(PaginatorError::InvalidPreviousState {
                    // Technically it's initial OR idle, but we don't really care here.
                    expected: PaginatorState::Idle,
                    actual: prev_state,
                });
            }
        }

        self.state.set_if_not_eq(PaginatorState::Idle);
        *self.prev_batch_token.lock().unwrap() = prev_batch_token.into();
        *self.next_batch_token.lock().unwrap() = next_batch_token.into();

        Ok(())
    }

    /// Returns the current previous batch token, as stored in this paginator.
    pub(super) fn prev_batch_token(&self) -> Option<String> {
        match &*self.prev_batch_token.lock().unwrap() {
            PaginationToken::HitEnd | PaginationToken::None => None,
            PaginationToken::HasMore(token) => Some(token.clone()),
        }
    }

    /// Starts the pagination from the initial event, requesting `num_events`
    /// additional context events.
    ///
    /// Only works for fresh [`Paginator`] objects, which are in the
    /// [`PaginatorState::Initial`] state.
    pub async fn start_from(
        &self,
        event_id: &EventId,
        num_events: UInt,
    ) -> Result<StartFromResult, PaginatorError> {
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

        let reset_state_guard = ResetStateGuard::new(self.state.clone(), PaginatorState::Initial);

        // TODO: do we want to lazy load members?
        let lazy_load_members = true;

        let response =
            self.room.event_with_context(event_id, lazy_load_members, num_events).await?;

        // NOTE: it's super important to not have any `await` after this point, since we
        // don't want the task to be interrupted anymore, or the internal state
        // may become incorrect.

        let has_prev = response.prev_batch_token.is_some();
        let has_next = response.next_batch_token.is_some();

        *self.prev_batch_token.lock().unwrap() = match response.prev_batch_token {
            Some(token) => PaginationToken::HasMore(token),
            None => PaginationToken::HitEnd,
        };
        *self.next_batch_token.lock().unwrap() = match response.next_batch_token {
            Some(token) => PaginationToken::HasMore(token),
            None => PaginationToken::HitEnd,
        };

        // Forget the reset state guard, so its Drop method is not called.
        reset_state_guard.disarm();
        // And set the final state.
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

    /// Runs a backward pagination (requesting `num_events` to the server), from
    /// the current state of the object.
    ///
    /// Will return immediately if we have already hit the start of the
    /// timeline.
    ///
    /// May return an error if it's already paginating, or if the call to
    /// /messages failed.
    pub async fn paginate_backward(
        &self,
        num_events: UInt,
    ) -> Result<PaginationResult, PaginatorError> {
        self.paginate(Direction::Backward, num_events, &self.prev_batch_token).await
    }

    /// Returns whether we've hit the start of the timeline.
    ///
    /// This is true if, and only if, we didn't have a previous-batch token and
    /// running backwards pagination would be useless.
    pub fn hit_timeline_start(&self) -> bool {
        matches!(*self.prev_batch_token.lock().unwrap(), PaginationToken::HitEnd)
    }

    /// Returns whether we've hit the end of the timeline.
    ///
    /// This is true if, and only if, we didn't have a next-batch token and
    /// running forwards pagination would be useless.
    pub fn hit_timeline_end(&self) -> bool {
        matches!(*self.next_batch_token.lock().unwrap(), PaginationToken::HitEnd)
    }

    /// Runs a forward pagination (requesting `num_events` to the server), from
    /// the current state of the object.
    ///
    /// Will return immediately if we have already hit the end of the timeline.
    ///
    /// May return an error if it's already paginating, or if the call to
    /// /messages failed.
    pub async fn paginate_forward(
        &self,
        num_events: UInt,
    ) -> Result<PaginationResult, PaginatorError> {
        self.paginate(Direction::Forward, num_events, &self.next_batch_token).await
    }

    /// Paginate in the given direction, requesting `num_events` events to the
    /// server, using the `token_lock` to read from and write the pagination
    /// token.
    async fn paginate(
        &self,
        dir: Direction,
        num_events: UInt,
        token_lock: &Mutex<PaginationToken>,
    ) -> Result<PaginationResult, PaginatorError> {
        self.check_state(PaginatorState::Idle)?;

        let token = {
            let token = token_lock.lock().unwrap();
            match &*token {
                PaginationToken::None => None,
                PaginationToken::HasMore(val) => Some(val.clone()),
                PaginationToken::HitEnd => {
                    return Ok(PaginationResult { events: Vec::new(), hit_end_of_timeline: true });
                }
            }
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

        let reset_state_guard = ResetStateGuard::new(self.state.clone(), PaginatorState::Idle);

        let mut options = MessagesOptions::new(dir).from(token.as_deref());
        options.limit = num_events;

        // In case of error, the state is reset to idle automatically thanks to
        // reset_state_guard.
        let response = self.room.messages(options).await?;

        // NOTE: it's super important to not have any `await` after this point, since we
        // don't want the task to be interrupted anymore, or the internal state
        // may be incorrect.

        let hit_end_of_timeline = response.end.is_none();

        *token_lock.lock().unwrap() = match response.end {
            Some(val) => PaginationToken::HasMore(val),
            None => PaginationToken::HitEnd,
        };

        // TODO: what to do with state events?

        // Forget the reset state guard, so its Drop method is not called.
        reset_state_guard.disarm();
        // And set the final state.
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
    /// ## Parameters
    ///
    /// - `event_id` is the identifier of the target event.
    /// - `lazy_load_members` controls whether room membership events are lazily
    ///   loaded as context
    /// state events.
    /// - `num_events` is the number of events (including the fetched event) to
    /// return as context.
    ///
    /// ## Returns
    ///
    /// Must return [`PaginatorError::EventNotFound`] whenever the target event
    /// could not be found, instead of causing an http `Err` result.
    async fn event_with_context(
        &self,
        event_id: &EventId,
        lazy_load_members: bool,
        num_events: UInt,
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
        num_events: UInt,
    ) -> Result<EventWithContextResponse, PaginatorError> {
        let response = match self.event_with_context(event_id, lazy_load_members, num_events).await
        {
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

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl PaginableRoom for WeakRoom {
    async fn event_with_context(
        &self,
        event_id: &EventId,
        lazy_load_members: bool,
        num_events: UInt,
    ) -> Result<EventWithContextResponse, PaginatorError> {
        let Some(room) = self.get() else {
            // Client is shutting down, return a default response.
            return Ok(EventWithContextResponse::default());
        };

        PaginableRoom::event_with_context(&room, event_id, lazy_load_members, num_events).await
    }

    /// Runs a /messages query for the given room.
    async fn messages(&self, opts: MessagesOptions) -> Result<Messages, PaginatorError> {
        let Some(room) = self.get() else {
            // Client is shutting down, return a default response.
            return Ok(Messages::default());
        };

        PaginableRoom::messages(&room, opts).await
    }
}

#[cfg(all(not(target_arch = "wasm32"), test))]
mod tests {
    use std::sync::Arc;

    use assert_matches2::assert_let;
    use async_trait::async_trait;
    use futures_core::Future;
    use futures_util::FutureExt as _;
    use matrix_sdk_base::deserialized_responses::TimelineEvent;
    use matrix_sdk_test::async_test;
    use once_cell::sync::Lazy;
    use ruma::{api::Direction, event_id, room_id, uint, user_id, EventId, RoomId, UInt, UserId};
    use tokio::{
        spawn,
        sync::{Mutex, Notify},
        task::AbortHandle,
    };

    use super::{PaginableRoom, PaginatorError, PaginatorState};
    use crate::{
        event_cache::paginator::Paginator,
        room::{EventWithContextResponse, Messages, MessagesOptions},
        test_utils::{assert_event_matches_msg, events::EventFactory},
    };

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
            num_events: UInt,
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

            // Properly simulate `num_events`: take either the closest num_events events
            // before, or use all of the before events and then consume after events.
            let mut num_events = u64::from(num_events) as usize;

            let prev_events = self.prev_events.lock().await;

            let events_before = if prev_events.is_empty() {
                Vec::new()
            } else {
                let len = prev_events.len();
                let take_before = num_events.min(len);
                // Subtract is safe because take_before <= num_events.
                num_events -= take_before;
                // Subtract is safe because take_before <= len
                prev_events[len - take_before..len].to_vec()
            };

            let events_after = self.next_events.lock().await;
            let events_after = if events_after.is_empty() {
                Vec::new()
            } else {
                events_after[0..num_events.min(events_after.len())].to_vec()
            };

            return Ok(EventWithContextResponse {
                event: Some(event),
                events_before,
                events_after,
                prev_batch_token: self.prev_batch_token.lock().await.clone(),
                next_batch_token: self.next_batch_token.lock().await.clone(),
                state: Vec::new(),
            });
        }

        async fn messages(&self, opts: MessagesOptions) -> Result<Messages, PaginatorError> {
            if self.wait_for_ready {
                self.room_ready.notified().await;
            }

            let limit = u64::from(opts.limit) as usize;

            let (end, events) = match opts.dir {
                Direction::Backward => {
                    let events = self.prev_events.lock().await;
                    let events = if events.is_empty() {
                        Vec::new()
                    } else {
                        let len = events.len();
                        let take_before = limit.min(len);
                        // Subtract is safe because take_before <= len
                        events[len - take_before..len].to_vec()
                    };
                    (self.prev_batch_token.lock().await.clone(), events)
                }

                Direction::Forward => {
                    let events = self.next_events.lock().await;
                    let events = if events.is_empty() {
                        Vec::new()
                    } else {
                        events[0..limit.min(events.len())].to_vec()
                    };
                    (self.next_batch_token.lock().await.clone(), events)
                }
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
        let context =
            paginator.start_from(event_id, uint!(100)).await.expect("start_from should work");

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
    async fn test_start_from_with_num_events() {
        // Prepare test data.
        let room = Box::new(TestRoom::new(false, *ROOM_ID, *USER_ID));

        let event_id = event_id!("$yoyoyo");
        let event_factory = &room.event_factory;

        *room.target_event_text.lock().await = "fetch_from".to_owned();
        *room.prev_events.lock().await = (0..100)
            .rev()
            .map(|i| {
                TimelineEvent::new(event_factory.text_msg(format!("ev{i}")).into_raw_timeline())
            })
            .collect();

        // When I call `Paginator::start_from`, it works,
        let paginator = Arc::new(Paginator::new(room.clone()));
        let context =
            paginator.start_from(event_id, uint!(10)).await.expect("start_from should work");

        // Then I only get 10 events + the target event, even if there was more than 10
        // events in the room.
        assert_eq!(context.events.len(), 11);

        for i in 0..10 {
            assert_event_matches_msg(&context.events[i], &format!("ev{i}"));
        }
        assert_event_matches_msg(&context.events[10], "fetch_from");
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

        assert!(!paginator.hit_timeline_start(), "we must have a prev-batch token");
        assert!(
            !paginator.hit_timeline_end(),
            "we don't know about the status of the next-batch token"
        );

        let context =
            paginator.start_from(event_id, uint!(100)).await.expect("start_from should work");

        // And I get the events I expected.
        assert_eq!(context.events.len(), 1);
        assert_event_matches_msg(&context.events[0], "initial");
        assert_eq!(context.events[0].event.deserialize().unwrap().event_id(), event_id);

        // There's a previous batch, but no next batch.
        assert!(context.has_prev);
        assert!(!context.has_next);

        assert!(!paginator.hit_timeline_start());
        assert!(paginator.hit_timeline_end());

        // Preparing data for the next back-pagination.
        *room.prev_events.lock().await = vec![event_factory.text_msg("previous").into_timeline()];
        *room.prev_batch_token.lock().await = Some("prev2".to_owned());

        // When I backpaginate, I get the events I expect.
        let prev =
            paginator.paginate_backward(uint!(100)).await.expect("paginate backward should work");
        assert!(!prev.hit_end_of_timeline);
        assert!(!paginator.hit_timeline_start());
        assert_eq!(prev.events.len(), 1);
        assert_event_matches_msg(&prev.events[0], "previous");

        // And I can backpaginate again, because there's a prev batch token
        // still.
        *room.prev_events.lock().await = vec![event_factory.text_msg("oldest").into_timeline()];
        *room.prev_batch_token.lock().await = None;

        let prev = paginator
            .paginate_backward(uint!(100))
            .await
            .expect("paginate backward the second time should work");
        assert!(prev.hit_end_of_timeline);
        assert!(paginator.hit_timeline_start());
        assert_eq!(prev.events.len(), 1);
        assert_event_matches_msg(&prev.events[0], "oldest");

        // I've hit the start of the timeline, but back-paginating again will
        // return immediately.
        let prev = paginator
            .paginate_backward(uint!(100))
            .await
            .expect("paginate backward the third time should work");
        assert!(prev.hit_end_of_timeline);
        assert!(paginator.hit_timeline_start());
        assert!(prev.events.is_empty());
    }

    #[async_test]
    async fn test_paginate_backward_with_limit() {
        // Prepare test data.
        let room = Box::new(TestRoom::new(false, *ROOM_ID, *USER_ID));

        let event_id = event_id!("$yoyoyo");
        let event_factory = &room.event_factory;

        *room.target_event_text.lock().await = "initial".to_owned();
        *room.prev_batch_token.lock().await = Some("prev".to_owned());

        // When I call `Paginator::start_from`, it works,
        let paginator = Arc::new(Paginator::new(room.clone()));
        let context =
            paginator.start_from(event_id, uint!(100)).await.expect("start_from should work");

        // And I get the events I expected.
        assert_eq!(context.events.len(), 1);
        assert_event_matches_msg(&context.events[0], "initial");
        assert_eq!(context.events[0].event.deserialize().unwrap().event_id(), event_id);

        // There's a previous batch.
        assert!(context.has_prev);
        assert!(!context.has_next);

        // Preparing data for the next back-pagination.
        *room.prev_events.lock().await = (0..100)
            .rev()
            .map(|i| {
                TimelineEvent::new(event_factory.text_msg(format!("prev{i}")).into_raw_timeline())
            })
            .collect();
        *room.prev_batch_token.lock().await = None;

        // When I backpaginate and request 100 events, I get only 10 events.
        let prev =
            paginator.paginate_backward(uint!(10)).await.expect("paginate backward should work");
        assert!(prev.hit_end_of_timeline);
        assert_eq!(prev.events.len(), 10);
        for i in 0..10 {
            assert_event_matches_msg(&prev.events[i], &format!("prev{}", 9 - i));
        }
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
        assert!(!paginator.hit_timeline_end(), "we must have a next-batch token");
        assert!(
            !paginator.hit_timeline_start(),
            "we don't know about the status of the prev-batch token"
        );

        let context =
            paginator.start_from(event_id, uint!(100)).await.expect("start_from should work");

        // And I get the events I expected.
        assert_eq!(context.events.len(), 1);
        assert_event_matches_msg(&context.events[0], "initial");
        assert_eq!(context.events[0].event.deserialize().unwrap().event_id(), event_id);

        // There's a next batch, but no previous batch (i.e. we've hit the start of the
        // timeline).
        assert!(!context.has_prev);
        assert!(context.has_next);

        assert!(paginator.hit_timeline_start());
        assert!(!paginator.hit_timeline_end());

        // Preparing data for the next forward-pagination.
        *room.next_events.lock().await = vec![event_factory.text_msg("next").into_timeline()];
        *room.next_batch_token.lock().await = Some("next2".to_owned());

        // When I forward-paginate, I get the events I expect.
        let next =
            paginator.paginate_forward(uint!(100)).await.expect("paginate forward should work");
        assert!(!next.hit_end_of_timeline);
        assert_eq!(next.events.len(), 1);
        assert_event_matches_msg(&next.events[0], "next");
        assert!(!paginator.hit_timeline_end());

        // And I can forward-paginate again, because there's a prev batch token
        // still.
        *room.next_events.lock().await = vec![event_factory.text_msg("latest").into_timeline()];
        *room.next_batch_token.lock().await = None;

        let next = paginator
            .paginate_forward(uint!(100))
            .await
            .expect("paginate forward the second time should work");
        assert!(next.hit_end_of_timeline);
        assert_eq!(next.events.len(), 1);
        assert_event_matches_msg(&next.events[0], "latest");
        assert!(paginator.hit_timeline_end());

        // I've hit the start of the timeline, but back-paginating again will
        // return immediately.
        let next = paginator
            .paginate_forward(uint!(100))
            .await
            .expect("paginate forward the third time should work");
        assert!(next.hit_end_of_timeline);
        assert!(next.events.is_empty());
        assert!(paginator.hit_timeline_end());
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
            paginator.paginate_backward(uint!(100)),
            PaginatorState::Idle,
            PaginatorState::Initial,
        )
        .await;

        assert!(state.next().now_or_never().is_none());

        // Running the initial query must work.
        let p = paginator.clone();
        let join_handle = spawn(async move { p.start_from(event_id, uint!(100)).await });

        assert_eq!(state.next().await, Some(PaginatorState::FetchingTargetEvent));
        assert!(state.next().now_or_never().is_none());

        // The query is pending. Running other operations must fail.
        assert_invalid_state(
            paginator.start_from(event_id, uint!(100)),
            PaginatorState::Initial,
            PaginatorState::FetchingTargetEvent,
        )
        .await;

        assert_invalid_state(
            paginator.paginate_backward(uint!(100)),
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
        let join_handle = spawn(async move { p.paginate_backward(uint!(100)).await });

        assert_eq!(state.next().await, Some(PaginatorState::Paginating));

        // The query is pending. Running other operations must fail.
        assert_invalid_state(
            paginator.start_from(event_id, uint!(100)),
            PaginatorState::Initial,
            PaginatorState::Paginating,
        )
        .await;

        assert_invalid_state(
            paginator.paginate_backward(uint!(100)),
            PaginatorState::Idle,
            PaginatorState::Paginating,
        )
        .await;

        assert_invalid_state(
            paginator.paginate_forward(uint!(100)),
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

    mod aborts {
        use super::*;

        #[derive(Clone, Default)]
        struct AbortingRoom {
            abort_handle: Arc<Mutex<Option<AbortHandle>>>,
            room_ready: Arc<Notify>,
        }

        impl AbortingRoom {
            async fn wait_abort_and_yield(&self) -> ! {
                // Wait for the controller to tell us we're ready.
                self.room_ready.notified().await;

                // Abort the given handle.
                let mut guard = self.abort_handle.lock().await;
                let handle = guard.take().expect("only call me when i'm initialized");
                handle.abort();

                // Enter an endless loop of yielding.
                loop {
                    tokio::task::yield_now().await;
                }
            }
        }

        #[async_trait]
        impl PaginableRoom for AbortingRoom {
            async fn event_with_context(
                &self,
                _event_id: &EventId,
                _lazy_load_members: bool,
                _num_events: UInt,
            ) -> Result<EventWithContextResponse, PaginatorError> {
                self.wait_abort_and_yield().await
            }

            async fn messages(&self, _opts: MessagesOptions) -> Result<Messages, PaginatorError> {
                self.wait_abort_and_yield().await
            }
        }

        #[async_test]
        async fn test_abort_while_starting_from() {
            let room = Box::new(AbortingRoom::default());

            let paginator = Arc::new(Paginator::new(room.clone()));

            let mut state = paginator.state();

            assert_eq!(state.get(), PaginatorState::Initial);
            assert!(state.next().now_or_never().is_none());

            // When I try to start the initial query…
            let p = paginator.clone();
            let join_handle = spawn(async move {
                let _ = p.start_from(event_id!("$yoyoyo"), uint!(100)).await;
            });

            *room.abort_handle.lock().await = Some(join_handle.abort_handle());

            assert_eq!(state.next().await, Some(PaginatorState::FetchingTargetEvent));
            assert!(state.next().now_or_never().is_none());

            room.room_ready.notify_one();

            // But it's aborted when awaiting the task.
            let join_result = join_handle.await;
            assert!(join_result.unwrap_err().is_cancelled());

            // Then the state is reset to initial.
            assert_eq!(state.next().await, Some(PaginatorState::Initial));
            assert!(state.next().now_or_never().is_none());
        }

        #[async_test]
        async fn test_abort_while_paginating() {
            let room = Box::new(AbortingRoom::default());

            // Assuming a paginator ready to back- or forward- paginate,
            let paginator = Paginator::new(room.clone());
            paginator.set_idle_state(Some("prev".to_owned()), Some("next".to_owned())).unwrap();

            let paginator = Arc::new(paginator);

            let mut state = paginator.state();

            assert_eq!(state.get(), PaginatorState::Idle);
            assert!(state.next().now_or_never().is_none());

            // When I try to back-paginate…
            let p = paginator.clone();
            let join_handle = spawn(async move {
                let _ = p.paginate_backward(uint!(100)).await;
            });

            *room.abort_handle.lock().await = Some(join_handle.abort_handle());

            assert_eq!(state.next().await, Some(PaginatorState::Paginating));
            assert!(state.next().now_or_never().is_none());

            room.room_ready.notify_one();

            // But it's aborted when awaiting the task.
            let join_result = join_handle.await;
            assert!(join_result.unwrap_err().is_cancelled());

            // Then the state is reset to idle.
            assert_eq!(state.next().await, Some(PaginatorState::Idle));
            assert!(state.next().now_or_never().is_none());

            // And ditto for forward pagination.
            let p = paginator.clone();
            let join_handle = spawn(async move {
                let _ = p.paginate_forward(uint!(100)).await;
            });

            *room.abort_handle.lock().await = Some(join_handle.abort_handle());

            assert_eq!(state.next().await, Some(PaginatorState::Paginating));
            assert!(state.next().now_or_never().is_none());

            room.room_ready.notify_one();

            let join_result = join_handle.await;
            assert!(join_result.unwrap_err().is_cancelled());

            assert_eq!(state.next().await, Some(PaginatorState::Idle));
            assert!(state.next().now_or_never().is_none());
        }
    }
}
