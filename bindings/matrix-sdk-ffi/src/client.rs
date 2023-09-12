use std::sync::{Arc, RwLock};

use anyhow::{anyhow, Context};
use matrix_sdk::{
    media::{MediaFileHandle as SdkMediaFileHandle, MediaFormat, MediaRequest, MediaThumbnailSize},
    oidc::{
        types::{
            client_credentials::ClientCredentials,
            registration::{
                ClientMetadata, ClientMetadataVerificationError, VerifiedClientMetadata,
            },
        },
        FullSession, RegisteredClientData,
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
        EventEncryptionAlgorithm, TransactionId, UInt, UserId,
    },
    AuthApi, AuthSession, Client as MatrixClient, SessionChange,
};
use matrix_sdk_ui::notification_client::NotificationProcessSetup as MatrixNotificationProcessSetup;
use mime::Mime;
use ruma::{
    api::client::discovery::discover_homeserver::AuthenticationServerInfo,
    push::{HttpPusherData as RumaHttpPusherData, PushFormat as RumaPushFormat},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, error};
use url::Url;

use super::{room::Room, session_verification::SessionVerificationController, RUNTIME};
use crate::{
    client,
    notification::NotificationClientBuilder,
    notification_settings::NotificationSettings,
    sync_service::{SyncService, SyncServiceBuilder},
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
    pub(crate) inner: MatrixClient,
    delegate: RwLock<Option<Arc<dyn ClientDelegate>>>,
    session_verification_controller:
        Arc<tokio::sync::RwLock<Option<SessionVerificationController>>>,
}

