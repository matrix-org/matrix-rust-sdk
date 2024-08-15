use std::{
    collections::BTreeMap,
    fmt::Debug,
    sync::{Arc, RwLock as StdRwLock},
    time::Duration,
};

use matrix_sdk_base::sliding_sync::http;
use matrix_sdk_common::timer;
use ruma::OwnedRoomId;
use tokio::sync::{broadcast::channel, Mutex as AsyncMutex, RwLock as AsyncRwLock};
use url::Url;

use super::{
    cache::{format_storage_key_prefix, restore_sliding_sync_state},
    sticky_parameters::SlidingSyncStickyManager,
    Error, SlidingSync, SlidingSyncInner, SlidingSyncListBuilder, SlidingSyncPositionMarkers,
};
use crate::{sliding_sync::SlidingSyncStickyParameters, Client, Result};

/// Configuration for a Sliding Sync instance.
///
/// Get a new builder with methods like [`crate::Client::sliding_sync`], or
/// [`crate::SlidingSync::builder`].
#[derive(Debug, Clone)]
pub struct SlidingSyncBuilder {
    id: String,
    storage_key: String,
    sliding_sync_proxy: Option<Url>,
    client: Client,
    lists: Vec<SlidingSyncListBuilder>,
    extensions: Option<http::request::Extensions>,
    subscriptions: BTreeMap<OwnedRoomId, http::request::RoomSubscription>,
    poll_timeout: Duration,
    network_timeout: Duration,
    #[cfg(feature = "e2e-encryption")]
    share_pos: bool,
}

impl SlidingSyncBuilder {
    pub(super) fn new(id: String, client: Client) -> Result<Self, Error> {
        if id.len() > 16 {
            Err(Error::InvalidSlidingSyncIdentifier)
        } else {
            let storage_key =
                format_storage_key_prefix(&id, client.user_id().ok_or(Error::UnauthenticatedUser)?);

            Ok(Self {
                id,
                storage_key,
                sliding_sync_proxy: None,
                client,
                lists: Vec::new(),
                extensions: None,
                subscriptions: BTreeMap::new(),
                poll_timeout: Duration::from_secs(30),
                network_timeout: Duration::from_secs(30),
                #[cfg(feature = "e2e-encryption")]
                share_pos: false,
            })
        }
    }

    /// Set the sliding sync proxy URL.
    ///
    /// Note you might not need that in general, since the client uses the
    /// `.well-known` endpoint to automatically find the sliding sync proxy
    /// URL. This method should only be called if the proxy is at a
    /// different URL than the one publicized in the `.well-known` endpoint.
    pub fn sliding_sync_proxy(mut self, value: Url) -> Self {
        self.sliding_sync_proxy = Some(value);
        self
    }

    /// Add the given list to the lists.
    ///
    /// Replace any list with the same name.
    pub fn add_list(mut self, list_builder: SlidingSyncListBuilder) -> Self {
        self.lists.push(list_builder);
        self
    }

    /// Enroll the list in caching, reloads it from the cache if possible, and
    /// adds it to the list of lists.
    ///
    /// This will raise an error if there was a I/O error reading from the
    /// cache.
    ///
    /// Replace any list with the same name.
    pub async fn add_cached_list(self, mut list: SlidingSyncListBuilder) -> Result<Self> {
        let _timer = timer!(format!("restoring (loading+processing) list {}", list.name));

        list.set_cached_and_reload(&self.client, &self.storage_key).await?;

        Ok(self.add_list(list))
    }

    /// Activate e2ee, to-device-message, account data, typing and receipt
    /// extensions if not yet configured.
    ///
    /// Will leave any extension configuration found untouched, so the order
    /// does not matter.
    pub fn with_all_extensions(mut self) -> Self {
        {
            let cfg = self.extensions.get_or_insert_with(Default::default);
            if cfg.to_device.enabled.is_none() {
                cfg.to_device.enabled = Some(true);
            }

            if cfg.e2ee.enabled.is_none() {
                cfg.e2ee.enabled = Some(true);
            }

            if cfg.account_data.enabled.is_none() {
                cfg.account_data.enabled = Some(true);
            }

            if cfg.receipts.enabled.is_none() {
                cfg.receipts.enabled = Some(true);
            }

            if cfg.typing.enabled.is_none() {
                cfg.typing.enabled = Some(true);
            }
        }
        self
    }

    /// Set the E2EE extension configuration.
    pub fn with_e2ee_extension(mut self, e2ee: http::request::E2EE) -> Self {
        self.extensions.get_or_insert_with(Default::default).e2ee = e2ee;
        self
    }

    /// Unset the E2EE extension configuration.
    pub fn without_e2ee_extension(mut self) -> Self {
        self.extensions.get_or_insert_with(Default::default).e2ee = http::request::E2EE::default();
        self
    }

    /// Set the ToDevice extension configuration.
    pub fn with_to_device_extension(mut self, to_device: http::request::ToDevice) -> Self {
        self.extensions.get_or_insert_with(Default::default).to_device = to_device;
        self
    }

