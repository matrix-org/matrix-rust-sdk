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
Typically one configures the custom homeserver endpoint, although it's
automatically detected using the `.well-known` endpoint, if configured.

At the time of writing, no Matrix server natively supports Sliding Sync;
a sidecar called the [Sliding Sync Proxy][proxy] is needed. As that
typically runs on a separate domain, it can be configured on the
[`SlidingSyncBuilder`].

A unique identifier, less than 16 chars long, is required for each instance
of Sliding Sync, and must be provided when getting a builder:

```rust,no_run
# use matrix_sdk::Client;
# use url::Url;
# async {
# let homeserver = Url::parse("http://example.com")?;
# let client = Client::new(homeserver).await?;
let sliding_sync_builder = client
    .sliding_sync("main-sync")?
    .sliding_sync_proxy(Url::parse("http://sliding-sync.example.org")?);

# anyhow::Ok(())
# };
```

After the general configuration, one typically wants to add a list via the
[`add_list`][`SlidingSyncBuilder::add_list`] function.

## Lists

A list defines a subset of matching rooms one wants to filter for, and be
kept up about. The [`http::request::ListFilters`] allows for a granular
specification of the exact rooms one wants the server to select and the way
one wants them to be ordered before receiving. Secondly each list has a set
of `ranges`: the subset of indexes of the entire list one is interested in
and a unique name to be identified with.

For example, a user might be part of thousands of rooms, but if the client
app always starts by showing the most recent direct message conversations,
loading all rooms is an inefficient approach. Instead with Sliding Sync one
defines a list (e.g. named `"main_list"`) filtering for `is_invite`, ordered
by recency and select to list the top 10 via `ranges: [ [0,9] ]` (indexes
are **inclusive**) like so:

```rust
# use matrix_sdk::sliding_sync::{SlidingSyncList, SlidingSyncMode};
use matrix_sdk_base::sliding_sync::http;
use ruma::assign;

let list_builder = SlidingSyncList::builder("main_list")
    .sync_mode(SlidingSyncMode::new_selective().add_range(0..=9))
    .filters(Some(assign!(
        http::request::ListFilters::default(), { is_invite: Some(true)}
    )));
```

