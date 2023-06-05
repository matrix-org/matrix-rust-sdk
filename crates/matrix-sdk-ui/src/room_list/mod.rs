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

//! `RoomList` API.
//!
//! The `RoomList` is a UI API dedicated to present a list of Matrix rooms to
//! the user. The syncing is handled by
//! [`SlidingSync`][matrix_sdk::SlidingSync]. The idea is to expose a simple API
//! to handle most of the client app use cases, like: Showing and updating a
//! list of rooms, filtering a list of rooms, handling particular updates of a
//! range of rooms (the ones the client app is showing to the view, i.e. the
//! rooms present in the viewport) etc.
//!
//! As such, the `RoomList` works as an opinionated state machine. The states
//! are defined by [`State`]. Actions are attached to the each state transition.
//! Apart from that, one can apply [`Input`]s on the state machine, like
//! notifying that the client app viewport of the room list has changed (if the
//! user of the client app has scrolled in the room list for example) etc.
//!
//! The API is purposely small. Sliding Sync is versatile. `RoomList` is _one_
//! specific usage of Sliding Sync.
//!
//! # Basic principle
//!
//! `RoomList` works with 2 Sliding Sync List:
//!
//! * `all_rooms` (referred by the constant [`ALL_ROOMS_LIST_NAME`]) is the main
//!   list. Its goal is to load all the user' rooms. It starts with a
//!   [`SlidingSyncMode::Selective`] sync-mode with a small range (i.e. a small
//!   set of rooms) to load the first rooms quickly, and then updates to a
//!   [`SlidingSyncMode::Growing`] sync-mode to load the remaining rooms “in the
//!   background”: it will sync the existing rooms and will fetch new rooms, by
//!   a certain batch size.
//! * `visible_rooms` (referred by the constant [`VISIBLE_ROOMS_LIST_NAME`]) is
//!   the “reactive” list. It's goal is to react to the client app user actions.
//!   If the user scrolls in the room list, the `visible_rooms` will be
//!   configured to sync for the particular range of rooms the user is actually
//!   seeing (the rooms in the current viewport). `visible_rooms` has a
//!   different configuration than `all_rooms` as it loads more timeline events:
//!   it means that the room will already have a “history”, a timeline, ready to
//!   be presented when the user enters the room.
//!
//! This behavior has proven to be empirically satisfying to provide a fast and
//! fluid user experience for a Matrix client.
//!
//! [`RoomList::entries`] provides a way to get a stream of room list entry.
//! This stream can be filtered, and the filter can be changed over time.
//!
//! [`RoomList::state_stream`] provides a way to get a stream of the state
//! machine's state, which can be pretty helpful for the client app.

use std::{future::ready, sync::Arc};

use async_stream::stream;
use async_trait::async_trait;
use eyeball::shared::Observable;
use eyeball_im::VectorDiff;
use futures_util::{pin_mut, Stream, StreamExt};
use imbl::Vector;
pub use matrix_sdk::RoomListEntry;
use matrix_sdk::{
    sliding_sync::Ranges, Client, Error as SlidingSyncError, SlidingSync, SlidingSyncList,
    SlidingSyncMode, SlidingSyncRoom,
};
use once_cell::sync::Lazy;
use ruma::{OwnedRoomId, RoomId};
use thiserror::Error;

use crate::{timeline::EventTimelineItem, Timeline};

pub const ALL_ROOMS_LIST_NAME: &str = "all_rooms";
pub const VISIBLE_ROOMS_LIST_NAME: &str = "visible_rooms";

/// The [`RoomList`] type. See the module's documentation to learn more.
#[derive(Debug)]
pub struct RoomList {
    sliding_sync: SlidingSync,
    state: Observable<State>,
}

impl RoomList {
    /// Create a new `RoomList`.
    ///
    /// A [`matrix_sdk::SlidingSync`] client will be created, with a cached list
    /// already pre-configured.
    pub async fn new(client: Client) -> Result<Self, Error> {
        let sliding_sync = client
            .sliding_sync("room-list")
            .map_err(Error::SlidingSync)?
            .enable_caching()
            .map_err(Error::SlidingSync)?
            .add_cached_list(
                SlidingSyncList::builder(ALL_ROOMS_LIST_NAME)
                    .sync_mode(SlidingSyncMode::new_selective().add_range(0..=19))
                    .timeline_limit(1),
            )
            .await
            .map_err(Error::SlidingSync)?
            .build()
            .await
            .map_err(Error::SlidingSync)?;

        Ok(Self { sliding_sync, state: Observable::new(State::Init) })
    }

