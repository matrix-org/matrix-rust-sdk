//! LiveKit helpers for room calls.

use async_trait::async_trait;
use futures_util::StreamExt;
use matrix_sdk::{Client, HttpError, Room};
use ruma::api::client::discovery::discover_homeserver::RtcFocusInfo;
use thiserror::Error;
use tracing::info;

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
    Connector(Box<dyn std::error::Error + Send + Sync>),
}

impl LiveKitError {
    /// Wrap a connector-specific error.
    pub fn connector<E>(error: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::Connector(Box::new(error))
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
    async fn connect(&self, service_url: &str, room: &Room) -> LiveKitResult<Self::Connection>;
}

/// Select the LiveKit service URL from the advertised RTC foci.
pub fn select_livekit_service_url(rtc_foci: &[RtcFocusInfo]) -> Option<String> {
    rtc_foci.iter().find_map(|focus| match focus {
        RtcFocusInfo::LiveKit(info) => Some(info.service_url.clone()),
        _ => None,
    })
}

/// Drive a LiveKit connection based on active room call memberships.
#[derive(Debug)]
pub struct LiveKitRoomDriver<C: LiveKitConnector> {
    room: Room,
    connector: C,
    connection: Option<C::Connection>,
}

impl<C> LiveKitRoomDriver<C>
where
    C: LiveKitConnector,
{
    /// Create a new driver for a room.
    pub fn new(room: Room, connector: C) -> Self {
        Self { room, connector, connection: None }
    }

    /// Run the driver until the room info stream ends.
    pub async fn run(mut self) -> LiveKitResult<()> {
        let service_url = self.livekit_service_url().await?;
        let mut info_stream = self.room.subscribe_info();

        self.update_connection(&service_url, &self.room.clone_info()).await?;

        while let Some(room_info) = info_stream.next().await {
            self.update_connection(&service_url, &room_info).await?;
        }

        if let Some(connection) = self.connection.take() {
            connection.disconnect().await?;
        }

        Ok(())
    }

    async fn livekit_service_url(&self) -> LiveKitResult<String> {
        let rtc_foci = self.room.client().rtc_foci().await?;
        select_livekit_service_url(&rtc_foci).ok_or(LiveKitError::MissingLiveKitFocus)
    }

    async fn update_connection(
        &mut self,
        service_url: &str,
        room_info: &matrix_sdk::RoomInfo,
    ) -> LiveKitResult<()> {
        let has_memberships = room_info.has_active_room_call();

        if has_memberships {
            if self.connection.is_none() {
                info!(
                    room_id = ?self.room.room_id(),
                    "joining LiveKit room for active call"
                );
                let connection = self.connector.connect(service_url, &self.room).await?;
                self.connection = Some(connection);
            }
        } else if let Some(connection) = self.connection.take() {
            info!(
                room_id = ?self.room.room_id(),
                "leaving LiveKit room because the call ended"
            );
            connection.disconnect().await?;
        }

        Ok(())
    }
}

/// Convenience helper to fetch the LiveKit service URL for a client.
pub async fn livekit_service_url(client: &Client) -> LiveKitResult<String> {
    let rtc_foci = client.rtc_foci().await?;
    select_livekit_service_url(&rtc_foci).ok_or(LiveKitError::MissingLiveKitFocus)
}
