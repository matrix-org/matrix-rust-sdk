// Copyright 2022-2023 Benjamin Kampmann
// Copyright 2022 The Matrix.org Foundation C.I.C.
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

//! Sliding Sync Client implementation of [MSC3575][MSC] & extensions
//!
//! [`Sliding Sync`][MSC] is the third generation synchronization mechanism of
//! Matrix with a strong focus on bandwidth efficiency. This is made possible by
//! allowing the client to filter the content very specifically in its request
//! which, as a result, allows the server to reduce the data sent to the
//! absolute necessary minimum needed. The API is modeled after common patterns
//! and UI components end-user messenger clients typically offer. By allowing a
//! tight coupling of what a client shows and synchronizing that state over
//! the protocol to the server, the server always sends exactly the information
//! necessary for the currently displayed subset for the user rather than
//! filling the connection with data the user isn't interested in right now.
//!
//! Sliding Sync is a live-protocol using [long-polling](#long-polling) HTTP(S)
//! connections to stay up to date. On the client side these updates are applied
//! and propagated through an [asynchronous reactive API](#reactive-api).
//!
//! The protocol is split into three major sections for that: room
//! lists or [views](#views), the [room details](#rooms) and
//! [extensions](#extensions), most notably the end-to-end-encryption and
//! to-device extensions to enable full end-to-end-encryption support.
//!
//! ## Starting up
//!
//! To create a new Sliding Sync session, one must query an existing
//! (authenticated) `Client` for a new [`SlidingSyncBuilder`] by calling
//! [`Client::sliding_sync`](`super::Client::sliding_sync`). The
//! [`SlidingSyncBuilder`] is the baseline configuration to create a
//! [`SlidingSync`] session by calling `.build()` once everything is ready.
//! Typically one configures the custom homeserver endpoint.
//!
//! At the time of writing, no Matrix server natively supports Sliding Sync;
//! a sidecar called the [Sliding Sync Proxy][proxy] is needed. As that
//! typically runs on a separate domain, it can be configured on the
//! [`SlidingSyncBuilder`]:
//!
//! ```no_run
//! # use futures::executor::block_on;
//! # use matrix_sdk::Client;
//! # use url::Url;
//! # block_on(async {
//! # let homeserver = Url::parse("http://example.com")?;
//! # let client = Client::new(homeserver).await?;
//! let sliding_sync_builder = client
//!     .sliding_sync()
//!     .await
//!     .homeserver(Url::parse("http://sliding-sync.example.org")?);
//!
//! # anyhow::Ok(())
//! # });
//! ```
//!
//! After the general configuration, one typically wants to add a view via the
//! [`add_view`][`SlidingSyncBuilder::add_view`] function.
//!
//! ## Views
//!
//! A view defines a subset of matching rooms one wants to filter for, and be
//! kept up about. The [`v4::SyncRequestListFilters`][] allows for a granular
//! specification of the exact rooms one wants the server to select and the way
//! one wants them to be ordered before receiving. Secondly each view has a set
//! of `ranges`: the subset of indexes of the entire list one is interested in
//! and a unique name to be identified with.
//!
//! For example, a user might be part of thousands of rooms, but if the client
//! app always starts by showing the most recent direct message conversations,
//! loading all rooms is an inefficient approach. Instead with Sliding Sync one
//! defines a view (e.g. named `"main_view"`) filtering for `is_dm`, ordered
//! by recency and select to view the top 10 via `ranges: [ [0,9] ]` (indexes
//! are **inclusive**) like so:
//!
//! ```rust
//! # use matrix_sdk::sliding_sync::{SlidingSyncViewBuilder, SlidingSyncMode};
//! use ruma::{assign, api::client::sync::sync_events::v4};
//!
//! let view_builder = SlidingSyncViewBuilder::default()
//!     .name("main_view")
//!     .sync_mode(SlidingSyncMode::Selective)
//!     .filters(Some(assign!(
//!         v4::SyncRequestListFilters::default(), { is_dm: Some(true)}
//!      )))
//!     .sort(vec!["by_recency".to_owned()])
//!     .set_range(0u32, 9u32);
//! ```
//!
//! Please refer to the [specification][MSC], the [Ruma types][ruma-types],
//! specifically [`SyncRequestListFilter`](https://docs.rs/ruma/latest/ruma/api/client/sync/sync_events/v4/struct.SyncRequestListFilters.html) and the
//! [`SlidingSyncViewBuilder`] for details on the filters, sort-order and
//! range-options and data one requests to be sent. Once the view is fully
//! configured, `build()` it and add the view to the sliding sync session
//! by supplying it to [`add_view`][`SlidingSyncBuilder::add_view`].
//!
//! Views are inherently stateful and all updates are applied on the shared
//! view-object. Once a view has been added to [`SlidingSync`], a cloned shared
//! copy can be retrieved by calling `SlidingSync::view()`, providing the name
//! of the view. Next to the configuration settings (like name and
//! `timeline_limit`), the view provides the stateful
//! [`rooms_count`](SlidingSyncView::rooms_count),
//! [`rooms_list`](SlidingSyncView::rooms_list) and
//! [`state`](SlidingSyncView::state):
//!
//!  - `rooms_count` is the number of rooms _total_ there were found matching
//!    the filters given.
//!  - `rooms_list` is a vector of `rooms_count` [`RoomListEntry`]'s at the
//!    current state. `RoomListEntry`'s only hold `the room_id` if given, the
//!    [Rooms API](#rooms) holds the actual information about each room
//!  - `state` is a [`SlidingSyncMode`] signalling meta information about the
//!    view and its stateful data — whether this is the state loaded from local
//!    cache, whether the [full sync](#helper-views) is in progress or whether
//!    this is the current live information
//!
//! These are updated upon every update received from the server. One can query
//! these for their current value at any time, or use the [Reactive API
//! to subscribe to changes](#reactive-api).
//!
//! ### Helper Views
//!
//! By default views run in the [`Selective` mode](SlidingSyncMode::Selective).
//! That means one sets the desired range(s) to see explicitly (as described
//! above). Very often, one still wants to load up the entire room list in
//! background though. For that, the client implementation offers to run views
//! in two additional full-sync-modes, which require additional configuration:
//!
//! - [`SlidingSyncMode::PagingFullSync`]: Pages through the entire list of
//!   rooms one request at a time asking for the next `batch_size` number of
//!   rooms up to the end or `limit` if configured
//! - [`SlidingSyncMode::GrowingFullSync`]: Grows the window by `batch_size` on
//!   every request till all rooms or until `limit` of rooms are in view.
//!
//! For both, one should configure
//! [`batch_size`](SlidingSyncViewBuilder::batch_size) and optionally
//! [`limit`](SlidingSyncViewBuilder::limit) on the [`SlidingSyncViewBuilder`].
//! Both full-sync views will notice if the number of rooms increased at runtime
//! and will attempt to catch up to that (barring the `limit`).
//!
//! ## Rooms
//!
//! Next to the room list, the details for rooms are the next important aspect.
//! Each [view](#views) only references the [`OwnedRoomId`][ruma::OwnedRoomId]
//! of the room at the given position. The details (`required_state`s and
//! timeline items) requested by all views are bundled, together with the common
//! details (e.g. whether it is a `dm` or its calculated name) and made
//! available on the Sliding Sync session struct as a [reactive](#reactive-api)
//! through [`.rooms`](SlidingSync::rooms), [`get_room`](SlidingSync::get_room)
//! and [`get_rooms`](SlidingSync::get_rooms) APIs.
//!
//! Notably, this map only knows about the rooms that have come down [Sliding
//! Sync protocol][MSC] and if the given room isn't in any active view range, it
//! may be stale. Additionally to selecting the room data via the room lists,
//! the [Sliding Sync protocol][MSC] allows to subscribe to specific rooms via
//! the [`subscribe()`](SlidingSync::subscribe). Any room subscribed to will
//! receive updates (with the given settings) regardless of whether they are
//! visible in any view. The most common case for using this API is when the
//! user enters a room - as we want to receive the incoming new messages
//! regardless of whether the room is pushed out of the views room list.
//!
//! ### Room List Entries
//!
//! As the room list of each view is a vec of the `rooms_count` len but a room
//! may only know of a subset of entries for sure at any given time, these
//! entries are wrapped in [`RoomListEntry`][]. This type, in close proximity to
//! the [specification][MSC], can be either `Empty`, `Filled` or `Invalidated`,
//! signaling the state of each entry position.
//! - `Empty` we don't know what sits here at this position in the list.
//! - `Filled`: there is this `room_id` at this position.
//! - `Invalidated` in that sense means that we _knew_ what was here before, but
//!   can't be sure anymore this is still accurate. This occurs when we move the
//!   sliding window (by changing the ranges) or when a room might drop out of
//!   the window we are looking at. For the sake of displaying, this is probably
//!   still fine to display to be at this position, but we can't be sure
//!   anymore.
//!
//! Because `Invalidated` occurs whenever a room we knew about before drops out
//! of focus, we aren't updated about its changes anymore either, there could be
//! duplicates rooms within invalidated rooms as well as in the union of
//! invalidated and filled rooms. Keep that in mind, as most UI frameworks don't
//! like it when their list entries aren't unique.
//!
//! When [restoring from cold cache][#caching] the room list also only
//! propagated with `Invalidated` rooms. So if you want to be able to display
//! data quickly, ensure you are able to render `Invalidated` entries.
//!
//! ### Unsubscribe
//!
//! Don't forget to [unsubscribe](`SlidingSync::subscribe`) when the data isn't
//! needed to be updated anymore, e.g. when the user leaves the room, to reduce
//! the bandwidth back down to what is really needed.
//!
//! ## Extensions
//!
//! Additionally to the rooms list and rooms with their state and latest
//! messages Matrix knows of many other exchange information. All these are
//! modeled as specific, optional extensions in the [sliding sync
//! protocol][MSC]. This includes end-to-end-encryption, to-device-messages,
//! typing- and presence-information and account-data, but can be extended by
//! any implementation as they please. Handling of the data of the e2ee,
//! to-device and typing-extensions takes place transparently within the SDK.
//!
//! By default [`SlidingSync`][] doesn't activate _any_ extensions to save on
//! bandwidth, but we generally recommend to use the [`with_common_extensions`
//! when building sliding sync](`SlidingSyncBuilder::with_common_extensions`) to
//! active e2ee, to-device-messages and account-data-extensions.
//!
//! ## Timeline events
//!
//! Both the view configuration as well as the [room subscription
//! settings](`v4::RoomSubscription`) allow to specify a `timeline_limit` to
//! receive timeline events. If that is unset or set to 0, no events are sent by
//! the server (which is the default), if multiple limits are found, the highest
//! takes precedence. Any positive number indicates that on the first request a
//! room should come into view, up to that count of messages are sent
//! (depending how many the server has in cache). Following, whenever new events
//! are found for the matching rooms, the server relays them to the client.
//!
//! All timeline events coming through Sliding Sync will be processed through
//! the [`BaseClient`][`matrix_sdk_base::BaseClient`] as in previous sync. This
//! allows for transparent decryption as well trigger the `client_handlers`.
//!
//! The current and then following live events list can be queried via the
//! [`timeline` API](`SlidingSyncRoom::timeline). This is prefilled with already
//! received data.
//!
//! ### Timeline trickling
//!
//! To allow for a quick startup, client might want to request only a very low
//! `timeline_limit` (maybe 1 or even 0) at first and update the count later on
//! the view or room subscription (see [reactive api](#reactive-api)), Since
//! `0.99.0-rc1` the [sliding sync proxy][proxy] will then "paginate back" and
//! resent the now larger number of events. All this is handled transparently.
//!
//! ## Long Polling
//!
//! [Sliding Sync][MSC] is a long-polling API. That means that immediately after
//! one has received data from the server, they re-open the network connection
//! again and await for a new response. As there might not be happening much or
//! a lot happening in short succession — from the client perspective we never
//! know when new data is received.
//!
//! One principle of long-polling is, therefore, that it might also takes one
//! or two requests before the changes one asked for to actually be applied
//! and the results come back for that. Just assume that at the same time one
//! adds a room subscription, a new message comes in. The server might reply
//! with that message immediately and will only kick off the process of
//! calculating the rooms details and respond with that in the next request one
//! does after.
//!
//! This is modelled as a [async `Stream`][`futures_core::stream::Stream`] in
//! our API, that one basically wants to continue polling. Once one has made its
//! setup ready and build its sliding sync sessions, one wants to acquire its
//! [`.stream()`](`SlidingSync::stream`) and continuously poll it.
//!
//! While the async stream API allows for streams to end (by returning `None`)
//! Sliding Sync streams items `Result<UpdateSummary, Error>`. For every
//! successful poll, all data is applied internally, through the base client and
//! the [reactive structs](#reactive-api) and an
//! [`Ok(UpdateSummary)`][`UpdateSummary`] is yielded with the minimum
//! information, which data has been refreshed _in this iteration_: names of
//! views and `room_id`s of rooms. Note that, the same way that a view isn't
//! reacting if only the room data has changed (but not its position in its
//! list), the view won't be mentioned here either, only the `room_id`. So be
//! sure to look at both for all subscribed objects.
//!
//! In full, this typically looks like this:
//!
//! ```no_run
//! # use futures::executor::block_on;
//! # use futures::{pin_mut, StreamExt};
//! # use futures_signals::{signal::SignalExt, signal_vec::SignalVecExt};
//! # use matrix_sdk::{
//! #    sliding_sync::{SlidingSyncMode, SlidingSyncViewBuilder},
//! #    Client,
//! # };
//! # use ruma::{
//! #    api::client::sync::sync_events::v4, assign,
//! # };
//! # use tracing::{debug, error, info, warn};
//! # use url::Url;
//! # block_on(async {
//! # let homeserver = Url::parse("http://example.com")?;
//! # let client = Client::new(homeserver).await?;
//! let sliding_sync = client
//!     .sliding_sync()
//!     .await
//!     // any views you want are added here.
//!     .build()
//!     .await?;
//!
//! let stream = sliding_sync.stream();
//!
//! // continuously poll for updates
//! pin_mut!(stream);
//! loop {
//!     let update = match stream.next().await {
//!         Some(Ok(u)) => {
//!             info!("Received an update. Summary: {u:?}");
//!         }
//!         Some(Err(e)) => {
//!             error!("loop was stopped by client error processing: {e}");
//!         }
//!         None => {
//!             error!("Streaming loop ended unexpectedly");
//!             break;
//!         }
//!     };
//! }
//!
//! # anyhow::Ok(())
//! # });
//! ```
//!
//! ### Quick refreshing
//!
//! A main purpose of [Sliding Sync][MSC] is to provide an API for snappy end
//! user applications. Long-polling on the other side means that we wait for the
//! server to respond and that can take quite some time, before sending the next
//! request with our updates, for example an update in a view's `range`.
//!
//! That is a bit unfortunate and leaks through the `stream` API as well. We are
//! waiting for a `stream.next().await` call before the next request is sent.
//! The [specification][MSC] on long polling also states, however, that if an
//! new request is found coming in, the previous one shall be sent out. In
//! practice that means one can just start a new stream and the old connection
//! will return immediately — with a proper response though. One just needs to
//! make sure to not call that stream any further. Additionally, as both
//! requests are sent with the same positional argument, the server might
//! respond with data, the client has already processed. This isn't a problem,
//! the [`SlidingSync`][] will only process new data and skip the processing
//! even across restarts.
//!
//! To support this, in practice one should usually wrap its `loop` in a
//! spawn with an atomic flag that tells it to stop, which one can set upon
//! restart. Something along the lines of:
//!
//! ```no_run
//! # use futures::executor::block_on;
//! # use futures::{pin_mut, StreamExt};
//! # use futures_signals::{signal::SignalExt, signal_vec::SignalVecExt};
//! # use matrix_sdk::{
//! #    sliding_sync::{SlidingSyncMode, SlidingSyncViewBuilder, SlidingSync, Error},
//! #    Client,
//! # };
//! # use ruma::{
//! #    api::client::sync::sync_events::v4, assign,
//! # };
//! # use tracing::{debug, error, info, warn};
//! # use url::Url;
//! # block_on(async {
//! # let homeserver = Url::parse("http://example.com")?;
//! # let client = Client::new(homeserver).await?;
//! # let sliding_sync = client
//! #    .sliding_sync()
//! #    .await
//! #    // any views you want are added here.
//! #    .build()
//! #    .await?;
//! use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
//!
//! struct MyRunner{ lock: Arc<AtomicBool>, sliding_sync: SlidingSync };
//!
//! impl MyRunner {
//!   pub fn restart_sync(&mut self) {
//!     self.lock.store(false, Ordering::SeqCst);
//!     // create a new lock
//!     self.lock = Arc::new(AtomicBool::new(false));
//!
//!     let stream_lock = self.lock.clone();
//!     let sliding_sync = self.sliding_sync.clone();
//!
//!     // continuously poll for updates
//!     tokio::spawn(async move {
//!         let stream = sliding_sync.stream();
//!         pin_mut!(stream);
//!         loop {
//!             match stream.next().await {
//!                 Some(Ok(u)) => {
//!                     info!("Received an update. Summary: {u:?}");
//!                 }
//!                 Some(Err(e)) => {
//!                     error!("loop was stopped by client error processing: {e}");
//!                 }
//!                 None => {
//!                     error!("Streaming loop ended unexpectedly");
//!                     break;
//!                 }
//!            };
//!             if !stream_lock.load(Ordering::SeqCst) {
//!                 info!("Asked to stop");
//!                 break
//!             }
//!         };
//!     });
//!   }
//! }
//!
//! # anyhow::Ok(())
//! # });
//! ```
//!
//!
//! ## Reactive API
//!
//! As the main source of truth is the data coming from the server, all updates
//! must be applied transparently throughout to the data layer. The simplest
//! way to stay up to date on what objects have changed is by checking the
//! [`views`](`UpdateSummary.views`) and [`rooms`](`UpdateSummary.rooms`) of
//! each `UpdateSummary` given by each stream iteration and update the local
//! copies accordingly. Because of where the loop sits in the stack, that can
//! be a bit tedious though, so views and rooms have an additional way of
//! subscribing to updates via [`futures_signals`][].
//!
//! The `rooms_list` is of the more specialized
//! [`MutableVec`](`futures_signals::signal_vec::MutableVec`) type. Rather than
//! just signaling the latest state (which can be very inefficient, especially
//! on large lists), its
//! [`MutableSignalVec`](`futures_signals::signal_vec::MutableSignalVec`) will
//! share the modifications made by signalling
//! [`VecDiff`](`futures_signals::signal_vec::VecDiff`) over the stream. This
//! allows for easy and efficient synchronization of exactly those parts that
//! have been changed. If you are keeping a memory copy of the
//! `Vec<RoomListItem>` for your view for example, you can apply changes that
//! come as `VecDiff` easily by calling
//! [`apply_to_vec`](`futures_signals::signal_vec::VecDiff::apply_to_vec`).
//!
//! The `Timeline` you can receive per room by calling
//! [`.timeline()`][`SlidingSyncRoom::timeline`] will be populated with the
//! currently cached timeline events.
//!
//! 👉 To learn more about [`future_signals` check out to their excellent
//! tutorial][future-signals-tutorial].
//!
//! ## Caching
//!
//! All room data, for filled but also _invalidated_ rooms, including the entire
//! timeline events as well as all view `room_lists` and `rooms_count` are held
//! in memory (unless one `pop`s the view out). Technically, one can access
//! `rooms_list` and `rooms` directly and mutate them but doing so invalidates
//! further updates received by the server - see [#1474][https://github.com/matrix-org/matrix-rust-sdk/issues/1474].
//!
//! This is a purely in-memory cache layer though. If one wants Sliding Sync to
//! persist and load from cold (storage) cache, one needs to set its key with
//! [`cold_cache(name)`][`SlidingSyncBuilder::cold_cache`] and for each view
//! present at `.build()`[`SlidingSyncBuilder::build`] sliding sync will attempt
//! to load their latest cached version from storage, as well as some overall
//! information of Sliding Sync. If that succeeded the views `state` has been
//! set to [`Preload`][SlidingSyncViewState::Preload]. Only room data of rooms
//! present in one of the views is loaded from storage.
//!
//! Once [#1441](https://github.com/matrix-org/matrix-rust-sdk/pull/1441) is merged
//! one can disable caching on a per-view basis by setting
//! [`cold_cache(false)`][`SlidingSyncViewBuilder::cold_cache`] when
//! constructing the builder.
//!
//! Notice that views added after Sliding Sync has been built **will not be
//! loaded from cache** regardless of their settings (as this could lead to
//! inconsistencies between views). The same goes for any extension: some
//! extension data (like the to-device-message position) are stored to storage,
//! but only retrieved upon `build()` of the `SlidingSyncBuilder`. So if one
//! only adds them later, they will not be reading the data from storage (to
//! avoid inconsistencies) and might require more data to be sent in their first
//! request than if they were loaded form cold-cache.
//!
//! When loading from storage `rooms_list` entries found are set to
//! `Invalidated` — the initial setting here is communicated as a single
//! `VecDiff::Replace` event through the [reactive API](#reactive-api).
//!
//! Only the latest 10 timeline items of each room are cached and they are reset
//! whenever a new set of timeline items is received by the server.
//!
//! ## Bot mode
//!
//! _Note_: This is not yet exposed via the API. See [#1475](https://github.com/matrix-org/matrix-rust-sdk/issues/1475)
//!
//! Sliding Sync is modeled for faster and more efficient user-facing client
//! applications, but offers significant speed ups even for bot cases through
//! its filtering mechanism. The sort-order and specific subsets, however, are
//! usually not of interest for bots. For that use case the the
//! [`v4::SyncRequestList`][] offers the
//! [`slow_get_all_rooms`](`v4::SyncRequestList::slow_get_all_rooms`) flag.
//!
//! Once switched on, this mode will not trigger any updates on "list
//! movements", ranges and sorting are ignored and all rooms matching the filter
//! will be returned with the given room details settings. Depending on the data
//! that is requested this will still be significantly faster as the response
//! only returns the matching rooms and states as per settings.
//!
//! Think about a bot that only interacts in `is_dm = true` and doesn't need
//! room topic, room avatar and all the other state. It will be a lot faster to
//! start up and retrieve only the data needed to actually run.
//!
//! # Full example
//!
//! ```no_run
//! # use futures::executor::block_on;
//! use matrix_sdk::{Client, sliding_sync::{SlidingSyncViewBuilder, SlidingSyncMode}};
//! use ruma::{assign, {api::client::sync::sync_events::v4, events::StateEventType}};
//! use tracing::{warn, error, info, debug};
//! use futures::{StreamExt, pin_mut};
//! use futures_signals::{signal::SignalExt, signal_vec::SignalVecExt};
//! use url::Url;
//! # block_on(async {
//! # let homeserver = Url::parse("http://example.com")?;
//! # let client = Client::new(homeserver).await?;
//! let full_sync_view_name = "full-sync".to_owned();
//! let active_view_name = "active-view".to_owned();
//! let sliding_sync_builder = client
//!     .sliding_sync()
//!     .await
//!     .homeserver(Url::parse("http://sliding-sync.example.org")?) // our proxy server
//!     .with_common_extensions() // we want the e2ee and to-device enabled, please
//!     .cold_cache("example-cache".to_owned()); // we want these to be loaded from and stored into the persistent storage
//!
//! let full_sync_view = SlidingSyncViewBuilder::default()
//!     .sync_mode(SlidingSyncMode::GrowingFullSync)  // sync up by growing the window
//!     .name(&full_sync_view_name)    // needed to lookup again.
//!     .sort(vec!["by_recency".to_owned()]) // ordered by most recent
//!     .required_state(vec![
//!         (StateEventType::RoomEncryption, "".to_owned())
//!      ]) // only want to know if the room is encrypted
//!     .batch_size(50)   // grow the window by 50 items at a time
//!     .limit(500)      // only sync up the top 500 rooms
//!     .build()?;
//!
//! let active_view = SlidingSyncViewBuilder::default()
//!     .name(&active_view_name)   // the active window
//!     .sync_mode(SlidingSyncMode::Selective)  // sync up the specific range only
//!     .set_range(0u32, 9u32) // only the top 10 items
//!     .sort(vec!["by_recency".to_owned()]) // last active
//!     .timeline_limit(5u32) // add the last 5 timeline items for room preview and faster timeline loading
//!     .required_state(vec![ // we want to know immediately:
//!         (StateEventType::RoomEncryption, "".to_owned()), // is it encrypted
//!         (StateEventType::RoomTopic, "".to_owned()),      // any topic if known
//!         (StateEventType::RoomAvatar, "".to_owned()),     // avatar if set
//!      ])
//!     .build()?;
//!
//! let sliding_sync = sliding_sync_builder
//!     .add_view(active_view)
//!     .add_view(full_sync_view)
//!     .build()
//!     .await?;
//!
//!  // subscribe to the view APIs for updates
//!
//! let active_view = sliding_sync.view(&active_view_name).unwrap();
//! let view_state_stream = active_view.state_stream();
//! let view_count_stream = active_view.rooms_count_stream();
//! let view_list_stream = active_view.rooms_list_stream();
//!
//! tokio::spawn(async move {
//!     pin_mut!(view_state_stream);
//!     while let Some(new_state) = view_state_stream.next().await {
//!         info!("active-view switched state to {new_state:?}");
//!     }
//! });
//!
//! tokio::spawn(async move {
//!     pin_mut!(view_count_stream);
//!     while let Some(new_count) = view_count_stream.next().await {
//!         info!("active-view new count: {new_count:?}");
//!     }
//! });
//!
//! tokio::spawn(async move {
//!     pin_mut!(view_list_stream);
//!     while let Some(v_diff) = view_list_stream.next().await {
//!         info!("active-view rooms view diff update: {v_diff:?}");
//!     }
//! });
//!
//! let stream = sliding_sync.stream();
//!
//! // continuously poll for updates
//! pin_mut!(stream);
//! loop {
//!     let update = match stream.next().await {
//!         Some(Ok(u)) => {
//!             info!("Received an update. Summary: {u:?}");
//!         },
//!         Some(Err(e)) => {
//!              error!("loop was stopped by client error processing: {e}");
//!         }
//!         None => {
//!             error!("Streaming loop ended unexpectedly");
//!             break;
//!         }
//!     };
//! }
//!
//! # anyhow::Ok(())
//! # });
//! ```
//!
//!
//! [MSC]: https://github.com/matrix-org/matrix-spec-proposals/pull/3575
//! [proxy]: https://github.com/matrix-org/sliding-sync
//! [futures_signals]: https://docs.rs/futures-signals/latest/futures_signals/index.html
//! [ruma-types]: https://docs.rs/ruma/latest/ruma/api/client/sync/sync_events/v4/index.html
//! [future-signals-tutorial]: https://docs.rs/futures-signals/latest/futures_signals/tutorial/index.html

