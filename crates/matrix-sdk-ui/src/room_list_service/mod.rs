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

//! `RoomListService` API.
//!
//! The `RoomListService` is a UI API dedicated to present a list of Matrix
//! rooms to the user. The syncing is handled by
//! [`SlidingSync`][matrix_sdk::SlidingSync]. The idea is to expose a simple API
//! to handle most of the client app use cases, like: Showing and updating a
//! list of rooms, filtering a list of rooms, handling particular updates of a
//! range of rooms (the ones the client app is showing to the view, i.e. the
//! rooms present in the viewport) etc.
//!
//! As such, the `RoomListService` works as an opinionated state machine. The
//! states are defined by [`State`]. Actions are attached to the each state
//! transition. Apart from that, one can apply [`Input`]s on the state machine,
//! like notifying that the client app viewport of the room list has changed (if
//! the user of the client app has scrolled in the room list for example) etc.
//!
//! The API is purposely small. Sliding Sync is versatile. `RoomListService` is
//! _one_ specific usage of Sliding Sync.
//!
//! # Basic principle
//!
//! `RoomListService` works with 2 Sliding Sync List:
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
//! [`RoomListService::all_rooms`] provides a way to get a [`RoomList`] for all
//! the rooms. From that, calling [`RoomList::entries`] provides a way to get a
//! stream of room list entry. This stream can be filtered, and the filter can
//! be changed over time.
//!
//! [`RoomListService::state`] provides a way to get a stream of the state
//! machine's state, which can be pretty helpful for the client app.

pub mod filters;
mod room;
mod room_list;
mod state;

use std::{future::ready, sync::Arc};

use async_stream::stream;
use eyeball::{SharedObservable, Subscriber};
use futures_util::{pin_mut, Stream, StreamExt};
pub use matrix_sdk::RoomListEntry;
use matrix_sdk::{
    sliding_sync::Ranges, Client, Error as SlidingSyncError, SlidingSync, SlidingSyncList,
    SlidingSyncListBuilder, SlidingSyncMode,
};
use matrix_sdk_base::ring_buffer::RingBuffer;
pub use room::*;
pub use room_list::*;
use ruma::{
    api::client::sync::sync_events::v4::{
        AccountDataConfig, E2EEConfig, SyncRequestListFilters, ToDeviceConfig,
    },
    assign,
    events::{StateEventType, TimelineEventType},
    OwnedRoomId, RoomId,
};
pub use state::*;
use thiserror::Error;
use tokio::sync::{Mutex, RwLock};

/// The [`RoomListService`] type. See the module's documentation to learn more.
#[derive(Debug)]
pub struct RoomListService {
    /// Client that has created this [`RoomListService`].
    client: Client,

    /// The Sliding Sync instance.
    sliding_sync: Arc<SlidingSync>,

    /// The current state of the `RoomListService`.
    ///
    /// `RoomListService` is a simple state-machine.
    state: SharedObservable<State>,

    /// Room cache, to avoid recreating `Room`s every time users fetch them.
    rooms: Arc<RwLock<RingBuffer<Room>>>,

    /// The current viewport ranges.
    ///
    /// This is useful to avoid resetting the ranges to the same value,
    /// which would cancel the current in-flight sync request.
    viewport_ranges: Mutex<Ranges>,
}

impl RoomListService {
    /// Size of the room's ring buffer.
    ///
    /// This number should be high enough so that navigating to a room
    /// previously visited is almost instant, but also not too high so as to
    /// avoid exhausting memory.
    const ROOM_OBJECT_CACHE_SIZE: usize = 128;

    /// Create a new `RoomList`.
    ///
    /// A [`matrix_sdk::SlidingSync`] client will be created, with a cached list
    /// already pre-configured.
    ///
    /// This won't start an encryption sync, and it's the user's responsibility
    /// to create one in this case using `EncryptionSync`.
    pub async fn new(client: Client) -> Result<Self, Error> {
        Self::new_internal(client, false).await
    }

    /// Create a new `RoomList` that enables encryption.
    ///
    /// This will include syncing the encryption information, so there must not
    /// be any instance of `EncryptionSync` running in the background.
    pub async fn new_with_encryption(client: Client) -> Result<Self, Error> {
        Self::new_internal(client, true).await
    }

