use std::{
    collections::HashMap,
    fmt::Debug,
    path::Path,
    sync::{Arc, RwLock},
};

use anyhow::{anyhow, Context as _};
use matrix_sdk::{
    media::{
        MediaFileHandle as SdkMediaFileHandle, MediaFormat, MediaRequestParameters,
        MediaThumbnailSettings,
    },
    oidc::{
        registrations::{ClientId, OidcRegistrations},
        requests::account_management::AccountManagementActionFull,
        types::{
            client_credentials::ClientCredentials,
            registration::{
                ClientMetadata, ClientMetadataVerificationError, VerifiedClientMetadata,
            },
            requests::Prompt as SdkOidcPrompt,
        },
        OidcAuthorizationData, OidcSession,
    },
    reqwest::StatusCode,
    ruma::{
        api::client::{
            push::{EmailPusherData, PusherIds, PusherInit, PusherKind as RumaPusherKind},
            room::{create_room, Visibility},
            session::get_login_types,
            user_directory::search_users,
        },
        events::{
            room::{avatar::RoomAvatarEventContent, encryption::RoomEncryptionEventContent},
            AnyInitialStateEvent, AnyToDeviceEvent, InitialStateEvent,
        },
        serde::Raw,
        EventEncryptionAlgorithm, RoomId, TransactionId, UInt, UserId,
    },
    sliding_sync::Version as SdkSlidingSyncVersion,
    AuthApi, AuthSession, Client as MatrixClient, HttpError, SessionChange, SessionTokens,
};
use matrix_sdk_ui::notification_client::{
    NotificationClient as MatrixNotificationClient,
    NotificationProcessSetup as MatrixNotificationProcessSetup,
};
use mime::Mime;
use ruma::{
    api::client::{
        alias::get_alias, discovery::discover_homeserver::AuthenticationServerInfo,
        uiaa::UserIdentifier,
    },
    events::{
        ignored_user_list::IgnoredUserListEventContent,
        room::{
            join_rules::{
                AllowRule as RumaAllowRule, JoinRule as RumaJoinRule, RoomJoinRulesEventContent,
            },
            power_levels::RoomPowerLevelsEventContent,
        },
        GlobalAccountDataEventType,
    },
    push::{HttpPusherData as RumaHttpPusherData, PushFormat as RumaPushFormat},
    OwnedServerName, RoomAliasId, RoomOrAliasId, ServerName,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, error};
use url::Url;

use super::{room::Room, session_verification::SessionVerificationController, RUNTIME};
use crate::{
    authentication::{HomeserverLoginDetails, OidcConfiguration, OidcError, SsoError, SsoHandler},
    client,
    encryption::Encryption,
    notification::NotificationClient,
    notification_settings::NotificationSettings,
    room_directory_search::RoomDirectorySearch,
    room_preview::RoomPreview,
    ruma::{AuthData, MediaSource},
    sync_service::{SyncService, SyncServiceBuilder},
    task_handle::TaskHandle,
    utils::AsyncRuntimeDropped,
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
                    ruma_data.data.insert("default_payload".to_owned(), json);
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

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait ClientDelegate: Sync + Send {
    fn did_receive_auth_error(&self, is_soft_logout: bool);
    fn did_refresh_tokens(&self);
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait ClientSessionDelegate: Sync + Send {
    fn retrieve_session_from_keychain(&self, user_id: String) -> Result<Session, ClientError>;
    fn save_session_in_keychain(&self, session: Session);
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait ProgressWatcher: Send + Sync {
    fn transmission_progress(&self, progress: TransmissionProgress);
}

/// A listener to the global (client-wide) error reporter of the send queue.
#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait SendQueueRoomErrorListener: Sync + Send {
    /// Called every time the send queue has ran into an error for a given room,
    /// which will disable the send queue for that particular room.
    fn on_error(&self, room_id: String, error: ClientError);
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
    pub(crate) inner: AsyncRuntimeDropped<MatrixClient>,
    delegate: RwLock<Option<Arc<dyn ClientDelegate>>>,
    session_verification_controller:
        Arc<tokio::sync::RwLock<Option<SessionVerificationController>>>,
}

impl Client {
    pub async fn new(
        sdk_client: MatrixClient,
        enable_oidc_refresh_lock: bool,
        session_delegate: Option<Arc<dyn ClientSessionDelegate>>,
    ) -> Result<Self, ClientError> {
        let session_verification_controller: Arc<
            tokio::sync::RwLock<Option<SessionVerificationController>>,
        > = Default::default();
        let controller = session_verification_controller.clone();

        sdk_client.add_event_handler(move |ev: AnyToDeviceEvent| async move {
            if let Some(session_verification_controller) = &*controller.clone().read().await {
                session_verification_controller.process_to_device_message(ev).await;
            } else {
                debug!("received to-device message, but verification controller isn't ready");
            }
        });

        let cross_process_store_locks_holder_name =
            sdk_client.cross_process_store_locks_holder_name().to_owned();

        let client = Client {
            inner: AsyncRuntimeDropped::new(sdk_client),
            delegate: RwLock::new(None),
            session_verification_controller,
        };

        if enable_oidc_refresh_lock {
            if session_delegate.is_none() {
                return Err(anyhow::anyhow!(
                    "missing session delegates when enabling the cross-process lock"
                ))?;
            }

            client
                .inner
                .oidc()
                .enable_cross_process_refresh_lock(cross_process_store_locks_holder_name)
                .await?;
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
                        Ok(Self::save_session(session_delegate, client)?)
                    })
                },
            )?;
        }

        Ok(client)
    }
}