mod builder;
mod client;
mod error;
mod room;
mod view;

use std::{
    collections::BTreeMap,
    fmt::Debug,
    mem,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex, RwLock as StdRwLock,
    },
    time::Duration,
};

pub use builder::*;
pub use client::*;
pub use error::*;
use eyeball::Observable;
use futures_core::stream::Stream;
pub use room::*;
use ruma::{
    api::client::{
        error::ErrorKind,
        sync::sync_events::v4::{
            self, AccountDataConfig, E2EEConfig, ExtensionsConfig, ToDeviceConfig,
        },
    },
    assign, OwnedRoomId, RoomId,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info_span, instrument, trace, warn, Instrument, Span};
use url::Url;
pub use view::*;

use crate::{config::RequestConfig, Client, Result};

/// Number of times a Sliding Sync session can expire before raising an error.
///
/// A Sliding Sync session can expire. In this case, it is reset. However, to
/// avoid entering an infinite loop of “it's expired, let's reset, it's expired,
/// let's reset…” (maybe if the network has an issue, or the server, or anything
/// else), we define a maximum times a session can expire before
/// raising a proper error.
const MAXIMUM_SLIDING_SYNC_SESSION_EXPIRATION: u8 = 3;

/// The Sliding Sync instance.
#[derive(Clone, Debug)]
pub struct SlidingSync {
    /// Customize the homeserver for sliding sync only
    homeserver: Option<Url>,