    /// Start to sync the room list.
    ///
    /// It's the main method of this entire API. Calling `sync` allows to
    /// receive updates on the room list: new rooms, rooms updates etc. Those
    /// updates can be read with [`Self::entries`]. This method returns a
    /// [`Stream`] where produced items only hold an empty value in case of a
    /// sync success, otherwise an error.
    ///
    /// The `RoomList`' state machine is run by this method.
    ///
    /// Stopping the [`Stream`] (i.e. stop polling it) and calling
    /// [`Self::sync`] again will resume from the previous state of the state
    /// machine.
    pub fn sync(&self) -> impl Stream<Item = Result<(), Error>> + '_ {
        stream! {
            let sync = self.sliding_sync.sync();
            pin_mut!(sync);

            // This is a state machine implementation.
            // Things happen in this order:
            //
            // 1. The next state is calculated,
            // 2. The actions associated to the next state are run,
            // 3. The next state is stored,
            // 4. A sync is done.
            //
            // So the sync is done after the machine _has entered_ into a new state.
            loop {
                {
                    let next_state = self.state.read().next(&self.sliding_sync).await?;

                    Observable::set(&self.state, next_state);
                }

                match sync.next().await {
                    Some(Ok(_update_summary)) => {
                        yield Ok(());
                    }

                    Some(Err(error)) => {
                        let next_state = State::Terminated { from: Box::new(self.state.get()) };

                        Observable::set(&self.state, next_state);

                        yield Err(Error::SlidingSync(error));

                        break;
                    }

                    None => {
                        let next_state = State::Terminated { from: Box::new(self.state.get()) };

                        Observable::set(&self.state, next_state);

                        break;
                    }
                }
            }
        }
    }

    /// Get the current state of the state machine.
    pub fn state(&self) -> State {
        self.state.get()
    }

    /// Get a [`Stream`] of [`State`]s.
    pub fn state_stream(&self) -> impl Stream<Item = State> {
        Observable::subscribe(&self.state)
    }

    /// Get all previous room list entries, in addition to a [`Stream`] to room
    /// list entry's updates.
    pub async fn entries(
        &self,
    ) -> Result<(Vector<RoomListEntry>, impl Stream<Item = VectorDiff<RoomListEntry>>), Error> {
        self.sliding_sync
            .on_list(ALL_ROOMS_LIST_NAME, |list| ready(list.room_list_stream()))
            .await
            .ok_or_else(|| Error::UnknownList(ALL_ROOMS_LIST_NAME.to_string()))
    }

    /// Similar to [`Self::entries`] except that it's possible to provide a
    /// filter that will filter out room list entries.
    pub async fn entries_filtered<F>(
        &self,
        filter: F,
    ) -> Result<(Vector<RoomListEntry>, impl Stream<Item = VectorDiff<RoomListEntry>>), Error>
    where
        F: Fn(&RoomListEntry) -> bool + Send + Sync + 'static,
    {
        self.sliding_sync
            .on_list(ALL_ROOMS_LIST_NAME, |list| ready(list.room_list_filtered_stream(filter)))
            .await
            .ok_or_else(|| Error::UnknownList(ALL_ROOMS_LIST_NAME.to_string()))
    }

    /// Pass an [`Input`] onto the state machine.
    pub async fn apply_input(&self, input: Input) -> Result<(), Error> {
        use Input::*;

        match input {
            Viewport(ranges) => {
                self.update_viewport(ranges).await?;
            }
        }

        Ok(())
    }

    async fn update_viewport(&self, ranges: Ranges) -> Result<(), Error> {
        self.sliding_sync
            .on_list(VISIBLE_ROOMS_LIST_NAME, |list| {
                list.set_sync_mode(SlidingSyncMode::new_selective().add_ranges(ranges.clone()));

                ready(())
            })
            .await
            .ok_or_else(|| Error::InputHasNotBeenApplied(Input::Viewport(ranges)))?;

        Ok(())
    }

    /// Get a [`Room`] if it exists.
    pub async fn room(&self, room_id: &RoomId) -> Result<Room, Error> {
        match self.sliding_sync.get_room(room_id).await {
            Some(room) => Room::new(room).await,
            None => Err(Error::RoomNotFound(room_id.to_owned())),
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn sliding_sync(&self) -> &SlidingSync {
        &self.sliding_sync
    }
}

/// A room in the room list.
///
/// It's cheap to clone this type.
#[derive(Clone, Debug)]
pub struct Room {
    inner: Arc<RoomInner>,
}

#[derive(Debug)]
struct RoomInner {
    /// The Sliding Sync room.
    sliding_sync_room: SlidingSyncRoom,

    /// The underlying client room.
    room: matrix_sdk::room::Room,

    /// The timeline of the room.
    timeline: Timeline,

    /// The “sneaky” timeline of the room, i.e. this timeline doesn't track the
    /// read marker nor the receipts.
    sneaky_timeline: Timeline,
}

impl Room {
    /// Create a new `Room`.
    async fn new(sliding_sync_room: SlidingSyncRoom) -> Result<Self, Error> {
        let room = sliding_sync_room
            .client()
            .get_room(sliding_sync_room.room_id())
            .ok_or_else(|| Error::RoomNotFound(sliding_sync_room.room_id().to_owned()))?;

        let timeline = Timeline::builder(&room)
            .events(sliding_sync_room.prev_batch(), sliding_sync_room.timeline_queue())
            .track_read_marker_and_receipts()
            .build()
            .await;

        let sneaky_timeline = Timeline::builder(&room)
            .events(sliding_sync_room.prev_batch(), sliding_sync_room.timeline_queue())
            .build()
            .await;

        Ok(Self {
            inner: Arc::new(RoomInner { sliding_sync_room, room, timeline, sneaky_timeline }),
        })
    }

    /// Get the best possible name for the room.
    ///
    /// If the sliding sync room has received a name from the server, then use
    /// it, otherwise, let's calculate a name.
    pub async fn name(&self) -> Option<String> {
        Some(match self.inner.sliding_sync_room.name() {
            Some(name) => name,
            None => self.inner.room.display_name().await.ok()?.to_string(),
        })
    }

    /// Get the timeline of the room.
    pub fn timeline(&self) -> &Timeline {
        &self.inner.timeline
    }

    /// Get the latest event of the timeline.
    ///
    /// It's different from `Self::timeline().latest_event()` as it won't track
    /// the read marker and receipts.
    pub async fn latest_event(&self) -> Option<EventTimelineItem> {
        self.inner.sneaky_timeline.latest_event().await
    }
}

/// [`RoomList`]'s errors.
#[derive(Debug, Error)]
pub enum Error {
    /// Error from [`matrix_sdk::SlidingSync`].
    #[error("SlidingSync failed")]
    SlidingSync(SlidingSyncError),

    /// An operation has been requested on an unknown list.
    #[error("Unknown list `{0}`")]
    UnknownList(String),

    /// An input was asked to be applied but it wasn't possible to apply it.
    #[error("The input has been not applied")]
    InputHasNotBeenApplied(Input),

    /// The requested room doesn't exist.
    #[error("Room `{0}` not found")]
    RoomNotFound(OwnedRoomId),
}

/// The state of the [`RoomList`]' state machine.
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
    Enjoy,

    /// At this state, the sync has been stopped (because it was requested, or
    /// because it has errored too many times previously).
    Terminated { from: Box<State> },
}

