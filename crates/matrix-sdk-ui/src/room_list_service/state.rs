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
// See the License for that specific language governing permissions and
// limitations under the License.

//! States and actions for the `RoomList` state machine.

use std::{future::ready, sync::Mutex};

use eyeball::{SharedObservable, Subscriber};
use matrix_sdk::{SlidingSync, SlidingSyncMode, sliding_sync::Range};
use ruma::time::{Duration, Instant};

use super::Error;

pub const ALL_ROOMS_LIST_NAME: &str = "all_rooms";

/// The state of the [`super::RoomList`].
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    /// That's the first initial state.
    Init,

    /// At this state, the first rooms have been synced.
    SettingUp,

    /// At this state, the system is recovering from `Error` or `Terminated`, or
    /// the time between the last sync was too long (see
    /// `StateMachine::state_lifespan` to learn more). It's similar to
    /// `SettingUp` but some lists may already exist, actions
    /// are then slightly different.
    Recovering,

    /// At this state, all rooms are syncing.
    Running,

    /// At this state, the sync has been stopped because an error happened.
    Error { from: Box<State> },

    /// At this state, the sync has been stopped because it was requested.
    Terminated { from: Box<State> },
}

/// Default value for `StateMachine::state_lifespan`.
const DEFAULT_STATE_LIFESPAN: Duration = Duration::from_secs(1800);

/// The state machine used to transition between the [`State`]s.
#[derive(Debug)]
pub struct StateMachine {
    /// The current state of the `RoomListService`.
    state: SharedObservable<State>,

    /// Last time the state has been updated.
    ///
    /// When the state has not been updated since a long time, we want to enter
    /// the [`State::Recovering`] state. Why do we need to do that? Because in
    /// some cases, the user might have received many updates between two
    /// distant syncs. If the sliding sync list range was too large, like
    /// 0..=499, the next sync is likely to be heavy and potentially slow.
    /// In this case, it's preferable to jump back onto `Recovering`, which will
    /// reset the range, so that the next sync will be fast for the client.
    ///
    /// To be used in coordination with `Self::state_lifespan`.
    ///
    /// This mutex is only taken for short periods of time, so it's sync.
    last_state_update_time: Mutex<Instant>,

    /// The maximum time before considering the state as “too old”.
    ///
    /// To be used in coordination with `Self::last_state_update_time`.
    state_lifespan: Duration,
}

impl StateMachine {
    pub(super) fn new() -> Self {
        StateMachine {
            state: SharedObservable::new(State::Init),
            last_state_update_time: Mutex::new(Instant::now()),
            state_lifespan: DEFAULT_STATE_LIFESPAN,
        }
    }

    /// Get the current state.
    pub(super) fn get(&self) -> State {
        self.state.get()
    }

    /// Clone the inner [`Self::state`].
    pub(super) fn cloned_state(&self) -> SharedObservable<State> {
        self.state.clone()
    }

    /// Set the new state.
    ///
    /// Setting a new state will update `Self::last_state_update`.
    pub(super) fn set(&self, state: State) {
        let mut last_state_update_time = self.last_state_update_time.lock().unwrap();
        *last_state_update_time = Instant::now();

        self.state.set(state);
    }

    /// Subscribe to state updates.
    pub fn subscribe(&self) -> Subscriber<State> {
        self.state.subscribe()
    }

    /// Transition to the next state, and execute the necessary transition on
    /// the sliding sync list.
    pub(super) async fn next(&self, sliding_sync: &SlidingSync) -> Result<State, Error> {
        use State::*;

        let next_state = match self.get() {
            Init => SettingUp,

            SettingUp | Recovering => {
                set_all_rooms_to_growing_sync_mode(sliding_sync).await?;
                Running
            }

            Running => {
                // We haven't changed the state for a while, we go back to `Recovering` to avoid
                // requesting potentially large data. See `Self::last_state_update` to learn
                // the details.
                if self.last_state_update_time.lock().unwrap().elapsed() > self.state_lifespan {
                    set_all_rooms_to_selective_sync_mode(sliding_sync).await?;

                    Recovering
                } else {
                    Running
                }
            }

            Error { from: previous_state } | Terminated { from: previous_state } => {
                match previous_state.as_ref() {
                    // Unreachable state.
                    Error { .. } | Terminated { .. } => {
                        unreachable!(
                            "It's impossible to reach `Error` or `Terminated` from `Error` or `Terminated`"
                        );
                    }

                    // If the previous state was `Running`, we enter the `Recovering` state.
                    Running => {
                        set_all_rooms_to_selective_sync_mode(sliding_sync).await?;
                        Recovering
                    }

                    // Jump back to the previous state that led to this termination.
                    state => state.to_owned(),
                }
            }
        };

        Ok(next_state)
    }
}

async fn set_all_rooms_to_growing_sync_mode(sliding_sync: &SlidingSync) -> Result<(), Error> {
    sliding_sync
        .on_list(ALL_ROOMS_LIST_NAME, |list| {
            list.set_sync_mode(SlidingSyncMode::new_growing(ALL_ROOMS_DEFAULT_GROWING_BATCH_SIZE));

            ready(())
        })
        .await
        .ok_or_else(|| Error::UnknownList(ALL_ROOMS_LIST_NAME.to_owned()))
}