    /// Unset the ToDevice extension configuration.
    pub fn without_to_device_extension(mut self) -> Self {
        self.extensions.get_or_insert_with(Default::default).to_device =
            http::request::ToDevice::default();
        self
    }

    /// Set the account data extension configuration.
    pub fn with_account_data_extension(mut self, account_data: http::request::AccountData) -> Self {
        self.extensions.get_or_insert_with(Default::default).account_data = account_data;
        self
    }

    /// Unset the account data extension configuration.
    pub fn without_account_data_extension(mut self) -> Self {
        self.extensions.get_or_insert_with(Default::default).account_data =
            http::request::AccountData::default();
        self
    }

    /// Set the Typing extension configuration.
    pub fn with_typing_extension(mut self, typing: http::request::Typing) -> Self {
        self.extensions.get_or_insert_with(Default::default).typing = typing;
        self
    }

    /// Unset the Typing extension configuration.
    pub fn without_typing_extension(mut self) -> Self {
        self.extensions.get_or_insert_with(Default::default).typing =
            http::request::Typing::default();
        self
    }

    /// Set the Receipt extension configuration.
    pub fn with_receipt_extension(mut self, receipt: http::request::Receipts) -> Self {
        self.extensions.get_or_insert_with(Default::default).receipts = receipt;
        self
    }

    /// Unset the Receipt extension configuration.
    pub fn without_receipt_extension(mut self) -> Self {
        self.extensions.get_or_insert_with(Default::default).receipts =
            http::request::Receipts::default();
        self
    }

    /// Sets a custom timeout duration for the sliding sync polling endpoint.
    ///
    /// This is the maximum time to wait before the sliding sync server returns
    /// the long-polling request. If no events (or other data) become
    /// available before this time elapses, the server will a return a
    /// response with empty fields.
    ///
    /// There's an additional network timeout on top of that that can be
    /// configured with [`Self::network_timeout`].
    pub fn poll_timeout(mut self, timeout: Duration) -> Self {
        self.poll_timeout = timeout;
        self
    }

    /// Sets a custom network timeout for the sliding sync polling.
    ///
    /// This is not the polling timeout that can be configured with
    /// [`Self::poll_timeout`], but an additional timeout that will be
    /// added to the former.
    pub fn network_timeout(mut self, timeout: Duration) -> Self {
        self.network_timeout = timeout;
        self
    }

    /// Should the sliding sync instance share its sync position through
    /// storage?
    ///
    /// In general, sliding sync instances will cache the sync position (`pos`
    /// field in the request) in internal memory. It can be useful, in
    /// multi-process scenarios, to save it into some shared storage so that one
    /// sliding sync instance running across two different processes can
    /// continue with the same sync position it had before being stopped.
    #[cfg(feature = "e2e-encryption")]
    pub fn share_pos(mut self) -> Self {
        self.share_pos = true;
        self
    }

    /// Build the Sliding Sync.
    ///
    /// If `self.storage_key` is `Some(_)`, load the cached data from cold
    /// storage.
    pub async fn build(self) -> Result<SlidingSync> {
        let client = self.client;

        let (internal_channel_sender, _internal_channel_receiver) = channel(8);

        let mut lists = BTreeMap::new();

        for list_builder in self.lists {
            let list = list_builder.build(internal_channel_sender.clone());

            lists.insert(list.name().to_owned(), list);
        }

        // Reload existing state from the cache.
        let restored_fields =
            restore_sliding_sync_state(&client, &self.storage_key, &lists).await?;

        let (pos, rooms) = if let Some(fields) = restored_fields {
            #[cfg(feature = "e2e-encryption")]
            let pos = if self.share_pos { fields.pos } else { None };
            #[cfg(not(feature = "e2e-encryption"))]
            let pos = None;

            (pos, fields.rooms)
        } else {
            (None, BTreeMap::new())
        };

        #[cfg(feature = "e2e-encryption")]
        let share_pos = self.share_pos;
        #[cfg(not(feature = "e2e-encryption"))]
        let share_pos = false;

        let rooms = AsyncRwLock::new(rooms);
        let lists = AsyncRwLock::new(lists);

        // Use the configured sliding sync proxy, or if not set, try to use the one
        // auto-discovered by the client, if any.
        let sliding_sync_proxy = self.sliding_sync_proxy.or_else(|| client.sliding_sync_proxy());

        Ok(SlidingSync::new(SlidingSyncInner {
            id: self.id,
            sliding_sync_proxy,

            client,
            storage_key: self.storage_key,
            share_pos,

            lists,
            rooms,

            position: Arc::new(AsyncMutex::new(SlidingSyncPositionMarkers { pos })),

            sticky: StdRwLock::new(SlidingSyncStickyManager::new(
                SlidingSyncStickyParameters::new(
                    self.subscriptions,
                    self.extensions.unwrap_or_default(),
                ),
            )),

            internal_channel: internal_channel_sender,

            poll_timeout: self.poll_timeout,
            network_timeout: self.network_timeout,
        }))
    }
}
