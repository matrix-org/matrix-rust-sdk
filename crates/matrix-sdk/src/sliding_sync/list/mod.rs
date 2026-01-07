mod builder;
mod frozen;
mod request_generator;

use std::{
    fmt,
    ops::RangeInclusive,
    sync::{Arc, RwLock as StdRwLock},
};

use eyeball::{SharedObservable, Subscriber};
use futures_core::Stream;
use ruma::{api::client::sync::sync_events::v5 as http, assign, events::StateEventType};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast::Sender;
use tracing::{instrument, warn};

pub use self::builder::*;
pub(super) use self::{frozen::FrozenSlidingSyncList, request_generator::*};
use super::{Error, PollTimeout, SlidingSyncInternalMessage};
use crate::Result;

/// Should this [`SlidingSyncList`] be stored in the cache, and automatically
/// reloaded from the cache upon creation?
#[derive(Clone, Copy, Debug)]
pub(crate) enum SlidingSyncListCachePolicy {
    /// Store and load this list from the cache.
    Enabled,
    /// Don't store and load this list from the cache.
    Disabled,
}

/// The type used to express natural bounds (including but not limited to:
/// ranges, timeline limit) in the Sliding Sync.
pub type Bound = u32;

/// One range of rooms in a response from Sliding Sync.
pub type Range = RangeInclusive<Bound>;

/// Many ranges of rooms.
pub type Ranges = Vec<Range>;

/// Holding a specific filtered list within the concept of sliding sync.
///
/// It is OK to clone this type as much as you need: cloning it is cheap.
#[derive(Clone, Debug)]
pub struct SlidingSyncList {
    inner: Arc<SlidingSyncListInner>,
}

impl SlidingSyncList {
    /// Create a new [`SlidingSyncListBuilder`] with the given name.
    pub fn builder(name: impl Into<String>) -> SlidingSyncListBuilder {
        SlidingSyncListBuilder::new(name)
    }

    /// Get the name of the list.
    pub fn name(&self) -> &str {
        self.inner.name.as_str()
    }

    /// Change the sync-mode.
    ///
    /// It is sometimes necessary to change the sync-mode of a list on-the-fly.
    ///
    /// This will change the sync-mode but also the request generator. A new
    /// request generator is generated. Since requests are calculated based on
    /// the request generator, changing the sync-mode is equivalent to
    /// “resetting” the list. The ranges and the state will be updated when the
    /// next request will be sent and a response will be received. The
    /// maximum number of rooms won't change.
    pub fn set_sync_mode<M>(&self, sync_mode: M)
    where
        M: Into<SlidingSyncMode>,
    {
        self.inner.set_sync_mode(sync_mode.into());

        // When the sync mode is changed, the sync loop must skip over any work in its
        // iteration and jump to the next iteration.
        self.inner.internal_channel_send_if_possible(
            SlidingSyncInternalMessage::SyncLoopSkipOverCurrentIteration,
        );
    }

    /// Get the current state.
    pub fn state(&self) -> SlidingSyncListLoadingState {
        self.inner.state.get()
    }

    /// Check whether this list requires a [`http::Request::timeout`] value.
    ///
    /// A list requires a `timeout` query if and only if we want the server to
    /// wait on new updates, i.e. to do a long-polling.
    pub(super) fn requires_timeout(&self) -> PollTimeout {
        let request_generator = &*self.inner.request_generator.read().unwrap();

        (self.inner.requires_timeout)(request_generator)
    }

    /// Get a stream of state updates.
    ///
    /// If this list has been reloaded from a cache, the initial value read from
    /// the cache will be published.
    ///
    /// There's no guarantee of ordering between items emitted by this stream
    /// and those emitted by other streams exposed on this structure.
    ///
    /// The first part of the returned tuple is the actual loading state, and
    /// the second part is the `Stream` to receive updates.
    pub fn state_stream(
        &self,
    ) -> (SlidingSyncListLoadingState, impl Stream<Item = SlidingSyncListLoadingState>) {
        (self.inner.state.get(), self.inner.state.subscribe())
    }

    /// Get the timeline limit.
    pub fn timeline_limit(&self) -> Bound {
        *self.inner.timeline_limit.read().unwrap()
    }

    /// Set timeline limit.
    pub fn set_timeline_limit(&self, timeline: Bound) {
        *self.inner.timeline_limit.write().unwrap() = timeline;
    }