async fn set_all_rooms_to_selective_sync_mode(sliding_sync: &SlidingSync) -> Result<(), Error> {
    sliding_sync
        .on_list(ALL_ROOMS_LIST_NAME, |list| {
            list.set_sync_mode(
                SlidingSyncMode::new_selective().add_range(ALL_ROOMS_DEFAULT_SELECTIVE_RANGE),
            );

            ready(())
        })
        .await
        .ok_or_else(|| Error::UnknownList(ALL_ROOMS_LIST_NAME.to_owned()))
}

/// Default `batch_size` for the selective sync-mode of the
/// `ALL_ROOMS_LIST_NAME` list.
pub const ALL_ROOMS_DEFAULT_SELECTIVE_RANGE: Range = 0..=19;

/// Default `batch_size` for the growing sync-mode of the `ALL_ROOMS_LIST_NAME`
/// list.
pub const ALL_ROOMS_DEFAULT_GROWING_BATCH_SIZE: u32 = 100;

#[cfg(test)]
mod tests {
    use matrix_sdk_test::async_test;
    use tokio::time::sleep;

    use super::{super::tests::new_room_list, *};

    #[async_test]
    async fn test_states() -> Result<(), Error> {
        let room_list = new_room_list().await?;
        let sliding_sync = room_list.sliding_sync();

        let state_machine = StateMachine::new();

        // Hypothetical error.
        {
            state_machine.set(State::Error { from: Box::new(state_machine.get()) });

            // Back to the previous state.
            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::Init);
        }

        // Hypothetical termination.
        {
            state_machine.set(State::Terminated { from: Box::new(state_machine.get()) });

            // Back to the previous state.
            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::Init);
        }

        // Next state.
        state_machine.set(state_machine.next(sliding_sync).await?);
        assert_eq!(state_machine.get(), State::SettingUp);

        // Hypothetical error.
        {
            state_machine.set(State::Error { from: Box::new(state_machine.get()) });

            // Back to the previous state.
            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::SettingUp);
        }

        // Hypothetical termination.
        {
            state_machine.set(State::Terminated { from: Box::new(state_machine.get()) });

            // Back to the previous state.
            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::SettingUp);
        }

        // Next state.
        state_machine.set(state_machine.next(sliding_sync).await?);
        assert_eq!(state_machine.get(), State::Running);

        // Hypothetical error.
        {
            state_machine.set(State::Error { from: Box::new(state_machine.get()) });

            // Jump to the **recovering** state!
            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::Recovering);

            // Now, back to the previous state.
            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::Running);
        }

        // Hypothetical termination.
        {
            state_machine.set(State::Terminated { from: Box::new(state_machine.get()) });

            // Jump to the **recovering** state!
            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::Recovering);

            // Now, back to the previous state.
            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::Running);
        }

        // Hypothetical error when recovering.
        {
            state_machine.set(State::Error { from: Box::new(State::Recovering) });

            // Back to the previous state.
            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::Recovering);
        }

        // Hypothetical termination when recovering.
        {
            state_machine.set(State::Terminated { from: Box::new(State::Recovering) });

            // Back to the previous state.
            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::Recovering);
        }

        Ok(())
    }

    #[async_test]
    async fn test_recover_state_after_delay() -> Result<(), Error> {
        let room_list = new_room_list().await?;
        let sliding_sync = room_list.sliding_sync();

        let mut state_machine = StateMachine::new();
        state_machine.state_lifespan = Duration::from_millis(50);

        {
            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::SettingUp);

            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::Running);

            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::Running);

            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::Running);
        }

        // Time passes.
        sleep(Duration::from_millis(100)).await;

        {
            // Time has elapsed, time to recover.
            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::Recovering);

            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::Running);

            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::Running);

            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::Running);
        }

        // Time passes, again. Just to test everything is going well.
        sleep(Duration::from_millis(100)).await;

        {
            // Time has elapsed, time to recover.
            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::Recovering);

            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::Running);

            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::Running);

            state_machine.set(state_machine.next(sliding_sync).await?);
            assert_eq!(state_machine.get(), State::Running);
        }

        Ok(())
    }

    #[async_test]
    async fn test_action_set_all_rooms_list_to_growing_and_selective_sync_mode() -> Result<(), Error>
    {
        let room_list = new_room_list().await?;
        let sliding_sync = room_list.sliding_sync();

        // List is present, in Selective mode.
        assert_eq!(
            sliding_sync
                .on_list(ALL_ROOMS_LIST_NAME, |list| ready(matches!(
                    list.sync_mode(),
                    SlidingSyncMode::Selective { ranges } if ranges == vec![ALL_ROOMS_DEFAULT_SELECTIVE_RANGE]
                )))
                .await,
            Some(true)
        );

        // Run the action!
        set_all_rooms_to_growing_sync_mode(sliding_sync).await.unwrap();

        // List is still present, in Growing mode.
        assert_eq!(
            sliding_sync
                .on_list(ALL_ROOMS_LIST_NAME, |list| ready(matches!(
                    list.sync_mode(),
                    SlidingSyncMode::Growing {
                        batch_size, ..
                    } if batch_size == ALL_ROOMS_DEFAULT_GROWING_BATCH_SIZE
                )))
                .await,
            Some(true)
        );

        // Run the other action!
        set_all_rooms_to_selective_sync_mode(sliding_sync).await.unwrap();

        // List is still present, in Selective mode.
        assert_eq!(
            sliding_sync
                .on_list(ALL_ROOMS_LIST_NAME, |list| ready(matches!(
                    list.sync_mode(),
                    SlidingSyncMode::Selective { ranges } if ranges == vec![ALL_ROOMS_DEFAULT_SELECTIVE_RANGE]
                )))
                .await,
            Some(true)
        );

        Ok(())
    }
}
