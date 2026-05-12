//! LiveKit SDK integration for MatrixRTC room calls.

use std::{error::Error, future::Future, sync::Arc};

use async_trait::async_trait;
use matrix_sdk::{Client, HttpError, Room as MatrixRoom};
use reqwest::Client as HttpClient;
use ruma::{
    OwnedRoomId,
    api::client::{account::request_openid_token, discovery::discover_homeserver::RtcFocusInfo},
};
use serde_json::Value as JsonValue;
use thiserror::Error;

#[cfg(feature = "experimental-widgets")]
pub mod element_call;
pub mod per_participant;

pub use livekit;
use livekit::RoomEvent;
pub use livekit::e2ee;
use livekit::id::ParticipantIdentity;
pub use livekit::{ConnectionState, Room, RoomOptions};
use tokio::sync::Mutex;
use tracing::info;
use url::Url;

/// Errors returned by the LiveKit integration layer.
#[derive(Debug, Error)]
pub enum LiveKitError {
    /// The homeserver did not advertise a LiveKit focus.
    #[error("missing livekit focus in homeserver discovery response")]
    MissingLiveKitFocus,

    /// The homeserver discovery request failed.
    #[error(transparent)]
    Http(#[from] HttpError),

    /// The LiveKit connector failed.
    #[error("livekit connector error: {0}")]
    Connector(Box<dyn Error + Send + Sync>),

    /// Element Call widget integration failed.
    #[cfg(feature = "experimental-widgets")]
    #[error("element call widget error: {0}")]
    Widget(Box<dyn Error + Send + Sync>),
}

impl LiveKitError {
    /// Wrap a connector-specific error.
    pub fn connector<E>(error: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self::Connector(Box::new(error))
    }
}

#[cfg(feature = "experimental-widgets")]
impl LiveKitError {
    /// Wrap a widget-specific error.
    pub fn widget<E>(error: E) -> Self
    where
        E: Error + Send + Sync + 'static,
    {
        Self::Widget(Box::new(error))
    }
}

/// Result type for LiveKit operations.
pub type LiveKitResult<T> = Result<T, LiveKitError>;

/// A LiveKit connection instance created by the connector.
#[async_trait]
pub trait LiveKitConnection: Send + Sync {
    /// Disconnect and tear down the LiveKit room connection.
    async fn disconnect(self) -> LiveKitResult<()>;
}

/// A connector capable of creating LiveKit room connections.
#[async_trait]
pub trait LiveKitConnector: Send + Sync {
    /// The type for LiveKit room connections.
    type Connection: LiveKitConnection;

