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
    events::TimelineEventType,
    OwnedRoomId,
};
use url::Url;

use super::{
    cache::restore_sliding_sync_state, Error, SlidingSync, SlidingSyncInner, SlidingSyncList,
    SlidingSyncPositionMarkers, SlidingSyncRoom,
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
    bump_event_types: Vec<TimelineEventType>,
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
            bump_event_types: Vec::new(),
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
            let mut cfg = self.extensions.get_or_insert_with(Default::default);
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
    pub async fn build(mut self) -> Result<SlidingSync> {
        let client = self.client.ok_or(Error::BuildMissingField("client"))?;

        let mut delta_token = None;
        let mut rooms_found: BTreeMap<OwnedRoomId, SlidingSyncRoom> = BTreeMap::new();

        // Load an existing state from the cache.
        if let Some(storage_key) = &self.storage_key {
            restore_sliding_sync_state(
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
            bump_event_types: self.bump_event_types,

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