    /// Get the maximum number of rooms. See [`Self::maximum_number_of_rooms`]
    /// to learn more.
    pub fn maximum_number_of_rooms(&self) -> Option<u32> {
        self.inner.maximum_number_of_rooms.get()
    }

    /// Get a stream of rooms count.
    ///
    /// If this list has been reloaded from a cache, the initial value is
    /// published too.
    ///
    /// There's no guarantee of ordering between items emitted by this stream
    /// and those emitted by other streams exposed on this structure.
    pub fn maximum_number_of_rooms_stream(&self) -> Subscriber<Option<u32>> {
        self.inner.maximum_number_of_rooms.subscribe()
    }

    /// Calculate the next request and return it.
    ///
    /// The next request is entirely calculated based on the request generator
    /// ([`SlidingSyncListRequestGenerator`]).
    pub(super) fn next_request(&self) -> Result<http::request::List, Error> {
        self.inner.next_request()
    }

    /// Returns the current cache policy for this list.
    pub(super) fn cache_policy(&self) -> SlidingSyncListCachePolicy {
        self.inner.cache_policy
    }

    /// Update the list based on the server's response.
    ///
    /// # Parameters
    ///
    /// - `maximum_number_of_rooms`: the `lists.$this_list.count` value, i.e.
    ///   maximum number of available rooms in this list, as defined by the
    ///   server.
    #[instrument(skip(self), fields(name = self.name()))]
    pub(super) fn update(&mut self, maximum_number_of_rooms: Option<u32>) -> Result<bool, Error> {
        // Make sure to update the generator state first; ordering matters because
        // `update_room_list` observes the latest ranges in the response.
        if let Some(maximum_number_of_rooms) = maximum_number_of_rooms {
            self.inner.update_request_generator_state(maximum_number_of_rooms)?;
        }

        let new_changes = self.inner.update_room_list(maximum_number_of_rooms)?;

        Ok(new_changes)
    }

    /// Get the sync-mode.
    #[cfg(feature = "testing")]
    pub fn sync_mode(&self) -> SlidingSyncMode {
        self.inner.sync_mode.read().unwrap().clone()
    }

    /// Set the maximum number of rooms.
    pub(super) fn set_maximum_number_of_rooms(&self, maximum_number_of_rooms: Option<u32>) {
        self.inner.maximum_number_of_rooms.set(maximum_number_of_rooms);
    }
}

pub(super) struct SlidingSyncListInner {
    /// Name of this list to easily recognize them.
    name: String,

    /// The state this list is in.
    state: SharedObservable<SlidingSyncListLoadingState>,

    /// Does this list require a timeout?
    #[cfg(not(target_family = "wasm"))]
    requires_timeout: Arc<dyn Fn(&SlidingSyncListRequestGenerator) -> PollTimeout + Send + Sync>,
    #[cfg(target_family = "wasm")]
    requires_timeout: Arc<dyn Fn(&SlidingSyncListRequestGenerator) -> PollTimeout>,

    /// Any filters to apply to the query.
    filters: Option<http::request::ListFilters>,

    /// Required states to return per room.
    required_state: Vec<(StateEventType, String)>,

    /// The maximum number of timeline events to query for.
    timeline_limit: StdRwLock<Bound>,

    /// The total number of rooms that is possible to interact with for the
    /// given list.
    ///
    /// It's not the total rooms that have been fetched. The server tells the
    /// client that it's possible to fetch this amount of rooms maximum.
    /// Since this number can change according to the list filters, it's
    /// observable.
    maximum_number_of_rooms: SharedObservable<Option<u32>>,

    /// The request generator, i.e. a type that yields the appropriate list
    /// request. See [`SlidingSyncListRequestGenerator`] to learn more.
    request_generator: StdRwLock<SlidingSyncListRequestGenerator>,

    /// Cache policy for this list.
    cache_policy: SlidingSyncListCachePolicy,

    /// The Sliding Sync internal channel sender. See
    /// [`SlidingSyncInner::internal_channel`] to learn more.
    sliding_sync_internal_channel_sender: Sender<SlidingSyncInternalMessage>,

    #[cfg(any(test, feature = "testing"))]
    sync_mode: StdRwLock<SlidingSyncMode>,
}

