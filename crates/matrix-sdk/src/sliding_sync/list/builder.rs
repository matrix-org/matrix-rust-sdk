//! Builder for [`SlidingSyncList`].

use std::{
    convert::identity,
    fmt,
    sync::{Arc, RwLock as StdRwLock},
};

use eyeball::SharedObservable;
use ruma::{api::client::sync::sync_events::v5 as http, events::StateEventType};
use tokio::sync::broadcast::Sender;

use super::{
    super::{SlidingSyncInternalMessage, cache::restore_sliding_sync_list},
    Bound, PollTimeout, SlidingSyncList, SlidingSyncListCachePolicy, SlidingSyncListInner,
    SlidingSyncListLoadingState, SlidingSyncListRequestGenerator, SlidingSyncMode,
};
use crate::Client;

/// Data that might have been read from the cache.
#[derive(Clone)]
struct SlidingSyncListCachedData {
    /// Total number of rooms that is possible to interact with the given list.
    /// See also comment of [`SlidingSyncList::maximum_number_of_rooms`].
    /// May be reloaded from the cache.
    maximum_number_of_rooms: Option<u32>,
}

/// Builder for [`SlidingSyncList`].
#[derive(Clone)]
pub struct SlidingSyncListBuilder {
    sync_mode: SlidingSyncMode,
    #[cfg(not(target_family = "wasm"))]
    requires_timeout: Arc<dyn Fn(&SlidingSyncListRequestGenerator) -> PollTimeout + Send + Sync>,
    #[cfg(target_family = "wasm")]
    requires_timeout: Arc<dyn Fn(&SlidingSyncListRequestGenerator) -> PollTimeout>,
    required_state: Vec<(StateEventType, String)>,
    filters: Option<http::request::ListFilters>,
    timeline_limit: Bound,
    pub(crate) name: String,

    /// Should this list be cached and reloaded from the cache?
    cache_policy: SlidingSyncListCachePolicy,

    /// If set, temporary data that's been read from the cache, reloaded from a
    /// `FrozenSlidingSyncList`.
    reloaded_cached_data: Option<SlidingSyncListCachedData>,

    #[cfg(not(target_family = "wasm"))]
    once_built: Arc<Box<dyn Fn(SlidingSyncList) -> SlidingSyncList + Send + Sync>>,
    #[cfg(target_family = "wasm")]
    once_built: Arc<Box<dyn Fn(SlidingSyncList) -> SlidingSyncList>>,
}