    /// The HTTP Matrix client.
    client: Client,

    /// The storage key to keep this cache at and load it from
    storage_key: Option<String>,

    /// The `pos` marker.
    pos: Arc<StdRwLock<Observable<Option<String>>>>,

    delta_token: Arc<StdRwLock<Observable<Option<String>>>>,

    /// The views of this sliding sync instance
    views: Arc<StdRwLock<BTreeMap<String, SlidingSyncView>>>,

    /// The rooms details
    rooms: Arc<StdRwLock<BTreeMap<OwnedRoomId, SlidingSyncRoom>>>,

    subscriptions: Arc<StdRwLock<BTreeMap<OwnedRoomId, v4::RoomSubscription>>>,
    unsubscribe: Arc<StdRwLock<Vec<OwnedRoomId>>>,

    /// Number of times a Sliding Session session has been reset.
    reset_counter: Arc<AtomicU8>,

    /// the intended state of the extensions being supplied to sliding /sync
    /// calls. May contain the latest next_batch for to_devices, etc.
    extensions: Arc<Mutex<Option<ExtensionsConfig>>>,

    /// the last extensions known to be successfully sent to the server.
    /// if the current extensions match this, we can avoid sending them again.
    sent_extensions: Arc<Mutex<Option<ExtensionsConfig>>>,
}

