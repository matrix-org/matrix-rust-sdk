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
//!   the “reactive” list. Its goal is to react to the client app user actions.
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
//! [`RoomList::state`] provides a way to get a stream of the state machine's
//! state, which can be pretty helpful for the client app.

mod room;
mod state;

use std::{future::ready, sync::Arc};

use async_stream::stream;
use eyeball::{shared::Observable, Subscriber};
use eyeball_im::VectorDiff;
use futures_util::{pin_mut, Stream, StreamExt};
use imbl::Vector;
pub use matrix_sdk::RoomListEntry;
use matrix_sdk::{
    sliding_sync::Ranges, Client, Error as SlidingSyncError, SlidingSync, SlidingSyncList,
    SlidingSyncListLoadingState, SlidingSyncMode,
};
pub use room::*;
use ruma::{
    api::client::sync::sync_events::v4::{E2EEConfig, SyncRequestListFilters, ToDeviceConfig},
    assign,
    events::{StateEventType, TimelineEventType},
    OwnedRoomId, RoomId,
};
pub use state::*;
use thiserror::Error;

/// The [`RoomList`] type. See the module's documentation to learn more.
#[derive(Debug)]
pub struct RoomList {
    sliding_sync: Arc<SlidingSync>,
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
            .with_common_extensions()
            // TODO different strategy when the encryption sync is in main by default
            .with_e2ee_extension(assign!(E2EEConfig::default(), { enabled: Some(true) }))
            .with_to_device_extension(assign!(ToDeviceConfig::default(), { enabled: Some(true) }))
            // TODO revert to `add_cached_list` when reloading rooms from the cache is blazingly
            // fast
            .add_list(
                SlidingSyncList::builder(ALL_ROOMS_LIST_NAME)
                    .sync_mode(SlidingSyncMode::new_selective().add_range(0..=19))
                    .timeline_limit(1)
                    .required_state(vec![
                        (StateEventType::RoomAvatar, "".to_owned()),
                        (StateEventType::RoomEncryption, "".to_owned()),
                        (StateEventType::RoomPowerLevels, "".to_owned()),
                    ])
                    .filters(Some(assign!(SyncRequestListFilters::default(), {
                        is_invite: Some(false),
                        is_tombstoned: Some(false),
                        not_room_types: vec!["m.space".to_owned()],
                    })))
                    .bump_event_types(&[
                        TimelineEventType::RoomMessage,
                        TimelineEventType::RoomEncrypted,
                        TimelineEventType::Sticker,
                    ]),
            )
            .build()
            .await
            .map(Arc::new)
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
    /// Stopping the [`Stream`] (i.e. by calling [`Self::stop_sync`]), and
    /// calling [`Self::sync`] again will resume from the previous state of
    /// the state machine.
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
                let next_state = self.state.get().next(&self.sliding_sync).await?;
                self.state.set(next_state);