    /// Connect to a LiveKit room for the given room id.
    async fn connect(
        &self,
        service_url: &str,
        room: &MatrixRoom,
    ) -> LiveKitResult<Self::Connection>;
}

/// Select the LiveKit service URL from the advertised RTC foci.
pub fn select_livekit_service_url(rtc_foci: &[RtcFocusInfo]) -> Option<String> {
    rtc_foci.iter().find_map(|focus| match focus {
        RtcFocusInfo::LiveKit(info) => Some(info.service_url.clone()),
        _ => None,
    })
}

/// Convenience helper to fetch the LiveKit service URL for a client.
pub async fn livekit_service_url(client: &Client) -> LiveKitResult<String> {
    let rtc_foci = client.rtc_foci().await?;
    select_livekit_service_url(&rtc_foci).ok_or(LiveKitError::MissingLiveKitFocus)
}

/// Drive a LiveKit connection based on active room call memberships.
#[derive(Debug)]
pub struct LiveKitRoomDriver<C: LiveKitConnector> {
    room: MatrixRoom,
    connector: C,
    connection: Option<C::Connection>,
}

impl<C> LiveKitRoomDriver<C>
where
    C: LiveKitConnector,
{
    /// Create a new driver for a room.
    pub fn new(room: MatrixRoom, connector: C) -> Self {
        Self { room, connector, connection: None }
    }

    /// Run the driver until the room info stream ends.
    pub async fn run(mut self) -> LiveKitResult<()> {
        let service_url = livekit_service_url(&self.room.client()).await?;
        let mut info_stream = self.room.subscribe_info();

        self.apply_connection_update(&service_url, &self.room.clone_info()).await?;

        while let Some(room_info) = info_stream.next().await {
            self.apply_connection_update(&service_url, &room_info).await?;
        }

        if let Some(connection) = self.connection.take() {
            connection.disconnect().await?;
        }

        Ok(())
    }

    async fn apply_connection_update(
        &mut self,
        service_url: &str,
        room_info: &matrix_sdk::RoomInfo,
    ) -> LiveKitResult<()> {
        match plan_connection_update(room_info, &mut self.connection) {
            ConnectionUpdatePlan::Join => {
                info!(room_id = ?self.room.room_id(), "joining LiveKit room for active call");
                let connection = self.connector.connect(service_url, &self.room).await?;
                self.connection = Some(connection);
            }
            ConnectionUpdatePlan::Leave(connection) => {
                info!(room_id = ?self.room.room_id(), "leaving LiveKit room because the call ended");
                connection.disconnect().await?;
            }
            ConnectionUpdatePlan::Unchanged => {}
        }

        Ok(())
    }
}

/// A token provider for joining LiveKit rooms.
#[async_trait]
pub trait LiveKitTokenProvider: Send + Sync {
    /// Provide a LiveKit access token for the given Matrix room.
    async fn token(&self, room: &MatrixRoom) -> LiveKitResult<String>;
}

/// Provides LiveKit room options.
pub trait LiveKitRoomOptionsProvider: Send + Sync {
    /// Create the LiveKit room options used when connecting to LiveKit.
    fn room_options(&self) -> RoomOptions;
}

impl<F> LiveKitRoomOptionsProvider for F
where
    F: Fn() -> RoomOptions + Send + Sync,
{
    fn room_options(&self) -> RoomOptions {
        (self)()
    }
}

/// A token provider that always returns a fixed token.
#[derive(Debug, Clone)]
pub struct EnvLiveKitTokenProvider {
    token: String,
}

impl EnvLiveKitTokenProvider {
    /// Create a provider from a static token value.
    pub fn new(token: impl Into<String>) -> Self {
        Self { token: token.into() }
    }
}

#[async_trait]
impl LiveKitTokenProvider for EnvLiveKitTokenProvider {
    async fn token(&self, _room: &MatrixRoom) -> LiveKitResult<String> {
        Ok(self.token.clone())
    }
}

/// A room options provider that uses [`RoomOptions::default`].
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultRoomOptionsProvider;

impl LiveKitRoomOptionsProvider for DefaultRoomOptionsProvider {
    fn room_options(&self) -> RoomOptions {
        RoomOptions::default()
    }
}

/// Connection details used by the LiveKit SDK connector.
#[derive(Debug, Clone)]
pub struct LiveKitConnectionDetails {
    /// LiveKit service URL.
    pub service_url: String,
    /// LiveKit JWT access token.
    pub token: String,
}

impl LiveKitConnectionDetails {
    /// Return the service URL with `access_token` appended if missing.
    pub fn authenticated_service_url(&self) -> LiveKitResult<String> {
        ensure_access_token_query(&self.service_url, &self.token)
    }
}

#[derive(Debug, thiserror::Error)]
enum LiveKitDetailsError {
    #[error("missing LiveKit token")]
    MissingToken,
    #[error("missing user id for OpenID token request")]
    MissingUserId,
    #[error("missing device id for /sfu/get request")]
    MissingDeviceId,
    #[error("missing LiveKit service url in /sfu/get response")]
    MissingSfuServiceUrl,
    #[error("missing LiveKit token in /sfu/get response")]
    MissingSfuToken,
    #[error(transparent)]
    UrlParse(#[from] url::ParseError),
}

/// Resolve LiveKit connection details either from `/sfu/get` or static env values.
pub async fn resolve_connection_details(
    client: &Client,
    room: &MatrixRoom,
    sfu_get_url: Option<&str>,
    service_url_override: Option<&str>,
    static_token: Option<&str>,
) -> LiveKitResult<LiveKitConnectionDetails> {
    if let Some(sfu_url) = sfu_get_url {
        let openid_token = request_openid_token(client).await?;
        let device_id = client
            .device_id()
            .ok_or_else(|| LiveKitError::connector(LiveKitDetailsError::MissingDeviceId))?
            .to_string();
        let (service_url, token) =
            fetch_sfu_token(sfu_url, room.room_id().to_owned(), device_id, &openid_token).await?;
        return Ok(LiveKitConnectionDetails { service_url, token });
    }

    let token = static_token
        .ok_or_else(|| LiveKitError::connector(LiveKitDetailsError::MissingToken))?
        .to_owned();
    let service_url = match service_url_override {
        Some(url) => url.to_owned(),
        None => livekit_service_url(client).await?,
    };

    Ok(LiveKitConnectionDetails { service_url, token })
}

/// Prepared SDK connector configuration for running the LiveKit room driver.
#[derive(Debug)]
pub struct PreparedLiveKitSdkConnector<O: LiveKitRoomOptionsProvider> {
    /// LiveKit service URL (including access token query parameter when needed).
    pub service_url: String,
    /// SDK connector configured with token and room options providers.
    pub connector: LiveKitSdkConnector<EnvLiveKitTokenProvider, O>,
    /// Length of the resolved token, useful for structured logging.
    pub token_len: usize,
}

/// Resolve connection details and build a [`LiveKitSdkConnector`] ready for driver execution.
pub async fn prepare_livekit_sdk_connector<O>(
    client: &Client,
    room: &MatrixRoom,
    sfu_get_url: Option<&str>,
    service_url_override: Option<&str>,
    static_token: Option<&str>,
    room_options_provider: O,
) -> LiveKitResult<PreparedLiveKitSdkConnector<O>>
where
    O: LiveKitRoomOptionsProvider,
{
    let connection_details =
        resolve_connection_details(client, room, sfu_get_url, service_url_override, static_token)
            .await?;
    let service_url = connection_details.authenticated_service_url()?;
    let token = connection_details.token;
    let token_len = token.len();
    let token_provider = EnvLiveKitTokenProvider::new(token);
    let connector = LiveKitSdkConnector::new(token_provider, room_options_provider);

    Ok(PreparedLiveKitSdkConnector { service_url, connector, token_len })
}

async fn request_openid_token(
    client: &Client,
) -> LiveKitResult<request_openid_token::v3::Response> {
    let user_id = client
        .user_id()
        .ok_or_else(|| LiveKitError::connector(LiveKitDetailsError::MissingUserId))?;
    let request = request_openid_token::v3::Request::new(user_id.to_owned());
    let response = client.send(request).await?;
    Ok(response)
}

#[derive(serde::Serialize)]
struct SfuGetRequest {
    room: String,
    openid_token: OpenIdToken,
    device_id: String,
}

#[derive(serde::Serialize)]
struct OpenIdToken {
    access_token: String,
    expires_in: u64,
    matrix_server_name: String,
    token_type: String,
}

async fn fetch_sfu_token(
    url: &str,
    room_id: OwnedRoomId,
    device_id: String,
    openid_token: &request_openid_token::v3::Response,
) -> LiveKitResult<(String, String)> {
    let request_body = SfuGetRequest {
        room: room_id.to_string(),
        openid_token: OpenIdToken {
            access_token: openid_token.access_token.clone(),
            expires_in: openid_token.expires_in.as_secs(),
            matrix_server_name: openid_token.matrix_server_name.to_string(),
            token_type: openid_token.token_type.to_string(),
        },
        device_id,
    };
    let client = HttpClient::new();
    let request = client.post(url).json(&request_body);

    let response = request
        .send()
        .await
        .map_err(LiveKitError::connector)?
        .error_for_status()
        .map_err(LiveKitError::connector)?;
    let payload: JsonValue = response.json().await.map_err(LiveKitError::connector)?;

    let service_url = extract_string(
        &payload,
        &["service_url", "livekit_service_url", "livekit_url", "sfu_base_url", "sfu_url", "url"],
    )
    .ok_or_else(|| LiveKitError::connector(LiveKitDetailsError::MissingSfuServiceUrl))?;
    let token = extract_string(&payload, &["token", "jwt", "access_token"])
        .ok_or_else(|| LiveKitError::connector(LiveKitDetailsError::MissingSfuToken))?;

    Ok((service_url, token))
}

fn extract_string(payload: &JsonValue, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        payload.get(*key).and_then(|value| value.as_str()).map(|value| value.to_owned())
    })
}

