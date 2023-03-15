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
//! The protocol is split into three major sections for that: [lists][#lists],
//! the [room details](#rooms) and [extensions](#extensions), most notably the
//! end-to-end-encryption and to-device extensions to enable full
//! end-to-end-encryption support.
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
//! After the general configuration, one typically wants to add a list via the
//! [`add_list`][`SlidingSyncBuilder::add_list`] function.
//!
//! ## Lists
//!
//! A list defines a subset of matching rooms one wants to filter for, and be
//! kept up about. The [`v4::SyncRequestListFilters`][] allows for a granular
//! specification of the exact rooms one wants the server to select and the way
//! one wants them to be ordered before receiving. Secondly each list has a set
//! of `ranges`: the subset of indexes of the entire list one is interested in
//! and a unique name to be identified with.
//!
//! For example, a user might be part of thousands of rooms, but if the client
//! app always starts by showing the most recent direct message conversations,
//! loading all rooms is an inefficient approach. Instead with Sliding Sync one
//! defines a list (e.g. named `"main_list"`) filtering for `is_dm`, ordered
//! by recency and select to list the top 10 via `ranges: [ [0,9] ]` (indexes
//! are **inclusive**) like so:
//!
//! ```rust
//! # use matrix_sdk::sliding_sync::{SlidingSyncList, SlidingSyncMode};
//! use ruma::{assign, api::client::sync::sync_events::v4};
//!
//! let list_builder = SlidingSyncList::builder()
//!     .name("main_list")
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
//! [`SlidingSyncListBuilder`] for details on the filters, sort-order and
//! range-options and data one requests to be sent. Once the list is fully
//! configured, `build()` it and add the list to the sliding sync session
//! by supplying it to [`add_list`][`SlidingSyncBuilder::add_list`].
//!
//! Lists are inherently stateful and all updates are applied on the shared
//! list-object. Once a list has been added to [`SlidingSync`], a cloned shared
//! copy can be retrieved by calling `SlidingSync::list()`, providing the name
//! of the list. Next to the configuration settings (like name and
//! `timeline_limit`), the list provides the stateful
//! [`maximum_number_of_rooms`](SlidingSyncList::maximum_number_of_rooms),
//! [`rooms_list`](SlidingSyncList::rooms_list) and
//! [`state`](SlidingSyncList::state):
//!
//!  - `maximum_number_of_rooms` is the number of rooms _total_ there were found
//!    matching the filters given.
//!  - `rooms_list` is a vector of `maximum_number_of_rooms` [`RoomListEntry`]'s
//!    at the current state. `RoomListEntry`'s only hold `the room_id` if given,
//!    the [Rooms API](#rooms) holds the actual information about each room
//!  - `state` is a [`SlidingSyncMode`] signalling meta information about the
//!    list and its stateful data — whether this is the state loaded from local
//!    cache, whether the [full sync](#helper-lists) is in progress or whether
//!    this is the current live information
//!
//! These are updated upon every update received from the server. One can query
//! these for their current value at any time, or use the [Reactive API
//! to subscribe to changes](#reactive-api).
//!
//! ### Helper lists
//!
//! By default lists run in the [`Selective` mode](SlidingSyncMode::Selective).
//! That means one sets the desired range(s) to see explicitly (as described
//! above). Very often, one still wants to load up the entire room list in
//! background though. For that, the client implementation offers to run lists
//! in two additional full-sync-modes, which require additional configuration:
//!
//! - [`SlidingSyncMode::PagingFullSync`]: Pages through the entire list of
//!   rooms one request at a time asking for the next `batch_size` number of
//!   rooms up to the end or `limit` if configured
//! - [`SlidingSyncMode::GrowingFullSync`]: Grows the window by `batch_size` on
//!   every request till all rooms or until `limit` of rooms are in list.
//!
//! For both, one should configure
//! [`batch_size`](SlidingSyncListBuilder::batch_size) and optionally
//! [`limit`](SlidingSyncListBuilder::limit) on the [`SlidingSyncListBuilder`].
//! Both full-sync lists will notice if the number of rooms increased at runtime
//! and will attempt to catch up to that (barring the `limit`).
//!
//! ## Rooms
//!
//! Next to the room list, the details for rooms are the next important aspect.
//! Each [list](#lists) only references the [`OwnedRoomId`][ruma::OwnedRoomId]
//! of the room at the given position. The details (`required_state`s and
//! timeline items) requested by all lists are bundled, together with the common
//! details (e.g. whether it is a `dm` or its calculated name) and made
//! available on the Sliding Sync session struct as a [reactive](#reactive-api)
//! through [`.rooms`](SlidingSync::rooms), [`get_room`](SlidingSync::get_room)
//! and [`get_rooms`](SlidingSync::get_rooms) APIs.
//!
//! Notably, this map only knows about the rooms that have come down [Sliding
//! Sync protocol][MSC] and if the given room isn't in any active list range, it
//! may be stale. Additionally to selecting the room data via the room lists,
//! the [Sliding Sync protocol][MSC] allows to subscribe to specific rooms via
//! the [`subscribe()`](SlidingSync::subscribe). Any room subscribed to will
//! receive updates (with the given settings) regardless of whether they are
//! visible in any list. The most common case for using this API is when the
//! user enters a room - as we want to receive the incoming new messages
//! regardless of whether the room is pushed out of the lists room list.
//!
//! ### Room List Entries
//!
//! As the room list of each list is a vec of the `maximum_number_of_rooms` len
//! but a room may only know of a subset of entries for sure at any given time,
//! these entries are wrapped in [`RoomListEntry`][]. This type, in close
//! proximity to the [specification][MSC], can be either `Empty`, `Filled` or
//! `Invalidated`, signaling the state of each entry position.
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
//! Both the list configuration as well as the [room subscription
//! settings](`v4::RoomSubscription`) allow to specify a `timeline_limit` to
//! receive timeline events. If that is unset or set to 0, no events are sent by
//! the server (which is the default), if multiple limits are found, the highest
//! takes precedence. Any positive number indicates that on the first request a
//! room should come into list, up to that count of messages are sent
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
//! the list or room subscription (see [reactive api](#reactive-api)), Since
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
//! lists and `room_id`s of rooms. Note that, the same way that a list isn't
//! reacting if only the room data has changed (but not its position in its
//! list), the list won't be mentioned here either, only the `room_id`. So be
//! sure to look at both for all subscribed objects.
//!
//! In full, this typically looks like this:
//!
//! ```no_run
//! # use futures::executor::block_on;
//! # use futures::{pin_mut, StreamExt};
//! # use matrix_sdk::{
//! #    sliding_sync::{SlidingSyncMode, SlidingSyncListBuilder},
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
//!     // any lists you want are added here.
//!     .build()
//!     .await?;
//!
//! let stream = sliding_sync.stream();
//!
//! // continuously poll for updates
//! pin_mut!(stream);
//!
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
//! request with our updates, for example an update in a list's `range`.
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
//! # use matrix_sdk::{
//! #    sliding_sync::{SlidingSyncMode, SlidingSyncListBuilder, SlidingSync, Error},
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
//! #    // any lists you want are added here.
//! #    .build()
//! #    .await?;
//! use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
//!
//! struct MyRunner { lock: Arc<AtomicBool>, sliding_sync: SlidingSync };
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
//! [`lists`](`UpdateSummary.lists`) and [`rooms`](`UpdateSummary.rooms`) of
//! each [`UpdateSummary`] given by each stream iteration and update the local
//! copies accordingly. Because of where the loop sits in the stack, that can
//! be a bit tedious though, so lists and rooms have an additional way of
//! subscribing to updates via [`eyeball`].
//!
//! The `Timeline` one can receive per room by calling
//! [`.timeline()`][`SlidingSyncRoom::timeline`] will be populated with the
//! currently cached timeline events.
//!
//! ## Caching
//!
//! All room data, for filled but also _invalidated_ rooms, including the entire
//! timeline events as well as all list `room_lists` and
//! `maximum_number_of_rooms` are held in memory (unless one `pop`s the list
//! out).
//!
//! This is a purely in-memory cache layer though. If one wants Sliding Sync to
//! persist and load from cold (storage) cache, one needs to set its key with
//! [`cold_cache(name)`][`SlidingSyncBuilder::cold_cache`] and for each list
//! present at `.build()`[`SlidingSyncBuilder::build`] sliding sync will attempt
//! to load their latest cached version from storage, as well as some overall
//! information of Sliding Sync. If that succeeded the lists `state` has been
//! set to [`Preload`][SlidingSyncListState::Preload]. Only room data of rooms
//! present in one of the lists is loaded from storage.
//!
//! Once [#1441](https://github.com/matrix-org/matrix-rust-sdk/pull/1441) is merged
//! one can disable caching on a per-list basis by setting
//! [`cold_cache(false)`][`SlidingSyncListBuilder::cold_cache`] when
//! constructing the builder.
//!
//! Notice that lists added after Sliding Sync has been built **will not be
//! loaded from cache** regardless of their settings (as this could lead to
//! inconsistencies between lists). The same goes for any extension: some
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
//! use matrix_sdk::{Client, sliding_sync::{SlidingSyncList, SlidingSyncMode}};
//! use ruma::{assign, {api::client::sync::sync_events::v4, events::StateEventType}};
//! use tracing::{warn, error, info, debug};
//! use futures::{StreamExt, pin_mut};
//! use url::Url;
//! # block_on(async {
//! # let homeserver = Url::parse("http://example.com")?;
//! # let client = Client::new(homeserver).await?;
//! let full_sync_list_name = "full-sync".to_owned();
//! let active_list_name = "active-list".to_owned();
//! let sliding_sync_builder = client
//!     .sliding_sync()
//!     .await
//!     .homeserver(Url::parse("http://sliding-sync.example.org")?) // our proxy server
//!     .with_common_extensions() // we want the e2ee and to-device enabled, please
//!     .cold_cache("example-cache".to_owned()); // we want these to be loaded from and stored into the persistent storage
//!
//! let full_sync_list = SlidingSyncList::builder()
//!     .sync_mode(SlidingSyncMode::GrowingFullSync)  // sync up by growing the window
//!     .name(&full_sync_list_name)    // needed to lookup again.
//!     .sort(vec!["by_recency".to_owned()]) // ordered by most recent
//!     .required_state(vec![
//!         (StateEventType::RoomEncryption, "".to_owned())
//!      ]) // only want to know if the room is encrypted
//!     .full_sync_batch_size(50)   // grow the window by 50 items at a time
//!     .full_sync_maximum_number_of_rooms_to_fetch(500)      // only sync up the top 500 rooms
//!     .build()?;
//!
//! let active_list = SlidingSyncList::builder()
//!     .name(&active_list_name)   // the active window
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
//!     .add_list(active_list)
//!     .add_list(full_sync_list)
//!     .build()
//!     .await?;
//!
//!  // subscribe to the list APIs for updates
//!
//! let active_list = sliding_sync.list(&active_list_name).unwrap();
//! let list_state_stream = active_list.state_stream();
//! let list_count_stream = active_list.maximum_number_of_rooms_stream();
//! let list_stream = active_list.rooms_list_stream();
//!
//! tokio::spawn(async move {
//!     pin_mut!(list_state_stream);
//!     while let Some(new_state) = list_state_stream.next().await {
//!         info!("active-list switched state to {new_state:?}");
//!     }
//! });
//!
//! tokio::spawn(async move {
//!     pin_mut!(list_count_stream);
//!     while let Some(new_count) = list_count_stream.next().await {
//!         info!("active-list new count: {new_count:?}");
//!     }
//! });
//!
//! tokio::spawn(async move {
//!     pin_mut!(list_stream);
//!     while let Some(v_diff) = list_stream.next().await {
//!         info!("active-list rooms list diff update: {v_diff:?}");
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
//! [MSC]: https://github.com/matrix-org/matrix-spec-proposals/pull/3575
//! [proxy]: https://github.com/matrix-org/sliding-sync
//! [ruma-types]: https://docs.rs/ruma/latest/ruma/api/client/sync/sync_events/v4/index.html

