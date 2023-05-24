//! `RoomList` API.

use async_stream::stream;
use async_trait::async_trait;
use futures_util::{pin_mut, Stream, StreamExt};
use matrix_sdk::{
    Client, Error, Result, SlidingSync, SlidingSyncList, SlidingSyncMode, UpdateSummary,
};
use once_cell::sync::Lazy;

pub const ALL_ROOMS_LIST_NAME: &str = "all_rooms";
pub const VISIBLE_ROOMS_LIST_NAME: &str = "visible_rooms";

#[derive(Debug)]
pub struct RoomList {
    sliding_sync: SlidingSync,
}

impl RoomList {
    pub async fn new(client: Client) -> Result<Self> {
        Ok(Self {
            sliding_sync: client
                .sliding_sync()
                .storage_key(Some("matrix-sdk-ui-roomlist".to_string()))
                .add_cached_list(
                    SlidingSyncList::builder(ALL_ROOMS_LIST_NAME)
                        .sync_mode(SlidingSyncMode::new_selective()),
                )
                .await?
                .build()
                .await?,
        })
    }

    pub fn sync(&self) -> impl Stream<Item = Result<(), Error>> + '_ {
        stream! {
            let sync = self.sliding_sync.sync();
            pin_mut!(sync);

            let mut state = State::LoadFirstRooms;

            while let Some(update_summary) = sync.next().await {
                state = state.next(update_summary, &self.sliding_sync).await?;

                // if let State::Failure = state {
                //     yield Err(());
                //     // transform into a custom `MyError::SyncFailure`
                // } else {
                    yield Ok(());
                // }
            }
        }
    }

    #[cfg(feature = "testing")]
    pub fn sliding_sync(&self) -> &SlidingSync {
        &self.sliding_sync
    }
}

enum State {
    LoadFirstRooms,
    LoadAllRooms,
    Enjoy,
    Failure,
}

impl State {
    async fn next(
        self,
        update_summary: Result<UpdateSummary, Error>,
        sliding_sync: &SlidingSync,
    ) -> Result<Self, Error> {
        use State::*;

        if update_summary.is_err() {
            return Ok(Failure);
        }

        let (next_state, actions) = match self {
            LoadFirstRooms => (LoadAllRooms, Actions::first_rooms_are_loaded()),
            LoadAllRooms => (Enjoy, Actions::none()),
            Enjoy => (Enjoy, Actions::none()),
            Failure => (Failure, Actions::none()),
        };

        for action in actions.iter() {
            action.run(sliding_sync).await?;
        }

        Ok(next_state)
    }
}

#[async_trait]
trait Action {
    async fn run(&self, sliding_sync: &SlidingSync) -> Result<(), Error>;
}

struct AddVisibleRoomsList;

#[async_trait]
impl Action for AddVisibleRoomsList {
    async fn run(&self, sliding_sync: &SlidingSync) -> Result<(), Error> {
        sliding_sync
            .add_list(
                SlidingSyncList::builder(VISIBLE_ROOMS_LIST_NAME)
                    .sync_mode(SlidingSyncMode::new_selective()),
            )
            .await?;

        Ok(())
    }
}

struct ChangeAllRoomsListToSelectiveSyncMode;

#[async_trait]
impl Action for ChangeAllRoomsListToSelectiveSyncMode {
    async fn run(&self, sliding_sync: &SlidingSync) -> Result<(), Error> {
        sliding_sync
            .on_list("all_rooms", |list| list.set_sync_mode(SlidingSyncMode::new_growing(20, None)))
            .unwrap() // transform into a custom `MyError::UnknownList`
            ?;

        Ok(())
    }
}

type OneAction = Box<dyn Action + Send + Sync>;
type ManyActions = Vec<OneAction>;

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
        first_rooms_are_loaded => [ChangeAllRoomsListToSelectiveSyncMode, AddVisibleRoomsList],
    }

    fn iter(&self) -> &[OneAction] {
        self.actions.as_slice()
    }
}