#[derive(Serialize, Deserialize)]
struct FrozenSlidingSync {
    #[serde(skip_serializing_if = "Option::is_none")]
    to_device_since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta_token: Option<String>,
}

impl From<&SlidingSync> for FrozenSlidingSync {
    fn from(v: &SlidingSync) -> Self {
        FrozenSlidingSync {
            delta_token: v.delta_token.read().unwrap().clone(),
            to_device_since: v
                .extensions
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|ext| ext.to_device.as_ref()?.since.clone()),
        }
    }
}

impl SlidingSync {
    async fn cache_to_storage(&self) -> Result<(), crate::Error> {
        let Some(storage_key) = self.storage_key.as_ref() else { return Ok(()) };
        trace!(storage_key, "Saving to storage for later use");

        let store = self.client.store();

        // Write this `SlidingSync` instance, as a `FrozenSlidingSync` instance, inside
        // the client store.
        store
            .set_custom_value(
                storage_key.as_bytes(),
                serde_json::to_vec(&FrozenSlidingSync::from(self))?,
            )
            .await?;

        // Write every `SlidingSyncView` inside the client the store.
        let frozen_views = {
            let rooms_lock = self.rooms.read().unwrap();

            self.views
                .read()
                .unwrap()
                .iter()
                .map(|(name, view)| {
                    Ok((
                        format!("{storage_key}::{name}"),
                        serde_json::to_vec(&FrozenSlidingSyncView::freeze(view, &rooms_lock))?,
                    ))
                })
                .collect::<Result<Vec<_>, crate::Error>>()?
        };

        for (storage_key, frozen_view) in frozen_views {
            trace!(storage_key, "Saving the frozen Sliding Sync View");

            store.set_custom_value(storage_key.as_bytes(), frozen_view).await?;
        }

        Ok(())
    }

