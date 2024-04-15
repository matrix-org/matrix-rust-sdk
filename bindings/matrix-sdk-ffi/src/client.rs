use std::{
    collections::HashMap,
    mem::ManuallyDrop,
    sync::{Arc, RwLock},
};

use anyhow::{anyhow, Context as _};
use matrix_sdk::{
    media::{MediaFileHandle as SdkMediaFileHandle, MediaFormat, MediaRequest, MediaThumbnailSize},
    oidc::{
        types::{
            client_credentials::ClientCredentials,
            registration::{
                ClientMetadata, ClientMetadataVerificationError, VerifiedClientMetadata,
            },
        },
        OidcAccountManagementAction, OidcSession,
    },
    ruma::{
        api::client::{
            account::whoami,
            media::get_content_thumbnail::v3::Method,
            push::{EmailPusherData, PusherIds, PusherInit, PusherKind as RumaPusherKind},
            room::{create_room, Visibility},
            session::get_login_types,
            user_directory::search_users,
        },
        events::{
            room::{
                avatar::RoomAvatarEventContent, encryption::RoomEncryptionEventContent, MediaSource,
            },
            AnyInitialStateEvent, AnyToDeviceEvent, InitialStateEvent,
        },
        serde::Raw,
        EventEncryptionAlgorithm, RoomId, TransactionId, UInt, UserId,
    },
    AuthApi, AuthSession, Client as MatrixClient, SessionChange, SessionTokens,
};
use matrix_sdk_ui::notification_client::NotificationProcessSetup as MatrixNotificationProcessSetup;
use mime::Mime;
use ruma::{
    api::client::discovery::discover_homeserver::AuthenticationServerInfo,
    events::{
        ignored_user_list::IgnoredUserListEventContent,
        room::power_levels::RoomPowerLevelsEventContent, GlobalAccountDataEventType,
    },
    push::{HttpPusherData as RumaHttpPusherData, PushFormat as RumaPushFormat},
    RoomAliasId,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, error};
use url::Url;

use super::{room::Room, session_verification::SessionVerificationController, RUNTIME};
use crate::{
    client,
    encryption::Encryption,
    notification::NotificationClientBuilder,
    notification_settings::NotificationSettings,
    room_directory_search::RoomDirectorySearch,
    room_preview::RoomPreview,
    sync_service::{SyncService, SyncServiceBuilder},
    task_handle::TaskHandle,
    ClientError,
};

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

#[uniffi::export(callback_interface)]
pub trait ClientDelegate: Sync + Send {
    fn did_receive_auth_error(&self, is_soft_logout: bool);
    fn did_refresh_tokens(&self);
}

#[uniffi::export(callback_interface)]
pub trait ClientSessionDelegate: Sync + Send {
    fn retrieve_session_from_keychain(&self, user_id: String) -> Result<Session, ClientError>;
    fn save_session_in_keychain(&self, session: Session);
}

#[uniffi::export(callback_interface)]
pub trait ProgressWatcher: Send + Sync {
    fn transmission_progress(&self, progress: TransmissionProgress);
}

#[derive(Clone, Copy, uniffi::Record)]
pub struct TransmissionProgress {
    pub current: u64,
    pub total: u64,
}

impl From<matrix_sdk::TransmissionProgress> for TransmissionProgress {
    fn from(value: matrix_sdk::TransmissionProgress) -> Self {
        Self {
            current: value.current.try_into().unwrap_or(u64::MAX),
            total: value.total.try_into().unwrap_or(u64::MAX),
        }
    }
}

#[derive(uniffi::Object)]
pub struct Client {
    pub(crate) inner: ManuallyDrop<MatrixClient>,
    delegate: RwLock<Option<Arc<dyn ClientDelegate>>>,
    session_verification_controller:
        Arc<tokio::sync::RwLock<Option<SessionVerificationController>>>,
}

impl Drop for Client {
    fn drop(&mut self) {
        // Dropping the inner OlmMachine must happen within a tokio context
        // because deadpool drops sqlite connections in the DB pool on tokio's
        // blocking threadpool to avoid blocking async worker threads.
        let _guard = RUNTIME.enter();
        // SAFETY: self.inner is never used again, which is the only requirement
        //         for ManuallyDrop::drop to be used safely.
        unsafe {
            ManuallyDrop::drop(&mut self.inner);
        }
    }
}