fn ensure_access_token_query(service_url: &str, token: &str) -> LiveKitResult<String> {
    let mut url = Url::parse(service_url)
        .map_err(|e| LiveKitError::connector(LiveKitDetailsError::UrlParse(e)))?;
    let has_access_token = url.query_pairs().any(|(key, _)| key == "access_token");
    if !has_access_token {
        url.query_pairs_mut().append_pair("access_token", token);
    }
    Ok(url.into())
}

/// A LiveKit connection backed by the LiveKit Rust SDK.
#[derive(Debug)]
pub struct LiveKitSdkConnection {
    room: Room,
    events: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<RoomEvent>>>,
}

impl LiveKitSdkConnection {
    /// Access the underlying LiveKit room handle.
    pub fn room(&self) -> &Room {
        &self.room
    }

    /// Consume and return the LiveKit room event stream.
    ///
    /// Returns `None` if the stream has already been taken.
    pub async fn take_events(&self) -> Option<tokio::sync::mpsc::UnboundedReceiver<RoomEvent>> {
        self.events.lock().await.take()
    }

    /// Consume this connection and return the underlying LiveKit room.
    pub fn into_room(self) -> Room {
        self.room
    }
}

#[async_trait]
impl LiveKitConnection for LiveKitSdkConnection {
    async fn disconnect(self) -> LiveKitResult<()> {
        drop(self);
        Ok(())
    }
}

