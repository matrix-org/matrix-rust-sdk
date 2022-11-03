use std::sync::{Arc, RwLock};

use anyhow::{anyhow, Context};
use matrix_sdk::{
    config::SyncSettings,
    media::{MediaFormat, MediaRequest, MediaThumbnailSize},
    ruma::{
        api::client::{
            account::whoami,
            error::ErrorKind,
            filter::{FilterDefinition, LazyLoadOptions, RoomEventFilter, RoomFilter},
            media::get_content_thumbnail::v3::Method,
            session::get_login_types,
            sync::sync_events::v3::Filter,
        },
        events::{room::MediaSource, AnyToDeviceEvent},
        serde::Raw,
        TransactionId, UInt,
    },
    Client as MatrixClient, Error, LoopCtrl, RumaApiError,
};

use super::{
    room::Room, session_verification::SessionVerificationController, ClientState, RUNTIME,
};

impl std::ops::Deref for Client {
    type Target = MatrixClient;
    fn deref(&self) -> &MatrixClient {
        &self.client
    }
}

pub trait ClientDelegate: Sync + Send {
    fn did_receive_sync_update(&self);
    fn did_receive_auth_error(&self, is_soft_logout: bool);
    fn did_update_restore_token(&self);
}

#[derive(Clone)]
pub struct Client {
    pub(crate) client: MatrixClient,
    state: Arc<RwLock<ClientState>>,
    delegate: Arc<RwLock<Option<Box<dyn ClientDelegate>>>>,
    session_verification_controller:
        Arc<matrix_sdk::locks::RwLock<Option<SessionVerificationController>>>,
}

impl Client {
    pub fn new(client: MatrixClient, state: ClientState) -> Self {
        let session_verification_controller: Arc<
            matrix_sdk::locks::RwLock<Option<SessionVerificationController>>,
        > = Default::default();
        let ctrl = session_verification_controller.clone();

        client.add_event_handler(move |ev: AnyToDeviceEvent| {
            let ctrl = ctrl.clone();
            async move {
                if let Some(session_verification_controller) = &*ctrl.clone().read().await {
                    session_verification_controller.process_to_device_message(ev).await;
                } else {
                    tracing::debug!(
                        "received to-device message, but verification controller isn't ready"
                    );
                }
            }
        });

        Client {
            client,
            state: Arc::new(RwLock::new(state)),
            delegate: Arc::new(RwLock::new(None)),
            session_verification_controller,
        }
    }

    /// Login using a username and password.
    pub fn login(
        &self,
        username: String,
        password: String,
        initial_device_name: Option<String>,
        device_id: Option<String>,
    ) -> anyhow::Result<()> {
        RUNTIME.block_on(async move {
            let mut builder = self.client.login_username(&username, &password);
            if let Some(initial_device_name) = initial_device_name.as_ref() {
                builder = builder.initial_device_display_name(initial_device_name);
            }
            if let Some(device_id) = device_id.as_ref() {
                builder = builder.device_id(device_id);
            }
            builder.send().await?;
            Ok(())
        })
    }

    /// Restores the client from a `Session`.
    pub fn restore_session(&self, session: Session) -> anyhow::Result<()> {
        let Session {
            access_token,
            refresh_token,
            user_id,
            device_id,
            homeserver_url: _,
            is_soft_logout,
        } = session;

        // update soft logout state
        self.state.write().unwrap().is_soft_logout = is_soft_logout;

        let session = matrix_sdk::Session {
            access_token,
            refresh_token,
            user_id: user_id.try_into()?,
            device_id: device_id.into(),
        };
        self.restore_session_inner(session)
    }

    /// Restores the client from a `matrix_sdk::Session`.
    pub(crate) fn restore_session_inner(&self, session: matrix_sdk::Session) -> anyhow::Result<()> {
        RUNTIME.block_on(async move {
            self.client.restore_session(session).await?;
            Ok(())
        })
    }