impl Client {
    pub fn new(
        sdk_client: MatrixClient,
        cross_process_refresh_lock_id: Option<String>,
        session_delegate: Option<Arc<dyn ClientSessionDelegate>>,
    ) -> Result<Self, ClientError> {
        let session_verification_controller: Arc<
            tokio::sync::RwLock<Option<SessionVerificationController>>,
        > = Default::default();
        let ctrl = session_verification_controller.clone();

        sdk_client.add_event_handler(move |ev: AnyToDeviceEvent| async move {
            if let Some(session_verification_controller) = &*ctrl.clone().read().await {
                session_verification_controller.process_to_device_message(ev).await;
            } else {
                debug!("received to-device message, but verification controller isn't ready");
            }
        });

        let client = Client {
            inner: ManuallyDrop::new(sdk_client),
            delegate: RwLock::new(None),
            session_verification_controller,
        };

        if let Some(process_id) = cross_process_refresh_lock_id {
            if session_delegate.is_none() {
                return Err(anyhow::anyhow!(
                    "missing session delegates when enabling the cross-process lock"
                ))?;
            }
            RUNTIME.block_on(async {
                client.inner.oidc().enable_cross_process_refresh_lock(process_id.clone()).await
            })?;
        }

        if let Some(session_delegate) = session_delegate {
            client.inner.set_session_callbacks(
                {
                    let session_delegate = session_delegate.clone();
                    Box::new(move |client| {
                        let session_delegate = session_delegate.clone();
                        let user_id = client.user_id().context("user isn't logged in")?;
                        Ok(Self::retrieve_session(session_delegate, user_id)?)
                    })
                },
                {
                    let session_delegate = session_delegate.clone();
                    Box::new(move |client| {
                        let session_delegate = session_delegate.clone();
                        Box::pin(
                            async move { Ok(Self::save_session(session_delegate, client).await?) },
                        )
                    })
                },
            )?;
        }

        Ok(client)
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl Client {
    /// Login using a username and password.
    pub async fn login(
        &self,
        username: String,
        password: String,
        initial_device_name: Option<String>,
        device_id: Option<String>,
    ) -> Result<(), ClientError> {
        let mut builder = self.inner.matrix_auth().login_username(&username, &password);
        if let Some(initial_device_name) = initial_device_name.as_ref() {
            builder = builder.initial_device_display_name(initial_device_name);
        }
        if let Some(device_id) = device_id.as_ref() {
            builder = builder.device_id(device_id);
        }
        builder.send().await?;
        Ok(())
    }

    pub async fn get_media_file(
        &self,
        media_source: Arc<MediaSource>,
        body: Option<String>,
        mime_type: String,
        use_cache: bool,
        temp_dir: Option<String>,
    ) -> Result<Arc<MediaFileHandle>, ClientError> {
        let source = (*media_source).clone();
        let mime_type: mime::Mime = mime_type.parse()?;

        let handle = self
            .inner
            .media()
            .get_media_file(
                &MediaRequest { source, format: MediaFormat::File },
                body,
                &mime_type,
                use_cache,
                temp_dir,
            )
            .await?;

        Ok(Arc::new(MediaFileHandle::new(handle)))
    }

    /// Restores the client from a `Session`.
    pub fn restore_session(&self, session: Session) -> Result<(), ClientError> {
        let sliding_sync_proxy = session.sliding_sync_proxy.clone();
        let auth_session: AuthSession = session.try_into()?;

        self.restore_session_inner(auth_session)?;

        if let Some(sliding_sync_proxy) = sliding_sync_proxy {
            let sliding_sync_proxy = Url::parse(&sliding_sync_proxy)
                .map_err(|error| ClientError::Generic { msg: error.to_string() })?;

            self.inner.set_sliding_sync_proxy(Some(sliding_sync_proxy));
        }

        Ok(())
    }
}

impl Client {
    /// Restores the client from an `AuthSession`.
    pub(crate) fn restore_session_inner(
        &self,
        session: impl Into<AuthSession>,
    ) -> anyhow::Result<()> {
        RUNTIME.block_on(async move {
            self.inner.restore_session(session).await?;
            Ok(())
        })
    }

    /// The homeserver's trusted OIDC Provider that was discovered in the
    /// well-known.
    ///
    /// This will only be set if the homeserver supports authenticating via
    /// OpenID Connect and this `Client` was constructed using auto-discovery by
    /// setting the homeserver with [`ClientBuilder::server_name()`].
    pub(crate) fn discovered_authentication_server(&self) -> Option<AuthenticationServerInfo> {
        self.inner.oidc().authentication_server_info().cloned()
    }

    /// The sliding sync proxy of the homeserver. It is either set automatically
    /// during discovery or manually via `set_sliding_sync_proxy` or `None`
    /// when not configured.
    pub fn sliding_sync_proxy(&self) -> Option<Url> {
        self.inner.sliding_sync_proxy()
    }

    /// Whether or not the client's homeserver supports the password login flow.
    pub(crate) async fn supports_password_login(&self) -> anyhow::Result<bool> {
        let login_types = self.inner.matrix_auth().get_login_types().await?;
        let supports_password = login_types
            .flows
            .iter()
            .any(|login_type| matches!(login_type, get_login_types::v3::LoginType::Password(_)));
        Ok(supports_password)
    }

    /// Gets information about the owner of a given access token.
    pub(crate) async fn whoami(&self) -> anyhow::Result<whoami::v3::Response> {
        Ok(self.inner.whoami().await?)
    }
}

#[uniffi::export(async_runtime = "tokio")]
impl Client {
    pub fn set_delegate(
        self: Arc<Self>,
        delegate: Option<Box<dyn ClientDelegate>>,
    ) -> Option<Arc<TaskHandle>> {
        delegate.map(|delegate| {
            let mut session_change_receiver = self.inner.subscribe_to_session_changes();
            let client_clone = self.clone();
            let session_change_task = RUNTIME.spawn(async move {
                loop {
                    match session_change_receiver.recv().await {
                        Ok(session_change) => client_clone.process_session_change(session_change),
                        Err(receive_error) => {
                            if let RecvError::Closed = receive_error {
                                break;
                            }
                        }
                    }
                }
            });

            *self.delegate.write().unwrap() = Some(Arc::from(delegate));
            Arc::new(TaskHandle::new(session_change_task))
        })
    }

    pub fn session(&self) -> Result<Session, ClientError> {
        RUNTIME.block_on(async move { Self::session_inner((*self.inner).clone()).await })
    }

    pub fn account_url(
        &self,
        action: Option<AccountManagementAction>,
    ) -> Result<Option<String>, ClientError> {
        match self.inner.oidc().account_management_url(action.map(Into::into)) {
            Ok(url) => Ok(url.map(|u| u.to_string())),
            Err(e) => {
                tracing::error!("Failed retrieving account management URL: {e}");
                Err(e.into())
            }
        }
    }

    pub fn user_id(&self) -> Result<String, ClientError> {
        let user_id = self.inner.user_id().context("No User ID found")?;
        Ok(user_id.to_string())
    }

    pub fn display_name(&self) -> Result<String, ClientError> {
        let l = self.inner.clone();
        RUNTIME.block_on(async move {
            let display_name = l.account().get_display_name().await?.context("No User ID found")?;
            Ok(display_name)
        })
    }

    pub fn set_display_name(&self, name: String) -> Result<(), ClientError> {
        let client = self.inner.clone();
        RUNTIME.block_on(async move {
            client
                .account()
                .set_display_name(Some(name.as_str()))
                .await
                .context("Unable to set display name")?;
            Ok(())
        })
    }

    pub fn upload_avatar(&self, mime_type: String, data: Vec<u8>) -> Result<(), ClientError> {
        let client = self.inner.clone();
        RUNTIME.block_on(async move {
            let mime: Mime = mime_type.parse()?;
            client.account().upload_avatar(&mime, data).await?;
            Ok(())
        })
    }

    pub fn remove_avatar(&self) -> Result<(), ClientError> {
        let client = self.inner.clone();
        RUNTIME.block_on(async move {
            client.account().set_avatar_url(None).await?;
            Ok(())
        })
    }

    pub fn avatar_url(&self) -> Result<Option<String>, ClientError> {
        let l = self.inner.clone();
        RUNTIME.block_on(async move {
            let avatar_url = l.account().get_avatar_url().await?;
            Ok(avatar_url.map(|u| u.to_string()))
        })
    }

    pub fn cached_avatar_url(&self) -> Result<Option<String>, ClientError> {
        let l = self.inner.clone();
        RUNTIME.block_on(async move {
            let url = l.account().get_cached_avatar_url().await?;
            Ok(url)
        })
    }

    pub fn device_id(&self) -> Result<String, ClientError> {
        let device_id = self.inner.device_id().context("No Device ID found")?;
        Ok(device_id.to_string())
    }

    pub fn create_room(&self, request: CreateRoomParameters) -> Result<String, ClientError> {
        let client = self.inner.clone();

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
            let event = self.inner.account().account_data_raw(event_type.into()).await?;
            Ok(event.map(|e| e.json().get().to_owned()))
        })
    }