impl fmt::Debug for SlidingSyncListInner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlidingSyncListInner")
            .field("name", &self.name)
            .field("state", &self.state)
            .finish()
    }
}

impl SlidingSyncListInner {
    /// Change the sync-mode.
    ///
    /// This will change the sync-mode but also the request generator.
    ///
    /// The [`Self::state`] is immediately updated to reflect the new state. The
    /// [`Self::maximum_number_of_rooms`] won't change.
    pub fn set_sync_mode(&self, sync_mode: SlidingSyncMode) {
        #[cfg(any(test, feature = "testing"))]
        {
            *self.sync_mode.write().unwrap() = sync_mode.clone();
        }

        {
            let mut request_generator = self.request_generator.write().unwrap();
            *request_generator = SlidingSyncListRequestGenerator::new(sync_mode);
        }

        self.state.update(|state| {
            *state = match state {
                SlidingSyncListLoadingState::NotLoaded => SlidingSyncListLoadingState::NotLoaded,
                SlidingSyncListLoadingState::Preloaded => SlidingSyncListLoadingState::Preloaded,
                SlidingSyncListLoadingState::PartiallyLoaded
                | SlidingSyncListLoadingState::FullyLoaded => {
                    SlidingSyncListLoadingState::PartiallyLoaded
                }
            }
        });
    }

    /// Update the state to the next request, and return it.
    fn next_request(&self) -> Result<http::request::List, Error> {
        let ranges = {
            // Use a dedicated scope to ensure the lock is released before continuing.
            let mut request_generator = self.request_generator.write().unwrap();
            request_generator.generate_next_ranges(self.maximum_number_of_rooms.get())?
        };

        // Here we go.
        Ok(self.request(ranges))
    }

    /// Build a [`http::request::List`] based on the current state of the
    /// request generator.
    #[instrument(skip(self), fields(name = self.name))]
    fn request(&self, ranges: Ranges) -> http::request::List {
        let ranges = ranges.into_iter().map(|r| ((*r.start()).into(), (*r.end()).into())).collect();

        let mut request = assign!(http::request::List::default(), { ranges });
        request.room_details.timeline_limit = (*self.timeline_limit.read().unwrap()).into();
        request.filters = self.filters.clone();
        request.room_details.required_state = self.required_state.clone();

        request
    }

    /// Update `[Self::maximum_number_of_rooms]`.
    ///
    /// The `maximum_number_of_rooms` is the `lists.$this_list.count` value,
    /// i.e. maximum number of available rooms as defined by the server.
    fn update_room_list(&self, maximum_number_of_rooms: Option<u32>) -> Result<bool, Error> {
        let mut new_changes = false;

        if maximum_number_of_rooms.is_some() {
            // Update the `maximum_number_of_rooms` if it has changed.
            if self.maximum_number_of_rooms.set_if_not_eq(maximum_number_of_rooms).is_some() {
                new_changes = true;
            }
        }

        Ok(new_changes)
    }

    /// Update the state of the [`SlidingSyncListRequestGenerator`] after
    /// receiving a response.
    fn update_request_generator_state(&self, maximum_number_of_rooms: u32) -> Result<(), Error> {
        let mut request_generator = self.request_generator.write().unwrap();

        let new_state = request_generator.handle_response(&self.name, maximum_number_of_rooms)?;
        self.state.set_if_not_eq(new_state);

        Ok(())
    }

    /// Send a message over the internal channel if there is a receiver, i.e. if
    /// the sync loop is running.
    #[instrument]
    fn internal_channel_send_if_possible(&self, message: SlidingSyncInternalMessage) {
        // If there is no receiver, the send will fail, but that's OK here.
        let _ = self.sliding_sync_internal_channel_sender.send(message);
    }
}

/// The state the [`SlidingSyncList`] is in.
///
/// The lifetime of a `SlidingSyncList` usually starts at `NotLoaded` or
/// `Preloaded` (if it is restored from a cache). When loading rooms in a list,
/// depending of the [`SlidingSyncMode`], it moves to `PartiallyLoaded` or
/// `FullyLoaded`.
///
/// If the client has been offline for a while, though, the `SlidingSyncList`
/// might return back to `PartiallyLoaded` at any point.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlidingSyncListLoadingState {
    /// Sliding Sync has not started to load anything yet.
    #[default]
    NotLoaded,

    /// Sliding Sync has been preloaded, i.e. restored from a cache for example.
    Preloaded,

    /// Updates are received from the loaded rooms, and new rooms are being
    /// fetched in the background.
    PartiallyLoaded,

    /// Updates are received for all the loaded rooms, and all rooms have been
    /// loaded!
    FullyLoaded,
}