/// Connector implementation that joins rooms using the LiveKit Rust SDK.
#[derive(Debug)]
pub struct LiveKitSdkConnector<T, O> {
    token_provider: T,
    room_options: O,
}

impl<T, O> LiveKitSdkConnector<T, O> {
    /// Create a new connector using the provided token provider and room options.
    pub fn new(token_provider: T, room_options: O) -> Self {
        Self { token_provider, room_options }
    }

    /// Access the configured token provider.
    pub fn token_provider(&self) -> &T {
        &self.token_provider
    }

    /// Access the configured room options provider.
    pub fn room_options_provider(&self) -> &O {
        &self.room_options
    }
}

/// Stream of events received from a joined LiveKit room.
pub type LiveKitRoomEvents = tokio::sync::mpsc::UnboundedReceiver<RoomEvent>;

/// Update event emitted by [`update_connection`].
#[derive(Debug)]
pub enum LiveKitConnectionUpdate {
    Joined { room: Arc<Room>, events: Option<LiveKitRoomEvents> },
    Left,
    Unchanged,
}

/// Handle a connection update by delegating joined/left transitions.
pub async fn handle_joined_left_connection_update<S, J, L, JFut, LFut>(
    state: S,
    update: LiveKitConnectionUpdate,
    on_joined: &J,
    on_left: &L,
) -> LiveKitResult<S>
where
    J: Fn(S, Arc<Room>, Option<LiveKitRoomEvents>) -> JFut,
    L: Fn(S) -> LFut,
    JFut: Future<Output = LiveKitResult<S>>,
    LFut: Future<Output = LiveKitResult<S>>,
{
    match update {
        LiveKitConnectionUpdate::Joined { room, events } => on_joined(state, room, events).await,
        LiveKitConnectionUpdate::Left => on_left(state).await,
        LiveKitConnectionUpdate::Unchanged => Ok(state),
    }
}

/// Handle a connection update and return the next driver state.
pub async fn handle_connection_update<S, F, Fut>(
    state: S,
    update: LiveKitConnectionUpdate,
    handler: &F,
) -> LiveKitResult<S>
where
    F: Fn(S, LiveKitConnectionUpdate) -> Fut,
    Fut: Future<Output = LiveKitResult<S>>,
{
    handler(state, update).await
}

