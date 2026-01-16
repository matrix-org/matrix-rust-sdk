use std::{
    collections::HashMap,
    fmt::Debug,
    path::PathBuf,
    sync::{Arc, OnceLock},
    time::Duration,
};

use anyhow::{anyhow, Context as _};
use futures_util::pin_mut;
#[cfg(not(target_family = "wasm"))]
use matrix_sdk::media::MediaFileHandle as SdkMediaFileHandle;
#[cfg(feature = "sqlite")]
use matrix_sdk::STATE_STORE_DATABASE_NAME;
use matrix_sdk::{
    authentication::oauth::{
        AccountManagementActionFull, ClientId, OAuthAuthorizationData, OAuthSession,
    },
    deserialized_responses::RawAnySyncOrStrippedTimelineEvent,
    media::{MediaFormat, MediaRequestParameters, MediaRetentionPolicy, MediaThumbnailSettings},
    ruma::{
        api::client::{
            discovery::{
                discover_homeserver::RtcFocusInfo,
                get_authorization_server_metadata::v1::Prompt as RumaOidcPrompt,
            },
            push::{EmailPusherData, PusherIds, PusherInit, PusherKind as RumaPusherKind},
            room::{create_room, Visibility},
            session::get_login_types,
            user_directory::search_users,
        },
        events::{
            room::{
                avatar::RoomAvatarEventContent, encryption::RoomEncryptionEventContent,
                message::MessageType,
            },
            AnyInitialStateEvent, InitialStateEvent,
        },
        serde::Raw,
        EventEncryptionAlgorithm, RoomId, TransactionId, UInt, UserId,
    },
    sliding_sync::Version as SdkSlidingSyncVersion,
    store::RoomLoadSettings as SdkRoomLoadSettings,
    Account, AuthApi, AuthSession, Client as MatrixClient, Error, SessionChange, SessionTokens,
};
use matrix_sdk_common::{stream::StreamExt, SendOutsideWasm, SyncOutsideWasm};
use matrix_sdk_ui::{
    notification_client::{
        NotificationClient as MatrixNotificationClient,
        NotificationProcessSetup as MatrixNotificationProcessSetup,
    },
    spaces::SpaceService as UISpaceService,
    unable_to_decrypt_hook::UtdHookManager,
};
use mime::Mime;
use oauth2::Scope;
use ruma::{
    api::client::{
        alias::get_alias,
        error::ErrorKind,
        profile::{AvatarUrl, DisplayName},
        room::create_room::{v3::CreationContent, RoomPowerLevelsContentOverride},
        uiaa::UserIdentifier,
    },
    events::{
        direct::DirectEventContent,
        fully_read::FullyReadEventContent,
        identity_server::IdentityServerEventContent,
        ignored_user_list::IgnoredUserListEventContent,
        key::verification::request::ToDeviceKeyVerificationRequestEvent,
        marked_unread::{MarkedUnreadEventContent, UnstableMarkedUnreadEventContent},
        push_rules::PushRulesEventContent,
        room::{
            history_visibility::RoomHistoryVisibilityEventContent,
            join_rules::{
                AllowRule as RumaAllowRule, JoinRule as RumaJoinRule, RoomJoinRulesEventContent,
            },
            message::{OriginalSyncRoomMessageEvent, Relation},
        },
        secret_storage::{
            default_key::SecretStorageDefaultKeyEventContent, key::SecretStorageKeyEventContent,
        },
        tag::TagEventContent,
        AnyMessageLikeEventContent, AnySyncTimelineEvent,
        GlobalAccountDataEvent as RumaGlobalAccountDataEvent,
        RoomAccountDataEvent as RumaRoomAccountDataEvent,
    },
    push::{HttpPusherData as RumaHttpPusherData, PushFormat as RumaPushFormat},
    room::RoomType,
    OwnedDeviceId, OwnedServerName, RoomAliasId, RoomOrAliasId, ServerName,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, error};
use url::Url;

use super::{
    room::{room_info::RoomInfo, Room},
    session_verification::SessionVerificationController,
};
use crate::{
    authentication::{HomeserverLoginDetails, OidcConfiguration, OidcError, SsoError, SsoHandler},
    client,
    encryption::Encryption,
    notification::{
        NotificationClient, NotificationEvent, NotificationItem, NotificationRoomInfo,
        NotificationSenderInfo,
    },
    notification_settings::NotificationSettings,
    qr_code::{GrantLoginWithQrCodeHandler, LoginWithQrCodeHandler},
    room::{RoomHistoryVisibility, RoomInfoListener, RoomSendQueueUpdate},
    room_directory_search::RoomDirectorySearch,
    room_preview::RoomPreview,
    ruma::{
        AccountDataEvent, AccountDataEventType, AuthData, InviteAvatars, MediaPreviewConfig,
        MediaPreviews, MediaSource, RoomAccountDataEvent, RoomAccountDataEventType,
    },
    runtime::get_runtime_handle,
    spaces::SpaceService,
    sync_service::{SyncService, SyncServiceBuilder},
    task_handle::TaskHandle,
    utd::{UnableToDecryptDelegate, UtdHook},
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
pub trait ClientDelegate: SyncOutsideWasm + SendOutsideWasm {
    fn did_receive_auth_error(&self, is_soft_logout: bool);
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait ClientSessionDelegate: SyncOutsideWasm + SendOutsideWasm {
    fn retrieve_session_from_keychain(&self, user_id: String) -> Result<Session, ClientError>;
    fn save_session_in_keychain(&self, session: Session);
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait ProgressWatcher: SyncOutsideWasm + SendOutsideWasm {
    fn transmission_progress(&self, progress: TransmissionProgress);
}

/// A listener to the global (client-wide) update reporter of the send queue.
#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait SendQueueRoomUpdateListener: SyncOutsideWasm + SendOutsideWasm {
    /// Called every time the send queue emits an update for a given room.
    fn on_update(&self, room_id: String, update: RoomSendQueueUpdate);
}

/// A listener to the global (client-wide) error reporter of the send queue.
#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait SendQueueRoomErrorListener: SyncOutsideWasm + SendOutsideWasm {
    /// Called every time the send queue has ran into an error for a given room,
    /// which will disable the send queue for that particular room.
    fn on_error(&self, room_id: String, error: ClientError);
}

/// A listener for changes of global account data events.
#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait AccountDataListener: SyncOutsideWasm + SendOutsideWasm {
    /// Called when a global account data event has changed.
    fn on_change(&self, event: AccountDataEvent);
}

/// A listener for changes of room account data events.
#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait RoomAccountDataListener: SyncOutsideWasm + SendOutsideWasm {
    /// Called when a room account data event was changed.
    fn on_change(&self, event: RoomAccountDataEvent, room_id: String);
}

/// A listener for notifications generated from sync responses.
///
/// This is called during sync for each event that triggers a notification
/// based on the user's push rules.
#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait SyncNotificationListener: SyncOutsideWasm + SendOutsideWasm {
    /// Called when a notifying event is received during sync.
    fn on_notification(&self, notification: NotificationItem, room_id: String);
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

    delegate: OnceLock<Arc<dyn ClientDelegate>>,

    pub(crate) utd_hook_manager: OnceLock<Arc<UtdHookManager>>,

    session_verification_controller:
        Arc<tokio::sync::RwLock<Option<SessionVerificationController>>>,

    /// The path to the directory where the state store and the crypto store are
    /// located, if the `Client` instance has been built with a store (either
    /// SQLite or IndexedDB).
    #[cfg_attr(not(feature = "sqlite"), allow(unused))]
    store_path: Option<PathBuf>,
}