#[cfg(test)]
impl SlidingSyncListLoadingState {
    /// Check whether the state is [`Self::FullyLoaded`].
    fn is_fully_loaded(&self) -> bool {
        matches!(self, Self::FullyLoaded)
    }
}

/// Builder for a new sliding sync list in selective mode.
///
/// Conveniently allows to add ranges.
#[derive(Clone, Debug, Default)]
pub struct SlidingSyncSelectiveModeBuilder {
    ranges: Ranges,
}

impl SlidingSyncSelectiveModeBuilder {
    /// Create a new `SlidingSyncSelectiveModeBuilder`.
    fn new() -> Self {
        Self::default()
    }

    /// Select a range to fetch.
    pub fn add_range(mut self, range: Range) -> Self {
        self.ranges.push(range);
        self
    }

    /// Select many ranges to fetch.
    pub fn add_ranges(mut self, ranges: Ranges) -> Self {
        self.ranges.extend(ranges);
        self
    }
}

impl From<SlidingSyncSelectiveModeBuilder> for SlidingSyncMode {
    fn from(builder: SlidingSyncSelectiveModeBuilder) -> Self {
        Self::Selective { ranges: builder.ranges }
    }
}

#[derive(Clone, Debug)]
enum WindowedModeBuilderKind {
    Paging,
    Growing,
}

/// Builder for a new sliding sync list in growing/paging mode.
#[derive(Clone, Debug)]
pub struct SlidingSyncWindowedModeBuilder {
    mode: WindowedModeBuilderKind,
    batch_size: u32,
    maximum_number_of_rooms_to_fetch: Option<u32>,
}

impl SlidingSyncWindowedModeBuilder {
    fn new(mode: WindowedModeBuilderKind, batch_size: u32) -> Self {
        Self { mode, batch_size, maximum_number_of_rooms_to_fetch: None }
    }

    /// The maximum number of rooms to fetch.
    pub fn maximum_number_of_rooms_to_fetch(mut self, num: u32) -> Self {
        self.maximum_number_of_rooms_to_fetch = Some(num);
        self
    }
}

impl From<SlidingSyncWindowedModeBuilder> for SlidingSyncMode {
    fn from(builder: SlidingSyncWindowedModeBuilder) -> Self {
        match builder.mode {
            WindowedModeBuilderKind::Paging => Self::Paging {
                batch_size: builder.batch_size,
                maximum_number_of_rooms_to_fetch: builder.maximum_number_of_rooms_to_fetch,
            },
            WindowedModeBuilderKind::Growing => Self::Growing {
                batch_size: builder.batch_size,
                maximum_number_of_rooms_to_fetch: builder.maximum_number_of_rooms_to_fetch,
            },
        }
    }
}

/// How a [`SlidingSyncList`] fetches the data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SlidingSyncMode {
    /// Only sync the specific defined windows/ranges.
    Selective {
        /// The specific defined ranges.
        ranges: Ranges,
    },

    /// Fully sync all rooms in the background, page by page of `batch_size`,
    /// like `0..=19`, `20..=39`, `40..=59` etc. assuming the `batch_size` is
    /// 20.
    Paging {
        /// The batch size.
        batch_size: u32,

        /// The maximum number of rooms to fetch. `None` to fetch everything
        /// possible.
        maximum_number_of_rooms_to_fetch: Option<u32>,
    },

    /// Fully sync all rooms in the background, with a growing window of
    /// `batch_size`, like `0..=19`, `0..=39`, `0..=59` etc. assuming the
    /// `batch_size` is 20.
    Growing {
        /// The batch size.
        batch_size: u32,

        /// The maximum number of rooms to fetch. `None` to fetch everything
        /// possible.
        maximum_number_of_rooms_to_fetch: Option<u32>,
    },
}

impl Default for SlidingSyncMode {
    fn default() -> Self {
        Self::Selective { ranges: Vec::new() }
    }
}

