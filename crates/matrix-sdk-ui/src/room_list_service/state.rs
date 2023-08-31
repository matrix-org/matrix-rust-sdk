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

use std::future::ready;

use async_trait::async_trait;
use matrix_sdk::{sliding_sync::Range, SlidingSync, SlidingSyncList, SlidingSyncMode};
use once_cell::sync::Lazy;
use ruma::{
    api::client::sync::sync_events::v4::SyncRequestListFilters, assign, events::StateEventType,
};

use super::Error;

pub const ALL_ROOMS_LIST_NAME: &str = "all_rooms";
pub const VISIBLE_ROOMS_LIST_NAME: &str = "visible_rooms";
pub const INVITES_LIST_NAME: &str = "invites";

/// The state of the [`super::RoomList`]' state machine.
#[derive(Clone, Debug, PartialEq)]
pub enum State {
    /// That's the first initial state.
    Init,

    /// At this state, the first rooms are starting to sync.
    SettingUp,

    /// At this state, the system is recovering from `Error` or `Terminated`.
    /// It's similar to `SettingUp` but some lists may already exist, actions
    /// are then slightly different.
    Recovering,

    /// At this state, all rooms are syncing, and the visible rooms + invites
    /// lists exist.
    Running,

    /// At this state, the sync has been stopped because an error happened.
    Error { from: Box<State> },

    /// At this state, the sync has been stopped because it was requested.
    Terminated { from: Box<State> },
}

impl State {
    /// Transition to the next state, and execute the associated transition's
    /// [`Actions`].
    pub(super) async fn next(&self, sliding_sync: &SlidingSync) -> Result<Self, Error> {
        use State::*;

        let (next_state, actions) = match self {
            Init => (SettingUp, Actions::none()),

            SettingUp => (Running, Actions::setup_lists()),

            Recovering => (Running, Actions::recover_lists()),

            Running => (Running, Actions::none()),

            Error { from: previous_state } | Terminated { from: previous_state } => {
                match previous_state.as_ref() {
                    // Unreachable state.
                    Error { .. } | Terminated { .. } => {
                        unreachable!(
                        "It's impossible to reach `Error` or `Terminated` from `Error` or `Terminated`"
                    );
                    }

                    // If the previous state was `Running`, we enter the `Recovering` state.
                    Running => (Recovering, Actions::reset_lists()),

                    // Jump back to the previous state that led to this termination.
                    state => (state.to_owned(), Actions::none()),
                }
            }
        };

        for action in actions.iter() {
            action.run(sliding_sync).await?;
        }

        Ok(next_state)
    }
}

/// A trait to define what an `Action` is.
#[async_trait]
trait Action {
    async fn run(&self, sliding_sync: &SlidingSync) -> Result<(), Error>;
}

struct AddVisibleRoomsList;

/// Default range for the `VISIBLE_ROOMS_LIST_NAME` list.
pub const VISIBLE_ROOMS_DEFAULT_RANGE: Range = 0..=19;

#[async_trait]
impl Action for AddVisibleRoomsList {
    async fn run(&self, sliding_sync: &SlidingSync) -> Result<(), Error> {
        sliding_sync
            .add_list(super::configure_all_or_visible_rooms_list(
                SlidingSyncList::builder(VISIBLE_ROOMS_LIST_NAME)
                    .sync_mode(
                        SlidingSyncMode::new_selective().add_range(VISIBLE_ROOMS_DEFAULT_RANGE),
                    )
                    .timeline_limit(20)
                    .required_state(vec![
                        (StateEventType::RoomEncryption, "".to_owned()),
                        (StateEventType::RoomMember, "$LAZY".to_owned()),
                    ]),
            ))
            .await
            .map_err(Error::SlidingSync)?;

        Ok(())
    }
}

struct SetAllRoomsListToSelectiveSyncMode;

/// Default `batch_size` for the selective sync-mode of the
/// `ALL_ROOMS_LIST_NAME` list.
pub const ALL_ROOMS_DEFAULT_SELECTIVE_BATCH_SIZE: Range = 0..=19;

#[async_trait]
impl Action for SetAllRoomsListToSelectiveSyncMode {
    async fn run(&self, sliding_sync: &SlidingSync) -> Result<(), Error> {
        sliding_sync
            .on_list(ALL_ROOMS_LIST_NAME, |list| {
                list.set_sync_mode(
                    SlidingSyncMode::new_selective()
                        .add_range(ALL_ROOMS_DEFAULT_SELECTIVE_BATCH_SIZE),
                );

                ready(())
            })
            .await
            .ok_or_else(|| Error::UnknownList(ALL_ROOMS_LIST_NAME.to_owned()))?;

        Ok(())
    }
}

struct SetAllRoomsListToGrowingSyncMode;

/// Default `batch_size` for the growing sync-mode of the `ALL_ROOMS_LIST_NAME`
/// list.
pub const ALL_ROOMS_DEFAULT_GROWING_BATCH_SIZE: u32 = 100;