    /// Create a new [`SlidingSyncBuilder`].
    pub fn builder() -> SlidingSyncBuilder {
        SlidingSyncBuilder::new()
    }

    /// Generate a new [`SlidingSyncBuilder`] with the same inner settings and
    /// views but without the current state.
    pub fn new_builder_copy(&self) -> SlidingSyncBuilder {
        let mut builder = Self::builder()
            .client(self.client.clone())
            .subscriptions(self.subscriptions.read().unwrap().to_owned());

        for view in self.views.read().unwrap().values().map(|view| {
            view.new_builder().build().expect("builder worked before, builder works now")
        }) {
            builder = builder.add_view(view);
        }

        if let Some(h) = &self.homeserver {
            builder.homeserver(h.clone())
        } else {
            builder
        }
    }

    /// Subscribe to a given room.
    ///
    /// Note: this does not cancel any pending request, so make sure to only
    /// poll the stream after you've altered this. If you do that during, it
    /// might take one round trip to take effect.
    pub fn subscribe(&self, room_id: OwnedRoomId, settings: Option<v4::RoomSubscription>) {
        self.subscriptions.write().unwrap().insert(room_id, settings.unwrap_or_default());
    }

    /// Unsubscribe from a given room.
    ///
    /// Note: this does not cancel any pending request, so make sure to only
    /// poll the stream after you've altered this. If you do that during, it
    /// might take one round trip to take effect.
    pub fn unsubscribe(&self, room_id: OwnedRoomId) {
        if self.subscriptions.write().unwrap().remove(&room_id).is_some() {
            self.unsubscribe.write().unwrap().push(room_id);
        }
    }