mod builder;
mod client;
mod error;
mod list;
mod room;

use std::{
    borrow::BorrowMut,
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
use eyeball::unique::Observable;
use futures_core::stream::Stream;
pub use list::*;
use matrix_sdk_base::sync::SyncResponse;
use matrix_sdk_common::locks::Mutex as AsyncMutex;
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
use tokio::spawn;
use tracing::{debug, error, info_span, instrument, trace, warn, Instrument, Span};
use url::Url;
use uuid::Uuid;

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
///
/// It is OK to clone this type as much as you need: cloning it is cheap.
#[derive(Clone, Debug)]
pub struct SlidingSync {
    /// The Sliding Sync data.
    inner: Arc<SlidingSyncInner>,

    /// A lock to ensure that responses are handled one at a time.
    response_handling_lock: Arc<AsyncMutex<()>>,
}

#[derive(Debug)]
pub(super) struct SlidingSyncInner {
    /// Customize the homeserver for sliding sync only
    homeserver: Option<Url>,

    /// The HTTP Matrix client.
    client: Client,

    /// The storage key to keep this cache at and load it from
    storage_key: Option<String>,

    /// The `pos`  and `delta_token` markers.
    position: StdRwLock<SlidingSyncPositionMarkers>,

    /// The lists of this Sliding Sync instance.
    lists: StdRwLock<BTreeMap<String, SlidingSyncList>>,

    /// The rooms details
    rooms: StdRwLock<BTreeMap<OwnedRoomId, SlidingSyncRoom>>,

    subscriptions: StdRwLock<BTreeMap<OwnedRoomId, v4::RoomSubscription>>,
    unsubscribe: StdRwLock<Vec<OwnedRoomId>>,

    /// Number of times a Sliding Session session has been reset.
    reset_counter: AtomicU8,

    /// the intended state of the extensions being supplied to sliding /sync
    /// calls. May contain the latest next_batch for to_devices, etc.
    extensions: Mutex<Option<ExtensionsConfig>>,
}

impl SlidingSync {
    pub(super) fn new(inner: SlidingSyncInner) -> Self {
        Self { inner: Arc::new(inner), response_handling_lock: Arc::new(AsyncMutex::new(())) }
    }

    async fn cache_to_storage(&self) -> Result<(), crate::Error> {
        let Some(storage_key) = self.inner.storage_key.as_ref() else { return Ok(()) };
        trace!(storage_key, "Saving to storage for later use");

        let store = self.inner.client.store();

        // Write this `SlidingSync` instance, as a `FrozenSlidingSync` instance, inside
        // the client store.
        store
            .set_custom_value(
                storage_key.as_bytes(),
                serde_json::to_vec(&FrozenSlidingSync::from(self))?,
            )
            .await?;

        // Write every `SlidingSyncList` inside the client the store.
        let frozen_lists = {
            let rooms_lock = self.inner.rooms.read().unwrap();

            self.inner
                .lists
                .read()
                .unwrap()
                .iter()
                .map(|(name, list)| {
                    Ok((
                        format!("{storage_key}::{name}"),
                        serde_json::to_vec(&FrozenSlidingSyncList::freeze(list, &rooms_lock))?,
                    ))
                })
                .collect::<Result<Vec<_>, crate::Error>>()?
        };

        for (storage_key, frozen_list) in frozen_lists {
            trace!(storage_key, "Saving the frozen Sliding Sync list");

            store.set_custom_value(storage_key.as_bytes(), frozen_list).await?;
        }

        Ok(())
    }

    /// Create a new [`SlidingSyncBuilder`].
    pub fn builder() -> SlidingSyncBuilder {
        SlidingSyncBuilder::new()
    }

    /// Generate a new [`SlidingSyncBuilder`] with the same inner settings and
    /// lists but without the current state.
    pub fn new_builder_copy(&self) -> SlidingSyncBuilder {
        let mut builder = Self::builder()
            .client(self.inner.client.clone())
            .subscriptions(self.inner.subscriptions.read().unwrap().to_owned());

        for list in self.inner.lists.read().unwrap().values().map(|list| {
            list.new_builder().build().expect("builder worked before, builder works now")
        }) {
            builder = builder.add_list(list);
        }

        if let Some(homeserver) = &self.inner.homeserver {
            builder.homeserver(homeserver.clone())
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
        self.inner.subscriptions.write().unwrap().insert(room_id, settings.unwrap_or_default());
    }

    /// Unsubscribe from a given room.
    ///
    /// Note: this does not cancel any pending request, so make sure to only
    /// poll the stream after you've altered this. If you do that during, it
    /// might take one round trip to take effect.
    pub fn unsubscribe(&self, room_id: OwnedRoomId) {
        if self.inner.subscriptions.write().unwrap().remove(&room_id).is_some() {
            self.inner.unsubscribe.write().unwrap().push(room_id);
        }
    }

    /// Add the common extensions if not already configured.
    pub fn add_common_extensions(&self) {
        let mut lock = self.inner.extensions.lock().unwrap();
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
        self.inner.rooms.read().unwrap().get(room_id).cloned()
    }

    /// Check the number of rooms.
    pub fn get_number_of_rooms(&self) -> usize {
        self.inner.rooms.read().unwrap().len()
    }

    #[instrument(skip(self))]
    fn update_to_device_since(&self, since: String) {
        // FIXME: Find a better place where the to-device since token should be
        // persisted.
        self.inner
            .extensions
            .lock()
            .unwrap()
            .get_or_insert_with(Default::default)
            .to_device
            .get_or_insert_with(Default::default)
            .since = Some(since);
    }

    /// Get access to the SlidingSyncList named `list_name`.
    ///
    /// Note: Remember that this list might have been changed since you started
    /// listening to the stream and is therefor not necessarily up to date
    /// with the lists used for the stream.
    pub fn list(&self, list_name: &str) -> Option<SlidingSyncList> {
        self.inner.lists.read().unwrap().get(list_name).cloned()
    }

    /// Remove the SlidingSyncList named `list_name` from the lists list if
    /// found.
    ///
    /// Note: Remember that this change will only be applicable for any new
    /// stream created after this. The old stream will still continue to use the
    /// previous set of lists.
    pub fn pop_list(&self, list_name: &String) -> Option<SlidingSyncList> {
        self.inner.lists.write().unwrap().remove(list_name)
    }

    /// Add the list to the list of lists.
    ///
    /// As lists need to have a unique `.name`, if a list with the same name
    /// is found the new list will replace the old one and the return it or
    /// `None`.
    ///
    /// Note: Remember that this change will only be applicable for any new
    /// stream created after this. The old stream will still continue to use the
    /// previous set of lists.
    pub fn add_list(&self, list: SlidingSyncList) -> Option<SlidingSyncList> {
        self.inner.lists.write().unwrap().insert(list.name.clone(), list)
    }

    /// Lookup a set of rooms
    pub fn get_rooms<I: Iterator<Item = OwnedRoomId>>(
        &self,
        room_ids: I,
    ) -> Vec<Option<SlidingSyncRoom>> {
        let rooms = self.inner.rooms.read().unwrap();

        room_ids.map(|room_id| rooms.get(&room_id).cloned()).collect()
    }

    /// Get all rooms.
    pub fn get_all_rooms(&self) -> Vec<SlidingSyncRoom> {
        self.inner.rooms.read().unwrap().values().cloned().collect()
    }

    fn prepare_extension_config(&self, pos: Option<&str>) -> ExtensionsConfig {
        if pos.is_none() {
            // The pos is `None`, it's either our initial sync or the proxy forgot about us
            // and sent us an `UnknownPos` error. We need to send out the config for our
            // extensions.
            let mut extensions = self.inner.extensions.lock().unwrap().clone().unwrap_or_default();

            // Always enable to-device events and the e2ee-extension on the initial request,
            // no matter what the caller wants.
            //
            // The to-device `since` parameter is either `None` or guaranteed to be set
            // because the `update_to_device_since()` method updates the
            // self.extensions field and they get persisted to the store using the
            // `cache_to_storage()` method.
            //
            // The token is also loaded from storage in the `SlidingSyncBuilder::build()`
            // method.
            let mut to_device = extensions.to_device.unwrap_or_default();
            to_device.enabled = Some(true);

            extensions.to_device = Some(to_device);
            extensions.e2ee = Some(assign!(E2EEConfig::default(), { enabled: Some(true) }));

            extensions
        } else {
            // We already enabled all the things, just fetch out the to-device since token
            // out of self.extensions and set it in a new, and empty, `ExtensionsConfig`.
            let since = self
                .inner
                .extensions
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|e| e.to_device.as_ref()?.since.to_owned());

            let mut extensions: ExtensionsConfig = Default::default();
            extensions.to_device = Some(assign!(ToDeviceConfig::default(), { since }));

            extensions
        }
    }

    /// Handle the HTTP response.
    #[instrument(skip_all, fields(lists = list_generators.len()))]
    fn handle_response(
        &self,
        sliding_sync_response: v4::Response,
        mut sync_response: SyncResponse,
        list_generators: &mut BTreeMap<String, SlidingSyncListRequestGenerator>,
    ) -> Result<UpdateSummary, crate::Error> {
        {
            debug!(
                pos = ?sliding_sync_response.pos,
                delta_token = ?sliding_sync_response.delta_token,
                "Update position markers`"
            );

            let mut position_lock = self.inner.position.write().unwrap();
            Observable::set(&mut position_lock.pos, Some(sliding_sync_response.pos));
            Observable::set(&mut position_lock.delta_token, sliding_sync_response.delta_token);
        }

        let update_summary = {
            let mut updated_rooms = Vec::new();
            let mut rooms_map = self.inner.rooms.write().unwrap();

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
                            self.inner.client.clone(),
                            room_id.clone(),
                            room_data,
                            timeline,
                        ),
                    );
                }

                updated_rooms.push(room_id);
            }

            let mut updated_lists = Vec::new();

            for (name, updates) in sliding_sync_response.lists {
                let Some(list_generator) = list_generators.get_mut(&name) else {
                    error!("Response for list `{name}` - unknown to us; skipping");

                    continue
                };

                let maximum_number_of_rooms: u32 =
                    updates.count.try_into().expect("the list total count convertible into u32");

                if list_generator.handle_response(
                    maximum_number_of_rooms,
                    &updates.ops,
                    &updated_rooms,
                )? {
                    updated_lists.push(name.clone());
                }
            }

            // Update the `to-device` next-batch if any.
            if let Some(to_device) = sliding_sync_response.extensions.to_device {
                self.update_to_device_since(to_device.next_batch);
            }

            UpdateSummary { lists: updated_lists, rooms: updated_rooms }
        };

        Ok(update_summary)
    }

    async fn sync_once(
        &self,
        stream_id: &str,
        list_generators: Arc<Mutex<BTreeMap<String, SlidingSyncListRequestGenerator>>>,
    ) -> Result<Option<UpdateSummary>> {
        let mut lists = BTreeMap::new();

        {
            let mut list_generators_lock = list_generators.lock().unwrap();
            let list_generators = list_generators_lock.borrow_mut();
            let mut lists_to_remove = Vec::new();

            for (name, generator) in list_generators.iter_mut() {
                if let Some(request) = generator.next() {
                    lists.insert(name.clone(), request);
                } else {
                    lists_to_remove.push(name.clone());
                }
            }

            for list_name in lists_to_remove {
                list_generators.remove(&list_name);
            }

            if list_generators.is_empty() {
                return Ok(None);
            }
        }

        let (pos, delta_token) = {
            let position_lock = self.inner.position.read().unwrap();

            (position_lock.pos.clone(), position_lock.delta_token.clone())
        };

        let room_subscriptions = self.inner.subscriptions.read().unwrap().clone();
        let unsubscribe_rooms = mem::take(&mut *self.inner.unsubscribe.write().unwrap());
        let timeout = Duration::from_secs(30);
        let extensions = self.prepare_extension_config(pos.as_deref());

        debug!("Sending the sliding sync request");

        // Configure long-polling. We need 30 seconds for the long-poll itself, in
        // addition to 30 more extra seconds for the network delays.
        let request_config = RequestConfig::default().timeout(timeout + Duration::from_secs(30));

        // Prepare the request.
        let request = assign!(v4::Request::new(), {
            pos,
            delta_token,
            // We want to track whether the incoming response maps to this
            // request. We use the (optional) `txn_id` field for that.
            txn_id: Some(stream_id.to_owned()),
            timeout: Some(timeout),
            lists,
            room_subscriptions,
            unsubscribe_rooms,
            extensions,
        });

        debug!(?request, "Sliding Sync will send this request");

        let request = self.inner.client.send_with_homeserver(
            request,
            Some(request_config),
            self.inner.homeserver.as_ref().map(ToString::to_string),
        );

        // Send the request and get a response with end-to-end encryption support.
        //
        // Sending the `/sync` request out when end-to-end encryption is enabled means
        // that we need to also send out any outgoing e2ee related request out
        // coming from the `OlmMachine::outgoing_requests()` method.
        #[cfg(feature = "e2e-encryption")]
        let response = {
            debug!("Sliding Sync is sending the request _with_ E2EE");

            let (e2ee_uploads, response) =
                futures_util::future::join(self.inner.client.send_outgoing_requests(), request)
                    .await;

            if let Err(error) = e2ee_uploads {
                error!(?error, "Error while sending outgoing E2EE requests");
            }

            response
        }?;

        // Send the request and get a response _without_ end-to-end encryption support.
        #[cfg(not(feature = "e2e-encryption"))]
        let response = {
            debug!("Sliding Sync is sending the request _without_ E2EE");

            request.await?
        };

        debug!(?response, "Sliding Sync response received");

        // At this point, the request has been sent, and a response has been received.
        //
        // We must ensure the handling of the response cannot be stopped/
        // cancelled. It must be done entirely, otherwise we can have
        // corrupted/incomplete states for Sliding Sync and other parts of
        // the code.
        //
        // That's why we are running the handling of the response in a spawned
        // future that cannot be cancelled by anything.
        let this = self.clone();
        let stream_id = stream_id.to_owned();

        // Spawn a new future to ensure that the code inside this future cannot be
        // cancelled if this method is cancelled.
        spawn(async move {
            debug!("Sliding Sync response handing starts");

            // In case the task running this future is detached, we must be
            // ensured responses are handled one at a time, hence we lock the
            // `response_handling_lock`.
            let response_handling_lock = this.response_handling_lock.lock().await;

            match &response.txn_id {
                None => {
                    error!(stream_id, "Sliding Sync has received an unexpected response: `txn_id` must match `stream_id`; it's missing");
                }

                Some(txn_id) if txn_id != &stream_id => {
                    error!(
                        stream_id,
                        txn_id,
                        "Sliding Sync has received an unexpected response: `txn_id` must match `stream_id`; they differ"
                    );
                }

                _ => {}
            }

            // Handle and transform a Sliding Sync Response to a `SyncResponse`.
            //
            // We may not need the `sync_response` in the future (once `SyncResponse` will
            // move to Sliding Sync, i.e. to `v4::Response`), but processing the
            // `sliding_sync_response` is vital, so it must be done somewhere; for now it
            // happens here.
            let sync_response = this.inner.client.process_sliding_sync(&response).await?;

            debug!(?sync_response, "Sliding Sync response has been processed");

            let updates = this.handle_response(response, sync_response, list_generators.lock().unwrap().borrow_mut())?;

            this.cache_to_storage().await?;

            // Release the lock.
            drop(response_handling_lock);

            debug!("Sliding sync response has been handled");

            Ok(Some(updates))
        }).await.unwrap()
    }

    /// Create a _new_ Sliding Sync stream.
    ///
    /// This stream will send requests and will handle responses automatically,
    /// hence updating the lists.
    #[allow(unknown_lints, clippy::let_with_type_underscore)] // triggered by instrument macro
    #[instrument(name = "sync_stream", skip_all, parent = &self.inner.client.inner.root_span)]
    pub fn stream<'a>(&'a self) -> impl Stream<Item = Result<UpdateSummary, crate::Error>> + 'a {
        // Collect all the lists that need to be updated.
        let list_generators = {
            let mut list_generators = BTreeMap::new();
            let lock = self.inner.lists.read().unwrap();

            for (name, lists) in lock.iter() {
                list_generators.insert(name.clone(), lists.request_generator());
            }

            list_generators
        };

        let stream_id = Uuid::new_v4().to_string();

        debug!(?self.inner.extensions, stream_id, "About to run the sync stream");

        let instrument_span = Span::current();
        let list_generators = Arc::new(Mutex::new(list_generators));

        async_stream::stream! {
            loop {
                let sync_span = info_span!(parent: &instrument_span, "sync_once");

                sync_span.in_scope(|| {
                    debug!(?self.inner.extensions, "Sync stream loop is running");
                });

                match self.sync_once(&stream_id, list_generators.clone()).instrument(sync_span.clone()).await {
                    Ok(Some(updates)) => {
                        self.inner.reset_counter.store(0, Ordering::SeqCst);

                        yield Ok(updates);
                    }

                    Ok(None) => {
                        break;
                    }

                    Err(error) => {
                        if error.client_api_error_kind() == Some(&ErrorKind::UnknownPos) {
                            // The session has expired.

                            // Has it expired too many times?
                            if self.inner.reset_counter.fetch_add(1, Ordering::SeqCst) >= MAXIMUM_SLIDING_SYNC_SESSION_EXPIRATION {
                                sync_span.in_scope(|| error!("Session expired {MAXIMUM_SLIDING_SYNC_SESSION_EXPIRATION} times in a row"));

                                // The session has expired too many times, let's raise an error!
                                yield Err(error.into());

                                break;
                            }

                            // Let's reset the Sliding Sync session.
                            sync_span.in_scope(|| {
                                warn!("Session expired. Restarting Sliding Sync.");

                                // To “restart” a Sliding Sync session, we set `pos` to its initial value.
                                {
                                    let mut position_lock = self.inner.position.write().unwrap();

                                    Observable::set(&mut position_lock.pos, None);
                                }

                                debug!(?self.inner.extensions, "Sliding Sync has been reset");
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
        let position_lock = self.inner.position.read().unwrap();

        position_lock.pos.clone()
    }

    /// Set a new value for `pos`.
    pub fn set_pos(&self, new_pos: String) {
        let mut position_lock = self.inner.position.write().unwrap();

        Observable::set(&mut position_lock.pos, Some(new_pos));
    }
}

#[derive(Debug)]
pub(super) struct SlidingSyncPositionMarkers {
    pos: Observable<Option<String>>,
    delta_token: Observable<Option<String>>,
}

#[derive(Serialize, Deserialize)]
struct FrozenSlidingSync {
    #[serde(skip_serializing_if = "Option::is_none")]
    to_device_since: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delta_token: Option<String>,
}

impl From<&SlidingSync> for FrozenSlidingSync {
    fn from(sliding_sync: &SlidingSync) -> Self {
        FrozenSlidingSync {
            delta_token: sliding_sync.inner.position.read().unwrap().delta_token.clone(),
            to_device_since: sliding_sync
                .inner
                .extensions
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|ext| ext.to_device.as_ref()?.since.clone()),
        }
    }
}

/// A summary of the updates received after a sync (like in
/// [`SlidingSync::stream`]).
#[derive(Debug, Clone)]
pub struct UpdateSummary {
    /// The names of the lists that have seen an update.
    pub lists: Vec<String>,
    /// The rooms that have seen updates
    pub rooms: Vec<OwnedRoomId>,
}

#[cfg(test)]
mod test {
    use assert_matches::assert_matches;
    use ruma::{room_id, uint};
    use serde_json::json;
    use wiremock::MockServer;

    use super::*;
    use crate::test_utils::logged_in_client;

    #[tokio::test]
    async fn check_find_room_in_list() -> Result<()> {
        let list =
            SlidingSyncList::builder().name("testlist").add_range(0u32, 9u32).build().unwrap();
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

        list.handle_response(
            10u32,
            &vec![full_window_update],
            &vec![(uint!(0), uint!(9))],
            &vec![],
        )
        .unwrap();

        let a02 = room_id!("!A00002:matrix.example").to_owned();
        let a05 = room_id!("!A00005:matrix.example").to_owned();
        let a09 = room_id!("!A00009:matrix.example").to_owned();

        assert_eq!(list.find_room_in_list(&a02), Some(2));
        assert_eq!(list.find_room_in_list(&a05), Some(5));
        assert_eq!(list.find_room_in_list(&a09), Some(9));

        assert_eq!(
            list.find_rooms_in_list(&[a02.clone(), a05.clone(), a09.clone()]),
            vec![(2, a02.clone()), (5, a05.clone()), (9, a09.clone())]
        );

        // we invalidate a few in the center
        let update: v4::SyncOp = serde_json::from_value(json! ({
            "op": "INVALIDATE",
            "range": [4, 7],
        }))
        .unwrap();

        list.handle_response(
            10u32,
            &vec![update],
            &vec![(uint!(0), uint!(3)), (uint!(8), uint!(9))],
            &vec![],
        )
        .unwrap();

        assert_eq!(list.find_room_in_list(room_id!("!A00002:matrix.example")), Some(2));
        assert_eq!(list.find_room_in_list(room_id!("!A00005:matrix.example")), None);
        assert_eq!(list.find_room_in_list(room_id!("!A00009:matrix.example")), Some(9));

        assert_eq!(
            list.find_rooms_in_list(&[a02.clone(), a05, a09.clone()]),
            vec![(2, a02), (9, a09)]
        );

        Ok(())
    }

    #[tokio::test]
    async fn to_device_is_enabled_when_pos_is_none() -> Result<()> {
        let server = MockServer::start().await;
        let client = logged_in_client(Some(server.uri())).await;

        let sync = client.sliding_sync().await.build().await?;
        let extensions = sync.prepare_extension_config(None);

        // If the user doesn't provide any extension config, we enable to-device and
        // e2ee anyways.
        assert_matches!(
            extensions.to_device,
            Some(ToDeviceConfig { enabled: Some(true), since: None, .. })
        );
        assert_matches!(extensions.e2ee, Some(E2EEConfig { enabled: Some(true), .. }));

        let some_since = "some_since".to_owned();
        sync.update_to_device_since(some_since.to_owned());
        let extensions = sync.prepare_extension_config(Some("foo"));

        // If there's a `pos` and to-device `since` token, we make sure we put the token
        // into the extension config. The rest doesn't need to be re-enabled due to
        // stickyness.
        assert_matches!(
            extensions.to_device,
            Some(ToDeviceConfig { enabled: None, since: Some(since), .. }) if since == some_since
        );
        assert_matches!(extensions.e2ee, None);

        let extensions = sync.prepare_extension_config(None);
        // Even if there isn't a `pos`, if we have a to-device `since` token, we put it
        // into the request.
        assert_matches!(
            extensions.to_device,
            Some(ToDeviceConfig { enabled: Some(true), since: Some(since), .. }) if since == some_since
        );

        Ok(())
    }
}