#[async_trait]
impl Action for SetAllRoomsListToGrowingSyncMode {
    async fn run(&self, sliding_sync: &SlidingSync) -> Result<(), Error> {
        sliding_sync
            .on_list(ALL_ROOMS_LIST_NAME, |list| {
                list.set_sync_mode(SlidingSyncMode::new_growing(
                    ALL_ROOMS_DEFAULT_GROWING_BATCH_SIZE,
                ));

                ready(())
            })
            .await
            .ok_or_else(|| Error::UnknownList(ALL_ROOMS_LIST_NAME.to_owned()))?;

        Ok(())
    }
}

struct AddInvitesList;

/// Default `batch_size` for the growing sync-mode of the `INVITES_LIST_NAME`
/// list.
pub const INVITES_DEFAULT_GROWING_BATCH_SIZE: u32 = 20;

#[async_trait]
impl Action for AddInvitesList {
    async fn run(&self, sliding_sync: &SlidingSync) -> Result<(), Error> {
        sliding_sync
            .add_list(
                SlidingSyncList::builder(INVITES_LIST_NAME)
                    .sync_mode(SlidingSyncMode::new_growing(INVITES_DEFAULT_GROWING_BATCH_SIZE))
                    .timeline_limit(0)
                    .required_state(vec![
                        (StateEventType::RoomAvatar, "".to_owned()),
                        (StateEventType::RoomEncryption, "".to_owned()),
                        (StateEventType::RoomMember, "$ME".to_owned()),
                        (StateEventType::RoomCanonicalAlias, "".to_owned()),
                    ])
                    .filters(Some(assign!(SyncRequestListFilters::default(), {
                        is_invite: Some(true),
                        is_tombstoned: Some(false),
                        not_room_types: vec!["m.space".to_owned()],

                    }))),
            )
            .await
            .map_err(Error::SlidingSync)?;

        Ok(())
    }
}

struct ResetInvitesListGrowingSyncMode;

#[async_trait]
impl Action for ResetInvitesListGrowingSyncMode {
    async fn run(&self, sliding_sync: &SlidingSync) -> Result<(), Error> {
        sliding_sync
            .on_list(INVITES_LIST_NAME, |list| {
                list.set_sync_mode(SlidingSyncMode::new_growing(
                    INVITES_DEFAULT_GROWING_BATCH_SIZE,
                ));

                ready(())
            })
            .await
            .ok_or_else(|| Error::UnknownList(INVITES_LIST_NAME.to_owned()))?;

        Ok(())
    }
}

/// Type alias to represent one action.
type OneAction = Box<dyn Action + Send + Sync>;

/// Type alias to represent many actions.
type ManyActions = Vec<OneAction>;

/// A type to represent multiple actions.
///
/// It contains helper methods to create pre-configured set of actions.
struct Actions {
    actions: &'static Lazy<ManyActions>,
}

macro_rules! actions {
    (
        $(
            $action_group_name:ident => [
                $( $action_name:ident ),* $(,)?
            ]
        ),*
        $(,)?
    ) => {
        $(
            fn $action_group_name () -> Self {
                static ACTIONS: Lazy<ManyActions> = Lazy::new(|| {
                    vec![
                        $( Box::new( $action_name ) ),*
                    ]
                });

                Self { actions: &ACTIONS }
            }
        )*
    };
}

impl Actions {
    actions! {
        none => [],
        setup_lists => [SetAllRoomsListToGrowingSyncMode, AddVisibleRoomsList, AddInvitesList],
        recover_lists => [SetAllRoomsListToGrowingSyncMode],
        reset_lists => [SetAllRoomsListToSelectiveSyncMode, ResetInvitesListGrowingSyncMode],
    }