impl SlidingSyncMode {
    /// Create a `SlidingSyncMode::Selective`.
    pub fn new_selective() -> SlidingSyncSelectiveModeBuilder {
        SlidingSyncSelectiveModeBuilder::new()
    }

    /// Create a `SlidingSyncMode::Paging`.
    pub fn new_paging(batch_size: u32) -> SlidingSyncWindowedModeBuilder {
        SlidingSyncWindowedModeBuilder::new(WindowedModeBuilderKind::Paging, batch_size)
    }

    /// Create a `SlidingSyncMode::Growing`.
    pub fn new_growing(batch_size: u32) -> SlidingSyncWindowedModeBuilder {
        SlidingSyncWindowedModeBuilder::new(WindowedModeBuilderKind::Growing, batch_size)
    }
}

#[cfg(test)]
mod tests {
    use std::{
        cell::Cell,
        ops::Not,
        sync::{Arc, Mutex},
    };

    use assert_matches::assert_matches;
    use matrix_sdk_test::async_test;
    use ruma::uint;
    use serde_json::json;
    use tokio::sync::broadcast::{channel, error::TryRecvError};

    use super::{PollTimeout, SlidingSyncList, SlidingSyncListLoadingState, SlidingSyncMode};
    use crate::sliding_sync::SlidingSyncInternalMessage;

    macro_rules! assert_json_roundtrip {
        (from $type:ty: $rust_value:expr => $json_value:expr) => {
            let json = serde_json::to_value(&$rust_value).unwrap();
            assert_eq!(json, $json_value);

            let rust: $type = serde_json::from_value(json).unwrap();
            assert_eq!(rust, $rust_value);
        };
    }

