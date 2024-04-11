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

enum DirThing {
    Forward,
    Backward,
}

impl From<Direction> for DirThing {
    fn from(dir: Direction) -> Self {
        match dir {
            Direction::Forward => Self::Forward,
            Direction::Backward => Self::Backward,
        }
    }
}

impl DirThing {
    async fn token(&self, paginator: &Paginator) -> Result<Option<String>, ()> {
        match self {
            DirThing::Backward => {
                let prev_batch_token = paginator.prev_batch_token.lock().await;
                if prev_batch_token.is_none() {
                    return Err(());
                };
                Ok(prev_batch_token.clone())
            }
            DirThing::Forward => {
                let next_batch_token = paginator.next_batch_token.lock().await;
                if next_batch_token.is_none() {
                    return Err(());
                };
                Ok(next_batch_token.clone())
            }
        }
    }

    async fn hit_end_of_timeline(
        &self,
        paginator: &Paginator,
        response_end: Option<String>,
    ) -> bool {
        let hit = response_end.is_none();
        match self {
            DirThing::Backward => {
                *paginator.prev_batch_token.lock().await = response_end;
            }
            DirThing::Forward => {
                *paginator.next_batch_token.lock().await = response_end;
            }
        }
        hit
    }
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
        self.paginate(Direction::Backward).await
    }

    /// Runs a forward pagination, from the current state of the object.
    ///
    /// Will return immediately if we have already hit the end of the timeline.
    ///
    /// May return an error if it's already paginating, or if the call to
    /// /messages failed.
    pub async fn paginate_forward(&self) -> Result<PaginationResult, PaginatorError> {
        self.paginate(Direction::Forward).await
    }

    async fn paginate(&self, dir: Direction) -> Result<PaginationResult, PaginatorError> {
        self.check_state(PaginatorState::Idle)?;

        let thing = DirThing::from(dir);

        let Ok(token) = thing.token(self).await else {
            return Ok(PaginationResult { events: Vec::new(), hit_end_of_timeline: true });
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

        let events = response.chunk;
        let hit_end_of_timeline = thing.hit_end_of_timeline(self, response.end).await;

        // TODO: what to do with state events?

        self.state.set(PaginatorState::Idle);

        Ok(PaginationResult { events, hit_end_of_timeline })
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

    #[derive(Default, Clone)]
    struct DummyRoom {
        room_ready: Arc<Notify>,
    }

    impl DummyRoom {
        /// Unblocks the next request.
        fn mark_ready(&self) {
            self.room_ready.notify_one();
        }
    }

    static ROOM_ID: Lazy<&RoomId> = Lazy::new(|| room_id!("!dune:herbert.org"));
    static USER_ID: Lazy<&UserId> = Lazy::new(|| user_id!("@paul:atreid.es"));

    #[async_trait]
    impl PaginableRoom for DummyRoom {
        async fn event_with_context(
            &self,
            event_id: &EventId,
            _lazy_load_members: bool,
        ) -> Result<EventWithContextResponse, PaginatorError> {
            // Wait for the room to be marked as ready first.
            self.room_ready.notified().await;

            let event_factory = EventFactory::new().room(*ROOM_ID).sender(*USER_ID);

            let event = event_factory.text_msg("hello!").event_id(event_id).into_raw_timeline();

            let before = (0..10)
                .rev()
                .map(|i| {
                    TimelineEvent::new(event_factory.text_msg(format!("{i}")).into_raw_timeline())
                })
                .collect();

            let after = (10..20)
                .map(|i| {
                    TimelineEvent::new(event_factory.text_msg(format!("{i}")).into_raw_timeline())
                })
                .collect();

            return Ok(EventWithContextResponse {
                event: Some(TimelineEvent::new(event)),
                events_before: before,
                events_after: after,
                prev_batch_token: Some("prev".to_owned()),
                next_batch_token: Some("next".to_owned()),
                state: Vec::new(),
            });
        }

        async fn messages(&self, opts: MessagesOptions) -> Result<Messages, PaginatorError> {
            assert_eq!(
                opts.from.as_deref(),
                Some("prev"),
                "we must receive a single back-pagination"
            );
            assert_eq!(opts.dir, Direction::Backward, "we must receive a single back-pagination");

            self.room_ready.notified().await;

            let event_factory = EventFactory::new().room(*ROOM_ID).sender(*USER_ID);

            let chunk = (20..30)
                .rev()
                .map(|i| {
                    TimelineEvent::new(event_factory.text_msg(format!("{i}")).into_raw_timeline())
                })
                .collect();

            return Ok(Messages {
                start: opts.from.unwrap().to_owned(),
                end: None,
                chunk,
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
    async fn test_state() {
        let room = Box::<DummyRoom>::default();

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

        let context = join_handle.await.expect("joined failed").expect("/context failed");
        assert!(context.has_prev);
        assert!(context.has_next);
        assert_eq!(context.events.len(), 21);
        for i in 0..10 {
            assert_event_matches_msg(&context.events[i], &format!("{i}"));
        }
        assert_event_matches_msg(&context.events[10], "hello!");
        for i in 0..10 {
            assert_event_matches_msg(&context.events[i + 11], &format!("{}", i + 10));
        }

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

        let messages = join_handle.await.expect("joined failed").expect("/messages failed");
        assert!(messages.hit_end_of_timeline);
        for i in 0..10 {
            // It's a backward pagination, so messages are in reverse topological order.
            assert_event_matches_msg(&messages.events[i], &format!("{}", 29 - i));
        }

        assert!(state.next().now_or_never().is_none());
    }
}
