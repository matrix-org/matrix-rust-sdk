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

    /// At this state, the first rooms start to be synced.
    FirstRooms,

    /// At this state, all rooms start to be synced.
    AllRooms,

    /// This state is the cruising speed, i.e. the “normal” state, where nothing
    /// fancy happens: all rooms are syncing, and life is great.
    CarryOn,

    /// At this state, the sync has been stopped (because it was requested, or
    /// because it has errored too many times previously).
    Terminated { from: Box<State> },
}

impl State {
    /// Transition to the next state, and execute the associated transition's
    /// [`Actions`].
    pub(super) async fn next(&self, sliding_sync: &SlidingSync) -> Result<Self, Error> {
        use State::*;

        let (next_state, actions) = match self {
            Init => (FirstRooms, Actions::none()),
            FirstRooms => (AllRooms, Actions::first_rooms_are_loaded()),
            AllRooms => (CarryOn, Actions::none()),
            CarryOn => (CarryOn, Actions::none()),
            // If the state was `Terminated` but the next state is calculated again, it means the
            // sync has been restarted. In this case, let's jump back on the previous state that led
            // to the termination. No action is required in this scenario.
            Terminated { from: previous_state } => {
                match previous_state.as_ref() {
                    state @ Init | state @ FirstRooms => {
                        // Do nothing.
                        (state.to_owned(), Actions::none())
                    }

                    state @ AllRooms | state @ CarryOn => {
                        // Refresh the lists.
                        (state.to_owned(), Actions::refresh_lists())
                    }

                    Terminated { .. } => {
                        // Having `Terminated { from: Terminated { … } }` is not allowed.
                        unreachable!("It's impossible to reach `Terminated` from `Terminated`");
                    }
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
            .add_list(
                SlidingSyncList::builder(VISIBLE_ROOMS_LIST_NAME)
                    .sync_mode(
                        SlidingSyncMode::new_selective().add_range(VISIBLE_ROOMS_DEFAULT_RANGE),
                    )
                    .timeline_limit(20)
                    .required_state(vec![(StateEventType::RoomEncryption, "".to_owned())])
                    .filters(Some(assign!(SyncRequestListFilters::default(), {
                        is_invite: Some(false),
                        is_tombstoned: Some(false),
                        not_room_types: vec!["m.space".to_owned()],
                    }))),
            )
            .await
            .map_err(Error::SlidingSync)?;

        Ok(())
    }
}

struct SetAllRoomsListToGrowingSyncMode;

#[async_trait]
impl Action for SetAllRoomsListToGrowingSyncMode {
    async fn run(&self, sliding_sync: &SlidingSync) -> Result<(), Error> {
        sliding_sync
            .on_list(ALL_ROOMS_LIST_NAME, |list| {
                list.set_sync_mode(SlidingSyncMode::new_growing(50));

                ready(())
            })
            .await
            .ok_or_else(|| Error::UnknownList(ALL_ROOMS_LIST_NAME.to_owned()))?;

        Ok(())
    }
}

struct AddInvitesList;

#[async_trait]
impl Action for AddInvitesList {
    async fn run(&self, sliding_sync: &SlidingSync) -> Result<(), Error> {
        sliding_sync
            .add_list(
                SlidingSyncList::builder(INVITES_LIST_NAME)
                    .sync_mode(SlidingSyncMode::new_growing(100))
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
        first_rooms_are_loaded => [SetAllRoomsListToGrowingSyncMode, AddVisibleRoomsList, AddInvitesList],
        refresh_lists => [SetAllRoomsListToGrowingSyncMode],
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

        // Hypothetical termination.
        {
            let state =
                State::Terminated { from: Box::new(state.clone()) }.next(sliding_sync).await?;
            assert_eq!(state, State::Init);
        }

        // Next state.
        let state = state.next(sliding_sync).await?;
        assert_eq!(state, State::FirstRooms);

        // Hypothetical termination.
        {
            let state =
                State::Terminated { from: Box::new(state.clone()) }.next(sliding_sync).await?;
            assert_eq!(state, State::FirstRooms);
        }

        // Next state.
        let state = state.next(sliding_sync).await?;
        assert_eq!(state, State::AllRooms);

        // Hypothetical termination.
        {
            let state =
                State::Terminated { from: Box::new(state.clone()) }.next(sliding_sync).await?;
            assert_eq!(state, State::AllRooms);
        }

        // Next state.
        let state = state.next(sliding_sync).await?;
        assert_eq!(state, State::CarryOn);

        // Hypothetical termination.
        {
            let state =
                State::Terminated { from: Box::new(state.clone()) }.next(sliding_sync).await?;
            assert_eq!(state, State::CarryOn);
        }

        // Next state.
        let state = state.next(sliding_sync).await?;
        assert_eq!(state, State::CarryOn);

        // Hypothetical termination.
        {
            let state =
                State::Terminated { from: Box::new(state.clone()) }.next(sliding_sync).await?;
            assert_eq!(state, State::CarryOn);
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
    async fn test_action_set_all_rooms_list_to_growing_sync_mode() -> Result<(), Error> {
        let room_list = new_room_list().await?;
        let sliding_sync = room_list.sliding_sync();

        // List is present, in Selective mode.
        assert_eq!(
            sliding_sync
                .on_list(ALL_ROOMS_LIST_NAME, |list| ready(matches!(
                    list.sync_mode(),
                    SlidingSyncMode::Selective { ranges } if ranges == vec![0..=19]
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
                    SlidingSyncMode::Growing { batch_size: 50, .. }
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
                    SlidingSyncMode::Growing { batch_size, .. } if batch_size == 100
                )))
                .await,
            Some(true)
        );

        Ok(())
    }
}