impl Client {
    pub async fn new(
        sdk_client: MatrixClient,
        enable_oidc_refresh_lock: bool,
        session_delegate: Option<Arc<dyn ClientSessionDelegate>>,
        store_path: Option<PathBuf>,
    ) -> Result<Self, ClientError> {
        let session_verification_controller: Arc<
            tokio::sync::RwLock<Option<SessionVerificationController>>,
        > = Default::default();
        let controller = session_verification_controller.clone();
        sdk_client.add_event_handler(
            move |event: ToDeviceKeyVerificationRequestEvent| async move {
                if let Some(session_verification_controller) = &*controller.clone().read().await {
                    session_verification_controller
                        .process_incoming_verification_request(
                            &event.sender,
                            event.content.transaction_id,
                        )
                        .await;
                }
            },
        );

        let controller = session_verification_controller.clone();
        sdk_client.add_event_handler(move |event: OriginalSyncRoomMessageEvent| async move {
            if let MessageType::VerificationRequest(_) = &event.content.msgtype {
                if let Some(session_verification_controller) = &*controller.clone().read().await {
                    session_verification_controller
                        .process_incoming_verification_request(&event.sender, event.event_id)
                        .await;
                }
            }
        });

        let cross_process_store_locks_holder_name =
            sdk_client.cross_process_store_locks_holder_name().to_owned();

        let client = Client {
            inner: AsyncRuntimeDropped::new(sdk_client.clone()),
            delegate: OnceLock::new(),
            utd_hook_manager: OnceLock::new(),
            session_verification_controller,
            store_path,
        };

        if enable_oidc_refresh_lock {
            if session_delegate.is_none() {
                return Err(anyhow::anyhow!(
                    "missing session delegates when enabling the cross-process lock"
                ))?;
            }

            client
                .inner
                .oauth()
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
    /// Perform database optimizations if any are available, i.e. vacuuming in
    /// SQLite.
    pub async fn optimize_stores(&self) -> Result<(), ClientError> {
        Ok(self.inner.optimize_stores().await?)
    }

    /// Returns the sizes of the existing stores, if known.
    pub async fn get_store_sizes(&self) -> Result<StoreSizes, ClientError> {
        Ok(self.inner.get_store_sizes().await?.into())
    }

    /// Information about login options for the client's homeserver.
    pub async fn homeserver_login_details(&self) -> Arc<HomeserverLoginDetails> {
        let oauth = self.inner.oauth();
        let (supports_oidc_login, supported_oidc_prompts) = match oauth.server_metadata().await {
            Ok(metadata) => {
                let prompts =
                    metadata.prompt_values_supported.into_iter().map(Into::into).collect();

                (true, prompts)
            }
            Err(error) => {
                error!("Failed to fetch OIDC provider metadata: {error}");
                (false, Default::default())
            }
        };

        let login_types = self.inner.matrix_auth().get_login_types().await.ok();
        let supports_password_login = login_types
            .as_ref()
            .map(|login_types| {
                login_types.flows.iter().any(|login_type| {
                    matches!(login_type, get_login_types::v3::LoginType::Password(_))
                })
            })
            .unwrap_or(false);
        let supports_sso_login = login_types
            .as_ref()
            .map(|login_types| {
                login_types
                    .flows
                    .iter()
                    .any(|login_type| matches!(login_type, get_login_types::v3::LoginType::Sso(_)))
            })
            .unwrap_or(false);
        let sliding_sync_version = self.sliding_sync_version();

        Arc::new(HomeserverLoginDetails {
            url: self.homeserver(),
            sliding_sync_version,
            supports_oidc_login,
            supported_oidc_prompts,
            supports_sso_login,
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
    ///
    /// # Arguments
    ///
    /// * `oidc_configuration` - The configuration used to load the credentials
    ///   of the client if it is already registered with the authorization
    ///   server, or register the client and store its credentials if it isn't.
    ///
    /// * `prompt` - The desired user experience in the web UI. No value means
    ///   that the user wishes to login into an existing account, and a value of
    ///   `Create` means that the user wishes to register a new account.
    ///
    /// * `login_hint` - A generic login hint that an identity provider can use
    ///   to pre-fill the login form. The format of this hint is not restricted
    ///   by the spec as external providers all have their own way to handle the hint.
    ///   However, it should be noted that when providing a user ID as a hint
    ///   for MAS (with no upstream provider), then the format to use is defined
    ///   by [MSC4198]: https://github.com/matrix-org/matrix-spec-proposals/pull/4198
    ///
    /// * `device_id` - The unique ID that will be associated with the session.
    ///   If not set, a random one will be generated. It can be an existing
    ///   device ID from a previous login call. Note that this should be done
    ///   only if the client also holds the corresponding encryption keys.
    ///
    /// * `additional_scopes` - Additional scopes to request from the
    ///   authorization server, e.g. "urn:matrix:client:com.example.msc9999.foo".
    ///   The scopes for API access and the device ID according to the
    ///   [specification](https://spec.matrix.org/v1.15/client-server-api/#allocated-scope-tokens)
    ///   are always requested.
    pub async fn url_for_oidc(
        &self,
        oidc_configuration: &OidcConfiguration,
        prompt: Option<OidcPrompt>,
        login_hint: Option<String>,
        device_id: Option<String>,
        additional_scopes: Option<Vec<String>>,
    ) -> Result<Arc<OAuthAuthorizationData>, OidcError> {
        let registration_data = oidc_configuration.registration_data()?;
        let redirect_uri = oidc_configuration.redirect_uri()?;

        let device_id = device_id.map(OwnedDeviceId::from);

        let additional_scopes =
            additional_scopes.map(|scopes| scopes.into_iter().map(Scope::new).collect::<Vec<_>>());

        let mut url_builder = self.inner.oauth().login(
            redirect_uri,
            device_id,
            Some(registration_data),
            additional_scopes,
        );

        if let Some(prompt) = prompt {
            url_builder = url_builder.prompt(vec![prompt.into()]);
        }
        if let Some(login_hint) = login_hint {
            url_builder = url_builder.login_hint(login_hint);
        }

        let data = url_builder.build().await?;

        Ok(Arc::new(data))
    }

    /// Aborts an existing OIDC login operation that might have been cancelled,
    /// failed etc.
    pub async fn abort_oidc_auth(&self, authorization_data: Arc<OAuthAuthorizationData>) {
        self.inner.oauth().abort_login(&authorization_data.state).await;
    }

    /// Completes the OIDC login process.
    pub async fn login_with_oidc_callback(&self, callback_url: String) -> Result<(), OidcError> {
        let url = Url::parse(&callback_url).or(Err(OidcError::CallbackUrlInvalid))?;

        self.inner.oauth().finish_login(url.into()).await?;

        Ok(())
    }

    /// Create a handler for requesting an existing device to grant login to
    /// this device by way of a QR code.
    ///
    /// # Arguments
    ///
    /// * `oidc_configuration` - The data to restore or register the client with
    ///   the server.
    pub fn new_login_with_qr_code_handler(
        self: Arc<Self>,
        oidc_configuration: OidcConfiguration,
    ) -> LoginWithQrCodeHandler {
        LoginWithQrCodeHandler::new(self.inner.oauth(), oidc_configuration)
    }

    /// Create a handler for granting login from this device to a new device by
    /// way of a QR code.
    pub fn new_grant_login_with_qr_code_handler(self: Arc<Self>) -> GrantLoginWithQrCodeHandler {
        GrantLoginWithQrCodeHandler::new(self.inner.oauth())
    }

    /// Restores the client from a `Session`.
    ///
    /// It reloads the entire set of rooms from the previous session.
    ///
    /// If you want to control the amount of rooms to reloads, check
    /// [`Client::restore_session_with`].
    pub async fn restore_session(&self, session: Session) -> Result<(), ClientError> {
        self.restore_session_with(session, RoomLoadSettings::All).await
    }

    /// Restores the client from a `Session`.
    ///
    /// It reloads a set of rooms controlled by [`RoomLoadSettings`].
    pub async fn restore_session_with(
        &self,
        session: Session,
        room_load_settings: RoomLoadSettings,
    ) -> Result<(), ClientError> {
        let sliding_sync_version = session.sliding_sync_version.clone();
        let auth_session: AuthSession = session.try_into()?;

        self.inner
            .restore_session_with(
                auth_session,
                room_load_settings
                    .try_into()
                    .map_err(|error| ClientError::from_str(error, None))?,
            )
            .await?;
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

    /// Enables or disables progress reporting for media uploads in the send
    /// queue.
    pub fn enable_send_queue_upload_progress(&self, enable: bool) {
        self.inner.send_queue().enable_upload_progress(enable);
    }

    /// Subscribe to the global send queue update reporter, at the
    /// client-wide level.
    ///
    /// The given listener will be immediately called with
    /// `RoomSendQueueUpdate::NewLocalEvent` for each local echo existing in
    /// the queue.
    pub async fn subscribe_to_send_queue_updates(
        &self,
        listener: Box<dyn SendQueueRoomUpdateListener>,
    ) -> Result<Arc<TaskHandle>, ClientError> {
        let q = self.inner.send_queue();
        let local_echoes = q.local_echoes().await?;
        let mut subscriber = q.subscribe();

        for (room_id, local_echoes) in local_echoes {
            for local_echo in local_echoes {
                listener.on_update(
                    room_id.clone().into(),
                    RoomSendQueueUpdate::NewLocalEvent {
                        transaction_id: local_echo.transaction_id.into(),
                    },
                );
            }
        }

        Ok(Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            loop {
                match subscriber.recv().await {
                    Ok(update) => {
                        let room_id = update.room_id.to_string();
                        match update.update.try_into() {
                            Ok(update) => listener.on_update(room_id, update),
                            Err(err) => error!("error when converting send queue update: {err}"),
                        }
                    }
                    Err(err) => {
                        error!("error when listening to the send queue update reporter: {err}");
                    }
                }
            }
        }))))
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

        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            // Respawn tasks for rooms that had unsent events. At this point we've just
            // created the subscriber, so it'll be notified about errors.
            q.respawn_tasks_for_rooms_with_unsent_requests().await;

            loop {
                match subscriber.recv().await {
                    Ok(report) => listener
                        .on_error(report.room_id.to_string(), ClientError::from_err(report.error)),
                    Err(err) => {
                        error!("error when listening to the send queue error reporter: {err}");
                    }
                }
            }
        })))
    }

    /// Subscribe to updates of global account data events.
    ///
    /// Be careful that only the most recent value can be observed. Subscribers
    /// are notified when a new value is sent, but there is no guarantee that
    /// they will see all values.
    pub fn observe_account_data_event(
        &self,
        event_type: AccountDataEventType,
        listener: Box<dyn AccountDataListener>,
    ) -> Arc<TaskHandle> {
        macro_rules! observe {
            ($t:ty, $cb: expr) => {{
                // Using an Arc here is mandatory or else the subscriber will never trigger
                let observer =
                    Arc::new(self.inner.observe_events::<RumaGlobalAccountDataEvent<$t>, ()>());

                Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
                    let mut subscriber = observer.subscribe();
                    loop {
                        if let Some(next) = subscriber.next().await {
                            $cb(next.0);
                        }
                    }
                })))
            }};

            ($t:ty) => {{
                observe!($t, |event: RumaGlobalAccountDataEvent<$t>| {
                    listener.on_change(event.into());
                })
            }};
        }

        match event_type {
            AccountDataEventType::Direct => {
                observe!(DirectEventContent)
            }
            AccountDataEventType::IdentityServer => {
                observe!(IdentityServerEventContent)
            }
            AccountDataEventType::IgnoredUserList => {
                observe!(IgnoredUserListEventContent)
            }
            AccountDataEventType::PushRules => {
                observe!(PushRulesEventContent, |event: RumaGlobalAccountDataEvent<
                    PushRulesEventContent,
                >| {
                    if let Ok(event) = event.try_into() {
                        listener.on_change(event);
                    }
                })
            }
            AccountDataEventType::SecretStorageDefaultKey => {
                observe!(SecretStorageDefaultKeyEventContent)
            }
            AccountDataEventType::SecretStorageKey { key_id } => {
                observe!(SecretStorageKeyEventContent, |event: RumaGlobalAccountDataEvent<
                    SecretStorageKeyEventContent,
                >| {
                    if event.content.key_id != key_id {
                        return;
                    }
                    if let Ok(event) = event.try_into() {
                        listener.on_change(event);
                    }
                })
            }
        }
    }

    /// Subscribe to updates of room account data events.
    ///
    /// Be careful that only the most recent value can be observed. Subscribers
    /// are notified when a new value is sent, but there is no guarantee that
    /// they will see all values.
    pub fn observe_room_account_data_event(
        &self,
        room_id: String,
        event_type: RoomAccountDataEventType,
        listener: Box<dyn RoomAccountDataListener>,
    ) -> Result<Arc<TaskHandle>, ClientError> {
        macro_rules! observe {
            ($t:ty, $cb: expr) => {{
                // Using an Arc here is mandatory or else the subscriber will never trigger
                let observer =
                    Arc::new(self.inner.observe_room_events::<RumaRoomAccountDataEvent<$t>, ()>(
                        &RoomId::parse(&room_id)?,
                    ));

                Ok(Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
                    let mut subscriber = observer.subscribe();
                    loop {
                        if let Some(next) = subscriber.next().await {
                            $cb(next.0);
                        }
                    }
                }))))
            }};

            ($t:ty) => {{
                observe!($t, |event: RumaRoomAccountDataEvent<$t>| {
                    listener.on_change(event.into(), room_id.clone());
                })
            }};
        }

        match event_type {
            RoomAccountDataEventType::FullyRead => {
                observe!(FullyReadEventContent)
            }
            RoomAccountDataEventType::MarkedUnread => {
                observe!(MarkedUnreadEventContent)
            }
            RoomAccountDataEventType::Tag => {
                observe!(TagEventContent, |event: RumaRoomAccountDataEvent<TagEventContent>| {
                    if let Ok(event) = event.try_into() {
                        listener.on_change(event, room_id.clone());
                    }
                })
            }
            RoomAccountDataEventType::UnstableMarkedUnread => {
                observe!(UnstableMarkedUnreadEventContent)
            }
        }
    }

    /// Register a handler for notifications generated from sync responses.
    ///
    /// The handler will be called during sync for each event that triggers
    /// a notification based on the user's push rules.
    ///
    /// The handler receives:
    /// - The notification with push actions and event data
    /// - The room ID where the notification occurred
    ///
    /// This is useful for implementing custom notification logic, such as
    /// displaying local notifications or updating notification badges.
    pub async fn register_notification_handler(&self, listener: Box<dyn SyncNotificationListener>) {
        let listener = Arc::new(listener);
        self.inner
            .register_notification_handler(move |notification, room, _client| {
                let listener = listener.clone();
                let room_id = room.room_id().to_string();

                async move {
                    // Extract information about the actions
                    let is_noisy = notification.actions.iter().any(|a| a.sound().is_some());
                    let has_mention = notification.actions.iter().any(|a| a.is_highlight());

                    // Convert SDK actions to FFI type
                    let actions: Vec<crate::notification_settings::Action> = notification
                        .actions
                        .into_iter()
                        .filter_map(|action| action.try_into().ok())
                        .collect();

                    // Convert SDK event to FFI type
                    let (sender, event, thread_id) = match notification.event {
                        RawAnySyncOrStrippedTimelineEvent::Sync(raw) => match raw.deserialize() {
                            Ok(deserialized) => {
                                let sender = deserialized.sender().to_owned();
                                let thread_id = match &deserialized {
                                    AnySyncTimelineEvent::MessageLike(event) => {
                                        match event.original_content() {
                                            Some(AnyMessageLikeEventContent::RoomMessage(
                                                content,
                                            )) => match content.relates_to {
                                                Some(Relation::Thread(thread)) => {
                                                    Some(thread.event_id.to_string())
                                                }
                                                _ => None,
                                            },
                                            _ => None,
                                        }
                                    }
                                    _ => None,
                                };
                                let event = NotificationEvent::Timeline {
                                    event: Arc::new(crate::event::TimelineEvent(Box::new(
                                        deserialized,
                                    ))),
                                };
                                (sender, event, thread_id)
                            }
                            Err(err) => {
                                tracing::warn!("Failed to deserialize timeline event: {err}");
                                return;
                            }
                        },
                        RawAnySyncOrStrippedTimelineEvent::Stripped(raw) => {
                            match raw.deserialize() {
                                Ok(deserialized) => {
                                    let sender = deserialized.sender().to_owned();
                                    let event =
                                        NotificationEvent::Invite { sender: sender.to_string() };
                                    let thread_id = None;
                                    (sender, event, thread_id)
                                }
                                Err(err) => {
                                    tracing::warn!(
                                        "Failed to deserialize stripped state event: {err}"
                                    );
                                    return;
                                }
                            }
                        }
                    };

                    // Compile sender info
                    let sender = room.get_member_no_sync(&sender).await.ok().flatten();
                    let sender_info = if let Some(sender) = sender.as_ref() {
                        NotificationSenderInfo {
                            display_name: sender.display_name().map(|name| name.to_owned()),
                            avatar_url: sender.avatar_url().map(|uri| uri.to_string()),
                            is_name_ambiguous: sender.name_ambiguous(),
                        }
                    } else {
                        NotificationSenderInfo {
                            display_name: None,
                            avatar_url: None,
                            is_name_ambiguous: false,
                        }
                    };

                    // Compile room info
                    let display_name = match room.display_name().await {
                        Ok(name) => name.to_string(),
                        Err(err) => {
                            tracing::warn!("Failed to calculate the room's display name: {err}");
                            return;
                        }
                    };
                    let is_direct = match room.is_direct().await {
                        Ok(is_direct) => is_direct,
                        Err(err) => {
                            tracing::warn!("Failed to determine if room is direct or not: {err}");
                            return;
                        }
                    };
                    let room_info = NotificationRoomInfo {
                        display_name,
                        avatar_url: room.avatar_url().map(Into::into),
                        canonical_alias: room.canonical_alias().map(Into::into),
                        topic: room.topic(),
                        join_rule: room
                            .join_rule()
                            .map(TryInto::try_into)
                            .transpose()
                            .ok()
                            .flatten(),
                        joined_members_count: room.joined_members_count(),
                        is_encrypted: Some(room.encryption_state().is_encrypted()),
                        is_direct,
                        is_space: room.is_space(),
                    };

                    listener.on_notification(
                        NotificationItem {
                            event,
                            sender_info,
                            room_info,
                            is_noisy: Some(is_noisy),
                            has_mention: Some(has_mention),
                            thread_id,
                            actions: Some(actions),
                        },
                        room_id,
                    );
                }
            })
            .await;
    }

    /// Allows generic GET requests to be made through the SDK's internal HTTP
    /// client. This is useful when the caller's native HTTP client wouldn't
    /// have the same configuration (such as certificates, proxies, etc.) This
    /// method returns the raw bytes of the response, so that any kind of
    /// resource can be fetched including images, files, etc.
    ///
    /// Note: When an HTTP error occurs, the error response can be found in the
    /// `ClientError::Generic`'s `details` field.
    pub async fn get_url(&self, url: String) -> Result<Vec<u8>, ClientError> {
        let response = self.inner.http_client().get(url).send().await?;
        if response.status().is_success() {
            Ok(response.bytes().await?.into())
        } else {
            Err(ClientError::Generic {
                msg: response.status().to_string(),
                details: response.text().await.ok(),
            })
        }
    }

    /// Empty the server version and unstable features cache.
    ///
    /// Since the SDK caches the supported versions, it's possible to have a
    /// stale entry in the cache. This functions makes it possible to force
    /// reset it.
    pub async fn reset_supported_versions(&self) -> Result<(), ClientError> {
        Ok(self.inner.reset_supported_versions().await?)
    }

    /// Empty the well-known cache.
    ///
    /// Since the SDK caches the well-known, it's possible to have a stale
    /// entry in the cache. This functions makes it possible to force reset
    /// it.
    pub async fn reset_well_known(&self) -> Result<(), ClientError> {
        Ok(self.inner.reset_well_known().await?)
    }
}