    /// Add the common extensions if not already configured.
    pub fn add_common_extensions(&self) {
        let mut lock = self.extensions.lock().unwrap();
        let mut cfg = lock.get_or_insert_with(Default::default);

        if cfg.to_device.is_none() {
            cfg.to_device = Some(assign!(ToDeviceConfig::default(), { enabled: Some(true) }));
        }

        if cfg.e2ee.is_none() {
            cfg.e2ee = Some(assign!(E2EEConfig::default(), { enabled: Some(true) }));
        }

        if cfg.account_data.is_none() {
            cfg.account_data = Some(assign!(AccountDataConfig::default(), { enabled: Some(true) }));
        }
    }

    /// Lookup a specific room
    pub fn get_room(&self, room_id: &RoomId) -> Option<SlidingSyncRoom> {
        self.rooms.read().unwrap().get(room_id).cloned()
    }

    /// Check the number of rooms.
    pub fn get_number_of_rooms(&self) -> usize {
        self.rooms.read().unwrap().len()
    }

    fn update_to_device_since(&self, since: String) {
        self.extensions
            .lock()
            .unwrap()
            .get_or_insert_with(Default::default)
            .to_device
            .get_or_insert_with(Default::default)
            .since = Some(since);
    }

    /// Get access to the SlidingSyncView named `view_name`
    ///
    /// Note: Remember that this list might have been changed since you started
    /// listening to the stream and is therefor not necessarily up to date
    /// with the views used for the stream.
    pub fn view(&self, view_name: &str) -> Option<SlidingSyncView> {
        self.views.read().unwrap().get(view_name).cloned()
    }

    /// Remove the SlidingSyncView named `view_name` from the views list if
    /// found
    ///
    /// Note: Remember that this change will only be applicable for any new
    /// stream created after this. The old stream will still continue to use the
    /// previous set of views.
    pub fn pop_view(&self, view_name: &String) -> Option<SlidingSyncView> {
        self.views.write().unwrap().remove(view_name)
    }

    /// Add the view to the list of views
    ///
    /// As views need to have a unique `.name`, if a view with the same name
    /// is found the new view will replace the old one and the return it or
    /// `None`.
    ///
    /// Note: Remember that this change will only be applicable for any new
    /// stream created after this. The old stream will still continue to use the
    /// previous set of views.
    pub fn add_view(&self, view: SlidingSyncView) -> Option<SlidingSyncView> {
        self.views.write().unwrap().insert(view.name.clone(), view)
    }