    #[async_test]
    async fn test_sliding_sync_list_selective_mode() {
        let (sender, mut receiver) = channel(1);

        // Set range on `Selective`.
        let list = SlidingSyncList::builder("foo")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=1).add_range(2..=3))
            .build(sender);

        {
            let mut generator = list.inner.request_generator.write().unwrap();
            assert_eq!(generator.requested_ranges(), &[0..=1, 2..=3]);

            let ranges = generator.generate_next_ranges(None).unwrap();
            assert_eq!(ranges, &[0..=1, 2..=3]);
        }

        // There shouldn't be any internal request to restart the sync loop yet.
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));

        list.set_sync_mode(SlidingSyncMode::new_selective().add_range(4..=5));

        {
            let mut generator = list.inner.request_generator.write().unwrap();
            assert_eq!(generator.requested_ranges(), &[4..=5]);

            let ranges = generator.generate_next_ranges(None).unwrap();
            assert_eq!(ranges, &[4..=5]);
        }

        // Setting the sync mode requests exactly one restart of the sync loop.
        assert!(matches!(
            receiver.try_recv(),
            Ok(SlidingSyncInternalMessage::SyncLoopSkipOverCurrentIteration)
        ));
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn test_sliding_sync_list_timeline_limit() {
        let (sender, _receiver) = channel(1);

        let list = SlidingSyncList::builder("foo")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=1))
            .timeline_limit(7)
            .build(sender);

        assert_eq!(list.timeline_limit(), 7);

        list.set_timeline_limit(42);
        assert_eq!(list.timeline_limit(), 42);
    }

    macro_rules! assert_ranges {
        (
            list = $list:ident,
            list_state = $first_list_state:ident,
            maximum_number_of_rooms = $maximum_number_of_rooms:expr,
            timeout = $initial_timeout:ident,
            $(
                next => {
                    ranges = $( $range_start:literal ..= $range_end:literal ),* ,
                    is_fully_loaded = $is_fully_loaded:expr,
                    list_state = $list_state:ident,
                    timeout = $timeout:ident,
                }
            ),*
            $(,)*
        ) => {
            assert_eq!($list.state(), SlidingSyncListLoadingState::$first_list_state, "first state");
            assert_matches!(
                $list.requires_timeout(),
                PollTimeout:: $initial_timeout,
                "initial timeout",
            );

            $(
                {
                    // Generate a new request.
                    let request = $list.next_request().unwrap();

                    assert_eq!(
                        request.ranges,
                        [
                            $( (uint!( $range_start ), uint!( $range_end )) ),*
                        ],
                        "ranges",
                    );

                    // Fake a response.
                    let _ = $list.update(Some($maximum_number_of_rooms));

                    assert_eq!(
                        $list.inner.request_generator.read().unwrap().is_fully_loaded(),
                        $is_fully_loaded,
                        "is fully loaded",
                    );
                    assert_eq!(
                        $list.state(),
                        SlidingSyncListLoadingState::$list_state,
                        "state",
                    );
                    assert_matches!(
                        $list.requires_timeout(),
                        PollTimeout:: $timeout,
                        "timeout",
                    );
                }
            )*
        };
    }

    #[test]
    fn test_generator_paging_full_sync() {
        let (sender, _receiver) = channel(1);

        let mut list = SlidingSyncList::builder("testing")
            .sync_mode(SlidingSyncMode::new_paging(10))
            .build(sender);

        assert_ranges! {
            list = list,
            list_state = NotLoaded,
            maximum_number_of_rooms = 25,
            timeout = None,
            next => {
                ranges = 0..=9,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
                timeout = None,
            },
            next => {
                ranges = 10..=19,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
                timeout = None,
            },
            // The maximum number of rooms is reached!
            next => {
                ranges = 20..=24,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
            // Now it's fully loaded, so the same request must be produced every time.
            next => {
                ranges = 0..=24, // the range starts at 0 now!
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
            next => {
                ranges = 0..=24,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
        };
    }

    #[test]
    fn test_generator_paging_full_sync_with_a_maximum_number_of_rooms_to_fetch() {
        let (sender, _receiver) = channel(1);

        let mut list = SlidingSyncList::builder("testing")
            .sync_mode(SlidingSyncMode::new_paging(10).maximum_number_of_rooms_to_fetch(22))
            .build(sender);

        assert_ranges! {
            list = list,
            list_state = NotLoaded,
            maximum_number_of_rooms = 25,
            timeout = None,
            next => {
                ranges = 0..=9,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
                timeout = None,
            },
            next => {
                ranges = 10..=19,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
                timeout = None,
            },
            // The maximum number of rooms to fetch is reached!
            next => {
                ranges = 20..=21,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
            // Now it's fully loaded, so the same request must be produced every time.
            next => {
                ranges = 0..=21, // the range starts at 0 now!
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
            next => {
                ranges = 0..=21,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
        };
    }

    #[test]
    fn test_generator_growing_full_sync() {
        let (sender, _receiver) = channel(1);

        let mut list = SlidingSyncList::builder("testing")
            .sync_mode(SlidingSyncMode::new_growing(10))
            .build(sender);

        assert_ranges! {
            list = list,
            list_state = NotLoaded,
            maximum_number_of_rooms = 25,
            timeout = None,
            next => {
                ranges = 0..=9,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
                timeout = None,
            },
            next => {
                ranges = 0..=19,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
                timeout = None,
            },
            // The maximum number of rooms is reached!
            next => {
                ranges = 0..=24,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
            // Now it's fully loaded, so the same request must be produced every time.
            next => {
                ranges = 0..=24,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
            next => {
                ranges = 0..=24,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
        };
    }

    #[test]
    fn test_generator_growing_full_sync_with_a_maximum_number_of_rooms_to_fetch() {
        let (sender, _receiver) = channel(1);

        let mut list = SlidingSyncList::builder("testing")
            .sync_mode(SlidingSyncMode::new_growing(10).maximum_number_of_rooms_to_fetch(22))
            .build(sender);

        assert_ranges! {
            list = list,
            list_state = NotLoaded,
            maximum_number_of_rooms = 25,
            timeout = None,
            next => {
                ranges = 0..=9,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
                timeout = None,
            },
            next => {
                ranges = 0..=19,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
                timeout = None,
            },
            // The maximum number of rooms is reached!
            next => {
                ranges = 0..=21,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
            // Now it's fully loaded, so the same request must be produced every time.
            next => {
                ranges = 0..=21,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
            next => {
                ranges = 0..=21,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
        };
    }

    #[test]
    fn test_generator_selective() {
        let (sender, _receiver) = channel(1);

        let mut list = SlidingSyncList::builder("testing")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10).add_range(42..=153))
            .build(sender);

        assert_ranges! {
            list = list,
            list_state = NotLoaded,
            maximum_number_of_rooms = 25,
            timeout = Default,
            // The maximum number of rooms is reached directly!
            next => {
                ranges = 0..=10, 42..=153,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
            // Now it's fully loaded, so the same request must be produced every time.
            next => {
                ranges = 0..=10, 42..=153,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
            next => {
                ranges = 0..=10, 42..=153,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            }
        };
    }

    #[async_test]
    async fn test_generator_selective_with_modifying_ranges_on_the_fly() {
        let (sender, _receiver) = channel(4);

        let mut list = SlidingSyncList::builder("testing")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10).add_range(42..=153))
            .build(sender);

        assert_ranges! {
            list = list,
            list_state = NotLoaded,
            maximum_number_of_rooms = 25,
            timeout = Default,
            // The maximum number of rooms is reached directly!
            next => {
                ranges = 0..=10, 42..=153,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
            // Now it's fully loaded, so the same request must be produced every time.
            next => {
                ranges = 0..=10, 42..=153,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
            next => {
                ranges = 0..=10, 42..=153,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            }
        };

        list.set_sync_mode(SlidingSyncMode::new_selective().add_range(3..=7));

        assert_ranges! {
            list = list,
            list_state = PartiallyLoaded,
            maximum_number_of_rooms = 25,
            timeout = Default,
            next => {
                ranges = 3..=7,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
        };

        list.set_sync_mode(SlidingSyncMode::new_selective().add_range(42..=77));

        assert_ranges! {
            list = list,
            list_state = PartiallyLoaded,
            maximum_number_of_rooms = 25,
            timeout = Default,
            next => {
                ranges = 42..=77,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
        };

        list.set_sync_mode(SlidingSyncMode::new_selective());

        assert_ranges! {
            list = list,
            list_state = PartiallyLoaded,
            maximum_number_of_rooms = 25,
            timeout = Default,
            next => {
                ranges = ,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
        };
    }

    #[async_test]
    async fn test_generator_changing_sync_mode_to_various_modes() {
        let (sender, _receiver) = channel(4);

        let mut list = SlidingSyncList::builder("testing")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=10).add_range(42..=153))
            .build(sender);

        assert_ranges! {
            list = list,
            list_state = NotLoaded,
            maximum_number_of_rooms = 25,
            timeout = Default,
            // The maximum number of rooms is reached directly!
            next => {
                ranges = 0..=10, 42..=153,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
            // Now it's fully loaded, so the same request must be produced every time.
            next => {
                ranges = 0..=10, 42..=153,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
            next => {
                ranges = 0..=10, 42..=153,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            }
        };

        // Changing from `Selective` to `Growing`.
        list.set_sync_mode(SlidingSyncMode::new_growing(10));

        assert_ranges! {
            list = list,
            list_state = PartiallyLoaded, // we had some partial state, but we can't be sure it's fully loaded until the next request
            maximum_number_of_rooms = 25,
            timeout = None,
            next => {
                ranges = 0..=9,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
                timeout = None,
            },
            next => {
                ranges = 0..=19,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
                timeout = None,
            },
            // The maximum number of rooms is reached!
            next => {
                ranges = 0..=24,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
            // Now it's fully loaded, so the same request must be produced every time.
            next => {
                ranges = 0..=24,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
            next => {
                ranges = 0..=24,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
        };

        // Changing from `Growing` to `Paging`.
        list.set_sync_mode(SlidingSyncMode::new_paging(10));

        assert_ranges! {
            list = list,
            list_state = PartiallyLoaded, // we had some partial state, but we can't be sure it's fully loaded until the next request
            maximum_number_of_rooms = 25,
            timeout = None,
            next => {
                ranges = 0..=9,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
                timeout = None,
            },
            next => {
                ranges = 10..=19,
                is_fully_loaded = false,
                list_state = PartiallyLoaded,
                timeout = None,
            },
            // The maximum number of rooms is reached!
            next => {
                ranges = 20..=24,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
            // Now it's fully loaded, so the same request must be produced every time.
            next => {
                ranges = 0..=24, // the range starts at 0 now!
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
            next => {
                ranges = 0..=24,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
        };

        // Changing from `Paging` to `Selective`.
        list.set_sync_mode(SlidingSyncMode::new_selective());

        assert_eq!(list.state(), SlidingSyncListLoadingState::PartiallyLoaded); // we had some partial state, but we can't be sure it's fully loaded until the
        // next request

        // We need to update the ranges, of course, as they are not managed
        // automatically anymore.
        list.set_sync_mode(SlidingSyncMode::new_selective().add_range(0..=100));

        assert_ranges! {
            list = list,
            list_state = PartiallyLoaded, // we had some partial state, but we can't be sure it's fully loaded until the next request
            maximum_number_of_rooms = 25,
            timeout = Default,
            // The maximum number of rooms is reached directly!
            next => {
                ranges = 0..=100,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
            // Now it's fully loaded, so the same request must be produced every time.
            next => {
                ranges = 0..=100,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            },
            next => {
                ranges = 0..=100,
                is_fully_loaded = true,
                list_state = FullyLoaded,
                timeout = Default,
            }
        };
    }

    #[async_test]
    #[allow(clippy::await_holding_lock)]
    async fn test_inner_update_maximum_number_of_rooms() {
        let (sender, _receiver) = channel(1);

        let mut list = SlidingSyncList::builder("foo")
            .sync_mode(SlidingSyncMode::new_selective().add_range(0..=3))
            .build(sender);

        assert!(list.maximum_number_of_rooms().is_none());

        // Simulate a request.
        let _ = list.next_request();
        let new_changes = list.update(Some(5)).unwrap();
        assert!(new_changes);

        // The `maximum_number_of_rooms` has been updated as expected.
        assert_eq!(list.maximum_number_of_rooms(), Some(5));

        // Simulate another request.
        let _ = list.next_request();
        let new_changes = list.update(Some(5)).unwrap();
        assert!(!new_changes);

        // The `maximum_number_of_rooms` has not changed.
        assert_eq!(list.maximum_number_of_rooms(), Some(5));
    }

    #[test]
    fn test_sliding_sync_mode_serialization() {
        assert_json_roundtrip!(
            from SlidingSyncMode: SlidingSyncMode::from(SlidingSyncMode::new_paging(1).maximum_number_of_rooms_to_fetch(2)) => json!({
                "Paging": {
                    "batch_size": 1,
                    "maximum_number_of_rooms_to_fetch": 2
                }
            })
        );
        assert_json_roundtrip!(
            from SlidingSyncMode: SlidingSyncMode::from(SlidingSyncMode::new_growing(1).maximum_number_of_rooms_to_fetch(2)) => json!({
                "Growing": {
                    "batch_size": 1,
                    "maximum_number_of_rooms_to_fetch": 2
                }
            })
        );
        assert_json_roundtrip!(from SlidingSyncMode: SlidingSyncMode::from(SlidingSyncMode::new_selective()) => json!({
                "Selective": {
                    "ranges": []
                }
            })
        );
    }

    #[test]
    fn test_sliding_sync_list_loading_state_serialization() {
        assert_json_roundtrip!(from SlidingSyncListLoadingState: SlidingSyncListLoadingState::NotLoaded => json!("NotLoaded"));
        assert_json_roundtrip!(from SlidingSyncListLoadingState: SlidingSyncListLoadingState::Preloaded => json!("Preloaded"));
        assert_json_roundtrip!(from SlidingSyncListLoadingState: SlidingSyncListLoadingState::PartiallyLoaded => json!("PartiallyLoaded"));
        assert_json_roundtrip!(from SlidingSyncListLoadingState: SlidingSyncListLoadingState::FullyLoaded => json!("FullyLoaded"));
    }

    #[test]
    fn test_sliding_sync_list_loading_state_is_fully_loaded() {
        assert!(SlidingSyncListLoadingState::NotLoaded.is_fully_loaded().not());
        assert!(SlidingSyncListLoadingState::Preloaded.is_fully_loaded().not());
        assert!(SlidingSyncListLoadingState::PartiallyLoaded.is_fully_loaded().not());
        assert!(SlidingSyncListLoadingState::FullyLoaded.is_fully_loaded());
    }

    #[test]
    fn test_once_built() {
        let (sender, _receiver) = channel(1);

        let probe = Arc::new(Mutex::new(Cell::new(false)));
        let probe_clone = probe.clone();

        let _list = SlidingSyncList::builder("testing")
            .once_built(move |list| {
                let mut probe_lock = probe.lock().unwrap();
                *probe_lock.get_mut() = true;

                list
            })
            .build(sender);

        let probe_lock = probe_clone.lock().unwrap();
        assert!(probe_lock.get());
    }
}
