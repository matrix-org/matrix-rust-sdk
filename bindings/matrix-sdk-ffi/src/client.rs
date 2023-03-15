use std::sync::{Arc, RwLock};

use anyhow::{anyhow, Context};
use matrix_sdk::{
    media::{MediaFileHandle as SdkMediaFileHandle, MediaFormat, MediaRequest, MediaThumbnailSize},
    ruma::{
        api::client::{
            account::whoami,
            error::ErrorKind,
            media::get_content_thumbnail::v3::Method,
            push::{EmailPusherData, PusherIds, PusherInit, PusherKind as RumaPusherKind},
            room::{create_room, Visibility},
            session::get_login_types,
        },
        events::{
            room::{
                avatar::RoomAvatarEventContent, encryption::RoomEncryptionEventContent, MediaSource,
            },
            AnyInitialStateEvent, AnyToDeviceEvent, InitialStateEvent,
        },
        serde::Raw,
        EventEncryptionAlgorithm, TransactionId, UInt, UserId,
    },
    Client as MatrixClient, Error, LoopCtrl,
};
use ruma::push::{HttpPusherData as RumaHttpPusherData, PushFormat as RumaPushFormat};
use serde_json::Value;
use tokio::sync::broadcast::{self, error::RecvError};
use tracing::{debug, error, warn};

use super::{room::Room, session_verification::SessionVerificationController, RUNTIME};
use crate::{client, ClientError};

#[derive(Clone, uniffi::Record)]
pub struct PusherIdentifiers {
    pub pushkey: String,
    pub app_id: String,
}

impl From<PusherIdentifiers> for PusherIds {
    fn from(value: PusherIdentifiers) -> Self {
        Self::new(value.pushkey, value.app_id)
    }
}

#[derive(Clone, uniffi::Record)]
pub struct HttpPusherData {
    pub url: String,
    pub format: Option<PushFormat>,
    pub default_payload: Option<String>,
}

#[derive(Clone, uniffi::Enum)]
pub enum PusherKind {
    Http { data: HttpPusherData },
    Email,
}

impl TryFrom<PusherKind> for RumaPusherKind {
    type Error = anyhow::Error;

    fn try_from(value: PusherKind) -> anyhow::Result<Self> {
        match value {
            PusherKind::Http { data } => {
                let mut ruma_data = RumaHttpPusherData::new(data.url);
                if let Some(payload) = data.default_payload {
                    let json: Value = serde_json::from_str(&payload)?;
                    ruma_data.default_payload = json;
                }
                ruma_data.format = data.format.map(Into::into);
                Ok(Self::Http(ruma_data))
            }
            PusherKind::Email => {
                let ruma_data = EmailPusherData::new();
                Ok(Self::Email(ruma_data))
            }
        }
    }
}

#[derive(Clone, uniffi::Enum)]
pub enum PushFormat {
    EventIdOnly,
}

impl From<PushFormat> for RumaPushFormat {
    fn from(value: PushFormat) -> Self {
        match value {
            client::PushFormat::EventIdOnly => Self::EventIdOnly,
        }
    }
}

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

    pub fn get_media_file(
        &self,
        media_source: Arc<MediaSource>,
        mime_type: String,
    ) -> anyhow::Result<Arc<MediaFileHandle>> {
        let client = self.client.clone();
        let source = (*media_source).clone();
        let mime_type: mime::Mime = mime_type.parse()?;

        RUNTIME.block_on(async move {
            let handle = client
                .media()
                .get_media_file(
                    &MediaRequest { source, format: MediaFormat::File },
                    &mime_type,
                    true,
                )
                .await?;

            Ok(Arc::new(MediaFileHandle { inner: handle }))
        })
    }
}

#[uniffi::export]
impl Client {
    /// Restores the client from a `Session`.
    pub fn restore_session(&self, session: Session) -> Result<(), ClientError> {
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
        Ok(self.restore_session_inner(session)?)
    }
}

impl Client {
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
}

