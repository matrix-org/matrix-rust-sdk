use std::sync::{Arc, RwLock};

use anyhow::{anyhow, Context};
use matrix_sdk::{
    media::{MediaFormat, MediaRequest, MediaThumbnailSize},
    ruma::{
        api::client::{
            account::whoami, error::ErrorKind, media::get_content_thumbnail::v3::Method,
            session::get_login_types,
        },
        events::{room::MediaSource, AnyToDeviceEvent},
        serde::Raw,
        TransactionId, UInt,
    },
    Client as MatrixClient, Error, LoopCtrl,
};
use tokio::sync::broadcast::{self, error::RecvError};
use tracing::{debug, warn};

use super::{room::Room, session_verification::SessionVerificationController, RUNTIME};

impl std::ops::Deref for Client {
    type Target = MatrixClient;
    fn deref(&self) -> &MatrixClient {
        &self.client
    }
}

pub trait ClientDelegate: Sync + Send {
    fn did_receive_auth_error(&self, is_soft_logout: bool);
}

#[derive(Clone)]
pub struct Client {
    pub(crate) client: MatrixClient,
    delegate: Arc<RwLock<Option<Box<dyn ClientDelegate>>>>,
    session_verification_controller:
        Arc<matrix_sdk::locks::RwLock<Option<SessionVerificationController>>>,
    /// The sliding sync proxy that the client is configured to use by default.
    /// If this value is `Some`, it will be automatically added to the builder
    /// when calling `sliding_sync()`.
    pub(crate) sliding_sync_proxy: Arc<RwLock<Option<String>>>,
    pub(crate) sliding_sync_reset_broadcast_tx: broadcast::Sender<()>,
}

impl Client {
    pub fn new(client: MatrixClient) -> Self {
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
                    debug!("received to-device message, but verification controller isn't ready");
                }
            }
        });

        let (sliding_sync_reset_broadcast_tx, _) = broadcast::channel(1);

        let client = Client {
            client,
            delegate: Arc::new(RwLock::new(None)),
            session_verification_controller,
            sliding_sync_proxy: Arc::new(RwLock::new(None)),
            sliding_sync_reset_broadcast_tx,
        };

        let mut unknown_token_error_receiver = client.subscribe_to_unknown_token_errors();
        let client_clone = client.clone();
        RUNTIME.spawn(async move {
            loop {
                match unknown_token_error_receiver.recv().await {
                    Ok(unknown_token) => client_clone.process_unknown_token_error(unknown_token),
                    Err(receive_error) => {
                        if let RecvError::Closed = receive_error {
                            break;
                        }
                    }
                }
            }
        });

        client
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
            sliding_sync_proxy,
        } = session;

        *self.sliding_sync_proxy.write().unwrap() = sliding_sync_proxy;

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

    /// The sliding sync proxy that is trusted by the homeserver. `None` when
    /// not configured.
    pub fn discovered_sliding_sync_proxy(&self) -> Option<String> {
        RUNTIME.block_on(async move {
            self.client.sliding_sync_proxy().await.map(|server| server.to_string())
        })
    }

    pub(crate) fn set_sliding_sync_proxy(&self, sliding_sync_proxy: Option<String>) {
        *self.sliding_sync_proxy.write().unwrap() = sliding_sync_proxy;
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
            let sliding_sync_proxy = self.sliding_sync_proxy.read().unwrap().clone();

            Ok(Session {
                access_token,
                refresh_token,
                user_id: user_id.to_string(),
                device_id: device_id.to_string(),
                homeserver_url,
                sliding_sync_proxy,
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

    pub fn set_display_name(&self, name: String) -> anyhow::Result<()> {
        let client = self.client.clone();
        RUNTIME.block_on(async move {
            client
                .account()
                .set_display_name(Some(name.as_str()))
                .await
                .context("Unable to set display name")
        })
    }

    pub fn avatar_url(&self) -> anyhow::Result<Option<String>> {
        let l = self.client.clone();
        RUNTIME.block_on(async move {
            let avatar_url = l.account().get_avatar_url().await?;
            Ok(avatar_url.map(|u| u.to_string()))
        })
    }

    pub fn cached_avatar_url(&self) -> anyhow::Result<Option<String>> {
        let l = self.client.clone();
        RUNTIME.block_on(async move {
            let url = l.account().get_cached_avatar_url().await?;
            Ok(url)
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

    pub fn upload_media(&self, mime_type: String, data: Vec<u8>) -> anyhow::Result<String> {
        let l = self.client.clone();

        RUNTIME.block_on(async move {
            let mime_type: mime::Mime = mime_type.parse()?;
            let response = l.media().upload(&mime_type, data).await?;
            Ok(String::from(response.content_uri))
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

            let session_verification_controller =
                SessionVerificationController::new(self.client.encryption(), user_identity);

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
        let client_api_error_kind = sync_error.client_api_error_kind();
        match client_api_error_kind {
            Some(ErrorKind::UnknownPos) => {
                let _ = self.sliding_sync_reset_broadcast_tx.send(());
                LoopCtrl::Continue
            }
            _ => {
                warn!("Ignoring sync error: {sync_error:?}");
                LoopCtrl::Continue
            }
        }
    }

    fn process_unknown_token_error(&self, unknown_token: matrix_sdk::UnknownToken) {
        if let Some(delegate) = &*self.delegate.read().unwrap() {
            delegate.did_receive_auth_error(unknown_token.soft_logout);
        }
    }
}

#[uniffi::export]
impl Client {
    /// The homeserver this client is configured to use.
    pub fn homeserver(&self) -> String {
        #[allow(unknown_lints, clippy::redundant_async_block)] // false positive
        RUNTIME.block_on(self.async_homeserver())
    }

    pub fn rooms(&self) -> Vec<Arc<Room>> {
        self.client.rooms().into_iter().map(|room| Arc::new(Room::new(room))).collect()
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
    pub sliding_sync_proxy: Option<String>,
}

#[uniffi::export]
fn gen_transaction_id() -> String {
    TransactionId::new().to_string()
}
