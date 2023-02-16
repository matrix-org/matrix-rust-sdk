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
//! matrix with a strong focus on bandwidth efficiency. This is made possible by
//! allowing the client to filter the content very specifically in its request
//! which as a result allows the server to reduce the data sent to the
//! absolute necessary minimum needed. The API is modeled after common patterns
//! and UI components end user messenger client typically offer. By allowing a
//! tight coupling of what the client shows and synchronizing that state over
//! the protocol to the server, the server always sends exactly the information
//! necessary for the currently displayed subset for the user rather than
//! filling the connection with data the user isn't interested in right now.
//!
//! Sliding Sync is a live-protocol using [long-polling](#long-polling) http(s)
//! connections to stay up to date. On the client side these updates are applied
//! and propagated through an [asynchronous reactive API](#reactive-api)
//! implemented with [`futures_signals`][futures_signals].
//!
//! The protocol is split into three major sections for that: room
//! lists or [views](#views), the [room details](#rooms) and
//! [extensions](#extensions), most notably the end-to-end-encryption and
//! to-device extensions to enable full end-to-end-encryption support.
//!
//! ## Starting up
//!
//! To create a new sliding-sync-session, you must query your existing
//! (authenticated) `Client` for a new [`SlidingSyncBuilder`] by calling
//! [`sliding_sync`](`super::Client::sliding_sync`) on client. The
//! [`SlidingSyncBuilder`] is the baseline configuration to create a
//! [`SlidingSync`]-session by calling `.build()` once everything is ready.
//! Typically one configures the custom homeserver endpoint. at the time of
//! writing no matrix server natively supports sliding sync but a sidecar called
//! the [Sliding Sync Proxy][proxy] is needed. As that typically runs one a
//! separate domain, it can be configured on the [`SlidingSyncBuilder`]:
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
//! After the general configuration, you typically want to add a view via the
//! [`add_view`][`SlidingSyncBuilder::add_view`] function.
//!
//! ## Views
//!
//! A view defines the subset of matching rooms you want to filter for, and be
//! kept up about. The [`v4::SyncRequestListFilters`][] allows for a granular
//! specification of the exact rooms you want the server to select and the way
//! you want them to be ordered before receiving. Secondly each view has a set
//! of `ranges`: the subset of indexes of the entire list you are interested in
//! and a unique name to be identified with.
//!
//! For example, a user might be part of thousands of rooms, but if your App
//! always starts by showing the most recent direct message conversations,
//! loading all rooms is an inefficient approach. Instead with sliding sync you
//! define yourself a view (named `"main_view"`) filtering for `is_dm`, ordered
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
//! range-options and data you request to be sent. Once your view is fully
//! configured you can issue the view builder to `build()` it and add the view
//! to the sliding sync session by supplying it to
//! [`add_view`][`SlidingSyncBuilder::add_view`].
//!
//! Views are inherently stateful and all updates are applied on the shared
//! view-object. Once a view has been added to [`SlidingSync`] a cloned shared
//! copy can be retrieved by calls `SlidingSync::view()` providing the name of
//! the view. Next to the configuration settings (like name and
//! `timeline_limit`), the view provides the stateful
//! [`rooms_count`](SlidingSyncView::rooms_count),
//! [`rooms_list`](SlidingSyncView::rooms_list) and
//! [`state`](SlidingSyncView::state):
//!
//!  - `rooms_count` is the number of rooms _total_ there were found matching
//!    the filters given.
//!  - `rooms_list` is a vector of `rooms_count` [`RoomListEntry`]'s at the
//!    current its current state. `RoomListEntry`'s only hold `the room_id` if
//!    given, the [Rooms API](#rooms) holds the actual information about each
//!    room
//!  - `state` is a [`SlidingSyncMode`] signalling meta information about the
//!    view and its stateful data - whether this is the state loaded from local
//!    cache, whether the [full sync](#helper-views) is in progress or whether
//!    this is the current live information
//!
//! These are update upon every update received from the server. You can query
//! these for their current value at any time, or use the [Reactive API
//! to subscribe to changes](#reactive-api).
//!
//! ### Helper Views
//!
//! By default views run in the [`Selective`-Mode](SlidingSyncMode::Selective).
//! That means you set the range(s) you want to see explicitly (as described
//! above). Very often you still want to load up the entire room list in
//! background though. For that the client implementation offers to run views in
//! two additional full-sync-modes, which require additional configuration:
//!
//! - [`SlidingSyncMode::PagingFullSync`]: Pages through the entire list of
//!   rooms one request at a time asking for the next `batch_size` number of
//!   rooms up to the end or `limit` if configured
//! - [`SlidingSyncMode::GrowingFullSync`]: Grows the window by `batch_size` on
//!   every request till all rooms or until `limit` of rooms are in view.
//!
//! For both one should configure
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
//! available on the sliding sync session struct as a [reactive](#reactive-api)
//! through [`.rooms`](SlidingSync::rooms), [`get_room`](SlidingSync::get_room)
//! and [`get_rooms`](SlidingSync::get_rooms) APIs.
//!
//! Notably, this map only knows about the rooms that have come down [sliding
//! sync protocol][MSC] and if the given room isn't in any active view range, it
//! may be stale. Additionally to selecting the room data via the room lists,
//! the [sliding sync protocol][MSC] allows to subscribe to specific rooms via
//! the [`subscribe()`](SlidingSync::subscribe). Any room subscribed to will
//! receive updates (with the given Settings) regardless of whether they are
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
//! - `Empty` should be self-explanatory: we don't know what sits here at this
//!   position in the list
//! - `Filled`, too is pretty clear: there is this room_id at this position;
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
//! All timeline events coming through sliding sync will be processed through
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
//! you've received data from the server, you re-open the network connection
//! again and await for a new response. As there might not be happening much or
//! a lot happening in short succession - from the client perspective we never
//! know when new data is received.
//!
//! One principle of long-polling is, therefore, that it might also takes one
//! or two requests before the changes you asked for will actually be applied
//! and the results come back for that. Just assume that at the same time you
//! add a room subscription, a new message comes in. The server might reply
//! with that message immediately and will only kick off the process of
//! calculating the rooms details and respond with that in the next request you
//! do after.
//!
//! This is modelled as a [async `Stream`][`futures_core::stream::Stream`] in
//! our API, that you basically want to continue polling. Once you've made your
//! setup ready and build your sliding sync sessions, you want to acquire its
//! [`.stream()`](`SlidingSync::stream`) and continuously poll it.
//!
//! While the async stream API allows for streams to end (by returning `None`)
//! sliding sync stream items `Result<UpdateSummary, Error>`. For every
//! successful poll, all data is applied internally, through the base client and
//! the [reactive structs](#reactive-api) and an
//! [`Ok(UpdateSummary)`][`UpdateSummary`] is yielded with the minimum
//! information, which data has been refreshed _in this iteration_: names of
//! views and room_ids of rooms. Note that, the same way that a view isn't
//! reacting if only the room data has changed (but not its position in its
//! list), the view won't be mentioned here either, only the `room_id`. So be
//! sure to look at both for all objects you have subscribed to.
//!
//! In full this typically looks like this:
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
//! #    api::client::sync::sync_events::v4, assign, events::TimelineEventType,
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
//! A main purpose of [sliding sync][MSC] is provide an API for snappy end user
//! applications. Long-polling on the other side means that we wait for the
//! server to respond and that can take quite some time, before sending the next
//! request with our updates, for example an update in a view's `range`.
//!
//! That is a bit unfortunate and leaks through the `stream` API as well. We are
//! waiting for a `stream.next().await` call before the next request is sent.
//! The [specification][MSC] on long polling also states, however, that if an
//! new request is found coming in, the previous one shall be sent out. In
//! practice that means you can just start a new stream and the old connection
//! will return immediately - with a proper response though. You just need to
//! make sure to not call that stream any further. Additionally, as both
//! requests are sent with the same positional argument, the server might
//! respond with data, the client has already processed. This isn't a problem,
//! the [`SlidingSync`][] will only process new data and skip the processing
//! even across restarts.
//!
//! To support this, in practice you probably want to wrap your `loop` in a
//! spawn with an atomic flag that tells it to stop, which you can set upon
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
//! #    api::client::sync::sync_events::v4, assign, events::TimelineEventType,
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
//! As already touched on in
//! description of [views](#views), their `state`, `rooms_list` and
//! `rooms_count` are all various forms of `futures_signals::Mutable`, a
//! low-cost, thread-safe futures based reactive API implementation.
//! [`SlidingSync.rooms`][], too, are of a mutable implementation, namely the
//! [`MutableBTreeMap`](`futures_signals::signal_map::MutableBTreeMap`) updated
//! in real time (even before the summary comes around) allowing you to
//! subscribe to their changes with a straight forward async API through the
//! [`SignalExt`](`futures_signals::signal::SignalExt`) you can get from each by
//! just calling `signal_cloned()` on it. For the most common use-cases you just
//! want to have a stream of updates of the value you can poll for changes,
//! which you then get by calling
//! [`to_stream()`](`futures_signals::signal::SignalExt::to_stream`). You can do
//! a lot more on the signal itself already, like
//! [`map`](`futures_signals::signal::SignalExt::map`),
//! [`for_each`](`futures_signals::signal::SignalExt::for_each`) or convert it
//! into a [`Broadcaster`](futures_signals::signal::SignalExt::broadcast)
//! depending on your needs.
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
//! currently cached timeline events. It itself uses the `future_signals` for
//! reactivity, too.
//!
//! 👉 To learn more about [`future_signals` check out to their excellent
//! tutorial][future-signals-tutorial].
//!
//! ## Caching
//!
//! All room data, for filled but also _invalidated_ rooms, including the entire
//! timeline events as well as all view room_lists and rooms_count are held
//! in memory (unless you `pop` the view out). Technically, you can access
//! `rooms_list` and `rooms` directly and mutate them but doing so invalidates
//! further updates received by the server - see [#1474][https://github.com/matrix-org/matrix-rust-sdk/issues/1474].
//!
//! This is a purely in-memory cache layer though. If you want sliding sync to
//! persist and load from cold (storage) cache you need to set its key with
//! [`cold_cache(name)`][`SlidingSyncBuilder::cold_cache`] and for each view
//! present at `.build()`[`SlidingSyncBuilder::build`] sliding sync will attempt
//! to load their latest cached version from storage, as well as some overall
//! information of sliding sync. If that succeeded the views `state` has been
//! set to [`Preload`][SlidingSyncViewState::Preload]. Only room data of rooms
//! present in one of the views is loaded from storage.
//!
//! Once [#1441](https://github.com/matrix-org/matrix-rust-sdk/pull/1441) is merged
//! you can disable caching on a per-view basis by setting
//! [`cold_cache(false)`][`SlidingSyncViewBuilder::cold_cache`] when
//! constructing the builder.
//!
//! Notice that views added after sliding sync has been built **will not be
//! loaded from cache** regardless of their settings (as this could lead to
//! inconsistencies between views). The same goes for any extension: some
//! extension data (like the to-device-message position) are stored to storage,
//! but only retrieved upon `build()` of the `SlidingSyncBuilder`. So if you
//! only add them later, they will not be reading the data from storage (to
//! avoid inconsistencies) and might require more data to be sent in their first
//! request than if they were loaded form cold-cache.
//!
//! When loading from storage `rooms_list` entries found are set to
//! `Invalidated` - the initial setting here is communicated as a single
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
//! [`slow_get_all_rooms`](`v4::SyncRequestList::slow_get_all_rooms`)-flag.
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
//! use ruma::{assign, {api::client::sync::sync_events::v4, events::TimelineEventType}};
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
//!         (TimelineEventType::RoomEncryption, "".to_owned())
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
//!         (TimelineEventType::RoomEncryption, "".to_owned()), // is it encrypted
//!         (TimelineEventType::RoomTopic, "".to_owned()),      // any topic if known
//!         (TimelineEventType::RoomAvatar, "".to_owned()),     // avatar if set
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
//! let view_state_stream = active_view.state.signal_cloned().to_stream();
//! let view_count_stream = active_view.rooms_count.signal_cloned().to_stream();
//! let view_list_stream = active_view.rooms_list.signal_vec_cloned().to_stream();
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

