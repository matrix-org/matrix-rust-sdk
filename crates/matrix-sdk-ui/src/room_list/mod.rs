//! `RoomList` API.

use std::{
    fmt,
    future::ready,
    sync::{Arc, Mutex},
};

use async_stream::stream;
use async_trait::async_trait;
use eyeball::shared::Observable;
use eyeball_im_util::FilteredVectorSubscriber;
use futures_util::{pin_mut, Stream, StreamExt};
use matrix_sdk::{
    Client, Error as SlidingSyncError, RoomListEntry, SlidingSync, SlidingSyncList, SlidingSyncMode,
};
use once_cell::sync::Lazy;
use thiserror::Error;

pub const ALL_ROOMS_LIST_NAME: &str = "all_rooms";
pub const VISIBLE_ROOMS_LIST_NAME: &str = "visible_rooms";

pub struct RoomList {
    sliding_sync: SlidingSync,
    entries_stream:
        FilteredVectorSubscriber<RoomListEntry, Box<dyn Fn(&RoomListEntry) -> bool + Sync + Send>>,
    state: Observable<State>,
}

impl fmt::Debug for RoomList {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RoomList")
            .field("sliding_sync", &self.sliding_sync)
            .field("state", &self.state)
            .finish_non_exhaustive()
    }
}

impl RoomList {
    pub async fn new(client: Client) -> Result<Self, Error> {
        let entries = Arc::new(Mutex::new(None));
        let sliding_sync = client
            .sliding_sync()
            .storage_key(Some("matrix-sdk-ui-roomlist".to_string()))
            .add_cached_list(
                SlidingSyncList::builder(ALL_ROOMS_LIST_NAME)
                    .sync_mode(SlidingSyncMode::new_selective().add_range(0..=19))
                    .once_built({
                        let entries = entries.clone();

                        move |list| {
                            *entries.lock().unwrap() =
                                Some(list.room_list_filtered_stream(Box::new(|_| true)));

                            list
                        }
                    }),
            )
            .await
            .map_err(Error::SlidingSync)?
            .build()
            .await
            .map_err(Error::SlidingSync)?;

        let entries =
            Arc::try_unwrap(entries).map_err(|_| Error::Entries)?.into_inner().unwrap().unwrap();

        Ok(Self { sliding_sync, entries_stream: entries, state: Observable::new(State::Init) })
    }

    pub fn sync(&self) -> impl Stream<Item = Result<(), Error>> + '_ {
        stream! {
            let sync = self.sliding_sync.sync();
            pin_mut!(sync);

            // Before doing the first sync, let's transition to the next state.
            {
                let next_state = self.state.read().next(&self.sliding_sync).await?;

                Observable::set(&self.state, next_state);
            }

            while let Some(update_summary) = sync.next().await {
                match update_summary {
                    Ok(_update_summary) => {
                        {
                            let next_state = self.state.read().next(&self.sliding_sync).await?;

                            Observable::set(&self.state, next_state);
                        }

                        yield Ok(());
                    }

                    Err(error) => {
                        yield Err(Error::SlidingSync(error));
                    }
                }

            }

            let next_state = State::Terminated;

            Observable::set(&self.state, next_state);
        }
    }

    pub fn state(&self) -> State {
        self.state.get()
    }

    pub fn state_stream(&self) -> impl Stream<Item = State> {
        Observable::subscribe(&self.state)
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn sliding_sync(&self) -> &SlidingSync {
        &self.sliding_sync
    }
}

#[derive(Debug, Error)]
pub enum Error {
    /// Error from [`matrix_sdk::SlidingSync`].
    #[error("SlidingSync failed")]
    SlidingSync(SlidingSyncError),

    /// An operation has been requested on an unknown list.
    #[error("Unknown list `{0}`")]
    UnknownList(String),