    /// Lookup a set of rooms
    pub fn get_rooms<I: Iterator<Item = OwnedRoomId>>(
        &self,
        room_ids: I,
    ) -> Vec<Option<SlidingSyncRoom>> {
        let rooms = self.rooms.read().unwrap();

        room_ids.map(|room_id| rooms.get(&room_id).cloned()).collect()
    }

    /// Get all rooms.
    pub fn get_all_rooms(&self) -> Vec<SlidingSyncRoom> {
        self.rooms.read().unwrap().values().cloned().collect()
    }

    /// Handle the HTTP response.
    ///
    /// But which response? `v4::Response`, aka the Sliding Sync response, or
    /// `SyncResponse`? We have both because `SyncResponse` doesn't support
    /// Sliding Sync yet.
    #[instrument(skip_all, fields(views = views.len()))]
    async fn handle_response(
        &self,
        sliding_sync_response: v4::Response,
        extensions: Option<ExtensionsConfig>,
        views: &mut BTreeMap<String, SlidingSyncViewRequestGenerator>,
    ) -> Result<UpdateSummary, crate::Error> {
        // We may not need the `sync_response` in the future (once `SyncResponse` will
        // move to Sliding Sync, i.e. to `v4::Response`), but processing the
        // `sliding_sync_response` is vital, so it must be done somewhere; for now it
        // happens here.
        let mut sync_response = self.client.process_sliding_sync(&sliding_sync_response).await?;

        debug!("sliding sync response has been processed");

        Observable::set(&mut self.pos.write().unwrap(), Some(sliding_sync_response.pos));
        Observable::set(&mut self.delta_token.write().unwrap(), sliding_sync_response.delta_token);

        let update_summary = {
            let mut rooms = Vec::new();
            let mut rooms_map = self.rooms.write().unwrap();

            for (room_id, mut room_data) in sliding_sync_response.rooms.into_iter() {
                // `sync_response` contains the rooms with decrypted events if any, so look at
                // the timeline events here first if the room exists.
                // Otherwise, let's look at the timeline inside the `sliding_sync_response`.
                let timeline = if let Some(joined_room) = sync_response.rooms.join.remove(&room_id)
                {
                    joined_room.timeline.events
                } else {
                    room_data.timeline.drain(..).map(Into::into).collect()
                };

                if let Some(mut room) = rooms_map.remove(&room_id) {
                    // The room existed before, let's update it.

                    room.update(room_data, timeline);
                    rooms_map.insert(room_id.clone(), room);
                } else {
                    // First time we need this room, let's create it.

                    rooms_map.insert(
                        room_id.clone(),
                        SlidingSyncRoom::new(
                            self.client.clone(),
                            room_id.clone(),
                            room_data,
                            timeline,
                        ),
                    );
                }

                rooms.push(room_id);
            }

            let mut updated_views = Vec::new();

            for (name, updates) in sliding_sync_response.lists {
                let Some(generator) = views.get_mut(&name) else {
                    error!("Response for view `{name}` - unknown to us; skipping");

                    continue
                };

                let count: u32 =
                    updates.count.try_into().expect("the list total count convertible into u32");

                if generator.handle_response(count, &updates.ops, &rooms)? {
                    updated_views.push(name.clone());
                }
            }

            // Update the `to-device` next-batch if any.
            if let Some(to_device) = sliding_sync_response.extensions.to_device {
                self.update_to_device_since(to_device.next_batch);
            }

            // Track the most recently successfully sent extensions (needed for sticky
            // semantics).
            if extensions.is_some() {
                *self.sent_extensions.lock().unwrap() = extensions;
            }

            UpdateSummary { views: updated_views, rooms }
        };

        self.cache_to_storage().await?;

        Ok(update_summary)
    }

    async fn sync_once(
        &self,
        views: &mut BTreeMap<String, SlidingSyncViewRequestGenerator>,
    ) -> Result<Option<UpdateSummary>> {
        let mut lists_of_requests = BTreeMap::new();

        {
            let mut views_to_remove = Vec::new();

            for (name, generator) in views.iter_mut() {
                if let Some(request) = generator.next() {
                    lists_of_requests.insert(name.clone(), request);
                } else {
                    views_to_remove.push(name.clone());
                }
            }

            for view_name in views_to_remove {
                views.remove(&view_name);
            }
        }

        if views.is_empty() {
            return Ok(None);
        }

        let pos = self.pos.read().unwrap().clone();
        let delta_token = self.delta_token.read().unwrap().clone();
        let room_subscriptions = self.subscriptions.read().unwrap().clone();
        let unsubscribe_rooms = mem::take(&mut *self.unsubscribe.write().unwrap());
        let timeout = Duration::from_secs(30);

        // Implement stickiness by only sending extensions if they have
        // changed since the last time we sent them
        let extensions = {
            let extensions = self.extensions.lock().unwrap();

            if *extensions == *self.sent_extensions.lock().unwrap() {
                None
            } else {
                extensions.clone()
            }
        };

        debug!("Sending the sliding sync request");

        // Configure long-polling. We need 30 seconds for the long-poll itself, in
        // addition to 30 more extra seconds for the network delays.
        let request_config = RequestConfig::default().timeout(timeout + Duration::from_secs(30));

        // Prepare the request.
        let request = self.client.send_with_homeserver(
            assign!(v4::Request::new(), {
                lists: lists_of_requests,
                pos,
                delta_token,
                timeout: Some(timeout),
                room_subscriptions,
                unsubscribe_rooms,
                extensions: extensions.clone().unwrap_or_default(),
            }),
            Some(request_config),
            self.homeserver.as_ref().map(ToString::to_string),
        );

        // Send the request and get a response with end-to-end encryption support.
        //
        // Sending the `/sync` request out when end-to-end encryption is enabled means
        // that we need to also send out any outgoing e2ee related request out
        // coming from the `OlmMachine::outgoing_requests()` method.
        //
        // FIXME: Processing outgiong requests at the same time while a `/sync` is in
        // flight is currently not supported.
        // More info: [#1386](https://github.com/matrix-org/matrix-rust-sdk/issues/1386).
        #[cfg(feature = "e2e-encryption")]
        let response = {
            let (e2ee_uploads, response) =
                futures_util::future::join(self.client.send_outgoing_requests(), request).await;

            if let Err(error) = e2ee_uploads {
                error!(?error, "Error while sending outgoing E2EE requests");
            }

            response
        }?;

        // Send the request and get a response _without_ end-to-end encryption support.
        #[cfg(not(feature = "e2e-encryption"))]
        let response = request.await?;

        debug!("Sliding sync response received");

        let updates = self.handle_response(response, extensions, views).await?;

        debug!("Sliding sync response has been handled");

        Ok(Some(updates))
    }