use std::{
    collections::BTreeMap,
    fmt::Debug,
    ops::{Deref, Not},
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use futures_core::stream::Stream;
use futures_signals::{
    signal::Mutable,
    signal_map::{MutableBTreeMap, MutableBTreeMapLockRef},
    signal_vec::{MutableVec, MutableVecLockMut},
};
use matrix_sdk_base::{deserialized_responses::SyncTimelineEvent, sync::SyncResponse};
use ruma::{
    api::client::{
        error::ErrorKind,
        sync::sync_events::{
            v4::{
                self, AccountDataConfig, E2EEConfig, ExtensionsConfig, ReceiptConfig,
                ToDeviceConfig, TypingConfig,
            },
            UnreadNotificationsCount,
        },
    },
    assign,
    events::{AnySyncStateEvent, TimelineEventType},
    serde::Raw,
    OwnedRoomId, RoomId, UInt,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, error, instrument, trace, warn};
use url::Url;

#[cfg(feature = "experimental-timeline")]
use crate::room::timeline::{EventTimelineItem, Timeline, TimelineBuilder};
use crate::{config::RequestConfig, Client, Result};

/// Internal representation of errors in Sliding Sync
#[derive(Error, Debug)]
#[non_exhaustive]
pub enum Error {
    /// The response we've received from the server can't be parsed or doesn't
    /// match up with the current expectations on the client side. A
    /// `sync`-restart might be required.
    #[error("The sliding sync response could not be handled: {0}")]
    BadResponse(String),
    /// The builder couldn't build `SlidingSync`
    #[error("Builder went wrong: {0}")]
    SlidingSyncBuilder(#[from] SlidingSyncBuilderError),
}

/// The state the [`SlidingSyncView`] is in.
///
/// The lifetime of a SlidingSync usually starts at a `Preload`, getting a fast
/// response for the first given number of Rooms, then switches into
/// `CatchingUp` during which the view fetches the remaining rooms, usually in
/// order, some times in batches. Once that is ready, it switches into `Live`.
///
/// If the client has been offline for a while, though, the SlidingSync might
/// return back to `CatchingUp` at any point.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlidingSyncState {
    /// Hasn't started yet
    #[default]
    Cold,
    /// We are quickly preloading a preview of the most important rooms
    Preload,
    /// We are trying to load all remaining rooms, might be in batches
    CatchingUp,
    /// We are all caught up and now only sync the live responses.
    Live,
}

/// The mode by which the the [`SlidingSyncView`] is in fetching the data.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlidingSyncMode {
    /// fully sync all rooms in the background, page by page of `batch_size`
    #[serde(alias = "FullSync")]
    PagingFullSync,
    /// fully sync all rooms in the background, with a growing window of
    /// `batch_size`,
    GrowingFullSync,
    /// Only sync the specific windows defined
    #[default]
    Selective,
}

/// The Entry in the sliding sync room list per sliding sync view
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub enum RoomListEntry {
    /// This entry isn't known at this point and thus considered `Empty`
    #[default]
    Empty,
    /// There was `OwnedRoomId` but since the server told us to invalid this
    /// entry. it is considered stale
    Invalidated(OwnedRoomId),
    /// This Entry is followed with `OwnedRoomId`
    Filled(OwnedRoomId),
}

impl RoomListEntry {
    /// Is this entry empty or invalidated?
    pub fn empty_or_invalidated(&self) -> bool {
        matches!(self, RoomListEntry::Empty | RoomListEntry::Invalidated(_))
    }

    /// The inner room_id if given
    pub fn as_room_id(&self) -> Option<&RoomId> {
        match &self {
            RoomListEntry::Empty => None,
            RoomListEntry::Invalidated(b) | RoomListEntry::Filled(b) => Some(b.as_ref()),
        }
    }

    fn freeze(&self) -> RoomListEntry {
        match &self {
            RoomListEntry::Empty => RoomListEntry::Empty,
            RoomListEntry::Invalidated(b) | RoomListEntry::Filled(b) => {
                RoomListEntry::Invalidated(b.clone())
            }
        }
    }
}

type AliveRoomTimeline = Arc<MutableVec<SyncTimelineEvent>>;

/// Room info as giving by the SlidingSync Feature.
#[derive(Debug, Clone)]
pub struct SlidingSyncRoom {
    client: Client,
    room_id: OwnedRoomId,
    inner: v4::SlidingSyncRoom,
    is_loading_more: Mutable<bool>,
    is_cold: Arc<AtomicBool>,
    prev_batch: Mutable<Option<String>>,
    timeline_queue: AliveRoomTimeline,
}

#[derive(Serialize, Deserialize)]
struct FrozenSlidingSyncRoom {
    room_id: OwnedRoomId,
    inner: v4::SlidingSyncRoom,
    prev_batch: Option<String>,
    timeline_queue: Vec<SyncTimelineEvent>,
}

impl From<&SlidingSyncRoom> for FrozenSlidingSyncRoom {
    fn from(value: &SlidingSyncRoom) -> Self {
        let locked_tl = value.timeline_queue.lock_ref();
        let tl_len = locked_tl.len();
        // To not overflow the database, we only freeze the newest 10 items. On doing
        // so, we must drop the `prev_batch` key however, as we'd otherwise
        // create a gap between what we have loaded and where the
        // prev_batch-key will start loading when paginating backwards.
        let (prev_batch, timeline) = if tl_len > 10 {
            let pos = tl_len - 10;
            (None, locked_tl.iter().skip(pos).cloned().collect())
        } else {
            (value.prev_batch.lock_ref().clone(), locked_tl.to_vec())
        };
        FrozenSlidingSyncRoom {
            prev_batch,
            timeline_queue: timeline,
            room_id: value.room_id.clone(),
            inner: value.inner.clone(),
        }
    }
}