#[cfg(not(tarpaulin_include))]
impl fmt::Debug for SlidingSyncListBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Print debug values for the builder, except `once_built` which is ignored.
        formatter
            .debug_struct("SlidingSyncListBuilder")
            .field("sync_mode", &self.sync_mode)
            .field("required_state", &self.required_state)
            .field("filters", &self.filters)
            .field("timeline_limit", &self.timeline_limit)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl SlidingSyncListBuilder {
    pub(super) fn new(name: impl Into<String>) -> Self {
        Self {
            sync_mode: SlidingSyncMode::default(),
            requires_timeout: Arc::new(|request_generator| {
                if request_generator.is_fully_loaded() {
                    PollTimeout::Default
                } else {
                    PollTimeout::None
                }
            }),
            required_state: vec![
                (StateEventType::RoomEncryption, "".to_owned()),
                (StateEventType::RoomTombstone, "".to_owned()),
            ],
            filters: None,
            timeline_limit: 1,
            name: name.into(),
            reloaded_cached_data: None,
            cache_policy: SlidingSyncListCachePolicy::Disabled,
            once_built: Arc::new(Box::new(identity)),
        }
    }

    /// Runs a callback once the list has been built.
    ///
    /// If the list was cached, then the cached fields won't be available in
    /// this callback. Use the streams to get published versions of the
    /// cached fields, once they've been set.
    #[cfg(not(target_family = "wasm"))]
    pub fn once_built<C>(mut self, callback: C) -> Self
    where
        C: Fn(SlidingSyncList) -> SlidingSyncList + Send + Sync + 'static,
    {
        self.once_built = Arc::new(Box::new(callback));
        self
    }

    /// Runs a callback once the list has been built.
    ///
    /// If the list was cached, then the cached fields won't be available in
    /// this callback. Use the streams to get published versions of the
    /// cached fields, once they've been set.
    #[cfg(target_family = "wasm")]
    pub fn once_built<C>(mut self, callback: C) -> Self
    where
        C: Fn(SlidingSyncList) -> SlidingSyncList + 'static,
    {
        self.once_built = Arc::new(Box::new(callback));
        self
    }

    /// Which SlidingSyncMode to start this list under.
    pub fn sync_mode(mut self, value: impl Into<SlidingSyncMode>) -> Self {
        self.sync_mode = value.into();
        self
    }

    /// Custom function to decide whether this list requires a
    /// [`http::Request::timeout`] value.
    ///
    /// A list requires a `timeout` query if and only if we want the server to
    /// wait on new updates, i.e. to do a long-polling.
    #[cfg(not(target_family = "wasm"))]
    pub fn requires_timeout<F>(mut self, f: F) -> Self
    where
        F: Fn(&SlidingSyncListRequestGenerator) -> PollTimeout + Send + Sync + 'static,
    {
        self.requires_timeout = Arc::new(f);
        self
    }

    /// Custom function to decide whether this list requires a
    /// [`http::Request::timeout`] value.
    ///
    /// A list requires a `timeout` query if and only if we want the server to
    /// wait on new updates, i.e. to do a long-polling.
    #[cfg(target_family = "wasm")]
    pub fn requires_timeout<F>(mut self, f: F) -> Self
    where
        F: Fn(&SlidingSyncListRequestGenerator) -> PollTimeout + 'static,
    {
        self.requires_timeout = Arc::new(f);
        self
    }

    /// Required states to return per room.
    pub fn required_state(mut self, value: Vec<(StateEventType, String)>) -> Self {
        self.required_state = value;
        self
    }

    /// Any filters to apply to the query.
    pub fn filters(mut self, value: Option<http::request::ListFilters>) -> Self {
        self.filters = value;
        self
    }

    /// Set the limit of regular events to fetch for the timeline.
    pub fn timeline_limit(mut self, timeline_limit: Bound) -> Self {
        self.timeline_limit = timeline_limit;
        self
    }

    /// Set the limit of regular events to fetch for the timeline to 0.
    pub fn no_timeline_limit(mut self) -> Self {
        self.timeline_limit = 0;
        self
    }

    /// Marks this list as sync'd from the cache, and attempts to reload it from
    /// storage.
    ///
    /// Returns a mapping of the room's data read from the cache, to be
    /// incorporated into the `SlidingSync` bookkeepping.
    pub(in super::super) async fn set_cached_and_reload(
        &mut self,
        client: &Client,
        storage_key: &str,
    ) -> crate::Result<()> {
        self.cache_policy = SlidingSyncListCachePolicy::Enabled;

        if let Some(frozen_list) =
            restore_sliding_sync_list(client.state_store(), storage_key, &self.name).await?
        {
            assert!(
                self.reloaded_cached_data.is_none(),
                "can't call `set_cached_and_reload` twice"
            );
            self.reloaded_cached_data = Some(SlidingSyncListCachedData {
                maximum_number_of_rooms: frozen_list.maximum_number_of_rooms,
            });
            Ok(())
        } else {
            Ok(())
        }
    }

    /// Build the list.
    pub(in super::super) fn build(
        self,
        sliding_sync_internal_channel_sender: Sender<SlidingSyncInternalMessage>,
    ) -> SlidingSyncList {
        let list = SlidingSyncList {
            inner: Arc::new(SlidingSyncListInner {
                #[cfg(any(test, feature = "testing"))]
                sync_mode: StdRwLock::new(self.sync_mode.clone()),

                // From the builder
                filters: self.filters,
                required_state: self.required_state,
                timeline_limit: StdRwLock::new(self.timeline_limit),
                name: self.name,
                cache_policy: self.cache_policy,
                requires_timeout: self.requires_timeout,

                // Computed from the builder.
                request_generator: StdRwLock::new(SlidingSyncListRequestGenerator::new(
                    self.sync_mode,
                )),

                // Values read from deserialization, or that are still equal to the default values
                // otherwise.
                state: SharedObservable::new(Default::default()),
                maximum_number_of_rooms: SharedObservable::new(None),

                // Internal data.
                sliding_sync_internal_channel_sender,
            }),
        };

        let once_built = self.once_built;

        let list = once_built(list);

        // If we reloaded from the cache, update values in the list here.
        //
        // Note about ordering: because of the contract with the observables, the
        // initial values, if filled, have to be observable in the `once_built`
        // callback. That's why we're doing this here *after* constructing the
        // list, and not a few lines above.

        if let Some(SlidingSyncListCachedData { maximum_number_of_rooms }) =
            self.reloaded_cached_data
        {
            // Mark state as preloaded.
            list.inner.state.set(SlidingSyncListLoadingState::Preloaded);

            // Reload the maximum number of rooms.
            list.inner.maximum_number_of_rooms.set(maximum_number_of_rooms);
        }

        list
    }
}
