use std::sync::Arc;

use anyhow::anyhow;
use matrix_sdk::{
    config::SyncSettings,
    media::{MediaFormat, MediaRequest},
    ruma::{
        api::client::{
            account::whoami,
            filter::{FilterDefinition, LazyLoadOptions, RoomEventFilter, RoomFilter},
            session::get_login_types,
            sync::sync_events::v3::Filter,
        },
        events::room::MediaSource,
        TransactionId,
    },
    Client as MatrixClient, LoopCtrl, Session,
};
use parking_lot::RwLock;

use super::{
    room::Room, session_verification::SessionVerificationController, ClientState, RestoreToken,
    RUNTIME,
};

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
    session_verification_controller:
        Arc<matrix_sdk::locks::RwLock<Option<SessionVerificationController>>>,
}

impl Client {
    pub fn new(client: MatrixClient, state: ClientState) -> Self {
        Client {
            client,
            state: Arc::new(RwLock::new(state)),
            delegate: Arc::new(RwLock::new(None)),
            session_verification_controller: Arc::new(matrix_sdk::locks::RwLock::new(None)),
        }
    }

    /// Login using a username and password.
    pub fn login(&self, username: String, password: String) -> anyhow::Result<()> {
        RUNTIME.block_on(async move {
            self.client.login_username(&username, &password).send().await?;
            Ok(())
        })
    }

    /// Restores the client from a `RestoreToken`.
    pub fn restore_login(&self, restore_token: String) -> anyhow::Result<()> {
        let RestoreToken { session, homeurl: _, is_guest: _ } =
            serde_json::from_str(&restore_token)?;

        self.restore_session(session)
    }

    /// Restores the client from a `Session`.
    pub fn restore_session(&self, session: Session) -> anyhow::Result<()> {
        RUNTIME.block_on(async move {
            self.client.restore_login(session).await?;
            Ok(())
        })
    }

    pub fn set_delegate(&self, delegate: Option<Box<dyn ClientDelegate>>) {
        *self.delegate.write() = delegate;
    }

    /// The homeserver this client is configured to use.
    pub fn homeserver(&self) -> String {
        RUNTIME.block_on(async move { self.async_homeserver().await })
    }

    pub async fn async_homeserver(&self) -> String {
        self.client.homeserver().await.to_string()
    }

    /// The OIDC Provider that is trusted by the homeserver. `None` when
    /// not configured.
    pub async fn authentication_issuer(&self) -> Option<String> {
        self.client.authentication_issuer().await.map(|server| server.to_string())
    }

    /// Whether or not the client's homeserver supports the password login flow.
    pub async fn supports_password_login(&self) -> anyhow::Result<bool> {
        let login_types = self.client.get_login_types().await?;
        let supports_password = login_types
            .flows
            .iter()
            .any(|login_type| matches!(login_type, get_login_types::v3::LoginType::Password(_)));
        Ok(supports_password)
    }

    /// Gets information about the owner of a given access token.
    pub fn whoami(&self) -> anyhow::Result<whoami::v3::Response> {
        RUNTIME
            .block_on(async move { self.client.whoami().await.map_err(|e| anyhow!(e.to_string())) })
    }

    pub fn start_sync(&self, timeline_limit: Option<u16>) {
        let client = self.client.clone();
        let state = self.state.clone();
        let delegate = self.delegate.clone();
        let session_verification_controller = self.session_verification_controller.clone();
        RUNTIME.spawn(async move {
            let mut filter = FilterDefinition::default();
            let mut room_filter = RoomFilter::default();
            let mut event_filter = RoomEventFilter::default();
            let mut timeline_filter = RoomEventFilter::default();

            event_filter.lazy_load_options =
                LazyLoadOptions::Enabled { include_redundant_members: false };
            room_filter.state = event_filter;
            filter.room = room_filter;

            timeline_filter.limit = timeline_limit.map(|limit| limit.into());
            filter.room.timeline = timeline_filter;

            let filter_id = client.get_or_upload_filter("sync", filter).await.unwrap();

            let sync_settings = SyncSettings::new().filter(Filter::FilterId(&filter_id));

            client
                .sync_with_callback(sync_settings, |sync_response| async {
                    if !state.read().has_first_synced {
                        state.write().has_first_synced = true
                    }

                    if state.read().should_stop_syncing {
                        state.write().is_syncing = false;
                        return LoopCtrl::Break;
                    } else if !state.read().is_syncing {
                        state.write().is_syncing = true;
                    }

                    if let Some(delegate) = &*delegate.read() {
                        delegate.did_receive_sync_update()
                    }

                    if let Some(session_verification_controller) =
                        &*session_verification_controller.read().await
                    {
                        session_verification_controller
                            .process_to_device_messages(sync_response.to_device)
                            .await;
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

    pub fn get_session_verification_controller(
        &self,
    ) -> anyhow::Result<Arc<SessionVerificationController>> {
        RUNTIME.block_on(async move {
            if let Some(session_verification_controller) =
                &*self.session_verification_controller.read().await
            {
                return Ok(Arc::new(session_verification_controller.clone()));
            }

            let user_id = self.client.user_id().expect("Failed retrieving current user_id");
            let user_identity = self
                .client
                .encryption()
                .get_user_identity(user_id)
                .await?
                .expect("Failed retrieving user identity");

            let session_verification_controller = SessionVerificationController::new(user_identity);

            *self.session_verification_controller.write().await =
                Some(session_verification_controller.clone());

            Ok(Arc::new(session_verification_controller))
        })
    }
}

pub fn gen_transaction_id() -> String {
    TransactionId::new().to_string()
}