#[matrix_sdk_ffi_macros::export]
impl Client {
    /// Information about login options for the client's homeserver.
    pub async fn homeserver_login_details(&self) -> Arc<HomeserverLoginDetails> {
        let oidc = self.inner.oidc();
        let (supports_oidc_login, supported_oidc_prompts) = match oidc
            .fetch_authentication_issuer()
            .await
        {
            Ok(issuer) => match &oidc.given_provider_metadata(&issuer).await {
                Ok(metadata) => {
                    let prompts = metadata
                        .prompt_values_supported
                        .as_ref()
                        .map_or_else(Vec::new, |prompts| prompts.iter().map(Into::into).collect());

                    (true, prompts)
                }
                Err(error) => {
                    error!("Failed to fetch OIDC provider metadata: {error}");
                    (true, Default::default())
                }
            },
            Err(error) => {
                error!("Failed to fetch authentication issuer: {error}");
                (false, Default::default())
            }
        };

        let supports_password_login = self.supports_password_login().await.ok().unwrap_or(false);
        let sliding_sync_version = self.sliding_sync_version();

        Arc::new(HomeserverLoginDetails {
            url: self.homeserver(),
            sliding_sync_version,
            supports_oidc_login,
            supported_oidc_prompts,
            supports_password_login,
        })
    }

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

    /// Login using JWT
    /// This is an implementation of the custom_login https://docs.rs/matrix-sdk/latest/matrix_sdk/matrix_auth/struct.MatrixAuth.html#method.login_custom
    /// For more information on logging in with JWT: https://element-hq.github.io/synapse/latest/jwt.html
    pub async fn custom_login_with_jwt(
        &self,
        jwt: String,
        initial_device_name: Option<String>,
        device_id: Option<String>,
    ) -> Result<(), ClientError> {
        let data = json!({ "token": jwt }).as_object().unwrap().clone();

        let mut builder = self.inner.matrix_auth().login_custom("org.matrix.login.jwt", data)?;

        if let Some(initial_device_name) = initial_device_name.as_ref() {
            builder = builder.initial_device_display_name(initial_device_name);
        }

        if let Some(device_id) = device_id.as_ref() {
            builder = builder.device_id(device_id);
        }

        builder.send().await?;
        Ok(())
    }

    /// Login using an email and password.
    pub async fn login_with_email(
        &self,
        email: String,
        password: String,
        initial_device_name: Option<String>,
        device_id: Option<String>,
    ) -> Result<(), ClientError> {
        let mut builder = self
            .inner
            .matrix_auth()
            .login_identifier(UserIdentifier::Email { address: email }, &password);

        if let Some(initial_device_name) = initial_device_name.as_ref() {
            builder = builder.initial_device_display_name(initial_device_name);
        }

        if let Some(device_id) = device_id.as_ref() {
            builder = builder.device_id(device_id);
        }

        builder.send().await?;

        Ok(())
    }

    /// Returns a handler to start the SSO login process.
    pub(crate) async fn start_sso_login(
        self: &Arc<Self>,
        redirect_url: String,
        idp_id: Option<String>,
    ) -> Result<Arc<SsoHandler>, SsoError> {
        let auth = self.inner.matrix_auth();
        let url = auth
            .get_sso_login_url(redirect_url.as_str(), idp_id.as_deref())
            .await
            .map_err(|e| SsoError::Generic { message: e.to_string() })?;
        Ok(Arc::new(SsoHandler { client: Arc::clone(self), url }))
    }

    /// Requests the URL needed for opening a web view using OIDC. Once the web
    /// view has succeeded, call `login_with_oidc_callback` with the callback it
    /// returns. If a failure occurs and a callback isn't available, make sure
    /// to call `abort_oidc_auth` to inform the client of this.
    pub async fn url_for_oidc(
        &self,
        oidc_configuration: &OidcConfiguration,
        prompt: OidcPrompt,
    ) -> Result<Arc<OidcAuthorizationData>, OidcError> {
        let oidc_metadata: VerifiedClientMetadata = oidc_configuration.try_into()?;
        let registrations_file = Path::new(&oidc_configuration.dynamic_registrations_file);
        let static_registrations = oidc_configuration
            .static_registrations
            .iter()
            .filter_map(|(issuer, client_id)| {
                let Ok(issuer) = Url::parse(issuer) else {
                    tracing::error!("Failed to parse {:?}", issuer);
                    return None;
                };
                Some((issuer, ClientId(client_id.clone())))
            })
            .collect::<HashMap<_, _>>();
        let registrations = OidcRegistrations::new(
            registrations_file,
            oidc_metadata.clone(),
            static_registrations,
        )?;

        let data =
            self.inner.oidc().url_for_oidc(oidc_metadata, registrations, prompt.into()).await?;

        Ok(Arc::new(data))
    }

    /// Aborts an existing OIDC login operation that might have been cancelled,
    /// failed etc.
    pub async fn abort_oidc_auth(&self, authorization_data: Arc<OidcAuthorizationData>) {
        self.inner.oidc().abort_authorization(&authorization_data.state).await;
    }

    /// Completes the OIDC login process.
    pub async fn login_with_oidc_callback(
        &self,
        authorization_data: Arc<OidcAuthorizationData>,
        callback_url: String,
    ) -> Result<(), OidcError> {
        let url = Url::parse(&callback_url).or(Err(OidcError::CallbackUrlInvalid))?;

        self.inner.oidc().login_with_oidc_callback(&authorization_data, url).await?;

        Ok(())
    }