#[uniffi::export]
impl Client {
    pub fn session(&self) -> Result<Session, ClientError> {
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

    pub fn user_id(&self) -> Result<String, ClientError> {
        let user_id = self.client.user_id().context("No User ID found")?;
        Ok(user_id.to_string())
    }

    pub fn display_name(&self) -> Result<String, ClientError> {
        let l = self.client.clone();
        RUNTIME.block_on(async move {
            let display_name = l.account().get_display_name().await?.context("No User ID found")?;
            Ok(display_name)
        })
    }

    pub fn set_display_name(&self, name: String) -> Result<(), ClientError> {
        let client = self.client.clone();
        RUNTIME.block_on(async move {
            client
                .account()
                .set_display_name(Some(name.as_str()))
                .await
                .context("Unable to set display name")?;
            Ok(())
        })
    }

    pub fn avatar_url(&self) -> Result<Option<String>, ClientError> {
        let l = self.client.clone();
        RUNTIME.block_on(async move {
            let avatar_url = l.account().get_avatar_url().await?;
            Ok(avatar_url.map(|u| u.to_string()))
        })
    }

    pub fn cached_avatar_url(&self) -> Result<Option<String>, ClientError> {
        let l = self.client.clone();
        RUNTIME.block_on(async move {
            let url = l.account().get_cached_avatar_url().await?;
            Ok(url)
        })
    }

    pub fn device_id(&self) -> Result<String, ClientError> {
        let device_id = self.client.device_id().context("No Device ID found")?;
        Ok(device_id.to_string())
    }

    pub fn create_room(&self, request: CreateRoomParameters) -> Result<String, ClientError> {
        let client = self.client.clone();

        RUNTIME.block_on(async move {
            let response = client.create_room(request.into()).await?;
            Ok(String::from(response.room_id()))
        })
    }

    /// Get the content of the event of the given type out of the account data
    /// store.
    ///
    /// It will be returned as a JSON string.
    pub fn account_data(&self, event_type: String) -> Result<Option<String>, ClientError> {
        RUNTIME.block_on(async move {
            let event = self.client.account().account_data_raw(event_type.into()).await?;
            Ok(event.map(|e| e.json().get().to_owned()))
        })
    }

    /// Set the given account data content for the given event type.
    ///
    /// It should be supplied as a JSON string.
    pub fn set_account_data(&self, event_type: String, content: String) -> Result<(), ClientError> {
        RUNTIME.block_on(async move {
            let raw_content = Raw::from_json_string(content)?;
            self.client.account().set_account_data_raw(event_type.into(), raw_content).await?;
            Ok(())
        })
    }

    pub fn upload_media(&self, mime_type: String, data: Vec<u8>) -> Result<String, ClientError> {
        let l = self.client.clone();

        RUNTIME.block_on(async move {
            let mime_type: mime::Mime = mime_type.parse().context("Parsing mime type")?;
            let response = l.media().upload(&mime_type, data).await?;
            Ok(String::from(response.content_uri))
        })
    }

    pub fn get_media_content(
        &self,
        media_source: Arc<MediaSource>,
    ) -> Result<Vec<u8>, ClientError> {
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
    ) -> Result<Vec<u8>, ClientError> {
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
    ) -> Result<Arc<SessionVerificationController>, ClientError> {
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
    pub fn logout(&self) -> Result<(), ClientError> {
        RUNTIME.block_on(self.client.logout())?;
        Ok(())
    }

    /// Registers a pusher with given parameters
    pub fn set_pusher(
        &self,
        identifiers: PusherIdentifiers,
        kind: PusherKind,
        app_display_name: String,
        device_display_name: String,
        profile_tag: Option<String>,
        lang: String,
    ) -> Result<(), ClientError> {
        RUNTIME.block_on(async move {
            let ids = identifiers.into();

            let pusher_init = PusherInit {
                ids,
                kind: kind.try_into()?,
                app_display_name,
                device_display_name,
                profile_tag,
                lang,
            };
            self.client.set_pusher(pusher_init.into()).await?;
            Ok(())
        })
    }

    /// The homeserver this client is configured to use.
    pub fn homeserver(&self) -> String {
        RUNTIME.block_on(self.async_homeserver())
    }

    pub fn rooms(&self) -> Vec<Arc<Room>> {
        self.client.rooms().into_iter().map(|room| Arc::new(Room::new(room))).collect()
    }
}

impl Client {
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

pub struct CreateRoomParameters {
    pub name: String,
    pub topic: Option<String>,
    pub is_encrypted: bool,
    pub is_direct: bool,
    pub visibility: RoomVisibility,
    pub preset: RoomPreset,
    pub invite: Option<Vec<String>>,
    pub avatar: Option<String>,
}

impl From<CreateRoomParameters> for create_room::v3::Request {
    fn from(value: CreateRoomParameters) -> create_room::v3::Request {
        let mut request = create_room::v3::Request::new();
        request.name = Some(value.name);
        request.topic = value.topic;
        request.is_direct = value.is_direct;
        request.visibility = value.visibility.into();
        request.preset = Some(value.preset.into());
        request.invite = match value.invite {
            Some(invite) => invite
                .iter()
                .filter_map(|user_id| match UserId::parse(user_id) {
                    Ok(id) => Some(id),
                    Err(e) => {
                        error!(user_id, "Skipping invalid user ID, error: {e}");
                        None
                    }
                })
                .collect(),
            None => vec![],
        };

        let mut initial_state: Vec<Raw<AnyInitialStateEvent>> = vec![];

        if value.is_encrypted {
            let content =
                RoomEncryptionEventContent::new(EventEncryptionAlgorithm::MegolmV1AesSha2);
            initial_state.push(InitialStateEvent::new(content).to_raw_any());
        }

        if let Some(url) = value.avatar {
            let mut content = RoomAvatarEventContent::new();
            content.url = Some(url.into());
            initial_state.push(InitialStateEvent::new(content).to_raw_any());
        }

        request.initial_state = initial_state;

        request
    }
}

pub enum RoomVisibility {
    /// Indicates that the room will be shown in the published room list.
    Public,

    /// Indicates that the room will not be shown in the published room list.
    Private,
}

impl From<RoomVisibility> for Visibility {
    fn from(value: RoomVisibility) -> Self {
        match value {
            RoomVisibility::Public => Self::Public,
            RoomVisibility::Private => Self::Private,
        }
    }
}

pub enum RoomPreset {
    /// `join_rules` is set to `invite` and `history_visibility` is set to
    /// `shared`.
    PrivateChat,

    /// `join_rules` is set to `public` and `history_visibility` is set to
    /// `shared`.
    PublicChat,

    /// Same as `PrivateChat`, but all initial invitees get the same power level
    /// as the creator.
    TrustedPrivateChat,
}

impl From<RoomPreset> for create_room::v3::RoomPreset {
    fn from(value: RoomPreset) -> Self {
        match value {
            RoomPreset::PrivateChat => Self::PrivateChat,
            RoomPreset::PublicChat => Self::PublicChat,
            RoomPreset::TrustedPrivateChat => Self::TrustedPrivateChat,
        }
    }
}

#[derive(uniffi::Record)]
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

/// A file handle that takes ownership of a media file on disk. When the handle
/// is dropped, the file will be removed from the disk.
pub struct MediaFileHandle {
    inner: SdkMediaFileHandle,
}

impl MediaFileHandle {
    /// Get the media file's path.
    pub fn path(&self) -> String {
        self.inner.path().to_str().unwrap().to_owned()
    }
}