    /// Set the given account data content for the given event type.
    ///
    /// It should be supplied as a JSON string.
    pub fn set_account_data(&self, event_type: String, content: String) -> Result<(), ClientError> {
        RUNTIME.block_on(async move {
            let raw_content = Raw::from_json_string(content)?;
            self.inner.account().set_account_data_raw(event_type.into(), raw_content).await?;
            Ok(())
        })
    }

    pub async fn upload_media(
        &self,
        mime_type: String,
        data: Vec<u8>,
        progress_watcher: Option<Box<dyn ProgressWatcher>>,
    ) -> Result<String, ClientError> {
        let mime_type: mime::Mime = mime_type.parse().context("Parsing mime type")?;
        let request = self.inner.media().upload(&mime_type, data);

        if let Some(progress_watcher) = progress_watcher {
            let mut subscriber = request.subscribe_to_send_progress();
            RUNTIME.spawn(async move {
                while let Some(progress) = subscriber.next().await {
                    progress_watcher.transmission_progress(progress.into());
                }
            });
        }

        let response = request.await?;

        Ok(String::from(response.content_uri))
    }

    pub async fn get_media_content(
        &self,
        media_source: Arc<MediaSource>,
    ) -> Result<Vec<u8>, ClientError> {
        let source = (*media_source).clone();

        Ok(self
            .inner
            .media()
            .get_media_content(&MediaRequest { source, format: MediaFormat::File }, true)
            .await?)
    }