impl SlidingSyncRoom {
    fn from_frozen(val: FrozenSlidingSyncRoom, client: Client) -> Self {
        let FrozenSlidingSyncRoom { room_id, inner, prev_batch, timeline_queue: timeline } = val;
        SlidingSyncRoom {
            client,
            room_id,
            inner,
            is_loading_more: Mutable::new(false),
            is_cold: Arc::new(AtomicBool::new(true)),
            prev_batch: Mutable::new(prev_batch),
            timeline_queue: Arc::new(MutableVec::new_with_values(timeline)),
        }
    }
}

impl SlidingSyncRoom {
    fn from(
        client: Client,
        room_id: OwnedRoomId,
        mut inner: v4::SlidingSyncRoom,
        timeline: Vec<SyncTimelineEvent>,
    ) -> Self {
        // we overwrite to only keep one copy
        inner.timeline = vec![];
        Self {
            client,
            room_id,
            is_loading_more: Mutable::new(false),
            is_cold: Arc::new(AtomicBool::new(false)),
            prev_batch: Mutable::new(inner.prev_batch.clone()),
            timeline_queue: Arc::new(MutableVec::new_with_values(timeline)),
            inner,
        }
    }

    /// RoomId of this SlidingSyncRoom
    pub fn room_id(&self) -> &OwnedRoomId {
        &self.room_id
    }

    /// Are we currently fetching more timeline events in this room?
    pub fn is_loading_more(&self) -> bool {
        *self.is_loading_more.lock_ref()
    }

    /// the `prev_batch` key to fetch more timeline events for this room
    pub fn prev_batch(&self) -> Option<String> {
        self.prev_batch.lock_ref().clone()
    }

    /// `Timeline` of this room
    pub async fn timeline(&self) -> Option<Timeline> {
        Some(self.timeline_builder()?.track_fully_read().build().await)
    }

    fn timeline_builder(&self) -> Option<TimelineBuilder> {
        if let Some(room) = self.client.get_room(&self.room_id) {
            let timeline_queue = self.timeline_queue.lock_ref().to_vec();
            let prev_batch = self.prev_batch.lock_ref().clone();
            Some(Timeline::builder(&room).events(prev_batch, timeline_queue))
        } else if let Some(invited_room) = self.client.get_invited_room(&self.room_id) {
            Some(Timeline::builder(&invited_room).events(None, vec![]))
        } else {
            error!(
                room_id = ?self.room_id,
                "Room not found in client. Can't provide a timeline for it"
            );
            None
        }
    }

    /// The latest timeline item of this room.
    ///
    /// Use `Timeline::latest_event` instead if you already have a timeline for
    /// this `SlidingSyncRoom`.
    pub async fn latest_event(&self) -> Option<EventTimelineItem> {
        self.timeline_builder()?.build().await.latest_event()
    }

    /// This rooms name as calculated by the server, if any
    pub fn name(&self) -> Option<&str> {
        self.inner.name.as_deref()
    }

    /// Is this a direct message?
    pub fn is_dm(&self) -> Option<bool> {
        self.inner.is_dm
    }

    /// Was this an initial response.
    pub fn is_initial_response(&self) -> Option<bool> {
        self.inner.initial
    }

    /// Is there any unread notifications?
    pub fn has_unread_notifications(&self) -> bool {
        self.inner.unread_notifications.is_empty().not()
    }

    /// Get unread notifications.
    pub fn unread_notifications(&self) -> &UnreadNotificationsCount {
        &self.inner.unread_notifications
    }

    /// Get the required state.
    pub fn required_state(&self) -> &Vec<Raw<AnySyncStateEvent>> {
        &self.inner.required_state
    }

    fn update(
        &mut self,
        room_data: &v4::SlidingSyncRoom,
        timeline_updates: Vec<SyncTimelineEvent>,
    ) {
        let v4::SlidingSyncRoom {
            name,
            initial,
            limited,
            is_dm,
            invite_state,
            unread_notifications,
            required_state,
            prev_batch,
            ..
        } = room_data;

        self.inner.unread_notifications = unread_notifications.clone();

        if name.is_some() {
            self.inner.name = name.clone();
        }
        if initial.is_some() {
            self.inner.initial = *initial;
        }
        if is_dm.is_some() {
            self.inner.is_dm = *is_dm;
        }
        if !invite_state.is_empty() {
            self.inner.invite_state = invite_state.clone();
        }
        if !required_state.is_empty() {
            self.inner.required_state = required_state.clone();
        }

        if let Some(batch) = prev_batch {
            self.prev_batch.lock_mut().replace(batch.clone());
        }

        // There is timeline updates.
        if !timeline_updates.is_empty() {
            if self.is_cold.load(Ordering::SeqCst) {
                // If we come from a cold storage, we overwrite the timeline queue with the
                // timeline updates.

                self.timeline_queue.lock_mut().replace_cloned(timeline_updates);
                self.is_cold.store(false, Ordering::SeqCst);
            } else if *limited {
                // The server alerted us that we missed items in between.

                self.timeline_queue.lock_mut().replace_cloned(timeline_updates);
            } else {
                // It's the hot path. We have new updates that must be added to the existing
                // timeline queue.

                let mut timeline_queue = self.timeline_queue.lock_mut();

                // If the `timeline_queue` contains:
                //     [D, E, F]
                // and if the `timeline_updates` contains:
                //     [A, B, C, D, E, F]
                // the resulting `timeline_queue` must be:
                //     [A, B, C, D, E, F]
                //
                // To do that, we find the longest suffix between `timeline_queue` and
                // `timeline_updates`, in this case:
                //     [D, E, F]
                // Remove the suffix from `timeline_updates`, we get `[A, B, C]` that is
                // prepended to `timeline_queue`.
                //
                // If the `timeline_queue` contains:
                //     [A, B, C, D, E, F]
                // and if the `timeline_updates` contains:
                //     [D, E, F]
                // the resulting `timeline_queue` must be:
                //     [A, B, C, D, E, F]
                //
                // To do that, we continue with the longest suffix. In this case, it is:
                //     [D, E, F]
                // Remove the suffix from `timeline_updates`, we get `[]`. It's empty, we don't
                // touch at `timeline_queue`.

                {
                    let timeline_queue_len = timeline_queue.len();
                    let timeline_updates_len = timeline_updates.len();

                    let position = match timeline_queue
                        .iter()
                        .rev()
                        .zip(timeline_updates.iter().rev())
                        .position(|(queue, update)| queue.event_id() != update.event_id())
                    {
                        // We have found a suffix that equals the size of `timeline_queue` or
                        // `timeline_update`, typically:
                        //     timeline_queue = [D, E, F]
                        //     timeline_update = [A, B, C, D, E, F]
                        // or
                        //     timeline_queue = [A, B, C, D, E, F]
                        //     timeline_update = [D, E, F]
                        // in both case, `position` will return `None` because we are looking for
                        // (from the end) an item that is different.
                        None => std::cmp::min(timeline_queue_len, timeline_updates_len),

                        // We may have found a suffix.
                        //
                        // If we have `Some(0)`, it means we don't have found a suffix. That's the
                        // hot path, `timeline_updates` will just be appended to `timeline_queue`.
                        //
                        // If we have `Some(n)` with `n > 0`, it means we have a prefix but it
                        // doesn't cover all `timeline_queue` or `timeline_update`, typically:
                        //     timeline_queue = [B, D, E, F]
                        //     timeline_update = [A, B, C, D, E, F]
                        // in this case, `position` will return `Some(3)`.
                        // That's annoying because it means we have an invalid `timeline_queue` or
                        // `timeline_update`, but let's try to do our best.
                        Some(position) => position,
                    };

                    if position == 0 {
                        // No prefix found.

                        timeline_queue.extend(timeline_updates);
                    } else {
                        // Prefix found.

                        let new_timeline_updates =
                            &timeline_updates[..timeline_updates_len - position];

                        if !new_timeline_updates.is_empty() {
                            for (at, update) in new_timeline_updates.iter().cloned().enumerate() {
                                timeline_queue.insert_cloned(at, update);
                            }
                        }
                    }
                }
            }
        } else if *limited {
            // The timeline updates are empty. But `limited` is set to true. It's a way to
            // alert that we are stale. In this case, we should just clear the
            // existing timeline.

            self.timeline_queue.lock_mut().clear();
        }
    }
}

type ViewState = Mutable<SlidingSyncState>;
type SyncMode = Mutable<SlidingSyncMode>;
type StringState = Mutable<Option<String>>;
type RangeState = Mutable<Vec<(UInt, UInt)>>;
type RoomsCount = Mutable<Option<u32>>;
type RoomsList = Arc<MutableVec<RoomListEntry>>;
type RoomsMap = Arc<MutableBTreeMap<OwnedRoomId, SlidingSyncRoom>>;
type RoomsSubscriptions = Arc<MutableBTreeMap<OwnedRoomId, v4::RoomSubscription>>;
type RoomUnsubscribe = Arc<MutableVec<OwnedRoomId>>;
type Views = Arc<MutableBTreeMap<String, SlidingSyncView>>;

use derive_builder::Builder;

