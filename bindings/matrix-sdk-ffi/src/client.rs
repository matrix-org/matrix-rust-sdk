use std::sync::Arc;

use matrix_sdk::{
    config::SyncSettings,
    media::{MediaFormat, MediaRequest},
    ruma::{
        api::client::{
            filter::{FilterDefinition, LazyLoadOptions, RoomEventFilter, RoomFilter},
            sync::sync_events::v3::Filter,
        },
        events::room::MediaSource,
        TransactionId,
    },
    Client as MatrixClient, LoopCtrl,
};
use parking_lot::RwLock;

use super::{room::Room, ClientState, RestoreToken, RUNTIME};

impl std::ops::Deref for Client {
    type Target = MatrixClient;
    fn deref(&self) -> &MatrixClient {
        &self.client
    }
}

pub trait ClientDelegate: Sync + Send {
    fn did_receive_sync_update(&self);
}

#[derive(Clone)]
pub struct Client {
    client: MatrixClient,
    state: Arc<RwLock<ClientState>>,
    delegate: Arc<RwLock<Option<Box<dyn ClientDelegate>>>>,
}

impl Client {
    pub fn new(client: MatrixClient, state: ClientState) -> Self {
        Client {
            client,
            state: Arc::new(RwLock::new(state)),
            delegate: Arc::new(RwLock::new(None)),
        }
    }

    pub fn set_delegate(&self, delegate: Option<Box<dyn ClientDelegate>>) {
        *self.delegate.write() = delegate;
    }

    pub fn start_sync(&self) {
        let client = self.client.clone();
        let state = self.state.clone();
        let delegate = self.delegate.clone();
        RUNTIME.spawn(async move {
            let mut filter = FilterDefinition::default();
            let mut room_filter = RoomFilter::default();
            let mut event_filter = RoomEventFilter::default();

            event_filter.lazy_load_options =
                LazyLoadOptions::Enabled { include_redundant_members: false };
            room_filter.state = event_filter;
            filter.room = room_filter;

            let filter_id = client.get_or_upload_filter("sync", filter).await.unwrap();

            let sync_settings = SyncSettings::new().filter(Filter::FilterId(&filter_id));

            client
                .sync_with_callback(sync_settings, |_| async {
                    if !state.read().has_first_synced {
                        state.write().has_first_synced = true
                    }

                    if state.read().should_stop_syncing {
                        state.write().is_syncing = false;
                        return LoopCtrl::Break;
                    } else if !state.read().is_syncing {
                        state.write().is_syncing = true;
                    }

                    if let Some(ref delegate) = *delegate.read() {
                        delegate.did_receive_sync_update()
                    }
                    LoopCtrl::Continue
                })
                .await;
        });
    }

    /// Indication whether we've received a first sync response since
    /// establishing the client (in memory)
    pub fn has_first_synced(&self) -> bool {
        self.state.read().has_first_synced
    }

    /// Indication whether we are currently syncing
    pub fn is_syncing(&self) -> bool {
        self.state.read().has_first_synced
    }

    /// Is this a guest account?
    pub fn is_guest(&self) -> bool {
        self.state.read().is_guest
    }

    pub fn restore_token(&self) -> anyhow::Result<String> {
        RUNTIME.block_on(async move {
            let session = self.client.session().expect("Missing session").clone();
            let homeurl = self.client.homeserver().await.into();
            Ok(serde_json::to_string(&RestoreToken {
                session,
                homeurl,
                is_guest: self.state.read().is_guest,
            })?)
        })
    }

    pub fn rooms(&self) -> Vec<Arc<Room>> {
        self.client.rooms().into_iter().map(|room| Arc::new(Room::new(room))).collect()
    }

    pub fn user_id(&self) -> anyhow::Result<String> {
        let user_id = self.client.user_id().expect("No User ID found");
        Ok(user_id.to_string())
    }

    pub fn display_name(&self) -> anyhow::Result<String> {
        let l = self.client.clone();
        RUNTIME.block_on(async move {
            let display_name = l.account().get_display_name().await?.expect("No User ID found");
            Ok(display_name)
        })
    }

    pub fn avatar_url(&self) -> anyhow::Result<String> {
        let l = self.client.clone();
        RUNTIME.block_on(async move {
            let avatar_url = l.account().get_avatar_url().await?.expect("No User ID found");
            Ok(avatar_url.to_string())
        })
    }

    pub fn device_id(&self) -> anyhow::Result<String> {
        let device_id = self.client.device_id().expect("No Device ID found");
        Ok(device_id.to_string())
    }

    pub fn get_media_content(&self, media_source: Arc<MediaSource>) -> anyhow::Result<Vec<u8>> {
        let l = self.client.clone();
        let source = (*media_source).clone();

        RUNTIME.block_on(async move {
            Ok(l.get_media_content(&MediaRequest { source, format: MediaFormat::File }, true)
                .await?)
        })
    }
}

pub fn gen_transaction_id() -> String {
    TransactionId::new().to_string()
}