enum ConnectionUpdatePlan<C> {
    Join,
    Leave(C),
    Unchanged,
}

fn plan_connection_update<C>(
    room_info: &matrix_sdk::RoomInfo,
    connection: &mut Option<C>,
) -> ConnectionUpdatePlan<C> {
    if room_info.has_active_room_call() {
        if connection.is_none() {
            ConnectionUpdatePlan::Join
        } else {
            ConnectionUpdatePlan::Unchanged
        }
    } else if let Some(connection) = connection.take() {
        ConnectionUpdatePlan::Leave(connection)
    } else {
        ConnectionUpdatePlan::Unchanged
    }
}

/// Update an existing LiveKit connection based on room call memberships.
pub async fn update_connection<T, O>(
    room: &matrix_sdk::Room,
    connector: &LiveKitSdkConnector<T, O>,
    service_url: &str,
    room_info: &matrix_sdk::RoomInfo,
    connection: &mut Option<Arc<Room>>,
) -> LiveKitResult<LiveKitConnectionUpdate>
where
    T: LiveKitTokenProvider,
    O: LiveKitRoomOptionsProvider,
{
    match plan_connection_update(room_info, connection) {
        ConnectionUpdatePlan::Join => {
            info!(room_id = ?room.room_id(), "joining LiveKit room for active call");
            let new_connection = connector.connect(service_url, room).await?;
            let livekit_events = new_connection.take_events().await;
            let room_handle = Arc::new(new_connection.into_room());
            *connection = Some(Arc::clone(&room_handle));
            Ok(LiveKitConnectionUpdate::Joined { room: room_handle, events: livekit_events })
        }
        ConnectionUpdatePlan::Leave(_) => {
            info!(room_id = ?room.room_id(), "leaving LiveKit room because the call ended");
            Ok(LiveKitConnectionUpdate::Left)
        }
        ConnectionUpdatePlan::Unchanged => Ok(LiveKitConnectionUpdate::Unchanged),
    }
}

/// Run the LiveKit room driver while delegating connection updates to a custom handler.
pub async fn run_livekit_driver_with_handler<T, O, S, F, Fut>(
    room: matrix_sdk::Room,
    connector: &LiveKitSdkConnector<T, O>,
    service_url: &str,
    mut state: S,
    handler: F,
) -> LiveKitResult<S>
where
    T: LiveKitTokenProvider,
    O: LiveKitRoomOptionsProvider,
    F: Fn(S, LiveKitConnectionUpdate) -> Fut,
    Fut: Future<Output = LiveKitResult<S>>,
{
    let mut connection: Option<Arc<Room>> = None;
    let mut info_stream = room.subscribe_info();

    let initial_update =
        update_connection(&room, connector, service_url, &room.clone_info(), &mut connection)
            .await?;
    state = handle_connection_update(state, initial_update, &handler).await?;

    while let Some(room_info) = info_stream.next().await {
        let update =
            update_connection(&room, connector, service_url, &room_info, &mut connection).await?;
        state = handle_connection_update(state, update, &handler).await?;
    }

    let _ = connection.take();

    Ok(state)
}

/// Run the LiveKit room driver and delegate joined/left transitions to dedicated handlers.
pub async fn run_livekit_driver_joined_left<T, O, S, J, L, JFut, LFut>(
    room: matrix_sdk::Room,
    connector: &LiveKitSdkConnector<T, O>,
    service_url: &str,
    state: S,
    on_joined: J,
    on_left: L,
) -> LiveKitResult<S>
where
    T: LiveKitTokenProvider,
    O: LiveKitRoomOptionsProvider,
    J: Fn(S, Arc<Room>, Option<LiveKitRoomEvents>) -> JFut,
    L: Fn(S) -> LFut,
    JFut: Future<Output = LiveKitResult<S>>,
    LFut: Future<Output = LiveKitResult<S>>,
{
    run_livekit_driver_with_handler(room, connector, service_url, state, |state, update| async {
        handle_joined_left_connection_update(state, update, &on_joined, &on_left).await
    })
    .await
}

