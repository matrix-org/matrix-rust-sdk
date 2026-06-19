use std::{
    collections::BTreeMap,
    fmt::Debug,
    sync::{Arc, RwLock as StdRwLock},
    time::Duration,
};

use cfg_if::cfg_if;
use matrix_sdk_common::timer;
use ruma::{OwnedRoomId, api::client::sync::sync_events::v5 as http};
use tokio::sync::{Mutex as AsyncMutex, RwLock as AsyncRwLock, broadcast::channel};

use super::{
    Error, SlidingSync, SlidingSyncInner, SlidingSyncListBuilder, SlidingSyncPositionMarkers,
    Version, cache::format_storage_key_prefix,
};
use crate::{Client, Result};

/// Configuration for a Sliding Sync instance.
///
/// Get a new builder with methods like [`crate::Client::sliding_sync`], or
/// [`crate::SlidingSync::builder`].
#[derive(Debug, Clone)]
pub struct SlidingSyncBuilder {
    id: String,
    storage_key: String,
    version: Option<Version>,
    client: Client,
    lists: Vec<SlidingSyncListBuilder>,
    extensions: Option<http::request::Extensions>,
    room_subscriptions: BTreeMap<OwnedRoomId, http::request::RoomSubscription>,
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
                version: None,
                client,
                lists: Vec::new(),
                extensions: None,
                room_subscriptions: BTreeMap::new(),
                poll_timeout: Duration::from_secs(30),
                network_timeout: Duration::from_secs(30),
                #[cfg(feature = "e2e-encryption")]
                share_pos: false,
            })
        }
    }

    /// Set a specific version that will override the one from the [`Client`].
    pub fn version(mut self, version: Version) -> Self {
        self.version = Some(version);
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

    /// Set the Threads subscriptions extension configuration.
    pub fn with_thread_subscriptions_extension(
        mut self,
        thread_subscriptions: http::request::ThreadSubscriptions,
    ) -> Self {
        self.extensions.get_or_insert_with(Default::default).thread_subscriptions =
            thread_subscriptions;
        self
    }

    /// Unset the Threads subscriptions extension configuration.
    pub fn without_thread_subscriptions_extension(mut self) -> Self {
        self.extensions.get_or_insert_with(Default::default).thread_subscriptions =
            Default::default();
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
    #[allow(clippy::unused_async)] // Async is only used if the e2e-encryption feature is enabled.
    pub async fn build(self) -> Result<SlidingSync> {
        let client = self.client;

        let version = self.version.unwrap_or_else(|| client.sliding_sync_version());

        if matches!(version, Version::None) {
            return Err(crate::error::Error::SlidingSync(Box::new(Error::VersionIsMissing)));
        }

        let (internal_channel_sender, _internal_channel_receiver) = channel(8);

        let mut lists = BTreeMap::new();

        for list_builder in self.lists {
            let list = list_builder.build(internal_channel_sender.clone());

            lists.insert(list.name().to_owned(), list);
        }

        let (share_pos, pos) = {
            cfg_if! {
                if #[cfg(feature = "e2e-encryption")] {
                    if self.share_pos {
                        // If the sliding sync instance is configured to share its current sync
                        // position, we will restore it from the cache.
                        (true, super::cache::restore_sliding_sync_state(&client, &self.storage_key).await?.and_then(|fields| fields.pos))
                    } else {
                        (false, None)
                    }
                } else {
                    (false, None)
                }
            }
        };

        let lists = AsyncRwLock::new(lists);

        Ok(SlidingSync::new(SlidingSyncInner {
            id: self.id,

            client,
            storage_key: self.storage_key,
            share_pos,

            lists,

            position: Arc::new(AsyncMutex::new(SlidingSyncPositionMarkers { pos })),

            room_subscriptions: StdRwLock::new(self.room_subscriptions),
            extensions: self.extensions.unwrap_or_default(),

            internal_channel: internal_channel_sender,

            poll_timeout: self.poll_timeout,
            network_timeout: self.network_timeout,
        }))
    }
}