    pub fn set_delegate(&self, delegate: Option<Box<dyn ClientDelegate>>) {
        *self.delegate.write().unwrap() = delegate;
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

    pub fn session(&self) -> anyhow::Result<Session> {
        RUNTIME.block_on(async move {
            let matrix_sdk::Session { access_token, refresh_token, user_id, device_id } =
                self.client.session().context("Missing session")?;
            let homeserver_url = self.client.homeserver().await.into();
            let is_soft_logout = self.state.read().unwrap().is_soft_logout;

            Ok(Session {
                access_token,
                refresh_token,
                user_id: user_id.to_string(),
                device_id: device_id.to_string(),
                homeserver_url,
                is_soft_logout,
            })
        })
    }

    pub fn user_id(&self) -> anyhow::Result<String> {
        let user_id = self.client.user_id().context("No User ID found")?;
        Ok(user_id.to_string())
    }

    pub fn display_name(&self) -> anyhow::Result<String> {
        let l = self.client.clone();
        RUNTIME.block_on(async move {
            let display_name = l.account().get_display_name().await?.context("No User ID found")?;
            Ok(display_name)
        })
    }

    pub fn avatar_url(&self) -> anyhow::Result<String> {
        let l = self.client.clone();
        RUNTIME.block_on(async move {
            let avatar_url = l.account().get_avatar_url().await?.context("No User ID found")?;
            Ok(avatar_url.to_string())
        })
    }

    pub fn device_id(&self) -> anyhow::Result<String> {
        let device_id = self.client.device_id().context("No Device ID found")?;
        Ok(device_id.to_string())
    }

    /// Get the content of the event of the given type out of the account data
    /// store.
    ///
    /// It will be returned as a JSON string.
    pub fn account_data(&self, event_type: String) -> anyhow::Result<Option<String>> {
        RUNTIME.block_on(async move {
            let event = self.client.account().account_data_raw(event_type.into()).await?;
            Ok(event.map(|e| e.json().get().to_owned()))
        })
    }

    /// Set the given account data content for the given event type.
    ///
    /// It should be supplied as a JSON string.
    pub fn set_account_data(&self, event_type: String, content: String) -> anyhow::Result<()> {
        RUNTIME.block_on(async move {
            let raw_content = Raw::from_json_string(content)?;
            self.client.account().set_account_data_raw(event_type.into(), raw_content).await?;
            Ok(())
        })
    }

    pub fn get_media_content(&self, media_source: Arc<MediaSource>) -> anyhow::Result<Vec<u8>> {
        let l = self.client.clone();
        let source = (*media_source).clone();

        RUNTIME.block_on(async move {
            Ok(l.media()
                .get_media_content(&MediaRequest { source, format: MediaFormat::File }, true)
                .await?)
        })
    }

    pub fn get_media_thumbnail(
        &self,
        media_source: Arc<MediaSource>,
        width: u64,
        height: u64,
    ) -> anyhow::Result<Vec<u8>> {
        let l = self.client.clone();
        let source = (*media_source).clone();

        RUNTIME.block_on(async move {
            Ok(l.media()
                .get_media_content(
                    &MediaRequest {
                        source,
                        format: MediaFormat::Thumbnail(MediaThumbnailSize {
                            method: Method::Scale,
                            width: UInt::new(width).unwrap(),
                            height: UInt::new(height).unwrap(),
                        }),
                    },
                    true,
                )
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
            let user_id = self.client.user_id().context("Failed retrieving current user_id")?;
            let user_identity = self
                .client
                .encryption()
                .get_user_identity(user_id)
                .await?
                .context("Failed retrieving user identity")?;

            let session_verification_controller = SessionVerificationController::new(user_identity);

            *self.session_verification_controller.write().await =
                Some(session_verification_controller.clone());

            Ok(Arc::new(session_verification_controller))
        })
    }

    /// Log out the current user
    pub fn logout(&self) -> anyhow::Result<()> {
        RUNTIME.block_on(async move {
            match self.client.logout().await {
                Ok(_) => Ok(()),
                Err(error) => Err(anyhow!(error.to_string())),
            }
        })
    }

    /// Process a sync error and return loop control accordingly
    pub(crate) fn process_sync_error(&self, sync_error: Error) -> LoopCtrl {
        if let Some(RumaApiError::ClientApi(error)) = sync_error.as_ruma_api_error() {
            if let ErrorKind::UnknownToken { soft_logout } = error.kind {
                self.state.write().unwrap().is_soft_logout = soft_logout;
                if let Some(delegate) = &*self.delegate.read().unwrap() {
                    delegate.did_update_restore_token();
                    delegate.did_receive_auth_error(soft_logout);
                }
                return LoopCtrl::Break;
            }
        }

        tracing::warn!("Ignoring sync error: {:?}", sync_error);
        LoopCtrl::Continue
    }
}

#[uniffi::export]
impl Client {
    /// The homeserver this client is configured to use.
    pub fn homeserver(&self) -> String {
        RUNTIME.block_on(async move { self.async_homeserver().await })
    }

    /// Indication whether we've received a first sync response since
    /// establishing the client (in memory)
    pub fn has_first_synced(&self) -> bool {
        self.state.read().unwrap().has_first_synced
    }

    /// Indication whether we are currently syncing
    pub fn is_syncing(&self) -> bool {
        self.state.read().unwrap().has_first_synced
    }

    /// Flag indicating whether the session is in soft logout mode
    pub fn is_soft_logout(&self) -> bool {
        self.state.read().unwrap().is_soft_logout
    }

    pub fn rooms(&self) -> Vec<Arc<Room>> {
        self.client.rooms().into_iter().map(|room| Arc::new(Room::new(room))).collect()
    }

    pub fn start_sync(&self, timeline_limit: Option<u16>) {
        let client = self.client.clone();
        let state = self.state.clone();
        let delegate = self.delegate.clone();
        let local_self = self.clone();
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
                .sync_with_result_callback(sync_settings, |result| async {
                    Ok(if result.is_ok() {
                        if !state.read().unwrap().has_first_synced {
                            state.write().unwrap().has_first_synced = true;
                        }

                        if state.read().unwrap().should_stop_syncing {
                            state.write().unwrap().is_syncing = false;
                            return Ok(LoopCtrl::Break);
                        } else if !state.read().unwrap().is_syncing {
                            state.write().unwrap().is_syncing = true;
                        }

                        if let Some(delegate) = &*delegate.read().unwrap() {
                            delegate.did_receive_sync_update()
                        }

                        LoopCtrl::Continue
                    } else {
                        local_self.process_sync_error(result.err().unwrap())
                    })
                })
                .await
                .unwrap();
        });
    }
}

pub struct Session {
    // Same fields as the Session type in matrix-sdk, just simpler types
    /// The access token used for this session.
    pub access_token: String,
    /// The token used for [refreshing the access token], if any.
    ///
    /// [refreshing the access token]: https://spec.matrix.org/v1.3/client-server-api/#refreshing-access-tokens
    pub refresh_token: Option<String>,
    /// The user the access token was issued for.
    pub user_id: String,
    /// The ID of the client device.
    pub device_id: String,

    // FFI-only fields (for now)
    pub homeserver_url: String,
    pub is_soft_logout: bool,
}

#[uniffi::export]
fn gen_transaction_id() -> String {
    TransactionId::new().to_string()
}