    #[error("foo")]
    Entries,
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum State {
    /// That's the first initial state. The next state is calculated before
    /// doing the first sync.
    Init,

    /// At this state, the first rooms starts to be synced.
    LoadFirstRooms,

    /// At this state, all rooms starts to be synced.
    LoadAllRooms,

    /// This state is the cruising speed, i.e. the “normal” state, where nothing
    /// fancy happens: all rooms are syncing, and life is great.
    Enjoy,

    /// At this state, the sync has been stopped (because it was requested, or
    /// because it has errored too many times previously).
    Terminated,
}

impl State {
    async fn next(&self, sliding_sync: &SlidingSync) -> Result<Self, Error> {
        use State::*;

        let (next_state, actions) = match self {
            Init => (LoadFirstRooms, Actions::none()),
            LoadFirstRooms => (LoadAllRooms, Actions::first_rooms_are_loaded()),
            LoadAllRooms => (Enjoy, Actions::none()),
            Enjoy => (Enjoy, Actions::none()),
            Terminated => (Terminated, Actions::none()),
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
            .await
            .map_err(Error::SlidingSync)?;

        Ok(())
    }
}

struct ChangeAllRoomsListToGrowingSyncMode;

#[async_trait]
impl Action for ChangeAllRoomsListToGrowingSyncMode {
    async fn run(&self, sliding_sync: &SlidingSync) -> Result<(), Error> {
        sliding_sync
            .on_list(ALL_ROOMS_LIST_NAME, |list| {
                ready(list.set_sync_mode(SlidingSyncMode::new_growing(50)))
            })
            .await
            .ok_or_else(|| Error::UnknownList(ALL_ROOMS_LIST_NAME.to_string()))?;

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
        first_rooms_are_loaded => [ChangeAllRoomsListToGrowingSyncMode, AddVisibleRoomsList],
    }

    fn iter(&self) -> &[OneAction] {
        self.actions.as_slice()
    }
}

#[cfg(test)]
mod tests {
    use matrix_sdk::{config::RequestConfig, Session};
    use matrix_sdk_test::async_test;
    use ruma::{api::MatrixVersion, device_id, user_id};
    use wiremock::MockServer;

    use super::*;

    async fn new_client() -> (Client, MockServer) {
        let session = Session {
            access_token: "1234".to_owned(),
            refresh_token: None,
            user_id: user_id!("@example:localhost").to_owned(),
            device_id: device_id!("DEVICEID").to_owned(),
        };

        let server = MockServer::start().await;
        let client = Client::builder()
            .homeserver_url(server.uri())
            .server_versions([MatrixVersion::V1_0])
            .request_config(RequestConfig::new().disable_retry())
            .build()
            .await
            .unwrap();
        client.restore_session(session).await.unwrap();

        (client, server)
    }

    async fn new_room_list() -> Result<RoomList, Error> {
        let (client, _) = new_client().await;

        RoomList::new(client).await
    }

    #[async_test]
    async fn test_all_rooms_are_declared() -> Result<(), Error> {
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

        Ok(())
    }

    #[async_test]
    async fn test_states() -> Result<(), Error> {
        let room_list = new_room_list().await?;
        let sliding_sync = room_list.sliding_sync();

        let state = State::Init;

        let state = state.next(&sliding_sync).await?;
        assert_eq!(state, State::LoadFirstRooms);

        let state = state.next(&sliding_sync).await?;
        assert_eq!(state, State::LoadAllRooms);

        let state = state.next(&sliding_sync).await?;
        assert_eq!(state, State::Enjoy);

        let state = state.next(&sliding_sync).await?;
        assert_eq!(state, State::Enjoy);

        let state = State::Terminated;

        let state = state.next(&sliding_sync).await?;
        assert_eq!(state, State::Terminated);

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
                    SlidingSyncMode::Selective { ranges } if ranges.is_empty()
                )))
                .await,
            Some(true)
        );

        Ok(())
    }

    #[async_test]
    async fn test_action_change_all_rooms_list_to_growing_sync_mode() -> Result<(), Error> {
        let room_list = new_room_list().await?;
        let sliding_sync = room_list.sliding_sync();

        // List is absent.
        assert_eq!(sliding_sync.on_list(VISIBLE_ROOMS_LIST_NAME, |_list| ready(())).await, None);

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
        ChangeAllRoomsListToGrowingSyncMode.run(sliding_sync).await.unwrap();

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
}
