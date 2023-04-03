use std::{
    collections::BTreeMap,
    fmt::Debug,
    sync::{Mutex, RwLock as StdRwLock},
};

use eyeball::unique::Observable;
use ruma::{
    api::client::sync::sync_events::v4::{
        self, AccountDataConfig, E2EEConfig, ExtensionsConfig, ReceiptsConfig, ToDeviceConfig,
        TypingConfig,
    },
    assign, OwnedRoomId,
};
use tracing::{trace, warn};
use url::Url;

use super::{
    Error, FrozenSlidingSync, FrozenSlidingSyncList, SlidingSync, SlidingSyncInner,
    SlidingSyncList, SlidingSyncListBuilder, SlidingSyncPositionMarkers, SlidingSyncRoom,
};
use crate::{Client, Result};

/// Configuration for a Sliding Sync instance.
///
/// Get a new builder with methods like [`crate::Client::sliding_sync`], or
/// [`crate::SlidingSync::builder`].
#[derive(Clone, Debug)]
pub struct SlidingSyncBuilder {
    storage_key: Option<String>,
    homeserver: Option<Url>,
    client: Option<Client>,
    lists: BTreeMap<String, SlidingSyncList>,
    extensions: Option<ExtensionsConfig>,
    subscriptions: BTreeMap<OwnedRoomId, v4::RoomSubscription>,
}

impl SlidingSyncBuilder {
    pub(super) fn new() -> Self {
        Self {
            storage_key: None,
            homeserver: None,
            client: None,
            lists: BTreeMap::new(),
            extensions: None,
            subscriptions: BTreeMap::new(),
        }
    }

    /// Set the storage key to keep this cache at and load it from.
    pub fn storage_key(mut self, value: Option<String>) -> Self {
        self.storage_key = value;
        self
    }

    /// Set the homeserver for sliding sync only.
    pub fn homeserver(mut self, value: Url) -> Self {
        self.homeserver = Some(value);
        self
    }

    /// Set the client this sliding sync will be using.
    pub fn client(mut self, value: Client) -> Self {
        self.client = Some(value);
        self
    }

    pub(super) fn subscriptions(
        mut self,
        value: BTreeMap<OwnedRoomId, v4::RoomSubscription>,
    ) -> Self {
        self.subscriptions = value;
        self
    }

    /// Convenience function to add a full-sync list to the builder
    pub fn add_fullsync_list(self) -> Self {
        self.add_list(
            SlidingSyncListBuilder::default_with_fullsync()
                .build()
                .expect("Building default full sync list doesn't fail"),
        )
    }

    /// The cold cache key to read from and store the frozen state at
    pub fn cold_cache<T: ToString>(mut self, name: T) -> Self {
        self.storage_key = Some(name.to_string());
        self
    }

    /// Do not use the cold cache
    pub fn no_cold_cache(mut self) -> Self {
        self.storage_key = None;
        self
    }

    /// Reset the lists to `None`
    pub fn no_lists(mut self) -> Self {
        self.lists.clear();
        self
    }

    /// Add the given list to the lists.
    ///
    /// Replace any list with the name.
    pub fn add_list(mut self, list: SlidingSyncList) -> Self {
        self.lists.insert(list.name().to_owned(), list);

        self
    }

    /// Activate e2ee, to-device-message and account data extensions if not yet
    /// configured.
    ///
    /// Will leave any extension configuration found untouched, so the order
    /// does not matter.
    pub fn with_common_extensions(mut self) -> Self {
        {
            let mut cfg = self.extensions.get_or_insert_with(Default::default);
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
            let mut cfg = self.extensions.get_or_insert_with(Default::default);
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

            if cfg.receipts.is_none() {
                cfg.receipts = Some(assign!(ReceiptsConfig::default(), { enabled: Some(true) }));
            }

            if cfg.typing.is_none() {
                cfg.typing = Some(assign!(TypingConfig::default(), { enabled: Some(true) }));
            }
        }
        self
    }

    /// Set the E2EE extension configuration.
    pub fn with_e2ee_extension(mut self, e2ee: E2EEConfig) -> Self {
        self.extensions.get_or_insert_with(Default::default).e2ee = Some(e2ee);
        self
    }