    async fn new_internal(client: Client, with_encryption: bool) -> Result<Self, Error> {
        let mut builder = client
            .sliding_sync("room-list")
            .map_err(Error::SlidingSync)?
            .with_account_data_extension(
                assign!(AccountDataConfig::default(), { enabled: Some(true) }),
            );

        if with_encryption {
            builder = builder
                .with_e2ee_extension(assign!(E2EEConfig::default(), { enabled: Some(true) }))
                .with_to_device_extension(
                    assign!(ToDeviceConfig::default(), { enabled: Some(true) }),
                );
        }

        let sliding_sync = builder
            .add_cached_list(configure_all_or_visible_rooms_list(
                SlidingSyncList::builder(ALL_ROOMS_LIST_NAME)
                    .sync_mode(SlidingSyncMode::new_selective().add_range(0..=19))
                    .timeline_limit(1)
                    .required_state(vec![
                        (StateEventType::RoomAvatar, "".to_owned()),
                        (StateEventType::RoomEncryption, "".to_owned()),
                        (StateEventType::RoomPowerLevels, "".to_owned()),
                    ]),
            ))
            .await
            .map_err(Error::SlidingSync)?
            .build()
            .await
            .map(Arc::new)
            .map_err(Error::SlidingSync)?;

        Ok(Self {
            client,
            sliding_sync,
            state: SharedObservable::new(State::Init),
            rooms: Arc::new(RwLock::new(RingBuffer::new(Self::ROOM_OBJECT_CACHE_SIZE))),
            viewport_ranges: Mutex::new(vec![VISIBLE_ROOMS_DEFAULT_RANGE]),
        })
    }

