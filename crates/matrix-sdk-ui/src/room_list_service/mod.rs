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
//! rooms to the user. The syncing is handled by [`SlidingSync`]. The idea is to
//! expose a simple API to handle most of the client app use cases, like:
//! Showing and updating a list of rooms, filtering a list of rooms, handling
//! particular updates of a range of rooms (the ones the client app is showing
//! to the view, i.e. the rooms present in the viewport) etc.
//!
//! As such, the `RoomListService` works as an opinionated state machine. The
//! states are defined by [`State`]. Actions are attached to the each state
//! transition.
//!
//! The API is purposely small. Sliding Sync is versatile. `RoomListService` is
//! _one_ specific usage of Sliding Sync.
//!
//! # Basic principle
//!
//! `RoomListService` works with 1 Sliding Sync List:
//!
//! * `all_rooms` (referred by the constant [`ALL_ROOMS_LIST_NAME`]) is the only
//!   list. Its goal is to load all the user' rooms. It starts with a
//!   [`SlidingSyncMode::Selective`] sync-mode with a small range (i.e. a small
//!   set of rooms) to load the first rooms quickly, and then updates to a
//!   [`SlidingSyncMode::Growing`] sync-mode to load the remaining rooms “in the
//!   background”: it will sync the existing rooms and will fetch new rooms, by
//!   a certain batch size.
//!
//! This behavior has proven to be empirically satisfying to provide a fast and
//! fluid user experience for a Matrix client.
//!
//! [`RoomListService::all_rooms`] provides a way to get a [`RoomList`] for all
//! the rooms. From that, calling [`RoomList::entries_with_dynamic_adapters`]
//! provides a way to get a stream of rooms. This stream is sorted, can be
//! filtered, and the filter can be changed over time.
//!
//! [`RoomListService::state`] provides a way to get a stream of the state
//! machine's state, which can be pretty helpful for the client app.

pub mod filters;
mod room_list;
pub mod sorters;
mod state;

use std::{sync::Arc, time::Duration};

use async_stream::stream;
use eyeball::Subscriber;
use futures_util::{Stream, StreamExt, pin_mut};
use matrix_sdk::{
    Client, Error as SlidingSyncError, Room, SlidingSync, SlidingSyncList, SlidingSyncMode,
    event_cache::EventCacheError, sliding_sync::PollTimeout, timeout::timeout,
};
pub use room_list::*;
use ruma::{
    OwnedRoomId, RoomId, UInt,
    api::{FeatureFlag, client::sync::sync_events::v5 as http},
    assign,
    events::StateEventType,
};
pub use state::*;
use thiserror::Error;
use tracing::{debug, error, warn};

/// The default `required_state` constant value for sliding sync lists and
/// sliding sync room subscriptions.
const DEFAULT_REQUIRED_STATE: &[(StateEventType, &str)] = &[
    (StateEventType::RoomName, ""),
    (StateEventType::RoomEncryption, ""),
    (StateEventType::RoomMember, "$LAZY"),
    (StateEventType::RoomMember, "$ME"),
    (StateEventType::RoomTopic, ""),
    // Temporary workaround for https://github.com/matrix-org/matrix-rust-sdk/issues/5285
    (StateEventType::RoomAvatar, ""),
    (StateEventType::RoomCanonicalAlias, ""),
    (StateEventType::RoomPowerLevels, ""),
    (StateEventType::CallMember, "*"),
    (StateEventType::RoomJoinRules, ""),
    (StateEventType::RoomTombstone, ""),
    // Those two events are required to properly compute room previews.
    // `StateEventType::RoomCreate` is also necessary to compute the room
    // version, and thus handling the tombstoned room correctly.
    (StateEventType::RoomCreate, ""),
    (StateEventType::RoomHistoryVisibility, ""),
    // Required to correctly calculate the room display name.
    (StateEventType::MemberHints, ""),
    (StateEventType::SpaceParent, "*"),
    (StateEventType::SpaceChild, "*"),
];