#[async_trait]
impl<T, O> LiveKitConnector for LiveKitSdkConnector<T, O>
where
    T: LiveKitTokenProvider,
    O: LiveKitRoomOptionsProvider,
{
    type Connection = LiveKitSdkConnection;

    async fn connect(
        &self,
        service_url: &str,
        room: &MatrixRoom,
    ) -> LiveKitResult<Self::Connection> {
        let token = self.token_provider.token(room).await?;
        let mut room_options = self.room_options_provider().room_options();
        #[allow(deprecated)]
        if room_options.encryption.is_none() {
            room_options.encryption = room_options.e2ee.clone();
        }

        if let Some(encryption) = room_options.encryption.as_ref() {
            let key_provider = &encryption.key_provider;
            let key_index = key_provider.get_latest_key_index();
            let maybe_local_key = room
                .client()
                .user_id()
                .zip(room.client().device_id())
                .map(|(user_id, device_id)| ParticipantIdentity(format!("{user_id}:{device_id}")))
                .and_then(|identity| key_provider.get_key(&identity, key_index));
            info!(
                room_id = ?room.room_id(),
                key_index,
                key_provider_key = ?maybe_local_key,
                "resolved LiveKit encryption key_provider key before connect"
            );
        }
        info!(
            room_id = ?room.room_id(),
            service_url,
            room_options = ?room_options,
            "connecting to LiveKit room with resolved room options"
        );
        let (livekit_room, events) = Room::connect(service_url, &token, room_options)
            .await
            .map_err(LiveKitError::connector)?;
        Ok(LiveKitSdkConnection { room: livekit_room, events: Mutex::new(Some(events)) })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LiveKitConnectionDetails, LiveKitConnectionUpdate, LiveKitError, handle_connection_update,
        select_livekit_service_url,
    };
    use ruma::api::client::discovery::discover_homeserver::RtcFocusInfo;

    #[test]
    fn select_livekit_service_url_prefers_first_livekit_focus() {
        let rtc_foci = vec![
            RtcFocusInfo::livekit("https://livekit-1.example.org".to_owned()),
            RtcFocusInfo::livekit("https://livekit-2.example.org".to_owned()),
        ];

        let service_url = select_livekit_service_url(&rtc_foci);

        assert_eq!(service_url.as_deref(), Some("https://livekit-1.example.org"));
    }

    #[test]
    fn select_livekit_service_url_returns_none_for_empty_list() {
        assert_eq!(select_livekit_service_url(&[]), None);
    }

    #[test]
    fn authenticated_service_url_adds_access_token() {
        let details = LiveKitConnectionDetails {
            service_url: "https://livekit.example.org/room?id=123".to_owned(),
            token: "secret-token".to_owned(),
        };

        let authenticated_url = details.authenticated_service_url().unwrap();

        assert_eq!(
            authenticated_url,
            "https://livekit.example.org/room?id=123&access_token=secret-token"
        );
    }

    #[test]
    fn authenticated_service_url_preserves_existing_access_token() {
        let details = LiveKitConnectionDetails {
            service_url: "https://livekit.example.org/room?access_token=existing".to_owned(),
            token: "ignored-token".to_owned(),
        };

        let authenticated_url = details.authenticated_service_url().unwrap();

        assert_eq!(authenticated_url, "https://livekit.example.org/room?access_token=existing");
    }

    #[test]
    fn authenticated_service_url_returns_error_for_invalid_url() {
        let details = LiveKitConnectionDetails {
            service_url: "not a valid url".to_owned(),
            token: "token".to_owned(),
        };

        let error = details.authenticated_service_url().unwrap_err();

        assert!(matches!(error, LiveKitError::Connector(_)));
    }

    #[tokio::test]
    async fn handle_connection_update_propagates_handler_state_change() {
        let result = handle_connection_update(
            String::new(),
            LiveKitConnectionUpdate::Left,
            &|mut state, update| async move {
                if matches!(update, LiveKitConnectionUpdate::Left) {
                    state.push_str("left");
                }
                Ok(state)
            },
        )
        .await
        .unwrap();

        assert_eq!(result, "left");
    }
}