    /// Start to sync the room list.
    ///
    /// It's the main method of this entire API. Calling `sync` allows to
    /// receive updates on the room list: new rooms, rooms updates etc. Those
    /// updates can be read with [`RoomList::entries`] for example. This method
    /// returns a [`Stream`] where produced items only hold an empty value
    /// in case of a sync success, otherwise an error.
    ///
    /// The `RoomListService`' state machine is run by this method.
    ///
    /// Stopping the [`Stream`] (i.e. by calling [`Self::stop_sync`]), and
    /// calling [`Self::sync`] again will resume from the previous state of
    /// the state machine.
    ///
    /// This should be used only for testing. In practice, most users should be
    /// using the [`SyncService`] instead.
    #[doc(hidden)]
    pub fn sync(&self) -> impl Stream<Item = Result<(), Error>> + '_ {
        stream! {
            let sync = self.sliding_sync.sync();
            pin_mut!(sync);

            // This is a state machine implementation.
            // Things happen in this order:
            //
            // 1. The next state is calculated,
            // 2. The actions associated to the next state are run,
            // 3. A sync is done,
            // 4. The next state is stored.
            loop {
                // Calculate the next state, and run the associated actions.
                let next_state = self.state.get().next(&self.sliding_sync).await?;

                // Do the sync.
                match sync.next().await {
                    // Got a successful result while syncing.
                    Some(Ok(_update_summary)) => {
                        // Update the state.
                        self.state.set(next_state);

                        yield Ok(());
                    }

                    // Got an error while syncing.
                    Some(Err(error)) => {
                        let next_state = State::Error { from: Box::new(next_state) };
                        self.state.set(next_state);

                        yield Err(Error::SlidingSync(error));

                        break;
                    }

                    // Sync-loop has terminated.
                    None => {
                        let next_state = State::Terminated { from: Box::new(next_state) };
                        self.state.set(next_state);

                        break;
                    }
                }
            }
        }
    }

    /// Force to stop the sync of the `RoomListService` started by
    /// [`Self::sync`].
    ///
    /// It's of utter importance to call this method rather than stop polling
    /// the `Stream` returned by [`Self::sync`] because it will force the
    /// cancellation and exit the sync-loop, i.e. it will cancel any
    /// in-flight HTTP requests, cancel any pending futures etc. and put the
    /// service into a termination state.
    ///
    /// Ideally, one wants to consume the `Stream` returned by [`Self::sync`]
    /// until it returns `None`, because of [`Self::stop_sync`], so that it
    /// ensures the states are correctly placed.
    ///
    /// Stopping the sync of the room list via this method will put the
    /// state-machine into the [`State::Terminated`] state.
    ///
    /// This should be used only for testing. In practice, most users should be
    /// using the [`SyncService`] instead.
    #[doc(hidden)]
    pub fn stop_sync(&self) -> Result<(), Error> {
        self.sliding_sync.stop_sync().map_err(Error::SlidingSync)
    }

    /// Force the sliding sync session to expire.
    ///
    /// This is used by [`SyncService`][crate::SyncService].
    ///
    /// **Warning**: This method **must** be called when the sync-loop isn't
    /// running!
    pub(crate) async fn expire_sync_session(&self) {
        self.sliding_sync.expire_session().await;

        // Usually, when the session expires, it leads the state to be `Error`,
        // thus some actions (like refreshing the lists) are executed. However,
        // if the sync-loop has been stopped manually, the state is `Terminated`, and
        // when the session is forced to expire, the state remains `Terminated`, thus
        // the actions aren't executed as expected. Consequently, let's update the
        // state.
        if let State::Terminated { from } = self.state.get() {
            self.state.set(State::Error { from });
        }
    }

    /// Get the [`Client`] that has been used to create [`Self`].
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Get a subscriber to the state.
    pub fn state(&self) -> Subscriber<State> {
        self.state.subscribe()
    }

    async fn list_for(&self, sliding_sync_list_name: &str) -> Result<RoomList, Error> {
        RoomList::new(&self.sliding_sync, sliding_sync_list_name, self.state()).await
    }

    /// Get a [`RoomList`] for all rooms.
    pub async fn all_rooms(&self) -> Result<RoomList, Error> {
        self.list_for(ALL_ROOMS_LIST_NAME).await
    }

    /// Get a [`RoomList`] for invites, i.e. rooms where the user is invited to
    /// join.
    pub async fn invites(&self) -> Result<RoomList, Error> {
        self.list_for(INVITES_LIST_NAME).await
    }

    /// Pass an [`Input`] onto the state machine.
    pub async fn apply_input(&self, input: Input) -> Result<InputResult, Error> {
        use Input::*;

        match input {
            Viewport(ranges) => self.update_viewport(ranges).await,
        }
    }

    async fn update_viewport(&self, ranges: Ranges) -> Result<InputResult, Error> {
        let mut viewport_ranges = self.viewport_ranges.lock().await;

        // Is it worth updating the viewport?
        // The viewport has the same ranges. Don't update it.
        if *viewport_ranges == ranges {
            return Ok(InputResult::Ignored);
        }

        self.sliding_sync
            .on_list(VISIBLE_ROOMS_LIST_NAME, |list| {
                list.set_sync_mode(SlidingSyncMode::new_selective().add_ranges(ranges.clone()));

                ready(())
            })
            .await
            .ok_or_else(|| Error::InputCannotBeApplied(Input::Viewport(ranges.clone())))?;

        *viewport_ranges = ranges;

        Ok(InputResult::Applied)
    }

    /// Get a [`Room`] if it exists.
    pub async fn room(&self, room_id: &RoomId) -> Result<Room, Error> {
        {
            let rooms = self.rooms.read().await;

            if let Some(room) = rooms.iter().rfind(|room| room.id() == room_id) {
                return Ok(room.clone());
            }
        }

        let room = match self.sliding_sync.get_room(room_id).await {
            Some(room) => Room::new(self.sliding_sync.clone(), room)?,
            None => return Err(Error::RoomNotFound(room_id.to_owned())),
        };

        self.rooms.write().await.push(room.clone());

        Ok(room)
    }

    #[cfg(test)]
    pub fn sliding_sync(&self) -> &SlidingSync {
        &self.sliding_sync
    }
}

/// Configure the Sliding Sync list for `ALL_ROOMS_LIST_NAME` and
/// `VISIBLE_ROOMS_LIST_NAME`.
///
/// This function configures the `sort`, the `filters` and the`bump_event_types`
/// properties, so that they are exactly the same.
fn configure_all_or_visible_rooms_list(
    list_builder: SlidingSyncListBuilder,
) -> SlidingSyncListBuilder {
    list_builder
        .sort(vec!["by_recency".to_owned(), "by_name".to_owned()])
        .filters(Some(assign!(SyncRequestListFilters::default(), {
            is_invite: Some(false),
            is_tombstoned: Some(false),
            not_room_types: vec!["m.space".to_owned()],
        })))
        .bump_event_types(&[
            TimelineEventType::RoomMessage,
            TimelineEventType::RoomEncrypted,
            TimelineEventType::Sticker,
        ])
}