impl Client {
    pub fn new(sdk_client: MatrixClient) -> Arc<Self> {
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

        let client = Arc::new(Client {
            inner: sdk_client,
            delegate: RwLock::new(None),
            session_verification_controller,
        });

        let mut session_change_receiver = client.inner.subscribe_to_session_changes();
        let client_clone = client.clone();
        RUNTIME.spawn(async move {
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

        client
    }
}

#[uniffi::export]
impl Client {
    /// Login using a username and password.
    pub fn login(
        &self,
        username: String,
        password: String,
        initial_device_name: Option<String>,
        device_id: Option<String>,
    ) -> Result<(), ClientError> {
        RUNTIME.block_on(async move {
            let mut builder = self.inner.matrix_auth().login_username(&username, &password);
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
        body: Option<String>,
        mime_type: String,
        temp_dir: Option<String>,
    ) -> Result<Arc<MediaFileHandle>, ClientError> {
        let client = self.inner.clone();
        let source = (*media_source).clone();
        let mime_type: mime::Mime = mime_type.parse()?;

        RUNTIME.block_on(async move {
            let handle = client
                .media()
                .get_media_file(
                    &MediaRequest { source, format: MediaFormat::File },
                    body,
                    &mime_type,
                    true,
                    temp_dir,
                )
                .await?;

            Ok(Arc::new(MediaFileHandle { inner: handle }))
        })
    }

    /// Restores the client from a `Session`.
    pub fn restore_session(&self, session: Session) -> Result<(), ClientError> {
        let Session {
            access_token,
            refresh_token,
            user_id,
            device_id,
            homeserver_url: _,
            oidc_data,
            sliding_sync_proxy,
        } = session;

        if let Some(oidc_data) = oidc_data {
            // Restore using an OIDC FullSession.
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
                tokens: matrix_sdk::oidc::SessionTokens {
                    access_token,
                    refresh_token,
                    latest_id_token,
                },
                issuer_info: oidc_data.issuer_info,
            };

            let session = FullSession {
                client: RegisteredClientData {
                    credentials: ClientCredentials::None { client_id: oidc_data.client_id },
                    metadata: oidc_data.client_metadata,
                },
                user: user_session,
            };

            self.restore_session_inner(session)?;
        } else {
            // Restore using a regular Matrix Session.
            let session = matrix_sdk::matrix_auth::Session {
                meta: matrix_sdk::SessionMeta {
                    user_id: user_id.try_into()?,
                    device_id: device_id.into(),
                },
                tokens: matrix_sdk::matrix_auth::SessionTokens { access_token, refresh_token },
            };

            self.restore_session_inner(session)?;
        }

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

    pub(crate) async fn async_homeserver(&self) -> String {
        self.inner.homeserver().await.to_string()
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

    /// The sliding sync proxy that is trusted by the homeserver. `None` when
    /// not configured.
    pub fn discovered_sliding_sync_proxy(&self) -> Option<Url> {
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
    pub(crate) fn whoami(&self) -> anyhow::Result<whoami::v3::Response> {
        RUNTIME
            .block_on(async move { self.inner.whoami().await.map_err(|e| anyhow!(e.to_string())) })
    }
}

#[uniffi::export]
impl Client {
    pub fn set_delegate(&self, delegate: Option<Box<dyn ClientDelegate>>) {
        *self.delegate.write().unwrap() = delegate.map(Arc::from);
    }

    pub fn session(&self) -> Result<Session, ClientError> {
        RUNTIME.block_on(async move {
            let auth_api = self.inner.auth_api().context("Missing authentication API")?;

            let homeserver_url = self.inner.homeserver().await.into();
            let sliding_sync_proxy =
                self.discovered_sliding_sync_proxy().map(|url| url.to_string());

            match auth_api {
                // Build the session from the regular Matrix Auth Session.
                AuthApi::Matrix(a) => {
                    let matrix_sdk::matrix_auth::Session {
                        meta: matrix_sdk::SessionMeta { user_id, device_id },
                        tokens:
                            matrix_sdk::matrix_auth::SessionTokens { access_token, refresh_token },
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
                            matrix_sdk::oidc::SessionTokens {
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
        })
    }

    pub fn account_url(&self) -> Option<String> {
        self.inner.oidc().account_management_url(None).ok().flatten().map(|url| url.to_string())
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

    pub fn upload_media(
        &self,
        mime_type: String,
        data: Vec<u8>,
        progress_watcher: Option<Box<dyn ProgressWatcher>>,
    ) -> Result<String, ClientError> {
        let l = self.inner.clone();

        RUNTIME.block_on(async move {
            let mime_type: mime::Mime = mime_type.parse().context("Parsing mime type")?;
            let request = l.media().upload(&mime_type, data);
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
        })
    }

    pub fn get_media_content(
        &self,
        media_source: Arc<MediaSource>,
    ) -> Result<Vec<u8>, ClientError> {
        let l = self.inner.clone();
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
        let l = self.inner.clone();
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
            self.inner.set_pusher(pusher_init.into()).await?;
            Ok(())
        })
    }

    /// The homeserver this client is configured to use.
    pub fn homeserver(&self) -> String {
        RUNTIME.block_on(self.async_homeserver())
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

    pub fn ignore_user(&self, user_id: String) -> Result<(), ClientError> {
        RUNTIME.block_on(async move {
            let user_id = UserId::parse(user_id)?;
            self.inner.account().ignore_user(&user_id).await?;
            Ok(())
        })
    }

    pub fn unignore_user(&self, user_id: String) -> Result<(), ClientError> {
        RUNTIME.block_on(async move {
            let user_id = UserId::parse(user_id)?;
            self.inner.account().unignore_user(&user_id).await?;
            Ok(())
        })
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
            let response = self.inner.get_profile(&owned_user_id).await?;

            let user_profile = UserProfile {
                user_id,
                display_name: response.displayname.clone(),
                avatar_url: response.avatar_url.as_ref().map(|url| url.to_string()),
            };

            Ok(user_profile)
        })
    }

    pub fn notification_client(
        &self,
        process_setup: NotificationProcessSetup,
    ) -> Result<Arc<NotificationClientBuilder>, ClientError> {
        NotificationClientBuilder::new(self.inner.clone(), process_setup.into())
    }

    pub fn sync_service(&self) -> Arc<SyncServiceBuilder> {
        SyncServiceBuilder::new(self.inner.clone())
    }

    pub fn get_notification_settings(&self) -> Arc<NotificationSettings> {
        RUNTIME.block_on(async move {
            Arc::new(NotificationSettings::new(
                self.inner.clone(),
                self.inner.notification_settings().await,
            ))
        })
    }
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
            RUNTIME.spawn_blocking(move || match session_change {
                SessionChange::UnknownToken { soft_logout } => {
                    delegate.did_receive_auth_error(soft_logout);
                }
                SessionChange::TokensRefreshed => {
                    delegate.did_refresh_tokens();
                }
            });
        }
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

#[uniffi::export]
fn gen_transaction_id() -> String {
    TransactionId::new().to_string()
}

/// A file handle that takes ownership of a media file on disk. When the handle
/// is dropped, the file will be removed from the disk.
#[derive(uniffi::Object)]
pub struct MediaFileHandle {
    inner: SdkMediaFileHandle,
}

#[uniffi::export]
impl MediaFileHandle {
    /// Get the media file's path.
    pub fn path(&self) -> String {
        self.inner.path().to_str().unwrap().to_owned()
    }
}