    fn iter(&self) -> &[OneAction] {
        self.actions.as_slice()
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk_test::async_test;

    use super::{super::tests::new_room_list, *};

    #[async_test]
    async fn test_states() -> Result<(), Error> {
        let room_list = new_room_list().await?;
        let sliding_sync = room_list.sliding_sync();

        // First state.
        let state = State::Init;

        // Hypothetical error.
        {
            let state = State::Error { from: Box::new(state.clone()) }.next(sliding_sync).await?;

            // Back to the previous state.
            assert_eq!(state, State::Init);
        }

        // Hypothetical termination.
        {
            let state =
                State::Terminated { from: Box::new(state.clone()) }.next(sliding_sync).await?;

            // Back to the previous state.
            assert_eq!(state, State::Init);
        }

        // Next state.
        let state = state.next(sliding_sync).await?;
        assert_eq!(state, State::SettingUp);

        // Hypothetical error.
        {
            let state = State::Error { from: Box::new(state.clone()) }.next(sliding_sync).await?;

            // Back to the previous state.
            assert_eq!(state, State::SettingUp);
        }

        // Hypothetical termination.
        {
            let state =
                State::Terminated { from: Box::new(state.clone()) }.next(sliding_sync).await?;

            // Back to the previous state.
            assert_eq!(state, State::SettingUp);
        }

        // Next state.
        let state = state.next(sliding_sync).await?;
        assert_eq!(state, State::Running);

        // Hypothetical error.
        {
            let state = State::Error { from: Box::new(state.clone()) }.next(sliding_sync).await?;

            // Jump to the **recovering** state!
            assert_eq!(state, State::Recovering);

            let state = state.next(sliding_sync).await?;

            // Now, back to the previous state.
            assert_eq!(state, State::Running);
        }

        // Hypothetical termination.
        {
            let state =
                State::Terminated { from: Box::new(state.clone()) }.next(sliding_sync).await?;

            // Jump to the **recovering** state!
            assert_eq!(state, State::Recovering);

            let state = state.next(sliding_sync).await?;

            // Now, back to the previous state.
            assert_eq!(state, State::Running);
        }

        // Hypothetical error when recovering.
        {
            let state =
                State::Error { from: Box::new(State::Recovering) }.next(sliding_sync).await?;

            // Back to the previous state.
            assert_eq!(state, State::Recovering);
        }

        // Hypothetical termination when recovering.
        {
            let state =
                State::Terminated { from: Box::new(State::Recovering) }.next(sliding_sync).await?;

            // Back to the previous state.
            assert_eq!(state, State::Recovering);
        }

        Ok(())
    }

    #[async_test]
    async fn test_action_add_visible_rooms_list() -> Result<(), Error> {
        let room_list = new_room_list().await?;
        let sliding_sync = room_list.sliding_sync();

        // List is absent.
        assert_eq!(sliding_sync.on_list(VISIBLE_ROOMS_LIST_NAME, |_list| ready(())).await, None);

        // Run the action!
        AddVisibleRoomsList.run(sliding_sync).await?;

        // List is present!
        assert_eq!(
            sliding_sync
                .on_list(VISIBLE_ROOMS_LIST_NAME, |list| ready(matches!(
                    list.sync_mode(),
                    SlidingSyncMode::Selective { ranges } if ranges == vec![VISIBLE_ROOMS_DEFAULT_RANGE]
                )))
                .await,
            Some(true)
        );

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
                    SlidingSyncMode::Selective { ranges } if ranges == vec![ALL_ROOMS_DEFAULT_SELECTIVE_BATCH_SIZE]
                )))
                .await,
            Some(true)
        );

        // Run the action!
        SetAllRoomsListToGrowingSyncMode.run(sliding_sync).await.unwrap();

        // List is still present, in Growing mode.
        assert_eq!(
            sliding_sync
                .on_list(ALL_ROOMS_LIST_NAME, |list| ready(matches!(
                    list.sync_mode(),
                    SlidingSyncMode::Growing { batch_size: 100, .. }
                )))
                .await,
            Some(true)
        );

        // Run the other action!
        SetAllRoomsListToSelectiveSyncMode.run(sliding_sync).await.unwrap();

        // List is still present, in Selective mode.
        assert_eq!(
            sliding_sync
                .on_list(ALL_ROOMS_LIST_NAME, |list| ready(matches!(
                    list.sync_mode(),
                    SlidingSyncMode::Selective { ranges } if ranges == vec![ALL_ROOMS_DEFAULT_SELECTIVE_BATCH_SIZE]
                )))
                .await,
            Some(true)
        );

        Ok(())
    }

    #[async_test]
    async fn test_action_add_invitess_list() -> Result<(), Error> {
        let room_list = new_room_list().await?;
        let sliding_sync = room_list.sliding_sync();

        // List is absent.
        assert_eq!(sliding_sync.on_list(INVITES_LIST_NAME, |_list| ready(())).await, None);

        // Run the action!
        AddInvitesList.run(sliding_sync).await?;

        // List is present!
        assert_eq!(
            sliding_sync
                .on_list(INVITES_LIST_NAME, |list| ready(matches!(
                    list.sync_mode(),
                    SlidingSyncMode::Growing { batch_size, .. } if batch_size == 20
                )))
                .await,
            Some(true)
        );

        Ok(())
    }

    #[async_test]
    async fn test_action_reset_invites_list_growing_sync_mode() -> Result<(), Error> {
        let room_list = new_room_list().await?;
        let sliding_sync = room_list.sliding_sync();

        // List is absent.
        assert_eq!(sliding_sync.on_list(INVITES_LIST_NAME, |_list| ready(())).await, None);

        // Run the action!
        AddInvitesList.run(sliding_sync).await?;

        // Change the growing sync-mode.
        sliding_sync
            .on_list(INVITES_LIST_NAME, |list| {
                list.set_sync_mode(SlidingSyncMode::new_growing(42));

                ready(())
            })
            .await
            .unwrap();

        assert_eq!(
            sliding_sync
                .on_list(INVITES_LIST_NAME, |list| ready(matches!(
                    list.sync_mode(),
                    SlidingSyncMode::Growing { batch_size, .. } if batch_size == 42
                )))
                .await,
            Some(true)
        );

        // Run the action!
        ResetInvitesListGrowingSyncMode.run(sliding_sync).await?;

        // List is still present, and reset.
        assert_eq!(
            sliding_sync
                .on_list(INVITES_LIST_NAME, |list| ready(matches!(
                    list.sync_mode(),
                    SlidingSyncMode::Growing { batch_size, .. } if batch_size == 20
                )))
                .await,
            Some(true)
        );

        Ok(())
    }
}