/// The default `required_state` constant value for sliding sync room
/// subscriptions that must be added to `DEFAULT_REQUIRED_STATE`.
const DEFAULT_ROOM_SUBSCRIPTION_EXTRA_REQUIRED_STATE: &[(StateEventType, &str)] =
    &[(StateEventType::RoomPinnedEvents, "")];

/// The default `timeline_limit` value when used with room subscriptions.
const DEFAULT_ROOM_SUBSCRIPTION_TIMELINE_LIMIT: u32 = 20;

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
    state_machine: StateMachine,
}

impl RoomListService {
    /// Create a new `RoomList`.
    ///
    /// A [`matrix_sdk::SlidingSync`] client will be created, with a cached list
    /// already pre-configured.
    ///
    /// This won't start an encryption sync, and it's the user's responsibility
    /// to create one in this case using
    /// [`EncryptionSyncService`][crate::encryption_sync_service::EncryptionSyncService].
    pub async fn new(client: Client) -> Result<Self, Error> {
        Self::new_with_share_pos(client, true).await
    }

    /// Like [`RoomListService::new`] but with a flag to turn the
    /// [`SlidingSyncBuilder::share_pos`] on and off.
    ///
    /// [`SlidingSyncBuilder::share_pos`]: matrix_sdk::sliding_sync::SlidingSyncBuilder::share_pos
    pub async fn new_with_share_pos(client: Client, share_pos: bool) -> Result<Self, Error> {
        let mut builder = client
            .sliding_sync("room-list")
            .map_err(Error::SlidingSync)?
            .with_account_data_extension(
                assign!(http::request::AccountData::default(), { enabled: Some(true) }),
            )
            .with_receipt_extension(assign!(http::request::Receipts::default(), {
                enabled: Some(true),
                rooms: Some(vec![http::request::ExtensionRoomConfig::AllSubscribed])
            }))
            .with_typing_extension(assign!(http::request::Typing::default(), {
                enabled: Some(true),
            }));

        if client.enabled_thread_subscriptions() {
            let server_features = if let Some(cached) = client
                .supported_versions_cached()
                .await
                .map_err(|e| Error::SlidingSync(e.into()))?
            {
                cached.features
            } else {
                client
                    .fetch_server_versions(None)
                    .await
                    .map_err(|e| Error::SlidingSync(e.into()))?
                    .as_supported_versions()
                    .features
            };

            if !server_features.contains(&FeatureFlag::from("org.matrix.msc4306")) {
                warn!(
                    "Thread subscriptions extension is requested on the client, but the server doesn't advertise support for it: not enabling."
                );
            } else {
                debug!("Enabling the thread subscriptions extension");
                builder = builder.with_thread_subscriptions_extension(
                    assign!(http::request::ThreadSubscriptions::default(), {
                        enabled: Some(true),
                        limit: Some(ruma::uint!(10))
                    }),
                );
            }
        }

        if share_pos {
            // The e2ee extensions aren't enabled in this sliding sync instance, and this is
            // the only one that could be used from a different process. So it's
            // fine to enable position sharing (i.e. reloading it from disk),
            // since it's always exclusively owned by the current process.
            debug!("Enabling `share_pos` for the room list sliding sync");
            builder = builder.share_pos();
        }

        let state_machine = StateMachine::new();
        let observable_state = state_machine.cloned_state();

        let sliding_sync = builder
            .add_cached_list(
                SlidingSyncList::builder(ALL_ROOMS_LIST_NAME)
                    .sync_mode(
                        SlidingSyncMode::new_selective()
                            .add_range(ALL_ROOMS_DEFAULT_SELECTIVE_RANGE),
                    )
                    .timeline_limit(1)
                    .required_state(
                        DEFAULT_REQUIRED_STATE
                            .iter()
                            .map(|(state_event, value)| (state_event.clone(), (*value).to_owned()))
                            .collect(),
                    )
                    .filters(Some(assign!(http::request::ListFilters::default(), {
                        // As defined in the [SlidingSync MSC](https://github.com/matrix-org/matrix-spec-proposals/blob/9450ced7fb9cf5ea9077d029b3adf36aebfa8709/proposals/3575-sync.md?plain=1#L444)
                        // If unset, both invited and joined rooms are returned. If false, no invited rooms are
                        // returned. If true, only invited rooms are returned.
                        is_invite: None,
                    })))
                    .requires_timeout(move |request_generator| {
                        // We want Sliding Sync to apply the poll + network timeout —i.e. to do the
                        // long-polling— in some particular cases. Let's define them.
                        match observable_state.get() {
                            // These are the states where we want an immediate response from the
                            // server, with no long-polling.
                            State::Init
                            | State::SettingUp
                            | State::Recovering
                            | State::Error { .. }
                            | State::Terminated { .. } => PollTimeout::Some(0),

                            // Otherwise we want long-polling if the list is fully-loaded.
                            State::Running => {
                                if request_generator.is_fully_loaded() {
                                    // Long-polling.
                                    PollTimeout::Default
                                } else {
                                    // No long-polling yet.
                                    PollTimeout::Some(0)
                                }
                            }
                        }
                    }),
            )
            .await
            .map_err(Error::SlidingSync)?
            .build()
            .await
            .map(Arc::new)
            .map_err(Error::SlidingSync)?;

        // Eagerly subscribe the event cache to sync responses.
        client.event_cache().subscribe()?;

        Ok(Self { client, sliding_sync, state_machine })
    }