/// The Summary of a new SlidingSync Update received
#[derive(Debug, Clone)]
pub struct UpdateSummary {
    /// The views (according to their name), which have seen an update
    pub views: Vec<String>,
    /// The Rooms that have seen updates
    pub rooms: Vec<OwnedRoomId>,
}

/// Configuration for a Sliding Sync Instance
#[derive(Clone, Debug, Builder)]
#[builder(
    public,
    name = "SlidingSyncBuilder",
    pattern = "owned",
    build_fn(name = "build_no_cache", private),
    derive(Clone, Debug)
)]
struct SlidingSyncConfig {
    /// The storage key to keep this cache at and load it from
    #[builder(setter(strip_option), default)]
    storage_key: Option<String>,
    /// Customize the homeserver for sliding sync only
    #[builder(setter(strip_option), default)]
    homeserver: Option<Url>,

    /// The client this sliding sync will be using
    client: Client,
    #[builder(private, default)]
    views: BTreeMap<String, SlidingSyncView>,
    #[builder(private, default)]
    extensions: Option<ExtensionsConfig>,
    #[builder(private, default)]
    subscriptions: BTreeMap<OwnedRoomId, v4::RoomSubscription>,
}

impl SlidingSyncConfig {
    pub async fn build(self) -> Result<SlidingSync> {
        let SlidingSyncConfig {
            homeserver,
            storage_key,
            client,
            mut views,
            mut extensions,
            subscriptions,
        } = self;
        let mut delta_token_inner = None;
        let mut rooms_found: BTreeMap<OwnedRoomId, SlidingSyncRoom> = BTreeMap::new();

        if let Some(storage_key) = storage_key.as_ref() {
            trace!(storage_key, "trying to load from cold");

            for (name, view) in views.iter_mut() {
                if let Some(frozen_view) = client
                    .store()
                    .get_custom_value(format!("{storage_key}::{name}").as_bytes())
                    .await?
                    .map(|v| serde_json::from_slice::<FrozenSlidingSyncView>(&v))
                    .transpose()?
                {
                    trace!(name, "frozen for view found");

                    let FrozenSlidingSyncView { rooms_count, rooms_list, rooms } = frozen_view;
                    view.set_from_cold(rooms_count, rooms_list);
                    for (key, frozen_room) in rooms.into_iter() {
                        rooms_found.entry(key).or_insert_with(|| {
                            SlidingSyncRoom::from_frozen(frozen_room, client.clone())
                        });
                    }
                } else {
                    trace!(name, "no frozen state for view found");
                }
            }

            if let Some(FrozenSlidingSync { to_device_since, delta_token }) = client
                .store()
                .get_custom_value(storage_key.as_bytes())
                .await?
                .map(|v| serde_json::from_slice::<FrozenSlidingSync>(&v))
                .transpose()?
            {
                trace!("frozen for generic found");
                if let Some(since) = to_device_since {
                    if let Some(to_device_ext) =
                        extensions.get_or_insert_with(Default::default).to_device.as_mut()
                    {
                        to_device_ext.since = Some(since);
                    }
                }
                delta_token_inner = delta_token;
            }
            trace!("sync unfrozen done");
        };

        trace!(len = rooms_found.len(), "rooms unfrozen");
        let rooms = Arc::new(MutableBTreeMap::with_values(rooms_found));

        let views = Arc::new(MutableBTreeMap::with_values(views));

        Ok(SlidingSync {
            homeserver,
            client,
            storage_key,

            views,
            rooms,

            extensions: Mutex::new(extensions).into(),
            sent_extensions: Mutex::new(None).into(),
            failure_count: Default::default(),

            pos: Mutable::new(None),
            delta_token: Mutable::new(delta_token_inner),
            subscriptions: Arc::new(MutableBTreeMap::with_values(subscriptions)),
            unsubscribe: Default::default(),
        })
    }
}

impl SlidingSyncBuilder {
    /// Convenience function to add a full-sync view to the builder
    pub fn add_fullsync_view(self) -> Self {
        self.add_view(
            SlidingSyncViewBuilder::default_with_fullsync()
                .build()
                .expect("Building default full sync view doesn't fail"),
        )
    }

    /// The cold cache key to read from and store the frozen state at
    pub fn cold_cache<T: ToString>(mut self, name: T) -> Self {
        self.storage_key = Some(Some(name.to_string()));
        self
    }

    /// Do not use the cold cache
    pub fn no_cold_cache(mut self) -> Self {
        self.storage_key = None;
        self
    }

    /// Reset the views to `None`
    pub fn no_views(mut self) -> Self {
        self.views = None;
        self
    }

    /// Add the given view to the views.
    ///
    /// Replace any view with the name.
    pub fn add_view(mut self, v: SlidingSyncView) -> Self {
        let views = self.views.get_or_insert_with(Default::default);
        views.insert(v.name.clone(), v);
        self
    }

    /// Activate e2ee, to-device-message and account data extensions if not yet
    /// configured.
    ///
    /// Will leave any extension configuration found untouched, so the order
    /// does not matter.
    pub fn with_common_extensions(mut self) -> Self {
        {
            let mut cfg = self
                .extensions
                .get_or_insert_with(Default::default)
                .get_or_insert_with(Default::default);
            if cfg.to_device.is_none() {
                cfg.to_device = Some(assign!(ToDeviceConfig::default(), { enabled: Some(true) }));
            }

            if cfg.e2ee.is_none() {
                cfg.e2ee = Some(assign!(E2EEConfig::default(), { enabled: Some(true) }));
            }

            if cfg.account_data.is_none() {
                cfg.account_data =
                    Some(assign!(AccountDataConfig::default(), { enabled: Some(true) }));
            }
        }
        self
    }

    /// Activate e2ee, to-device-message, account data, typing and receipt
    /// extensions if not yet configured.
    ///
    /// Will leave any extension configuration found untouched, so the order
    /// does not matter.
    pub fn with_all_extensions(mut self) -> Self {
        {
            let mut cfg = self
                .extensions
                .get_or_insert_with(Default::default)
                .get_or_insert_with(Default::default);
            if cfg.to_device.is_none() {
                cfg.to_device = Some(assign!(ToDeviceConfig::default(), { enabled: Some(true) }));
            }

            if cfg.e2ee.is_none() {
                cfg.e2ee = Some(assign!(E2EEConfig::default(), { enabled: Some(true) }));
            }

            if cfg.account_data.is_none() {
                cfg.account_data =
                    Some(assign!(AccountDataConfig::default(), { enabled: Some(true) }));
            }

            if cfg.receipt.is_none() {
                cfg.receipt = Some(assign!(ReceiptConfig::default(), { enabled: Some(true) }));
            }

            if cfg.typing.is_none() {
                cfg.typing = Some(assign!(TypingConfig::default(), { enabled: Some(true) }));
            }
        }
        self
    }

    /// Set the E2EE extension configuration.
    pub fn with_e2ee_extension(mut self, e2ee: E2EEConfig) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .e2ee = Some(e2ee);
        self
    }

    /// Unset the E2EE extension configuration.
    pub fn without_e2ee_extension(mut self) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .e2ee = None;
        self
    }

    /// Set the ToDevice extension configuration.
    pub fn with_to_device_extension(mut self, to_device: ToDeviceConfig) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .to_device = Some(to_device);
        self
    }

    /// Unset the ToDevice extension configuration.
    pub fn without_to_device_extension(mut self) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .to_device = None;
        self
    }

    /// Set the account data extension configuration.
    pub fn with_account_data_extension(mut self, account_data: AccountDataConfig) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .account_data = Some(account_data);
        self
    }

    /// Unset the account data extension configuration.
    pub fn without_account_data_extension(mut self) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .account_data = None;
        self
    }

    /// Set the Typing extension configuration.
    pub fn with_typing_extension(mut self, typing: TypingConfig) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .typing = Some(typing);
        self
    }

    /// Unset the Typing extension configuration.
    pub fn without_typing_extension(mut self) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .typing = None;
        self
    }

    /// Set the Receipt extension configuration.
    pub fn with_receipt_extension(mut self, receipt: ReceiptConfig) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .receipt = Some(receipt);
        self
    }

    /// Unset the Receipt extension configuration.
    pub fn without_receipt_extension(mut self) -> Self {
        self.extensions
            .get_or_insert_with(Default::default)
            .get_or_insert_with(Default::default)
            .receipt = None;
        self
    }

    /// Build the Sliding Sync
    ///
    /// if configured, load the cached data from cold storage
    pub async fn build(self) -> Result<SlidingSync> {
        self.build_no_cache().map_err(Error::SlidingSyncBuilder)?.build().await
    }
}

/// The sliding sync instance
#[derive(Clone, Debug)]
pub struct SlidingSync {
    /// Customize the homeserver for sliding sync only
    homeserver: Option<Url>,

    client: Client,

    /// The storage key to keep this cache at and load it from
    storage_key: Option<String>,

    // ------ Internal state
    pub(crate) pos: StringState,
    delta_token: StringState,

    /// The views of this sliding sync instance
    pub views: Views,

    /// The rooms details
    pub rooms: RoomsMap,