/// [`RoomList`]'s errors.
#[derive(Debug, Error)]
pub enum Error {
    /// Error from [`matrix_sdk::SlidingSync`].
    #[error("SlidingSync failed: {0}")]
    SlidingSync(SlidingSyncError),

    /// An operation has been requested on an unknown list.
    #[error("Unknown list `{0}`")]
    UnknownList(String),

    /// An input was asked to be applied but it wasn't possible to apply it.
    #[error("The input cannot be applied")]
    InputCannotBeApplied(Input),

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

/// An [`Input`] Ok result: whether it's been applied, or ignored.
#[derive(Debug, Eq, PartialEq)]
pub enum InputResult {
    /// The input has been applied.
    Applied,

    /// The input has been ignored.
    ///
    /// Note that this is not an error. The input was valid, but simply ignored.
    Ignored,
}

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
    use serde_json::json;
    use wiremock::{http::Method, Match, Mock, MockServer, Request, ResponseTemplate};

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

    pub(super) async fn new_room_list() -> Result<RoomListService, Error> {
        let (client, _) = new_client().await;

        RoomListService::new(client).await
    }

    struct SlidingSyncMatcher;

    impl Match for SlidingSyncMatcher {
        fn matches(&self, request: &Request) -> bool {
            request.url.path() == "/_matrix/client/unstable/org.matrix.msc3575/sync"
                && request.method == Method::Post
        }
    }

    #[async_test]
    async fn test_sliding_sync_proxy_url() -> Result<(), Error> {
        let (client, _) = new_client().await;

        {
            let room_list = RoomListService::new(client.clone()).await?;

            assert!(room_list.sliding_sync().sliding_sync_proxy().is_none());
        }

        {
            let url = Url::parse("https://foo.matrix/").unwrap();
            client.set_sliding_sync_proxy(Some(url.clone()));

            let room_list = RoomListService::new(client.clone()).await?;

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

    #[async_test]
    async fn test_no_to_device_and_e2ee_if_not_explicitly_set() -> Result<(), Error> {
        let (client, _) = new_client().await;

        let no_encryption = RoomListService::new(client.clone()).await?;
        let extensions = no_encryption.sliding_sync.extensions_config();
        assert_eq!(extensions.e2ee.enabled, None);
        assert_eq!(extensions.to_device.enabled, None);

        let with_encryption = RoomListService::new_with_encryption(client).await?;
        let extensions = with_encryption.sliding_sync.extensions_config();
        assert_eq!(extensions.e2ee.enabled, Some(true));
        assert_eq!(extensions.to_device.enabled, Some(true));

        Ok(())
    }

    #[async_test]
    async fn test_expire_sliding_sync_session_manually() -> Result<(), Error> {
        let (client, server) = new_client().await;

        let room_list = RoomListService::new(client).await?;

        let sync = room_list.sync();
        pin_mut!(sync);

        // Run a first sync.
        {
            let _mock_guard = Mock::given(SlidingSyncMatcher)
                .respond_with(move |_request: &Request| {
                    ResponseTemplate::new(200).set_body_json(json!({
                        "pos": "0",
                        "lists": {
                            ALL_ROOMS_LIST_NAME: {
                                "count": 0,
                                "ops": [],
                            },
                        },
                        "rooms": {},
                    }))
                })
                .mount_as_scoped(&server)
                .await;

            let _ = sync.next().await;
        }

        assert_eq!(room_list.state().get(), State::SettingUp);

        // Stop the sync.
        room_list.stop_sync()?;

        // Do another sync.
        let _ = sync.next().await;

        // State is `Terminated`, as expected!
        assert_eq!(room_list.state.get(), State::Terminated { from: Box::new(State::Running) });

        // Now, let's make the sliding sync session to expire.
        room_list.expire_sync_session().await;

        // State is `Error`, as a regular session expiration would generate!
        assert_eq!(room_list.state.get(), State::Error { from: Box::new(State::Running) });

        Ok(())
    }
}