                match sync.next().await {
                    Some(Ok(_update_summary)) => {
                        yield Ok(());
                    }

                    Some(Err(error)) => {
                        let next_state = State::Error { from: Box::new(self.state.get()) };
                        self.state.set(next_state);

                        yield Err(Error::SlidingSync(error));

                        break;
                    }

                    None => {
                        let next_state = State::Terminated { from: Box::new(self.state.get()) };
                        self.state.set(next_state);

                        break;
                    }
                }
            }
        }
    }

    /// Force to stop the sync of the room list started by [`Self::sync`].
    ///
    /// It's better to call this method rather than stop polling the `Stream`
    /// returned by [`Self::sync`] because it will force the cancellation and
    /// exit the sync-loop, i.e. it will cancel any in-flight HTTP requests,
    /// cancel any pending futures etc.
    ///
    /// Ideally, one wants to consume the `Stream` returned by [`Self::sync`]
    /// until it returns `None`, because of [`Self::stop_sync`], so that it
    /// ensures the states are correctly placed.
    ///
    /// Stopping the sync of the room list via this method will put the
    /// state-machine into the [`State::Terminated`] state.
    pub fn stop_sync(&self) -> Result<(), Error> {
        self.sliding_sync.stop_sync().map_err(Error::SlidingSync)
    }

    /// Get a subscriber to the state.
    pub fn state(&self) -> Subscriber<State> {
        self.state.subscribe()
    }

    /// Get all previous room list entries, in addition to a [`Stream`] to room
    /// list entry's updates.
    pub async fn entries(
        &self,
    ) -> Result<(Vector<RoomListEntry>, impl Stream<Item = VectorDiff<RoomListEntry>>), Error> {
        self.sliding_sync
            .on_list(ALL_ROOMS_LIST_NAME, |list| ready(list.room_list_stream()))
            .await
            .ok_or_else(|| Error::UnknownList(ALL_ROOMS_LIST_NAME.to_owned()))
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
            .ok_or_else(|| Error::UnknownList(ALL_ROOMS_LIST_NAME.to_owned()))
    }

    /// Get the entries loading state.
    ///
    /// It's a different state than [`State`]. It's also different than
    /// [`Self::entries`] which subscribes to room entries updates.
    ///
    /// This method is used to subscribe to “loading state”
    pub async fn entries_loading_state(
        &self,
    ) -> Result<(EntriesLoadingState, impl Stream<Item = EntriesLoadingState>), Error> {
        self.sliding_sync
            .on_list(ALL_ROOMS_LIST_NAME, |list| ready(list.state_stream()))
            .await
            .ok_or_else(|| Error::UnknownList(ALL_ROOMS_LIST_NAME.to_owned()))
    }

    /// Get all previous invites, in addition to a [`Stream`] to invites.
    ///
    /// Invites are taking the form of `RoomListEntry`, it's like a “sub” room
    /// list.
    pub async fn invites(
        &self,
    ) -> Result<(Vector<RoomListEntry>, impl Stream<Item = VectorDiff<RoomListEntry>>), Error> {
        self.sliding_sync
            .on_list(INVITES_LIST_NAME, |list| ready(list.room_list_stream()))
            .await
            .ok_or_else(|| Error::UnknownList(INVITES_LIST_NAME.to_owned()))
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
            Some(room) => Room::new(self.sliding_sync.clone(), room).await,
            None => Err(Error::RoomNotFound(room_id.to_owned())),
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn sliding_sync(&self) -> &SlidingSync {
        &self.sliding_sync
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

/// Type alias for entries loading state.
pub type EntriesLoadingState = SlidingSyncListLoadingState;

#[cfg(test)]
mod tests {
    use matrix_sdk::{
        config::RequestConfig,
        matrix_auth::{Session, SessionTokens},
        reqwest::Url,
    };
    use matrix_sdk_base::SessionMeta;
    use matrix_sdk_test::async_test;
    use ruma::{api::MatrixVersion, device_id, user_id};
    use wiremock::MockServer;

    use super::*;

    async fn new_client() -> (Client, MockServer) {
        let session = Session {
            meta: SessionMeta {
                user_id: user_id!("@example:localhost").to_owned(),
                device_id: device_id!("DEVICEID").to_owned(),
            },
            tokens: SessionTokens { access_token: "1234".to_owned(), refresh_token: None },
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

    pub(super) async fn new_room_list() -> Result<RoomList, Error> {
        let (client, _) = new_client().await;

        RoomList::new(client).await
    }

    #[async_test]
    async fn test_sliding_sync_proxy_url() -> Result<(), Error> {
        let (client, _) = new_client().await;

        {
            let room_list = RoomList::new(client.clone()).await?;

            assert!(room_list.sliding_sync().sliding_sync_proxy().is_none());
        }

        {
            let url = Url::parse("https://foo.matrix/").unwrap();
            client.set_sliding_sync_proxy(Some(url.clone()));

            let room_list = RoomList::new(client.clone()).await?;

            assert_eq!(room_list.sliding_sync().sliding_sync_proxy(), Some(url));
        }

        Ok(())
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
}