    subscriptions: RoomsSubscriptions,
    unsubscribe: RoomUnsubscribe,

    /// keeping track of retries and failure counts
    failure_count: Arc<AtomicU8>,

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
            delta_token: v.delta_token.get_cloned(),
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
    async fn cache_to_storage(&self) -> Result<()> {
        let Some(storage_key) = self.storage_key.as_ref() else { return Ok(()) };
        trace!(storage_key, "saving to storage for later use");
        let v = serde_json::to_vec(&FrozenSlidingSync::from(self))?;
        self.client.store().set_custom_value(storage_key.as_bytes(), v).await?;
        let frozen_views = {
            let rooms_lock = self.rooms.lock_ref();
            self.views
                .lock_ref()
                .iter()
                .map(|(name, view)| {
                    (name.clone(), FrozenSlidingSyncView::freeze(view, &rooms_lock))
                })
                .collect::<Vec<_>>()
        };
        for (name, frozen) in frozen_views {
            trace!(storage_key, name, "saving to view for later use");
            self.client
                .store()
                .set_custom_value(
                    format!("{storage_key}::{name}").as_bytes(),
                    serde_json::to_vec(&frozen)?,
                )
                .await?; // FIXME: parallelize?
        }
        Ok(())
    }
}

impl SlidingSync {
    /// Generate a new SlidingSyncBuilder with the same inner settings and views
    /// but without the current state
    pub fn new_builder_copy(&self) -> SlidingSyncBuilder {
        let mut builder = SlidingSyncBuilder::default()
            .client(self.client.clone())
            .subscriptions(self.subscriptions.lock_ref().to_owned());
        for view in self
            .views
            .lock_ref()
            .values()
            .map(|v| v.new_builder().build().expect("builder worked before, builder works now"))
        {
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
        self.subscriptions.lock_mut().insert_cloned(room_id, settings.unwrap_or_default());
    }

    /// Unsubscribe from a given room.
    ///
    /// Note: this does not cancel any pending request, so make sure to only
    /// poll the stream after you've altered this. If you do that during, it
    /// might take one round trip to take effect.
    pub fn unsubscribe(&self, room_id: OwnedRoomId) {
        if self.subscriptions.lock_mut().remove(&room_id).is_some() {
            self.unsubscribe.lock_mut().push_cloned(room_id);
        }
    }

    /// Add the common extensions if not already configured
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
        self.rooms.lock_ref().get(room_id).cloned()
    }

    /// Check the number of rooms.
    pub fn get_number_of_rooms(&self) -> usize {
        self.rooms.lock_ref().len()
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
        self.views.lock_ref().get(view_name).cloned()
    }

    /// Remove the SlidingSyncView named `view_name` from the views list if
    /// found
    ///
    /// Note: Remember that this change will only be applicable for any new
    /// stream created after this. The old stream will still continue to use the
    /// previous set of views
    pub fn pop_view(&self, view_name: &String) -> Option<SlidingSyncView> {
        self.views.lock_mut().remove(view_name)
    }

    /// Add the view to the list of views
    ///
    /// As views need to have a unique `.name`, if a view with the same name
    /// is found the new view will replace the old one and the return it or
    /// `None`.
    ///
    /// Note: Remember that this change will only be applicable for any new
    /// stream created after this. The old stream will still continue to use the
    /// previous set of views
    pub fn add_view(&self, view: SlidingSyncView) -> Option<SlidingSyncView> {
        self.views.lock_mut().insert_cloned(view.name.clone(), view)
    }

    /// Lookup a set of rooms
    pub fn get_rooms<I: Iterator<Item = OwnedRoomId>>(
        &self,
        room_ids: I,
    ) -> Vec<Option<SlidingSyncRoom>> {
        let rooms = self.rooms.lock_ref();
        room_ids.map(|room_id| rooms.get(&room_id).cloned()).collect()
    }

    /// Get all rooms.
    pub fn get_all_rooms(&self) -> Vec<SlidingSyncRoom> {
        self.rooms.lock_ref().iter().map(|(_, room)| room.clone()).collect()
    }

    #[instrument(skip_all, fields(views = views.len()))]
    async fn handle_response(
        &self,
        resp: v4::Response,
        extensions: Option<ExtensionsConfig>,
        views: &mut BTreeMap<String, SlidingSyncViewRequestGenerator>,
    ) -> Result<UpdateSummary, crate::Error> {
        let mut processed = self.client.process_sliding_sync(resp.clone()).await?;
        debug!("main client processed.");
        self.pos.replace(Some(resp.pos));
        self.delta_token.replace(resp.delta_token);
        let update = {
            let mut rooms = Vec::new();
            let mut rooms_map = self.rooms.lock_mut();
            for (id, mut room_data) in resp.rooms.into_iter() {
                let timeline = if let Some(joined_room) = processed.rooms.join.remove(&id) {
                    joined_room.timeline.events
                } else {
                    let events = room_data.timeline.into_iter().map(Into::into).collect();
                    room_data.timeline = vec![];
                    events
                };

                if let Some(mut room) = rooms_map.remove(&id) {
                    // The room existed before, let's update it.

                    room.update(&room_data, timeline);
                    rooms_map.insert_cloned(id.clone(), room);
                    rooms.push(id.clone());
                } else {
                    // First time we need this room, let's create it.

                    rooms_map.insert_cloned(
                        id.clone(),
                        SlidingSyncRoom::from(self.client.clone(), id.clone(), room_data, timeline),
                    );
                    rooms.push(id);
                }
            }

            let mut updated_views = Vec::new();

            for (name, updates) in resp.lists {
                let Some(generator) = views.get_mut(&name) else {
                    error!("Response for view {name} - unknown to us. skipping");
                    continue
                };
                let count: u32 =
                    updates.count.try_into().expect("the list total count convertible into u32");
                if generator.handle_response(count, &updates.ops, &rooms)? {
                    updated_views.push(name.clone());
                }
            }

            // Update the `to-device` next-batch if found.
            if let Some(to_device_since) = resp.extensions.to_device.map(|t| t.next_batch) {
                self.update_to_device_since(to_device_since)
            }

            // track the most recently successfully sent extensions (needed for sticky
            // semantics)
            if extensions.is_some() {
                *self.sent_extensions.lock().unwrap() = extensions;
            }

            UpdateSummary { views: updated_views, rooms }
        };

        self.cache_to_storage().await?;

        Ok(update)
    }

    async fn sync_once(
        &self,
        views: &mut BTreeMap<String, SlidingSyncViewRequestGenerator>,
    ) -> Result<Option<UpdateSummary>> {
        let mut requests = BTreeMap::new();
        let mut to_remove = Vec::new();

        for (name, generator) in views.iter_mut() {
            if let Some(request) = generator.next() {
                requests.insert(name.clone(), request);
            } else {
                to_remove.push(name.clone());
            }
        }

        for n in to_remove {
            views.remove(&n);
        }

        if views.is_empty() {
            return Ok(None);
        }

        let pos = self.pos.get_cloned();
        let delta_token = self.delta_token.get_cloned();
        let room_subscriptions = self.subscriptions.lock_ref().clone();
        let unsubscribe_rooms = {
            let unsubs = self.unsubscribe.lock_ref().to_vec();
            if !unsubs.is_empty() {
                self.unsubscribe.lock_mut().clear();
            }
            unsubs
        };
        let timeout = Duration::from_secs(30);

        // implement stickiness by only sending extensions if they have
        // changed since the last time we sent them
        let extensions = {
            let extensions = self.extensions.lock().unwrap();
            if *extensions == *self.sent_extensions.lock().unwrap() {
                None
            } else {
                extensions.clone()
            }
        };

        let request = assign!(v4::Request::new(), {
            lists: requests,
            pos,
            delta_token,
            timeout: Some(timeout),
            room_subscriptions,
            unsubscribe_rooms,
            extensions: extensions.clone().unwrap_or_default(),
        });
        debug!("requesting");

        // 30s for the long poll + 30s for network delays
        let request_config = RequestConfig::default().timeout(timeout + Duration::from_secs(30));
        let request = self.client.send_with_homeserver(
            request,
            Some(request_config),
            self.homeserver.as_ref().map(ToString::to_string),
        );

        #[cfg(feature = "e2e-encryption")]
        let response = {
            let (e2ee_uploads, resp) =
                futures_util::join!(self.client.send_outgoing_requests(), request);
            if let Err(e) = e2ee_uploads {
                error!(error = ?e, "Error while sending outgoing E2EE requests");
            }
            resp
        }?;
        #[cfg(not(feature = "e2e-encryption"))]
        let response = request.await?;
        debug!("received");

        let updates = self.handle_response(response, extensions, views).await?;
        debug!("handled");

        Ok(Some(updates))
    }

    /// Create the inner stream for the view.
    ///
    /// Run this stream to receive new updates from the server.
    pub fn stream(&self) -> impl Stream<Item = Result<UpdateSummary, crate::Error>> + '_ {
        let mut views = {
            let mut views = BTreeMap::new();
            let views_lock = self.views.lock_ref();
            for (name, view) in views_lock.deref().iter() {
                views.insert(name.clone(), view.request_generator());
            }
            views
        };

