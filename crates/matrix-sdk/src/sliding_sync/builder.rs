use std::{
    collections::BTreeMap,
    fmt::Debug,
    sync::{Arc, Mutex, RwLock as StdRwLock},
};

use eyeball::Observable;
use ruma::{
    api::client::sync::sync_events::v4::{
        self, AccountDataConfig, E2EEConfig, ExtensionsConfig, ReceiptConfig, ToDeviceConfig,
        TypingConfig,
    },
    assign, OwnedRoomId,
};
use tracing::trace;
use url::Url;

use super::{
    Error, FrozenSlidingSync, FrozenSlidingSyncList, SlidingSync, SlidingSyncList,
    SlidingSyncListBuilder, SlidingSyncRoom,
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
        self.lists.insert(list.name.clone(), list);

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
    pub fn with_receipt_extension(mut self, receipt: ReceiptConfig) -> Self {
        self.extensions.get_or_insert_with(Default::default).receipt = Some(receipt);
        self
    }

    /// Unset the Receipt extension configuration.
    pub fn without_receipt_extension(mut self) -> Self {
        self.extensions.get_or_insert_with(Default::default).receipt = None;
        self
    }

    /// Build the Sliding Sync.
    ///
    /// If `self.storage_key` is `Some(_)`, load the cached data from cold
    /// storage.
    pub async fn build(mut self) -> Result<SlidingSync> {
        let client = self.client.ok_or(Error::BuildMissingField("client"))?;

        let mut delta_token_inner = None;
        let mut rooms_found: BTreeMap<OwnedRoomId, SlidingSyncRoom> = BTreeMap::new();

        if let Some(storage_key) = &self.storage_key {
            trace!(storage_key, "trying to load from cold");

            for (name, list) in &mut self.lists {
                if let Some(frozen_list) = client
                    .store()
                    .get_custom_value(format!("{storage_key}::{name}").as_bytes())
                    .await?
                    .map(|v| serde_json::from_slice::<FrozenSlidingSyncList>(&v))
                    .transpose()?
                {
                    trace!(name, "frozen for list found");

                    let FrozenSlidingSyncList { rooms_count, rooms_list, rooms } = frozen_list;
                    list.set_from_cold(rooms_count, rooms_list);

                    for (key, frozen_room) in rooms.into_iter() {
                        rooms_found.entry(key).or_insert_with(|| {
                            SlidingSyncRoom::from_frozen(frozen_room, client.clone())
                        });
                    }
                } else {
                    trace!(name, "no frozen state for list found");
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
                        self.extensions.get_or_insert_with(Default::default).to_device.as_mut()
                    {
                        to_device_ext.since = Some(since);
                    }
                }

                delta_token_inner = delta_token;
            }

            trace!("sync unfrozen done");
        };

        trace!(len = rooms_found.len(), "rooms unfrozen");

        let rooms = Arc::new(StdRwLock::new(rooms_found));
        let lists = Arc::new(StdRwLock::new(self.lists));

        Ok(SlidingSync {
            homeserver: self.homeserver,
            client,
            storage_key: self.storage_key,

            lists,
            rooms,

            extensions: Mutex::new(self.extensions).into(),
            reset_counter: Default::default(),

            pos: Arc::new(StdRwLock::new(Observable::new(None))),
            delta_token: Arc::new(StdRwLock::new(Observable::new(delta_token_inner))),
            subscriptions: Arc::new(StdRwLock::new(self.subscriptions)),
            unsubscribe: Default::default(),
        })
    }
}