    /// Create a _new_ Sliding Sync stream.
    ///
    /// This stream will send requests and will handle responses automatically,
    /// hence updating the views.
    #[instrument(name = "sync_stream", skip_all, parent = &self.client.root_span)]
    pub fn stream(&self) -> impl Stream<Item = Result<UpdateSummary, crate::Error>> + '_ {
        // Collect all the views that needsto be updated.
        let mut views = {
            let mut views = BTreeMap::new();
            let lock = self.views.read().unwrap();

            for (name, view) in lock.iter() {
                views.insert(name.clone(), view.request_generator());
            }

            views
        };

        debug!(?self.extensions, "About to run the sync stream");

        let instrument_span = Span::current();

        async_stream::stream! {
            loop {
                let sync_span = info_span!(parent: &instrument_span, "sync_once");

                sync_span.in_scope(|| {
                    debug!(?self.extensions, "Sync stream loop is running");
                });

                match self.sync_once(&mut views).instrument(sync_span.clone()).await {
                    Ok(Some(updates)) => {
                        self.reset_counter.store(0, Ordering::SeqCst);

                        yield Ok(updates);
                    }

                    Ok(None) => {
                        break;
                    }

                    Err(error) => {
                        if error.client_api_error_kind() == Some(&ErrorKind::UnknownPos) {
                            // The session has expired.

                            // Has it expired too many times?
                            if self.reset_counter.fetch_add(1, Ordering::SeqCst) >= MAXIMUM_SLIDING_SYNC_SESSION_EXPIRATION {
                                sync_span.in_scope(|| error!("Session expired {MAXIMUM_SLIDING_SYNC_SESSION_EXPIRATION} times in a row"));

                                // The session has expired too many times, let's raise an error!
                                yield Err(error.into());

                                break;
                            }

                            // Let's reset the Sliding Sync session.
                            sync_span.in_scope(|| {
                                warn!("Session expired. Restarting Sliding Sync.");

                                // To “restart” a Sliding Sync session, we set `pos` to its initial value.
                                Observable::set(&mut self.pos.write().unwrap(), None);

                                // We also need to reset our extensions to the last known good ones.
                                *self.extensions.lock().unwrap() = self.sent_extensions.lock().unwrap().take();

                                debug!(?self.extensions, "Sliding Sync has been reset");
                            });
                        }

                        yield Err(error.into());

                        continue;
                    }
                }
            }
        }
    }
}

#[cfg(any(test, feature = "testing"))]
impl SlidingSync {
    /// Get a copy of the `pos` value.
    pub fn pos(&self) -> Option<String> {
        self.pos.read().unwrap().clone()
    }

    /// Set a new value for `pos`.
    pub fn set_pos(&self, new_pos: String) {
        Observable::set(&mut self.pos.write().unwrap(), Some(new_pos));
    }
}

/// A summary of the updates received after a sync (like in
/// [`SlidingSync::stream`]).
#[derive(Debug, Clone)]
pub struct UpdateSummary {
    /// The names of the views that have seen an update.
    pub views: Vec<String>,
    /// The rooms that have seen updates
    pub rooms: Vec<OwnedRoomId>,
}

#[cfg(test)]
mod test {
    use ruma::room_id;
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn check_find_room_in_view() -> Result<()> {
        let view =
            SlidingSyncView::builder().name("testview").add_range(0u32, 9u32).build().unwrap();
        let full_window_update: v4::SyncOp = serde_json::from_value(json! ({
            "op": "SYNC",
            "range": [0, 9],
            "room_ids": [
                "!A00000:matrix.example",
                "!A00001:matrix.example",
                "!A00002:matrix.example",
                "!A00003:matrix.example",
                "!A00004:matrix.example",
                "!A00005:matrix.example",
                "!A00006:matrix.example",
                "!A00007:matrix.example",
                "!A00008:matrix.example",
                "!A00009:matrix.example"
            ],
        }))
        .unwrap();

        view.handle_response(10u32, &vec![full_window_update], &vec![(0, 9)], &vec![]).unwrap();

        let a02 = room_id!("!A00002:matrix.example").to_owned();
        let a05 = room_id!("!A00005:matrix.example").to_owned();
        let a09 = room_id!("!A00009:matrix.example").to_owned();

        assert_eq!(view.find_room_in_view(&a02), Some(2));
        assert_eq!(view.find_room_in_view(&a05), Some(5));
        assert_eq!(view.find_room_in_view(&a09), Some(9));

        assert_eq!(
            view.find_rooms_in_view(&[a02.clone(), a05.clone(), a09.clone()]),
            vec![(2, a02.clone()), (5, a05.clone()), (9, a09.clone())]
        );

        // we invalidate a few in the center
        let update: v4::SyncOp = serde_json::from_value(json! ({
            "op": "INVALIDATE",
            "range": [4, 7],
        }))
        .unwrap();

        view.handle_response(10u32, &vec![update], &vec![(0, 3), (8, 9)], &vec![]).unwrap();

        assert_eq!(view.find_room_in_view(room_id!("!A00002:matrix.example")), Some(2));
        assert_eq!(view.find_room_in_view(room_id!("!A00005:matrix.example")), None);
        assert_eq!(view.find_room_in_view(room_id!("!A00009:matrix.example")), Some(9));

        assert_eq!(
            view.find_rooms_in_view(&[a02.clone(), a05, a09.clone()]),
            vec![(2, a02), (9, a09)]
        );

        Ok(())
    }
}