        debug!(?self.extensions, "Setting view stream going");

        async_stream::stream! {
            loop {
                debug!(?self.extensions, "Sync loop running");

                match self.sync_once(&mut views).await {
                    Ok(Some(updates)) => {
                        self.failure_count.store(0, Ordering::SeqCst);
                        yield Ok(updates)
                    },
                    Ok(None) => {
                        break;
                    }
                    Err(e) => {
                        if e.client_api_error_kind() == Some(&ErrorKind::UnknownPos) {
                            // session expired, let's reset
                            if self.failure_count.fetch_add(1, Ordering::SeqCst) >= 3 {
                                error!("session expired three times in a row");
                                yield Err(e.into());

                                break
                            }

                            warn!("Session expired. Restarting sliding sync.");
                            *self.pos.lock_mut() = None;

                            // reset our extensions to the last known good ones.
                            *self.extensions.lock().unwrap() = self.sent_extensions.lock().unwrap().take();

                            debug!(?self.extensions, "Resetting view stream");
                        }

                        yield Err(e.into());

                        continue
                    }
                }
            }
        }
    }
}

/// Holding a specific filtered view within the concept of sliding sync.
/// Main entrypoint to the SlidingSync
///
///
/// ```no_run
/// # use futures::executor::block_on;
/// # use matrix_sdk::Client;
/// # use url::Url;
/// # block_on(async {
/// # let homeserver = Url::parse("http://example.com")?;
/// let client = Client::new(homeserver).await?;
/// let sliding_sync =
///     client.sliding_sync().await.add_fullsync_view().build().await?;
///
/// # anyhow::Ok(())
/// # });
/// ```
#[derive(Clone, Debug, Builder)]
#[builder(build_fn(name = "finish_build"), pattern = "owned", derive(Clone, Debug))]
pub struct SlidingSyncView {
    /// Which SyncMode to start this view under
    #[builder(setter(custom), default)]
    sync_mode: SyncMode,

    /// Sort the rooms list by this
    #[builder(default = "SlidingSyncViewBuilder::default_sort()")]
    sort: Vec<String>,

    /// Required states to return per room
    #[builder(default = "SlidingSyncViewBuilder::default_required_state()")]
    required_state: Vec<(TimelineEventType, String)>,

    /// How many rooms request at a time when doing a full-sync catch up
    #[builder(default = "20")]
    batch_size: u32,

    /// Whether the view should send `UpdatedAt`-Diff signals for rooms
    /// that have changed
    #[builder(default = "false")]
    send_updates_for_items: bool,

    /// How many rooms request a total hen doing a full-sync catch up
    #[builder(setter(into), default)]
    limit: Option<u32>,

    /// Any filters to apply to the query
    #[builder(default)]
    filters: Option<v4::SyncRequestListFilters>,

    /// The maximum number of timeline events to query for
    #[builder(setter(name = "timeline_limit_raw"), default)]
    pub timeline_limit: Mutable<Option<UInt>>,

    // ----- Public state
    /// Name of this view to easily recognize them
    #[builder(setter(into))]
    pub name: String,

    /// The state this view is in
    #[builder(private, default)]
    pub state: ViewState,

    /// The total known number of rooms,
    #[builder(private, default)]
    pub rooms_count: RoomsCount,

    /// The rooms in order
    #[builder(private, default)]
    pub rooms_list: RoomsList,

    /// The ranges windows of the view
    #[builder(setter(name = "ranges_raw"), default)]
    ranges: RangeState,

    /// Signaling updates on the room list after processing
    #[builder(private)]
    rooms_updated_signal: futures_signals::signal::Sender<()>,

    #[builder(private)]
    is_cold: Arc<AtomicBool>,

    /// Get informed if anything in the room changed
    ///
    /// If you only care to know about changes once all of them have applied
    /// (including the total) listen to a clone of this signal.
    #[builder(private)]
    pub rooms_updated_broadcaster:
        futures_signals::signal::Broadcaster<futures_signals::signal::Receiver<()>>,
}

#[derive(Serialize, Deserialize)]
struct FrozenSlidingSyncView {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rooms_count: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    rooms_list: Vec<RoomListEntry>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    rooms: BTreeMap<OwnedRoomId, FrozenSlidingSyncRoom>,
}

impl FrozenSlidingSyncView {
    fn freeze(
        source_view: &SlidingSyncView,
        rooms_map: &MutableBTreeMapLockRef<'_, OwnedRoomId, SlidingSyncRoom>,
    ) -> Self {
        let mut rooms = BTreeMap::new();
        let mut rooms_list = Vec::new();
        for entry in source_view.rooms_list.lock_ref().iter() {
            match entry {
                RoomListEntry::Filled(o) | RoomListEntry::Invalidated(o) => {
                    rooms.insert(o.clone(), rooms_map.get(o).expect("rooms always exists").into());
                }
                _ => {}
            };

            rooms_list.push(entry.freeze());
        }
        FrozenSlidingSyncView {
            rooms_count: *source_view.rooms_count.lock_ref(),
            rooms_list,
            rooms,
        }
    }
}

impl SlidingSyncView {
    fn set_from_cold(&mut self, rooms_count: Option<u32>, rooms_list: Vec<RoomListEntry>) {
        self.state.set(SlidingSyncState::Preload);
        self.is_cold.store(true, Ordering::SeqCst);
        self.rooms_count.replace(rooms_count);
        self.rooms_list.lock_mut().replace_cloned(rooms_list);
    }
}

/// the default name for the full sync view
pub const FULL_SYNC_VIEW_NAME: &str = "full-sync";

impl SlidingSyncViewBuilder {
    /// Create a Builder set up for full sync
    pub fn default_with_fullsync() -> Self {
        Self::default().name(FULL_SYNC_VIEW_NAME).sync_mode(SlidingSyncMode::PagingFullSync)
    }

    /// Build the view
    pub fn build(mut self) -> Result<SlidingSyncView, SlidingSyncViewBuilderError> {
        let (sender, receiver) = futures_signals::signal::channel(());
        self.is_cold = Some(Arc::new(AtomicBool::new(false)));
        self.rooms_updated_signal = Some(sender);
        self.rooms_updated_broadcaster = Some(futures_signals::signal::Broadcaster::new(receiver));
        self.finish_build()
    }

    fn default_sort() -> Vec<String> {
        vec!["by_recency".to_owned(), "by_name".to_owned()]
    }

    fn default_required_state() -> Vec<(TimelineEventType, String)> {
        vec![
            (TimelineEventType::RoomEncryption, "".to_owned()),
            (TimelineEventType::RoomTombstone, "".to_owned()),
        ]
    }

    /// Set the Syncing mode
    pub fn sync_mode(mut self, sync_mode: SlidingSyncMode) -> Self {
        self.sync_mode = Some(SyncMode::new(sync_mode));
        self
    }

    /// Set the ranges to fetch
    pub fn ranges<U: Into<UInt>>(mut self, range: Vec<(U, U)>) -> Self {
        self.ranges =
            Some(RangeState::new(range.into_iter().map(|(a, b)| (a.into(), b.into())).collect()));
        self
    }

    /// Set a single range fetch
    pub fn set_range<U: Into<UInt>>(mut self, from: U, to: U) -> Self {
        self.ranges = Some(RangeState::new(vec![(from.into(), to.into())]));
        self
    }

    /// Set the ranges to fetch
    pub fn add_range<U: Into<UInt>>(mut self, from: U, to: U) -> Self {
        let r = self.ranges.get_or_insert_with(|| RangeState::new(Vec::new()));
        r.lock_mut().push((from.into(), to.into()));
        self
    }

    /// Set the ranges to fetch
    pub fn reset_ranges(mut self) -> Self {
        self.ranges = None;
        self
    }

    /// Set the limit of regular events to fetch for the timeline.
    pub fn timeline_limit<U: Into<UInt>>(mut self, timeline_limit: U) -> Self {
        self.timeline_limit = Some(Mutable::new(Some(timeline_limit.into())));
        self
    }

    /// Reset the limit of regular events to fetch for the timeline. It is left
    /// to the server to decide how many to send back
    pub fn no_timeline_limit(mut self) -> Self {
        self.timeline_limit = None;
        self
    }
}

enum InnerSlidingSyncViewRequestGenerator {
    GrowingFullSync { position: u32, batch_size: u32, limit: Option<u32>, live: bool },
    PagingFullSync { position: u32, batch_size: u32, limit: Option<u32>, live: bool },
    Live,
}

struct SlidingSyncViewRequestGenerator {
    view: SlidingSyncView,
    ranges: Vec<(usize, usize)>,
    inner: InnerSlidingSyncViewRequestGenerator,
}

impl SlidingSyncViewRequestGenerator {
    fn new_with_paging_syncup(view: SlidingSyncView) -> Self {
        let batch_size = view.batch_size;
        let limit = view.limit;
        let position = view
            .ranges
            .get_cloned()
            .first()
            .map(|(_start, end)| u32::try_from(*end).unwrap())
            .unwrap_or_default();

        SlidingSyncViewRequestGenerator {
            view,
            ranges: Default::default(),
            inner: InnerSlidingSyncViewRequestGenerator::PagingFullSync {
                position,
                batch_size,
                limit,
                live: false,
            },
        }
    }