    /// Start to sync the room list.
    ///
    /// It's the main method of this entire API. Calling `sync` allows to
    /// receive updates on the room list: new rooms, rooms updates etc. Those
    /// updates can be read with `RoomList::entries` for example. This method
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
    /// using the [`SyncService`](crate::sync_service::SyncService) instead.
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
                debug!("Run a sync iteration");

                // Calculate the next state, and run the associated actions.
                let next_state = self.state_machine.next(&self.sliding_sync).await?;

                // Do the sync.
                match sync.next().await {
                    // Got a successful result while syncing.
                    Some(Ok(_update_summary)) => {
                        debug!(state = ?next_state, "New state");

                        // Update the state.
                        self.state_machine.set(next_state);

                        yield Ok(());
                    }

                    // Got an error while syncing.
                    Some(Err(error)) => {
                        debug!(expected_state = ?next_state, "New state is an error");

                        let next_state = State::Error { from: Box::new(next_state) };
                        self.state_machine.set(next_state);

                        yield Err(Error::SlidingSync(error));

                        break;
                    }

                    // Sync loop has terminated.
                    None => {
                        debug!(expected_state = ?next_state, "New state is a termination");

                        let next_state = State::Terminated { from: Box::new(next_state) };
                        self.state_machine.set(next_state);

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
    /// cancellation and exit the sync loop, i.e. it will cancel any
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
    /// using the [`SyncService`](crate::sync_service::SyncService) instead.
    #[doc(hidden)]
    pub fn stop_sync(&self) -> Result<(), Error> {
        self.sliding_sync.stop_sync().map_err(Error::SlidingSync)
    }

    /// Force the sliding sync session to expire.
    ///
    /// This is used by [`SyncService`](crate::sync_service::SyncService).
    ///
    /// **Warning**: This method **must not** be called while the sync loop is
    /// running!
    pub(crate) async fn expire_sync_session(&self) {
        self.sliding_sync.expire_session().await;

        // Usually, when the session expires, it leads the state to be `Error`,
        // thus some actions (like refreshing the lists) are executed. However,
        // if the sync loop has been stopped manually, the state is `Terminated`, and
        // when the session is forced to expire, the state remains `Terminated`, thus
        // the actions aren't executed as expected. Consequently, let's update the
        // state.
        if let State::Terminated { from } = self.state_machine.get() {
            self.state_machine.set(State::Error { from });
        }
    }

    /// Get a [`Stream`] of [`SyncIndicator`].
    ///
    /// Read the documentation of [`SyncIndicator`] to learn more about it.
    pub fn sync_indicator(
        &self,
        delay_before_showing: Duration,
        delay_before_hiding: Duration,
    ) -> impl Stream<Item = SyncIndicator> + use<> {
        let mut state = self.state();

        stream! {
            // Ensure the `SyncIndicator` is always hidden to start with.
            yield SyncIndicator::Hide;

            // Let's not wait for an update to happen. The `SyncIndicator` must be
            // computed as fast as possible.
            let mut current_state = state.next_now();

            loop {
                let (sync_indicator, yield_delay) = match current_state {
                    State::SettingUp | State::Error { .. } => {
                        (SyncIndicator::Show, delay_before_showing)
                    }

                    State::Init | State::Recovering | State::Running | State::Terminated { .. } => {
                        (SyncIndicator::Hide, delay_before_hiding)
                    }
                };

                // `state.next().await` has a maximum of `yield_delay` time to execute…
                let next_state = match timeout(state.next(), yield_delay).await {
                    // A new state has been received before `yield_delay` time. The new
                    // `sync_indicator` value won't be yielded.
                    Ok(next_state) => next_state,

                    // No new state has been received before `yield_delay` time. The
                    // `sync_indicator` value can be yielded.
                    Err(_) => {
                        yield sync_indicator;

                        // Now that `sync_indicator` has been yielded, let's wait on
                        // the next state again.
                        state.next().await
                    }
                };

                if let Some(next_state) = next_state {
                    // Update the `current_state`.
                    current_state = next_state;
                } else {
                    // Something is broken with the state. Let's stop this stream too.
                    break;
                }
            }
        }
    }

    /// Get the [`Client`] that has been used to create [`Self`].
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Get a subscriber to the state.
    pub fn state(&self) -> Subscriber<State> {
        self.state_machine.subscribe()
    }

    async fn list_for(&self, sliding_sync_list_name: &str) -> Result<RoomList, Error> {
        RoomList::new(&self.client, &self.sliding_sync, sliding_sync_list_name, self.state()).await
    }

    /// Get a [`RoomList`] for all rooms.
    pub async fn all_rooms(&self) -> Result<RoomList, Error> {
        self.list_for(ALL_ROOMS_LIST_NAME).await
    }

    /// Get a [`Room`] if it exists.
    pub fn room(&self, room_id: &RoomId) -> Result<Room, Error> {
        self.client.get_room(room_id).ok_or_else(|| Error::RoomNotFound(room_id.to_owned()))
    }

    /// Subscribe to rooms.
    ///
    /// It means that all events from these rooms will be received every time,
    /// no matter how the `RoomList` is configured.
    ///
    /// [`LatestEvents::listen_to_room`][listen_to_room] will be called for each
    /// room in `room_ids`, so that the [`LatestEventValue`] will automatically
    /// be calculated and updated for these rooms, for free.
    ///
    /// All previous room subscriptions will be forgotten.
    ///
    /// [listen_to_room]: matrix_sdk::latest_events::LatestEvents::listen_to_room
    /// [`LatestEventValue`]: matrix_sdk::latest_events::LatestEventValue
    pub async fn subscribe_to_rooms(&self, room_ids: &[&RoomId]) {
        // Calculate the settings for the room subscriptions.
        let settings = assign!(http::request::RoomSubscription::default(), {
            required_state: DEFAULT_REQUIRED_STATE.iter().map(|(state_event, value)| {
                (state_event.clone(), (*value).to_owned())
            })
            .chain(
                DEFAULT_ROOM_SUBSCRIPTION_EXTRA_REQUIRED_STATE.iter().map(|(state_event, value)| {
                    (state_event.clone(), (*value).to_owned())
                })
            )
            .collect(),
            timeline_limit: UInt::from(DEFAULT_ROOM_SUBSCRIPTION_TIMELINE_LIMIT),
        });

        // Decide whether the in-flight request (if any) should be cancelled if needed.
        let cancel_in_flight_request = match self.state_machine.get() {
            State::Init | State::Recovering | State::Error { .. } | State::Terminated { .. } => {
                false
            }
            State::SettingUp | State::Running => true,
        };

        // Before subscribing, let's listen these rooms to calculate their latest
        // events.
        if self.client.event_cache().has_subscribed() {
            let latest_events = self.client.latest_events().await;

            for room_id in room_ids {
                if let Err(error) = latest_events.listen_to_room(room_id).await {
                    // Let's not fail the room subscription. Instead, emit a log because it's very
                    // unlikely to happen.
                    error!(?error, ?room_id, "Failed to listen to the latest event for this room");
                }
            }
        }

        // Subscribe to the rooms.
        self.sliding_sync.clear_and_subscribe_to_rooms(
            room_ids,
            Some(settings),
            cancel_in_flight_request,
        )
    }

    #[cfg(test)]
    pub fn sliding_sync(&self) -> &SlidingSync {
        &self.sliding_sync
    }
}

/// [`RoomList`]'s errors.
#[derive(Debug, Error)]
pub enum Error {
    /// Error from [`matrix_sdk::SlidingSync`].
    #[error(transparent)]
    SlidingSync(SlidingSyncError),

    /// An operation has been requested on an unknown list.
    #[error("Unknown list `{0}`")]
    UnknownList(String),

    /// The requested room doesn't exist.
    #[error("Room `{0}` not found")]
    RoomNotFound(OwnedRoomId),

    #[error(transparent)]
    EventCache(#[from] EventCacheError),
}

/// An hint whether a _sync spinner/loader/toaster_ should be prompted to the
/// user, indicating that the [`RoomListService`] is syncing.
///
/// This is entirely arbitrary and optinionated. Of course, once
/// [`RoomListService::sync`] has been called, it's going to be constantly
/// syncing, until [`RoomListService::stop_sync`] is called, or until an error
/// happened. But in some cases, it's better for the user experience to prompt
/// to the user that a sync is happening. It's usually the first sync, or the
/// recovering sync. However, the sync indicator must be prompted if the
/// aforementioned sync is “slow”, otherwise the indicator is likely to “blink”
/// pretty fast, which can be very confusing. It's also common to indicate to
/// the user that a syncing is happening in case of a network error, that
/// something is catching up etc.
#[derive(Debug, Eq, PartialEq)]
pub enum SyncIndicator {
    /// Show the sync indicator.
    Show,

    /// Hide the sync indicator.
    Hide,
}

#[cfg(test)]
mod tests {
    use std::future::ready;

    use futures_util::{StreamExt, pin_mut};
    use matrix_sdk::{
        Client, SlidingSyncMode, config::RequestConfig, test_utils::client::mock_matrix_session,
    };
    use matrix_sdk_test::async_test;
    use ruma::api::MatrixVersion;
    use serde_json::json;
    use wiremock::{Match, Mock, MockServer, Request, ResponseTemplate, http::Method};

    use super::{ALL_ROOMS_LIST_NAME, Error, RoomListService, State};

    async fn new_client() -> (Client, MockServer) {
        let session = mock_matrix_session();

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
            request.url.path() == "/_matrix/client/unstable/org.matrix.simplified_msc3575/sync"
                && request.method == Method::POST
        }
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
        assert_eq!(
            room_list.state_machine.get(),
            State::Terminated { from: Box::new(State::Running) }
        );

        // Now, let's make the sliding sync session to expire.
        room_list.expire_sync_session().await;

        // State is `Error`, as a regular session expiration would generate!
        assert_eq!(room_list.state_machine.get(), State::Error { from: Box::new(State::Running) });

        Ok(())
    }
}
