Sliding Sync Client implementation of [MSC3575][MSC] & extensions

[`Sliding Sync`][MSC] is the third generation synchronization mechanism of
Matrix with a strong focus on bandwidth efficiency. This is made possible by
allowing the client to filter the content very specifically in its request
which, as a result, allows the server to reduce the data sent to the
absolute necessary minimum needed. The API is modeled after common patterns
and UI components end-user messenger clients typically offer. By allowing a
tight coupling of what a client shows and synchronizing that state over
the protocol to the server, the server always sends exactly the information
necessary for the currently displayed subset for the user rather than
filling the connection with data the user isn't interested in right now.

Sliding Sync is a live-protocol using [long-polling](#long-polling) HTTP(S)
connections to stay up to date. On the client side these updates are applied
and propagated through an [asynchronous reactive API](#reactive-api).

The protocol is split into three major sections for that: [lists][#lists],
the [room details](#rooms) and [extensions](#extensions), most notably the
end-to-end-encryption and to-device extensions to enable full
end-to-end-encryption support.

## Starting up

To create a new Sliding Sync session, one must query an existing
(authenticated) `Client` for a new [`SlidingSyncBuilder`] by calling
[`Client::sliding_sync`](`super::Client::sliding_sync`). The
[`SlidingSyncBuilder`] is the baseline configuration to create a
[`SlidingSync`] session by calling `.build()` once everything is ready.
Typically one configures the custom homeserver endpoint.

At the time of writing, no Matrix server natively supports Sliding Sync;
a sidecar called the [Sliding Sync Proxy][proxy] is needed. As that
typically runs on a separate domain, it can be configured on the
[`SlidingSyncBuilder`]:

```rust,no_run
# use matrix_sdk::Client;
# use url::Url;
# async {
# let homeserver = Url::parse("http://example.com")?;
# let client = Client::new(homeserver).await?;
let sliding_sync_builder = client
    .sliding_sync()
    .await
    .homeserver(Url::parse("http://sliding-sync.example.org")?);

# anyhow::Ok(())
# };
```

After the general configuration, one typically wants to add a list via the
[`add_list`][`SlidingSyncBuilder::add_list`] function.

## Lists

A list defines a subset of matching rooms one wants to filter for, and be
kept up about. The [`v4::SyncRequestListFilters`][] allows for a granular
specification of the exact rooms one wants the server to select and the way
one wants them to be ordered before receiving. Secondly each list has a set
of `ranges`: the subset of indexes of the entire list one is interested in
and a unique name to be identified with.

For example, a user might be part of thousands of rooms, but if the client
app always starts by showing the most recent direct message conversations,
loading all rooms is an inefficient approach. Instead with Sliding Sync one
defines a list (e.g. named `"main_list"`) filtering for `is_dm`, ordered
by recency and select to list the top 10 via `ranges: [ [0,9] ]` (indexes
are **inclusive**) like so:

```rust
# use matrix_sdk::sliding_sync::{SlidingSyncList, SlidingSyncMode};
use ruma::{assign, api::client::sync::sync_events::v4};

let list_builder = SlidingSyncList::builder("main_list")
    .sync_mode(SlidingSyncMode::Selective)
    .filters(Some(assign!(
        v4::SyncRequestListFilters::default(), { is_dm: Some(true)}
    )))
    .sort(vec!["by_recency".to_owned()])
    .set_range(0u32..=9);
```

Please refer to the [specification][MSC], the [Ruma types][ruma-types],
specifically [`SyncRequestListFilter`](https://docs.rs/ruma/latest/ruma/api/client/sync/sync_events/v4/struct.SyncRequestListFilters.html) and the
[`SlidingSyncListBuilder`] for details on the filters, sort-order and
range-options and data one requests to be sent. Once the list is fully
configured, `build()` it and add the list to the sliding sync session
by supplying it to [`add_list`][`SlidingSyncBuilder::add_list`].

Lists are inherently stateful and all updates are applied on the shared
list-object. Once a list has been added to [`SlidingSync`], a cloned shared
copy can be retrieved by calling `SlidingSync::list()`, providing the name
of the list. Next to the configuration settings (like name and
`timeline_limit`), the list provides the stateful
[`maximum_number_of_rooms`](SlidingSyncList::maximum_number_of_rooms),
[`room_list`](SlidingSyncList::room_list) and
[`state`](SlidingSyncList::state):

 - `maximum_number_of_rooms` is the number of rooms _total_ there were found
   matching the filters given.
 - `room_list` is a vector of `maximum_number_of_rooms` [`RoomListEntry`]'s
   at the current state. `RoomListEntry`'s only hold `the room_id` if given,
   the [Rooms API](#rooms) holds the actual information about each room
 - `state` is a [`SlidingSyncMode`] signalling meta information about the
   list and its stateful data — whether this is the state loaded from local
   cache, whether the [full sync](#helper-lists) is in progress or whether
   this is the current live information

These are updated upon every update received from the server. One can query
these for their current value at any time, or use the [Reactive API
to subscribe to changes](#reactive-api).

### Helper lists

By default lists run in the [`Selective` mode](SlidingSyncMode::Selective).
That means one sets the desired range(s) to see explicitly (as described
above). Very often, one still wants to load up the entire room list in
background though. For that, the client implementation offers to run lists
in two additional full-sync-modes, which require additional configuration:

- [`SlidingSyncMode::Paging`]: Pages through the entire list of
  rooms one request at a time asking for the next `batch_size` number of
  rooms up to the end or `maximum_number_of_rooms_to_fetch` if configured
- [`SlidingSyncMode::Growing`]: Grows the window by `batch_size` on every
  request till all rooms or until `maximum_number_of_rooms_to_fetch` of rooms
  are in list.

## Rooms

Next to the room list, the details for rooms are the next important aspect.
Each [list](#lists) only references the [`OwnedRoomId`][ruma::OwnedRoomId]
of the room at the given position. The details (`required_state`s and
timeline items) requested by all lists are bundled, together with the common
details (e.g. whether it is a `dm` or its calculated name) and made
available on the Sliding Sync session struct as a [reactive](#reactive-api)
through [`.get_all_rooms`](SlidingSync::get_all_rooms), [`get_room`](SlidingSync::get_room)
and [`get_rooms`](SlidingSync::get_rooms) APIs.

Notably, this map only knows about the rooms that have come down [Sliding
Sync protocol][MSC] and if the given room isn't in any active list range, it
may be stale. Additionally to selecting the room data via the room lists,
the [Sliding Sync protocol][MSC] allows to subscribe to specific rooms via
the [`subscribe_to_room()`](SlidingSync::subscribe_to_room). Any room subscribed
to will receive updates (with the given settings) regardless of whether they are
visible in any list. The most common case for using this API is when the user
enters a room - as we want to receive the incoming new messages regardless of
whether the room is pushed out of the lists room list.

### Room List Entries

As the room list of each list is a vec of the `maximum_number_of_rooms` len
but a room may only know of a subset of entries for sure at any given time,
these entries are wrapped in [`RoomListEntry`][]. This type, in close
proximity to the [specification][MSC], can be either `Empty`, `Filled` or
`Invalidated`, signaling the state of each entry position.
- `Empty` we don't know what sits here at this position in the list.
- `Filled`: there is this `room_id` at this position.
- `Invalidated` in that sense means that we _knew_ what was here before, but
  can't be sure anymore this is still accurate. This occurs when we move the
  sliding window (by changing the ranges) or when a room might drop out of
  the window we are looking at. For the sake of displaying, this is probably
  still fine to display to be at this position, but we can't be sure
  anymore.

Because `Invalidated` occurs whenever a room we knew about before drops out
of focus, we aren't updated about its changes anymore either, there could be
duplicates rooms within invalidated rooms as well as in the union of
invalidated and filled rooms. Keep that in mind, as most UI frameworks don't
like it when their list entries aren't unique.

When [restoring from cold cache][#caching] the room list also only
propagated with `Invalidated` rooms. So if you want to be able to display
data quickly, ensure you are able to render `Invalidated` entries.

### Unsubscribe

Don't forget to [unsubscribe](`SlidingSync::unsubscribe_from_room`) when the
data isn't needed to be updated anymore, e.g. when the user leaves the room, to
reduce the bandwidth back down to what is really needed.

## Extensions

Additionally to the room list and rooms with their state and latest
messages Matrix knows of many other exchange information. All these are
modeled as specific, optional extensions in the [sliding sync
protocol][MSC]. This includes end-to-end-encryption, to-device-messages,
typing- and presence-information and account-data, but can be extended by
any implementation as they please. Handling of the data of the e2ee,
to-device and typing-extensions takes place transparently within the SDK.

By default [`SlidingSync`][] doesn't activate _any_ extensions to save on
bandwidth, but we generally recommend to use the [`with_common_extensions`
when building sliding sync](`SlidingSyncBuilder::with_common_extensions`) to
active e2ee, to-device-messages and account-data-extensions.

## Timeline events

Both the list configuration as well as the [room subscription
settings](`v4::RoomSubscription`) allow to specify a `timeline_limit` to
receive timeline events. If that is unset or set to 0, no events are sent by
the server (which is the default), if multiple limits are found, the highest
takes precedence. Any positive number indicates that on the first request a
room should come into list, up to that count of messages are sent
(depending how many the server has in cache). Following, whenever new events
are found for the matching rooms, the server relays them to the client.

All timeline events coming through Sliding Sync will be processed through
the [`BaseClient`][`matrix_sdk_base::BaseClient`] as in previous sync. This
allows for transparent decryption as well trigger the `client_handlers`.

The current and then following live events list can be queried via the
`timeline` API from `matrix-sdk-ui`. This is prefilled with already received
data.

### Timeline trickling

To allow for a quick startup, client might want to request only a very low
`timeline_limit` (maybe 1 or even 0) at first and update the count later on
the list or room subscription (see [reactive api](#reactive-api)), Since
`0.99.0-rc1` the [sliding sync proxy][proxy] will then "paginate back" and
resent the now larger number of events. All this is handled transparently.

## Long Polling

[Sliding Sync][MSC] is a long-polling API. That means that immediately after
one has received data from the server, they re-open the network connection
again and await for a new response. As there might not be happening much or
a lot happening in short succession — from the client perspective we never
know when new data is received.

One principle of long-polling is, therefore, that it might also takes one
or two requests before the changes one asked for to actually be applied
and the results come back for that. Just assume that at the same time one
adds a room subscription, a new message comes in. The server might reply
with that message immediately and will only kick off the process of
calculating the rooms details and respond with that in the next request one
does after.

This is modelled as a [async `Stream`][`futures_core::stream::Stream`] in
our API, that one basically wants to continue polling. Once one has made its
setup ready and build its sliding sync sessions, one wants to acquire its
[`.stream()`](`SlidingSync::stream`) and continuously poll it.

While the async stream API allows for streams to end (by returning `None`)
Sliding Sync streams items `Result<UpdateSummary, Error>`. For every
successful poll, all data is applied internally, through the base client and
the [reactive structs](#reactive-api) and an
[`Ok(UpdateSummary)`][`UpdateSummary`] is yielded with the minimum
information, which data has been refreshed _in this iteration_: names of
lists and `room_id`s of rooms. Note that, the same way that a list isn't
reacting if only the room data has changed (but not its position in its
list), the list won't be mentioned here either, only the `room_id`. So be
sure to look at both for all subscribed objects.

In full, this typically looks like this:

```rust,no_run
# use futures::{pin_mut, StreamExt};
# use matrix_sdk::{
#    sliding_sync::{SlidingSyncMode, SlidingSyncListBuilder},
#    Client,
# };
# use ruma::{
#    api::client::sync::sync_events::v4, assign,
# };
# use tracing::{debug, error, info, warn};
# use url::Url;
# async {
# let homeserver = Url::parse("http://example.com")?;
# let client = Client::new(homeserver).await?;
let sliding_sync = client
    .sliding_sync()
    .await
    // any lists you want are added here.
    .build()
    .await?;

let stream = sliding_sync.stream();

// continuously poll for updates
pin_mut!(stream);

loop {
    let update = match stream.next().await {
        Some(Ok(u)) => {
            info!("Received an update. Summary: {u:?}");
        }
        Some(Err(e)) => {
            error!("loop was stopped by client error processing: {e}");
        }
        None => {
            error!("Streaming loop ended unexpectedly");
            break;
        }
    };
}

# anyhow::Ok(())
# };
```

### Quick refreshing

A main purpose of [Sliding Sync][MSC] is to provide an API for snappy end
user applications. Long-polling on the other side means that the client has
to wait for the server to respond and that can take quite some time, before
sending the next request with updates, for example an update in a list's
`range`.

That is a bit unfortunate and leaks through the `stream` API as well. The
client is waiting for a `stream.next().await` call before the next request
is sent. The [specification][MSC] on long-polling also states, however, that
if an new request is found coming in, the previous one shall be sent out. In
practice that means one can just start a new stream and the old connection
will return immediately — with a proper response though. One just needs to
make sure to not call that stream any further. Additionally, as both
requests are sent with the same positional argument, the server might
respond with data, the client has already processed. This isn't a problem,
the [`SlidingSync`][] will only process new data and skip the processing
even across restarts.

To support this, in practice, one can spawn a `Future` that runs
[`SlidingSync::stream`]. The spawned `Future` can be cancelled safely. If
the client was waiting on a response, it's cancelled without any issue. If
a response was just received, it
will be fully handled by `SlidingSync`. This _response is always
handled_ process isn't blocking. The cancellation of the spawned `Future`
will be as immediate as possible, and the response handling (if necessary)
will be done in a “detached mode”. However, any further responses handling
will have to wait. It is not possible for `SlidingSync` to handle responses
concurrently.

## Reactive API

As the main source of truth is the data coming from the server, all updates
must be applied transparently throughout to the data layer. The simplest
way to stay up to date on what objects have changed is by checking the
[`lists`](`UpdateSummary.lists`) and [`rooms`](`UpdateSummary.rooms`) of
each [`UpdateSummary`] given by each stream iteration and update the local
copies accordingly. Because of where the loop sits in the stack, that can
be a bit tedious though, so lists and rooms have an additional way of
subscribing to updates via [`eyeball`].

The `Timeline` one can receive per room by calling `.timeline()` (from
`matrix_sdk_ui::timeline::SlidingSyncRoomExt`) will be populated with the
currently cached timeline events.

## Caching

All room data, for filled but also _invalidated_ rooms, including the entire
timeline events as well as all list `room_lists` and
`maximum_number_of_rooms` are held in memory (unless one `pop`s the list
out).

This is a purely in-memory cache layer though. If one wants Sliding Sync to
persist and load from cold (storage) cache, one needs to set its key with
[`storage_key(name)`][`SlidingSyncBuilder::storage_key`] and for each list
present at `.build()`[`SlidingSyncBuilder::build`] sliding sync will attempt
to load their latest cached version from storage, as well as some overall
information of Sliding Sync. If that succeeded the lists `state` has been
set to [`Preloaded`][SlidingSyncState::Preloaded]. Only room data of rooms
present in one of the lists is loaded from storage.

Notice that lists added after Sliding Sync has been built **will not be
loaded from cache** regardless of their settings (as this could lead to
inconsistencies between lists). The same goes for any extension: some
extension data (like the to-device-message position) are stored to storage,
but only retrieved upon `build()` of the `SlidingSyncBuilder`. So if one
only adds them later, they will not be reading the data from storage (to
avoid inconsistencies) and might require more data to be sent in their first
request than if they were loaded form cold-cache.

When loading from storage `room_list` entries found are set to
`Invalidated` — the initial setting here is communicated as a single
`VecDiff::Replace` event through the [reactive API](#reactive-api).

Only the latest 10 timeline items of each room are cached and they are reset
whenever a new set of timeline items is received by the server.

## Bot mode

_Note_: This is not yet exposed via the API. See [#1475](https://github.com/matrix-org/matrix-rust-sdk/issues/1475)

Sliding Sync is modeled for faster and more efficient user-facing client
applications, but offers significant speed ups even for bot cases through
its filtering mechanism. The sort-order and specific subsets, however, are
usually not of interest for bots. For that use case the the
[`v4::SyncRequestList`][] offers the
[`slow_get_all_rooms`](`v4::SyncRequestList::slow_get_all_rooms`) flag.

Once switched on, this mode will not trigger any updates on "list
movements", ranges and sorting are ignored and all rooms matching the filter
will be returned with the given room details settings. Depending on the data
that is requested this will still be significantly faster as the response
only returns the matching rooms and states as per settings.

Think about a bot that only interacts in `is_dm = true` and doesn't need
room topic, room avatar and all the other state. It will be a lot faster to
start up and retrieve only the data needed to actually run.

# Full example

```rust,no_run
use matrix_sdk::{Client, sliding_sync::{SlidingSyncList, SlidingSyncMode}};
use ruma::{assign, {api::client::sync::sync_events::v4, events::StateEventType}};
use tracing::{warn, error, info, debug};
use futures::{StreamExt, pin_mut};
use url::Url;
# async {
# let homeserver = Url::parse("http://example.com")?;
# let client = Client::new(homeserver).await?;
let full_sync_list_name = "full-sync".to_owned();
let active_list_name = "active-list".to_owned();
let sliding_sync_builder = client
    .sliding_sync()
    .await
    .homeserver(Url::parse("http://sliding-sync.example.org")?) // our proxy server
    .with_common_extensions() // we want the e2ee and to-device enabled, please
    .storage_key(Some("example-cache".to_owned())); // we want these to be loaded from and stored into the persistent storage

let full_sync_list = SlidingSyncList::builder(&full_sync_list_name)
    .sync_mode(SlidingSyncMode::Growing { batch_size: 50, maximum_number_of_rooms_to_fetch: Some(500) }) // sync up by growing the window
    .sort(vec!["by_recency".to_owned()]) // ordered by most recent
    .required_state(vec![
        (StateEventType::RoomEncryption, "".to_owned())
     ]); // only want to know if the room is encrypted

let active_list = SlidingSyncList::builder(&active_list_name) // the active window
    .sync_mode(SlidingSyncMode::Selective)  // sync up the specific range only
    .set_range(0u32..=9) // only the top 10 items
    .sort(vec!["by_recency".to_owned()]) // last active
    .timeline_limit(5u32) // add the last 5 timeline items for room preview and faster timeline loading
    .required_state(vec![ // we want to know immediately:
        (StateEventType::RoomEncryption, "".to_owned()), // is it encrypted
        (StateEventType::RoomTopic, "".to_owned()),      // any topic if known
        (StateEventType::RoomAvatar, "".to_owned()),     // avatar if set
     ]);

let sliding_sync = sliding_sync_builder
    .add_list(active_list)
    .add_list(full_sync_list)
    .build()
    .await?;

// subscribe to the list APIs for updates

let (list_state_stream, list_count_stream, list_stream) = sliding_sync.on_list(&active_list_name, |list| {
    (list.state_stream(), list.maximum_number_of_rooms_stream(), list.room_list_stream())
}).unwrap();

tokio::spawn(async move {
    pin_mut!(list_state_stream);
    while let Some(new_state) = list_state_stream.next().await {
        info!("active-list switched state to {new_state:?}");
    }
});

tokio::spawn(async move {
    pin_mut!(list_count_stream);
    while let Some(new_count) = list_count_stream.next().await {
        info!("active-list new count: {new_count:?}");
    }
});

tokio::spawn(async move {
    pin_mut!(list_stream);
    while let Some(v_diff) = list_stream.next().await {
        info!("active-list room list diff update: {v_diff:?}");
    }
});

let stream = sliding_sync.stream();

// continuously poll for updates
pin_mut!(stream);
loop {
    let update = match stream.next().await {
        Some(Ok(u)) => {
            info!("Received an update. Summary: {u:?}");
        },
        Some(Err(e)) => {
             error!("loop was stopped by client error processing: {e}");
        }
        None => {
            error!("Streaming loop ended unexpectedly");
            break;
        }
    };
}

# anyhow::Ok(())
# };
```

[MSC]: https://github.com/matrix-org/matrix-spec-proposals/pull/3575
[proxy]: https://github.com/matrix-org/sliding-sync
[ruma-types]: https://docs.rs/ruma/latest/ruma/api/client/sync/sync_events/v4/index.html