    pub async fn get_media_thumbnail(
        &self,
        media_source: Arc<MediaSource>,
        width: u64,
        height: u64,
    ) -> Result<Vec<u8>, ClientError> {
        let source = (*media_source).clone();

        Ok(self
            .inner
            .media()
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
            let user_id = self.inner.user_id().context("Failed retrieving current user_id")?;
            let user_identity = self
                .inner
                .encryption()
                .get_user_identity(user_id)
                .await?
                .context("Failed retrieving user identity")?;

            let session_verification_controller =
                SessionVerificationController::new(self.inner.encryption(), user_identity);

            *self.session_verification_controller.write().await =
                Some(session_verification_controller.clone());

            Ok(Arc::new(session_verification_controller))
        })
    }

    /// Log out the current user. This method returns an optional URL that
    /// should be presented to the user to complete logout (in the case of
    /// Session having been authenticated using OIDC).
    pub fn logout(&self) -> Result<Option<String>, ClientError> {
        let Some(auth_api) = self.inner.auth_api() else {
            return Err(anyhow!("Missing authentication API").into());
        };

        match auth_api {
            AuthApi::Matrix(a) => {
                tracing::info!("Logging out via the homeserver.");
                RUNTIME.block_on(a.logout())?;
                Ok(None)
            }
            AuthApi::Oidc(api) => {
                tracing::info!("Logging out via OIDC.");
                let end_session_builder = RUNTIME.block_on(api.logout())?;

                if let Some(builder) = end_session_builder {
                    let url = builder.build()?.url;
                    return Ok(Some(url.to_string()));
                }

                Ok(None)
            }
            _ => Err(anyhow!("Unknown authentication API").into()),
        }
    }

    /// Registers a pusher with given parameters
    pub async fn set_pusher(
        &self,
        identifiers: PusherIdentifiers,
        kind: PusherKind,
        app_display_name: String,
        device_display_name: String,
        profile_tag: Option<String>,
        lang: String,
    ) -> Result<(), ClientError> {
        let ids = identifiers.into();

        let pusher_init = PusherInit {
            ids,
            kind: kind.try_into()?,
            app_display_name,
            device_display_name,
            profile_tag,
            lang,
        };
        self.inner.pusher().set(pusher_init.into()).await?;
        Ok(())
    }

    /// Deletes a pusher of given pusher ids
    pub async fn delete_pusher(&self, identifiers: PusherIdentifiers) -> Result<(), ClientError> {
        self.inner.pusher().delete(identifiers.into()).await?;
        Ok(())
    }

    /// The homeserver this client is configured to use.
    pub fn homeserver(&self) -> String {
        self.inner.homeserver().to_string()
    }

    pub fn rooms(&self) -> Vec<Arc<Room>> {
        self.inner.rooms().into_iter().map(|room| Arc::new(Room::new(room))).collect()
    }

    pub fn get_dm_room(&self, user_id: String) -> Result<Option<Arc<Room>>, ClientError> {
        let user_id = UserId::parse(user_id)?;
        let sdk_room = self.inner.get_dm_room(&user_id);
        let dm = sdk_room.map(|room| Arc::new(Room::new(room)));
        Ok(dm)
    }

    pub fn search_users(
        &self,
        search_term: String,
        limit: u64,
    ) -> Result<SearchUsersResults, ClientError> {
        RUNTIME.block_on(async move {
            let response = self.inner.search_users(&search_term, limit).await?;
            Ok(SearchUsersResults::from(response))
        })
    }

    pub fn get_profile(&self, user_id: String) -> Result<UserProfile, ClientError> {
        RUNTIME.block_on(async move {
            let owned_user_id = UserId::parse(user_id.clone())?;
            let response = self.inner.account().fetch_user_profile_of(&owned_user_id).await?;

            let user_profile = UserProfile {
                user_id,
                display_name: response.displayname.clone(),
                avatar_url: response.avatar_url.as_ref().map(|url| url.to_string()),
            };

            Ok(user_profile)
        })
    }

    pub fn notification_client(
        self: Arc<Self>,
        process_setup: NotificationProcessSetup,
    ) -> Result<Arc<NotificationClientBuilder>, ClientError> {
        NotificationClientBuilder::new(self.clone(), process_setup.into())
    }

    pub fn sync_service(&self) -> Arc<SyncServiceBuilder> {
        SyncServiceBuilder::new((*self.inner).clone())
    }

    pub fn get_notification_settings(&self) -> Arc<NotificationSettings> {
        RUNTIME.block_on(async move {
            Arc::new(NotificationSettings::new(
                (*self.inner).clone(),
                self.inner.notification_settings().await,
            ))
        })
    }

    pub fn encryption(&self) -> Arc<Encryption> {
        Arc::new(self.inner.encryption().into())
    }

    // Ignored users

    pub async fn ignored_users(&self) -> Result<Vec<String>, ClientError> {
        if let Some(raw_content) = self
            .inner
            .account()
            .fetch_account_data(GlobalAccountDataEventType::IgnoredUserList)
            .await?
        {
            let content = raw_content.deserialize_as::<IgnoredUserListEventContent>()?;
            let user_ids: Vec<String> =
                content.ignored_users.keys().map(|id| id.to_string()).collect();

            return Ok(user_ids);
        }

        Ok(vec![])
    }

    pub async fn ignore_user(&self, user_id: String) -> Result<(), ClientError> {
        let user_id = UserId::parse(user_id)?;
        self.inner.account().ignore_user(&user_id).await?;
        Ok(())
    }

    pub async fn unignore_user(&self, user_id: String) -> Result<(), ClientError> {
        let user_id = UserId::parse(user_id)?;
        self.inner.account().unignore_user(&user_id).await?;
        Ok(())
    }

    pub fn subscribe_to_ignored_users(
        &self,
        listener: Box<dyn IgnoredUsersListener>,
    ) -> Arc<TaskHandle> {
        let mut subscriber = self.inner.subscribe_to_ignore_user_list_changes();
        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            while let Some(user_ids) = subscriber.next().await {
                listener.call(user_ids);
            }
        })))
    }

    pub fn room_directory_search(&self) -> Arc<RoomDirectorySearch> {
        Arc::new(RoomDirectorySearch::new(
            matrix_sdk::room_directory_search::RoomDirectorySearch::new((*self.inner).clone()),
        ))
    }

    pub async fn join_room_by_id(&self, room_id: String) -> Result<Arc<Room>, ClientError> {
        let room_id = RoomId::parse(room_id)?;
        let room = self.inner.join_room_by_id(room_id.as_ref()).await?;
        Ok(Arc::new(Room::new(room)))
    }

    pub async fn get_recently_visited_rooms(&self) -> Result<Vec<String>, ClientError> {
        Ok(self.inner.account().get_recently_visited_rooms().await?)
    }

    pub async fn track_recently_visited_room(&self, room: String) -> Result<(), ClientError> {
        self.inner.account().track_recently_visited_room(room).await?;
        Ok(())
    }

    /// Resolves the given room alias to a room id, if possible.
    pub async fn resolve_room_alias(&self, room_alias: String) -> Result<String, ClientError> {
        let room_alias = RoomAliasId::parse(&room_alias)?;
        let response = self.inner.resolve_room_alias(&room_alias).await?;
        Ok(response.room_id.to_string())
    }

    /// Get the preview of a room, to interact with it.
    pub async fn get_room_preview(
        &self,
        room_id_or_alias: String,
    ) -> Result<RoomPreview, ClientError> {
        let room_id = if let Ok(parsed_room_id) = RoomId::parse(&room_id_or_alias) {
            parsed_room_id
        } else {
            // Try to resolve it as an alias.
            let Ok(room_alias) = RoomAliasId::parse(&room_id_or_alias) else {
                return Err(anyhow!("room_id_or_alias is neither a room id or an alias").into());
            };
            let response = self.inner.resolve_room_alias(&room_alias).await?;
            response.room_id
        };

        let sdk_room_preview = self.inner.get_room_preview(&room_id).await?;

        Ok(RoomPreview::from_sdk(room_id, sdk_room_preview))
    }
}