    /// Unset the E2EE extension configuration.
    pub fn without_e2ee_extension(mut self) -> Self {
        self.extensions.get_or_insert_with(Default::default).e2ee = None;
        self
    }

    /// Set the ToDevice extension configuration.
    pub fn with_to_device_extension(mut self, to_device: ToDeviceConfig) -> Self {
        self.extensions.get_or_insert_with(Default::default).to_device = Some(to_device);
        self
    }

    /// Unset the ToDevice extension configuration.
    pub fn without_to_device_extension(mut self) -> Self {
        self.extensions.get_or_insert_with(Default::default).to_device = None;
        self
    }

    /// Set the account data extension configuration.
    pub fn with_account_data_extension(mut self, account_data: AccountDataConfig) -> Self {
        self.extensions.get_or_insert_with(Default::default).account_data = Some(account_data);
        self
    }

    /// Unset the account data extension configuration.
    pub fn without_account_data_extension(mut self) -> Self {
        self.extensions.get_or_insert_with(Default::default).account_data = None;
        self
    }

    /// Set the Typing extension configuration.
    pub fn with_typing_extension(mut self, typing: TypingConfig) -> Self {
        self.extensions.get_or_insert_with(Default::default).typing = Some(typing);
        self
    }

    /// Unset the Typing extension configuration.
    pub fn without_typing_extension(mut self) -> Self {
        self.extensions.get_or_insert_with(Default::default).typing = None;
        self
    }

    /// Set the Receipt extension configuration.
    pub fn with_receipt_extension(mut self, receipt: ReceiptsConfig) -> Self {
        self.extensions.get_or_insert_with(Default::default).receipts = Some(receipt);
        self
    }

    /// Unset the Receipt extension configuration.
    pub fn without_receipt_extension(mut self) -> Self {
        self.extensions.get_or_insert_with(Default::default).receipts = None;
        self
    }

    /// Build the Sliding Sync.
    ///
    /// If `self.storage_key` is `Some(_)`, load the cached data from cold
    /// storage.
    pub async fn build(mut self) -> Result<SlidingSync> {
        let client = self.client.ok_or(Error::BuildMissingField("client"))?;

        let mut delta_token = None;
        let mut rooms_found: BTreeMap<OwnedRoomId, SlidingSyncRoom> = BTreeMap::new();

        // Load an existing state from the cache.
        if let Some(storage_key) = &self.storage_key {
            build_from_storage(
                &client,
                storage_key,
                &mut self.lists,
                &mut delta_token,
                &mut rooms_found,
                &mut self.extensions,
            )
            .await?;
        }

        let rooms = StdRwLock::new(rooms_found);
        let lists = StdRwLock::new(self.lists);

        Ok(SlidingSync::new(SlidingSyncInner {
            homeserver: self.homeserver,
            client,
            storage_key: self.storage_key,

            lists,
            rooms,

            extensions: Mutex::new(self.extensions),
            reset_counter: Default::default(),

            position: StdRwLock::new(SlidingSyncPositionMarkers {
                pos: Observable::new(None),
                delta_token: Observable::new(delta_token),
            }),

            subscriptions: StdRwLock::new(self.subscriptions),
            unsubscribe: Default::default(),
        }))
    }
}

fn format_storage_key_for_sliding_sync(storage_key: &str) -> String {
    format!("sliding_sync_store::{storage_key}")
}

fn format_storage_key_for_sliding_sync_list(storage_key: &str, list_name: &str) -> String {
    format!("sliding_sync_store::{storage_key}::{list_name}")
}

/// Clean the storage for everything related to `SlidingSync`.
async fn clean_storage(
    client: &Client,
    storage_key: &str,
    lists: &BTreeMap<String, SlidingSyncList>,
) {
    let storage = client.store();

    for list_name in lists.keys() {
        let storage_key_for_list = format_storage_key_for_sliding_sync_list(storage_key, list_name);

        let _ = storage.remove_custom_value(storage_key_for_list.as_bytes()).await;
    }

    let _ = storage
        .remove_custom_value(format_storage_key_for_sliding_sync(storage_key).as_bytes())
        .await;
}