    pub async fn get_media_file(
        &self,
        media_source: Arc<MediaSource>,
        filename: Option<String>,
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
                &MediaRequestParameters { source: source.media_source, format: MediaFormat::File },
                filename,
                &mime_type,
                use_cache,
                temp_dir,
            )
            .await?;

        Ok(Arc::new(MediaFileHandle::new(handle)))
    }

    /// Restores the client from a `Session`.
    pub async fn restore_session(&self, session: Session) -> Result<(), ClientError> {
        let sliding_sync_version = session.sliding_sync_version.clone();
        let auth_session: AuthSession = session.try_into()?;

        self.restore_session_inner(auth_session).await?;
        self.inner.set_sliding_sync_version(sliding_sync_version.try_into()?);

        Ok(())
    }

    /// Enables or disables all the room send queues at once.
    ///
    /// When connectivity is lost on a device, it is recommended to disable the
    /// room sending queues.
    ///
    /// This can be controlled for individual rooms, using
    /// [`Room::enable_send_queue`].
    pub async fn enable_all_send_queues(&self, enable: bool) {
        self.inner.send_queue().set_enabled(enable).await;
    }

    /// Subscribe to the global enablement status of the send queue, at the
    /// client-wide level.
    ///
    /// The given listener will be immediately called with the initial value of
    /// the enablement status.
    pub fn subscribe_to_send_queue_status(
        &self,
        listener: Box<dyn SendQueueRoomErrorListener>,
    ) -> Arc<TaskHandle> {
        let q = self.inner.send_queue();
        let mut subscriber = q.subscribe_errors();

        Arc::new(TaskHandle::new(RUNTIME.spawn(async move {
            // Respawn tasks for rooms that had unsent events. At this point we've just
            // created the subscriber, so it'll be notified about errors.
            q.respawn_tasks_for_rooms_with_unsent_requests().await;

            loop {
                match subscriber.recv().await {
                    Ok(report) => listener
                        .on_error(report.room_id.to_string(), ClientError::new(report.error)),
                    Err(err) => {
                        error!("error when listening to the send queue error reporter: {err}");
                    }
                }
            }
        })))
    }

    /// Allows generic GET requests to be made through the SDKs internal HTTP
    /// client
    pub async fn get_url(&self, url: String) -> Result<String, ClientError> {
        let http_client = self.inner.http_client();
        Ok(http_client.get(url).send().await?.text().await?)
    }

    /// Empty the server version and unstable features cache.
    ///
    /// Since the SDK caches server capabilities (versions and unstable
    /// features), it's possible to have a stale entry in the cache. This
    /// functions makes it possible to force reset it.
    pub async fn reset_server_capabilities(&self) -> Result<(), ClientError> {
        Ok(self.inner.reset_server_capabilities().await?)
    }
}

impl Client {
    /// Restores the client from an `AuthSession`.
    pub(crate) async fn restore_session_inner(
        &self,
        session: impl Into<AuthSession>,
    ) -> anyhow::Result<()> {
        self.inner.restore_session(session).await?;
        Ok(())
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
}

#[matrix_sdk_ffi_macros::export]
impl Client {
    /// The sliding sync version.
    pub fn sliding_sync_version(&self) -> SlidingSyncVersion {
        self.inner.sliding_sync_version().into()
    }

    /// Find all sliding sync versions that are available.
    ///
    /// Be careful: This method may hit the store and will send new requests for
    /// each call. It can be costly to call it repeatedly.
    ///
    /// If `.well-known` or `/versions` is unreachable, it will simply move
    /// potential sliding sync versions aside. No error will be reported.
    pub async fn available_sliding_sync_versions(&self) -> Vec<SlidingSyncVersion> {
        self.inner.available_sliding_sync_versions().await.into_iter().map(Into::into).collect()
    }

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
        Self::session_inner((*self.inner).clone())
    }