#[uniffi::export(callback_interface)]
pub trait IgnoredUsersListener: Sync + Send {
    fn call(&self, ignored_user_ids: Vec<String>);
}

#[derive(uniffi::Enum)]
pub enum NotificationProcessSetup {
    MultipleProcesses,
    SingleProcess { sync_service: Arc<SyncService> },
}

impl From<NotificationProcessSetup> for MatrixNotificationProcessSetup {
    fn from(value: NotificationProcessSetup) -> Self {
        match value {
            NotificationProcessSetup::MultipleProcesses => {
                MatrixNotificationProcessSetup::MultipleProcesses
            }
            NotificationProcessSetup::SingleProcess { sync_service } => {
                MatrixNotificationProcessSetup::SingleProcess {
                    sync_service: sync_service.inner.clone(),
                }
            }
        }
    }
}

#[derive(uniffi::Record)]
pub struct SearchUsersResults {
    pub results: Vec<UserProfile>,
    pub limited: bool,
}

impl From<search_users::v3::Response> for SearchUsersResults {
    fn from(value: search_users::v3::Response) -> Self {
        let results: Vec<UserProfile> = value.results.iter().map(UserProfile::from).collect();
        SearchUsersResults { results, limited: value.limited }
    }
}

#[derive(uniffi::Record)]
pub struct UserProfile {
    pub user_id: String,
    pub display_name: Option<String>,
    pub avatar_url: Option<String>,
}

