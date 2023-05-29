use std::{collections::BTreeMap, fmt::Debug, sync::RwLock as StdRwLock};

use ruma::{
    api::client::sync::sync_events::v4::{
        self, AccountDataConfig, E2EEConfig, ExtensionsConfig, ReceiptsConfig, ToDeviceConfig,
        TypingConfig,
    },
    events::TimelineEventType,
    OwnedRoomId,
};
use tokio::sync::{mpsc::channel, RwLock as AsyncRwLock};
use url::Url;

use super::{
    cache::restore_sliding_sync_state, SlidingSync, SlidingSyncInner, SlidingSyncListBuilder,
    SlidingSyncPositionMarkers, SlidingSyncRoom,
};
use crate::{Client, Result};

/// Configuration for a Sliding Sync instance.
///
/// Get a new builder with methods like [`crate::Client::sliding_sync`], or
/// [`crate::SlidingSync::builder`].
#[derive(Debug, Clone)]
pub struct SlidingSyncBuilder {
    storage_key: Option<String>,
    homeserver: Option<Url>,
    client: Client,
    lists: Vec<SlidingSyncListBuilder>,
    bump_event_types: Vec<TimelineEventType>,
    extensions: Option<ExtensionsConfig>,
    subscriptions: BTreeMap<OwnedRoomId, v4::RoomSubscription>,
    rooms: BTreeMap<OwnedRoomId, SlidingSyncRoom>,
}

impl SlidingSyncBuilder {
    pub(super) fn new(client: Client) -> Self {
        Self {
            storage_key: None,
            homeserver: None,
            client,
            lists: Vec::new(),
            bump_event_types: Vec::new(),
            extensions: None,
            subscriptions: BTreeMap::new(),
            rooms: BTreeMap::new(),
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
    /// This will raise an error if a [`storage_key()`][Self::storage_key] was
    /// not set, or if there was a I/O error reading from the cache.
    ///
    /// Replace any list with the same name.
    pub async fn add_cached_list(mut self, mut list: SlidingSyncListBuilder) -> Result<Self> {
        let Some(ref storage_key) = self.storage_key else {
            return Err(super::error::Error::MissingStorageKeyForCaching.into());
        };

        let reloaded_rooms = list.set_cached_and_reload(&self.client, storage_key).await?;

        for (key, frozen) in reloaded_rooms {
            self.rooms
                .entry(key)
                .or_insert_with(|| SlidingSyncRoom::from_frozen(frozen, self.client.clone()));
        }

        Ok(self.add_list(list))
    }

    /// Activate e2ee, to-device-message and account data extensions if not yet
    /// configured.
    ///
    /// Will leave any extension configuration found untouched, so the order
    /// does not matter.
    pub fn with_common_extensions(mut self) -> Self {
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
    pub fn with_e2ee_extension(mut self, e2ee: E2EEConfig) -> Self {
        self.extensions.get_or_insert_with(Default::default).e2ee = e2ee;
        self
    }

    /// Unset the E2EE extension configuration.
    pub fn without_e2ee_extension(mut self) -> Self {
        self.extensions.get_or_insert_with(Default::default).e2ee = E2EEConfig::default();
        self
    }

    /// Set the ToDevice extension configuration.
    pub fn with_to_device_extension(mut self, to_device: ToDeviceConfig) -> Self {
        self.extensions.get_or_insert_with(Default::default).to_device = to_device;
        self
    }

    /// Unset the ToDevice extension configuration.
    pub fn without_to_device_extension(mut self) -> Self {
        self.extensions.get_or_insert_with(Default::default).to_device = ToDeviceConfig::default();
        self
    }

    /// Set the account data extension configuration.
    pub fn with_account_data_extension(mut self, account_data: AccountDataConfig) -> Self {
        self.extensions.get_or_insert_with(Default::default).account_data = account_data;
        self
    }

    /// Unset the account data extension configuration.
    pub fn without_account_data_extension(mut self) -> Self {
        self.extensions.get_or_insert_with(Default::default).account_data =
            AccountDataConfig::default();
        self
    }

    /// Set the Typing extension configuration.
    pub fn with_typing_extension(mut self, typing: TypingConfig) -> Self {
        self.extensions.get_or_insert_with(Default::default).typing = typing;
        self
    }

    /// Unset the Typing extension configuration.
    pub fn without_typing_extension(mut self) -> Self {
        self.extensions.get_or_insert_with(Default::default).typing = TypingConfig::default();
        self
    }

    /// Set the Receipt extension configuration.
    pub fn with_receipt_extension(mut self, receipt: ReceiptsConfig) -> Self {
        self.extensions.get_or_insert_with(Default::default).receipts = receipt;
        self
    }

    /// Unset the Receipt extension configuration.
    pub fn without_receipt_extension(mut self) -> Self {
        self.extensions.get_or_insert_with(Default::default).receipts = ReceiptsConfig::default();
        self
    }

    /// Allowlist of event types which should be considered recent activity
    /// when sorting `by_recency`. By omitting event types, clients can ensure
    /// that uninteresting events (e.g. a profile rename) do not cause a
    /// room to jump to the top of its list(s). Empty or
    /// omitted `bump_event_types` have no effect: all events in a room will
    /// be considered recent activity.
    pub fn bump_event_types(mut self, bump_event_types: &[TimelineEventType]) -> Self {
        self.bump_event_types = bump_event_types.to_vec();
        self
    }

    /// Build the Sliding Sync.
    ///
    /// If `self.storage_key` is `Some(_)`, load the cached data from cold
    /// storage.
    pub async fn build(self) -> Result<SlidingSync> {
        let client = self.client;

        let mut delta_token = None;
        let mut to_device_token = None;

        let (internal_channel_sender, internal_channel_receiver) = channel(8);

        let mut lists = BTreeMap::new();

        for list_builder in self.lists {
            let list = list_builder.build(internal_channel_sender.clone());

            lists.insert(list.name().to_owned(), list);
        }

        // Load an existing state from the cache.
        if let Some(storage_key) = &self.storage_key {
            restore_sliding_sync_state(
                &client,
                storage_key,
                &lists,
                &mut delta_token,
                &mut to_device_token,
            )
            .await?;
        }

        let rooms = StdRwLock::new(self.rooms);
        let lists = StdRwLock::new(lists);

        Ok(SlidingSync::new(SlidingSyncInner {
            homeserver: self.homeserver,
            client,
            storage_key: self.storage_key,

            lists,
            rooms,
            bump_event_types: self.bump_event_types,

            extensions: self.extensions.unwrap_or_default(),
            reset_counter: Default::default(),

            position: StdRwLock::new(SlidingSyncPositionMarkers {
                pos: None,
                delta_token,
                to_device_token,
            }),

            room_subscriptions: StdRwLock::new(self.subscriptions),
            room_unsubscriptions: Default::default(),

            internal_channel: (
                internal_channel_sender,
                AsyncRwLock::new(internal_channel_receiver),
            ),
        }))
    }
}