#[matrix_sdk_ffi_macros::export]
impl Client {
    /// Retrieves a media file from the media source
    ///
    /// Not available on Wasm platforms, due to lack of accessible file system.
    pub async fn get_media_file(
        &self,
        media_source: Arc<MediaSource>,
        filename: Option<String>,
        mime_type: String,
        use_cache: bool,
        temp_dir: Option<String>,
    ) -> Result<Arc<MediaFileHandle>, ClientError> {
        #[cfg(not(target_family = "wasm"))]
        {
            let source = (*media_source).clone();
            let mime_type: mime::Mime = mime_type.parse()?;

            let handle = self
                .inner
                .media()
                .get_media_file(
                    &MediaRequestParameters {
                        source: source.media_source,
                        format: MediaFormat::File,
                    },
                    filename,
                    &mime_type,
                    use_cache,
                    temp_dir,
                )
                .await?;

            Ok(Arc::new(MediaFileHandle::new(handle)))
        }

        /// MediaFileHandle uses SdkMediaFileHandle which requires an
        /// intermediate TempFile which is not available on wasm
        /// platforms due to lack of an accessible file system.
        #[cfg(target_family = "wasm")]
        Err(ClientError::Generic {
            msg: "get_media_file is not supported on wasm platforms".to_owned(),
            details: None,
        })
    }