impl From<&search_users::v3::User> for UserProfile {
    fn from(value: &search_users::v3::User) -> Self {
        UserProfile {
            user_id: value.user_id.to_string(),
            display_name: value.display_name.clone(),
            avatar_url: value.avatar_url.as_ref().map(|url| url.to_string()),
        }
    }
}

impl Client {
    fn process_session_change(&self, session_change: SessionChange) {
        if let Some(delegate) = self.delegate.read().unwrap().clone() {
            debug!("Applying session change: {session_change:?}");
            RUNTIME.spawn_blocking(move || match session_change {
                SessionChange::UnknownToken { soft_logout } => {
                    delegate.did_receive_auth_error(soft_logout);
                }
                SessionChange::TokensRefreshed => {
                    delegate.did_refresh_tokens();
                }
            });
        } else {
            debug!(
                "No client delegate found, session change couldn't be applied: {session_change:?}"
            );
        }
    }

    fn retrieve_session(
        session_delegate: Arc<dyn ClientSessionDelegate>,
        user_id: &UserId,
    ) -> anyhow::Result<SessionTokens> {
        let session = session_delegate.retrieve_session_from_keychain(user_id.to_string())?;
        let auth_session = TryInto::<AuthSession>::try_into(session)?;
        match auth_session {
            AuthSession::Oidc(session) => Ok(SessionTokens::Oidc(session.user.tokens)),
            AuthSession::Matrix(session) => Ok(SessionTokens::Matrix(session.tokens)),
            _ => anyhow::bail!("Unexpected session kind."),
        }
    }

    async fn session_inner(client: matrix_sdk::Client) -> Result<Session, ClientError> {
        let auth_api = client.auth_api().context("Missing authentication API")?;

        let homeserver_url = client.homeserver().into();
        let sliding_sync_proxy = client.sliding_sync_proxy().map(|url| url.to_string());

        Session::new(auth_api, homeserver_url, sliding_sync_proxy)
    }

    async fn save_session(
        session_delegate: Arc<dyn ClientSessionDelegate>,
        client: matrix_sdk::Client,
    ) -> anyhow::Result<()> {
        let session = Self::session_inner(client).await?;
        session_delegate.save_session_in_keychain(session);
        Ok(())
    }
}

#[derive(uniffi::Record)]
pub struct NotificationPowerLevels {
    pub room: i32,
}

impl From<NotificationPowerLevels> for ruma::power_levels::NotificationPowerLevels {
    fn from(value: NotificationPowerLevels) -> Self {
        let mut notification_power_levels = Self::new();
        notification_power_levels.room = value.room.into();
        notification_power_levels
    }
}

#[derive(uniffi::Record)]
pub struct PowerLevels {
    pub users_default: Option<i32>,
    pub events_default: Option<i32>,
    pub state_default: Option<i32>,
    pub ban: Option<i32>,
    pub kick: Option<i32>,
    pub redact: Option<i32>,
    pub invite: Option<i32>,
    pub notifications: Option<NotificationPowerLevels>,
    pub users: HashMap<String, i32>,
    pub events: HashMap<String, i32>,
}