Please refer to the [specification][MSC], the [Ruma types][ruma-types],
specifically [`SyncRequestListFilter`](https://docs.rs/ruma/latest/ruma/api/client/sync/sync_events/v4/struct.SyncRequestListFilters.html) and the
[`SlidingSyncListBuilder`] for details on the filters, and
range-options and data one requests to be sent. Once the list is fully
configured, `build()` it and add the list to the sliding sync session
by supplying it to [`add_list`][`SlidingSyncBuilder::add_list`].

Lists are inherently stateful and all updates are applied on the shared
list-object. Once a list has been added to [`SlidingSync`], a cloned shared
copy can be retrieved by calling `SlidingSync::list()`, providing the name
of the list. Next to the configuration settings (like name and
`timeline_limit`), the list provides the stateful
[`maximum_number_of_rooms`](SlidingSyncList::maximum_number_of_rooms) and
[`state`](SlidingSyncList::state):

 - `maximum_number_of_rooms` is the number of rooms _total_ there were found
   matching the filters given.
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
Each [list](#lists) only references the [`OwnedRoomId`] of the room at the given
position. The details (`required_state`s and timeline items) requested by all
lists are bundled, together with the common details (e.g. whether it is a `dm`
or its calculated name) and made available on the Sliding Sync session struct as
a [reactive](#reactive-api) through [`.get_all_rooms`](SlidingSync::get_all_rooms),
[`get_room`](SlidingSync::get_room) and [`get_rooms`](SlidingSync::get_rooms)
APIs.

Notably, this map only knows about the rooms that have come down [Sliding
Sync protocol][MSC] and if the given room isn't in any active list range, it
may be stale. Additionally to selecting the room data via the room lists,
the [Sliding Sync protocol][MSC] allows to subscribe to specific rooms via
the [`subscribe_to_rooms()`](SlidingSync::subscribe_to_rooms). Any room subscribed
to will receive updates (with the given settings) regardless of whether they are
visible in any list. The most common case for using this API is when the user
enters a room - as we want to receive the incoming new messages regardless of
whether the room is pushed out of the lists room list.

## Extensions

Additionally to the room list and rooms with their state and latest
messages Matrix knows of many other exchange information. All these are
modeled as specific, optional extensions in the [sliding sync
protocol][MSC]. This includes end-to-end-encryption, to-device-messages,
typing- and presence-information and account-data, but can be extended by
any implementation as they please. Handling of the data of the e2ee,
to-device and typing-extensions takes place transparently within the SDK.

By default [`SlidingSync`] doesn't activate _any_ extensions to save on
bandwidth, but we generally recommend to use the `with_XXX_extensions` family
of methods when building sliding sync to enable e2ee, to-device-messages and
account-data-extensions.

## Timeline events

Both the list configuration as well as the [room subscription
settings](`http::request::RoomSubscription`) allow to specify a `timeline_limit` to
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
[`.sync()`](`SlidingSync::sync`) and continuously poll it.

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
# use futures_util::{pin_mut, StreamExt};
# use matrix_sdk::Client;
# use tracing::{error, info};
# use url::Url;
# async {
# let homeserver = Url::parse("http://example.com")?;
# let client = Client::new(homeserver).await?;
let sliding_sync = client
    .sliding_sync("main-sync")?
    // any lists you want are added here.
    .build()
    .await?;

let stream = sliding_sync.sync();

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
the [`SlidingSync`] will only process new data and skip the processing
even across restarts.

To support this, in practice, one can spawn a `Future` that runs
[`SlidingSync::sync`]. The spawned `Future` can be cancelled safely. If
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

## Caching

All room data, including the entire timeline events as well as all lists and
`maximum_number_of_rooms` are held in memory (unless one `pop`s the list out).

Sliding Sync instances are also cached on disk, and restored from disk at
creation. This ensures that we keep track of important position markers, like
the `since` tokens used in the sync requests.

Caching for lists can be enabled independently, using the
[`add_cached_list`][`SlidingSyncBuilder::add_cached_list`] method, assuming
caching has been enabled before. In this case, during
`.build()`[`SlidingSyncBuilder::build`] sliding sync will attempt to load their
latest cached version from storage, as well as some overall information of
Sliding Sync. If that succeeded the lists `state` has been set to
[`Preloaded`][SlidingSyncListLoadingState::Preloaded]. Only room data of rooms
present in one of the lists is loaded from storage.

Any extension data will not be loaded from the cache, if added after Sliding
Sync has been built: some extension data (like the to-device-message position)
are stored to storage, but only retrieved upon `build()` of the
`SlidingSyncBuilder`. So if one only adds them later, they will not be reading
the data from storage (to avoid inconsistencies) and might require more data to
be sent in their first request than if they were loaded from a cold cache.

Only the latest 10 timeline items of each room are cached and they are reset
whenever a new set of timeline items is received by the server.

# Full example

```rust,no_run
use matrix_sdk::{Client, sliding_sync::{SlidingSyncList, SlidingSyncMode}};
use matrix_sdk_base::sliding_sync::http;
use ruma::{assign, events::StateEventType};
use tracing::{warn, error, info, debug};
use futures_util::{pin_mut, StreamExt};
use url::Url;
use std::future::ready;
# async {
# let homeserver = Url::parse("http://example.com")?;
# let client = Client::new(homeserver).await?;
let full_sync_list_name = "full-sync".to_owned();
let active_list_name = "active-list".to_owned();
let sliding_sync_builder = client
    .sliding_sync("main-sync")?
    .sliding_sync_proxy(Url::parse("http://sliding-sync.example.org")?) // our proxy server
    .with_account_data_extension(
        assign!(http::request::AccountData::default(), { enabled: Some(true) }),
    ) // we enable the account-data extension
    .with_e2ee_extension(assign!(http::request::E2EE::default(), { enabled: Some(true) })) // and the e2ee extension
    .with_to_device_extension(
        assign!(http::request::ToDevice::default(), { enabled: Some(true) }),
    ); // and the to-device extension

let full_sync_list = SlidingSyncList::builder(&full_sync_list_name)
    .sync_mode(SlidingSyncMode::Growing { batch_size: 50, maximum_number_of_rooms_to_fetch: Some(500) }) // sync up by growing the window
    .required_state(vec![
        (StateEventType::RoomEncryption, "".to_owned())
     ]); // only want to know if the room is encrypted

let active_list = SlidingSyncList::builder(&active_list_name) // the active window
    .sync_mode(SlidingSyncMode::new_selective().add_range(0..=9))  // sync up the specific range only, first 10 items
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


tokio::spawn(async move {
    // subscribe to rooms updates
    let (_rooms, rooms_stream) = client.rooms_stream();
    // do something with `_rooms`

    pin_mut!(rooms_stream);
    while let Some(_room) = rooms_stream.next().await {
        info!("a room has been updated");
    }
});

let stream = sliding_sync.sync();

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