    pub async fn set_display_name(&self, name: String) -> Result<(), ClientError> {
        #[cfg(not(target_family = "wasm"))]
        {
            self.inner
                .account()
                .set_display_name(Some(name.as_str()))
                .await
                .context("Unable to set display name")?;
        }

        #[cfg(target_family = "wasm")]
        {
            self.inner.account().set_display_name(Some(name.as_str())).await.map_err(|e| {
                ClientError::Generic {
                    msg: "Unable to set display name".to_owned(),
                    details: Some(e.to_string()),
                }
            })?;
        }

        Ok(())
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

    /// Sets the [ClientDelegate] which will inform about authentication errors.
    /// Returns an error if the delegate was already set.
    pub fn set_delegate(
        self: Arc<Self>,
        delegate: Option<Box<dyn ClientDelegate>>,
    ) -> Result<Option<Arc<TaskHandle>>, ClientError> {
        if self.delegate.get().is_some() {
            return Err(ClientError::Generic {
                msg: "Delegate already initialized".to_owned(),
                details: None,
            });
        }

        Ok(delegate.map(|delegate| {
            let mut session_change_receiver = self.inner.subscribe_to_session_changes();
            let client_clone = self.clone();
            let session_change_task = get_runtime_handle().spawn(async move {
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

            self.delegate.get_or_init(|| Arc::from(delegate));

            Arc::new(TaskHandle::new(session_change_task))
        }))
    }

    /// Sets the [UnableToDecryptDelegate] which will inform about UTDs.
    /// Returns an error if the delegate was already set.
    pub async fn set_utd_delegate(
        self: Arc<Self>,
        utd_delegate: Box<dyn UnableToDecryptDelegate>,
    ) -> Result<(), ClientError> {
        if self.utd_hook_manager.get().is_some() {
            return Err(ClientError::Generic {
                msg: "UTD delegate already initialized".to_owned(),
                details: None,
            });
        }

        // UTDs detected before this duration may be reclassified as "late decryption"
        // events (or discarded, if they get decrypted fast enough).
        const UTD_HOOK_GRACE_PERIOD: Duration = Duration::from_secs(60);

        let mut utd_hook_manager = UtdHookManager::new(
            Arc::new(UtdHook { delegate: utd_delegate.into() }),
            (*self.inner).clone(),
        )
        .with_max_delay(UTD_HOOK_GRACE_PERIOD);

        if let Err(e) = utd_hook_manager.reload_from_store().await {
            error!("Unable to reload UTD hook data from data store: {e}");
            // Carry on with the setup anyway; we shouldn't fail setup just
            // because the UTD hook failed to load its data.
        }

        self.utd_hook_manager.get_or_init(|| Arc::new(utd_hook_manager));

        Ok(())
    }

    pub fn session(&self) -> Result<Session, ClientError> {
        Self::session_inner((*self.inner).clone())
    }

    pub async fn account_url(
        &self,
        action: Option<AccountManagementAction>,
    ) -> Result<Option<String>, ClientError> {
        if !matches!(self.inner.auth_api(), Some(AuthApi::OAuth(..))) {
            return Ok(None);
        }

        let mut url_builder = match self.inner.oauth().account_management_url().await {
            Ok(Some(url_builder)) => url_builder,
            Ok(None) => return Ok(None),
            Err(e) => {
                error!("Failed retrieving account management URL: {e}");
                return Err(e.into());
            }
        };

        if let Some(action) = action {
            url_builder = url_builder.action(action.into());
        }

        Ok(Some(url_builder.build().to_string()))
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
    pub async fn cached_avatar_url(&self) -> Result<Option<String>, ClientError> {
        Ok(self.inner.account().get_cached_avatar_url().await?.map(Into::into))
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
            get_runtime_handle().spawn(async move {
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

        let session_verification_controller = SessionVerificationController::new(
            self.inner.encryption(),
            user_identity,
            self.inner.account(),
        );

        *self.session_verification_controller.write().await =
            Some(session_verification_controller.clone());

        Ok(Arc::new(session_verification_controller))
    }

    /// Log the current user out.
    pub async fn logout(&self) -> Result<(), ClientError> {
        Ok(self.inner.logout().await?)
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
        self.inner
            .rooms()
            .into_iter()
            .map(|room| Arc::new(Room::new(room, self.utd_hook_manager.get().cloned())))
            .collect()
    }

    /// Get a room by its ID.
    ///
    /// # Arguments
    ///
    /// * `room_id` - The ID of the room to get.
    ///
    /// # Returns
    ///
    /// A `Result` containing an optional room, or a `ClientError`.
    /// This method will not initialize the room's timeline or populate it with
    /// events.
    pub fn get_room(&self, room_id: String) -> Result<Option<Arc<Room>>, ClientError> {
        let room_id = RoomId::parse(room_id)?;
        let sdk_room = self.inner.get_room(&room_id);

        let room =
            sdk_room.map(|room| Arc::new(Room::new(room, self.utd_hook_manager.get().cloned())));
        Ok(room)
    }

    pub fn get_dm_room(&self, user_id: String) -> Result<Option<Arc<Room>>, ClientError> {
        let user_id = UserId::parse(user_id)?;
        let sdk_room = self.inner.get_dm_room(&user_id);
        let dm =
            sdk_room.map(|room| Arc::new(Room::new(room, self.utd_hook_manager.get().cloned())));
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
        let user_id = <&UserId>::try_from(user_id.as_str())?;
        UserProfile::fetch(&self.inner.account(), user_id).await
    }

    pub async fn notification_client(
        self: Arc<Self>,
        process_setup: NotificationProcessSetup,
    ) -> Result<Arc<NotificationClient>, ClientError> {
        Ok(Arc::new(NotificationClient {
            inner: MatrixNotificationClient::new((*self.inner).clone(), process_setup.into())
                .await?,
            client: self.clone(),
        }))
    }

    pub fn sync_service(&self) -> Arc<SyncServiceBuilder> {
        SyncServiceBuilder::new((*self.inner).clone(), self.utd_hook_manager.get().cloned())
    }

    pub async fn space_service(&self) -> Arc<SpaceService> {
        let inner = UISpaceService::new((*self.inner).clone()).await;
        Arc::new(SpaceService::new(inner))
    }

    pub async fn get_notification_settings(&self) -> Arc<NotificationSettings> {
        let inner = self.inner.notification_settings().await;

        Arc::new(NotificationSettings::new((*self.inner).clone(), inner))
    }

    pub fn encryption(self: Arc<Self>) -> Arc<Encryption> {
        Arc::new(Encryption { inner: self.inner.encryption(), _client: self.clone() })
    }

    // Ignored users

    pub async fn ignored_users(&self) -> Result<Vec<String>, ClientError> {
        if let Some(raw_content) =
            self.inner.account().fetch_account_data_static::<IgnoredUserListEventContent>().await?
        {
            let content = raw_content.deserialize()?;
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
        Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
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
        Ok(Arc::new(Room::new(room, self.utd_hook_manager.get().cloned())))
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
        Ok(Arc::new(Room::new(room, self.utd_hook_manager.get().cloned())))
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
        Ok(Arc::new(Room::new(room, self.utd_hook_manager.get().cloned())))
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
            Err(error) => match error.client_api_error_kind() {
                // The room alias wasn't found, so we return None.
                Some(ErrorKind::NotFound) => Ok(None),
                // In any other case we just return the error, mapped.
                _ => Err(error.into()),
            },
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
        Ok(Arc::new(Room::new(
            self.inner.await_room_remote_echo(&room_id).await,
            self.utd_hook_manager.get().cloned(),
        )))
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

    /// Set the media retention policy.
    pub async fn set_media_retention_policy(
        &self,
        policy: MediaRetentionPolicy,
    ) -> Result<(), ClientError> {
        let closure = async || -> Result<_, Error> {
            let store = self.inner.media_store().lock().await?;
            Ok(store.set_media_retention_policy(policy).await?)
        };

        Ok(closure().await?)
    }

    /// Clear all the non-critical caches for this Client instance.
    ///
    /// WARNING: This will clear all the caches, including the base store (state
    /// store), so callers must make sure that the Client is at rest before
    /// calling it.
    ///
    /// In particular, if a [`SyncService`] is running, it must be passed here
    /// as a parameter, or stopped before calling this method. Ideally, the
    /// send queues should have been disabled and must all be inactive (i.e.
    /// not sending events); this method will disable them, but it might not
    /// be enough if the queues are still processing events.
    ///
    /// After the method returns, the Client will be in an unstable
    /// state, and it is required that the caller reinstantiates a new
    /// Client instance, be it via dropping the previous and re-creating it,
    /// restarting their application, or any other similar means.
    ///
    /// - This will get rid of the backing state store file, if provided.
    /// - This will empty all the room's persisted event caches, so all rooms
    ///   will start as if they were empty.
    /// - This will empty the media cache according to the current media
    ///   retention policy.
    pub async fn clear_caches(
        &self,
        sync_service: Option<Arc<SyncService>>,
    ) -> Result<(), ClientError> {
        let closure = async || -> Result<_, ClientError> {
            // First, make sure to expire sessions in the sync service.
            if let Some(sync_service) = sync_service {
                sync_service.inner.expire_sessions().await;
            }

            // Disable the send queues, as they might read and write to the state store.
            // Events being send might still be active, and cause errors if
            // processing finishes, so this will only minimize damage. Since
            // this method should only be called in exceptional cases, this has
            // been deemed acceptable.
            self.inner.send_queue().set_enabled(false).await;

            // Clean up the media cache according to the current media retention policy.
            self.inner
                .media_store()
                .lock()
                .await
                .map_err(Error::from)?
                .clean()
                .await
                .map_err(Error::from)?;

            // Clear all the room chunks. It's important to *not* call
            // `EventCacheStore::clear_all_linked_chunks` here, because there might be live
            // observers of the linked chunks, and that would cause some very bad state
            // mismatch.
            self.inner.event_cache().clear_all_rooms().await?;

            // Delete the state store file, if it exists.
            #[cfg(feature = "sqlite")]
            if let Some(store_path) = &self.store_path {
                debug!("Removing the state store: {}", store_path.display());

                // The state store and the crypto store both live in the same store path, so we
                // can't blindly delete the directory.
                //
                // Delete the state store SQLite file, as well as the write-ahead log (WAL) and
                // shared-memory (SHM) files, if they exist.

                for file_name in [
                    PathBuf::from(STATE_STORE_DATABASE_NAME),
                    PathBuf::from(format!("{STATE_STORE_DATABASE_NAME}.wal")),
                    PathBuf::from(format!("{STATE_STORE_DATABASE_NAME}.shm")),
                ] {
                    let file_path = store_path.join(file_name);
                    if file_path.exists() {
                        debug!("Removing file: {}", file_path.display());
                        std::fs::remove_file(&file_path).map_err(|err| ClientError::Generic {
                            msg: format!(
                                "couldn't delete the state store file {}: {err}",
                                file_path.display()
                            ),
                            details: None,
                        })?;
                    }
                }
            }

            Ok(())
        };

        closure().await
    }

    /// Checks if the server supports the report room API.
    pub async fn is_report_room_api_supported(&self) -> Result<bool, ClientError> {
        Ok(self.inner.server_versions().await?.contains(&ruma::api::MatrixVersion::V1_13))
    }

    /// Checks if the server supports the LiveKit RTC focus for placing calls.
    pub async fn is_livekit_rtc_supported(&self) -> Result<bool, ClientError> {
        Ok(self
            .inner
            .rtc_foci()
            .await?
            .iter()
            .any(|focus| matches!(focus, RtcFocusInfo::LiveKit(_))))
    }

    /// Checks if the server supports login using a QR code.
    pub async fn is_login_with_qr_code_supported(&self) -> Result<bool, ClientError> {
        Ok(matches!(self.inner.auth_api(), Some(AuthApi::OAuth(_)))
            && self.inner.unstable_features().await?.contains(&ruma::api::FeatureFlag::Msc4108))
    }

    /// Get server vendor information from the federation API.
    ///
    /// This method retrieves information about the server's name and version
    /// by calling the `/_matrix/federation/v1/version` endpoint.
    pub async fn server_vendor_info(&self) -> Result<matrix_sdk::ServerVendorInfo, ClientError> {
        Ok(self.inner.server_vendor_info(None).await?)
    }

    /// Subscribe to changes in the media preview configuration.
    pub async fn subscribe_to_media_preview_config(
        &self,
        listener: Box<dyn MediaPreviewConfigListener>,
    ) -> Result<Arc<TaskHandle>, ClientError> {
        let (initial_value, stream) = self.inner.account().observe_media_preview_config().await?;
        Ok(Arc::new(TaskHandle::new(get_runtime_handle().spawn(async move {
            // Send the initial value to the listener.
            listener.on_change(initial_value.map(|config| config.into()));
            // Listen for changes and notify the listener.
            pin_mut!(stream);
            while let Some(media_preview_config) = stream.next().await {
                listener.on_change(Some(media_preview_config.into()));
            }
        }))))
    }

    /// Set the media previews timeline display policy
    pub async fn set_media_preview_display_policy(
        &self,
        policy: MediaPreviews,
    ) -> Result<(), ClientError> {
        self.inner.account().set_media_previews_display_policy(policy.into()).await?;
        Ok(())
    }

    /// Get the media previews timeline display policy
    /// currently stored in the cache.
    pub async fn get_media_preview_display_policy(
        &self,
    ) -> Result<Option<MediaPreviews>, ClientError> {
        let configuration = self.inner.account().get_media_preview_config_event_content().await?;
        match configuration {
            Some(configuration) => Ok(configuration.media_previews.map(Into::into)),
            None => Ok(None),
        }
    }

    /// Set the invite request avatars display policy
    pub async fn set_invite_avatars_display_policy(
        &self,
        policy: InviteAvatars,
    ) -> Result<(), ClientError> {
        self.inner.account().set_invite_avatars_display_policy(policy.into()).await?;
        Ok(())
    }

    /// Get the invite request avatars display policy
    /// currently stored in the cache.
    pub async fn get_invite_avatars_display_policy(
        &self,
    ) -> Result<Option<InviteAvatars>, ClientError> {
        let configuration = self.inner.account().get_media_preview_config_event_content().await?;
        match configuration {
            Some(configuration) => Ok(configuration.invite_avatars.map(Into::into)),
            None => Ok(None),
        }
    }

    /// Fetch the media preview configuration from the server.
    pub async fn fetch_media_preview_config(
        &self,
    ) -> Result<Option<MediaPreviewConfig>, ClientError> {
        Ok(self.inner.account().fetch_media_preview_config_event_content().await?.map(Into::into))
    }

    /// Gets the `max_upload_size` value from the homeserver, which controls the
    /// max size a media upload request can have.
    pub async fn get_max_media_upload_size(&self) -> Result<u64, ClientError> {
        let max_upload_size = self.inner.load_or_fetch_max_upload_size().await?;
        Ok(max_upload_size.into())
    }

    /// Subscribe to [`RoomInfo`] updates given a provided [`RoomId`].
    ///
    /// This works even for rooms we haven't received yet, so we can subscribe
    /// to this and wait until we receive updates from them when sync responses
    /// are processed.
    ///
    /// Note this method should be used sparingly since using callback
    /// interfaces is expensive, as well as keeping them alive for a long
    /// time. Usages of this method should be short-lived and dropped as
    /// soon as possible.
    pub async fn subscribe_to_room_info(
        &self,
        room_id: String,
        listener: Box<dyn RoomInfoListener>,
    ) -> Result<Arc<TaskHandle>, ClientError> {
        let room_id = RoomId::parse(room_id)?;

        // Emit the initial event, if present
        if let Some(room) = self.inner.get_room(&room_id) {
            if let Ok(room_info) = RoomInfo::new(&room).await {
                listener.call(room_info);
            }
        }

        Ok(Arc::new(TaskHandle::new(get_runtime_handle().spawn({
            let client = self.inner.clone();
            let mut receiver = client.room_info_notable_update_receiver();
            async move {
                while let Ok(room_update) = receiver.recv().await {
                    if room_update.room_id != room_id {
                        continue;
                    }

                    if let Some(room) = client.get_room(&room_id) {
                        if let Ok(room_info) = RoomInfo::new(&room).await {
                            listener.call(room_info);
                        }
                    }
                }
            }
        }))))
    }
}

#[cfg(feature = "experimental-element-recent-emojis")]
mod recent_emoji {
    use crate::{client::Client, error::ClientError};

    /// Represents an emoji recently used for reactions.
    #[derive(Debug, uniffi::Record)]
    pub struct RecentEmoji {
        /// The actual emoji text representation.
        pub emoji: String,
        /// The number of times this emoji has been used for reactions.
        pub count: u64,
    }

    #[matrix_sdk_ffi_macros::export]
    impl Client {
        /// Adds a recently used emoji to the list and uploads the updated
        /// `io.element.recent_emoji` content to the global account data.
        pub async fn add_recent_emoji(&self, emoji: String) -> Result<(), ClientError> {
            Ok(self.inner.account().add_recent_emoji(&emoji).await?)
        }

        /// Gets the list of recently used emojis from the
        /// `io.element.recent_emoji` global account data.
        pub async fn get_recent_emojis(&self) -> Result<Vec<RecentEmoji>, ClientError> {
            Ok(self
                .inner
                .account()
                .get_recent_emojis(false)
                .await?
                .into_iter()
                .map(|(emoji, count)| RecentEmoji { emoji, count: count.into() })
                .collect::<Vec<RecentEmoji>>())
        }
    }
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait MediaPreviewConfigListener: SyncOutsideWasm + SendOutsideWasm {
    fn on_change(&self, media_preview_config: Option<MediaPreviewConfig>);
}

#[matrix_sdk_ffi_macros::export(callback_interface)]
pub trait IgnoredUsersListener: SyncOutsideWasm + SendOutsideWasm {
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

impl UserProfile {
    /// Fetch the profile for the given user ID, using the given [`Account`]
    /// API.
    pub(crate) async fn fetch(account: &Account, user_id: &UserId) -> Result<Self, ClientError> {
        let response = account.fetch_user_profile_of(user_id).await?;
        let display_name = response.get_static::<DisplayName>()?;
        let avatar_url = response.get_static::<AvatarUrl>()?.map(|url| url.to_string());

        Ok(UserProfile { user_id: user_id.to_string(), display_name, avatar_url })
    }
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
        if let Some(delegate) = self.delegate.get().cloned() {
            debug!("Applying session change: {session_change:?}");
            get_runtime_handle().spawn_blocking(move || match session_change {
                SessionChange::UnknownToken { soft_logout } => {
                    delegate.did_receive_auth_error(soft_logout);
                }
                SessionChange::TokensRefreshed => {}
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
        Ok(session_delegate.retrieve_session_from_keychain(user_id.to_string())?.into_tokens())
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

/// Configure how many rooms will be restored when restoring the session with
/// [`Client::restore_session_with`].
///
/// Please, see the documentation of [`matrix_sdk::store::RoomLoadSettings`] to
/// learn more.
#[derive(uniffi::Enum)]
pub enum RoomLoadSettings {
    /// Load all rooms from the `StateStore` into the in-memory state store
    /// `BaseStateStore`.
    All,

    /// Load a single room from the `StateStore` into the in-memory state
    /// store `BaseStateStore`.
    ///
    /// Please, be careful with this option. Read the documentation of
    /// [`RoomLoadSettings`].
    One { room_id: String },
}

impl TryInto<SdkRoomLoadSettings> for RoomLoadSettings {
    type Error = String;

    fn try_into(self) -> Result<SdkRoomLoadSettings, Self::Error> {
        Ok(match self {
            Self::All => SdkRoomLoadSettings::All,
            Self::One { room_id } => {
                SdkRoomLoadSettings::One(RoomId::parse(room_id).map_err(|error| error.to_string())?)
            }
        })
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

impl From<PowerLevels> for RoomPowerLevelsContentOverride {
    fn from(value: PowerLevels) -> Self {
        let mut power_levels = RoomPowerLevelsContentOverride::default();
        power_levels.users_default = value.users_default.map(Into::into);
        power_levels.state_default = value.state_default.map(Into::into);
        power_levels.events_default = value.events_default.map(Into::into);
        power_levels.ban = value.ban.map(Into::into);
        power_levels.kick = value.kick.map(Into::into);
        power_levels.redact = value.redact.map(Into::into);
        power_levels.invite = value.invite.map(Into::into);
        power_levels.notifications = value.notifications.map(Into::into).unwrap_or_default();
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
    pub history_visibility_override: Option<RoomHistoryVisibility>,
    #[uniffi(default = None)]
    pub canonical_alias: Option<String>,
    #[uniffi(default = false)]
    pub is_space: bool,
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
            initial_state.push(InitialStateEvent::with_empty_state_key(content).to_raw_any());
        }

        if let Some(url) = value.avatar {
            let mut content = RoomAvatarEventContent::new();
            content.url = Some(url.into());
            initial_state.push(InitialStateEvent::with_empty_state_key(content).to_raw_any());
        }

        if let Some(join_rule_override) = value.join_rule_override {
            let content = RoomJoinRulesEventContent::new(join_rule_override.try_into()?);
            initial_state.push(InitialStateEvent::with_empty_state_key(content).to_raw_any());
        }

        if let Some(history_visibility_override) = value.history_visibility_override {
            let content =
                RoomHistoryVisibilityEventContent::new(history_visibility_override.try_into()?);
            initial_state.push(InitialStateEvent::with_empty_state_key(content).to_raw_any());
        }

        request.initial_state = initial_state;

        if value.is_space {
            let mut creation_content = CreationContent::new();
            creation_content.room_type = Some(RoomType::Space);
            request.creation_content = Some(Raw::new(&creation_content)?);
        }

        if let Some(power_levels) = value.power_level_content_override {
            match Raw::<RoomPowerLevelsContentOverride>::new(&power_levels.into()) {
                Ok(power_levels) => {
                    request.power_level_content_override = Some(power_levels.cast());
                }
                Err(e) => {
                    return Err(ClientError::Generic {
                        msg: format!("Failed to serialize power levels, error: {e}"),
                        details: Some(format!("{e:?}")),
                    });
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

    /// A custom value that's not present in the spec.
    Custom { value: String },
}

impl From<RoomVisibility> for Visibility {
    fn from(value: RoomVisibility) -> Self {
        match value {
            RoomVisibility::Public => Self::Public,
            RoomVisibility::Private => Self::Private,
            RoomVisibility::Custom { value } => value.as_str().into(),
        }
    }
}

impl From<Visibility> for RoomVisibility {
    fn from(value: Visibility) -> Self {
        match value {
            Visibility::Public => Self::Public,
            Visibility::Private => Self::Private,
            _ => Self::Custom { value: value.as_str().to_owned() },
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
                let matrix_sdk::authentication::matrix::MatrixSession {
                    meta: matrix_sdk::SessionMeta { user_id, device_id },
                    tokens: matrix_sdk::SessionTokens { access_token, refresh_token },
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
            AuthApi::OAuth(api) => {
                let matrix_sdk::authentication::oauth::UserSession {
                    meta: matrix_sdk::SessionMeta { user_id, device_id },
                    tokens: matrix_sdk::SessionTokens { access_token, refresh_token },
                } = api.user_session().context("Missing session")?;
                let client_id = api.client_id().context("OIDC client ID is missing.")?.clone();
                let oidc_data = OidcSessionData { client_id };

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

    fn into_tokens(self) -> matrix_sdk::SessionTokens {
        SessionTokens { access_token: self.access_token, refresh_token: self.refresh_token }
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
            let oidc_data = serde_json::from_str::<OidcSessionData>(&oidc_data)?;

            let user_session = matrix_sdk::authentication::oauth::UserSession {
                meta: matrix_sdk::SessionMeta {
                    user_id: user_id.try_into()?,
                    device_id: device_id.into(),
                },
                tokens: matrix_sdk::SessionTokens { access_token, refresh_token },
            };

            let session = OAuthSession { client_id: oidc_data.client_id, user: user_session };

            Ok(AuthSession::OAuth(session.into()))
        } else {
            // Create a regular Matrix Session.
            let session = matrix_sdk::authentication::matrix::MatrixSession {
                meta: matrix_sdk::SessionMeta {
                    user_id: user_id.try_into()?,
                    device_id: device_id.into(),
                },
                tokens: matrix_sdk::SessionTokens { access_token, refresh_token },
            };

            Ok(AuthSession::Matrix(session))
        }
    }
}

/// Represents a client registration against an OpenID Connect authentication
/// issuer.
#[derive(Serialize, Deserialize)]
pub(crate) struct OidcSessionData {
    client_id: ClientId,
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
            AccountManagementAction::SessionView { device_id } => {
                Self::SessionView { device_id: device_id.into() }
            }
            AccountManagementAction::SessionEnd { device_id } => {
                Self::SessionEnd { device_id: device_id.into() }
            }
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
    #[cfg(not(target_family = "wasm"))]
    inner: std::sync::RwLock<Option<SdkMediaFileHandle>>,
}

impl MediaFileHandle {
    #[cfg(not(target_family = "wasm"))]
    fn new(handle: SdkMediaFileHandle) -> Self {
        Self { inner: std::sync::RwLock::new(Some(handle)) }
    }
}

#[matrix_sdk_ffi_macros::export]
impl MediaFileHandle {
    /// Get the media file's path.
    pub fn path(&self) -> Result<String, ClientError> {
        #[cfg(not(target_family = "wasm"))]
        return Ok(self
            .inner
            .read()
            .unwrap()
            .as_ref()
            .context("MediaFileHandle must not be used after calling persist")?
            .path()
            .to_str()
            .unwrap()
            .to_owned());
        #[cfg(target_family = "wasm")]
        Err(ClientError::Generic {
            msg: "MediaFileHandle.path() is not supported on WASM targets".to_string(),
            details: None,
        })
    }

    pub fn persist(&self, path: String) -> Result<bool, ClientError> {
        #[cfg(not(target_family = "wasm"))]
        {
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
        #[cfg(target_family = "wasm")]
        Err(ClientError::Generic {
            msg: "MediaFileHandle.persist() is not supported on WASM targets".to_string(),
            details: None,
        })
    }
}

#[derive(Clone, uniffi::Enum)]
pub enum SlidingSyncVersion {
    None,
    Native,
}

impl From<SdkSlidingSyncVersion> for SlidingSyncVersion {
    fn from(value: SdkSlidingSyncVersion) -> Self {
        match value {
            SdkSlidingSyncVersion::None => Self::None,
            SdkSlidingSyncVersion::Native => Self::Native,
        }
    }
}

impl TryFrom<SlidingSyncVersion> for SdkSlidingSyncVersion {
    type Error = ClientError;

    fn try_from(value: SlidingSyncVersion) -> Result<Self, Self::Error> {
        Ok(match value {
            SlidingSyncVersion::None => Self::None,
            SlidingSyncVersion::Native => Self::Native,
        })
    }
}

#[derive(Clone, uniffi::Enum)]
pub enum OidcPrompt {
    /// The Authorization Server should prompt the End-User to create a user
    /// account.
    ///
    /// Defined in [Initiating User Registration via OpenID Connect](https://openid.net/specs/openid-connect-prompt-create-1_0.html).
    Create,

    /// The Authorization Server should prompt the End-User for
    /// reauthentication.
    Login,

    /// The Authorization Server should prompt the End-User for consent before
    /// returning information to the Client.
    Consent,

    /// An unknown value.
    Unknown { value: String },
}

impl From<RumaOidcPrompt> for OidcPrompt {
    fn from(value: RumaOidcPrompt) -> Self {
        match value {
            RumaOidcPrompt::Create => Self::Create,
            value => match value.as_str() {
                "consent" => Self::Consent,
                "login" => Self::Login,
                _ => Self::Unknown { value: value.to_string() },
            },
        }
    }
}

impl From<OidcPrompt> for RumaOidcPrompt {
    fn from(value: OidcPrompt) -> Self {
        match value {
            OidcPrompt::Create => Self::Create,
            OidcPrompt::Consent => Self::from("consent"),
            OidcPrompt::Login => Self::from("login"),
            OidcPrompt::Unknown { value } => value.into(),
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
                Ok(Self::RoomMembership(ruma::room::RoomMembership::new(room_id)))
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
            _ => Err(format!("Unknown JoinRule: {value:?}")),
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
            _ => Err(format!("Invalid AllowRule: {value:?}")),
        }
    }
}

/// Contains the disk size of the different stores, if known. It won't be
/// available for in-memory stores.
#[derive(Debug, Clone, uniffi::Record)]
pub struct StoreSizes {
    /// The size of the CryptoStore.
    crypto_store: Option<u64>,
    /// The size of the StateStore.
    state_store: Option<u64>,
    /// The size of the EventCacheStore.
    event_cache_store: Option<u64>,
    /// The size of the MediaStore.
    media_store: Option<u64>,
}

impl From<matrix_sdk::StoreSizes> for StoreSizes {
    fn from(value: matrix_sdk::StoreSizes) -> Self {
        Self {
            crypto_store: value.crypto_store.map(|v| v as u64),
            state_store: value.state_store.map(|v| v as u64),
            event_cache_store: value.event_cache_store.map(|v| v as u64),
            media_store: value.media_store.map(|v| v as u64),
        }
    }
}

#[cfg(test)]
mod tests {
    use ruma::{
        api::client::room::{create_room, Visibility},
        events::StateEventType,
        room::RoomType,
    };

    use crate::{
        client::{CreateRoomParameters, JoinRule, RoomPreset, RoomVisibility},
        room::RoomHistoryVisibility,
    };

    #[test]
    fn test_create_room_parameters_mapping() {
        let params = CreateRoomParameters {
            name: Some("A room".to_owned()),
            topic: Some("A topic".to_owned()),
            is_encrypted: true,
            is_direct: true,
            visibility: RoomVisibility::Public,
            preset: RoomPreset::PublicChat,
            invite: Some(vec!["@user:example.com".to_owned()]),
            avatar: Some("http://example.com/avatar.jpg".to_owned()),
            power_level_content_override: None,
            join_rule_override: Some(JoinRule::Knock),
            history_visibility_override: Some(RoomHistoryVisibility::Shared),
            canonical_alias: Some("#a-room:example.com".to_owned()),
            is_space: true,
        };

        let request: create_room::v3::Request =
            params.try_into().expect("CreateRoomParameters couldn't be transformed into a Request");
        let initial_state = request
            .initial_state
            .iter()
            .map(|raw| raw.deserialize().expect("Initial state event failed to deserialize"))
            .collect::<Vec<_>>();

        assert_eq!(request.name, Some("A room".to_owned()));
        assert_eq!(request.topic, Some("A topic".to_owned()));
        assert!(initial_state.iter().any(|e| e.event_type() == StateEventType::RoomEncryption));
        assert!(request.is_direct);
        assert_eq!(request.visibility, Visibility::Public);
        assert_eq!(request.preset, Some(create_room::v3::RoomPreset::PublicChat));
        assert_eq!(request.invite.len(), 1);
        assert!(initial_state.iter().any(|e| e.event_type() == StateEventType::RoomAvatar));
        assert!(initial_state.iter().any(|e| e.event_type() == StateEventType::RoomJoinRules));
        assert!(initial_state
            .iter()
            .any(|e| e.event_type() == StateEventType::RoomHistoryVisibility));
        assert_eq!(request.room_alias_name, Some("#a-room:example.com".to_owned()));

        let room_type = request
            .creation_content
            .expect("Creation content is missing")
            .deserialize()
            .expect("Creation content can't be deserialized")
            .room_type;
        assert_eq!(room_type, Some(RoomType::Space));
    }
}