impl From<PowerLevels> for RoomPowerLevelsEventContent {
    fn from(value: PowerLevels) -> Self {
        let mut power_levels = RoomPowerLevelsEventContent::new();

        if let Some(users_default) = value.users_default {
            power_levels.users_default = users_default.into();
        }
        if let Some(state_default) = value.state_default {
            power_levels.state_default = state_default.into();
        }
        if let Some(events_default) = value.events_default {
            power_levels.events_default = events_default.into();
        }
        if let Some(ban) = value.ban {
            power_levels.ban = ban.into();
        }
        if let Some(kick) = value.kick {
            power_levels.kick = kick.into();
        }
        if let Some(redact) = value.redact {
            power_levels.redact = redact.into();
        }
        if let Some(invite) = value.invite {
            power_levels.invite = invite.into();
        }
        if let Some(notifications) = value.notifications {
            power_levels.notifications = notifications.into()
        }
        power_levels.users = value
            .users
            .iter()
            .filter_map(|(user_id, power_level)| match UserId::parse(user_id) {
                Ok(id) => Some((id, (*power_level).into())),
                Err(e) => {
                    error!(user_id, "Skipping invalid user ID, error: {e}");
                    None
                }
            })
            .collect();

        power_levels.events = value
            .events
            .iter()
            .map(|(event_type, power_level)| {
                let event_type: ruma::events::TimelineEventType = event_type.as_str().into();
                (event_type, (*power_level).into())
            })
            .collect();

        power_levels
    }
}

#[derive(uniffi::Record)]
pub struct CreateRoomParameters {
    pub name: Option<String>,
    #[uniffi(default = None)]
    pub topic: Option<String>,
    pub is_encrypted: bool,
    #[uniffi(default = false)]
    pub is_direct: bool,
    pub visibility: RoomVisibility,
    pub preset: RoomPreset,
    #[uniffi(default = None)]
    pub invite: Option<Vec<String>>,
    #[uniffi(default = None)]
    pub avatar: Option<String>,
    #[uniffi(default = None)]
    pub power_level_content_override: Option<PowerLevels>,
}

impl From<CreateRoomParameters> for create_room::v3::Request {
    fn from(value: CreateRoomParameters) -> create_room::v3::Request {
        let mut request = create_room::v3::Request::new();
        request.name = value.name;
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

        if let Some(power_levels) = value.power_level_content_override {
            match Raw::new(&power_levels.into()) {
                Ok(power_levels) => {
                    request.power_level_content_override = Some(power_levels);
                }
                Err(e) => {
                    error!("Failed to serialize power levels, error: {e}");
                }
            }
        }

        request
    }
}

#[derive(uniffi::Enum)]
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

#[derive(uniffi::Enum)]
#[allow(clippy::enum_variant_names)]
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
    /// The URL for the homeserver used for this session.
    pub homeserver_url: String,
    /// Additional data for this session if OpenID Connect was used for
    /// authentication.
    pub oidc_data: Option<String>,
    /// The URL for the sliding sync proxy used for this session.
    pub sliding_sync_proxy: Option<String>,
}

impl Session {
    fn new(
        auth_api: AuthApi,
        homeserver_url: String,
        sliding_sync_proxy: Option<String>,
    ) -> Result<Session, ClientError> {
        match auth_api {
            // Build the session from the regular Matrix Auth Session.
            AuthApi::Matrix(a) => {
                let matrix_sdk::matrix_auth::MatrixSession {
                    meta: matrix_sdk::SessionMeta { user_id, device_id },
                    tokens:
                        matrix_sdk::matrix_auth::MatrixSessionTokens { access_token, refresh_token },
                } = a.session().context("Missing session")?;

                Ok(Session {
                    access_token,
                    refresh_token,
                    user_id: user_id.to_string(),
                    device_id: device_id.to_string(),
                    homeserver_url,
                    oidc_data: None,
                    sliding_sync_proxy,
                })
            }
            // Build the session from the OIDC UserSession.
            AuthApi::Oidc(api) => {
                let matrix_sdk::oidc::UserSession {
                    meta: matrix_sdk::SessionMeta { user_id, device_id },
                    tokens:
                        matrix_sdk::oidc::OidcSessionTokens {
                            access_token,
                            refresh_token,
                            latest_id_token,
                        },
                    issuer_info,
                } = api.user_session().context("Missing session")?;
                let client_id = api
                    .client_credentials()
                    .context("OIDC client credentials are missing.")?
                    .client_id()
                    .to_owned();
                let client_metadata =
                    api.client_metadata().context("OIDC client metadata is missing.")?.clone();
                let oidc_data = OidcSessionData {
                    client_id,
                    client_metadata,
                    latest_id_token: latest_id_token.map(|t| t.to_string()),
                    issuer_info,
                };

                let oidc_data = serde_json::to_string(&oidc_data).ok();
                Ok(Session {
                    access_token,
                    refresh_token,
                    user_id: user_id.to_string(),
                    device_id: device_id.to_string(),
                    homeserver_url,
                    oidc_data,
                    sliding_sync_proxy,
                })
            }
            _ => Err(anyhow!("Unknown authentication API").into()),
        }
    }
}