/// Build the `SlidingSync` and siblings from the storage.
async fn build_from_storage(
    client: &Client,
    storage_key: &str,
    lists: &mut BTreeMap<String, SlidingSyncList>,
    delta_token: &mut Option<String>,
    rooms_found: &mut BTreeMap<OwnedRoomId, SlidingSyncRoom>,
    extensions: &mut Option<ExtensionsConfig>,
) -> Result<()> {
    let storage = client.store();

    let mut collected_lists_and_frozen_lists = Vec::with_capacity(lists.len());

    // Preload the `FrozenSlidingSyncList` objects from the cache.
    //
    // Even if a cache was detected as obsolete, we go over all of them, so that we
    // are sure all obsolete cache entries are removed.
    for (list_name, list) in lists.iter_mut() {
        let storage_key_for_list = format_storage_key_for_sliding_sync_list(storage_key, list_name);

        match storage
            .get_custom_value(storage_key_for_list.as_bytes())
            .await?
            .map(|custom_value| serde_json::from_slice::<FrozenSlidingSyncList>(&custom_value))
        {
            // List has been found and successfully deserialized.
            Some(Ok(frozen_list)) => {
                trace!(list_name, "successfully read the list from cache");

                // Keep it for later.
                collected_lists_and_frozen_lists.push((list, frozen_list));
            }

            // List has been found, but it wasn't possible to deserialize it. It's declared
            // as obsolete. The main reason might be that the internal representation of a
            // `SlidingSyncList` might have changed. Instead of considering this as a strong
            // error, we remove the entry from the cache and keep the list in its initial
            // state.
            Some(Err(_)) => {
                warn!(
                    list_name,
                    "failed to deserialize the list from the cache, it is obsolete; removing the cache entry!"
                );

                // Let's clear everything and stop here.
                clean_storage(client, storage_key, lists).await;

                return Ok(());
            }

            None => {
                trace!(list_name, "failed to find the list in the cache");

                // A missing cache doesn't make anything obsolete.
                // We just do nothing here.
            }
        }
    }

    // Preload the `SlidingSync` object from the cache.
    match storage
        .get_custom_value(format_storage_key_for_sliding_sync(storage_key).as_bytes())
        .await?
        .map(|custom_value| serde_json::from_slice::<FrozenSlidingSync>(&custom_value))
    {
        // `SlidingSync` has been found and successfully deserialized.
        Some(Ok(FrozenSlidingSync { to_device_since, delta_token: frozen_delta_token })) => {
            trace!("successfully read the `SlidingSync` from the cache");

            // OK, at this step, everything has been loaded successfully from the cache.

            // Let's update all the `SlidingSyncList`.
            for (list, FrozenSlidingSyncList { maximum_number_of_rooms, room_list, rooms }) in
                collected_lists_and_frozen_lists
            {
                list.set_from_cold(maximum_number_of_rooms, room_list);

                for (key, frozen_room) in rooms.into_iter() {
                    rooms_found.entry(key).or_insert_with(|| {
                        SlidingSyncRoom::from_frozen(frozen_room, client.clone())
                    });
                }
            }

            // Let's update the `SlidingSync`.
            if let Some(since) = to_device_since {
                if let Some(to_device_ext) =
                    extensions.get_or_insert_with(Default::default).to_device.as_mut()
                {
                    to_device_ext.since = Some(since);
                }
            }

            *delta_token = frozen_delta_token;
        }

        // `SlidingSync` has been found, but it wasn't possible to deserialize it. It's
        // declared as obsolete. The main reason might be that the internal
        // representation of a `SlidingSync` might have changed.
        // Instead of considering this as a strong error, we remove
        // the entry from the cache and keep `SlidingSync` in its initial
        // state.
        Some(Err(_)) => {
            warn!(
                "failed to deserialize `SlidingSync` from the cache, it is obsolote; removing the cache entry!"
            );

            // Let's clear everything and stop here.
            clean_storage(client, storage_key, lists).await;

            return Ok(());
        }

        None => {
            trace!("Failed to find the `SlidingSync` object in the cache");
        }
    }

    Ok(())
}