impl State {
    /// Transition to the next state, and execute the associated transition's
    /// [`Actions`].
    async fn next(&self, sliding_sync: &SlidingSync) -> Result<Self, Error> {
        use State::*;

        let (next_state, actions) = match self {
            Init => (FirstRooms, Actions::none()),
            FirstRooms => (AllRooms, Actions::first_rooms_are_loaded()),
            AllRooms => (Enjoy, Actions::none()),
            Enjoy => (Enjoy, Actions::none()),
            // If the state was `Terminated` but the next state is calculated again, it means the
            // sync has been restarted. In this case, let's jump back on the previous state that led
            // to the termination. No action is required in this scenario.
            Terminated { from: previous_state } => {
                match previous_state.as_ref() {
                    state @ Init | state @ FirstRooms => {
                        // Do nothing.
                        (state.to_owned(), Actions::none())
                    }

                    state @ AllRooms | state @ Enjoy => {
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

#[async_trait]
impl Action for AddVisibleRoomsList {
    async fn run(&self, sliding_sync: &SlidingSync) -> Result<(), Error> {
        sliding_sync
            .add_list(
                SlidingSyncList::builder(VISIBLE_ROOMS_LIST_NAME)
                    .sync_mode(SlidingSyncMode::new_selective())
                    .timeline_limit(20),
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
            .ok_or_else(|| Error::UnknownList(ALL_ROOMS_LIST_NAME.to_string()))?;

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
        first_rooms_are_loaded => [SetAllRoomsListToGrowingSyncMode, AddVisibleRoomsList],
        refresh_lists => [SetAllRoomsListToGrowingSyncMode],
    }

    fn iter(&self) -> &[OneAction] {
        self.actions.as_slice()
    }
}

/// An input for the [`RoomList`]' state machine.
///
/// An input is something that has happened or is happening or is requested by
/// the client app using this [`RoomList`].
#[derive(Debug)]
pub enum Input {
    /// The client app's viewport of the room list has changed.
    ///
    /// Use this input when the user of the client app is scrolling inside the
    /// room list, and the viewport has changed. The viewport is defined as the
    /// range of visible rooms in the room list.
    Viewport(Ranges),
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
        assert_eq!(state, State::Enjoy);

        // Hypothetical termination.
        {
            let state =
                State::Terminated { from: Box::new(state.clone()) }.next(sliding_sync).await?;
            assert_eq!(state, State::Enjoy);
        }

        // Next state.
        let state = state.next(sliding_sync).await?;
        assert_eq!(state, State::Enjoy);

        // Hypothetical termination.
        {
            let state =
                State::Terminated { from: Box::new(state.clone()) }.next(sliding_sync).await?;
            assert_eq!(state, State::Enjoy);
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
                    SlidingSyncMode::Selective { ranges } if ranges.is_empty()
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
}