impl TryFrom<Session> for AuthSession {
    type Error = ClientError;
    fn try_from(value: Session) -> Result<Self, Self::Error> {
        let Session {
            access_token,
            refresh_token,
            user_id,
            device_id,
            homeserver_url: _,
            oidc_data,
            sliding_sync_proxy: _,
        } = value;

        if let Some(oidc_data) = oidc_data {
            // Create an OidcSession.
            let oidc_data = serde_json::from_str::<OidcUnvalidatedSessionData>(&oidc_data)?
                .validate()
                .context("OIDC metadata validation failed.")?;
            let latest_id_token = oidc_data
                .latest_id_token
                .map(TryInto::try_into)
                .transpose()
                .context("OIDC latest_id_token is invalid.")?;

            let user_session = matrix_sdk::oidc::UserSession {
                meta: matrix_sdk::SessionMeta {
                    user_id: user_id.try_into()?,
                    device_id: device_id.into(),
                },
                tokens: matrix_sdk::oidc::OidcSessionTokens {
                    access_token,
                    refresh_token,
                    latest_id_token,
                },
                issuer_info: oidc_data.issuer_info,
            };

            let session = OidcSession {
                credentials: ClientCredentials::None { client_id: oidc_data.client_id },
                metadata: oidc_data.client_metadata,
                user: user_session,
            };

            Ok(AuthSession::Oidc(session))
        } else {
            // Create a regular Matrix Session.
            let session = matrix_sdk::matrix_auth::MatrixSession {
                meta: matrix_sdk::SessionMeta {
                    user_id: user_id.try_into()?,
                    device_id: device_id.into(),
                },
                tokens: matrix_sdk::matrix_auth::MatrixSessionTokens {
                    access_token,
                    refresh_token,
                },
            };

            Ok(AuthSession::Matrix(session))
        }
    }
}

/// Represents a client registration against an OpenID Connect authentication
/// issuer.
#[derive(Serialize)]
pub(crate) struct OidcSessionData {
    client_id: String,
    client_metadata: VerifiedClientMetadata,
    latest_id_token: Option<String>,
    issuer_info: AuthenticationServerInfo,
}

/// Represents an unverified client registration against an OpenID Connect
/// authentication issuer. Call `validate` on this to use it for restoration.
#[derive(Deserialize)]
pub(crate) struct OidcUnvalidatedSessionData {
    client_id: String,
    client_metadata: ClientMetadata,
    latest_id_token: Option<String>,
    issuer_info: AuthenticationServerInfo,
}

impl OidcUnvalidatedSessionData {
    /// Validates the data so that it can be used.
    fn validate(self) -> Result<OidcSessionData, ClientMetadataVerificationError> {
        Ok(OidcSessionData {
            client_id: self.client_id,
            client_metadata: self.client_metadata.validate()?,
            latest_id_token: self.latest_id_token,
            issuer_info: self.issuer_info,
        })
    }
}

#[derive(uniffi::Enum)]
pub enum AccountManagementAction {
    Profile,
    SessionsList,
    SessionView { device_id: String },
    SessionEnd { device_id: String },
}

impl From<AccountManagementAction> for OidcAccountManagementAction {
    fn from(value: AccountManagementAction) -> Self {
        match value {
            AccountManagementAction::Profile => Self::Profile,
            AccountManagementAction::SessionsList => Self::SessionsList,
            AccountManagementAction::SessionView { device_id } => {
                Self::SessionView { device_id: device_id.into() }
            }
            AccountManagementAction::SessionEnd { device_id } => {
                Self::SessionEnd { device_id: device_id.into() }
            }
        }
    }
}

#[uniffi::export]
fn gen_transaction_id() -> String {
    TransactionId::new().to_string()
}

/// A file handle that takes ownership of a media file on disk. When the handle
/// is dropped, the file will be removed from the disk.
#[derive(uniffi::Object)]
pub struct MediaFileHandle {
    inner: RwLock<Option<SdkMediaFileHandle>>,
}

impl MediaFileHandle {
    fn new(handle: SdkMediaFileHandle) -> Self {
        Self { inner: RwLock::new(Some(handle)) }
    }
}

#[uniffi::export]
impl MediaFileHandle {
    /// Get the media file's path.
    pub fn path(&self) -> Result<String, ClientError> {
        Ok(self
            .inner
            .read()
            .unwrap()
            .as_ref()
            .context("MediaFileHandle must not be used after calling persist")?
            .path()
            .to_str()
            .unwrap()
            .to_owned())
    }

    pub fn persist(&self, path: String) -> Result<bool, ClientError> {
        let mut guard = self.inner.write().unwrap();
        Ok(
            match guard
                .take()
                .context("MediaFileHandle was already persisted")?
                .persist(path.as_ref())
            {
                Ok(_) => true,
                Err(e) => {
                    *guard = Some(e.file);
                    false
                }
            },
        )
    }
}