    fn new_with_growing_syncup(view: SlidingSyncView) -> Self {
        let batch_size = view.batch_size;
        let limit = view.limit;
        let position = view
            .ranges
            .get_cloned()
            .first()
            .map(|(_start, end)| u32::try_from(*end).unwrap())
            .unwrap_or_default();

        SlidingSyncViewRequestGenerator {
            view,
            ranges: Default::default(),
            inner: InnerSlidingSyncViewRequestGenerator::GrowingFullSync {
                position,
                batch_size,
                limit,
                live: false,
            },
        }
    }

    fn new_live(view: SlidingSyncView) -> Self {
        SlidingSyncViewRequestGenerator {
            view,
            ranges: Default::default(),
            inner: InnerSlidingSyncViewRequestGenerator::Live,
        }
    }

    fn prefetch_request(
        &mut self,
        start: u32,
        batch_size: u32,
        limit: Option<u32>,
    ) -> v4::SyncRequestList {
        let calc_end = start + batch_size;
        let end = match limit {
            Some(l) => std::cmp::min(l, calc_end),
            _ => calc_end,
        };
        self.make_request_for_ranges(vec![(start.into(), end.into())])
    }

    #[instrument(skip(self), fields(name = self.view.name))]
    fn make_request_for_ranges(&mut self, ranges: Vec<(UInt, UInt)>) -> v4::SyncRequestList {
        let sort = self.view.sort.clone();
        let required_state = self.view.required_state.clone();
        let timeline_limit = self.view.timeline_limit.get_cloned();
        let filters = self.view.filters.clone();

        self.ranges = ranges
            .iter()
            .map(|(a, b)| {
                (
                    usize::try_from(*a).expect("range is a valid u32"),
                    usize::try_from(*b).expect("range is a valid u32"),
                )
            })
            .collect();

        assign!(v4::SyncRequestList::default(), {
            ranges: ranges,
            room_details: assign!(v4::RoomDetailsConfig::default(), {
                required_state,
                timeline_limit,
            }),
            sort,
            filters,
        })
    }

    // generate the next live request
    fn live_request(&mut self) -> v4::SyncRequestList {
        let ranges = self.view.ranges.read_only().get_cloned();
        self.make_request_for_ranges(ranges)
    }

    #[instrument(skip_all, fields(name = self.view.name, rooms_count, has_ops = !ops.is_empty()))]
    fn handle_response(
        &mut self,
        rooms_count: u32,
        ops: &Vec<v4::SyncOp>,
        rooms: &Vec<OwnedRoomId>,
    ) -> Result<bool, Error> {
        let res = self.view.handle_response(rooms_count, ops, &self.ranges, rooms)?;
        self.update_state(rooms_count.saturating_sub(1)); // index is 0 based, count is 1 based
        Ok(res)
    }

    fn update_state(&mut self, max_index: u32) {
        let Some((_start, range_end)) = self.ranges.first() else {
            error!("Why don't we have any ranges?");
            return
        };

        let end = if &(max_index as usize) < range_end { max_index } else { *range_end as u32 };

        trace!(end, max_index, range_end, name = self.view.name, "updating state");

        match &mut self.inner {
            InnerSlidingSyncViewRequestGenerator::PagingFullSync {
                position, live, limit, ..
            }
            | InnerSlidingSyncViewRequestGenerator::GrowingFullSync {
                position, live, limit, ..
            } => {
                let max = limit.map(|limit| std::cmp::min(limit, max_index)).unwrap_or(max_index);
                trace!(end, max, name = self.view.name, "updating state");
                if end >= max {
                    trace!(name = self.view.name, "going live");
                    // we are switching to live mode
                    self.view.set_range(0, max);
                    *position = max;
                    *live = true;

                    self.view.state.set_if(SlidingSyncState::Live, |before, _now| {
                        !matches!(before, SlidingSyncState::Live)
                    });
                } else {
                    *position = end;
                    *live = false;
                    self.view.set_range(0, end);
                    self.view.state.set_if(SlidingSyncState::CatchingUp, |before, _now| {
                        !matches!(before, SlidingSyncState::CatchingUp)
                    });
                }
            }
            InnerSlidingSyncViewRequestGenerator::Live => {
                self.view.state.set_if(SlidingSyncState::Live, |before, _now| {
                    !matches!(before, SlidingSyncState::Live)
                });
            }
        }
    }
}

impl Iterator for SlidingSyncViewRequestGenerator {
    type Item = v4::SyncRequestList;

    fn next(&mut self) -> Option<Self::Item> {
        match self.inner {
            InnerSlidingSyncViewRequestGenerator::PagingFullSync { live, .. }
            | InnerSlidingSyncViewRequestGenerator::GrowingFullSync { live, .. }
                if live =>
            {
                Some(self.live_request())
            }
            InnerSlidingSyncViewRequestGenerator::PagingFullSync {
                position,
                batch_size,
                limit,
                ..
            } => Some(self.prefetch_request(position, batch_size, limit)),
            InnerSlidingSyncViewRequestGenerator::GrowingFullSync {
                position,
                batch_size,
                limit,
                ..
            } => Some(self.prefetch_request(0, position + batch_size, limit)),
            InnerSlidingSyncViewRequestGenerator::Live => Some(self.live_request()),
        }
    }
}

#[instrument(skip(ops))]
fn room_ops(
    rooms_list: &mut MutableVecLockMut<'_, RoomListEntry>,
    ops: &Vec<v4::SyncOp>,
    room_ranges: &Vec<(usize, usize)>,
) -> Result<(), Error> {
    let index_in_range = |idx| room_ranges.iter().any(|(start, end)| idx >= *start && idx <= *end);
    for op in ops {
        match &op.op {
            v4::SlidingOp::Sync => {
                let start: u32 = op
                    .range
                    .ok_or_else(|| {
                        Error::BadResponse(
                            "`range` must be present for Sync and Update operation".to_owned(),
                        )
                    })?
                    .0
                    .try_into()
                    .map_err(|e| Error::BadResponse(format!("`range` not a valid int: {e:}")))?;
                let room_ids = op.room_ids.clone();
                room_ids
                    .into_iter()
                    .enumerate()
                    .map(|(i, r)| {
                        let idx = start as usize + i;
                        if idx >= rooms_list.len() {
                            rooms_list.insert_cloned(idx, RoomListEntry::Filled(r));
                        } else {
                            rooms_list.set_cloned(idx, RoomListEntry::Filled(r));
                        }
                    })
                    .count();
            }
            v4::SlidingOp::Delete => {
                let pos: u32 = op
                    .index
                    .ok_or_else(|| {
                        Error::BadResponse(
                            "`index` must be present for DELETE operation".to_owned(),
                        )
                    })?
                    .try_into()
                    .map_err(|e| {
                        Error::BadResponse(format!("`index` not a valid int for DELETE: {e:}"))
                    })?;
                rooms_list.set_cloned(pos as usize, RoomListEntry::Empty);
            }
            v4::SlidingOp::Insert => {
                let pos: usize = op
                    .index
                    .ok_or_else(|| {
                        Error::BadResponse(
                            "`index` must be present for INSERT operation".to_owned(),
                        )
                    })?
                    .try_into()
                    .map_err(|e| {
                        Error::BadResponse(format!("`index` not a valid int for INSERT: {e:}"))
                    })?;
                let sliced = rooms_list.as_slice();
                let room = RoomListEntry::Filled(op.room_id.clone().ok_or_else(|| {
                    Error::BadResponse("`room_id` must be present for INSERT operation".to_owned())
                })?);
                let mut dif = 0usize;
                loop {
                    // find the next empty slot and drop it
                    let (prev_p, prev_overflow) = pos.overflowing_sub(dif);
                    let check_prev = !prev_overflow && index_in_range(prev_p);
                    let (next_p, overflown) = pos.overflowing_add(dif);
                    let check_after = !overflown && next_p < sliced.len() && index_in_range(next_p);
                    if !check_prev && !check_after {
                        return Err(Error::BadResponse(
                            "We were asked to insert but could not find any direction to shift to"
                                .to_owned(),
                        ));
                    }

                    if check_prev && sliced[prev_p].empty_or_invalidated() {
                        // we only check for previous, if there are items left
                        rooms_list.remove(prev_p);
                        break;
                    } else if check_after && sliced[next_p].empty_or_invalidated() {
                        rooms_list.remove(next_p);
                        break;
                    } else {
                        // let's check the next position;
                        dif += 1;
                    }
                }
                rooms_list.insert_cloned(pos, room);
            }
            v4::SlidingOp::Invalidate => {
                let max_len = rooms_list.len();
                let (mut pos, end): (u32, u32) = if let Some(range) = op.range {
                    (
                        range.0.try_into().map_err(|e| {
                            Error::BadResponse(format!("`range.0` not a valid int: {e:}"))
                        })?,
                        range.1.try_into().map_err(|e| {
                            Error::BadResponse(format!("`range.1` not a valid int: {e:}"))
                        })?,
                    )
                } else {
                    return Err(Error::BadResponse(
                        "`range` must be given on `Invalidate` operation".to_owned(),
                    ));
                };

                if pos > end {
                    return Err(Error::BadResponse(
                        "Invalid invalidation, end smaller than start".to_owned(),
                    ));
                }

                // ranges are inclusive up to the last index. e.g. `[0, 10]`; `[0, 0]`.
                // ensure we pick them all up
                while pos <= end {
                    if pos as usize >= max_len {
                        break; // how does this happen?
                    }
                    let idx = pos as usize;
                    let entry = if let Some(RoomListEntry::Filled(b)) = rooms_list.get(idx) {
                        Some(b.clone())
                    } else {
                        None
                    };

                    if let Some(b) = entry {
                        rooms_list.set_cloned(pos as usize, RoomListEntry::Invalidated(b));
                    } else {
                        rooms_list.set_cloned(pos as usize, RoomListEntry::Empty);
                    }
                    pos += 1;
                }
            }
            s => {
                warn!("Unknown operation occurred: {:?}", s);
            }
        }
    }

    Ok(())
}