    pub async fn account_url(
        &self,
        action: Option<AccountManagementAction>,
    ) -> Result<Option<String>, ClientError> {
        if !matches!(self.inner.auth_api(), Some(AuthApi::Oidc(..))) {
            return Ok(None);
        }

        match self.inner.oidc().account_management_url(action.map(Into::into)).await {
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

    /// The server name part of the current user ID
    pub fn user_id_server_name(&self) -> Result<String, ClientError> {
        let user_id = self.inner.user_id().context("No User ID found")?;
        Ok(user_id.server_name().to_string())
    }

    pub async fn display_name(&self) -> Result<String, ClientError> {
        let display_name =
            self.inner.account().get_display_name().await?.context("No User ID found")?;
        Ok(display_name)
    }

    pub async fn set_display_name(&self, name: String) -> Result<(), ClientError> {
        self.inner
            .account()
            .set_display_name(Some(name.as_str()))
            .await
            .context("Unable to set display name")?;
        Ok(())
    }

    pub async fn upload_avatar(&self, mime_type: String, data: Vec<u8>) -> Result<(), ClientError> {
        let mime: Mime = mime_type.parse()?;
        self.inner.account().upload_avatar(&mime, data).await?;
        Ok(())
    }

    pub async fn remove_avatar(&self) -> Result<(), ClientError> {
        self.inner.account().set_avatar_url(None).await?;
        Ok(())
    }

    /// Sends a request to retrieve the avatar URL. Will fill the cache used by
    /// [`Self::cached_avatar_url`] on success.
    pub async fn avatar_url(&self) -> Result<Option<String>, ClientError> {
        let avatar_url = self.inner.account().get_avatar_url().await?;

        Ok(avatar_url.map(|u| u.to_string()))
    }

    /// Retrieves an avatar cached from a previous call to [`Self::avatar_url`].
    pub fn cached_avatar_url(&self) -> Result<Option<String>, ClientError> {
        Ok(RUNTIME.block_on(self.inner.account().get_cached_avatar_url())?.map(Into::into))
    }

    pub fn device_id(&self) -> Result<String, ClientError> {
        let device_id = self.inner.device_id().context("No Device ID found")?;
        Ok(device_id.to_string())
    }

    pub async fn create_room(&self, request: CreateRoomParameters) -> Result<String, ClientError> {
        let response = self.inner.create_room(request.try_into()?).await?;
        Ok(String::from(response.room_id()))
    }

    /// Get the content of the event of the given type out of the account data
    /// store.
    ///
    /// It will be returned as a JSON string.
    pub async fn account_data(&self, event_type: String) -> Result<Option<String>, ClientError> {
        let event = self.inner.account().account_data_raw(event_type.into()).await?;
        Ok(event.map(|e| e.json().get().to_owned()))
    }

    /// Set the given account data content for the given event type.
    ///
    /// It should be supplied as a JSON string.
    pub async fn set_account_data(
        &self,
        event_type: String,
        content: String,
    ) -> Result<(), ClientError> {
        let raw_content = Raw::from_json_string(content)?;
        self.inner.account().set_account_data_raw(event_type.into(), raw_content).await?;
        Ok(())
    }

    pub async fn upload_media(
        &self,
        mime_type: String,
        data: Vec<u8>,
        progress_watcher: Option<Box<dyn ProgressWatcher>>,
    ) -> Result<String, ClientError> {
        let mime_type: mime::Mime = mime_type.parse().context("Parsing mime type")?;
        let request = self.inner.media().upload(&mime_type, data, None);

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
        let source = (*media_source).clone().media_source;

        debug!(?source, "requesting media file");
        Ok(self
            .inner
            .media()
            .get_media_content(&MediaRequestParameters { source, format: MediaFormat::File }, true)
            .await?)
    }

    pub async fn get_media_thumbnail(
        &self,
        media_source: Arc<MediaSource>,
        width: u64,
        height: u64,
    ) -> Result<Vec<u8>, ClientError> {
        let source = (*media_source).clone().media_source;

        debug!(?source, width, height, "requesting media thumbnail");
        Ok(self
            .inner
            .media()
            .get_media_content(
                &MediaRequestParameters {
                    source,
                    format: MediaFormat::Thumbnail(MediaThumbnailSettings::new(
                        UInt::new(width).unwrap(),
                        UInt::new(height).unwrap(),
                    )),
                },
                true,
            )
            .await?)
    }

    pub async fn get_session_verification_controller(
        &self,
    ) -> Result<Arc<SessionVerificationController>, ClientError> {
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
    }

    /// Log out the current user. This method returns an optional URL that
    /// should be presented to the user to complete logout (in the case of
    /// Session having been authenticated using OIDC).
    pub async fn logout(&self) -> Result<Option<String>, ClientError> {
        let Some(auth_api) = self.inner.auth_api() else {
            return Err(anyhow!("Missing authentication API").into());
        };

        match auth_api {
            AuthApi::Matrix(a) => {
                tracing::info!("Logging out via the homeserver.");
                a.logout().await?;
                Ok(None)
            }

            AuthApi::Oidc(api) => {
                tracing::info!("Logging out via OIDC.");
                let end_session_builder = api.logout().await?;

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

    /// The URL of the server.
    ///
    /// Not to be confused with the `Self::homeserver`. `server` is usually
    /// the server part in a user ID, e.g. with `@mnt_io:matrix.org`, here
    /// `matrix.org` is the server, whilst `matrix-client.matrix.org` is the
    /// homeserver (at the time of writing — 2024-08-28).
    ///
    /// This value is optional depending on how the `Client` has been built.
    /// If it's been built from a homeserver URL directly, we don't know the
    /// server. However, if the `Client` has been built from a server URL or
    /// name, then the homeserver has been discovered, and we know both.
    pub fn server(&self) -> Option<String> {
        self.inner.server().map(ToString::to_string)
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

    pub async fn search_users(
        &self,
        search_term: String,
        limit: u64,
    ) -> Result<SearchUsersResults, ClientError> {
        let response = self.inner.search_users(&search_term, limit).await?;
        Ok(SearchUsersResults::from(response))
    }

    pub async fn get_profile(&self, user_id: String) -> Result<UserProfile, ClientError> {
        let owned_user_id = UserId::parse(user_id.clone())?;

        let response = self.inner.account().fetch_user_profile_of(&owned_user_id).await?;

        Ok(UserProfile {
            user_id,
            display_name: response.displayname.clone(),
            avatar_url: response.avatar_url.as_ref().map(|url| url.to_string()),
        })
    }

    pub async fn notification_client(
        self: Arc<Self>,
        process_setup: NotificationProcessSetup,
    ) -> Result<Arc<NotificationClient>, ClientError> {
        Ok(Arc::new(NotificationClient {
            inner: MatrixNotificationClient::new((*self.inner).clone(), process_setup.into())
                .await?,
            _client: self.clone(),
        }))
    }

    pub fn sync_service(&self) -> Arc<SyncServiceBuilder> {
        SyncServiceBuilder::new((*self.inner).clone())
    }

    pub fn get_notification_settings(&self) -> Arc<NotificationSettings> {
        let inner = RUNTIME.block_on(self.inner.notification_settings());

        Arc::new(NotificationSettings::new((*self.inner).clone(), inner))
    }

    pub fn encryption(self: Arc<Self>) -> Arc<Encryption> {
        Arc::new(Encryption { inner: self.inner.encryption(), _client: self.clone() })
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

    /// Join a room by its ID.
    ///
    /// Use this method when the homeserver already knows of the given room ID.
    /// Otherwise use `join_room_by_id_or_alias` so you can pass a list of
    /// server names for the homeserver to find the room.
    pub async fn join_room_by_id(&self, room_id: String) -> Result<Arc<Room>, ClientError> {
        let room_id = RoomId::parse(room_id)?;
        let room = self.inner.join_room_by_id(room_id.as_ref()).await?;
        Ok(Arc::new(Room::new(room)))
    }

    /// Join a room by its ID or alias.
    ///
    /// When supplying the room's ID, you can also supply a list of server names
    /// for the homeserver to find the room. Typically these server names
    /// come from a permalink's `via` parameters, or from resolving a room's
    /// alias into an ID.
    pub async fn join_room_by_id_or_alias(
        &self,
        room_id_or_alias: String,
        server_names: Vec<String>,
    ) -> Result<Arc<Room>, ClientError> {
        let room_id = RoomOrAliasId::parse(&room_id_or_alias)?;
        let server_names = server_names
            .iter()
            .map(|name| OwnedServerName::try_from(name.as_str()))
            .collect::<Result<Vec<_>, _>>()?;
        let room =
            self.inner.join_room_by_id_or_alias(room_id.as_ref(), server_names.as_ref()).await?;
        Ok(Arc::new(Room::new(room)))
    }

    /// Knock on a room to join it using its ID or alias.
    pub async fn knock(
        &self,
        room_id_or_alias: String,
        reason: Option<String>,
        server_names: Vec<String>,
    ) -> Result<Arc<Room>, ClientError> {
        let room_id = RoomOrAliasId::parse(&room_id_or_alias)?;
        let server_names =
            server_names.iter().map(ServerName::parse).collect::<Result<Vec<_>, _>>()?;
        let room = self.inner.knock(room_id, reason, server_names).await?;
        Ok(Arc::new(Room::new(room)))
    }

    pub async fn get_recently_visited_rooms(&self) -> Result<Vec<String>, ClientError> {
        Ok(self
            .inner
            .account()
            .get_recently_visited_rooms()
            .await?
            .into_iter()
            .map(Into::into)
            .collect())
    }

    pub async fn track_recently_visited_room(&self, room: String) -> Result<(), ClientError> {
        let room_id = RoomId::parse(room)?;
        self.inner.account().track_recently_visited_room(room_id).await?;
        Ok(())
    }

    /// Resolves the given room alias to a room ID (and a list of servers), if
    /// possible.
    pub async fn resolve_room_alias(
        &self,
        room_alias: String,
    ) -> Result<Option<ResolvedRoomAlias>, ClientError> {
        let room_alias = RoomAliasId::parse(&room_alias)?;
        match self.inner.resolve_room_alias(&room_alias).await {
            Ok(response) => Ok(Some(response.into())),
            Err(HttpError::Reqwest(http_error)) => match http_error.status() {
                Some(StatusCode::NOT_FOUND) => Ok(None),
                _ => Err(http_error.into()),
            },
            Err(error) => Err(error.into()),
        }
    }

    /// Checks if a room alias exists in the current homeserver.
    pub async fn room_alias_exists(&self, room_alias: String) -> Result<bool, ClientError> {
        self.resolve_room_alias(room_alias).await.map(|ret| ret.is_some())
    }

    /// Given a room id, get the preview of a room, to interact with it.
    ///
    /// The list of `via_servers` must be a list of servers that know
    /// about the room and can resolve it, and that may appear as a `via`
    /// parameter in e.g. a permalink URL. This list can be empty.
    pub async fn get_room_preview_from_room_id(
        &self,
        room_id: String,
        via_servers: Vec<String>,
    ) -> Result<Arc<RoomPreview>, ClientError> {
        let room_id = RoomId::parse(&room_id).context("room_id is not a valid room id")?;

        let via_servers = via_servers
            .into_iter()
            .map(ServerName::parse)
            .collect::<Result<Vec<_>, _>>()
            .context("at least one `via` server name is invalid")?;

        // The `into()` call below doesn't work if I do `(&room_id).into()`, so I let
        // rustc win that one fight.
        let room_id: &RoomId = &room_id;

        let room_preview = self.inner.get_room_preview(room_id.into(), via_servers).await?;

        Ok(Arc::new(RoomPreview::new(self.inner.clone(), room_preview)))
    }

    /// Given a room alias, get the preview of a room, to interact with it.
    pub async fn get_room_preview_from_room_alias(
        &self,
        room_alias: String,
    ) -> Result<Arc<RoomPreview>, ClientError> {
        let room_alias =
            RoomAliasId::parse(&room_alias).context("room_alias is not a valid room alias")?;

        // The `into()` call below doesn't work if I do `(&room_id).into()`, so I let
        // rustc win that one fight.
        let room_alias: &RoomAliasId = &room_alias;

        let room_preview = self.inner.get_room_preview(room_alias.into(), Vec::new()).await?;

        Ok(Arc::new(RoomPreview::new(self.inner.clone(), room_preview)))
    }

    /// Waits until an at least partially synced room is received, and returns
    /// it.
    ///
    /// **Note: this function will loop endlessly until either it finds the room
    /// or an externally set timeout happens.**
    pub async fn await_room_remote_echo(&self, room_id: String) -> Result<Arc<Room>, ClientError> {
        let room_id = RoomId::parse(room_id)?;
        Ok(Arc::new(Room::new(self.inner.await_room_remote_echo(&room_id).await)))
    }

    /// Lets the user know whether this is an `m.login.password` based
    /// auth and if the account can actually be deactivated
    pub fn can_deactivate_account(&self) -> bool {
        matches!(self.inner.auth_api(), Some(AuthApi::Matrix(_)))
    }

    /// Deactivate this account definitively.
    /// Similarly to `encryption::reset_identity` this
    /// will only work with password-based authentication (`m.login.password`)
    ///
    /// # Arguments
    ///
    /// * `auth_data` - This request uses the [User-Interactive Authentication
    ///   API][uiaa]. The first request needs to set this to `None` and will
    ///   always fail and the same request needs to be made but this time with
    ///   some `auth_data` provided.
    pub async fn deactivate_account(
        &self,
        auth_data: Option<AuthData>,
        erase_data: bool,
    ) -> Result<(), ClientError> {
        if let Some(auth_data) = auth_data {
            _ = self.inner.account().deactivate(None, Some(auth_data.into()), erase_data).await?;
        } else {
            _ = self.inner.account().deactivate(None, None, erase_data).await?;
        }

        Ok(())
    }

    /// Checks if a room alias is not in use yet.
    ///
    /// Returns:
    /// - `Ok(true)` if the room alias is available.
    /// - `Ok(false)` if it's not (the resolve alias request returned a `404`
    ///   status code).
    /// - An `Err` otherwise.
    pub async fn is_room_alias_available(&self, alias: String) -> Result<bool, ClientError> {
        let alias = RoomAliasId::parse(alias)?;
        self.inner.is_room_alias_available(&alias).await.map_err(Into::into)
    }
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
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

/// Information about a room, that was resolved from a room alias.
#[derive(uniffi::Record)]
pub struct ResolvedRoomAlias {
    /// The room ID that the alias resolved to.
    pub room_id: String,
    /// A list of servers that can be used to find the room by its room ID.
    pub servers: Vec<String>,
}

impl From<get_alias::v3::Response> for ResolvedRoomAlias {
    fn from(value: get_alias::v3::Response) -> Self {
        Self {
            room_id: value.room_id.to_string(),
            servers: value.servers.iter().map(ToString::to_string).collect(),
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

    fn session_inner(client: matrix_sdk::Client) -> Result<Session, ClientError> {
        let auth_api = client.auth_api().context("Missing authentication API")?;

        let homeserver_url = client.homeserver().into();
        let sliding_sync_version = client.sliding_sync_version();

        Session::new(auth_api, homeserver_url, sliding_sync_version.into())
    }

    fn save_session(
        session_delegate: Arc<dyn ClientSessionDelegate>,
        client: matrix_sdk::Client,
    ) -> anyhow::Result<()> {
        let session = Self::session_inner(client)?;
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
    #[uniffi(default = None)]
    pub join_rule_override: Option<JoinRule>,
    #[uniffi(default = None)]
    pub canonical_alias: Option<String>,
}

impl TryFrom<CreateRoomParameters> for create_room::v3::Request {
    type Error = ClientError;

    fn try_from(value: CreateRoomParameters) -> Result<create_room::v3::Request, Self::Error> {
        let mut request = create_room::v3::Request::new();
        request.name = value.name;
        request.topic = value.topic;
        request.is_direct = value.is_direct;
        request.visibility = value.visibility.into();
        request.preset = Some(value.preset.into());
        request.room_alias_name = value.canonical_alias;
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

        if let Some(join_rule_override) = value.join_rule_override {
            let content = RoomJoinRulesEventContent::new(join_rule_override.try_into()?);
            initial_state.push(InitialStateEvent::new(content).to_raw_any());
        }

        request.initial_state = initial_state;

        if let Some(power_levels) = value.power_level_content_override {
            match Raw::new(&power_levels.into()) {
                Ok(power_levels) => {
                    request.power_level_content_override = Some(power_levels);
                }
                Err(e) => {
                    return Err(ClientError::Generic {
                        msg: format!("Failed to serialize power levels, error: {e}"),
                    })
                }
            }
        }

        Ok(request)
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
    /// The sliding sync version used for this session.
    pub sliding_sync_version: SlidingSyncVersion,
}

impl Session {
    fn new(
        auth_api: AuthApi,
        homeserver_url: String,
        sliding_sync_version: SlidingSyncVersion,
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
                    sliding_sync_version,
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
                    issuer,
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
                    issuer,
                };

                let oidc_data = serde_json::to_string(&oidc_data).ok();
                Ok(Session {
                    access_token,
                    refresh_token,
                    user_id: user_id.to_string(),
                    device_id: device_id.to_string(),
                    homeserver_url,
                    oidc_data,
                    sliding_sync_version,
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
            sliding_sync_version: _,
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
                issuer: oidc_data.issuer,
            };

            let session = OidcSession {
                credentials: ClientCredentials::None { client_id: oidc_data.client_id },
                metadata: oidc_data.client_metadata,
                user: user_session,
            };

            Ok(AuthSession::Oidc(session.into()))
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
    issuer: String,
}

/// Represents an unverified client registration against an OpenID Connect
/// authentication issuer. Call `validate` on this to use it for restoration.
#[derive(Deserialize)]
#[serde(try_from = "OidcUnvalidatedSessionDataDeHelper")]
pub(crate) struct OidcUnvalidatedSessionData {
    client_id: String,
    client_metadata: ClientMetadata,
    latest_id_token: Option<String>,
    issuer: String,
}

impl OidcUnvalidatedSessionData {
    /// Validates the data so that it can be used.
    fn validate(self) -> Result<OidcSessionData, ClientMetadataVerificationError> {
        Ok(OidcSessionData {
            client_id: self.client_id,
            client_metadata: self.client_metadata.validate()?,
            latest_id_token: self.latest_id_token,
            issuer: self.issuer,
        })
    }
}

#[derive(Deserialize)]
struct OidcUnvalidatedSessionDataDeHelper {
    client_id: String,
    client_metadata: ClientMetadata,
    latest_id_token: Option<String>,
    issuer_info: Option<AuthenticationServerInfo>,
    issuer: Option<String>,
}

impl TryFrom<OidcUnvalidatedSessionDataDeHelper> for OidcUnvalidatedSessionData {
    type Error = String;

    fn try_from(value: OidcUnvalidatedSessionDataDeHelper) -> Result<Self, Self::Error> {
        let OidcUnvalidatedSessionDataDeHelper {
            client_id,
            client_metadata,
            latest_id_token,
            issuer_info,
            issuer,
        } = value;

        let issuer = issuer
            .or(issuer_info.map(|info| info.issuer))
            .ok_or_else(|| "missing field `issuer`".to_owned())?;

        Ok(Self { client_id, client_metadata, latest_id_token, issuer })
    }
}

#[derive(uniffi::Enum)]
pub enum AccountManagementAction {
    Profile,
    SessionsList,
    SessionView { device_id: String },
    SessionEnd { device_id: String },
    AccountDeactivate,
    CrossSigningReset,
}

impl From<AccountManagementAction> for AccountManagementActionFull {
    fn from(value: AccountManagementAction) -> Self {
        match value {
            AccountManagementAction::Profile => Self::Profile,
            AccountManagementAction::SessionsList => Self::SessionsList,
            AccountManagementAction::SessionView { device_id } => Self::SessionView { device_id },
            AccountManagementAction::SessionEnd { device_id } => Self::SessionEnd { device_id },
            AccountManagementAction::AccountDeactivate => Self::AccountDeactivate,
            AccountManagementAction::CrossSigningReset => Self::CrossSigningReset,
        }
    }
}

#[matrix_sdk_ffi_macros::export]
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

#[matrix_sdk_ffi_macros::export]
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

#[derive(Clone, uniffi::Enum)]
pub enum SlidingSyncVersion {
    None,
    Proxy { url: String },
    Native,
}

impl From<SdkSlidingSyncVersion> for SlidingSyncVersion {
    fn from(value: SdkSlidingSyncVersion) -> Self {
        match value {
            SdkSlidingSyncVersion::None => Self::None,
            SdkSlidingSyncVersion::Proxy { url } => Self::Proxy { url: url.to_string() },
            SdkSlidingSyncVersion::Native => Self::Native,
        }
    }
}

impl TryFrom<SlidingSyncVersion> for SdkSlidingSyncVersion {
    type Error = ClientError;

    fn try_from(value: SlidingSyncVersion) -> Result<Self, Self::Error> {
        Ok(match value {
            SlidingSyncVersion::None => Self::None,
            SlidingSyncVersion::Proxy { url } => Self::Proxy {
                url: Url::parse(&url).map_err(|e| ClientError::Generic { msg: e.to_string() })?,
            },
            SlidingSyncVersion::Native => Self::Native,
        })
    }
}

#[derive(Clone, uniffi::Enum)]
pub enum OidcPrompt {
    /// The Authorization Server must not display any authentication or consent
    /// user interface pages.
    None,

    /// The Authorization Server should prompt the End-User for
    /// reauthentication.
    Login,

    /// The Authorization Server should prompt the End-User for consent before
    /// returning information to the Client.
    Consent,

    /// The Authorization Server should prompt the End-User to select a user
    /// account.
    ///
    /// This enables an End-User who has multiple accounts at the Authorization
    /// Server to select amongst the multiple accounts that they might have
    /// current sessions for.
    SelectAccount,

    /// The Authorization Server should prompt the End-User to create a user
    /// account.
    ///
    /// Defined in [Initiating User Registration via OpenID Connect](https://openid.net/specs/openid-connect-prompt-create-1_0.html).
    Create,

    /// An unknown value.
    Unknown { value: String },
}

impl From<&SdkOidcPrompt> for OidcPrompt {
    fn from(value: &SdkOidcPrompt) -> Self {
        match value {
            SdkOidcPrompt::None => Self::None,
            SdkOidcPrompt::Login => Self::Login,
            SdkOidcPrompt::Consent => Self::Consent,
            SdkOidcPrompt::SelectAccount => Self::SelectAccount,
            SdkOidcPrompt::Create => Self::Create,
            SdkOidcPrompt::Unknown(value) => Self::Unknown { value: value.to_owned() },
            _ => Self::Unknown { value: value.to_string() },
        }
    }
}

impl From<OidcPrompt> for SdkOidcPrompt {
    fn from(value: OidcPrompt) -> Self {
        match value {
            OidcPrompt::None => Self::None,
            OidcPrompt::Login => Self::Login,
            OidcPrompt::Consent => Self::Consent,
            OidcPrompt::SelectAccount => Self::SelectAccount,
            OidcPrompt::Create => Self::Create,
            OidcPrompt::Unknown { value } => Self::Unknown(value),
        }
    }
}

/// The rule used for users wishing to join this room.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum JoinRule {
    /// Anyone can join the room without any prior action.
    Public,

    /// A user who wishes to join the room must first receive an invite to the
    /// room from someone already inside of the room.
    Invite,

    /// Users can join the room if they are invited, or they can request an
    /// invite to the room.
    ///
    /// They can be allowed (invited) or denied (kicked/banned) access.
    Knock,

    /// Reserved but not yet implemented by the Matrix specification.
    Private,

    /// Users can join the room if they are invited, or if they meet any of the
    /// conditions described in a set of [`AllowRule`]s.
    Restricted { rules: Vec<AllowRule> },

    /// Users can join the room if they are invited, or if they meet any of the
    /// conditions described in a set of [`AllowRule`]s, or they can request
    /// an invite to the room.
    KnockRestricted { rules: Vec<AllowRule> },

    /// A custom join rule, up for interpretation by the consumer.
    Custom {
        /// The string representation for this custom rule.
        repr: String,
    },
}

/// An allow rule which defines a condition that allows joining a room.
#[derive(Debug, Clone, uniffi::Enum)]
pub enum AllowRule {
    /// Only a member of the `room_id` Room can join the one this rule is used
    /// in.
    RoomMembership { room_id: String },

    /// A custom allow rule implementation, containing its JSON representation
    /// as a `String`.
    Custom { json: String },
}

impl TryFrom<JoinRule> for RumaJoinRule {
    type Error = ClientError;

    fn try_from(value: JoinRule) -> Result<Self, Self::Error> {
        match value {
            JoinRule::Public => Ok(Self::Public),
            JoinRule::Invite => Ok(Self::Invite),
            JoinRule::Knock => Ok(Self::Knock),
            JoinRule::Private => Ok(Self::Private),
            JoinRule::Restricted { rules } => {
                let rules = ruma_allow_rules_from_ffi(rules)?;
                Ok(Self::Restricted(ruma::events::room::join_rules::Restricted::new(rules)))
            }
            JoinRule::KnockRestricted { rules } => {
                let rules = ruma_allow_rules_from_ffi(rules)?;
                Ok(Self::KnockRestricted(ruma::events::room::join_rules::Restricted::new(rules)))
            }
            JoinRule::Custom { repr } => Ok(serde_json::from_str(&repr)?),
        }
    }
}

fn ruma_allow_rules_from_ffi(value: Vec<AllowRule>) -> Result<Vec<RumaAllowRule>, ClientError> {
    let mut ret = Vec::with_capacity(value.len());
    for rule in value {
        let rule: Result<RumaAllowRule, ClientError> = rule.try_into();
        match rule {
            Ok(rule) => ret.push(rule),
            Err(error) => return Err(error),
        }
    }
    Ok(ret)
}

impl TryFrom<AllowRule> for RumaAllowRule {
    type Error = ClientError;

    fn try_from(value: AllowRule) -> Result<Self, Self::Error> {
        match value {
            AllowRule::RoomMembership { room_id } => {
                let room_id = RoomId::parse(room_id)?;
                Ok(Self::RoomMembership(ruma::events::room::join_rules::RoomMembership::new(
                    room_id,
                )))
            }
            AllowRule::Custom { json } => Ok(Self::_Custom(Box::new(serde_json::from_str(&json)?))),
        }
    }
}

impl TryFrom<RumaJoinRule> for JoinRule {
    type Error = String;
    fn try_from(value: RumaJoinRule) -> Result<Self, Self::Error> {
        match value {
            RumaJoinRule::Knock => Ok(JoinRule::Knock),
            RumaJoinRule::Public => Ok(JoinRule::Public),
            RumaJoinRule::Private => Ok(JoinRule::Private),
            RumaJoinRule::KnockRestricted(restricted) => {
                let rules = restricted.allow.into_iter().map(TryInto::try_into).collect::<Result<
                    Vec<_>,
                    Self::Error,
                >>(
                )?;
                Ok(JoinRule::KnockRestricted { rules })
            }
            RumaJoinRule::Restricted(restricted) => {
                let rules = restricted.allow.into_iter().map(TryInto::try_into).collect::<Result<
                    Vec<_>,
                    Self::Error,
                >>(
                )?;
                Ok(JoinRule::Restricted { rules })
            }
            RumaJoinRule::Invite => Ok(JoinRule::Invite),
            RumaJoinRule::_Custom(_) => Ok(JoinRule::Custom { repr: value.as_str().to_owned() }),
            _ => Err(format!("Unknown JoinRule: {:?}", value)),
        }
    }
}

impl TryFrom<RumaAllowRule> for AllowRule {
    type Error = String;
    fn try_from(value: RumaAllowRule) -> Result<Self, Self::Error> {
        match value {
            RumaAllowRule::RoomMembership(membership) => {
                Ok(AllowRule::RoomMembership { room_id: membership.room_id.to_string() })
            }
            RumaAllowRule::_Custom(repr) => {
                let json = serde_json::to_string(&repr)
                    .map_err(|e| format!("Couldn't serialize custom AllowRule: {e:?}"))?;
                Ok(Self::Custom { json })
            }
            _ => Err(format!("Invalid AllowRule: {:?}", value)),
        }
    }
}