impl SlidingSyncView {
    /// Return a builder with the same settings as before
    pub fn new_builder(&self) -> SlidingSyncViewBuilder {
        SlidingSyncViewBuilder::default()
            .name(&self.name)
            .sync_mode(self.sync_mode.lock_ref().clone())
            .sort(self.sort.clone())
            .required_state(self.required_state.clone())
            .batch_size(self.batch_size)
            .ranges(self.ranges.read_only().get_cloned())
    }

    /// Set the ranges to fetch
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn set_ranges(&self, range: Vec<(u32, u32)>) -> &Self {
        *self.ranges.lock_mut() = range.into_iter().map(|(a, b)| (a.into(), b.into())).collect();
        self
    }

    /// Reset the ranges to a particular set
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn set_range(&self, start: u32, end: u32) -> &Self {
        *self.ranges.lock_mut() = vec![(start.into(), end.into())];
        self
    }

    /// Set the ranges to fetch
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn add_range(&self, start: u32, end: u32) -> &Self {
        self.ranges.lock_mut().push((start.into(), end.into()));
        self
    }

    /// Set the ranges to fetch
    ///
    /// Note: sending an empty list of ranges is, according to the spec, to be
    /// understood that the consumer doesn't care about changes of the room
    /// order but you will only receive updates when for rooms entering or
    /// leaving the set.
    ///
    /// Remember to cancel the existing stream and fetch a new one as this will
    /// only be applied on the next request.
    pub fn reset_ranges(&self) -> &Self {
        self.ranges.lock_mut().clear();
        self
    }

    /// Find the current valid position of the room in the view room_list.
    ///
    /// Only matches against the current ranges and only against filled items.
    /// Invalid items are ignore. Return the total position the item was
    /// found in the room_list, return None otherwise.
    pub fn find_room_in_view(&self, room_id: &RoomId) -> Option<usize> {
        let ranges = self.ranges.lock_ref();
        let listing = self.rooms_list.lock_ref();
        for (start_uint, end_uint) in ranges.iter() {
            let mut cur_pos: usize = (*start_uint).try_into().unwrap();
            let end: usize = (*end_uint).try_into().unwrap();
            let iterator = listing.iter().skip(cur_pos);
            for n in iterator {
                if let RoomListEntry::Filled(r) = n {
                    if room_id == r {
                        return Some(cur_pos);
                    }
                }
                if cur_pos == end {
                    break;
                }
                cur_pos += 1;
            }
        }
        None
    }

    /// Find the current valid position of the rooms in the views room_list.
    ///
    /// Only matches against the current ranges and only against filled items.
    /// Invalid items are ignore. Return the total position the items that were
    /// found in the room_list, will skip any room not found in the rooms_list.
    pub fn find_rooms_in_view(&self, room_ids: &[OwnedRoomId]) -> Vec<(usize, OwnedRoomId)> {
        let ranges = self.ranges.lock_ref();
        let listing = self.rooms_list.lock_ref();
        let mut rooms_found = Vec::new();
        for (start_uint, end_uint) in ranges.iter() {
            let mut cur_pos: usize = (*start_uint).try_into().unwrap();
            let end: usize = (*end_uint).try_into().unwrap();
            let iterator = listing.iter().skip(cur_pos);
            for n in iterator {
                if let RoomListEntry::Filled(r) = n {
                    if room_ids.contains(r) {
                        rooms_found.push((cur_pos, r.clone()));
                    }
                }
                if cur_pos == end {
                    break;
                }
                cur_pos += 1;
            }
        }
        rooms_found
    }

    /// Return the room_id at the given index
    pub fn get_room_id(&self, index: usize) -> Option<OwnedRoomId> {
        self.rooms_list.lock_ref().get(index).and_then(|e| e.as_room_id().map(ToOwned::to_owned))
    }

    #[instrument(skip(self, ops), fields(name = self.name, ops_count = ops.len()))]
    fn handle_response(
        &self,
        rooms_count: u32,
        ops: &Vec<v4::SyncOp>,
        ranges: &Vec<(usize, usize)>,
        rooms: &Vec<OwnedRoomId>,
    ) -> Result<bool, Error> {
        let current_rooms_count = self.rooms_count.get();
        if current_rooms_count.is_none()
            || current_rooms_count == Some(0)
            || self.is_cold.load(Ordering::SeqCst)
        {
            debug!("first run, replacing rooms list");
            // first response, we do that slightly differently
            let rooms_list =
                MutableVec::new_with_values(vec![RoomListEntry::Empty; rooms_count as usize]);
            // then we apply it
            let mut locked = rooms_list.lock_mut();
            room_ops(&mut locked, ops, ranges)?;
            self.rooms_list.lock_mut().replace_cloned(locked.as_slice().to_vec());
            self.rooms_count.set(Some(rooms_count));
            self.is_cold.store(false, Ordering::SeqCst);
            return Ok(true);
        }

        debug!("regular update");
        let mut missing =
            rooms_count.checked_sub(self.rooms_list.lock_ref().len() as u32).unwrap_or_default();
        let mut changed = false;
        if missing > 0 {
            let mut list = self.rooms_list.lock_mut();
            list.reserve_exact(missing as usize);
            while missing > 0 {
                list.push_cloned(RoomListEntry::Empty);
                missing -= 1;
            }
            changed = true;
        }

        {
            // keep the lock scoped so that the later find_rooms_in_view doesn't deadlock
            let mut rooms_list = self.rooms_list.lock_mut();

            if !ops.is_empty() {
                room_ops(&mut rooms_list, ops, ranges)?;
                changed = true;
            } else {
                debug!("no rooms operations found");
            }
        }

        if self.rooms_count.get() != Some(rooms_count) {
            self.rooms_count.set(Some(rooms_count));
            changed = true;
        }

        if self.send_updates_for_items && !rooms.is_empty() {
            let found_views = self.find_rooms_in_view(rooms);
            if !found_views.is_empty() {
                debug!("room details found");
                let mut rooms_list = self.rooms_list.lock_mut();
                for (pos, room_id) in found_views {
                    // trigger an `UpdatedAt` update
                    rooms_list.set_cloned(pos, RoomListEntry::Filled(room_id));
                    changed = true;
                }
            }
        }

        if changed {
            if let Err(e) = self.rooms_updated_signal.send(()) {
                warn!("Could not inform about rooms updated: {:?}", e);
            }
        }

        Ok(changed)
    }

    fn request_generator(&self) -> SlidingSyncViewRequestGenerator {
        match self.sync_mode.read_only().get_cloned() {
            SlidingSyncMode::PagingFullSync => {
                SlidingSyncViewRequestGenerator::new_with_paging_syncup(self.clone())
            }
            SlidingSyncMode::GrowingFullSync => {
                SlidingSyncViewRequestGenerator::new_with_growing_syncup(self.clone())
            }
            SlidingSyncMode::Selective => SlidingSyncViewRequestGenerator::new_live(self.clone()),
        }
    }
}

impl Client {
    /// Create a SlidingSyncBuilder tied to this client
    pub async fn sliding_sync(&self) -> SlidingSyncBuilder {
        SlidingSyncBuilder::default().client(self.clone())
    }

    #[instrument(skip(self, response))]
    pub(crate) async fn process_sliding_sync(
        &self,
        response: v4::Response,
    ) -> Result<SyncResponse> {
        let response = self.base_client().process_sliding_sync(response).await?;
        debug!("done processing on base_client");
        self.handle_sync_response(&response).await?;
        Ok(response)
    }
}

#[cfg(test)]
mod test {
    use ruma::room_id;
    use serde_json::json;

    use super::*;

    #[tokio::test]
    async fn check_find_room_in_view() -> Result<()> {
        let view = SlidingSyncViewBuilder::default()
            .name("testview")
            .add_range(0u32, 9u32)
            .build()
            .unwrap();
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
